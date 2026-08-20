//! Native ChatGPT subscription OAuth and secure token lifecycle.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use grey_core::{ProviderFailure, ProviderFailureKind};
use rand::rngs::OsRng;
use rand::RngCore;
use reqwest::{Client, Url};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

pub const AUTH_ISSUER: &str = "https://auth.openai.com";
pub const AUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const AUTH_AUTHORIZE_PATH: &str = "/oauth/authorize";
pub const AUTH_TOKEN_PATH: &str = "/oauth/token";
pub const AUTH_SCOPE: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";
pub const CHATGPT_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
pub const ORIGINATOR: &str = "grey";

const KEYRING_SERVICE: &str = "grey.chatgpt.oauth";
const KEYRING_ENTRY: &str = "openai";
const CALLBACK_PORTS: [u16; 2] = [1455, 1457];
const CALLBACK_PATH: &str = "/auth/callback";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const REFRESH_EARLY: Duration = Duration::from_secs(5 * 60);
const MAX_REQUEST_LINE_BYTES: usize = 8 * 1024;
const MAX_HEADER_BYTES: usize = 32 * 1024;
const MAX_HTML_BYTES: usize = 4 * 1024;
const MAX_TOKEN_RESPONSE_BYTES: usize = 64 * 1024;

pub struct LoginAttempt {
    port: u16,
    code_verifier: String,
    code_challenge: String,
    state: String,
}

impl LoginAttempt {
    fn generate(port: u16) -> Result<Self> {
        let mut verifier = [0_u8; 64];
        let mut state = [0_u8; 32];
        let mut rng = OsRng;
        rng.try_fill_bytes(&mut verifier)
            .context("generating ChatGPT OAuth PKCE verifier")?;
        rng.try_fill_bytes(&mut state)
            .context("generating ChatGPT OAuth state")?;
        let code_verifier = URL_SAFE_NO_PAD.encode(verifier);
        let code_challenge = pkce_challenge(&code_verifier);
        Ok(Self {
            port,
            code_verifier,
            code_challenge,
            state: URL_SAFE_NO_PAD.encode(state),
        })
    }

    #[cfg(test)]
    fn from_parts(port: u16, verifier: &str, challenge: &str, state: &str) -> Result<Self> {
        if !matches!(port, 1455 | 1457) {
            bail!("ChatGPT OAuth callback port must be 1455 or 1457");
        }
        Ok(Self {
            port,
            code_verifier: verifier.to_string(),
            code_challenge: challenge.to_string(),
            state: state.to_string(),
        })
    }

    pub fn authorize_url(&self) -> Url {
        let mut url = Url::parse(&format!("{AUTH_ISSUER}{AUTH_AUTHORIZE_PATH}"))
            .expect("fixed ChatGPT OAuth authorization URL is valid");
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", AUTH_CLIENT_ID)
            .append_pair("redirect_uri", &self.redirect_uri())
            .append_pair("scope", AUTH_SCOPE)
            .append_pair("code_challenge", &self.code_challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &self.state)
            .append_pair("originator", ORIGINATOR);
        url
    }

    fn redirect_uri(&self) -> String {
        format!("http://localhost:{}{CALLBACK_PATH}", self.port)
    }
}

pub struct PendingLogin {
    listener: TcpListener,
    attempt: LoginAttempt,
}

impl PendingLogin {
    pub fn authorize_url(&self) -> Url {
        self.attempt.authorize_url()
    }
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

#[derive(Clone)]
pub struct ChatgptOauth {
    store: CredentialStore,
    exchange: Arc<dyn TokenExchange>,
    refresh_lock: Arc<Mutex<()>>,
}

impl ChatgptOauth {
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building ChatGPT OAuth HTTP client")?;
        Ok(Self {
            store: CredentialStore::new(Arc::new(OsKeyringBackend)),
            exchange: Arc::new(HttpTokenExchange { client }),
            refresh_lock: Arc::new(Mutex::new(())),
        })
    }

    pub async fn begin_login(&self) -> Result<PendingLogin> {
        let (listener, port) = bind_callback_listener(&CALLBACK_PORTS).await?;
        Ok(PendingLogin {
            listener,
            attempt: LoginAttempt::generate(port)?,
        })
    }

    pub async fn complete_login(&self, pending: PendingLogin) -> Result<()> {
        let code = wait_for_callback(
            pending.listener,
            pending.attempt.state.clone(),
            CALLBACK_TIMEOUT,
        )
        .await?;
        let form = authorization_code_form(&code, &pending.attempt);
        let value = self.exchange.exchange(form).await?;
        let token = token_from_response(&value, None)?;
        self.store.replace(&token).await
    }

    pub async fn logout(&self) -> Result<()> {
        self.store.delete().await
    }

    pub(crate) async fn access(&self) -> Result<AccessGrant> {
        let token = self.store.load().await?.ok_or_else(|| {
            ProviderFailure::new(
                ProviderFailureKind::Auth,
                "ChatGPT OAuth credential is not available; log in first",
            )
        })?;
        if !token.expires_within(REFRESH_EARLY) {
            return Ok(token.grant());
        }
        self.refresh_if_current(&token.access_token).await
    }

    pub(crate) async fn refresh_after_unauthorized(
        &self,
        rejected_access_token: &str,
    ) -> Result<AccessGrant> {
        self.refresh_if_current(rejected_access_token).await
    }

    pub(crate) async fn with_401_retry<T, F, Fut>(
        &self,
        mut send: F,
    ) -> Result<(reqwest::StatusCode, T)>
    where
        F: FnMut(AccessGrant) -> Fut,
        Fut: Future<Output = Result<(reqwest::StatusCode, T)>>,
    {
        let grant = self.access().await?;
        let rejected_access_token = grant.access_token.clone();
        let first = send(grant).await?;
        if first.0 != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(first);
        }
        let replacement = self
            .refresh_after_unauthorized(&rejected_access_token)
            .await?;
        send(replacement).await
    }

    async fn refresh_if_current(&self, observed_access_token: &str) -> Result<AccessGrant> {
        let _guard = self.refresh_lock.lock().await;
        let current = self.store.load().await?.ok_or_else(|| {
            ProviderFailure::new(
                ProviderFailureKind::Auth,
                "ChatGPT OAuth credential is not available; log in first",
            )
        })?;
        if current.access_token != observed_access_token && !current.expires_within(REFRESH_EARLY) {
            return Ok(current.grant());
        }
        let refresh_token = current
            .refresh_token
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                ProviderFailure::new(
                    ProviderFailureKind::Auth,
                    "ChatGPT OAuth credential has no refresh token; log in again",
                )
            })?;
        let value = self.exchange.exchange(refresh_form(refresh_token)).await?;
        let replacement = token_from_response(&value, Some(&current))?;
        self.store.replace(&replacement).await?;
        Ok(replacement.grant())
    }
}

async fn bind_callback_listener(ports: &[u16]) -> Result<(TcpListener, u16)> {
    for &port in ports {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)).await {
            return Ok((listener, port));
        }
    }
    bail!("unable to bind ChatGPT OAuth callback on 127.0.0.1 ports 1455 and 1457")
}

pub(crate) struct AccessGrant {
    pub(crate) access_token: String,
    pub(crate) account_id: String,
}

struct TokenCredential {
    access_token: String,
    refresh_token: Option<String>,
    account_id: String,
    expires_at: u64,
}

impl TokenCredential {
    fn grant(&self) -> AccessGrant {
        AccessGrant {
            access_token: self.access_token.clone(),
            account_id: self.account_id.clone(),
        }
    }

    fn expires_within(&self, duration: Duration) -> bool {
        self.expires_at <= unix_now().saturating_add(duration.as_secs())
    }

    fn encode(&self) -> String {
        json!({
            "access_token": self.access_token,
            "refresh_token": self.refresh_token,
            "account_id": self.account_id,
            "expires_at": self.expires_at,
        })
        .to_string()
    }

    fn decode(raw: &str) -> Result<Self> {
        let value: Value = serde_json::from_str(raw).context("parsing ChatGPT OAuth credential")?;
        let access_token = required_string(&value, "access_token")?;
        let account_id = required_string(&value, "account_id")?;
        let expires_at = value
            .get("expires_at")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("ChatGPT OAuth credential is missing expires_at"))?;
        let refresh_token = match value.get("refresh_token") {
            None | Some(Value::Null) => None,
            Some(Value::String(value)) if !value.is_empty() => Some(value.clone()),
            Some(_) => bail!("ChatGPT OAuth credential has malformed refresh_token"),
        };
        Ok(Self {
            access_token,
            refresh_token,
            account_id,
            expires_at,
        })
    }
}

fn required_string(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("ChatGPT OAuth value is missing {field}"))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

trait CredentialBackend: Send + Sync {
    fn load(&self) -> Result<Option<String>>;
    fn replace(&self, value: &str) -> Result<()>;
    fn delete(&self) -> Result<()>;
}

struct OsKeyringBackend;

impl OsKeyringBackend {
    fn entry(&self) -> Result<keyring::Entry> {
        keyring::Entry::new(KEYRING_SERVICE, KEYRING_ENTRY).context("opening ChatGPT OAuth keyring")
    }
}

impl CredentialBackend for OsKeyringBackend {
    fn load(&self) -> Result<Option<String>> {
        match self.entry()?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error).context("loading ChatGPT OAuth keyring credential"),
        }
    }

    fn replace(&self, value: &str) -> Result<()> {
        self.entry()?
            .set_password(value)
            .context("replacing ChatGPT OAuth keyring credential")
    }

    fn delete(&self) -> Result<()> {
        match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error).context("deleting ChatGPT OAuth keyring credential"),
        }
    }
}

#[derive(Clone)]
struct CredentialStore {
    backend: Arc<dyn CredentialBackend>,
}

impl CredentialStore {
    fn new(backend: Arc<dyn CredentialBackend>) -> Self {
        Self { backend }
    }

    async fn load(&self) -> Result<Option<TokenCredential>> {
        let backend = Arc::clone(&self.backend);
        let raw = tokio::task::spawn_blocking(move || backend.load())
            .await
            .context("joining ChatGPT OAuth keyring load")??;
        raw.map(|value| TokenCredential::decode(&value)).transpose()
    }

    async fn replace(&self, token: &TokenCredential) -> Result<()> {
        let backend = Arc::clone(&self.backend);
        let encoded = token.encode();
        tokio::task::spawn_blocking(move || backend.replace(&encoded))
            .await
            .context("joining ChatGPT OAuth keyring replace")??;
        Ok(())
    }

    async fn delete(&self) -> Result<()> {
        let backend = Arc::clone(&self.backend);
        tokio::task::spawn_blocking(move || backend.delete())
            .await
            .context("joining ChatGPT OAuth keyring delete")??;
        Ok(())
    }
}

#[async_trait]
trait TokenExchange: Send + Sync {
    async fn exchange(&self, form: Vec<(String, String)>) -> Result<Value>;
}

struct HttpTokenExchange {
    client: Client,
}

impl HttpTokenExchange {
    fn build_request(client: &Client, form: Vec<(String, String)>) -> Result<reqwest::Request> {
        client
            .post(format!("{AUTH_ISSUER}{AUTH_TOKEN_PATH}"))
            .form(&form)
            .build()
            .context("building ChatGPT OAuth token request")
    }
}

#[async_trait]
impl TokenExchange for HttpTokenExchange {
    async fn exchange(&self, form: Vec<(String, String)>) -> Result<Value> {
        let request = Self::build_request(&self.client, form)?;
        let response = self.client.execute(request).await.map_err(|error| {
            ProviderFailure::with_source(
                ProviderFailureKind::Transport,
                "ChatGPT OAuth token request failed",
                error,
            )
        })?;
        if !response.status().is_success() {
            return Err(
                crate::bounded_http_error(response, "ChatGPT OAuth token endpoint")
                    .await
                    .into(),
            );
        }
        decode_bounded_token_response(response).await
    }
}

async fn decode_bounded_token_response(mut response: reqwest::Response) -> Result<Value> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        ProviderFailure::with_source(
            ProviderFailureKind::Transport,
            "ChatGPT OAuth token response transport failed",
            error,
        )
    })? {
        if chunk.len() > MAX_TOKEN_RESPONSE_BYTES.saturating_sub(body.len()) {
            return Err(ProviderFailure::new(
                ProviderFailureKind::Protocol,
                format!("ChatGPT OAuth token response exceeds {MAX_TOKEN_RESPONSE_BYTES} bytes"),
            )
            .into());
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|error| {
        ProviderFailure::with_source(
            ProviderFailureKind::Protocol,
            "ChatGPT OAuth token response is malformed",
            error,
        )
        .into()
    })
}

fn authorization_code_form(code: &str, attempt: &LoginAttempt) -> Vec<(String, String)> {
    vec![
        ("grant_type".into(), "authorization_code".into()),
        ("code".into(), code.into()),
        ("redirect_uri".into(), attempt.redirect_uri()),
        ("client_id".into(), AUTH_CLIENT_ID.into()),
        ("code_verifier".into(), attempt.code_verifier.clone()),
    ]
}

fn refresh_form(refresh_token: &str) -> Vec<(String, String)> {
    vec![
        ("grant_type".into(), "refresh_token".into()),
        ("refresh_token".into(), refresh_token.into()),
        ("client_id".into(), AUTH_CLIENT_ID.into()),
    ]
}

fn token_from_response(
    value: &Value,
    previous: Option<&TokenCredential>,
) -> Result<TokenCredential> {
    let access_token = required_string(value, "access_token")?;
    let refresh_token = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| previous.and_then(|token| token.refresh_token.clone()));
    let expires_in = value
        .get("expires_in")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("ChatGPT OAuth token response is missing expires_in"))?;
    let account_id = account_id_from_jwt(&access_token)
        .ok()
        .or_else(|| {
            value
                .get("id_token")
                .and_then(Value::as_str)
                .and_then(|token| account_id_from_jwt(token).ok())
        })
        .or_else(|| previous.map(|token| token.account_id.clone()))
        .ok_or_else(|| anyhow!("ChatGPT OAuth token response has no account claim"))?;
    Ok(TokenCredential {
        access_token,
        refresh_token,
        account_id,
        expires_at: unix_now().saturating_add(expires_in),
    })
}

fn account_id_from_jwt(jwt: &str) -> Result<String> {
    let payload = jwt
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow!("ChatGPT OAuth JWT is malformed"))?;
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .context("decoding ChatGPT OAuth JWT payload")?;
    let claims: Value =
        serde_json::from_slice(&decoded).context("parsing ChatGPT OAuth JWT claims")?;
    claims
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .or_else(|| claims.get("chatgpt_account_id").and_then(Value::as_str))
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("ChatGPT OAuth JWT has no account claim"))
}

async fn wait_for_callback(
    listener: TcpListener,
    state: String,
    timeout: Duration,
) -> Result<String> {
    tokio::time::timeout(timeout, async move {
        let (mut stream, _) = listener
            .accept()
            .await
            .context("accepting ChatGPT OAuth callback")?;
        handle_callback_connection(&mut stream, &state).await
    })
    .await
    .map_err(|_| anyhow!("ChatGPT OAuth callback timed out"))?
}

async fn handle_callback_connection(
    stream: &mut TcpStream,
    expected_state: &str,
) -> Result<String> {
    let request = read_callback_request(stream).await?;
    let parsed = parse_callback_request(&request, expected_state);
    let message = match &parsed {
        Ok(_) => "Authentication complete. You can close this window.".to_string(),
        Err(error) => error.to_string(),
    };
    let html = callback_html(&message);
    let status = if parsed.is_ok() {
        "200 OK"
    } else {
        "400 Bad Request"
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .context("writing ChatGPT OAuth callback response")?;
    parsed
}

async fn read_callback_request(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut request = Vec::new();
    let max = MAX_REQUEST_LINE_BYTES + MAX_HEADER_BYTES + 4;
    let mut buffer = [0_u8; 1024];
    loop {
        let read = stream
            .read(&mut buffer)
            .await
            .context("reading ChatGPT OAuth callback")?;
        if read == 0 {
            bail!("ChatGPT OAuth callback ended before headers completed");
        }
        if request.len().saturating_add(read) > max {
            bail!("ChatGPT OAuth callback headers are too large");
        }
        request.extend_from_slice(&buffer[..read]);
        if request.windows(4).any(|part| part == b"\r\n\r\n") {
            return Ok(request);
        }
    }
}

fn parse_callback_request(request: &[u8], expected_state: &str) -> Result<String> {
    let text = std::str::from_utf8(request).context("ChatGPT OAuth callback is not UTF-8")?;
    let header_end = text
        .find("\r\n\r\n")
        .ok_or_else(|| anyhow!("ChatGPT OAuth callback headers are incomplete"))?;
    if !text[header_end + 4..].is_empty() {
        bail!("ChatGPT OAuth callback must not include a request body");
    }
    let (request_line, headers) = text[..header_end]
        .split_once("\r\n")
        .unwrap_or((&text[..header_end], ""));
    if request_line.len() > MAX_REQUEST_LINE_BYTES || headers.len() > MAX_HEADER_BYTES {
        bail!("ChatGPT OAuth callback request is too large");
    }
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        bail!("ChatGPT OAuth callback request line is malformed");
    };
    if method != "GET" || version != "HTTP/1.1" {
        bail!("ChatGPT OAuth callback must be an HTTP/1.1 GET");
    }
    for line in headers.split("\r\n").filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| anyhow!("ChatGPT OAuth callback header is malformed"))?;
        if name.eq_ignore_ascii_case("transfer-encoding")
            || (name.eq_ignore_ascii_case("content-length") && value.trim() != "0")
        {
            bail!("ChatGPT OAuth callback must not include a request body");
        }
    }
    let url = Url::parse(&format!("http://localhost{target}"))
        .context("parsing ChatGPT OAuth callback target")?;
    if url.path() != CALLBACK_PATH {
        bail!("ChatGPT OAuth callback path is invalid");
    }
    let mut code = None;
    let mut state = None;
    for (name, value) in url.query_pairs() {
        match name.as_ref() {
            "code" if code.is_none() => code = Some(value.into_owned()),
            "state" if state.is_none() => state = Some(value.into_owned()),
            "code" | "state" => bail!("ChatGPT OAuth callback parameter is duplicated"),
            _ => {}
        }
    }
    let code = code.filter(|value| !value.is_empty()).ok_or_else(|| {
        anyhow!("ChatGPT OAuth callback is missing a non-empty authorization code")
    })?;
    if state.as_deref() != Some(expected_state) {
        bail!("ChatGPT OAuth callback state did not match");
    }
    Ok(code)
}

fn callback_html(message: &str) -> String {
    const PREFIX: &str = "<!doctype html><meta charset=utf-8><title>Grey OAuth</title><p>";
    const SUFFIX: &str = "</p>";
    let mut html = String::with_capacity(MAX_HTML_BYTES);
    html.push_str(PREFIX);
    for character in message.chars() {
        let escaped = match character {
            '&' => "&amp;",
            '<' => "&lt;",
            '>' => "&gt;",
            '"' => "&quot;",
            '\'' => "&#39;",
            _ => {
                if html.len() + character.len_utf8() + SUFFIX.len() > MAX_HTML_BYTES {
                    break;
                }
                html.push(character);
                continue;
            }
        };
        if html.len() + escaped.len() + SUFFIX.len() > MAX_HTML_BYTES {
            break;
        }
        html.push_str(escaped);
    }
    html.push_str(SUFFIX);
    html
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;

    use super::*;

    struct FakeBackend {
        value: StdMutex<Option<String>>,
        fail_replace: StdMutex<bool>,
        deletes: AtomicUsize,
    }

    impl FakeBackend {
        fn with_token(token: &TokenCredential) -> Self {
            Self {
                value: StdMutex::new(Some(token.encode())),
                fail_replace: StdMutex::new(false),
                deletes: AtomicUsize::new(0),
            }
        }
    }

    impl CredentialBackend for FakeBackend {
        fn load(&self) -> Result<Option<String>> {
            Ok(self.value.lock().unwrap().clone())
        }

        fn replace(&self, value: &str) -> Result<()> {
            if *self.fail_replace.lock().unwrap() {
                bail!("injected replace failure");
            }
            *self.value.lock().unwrap() = Some(value.to_string());
            Ok(())
        }

        fn delete(&self) -> Result<()> {
            self.deletes.fetch_add(1, Ordering::SeqCst);
            *self.value.lock().unwrap() = None;
            Ok(())
        }
    }

    struct FakeExchange {
        calls: AtomicUsize,
        response: Value,
    }

    #[async_trait]
    impl TokenExchange for FakeExchange {
        async fn exchange(&self, _form: Vec<(String, String)>) -> Result<Value> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            Ok(self.response.clone())
        }
    }

    fn token(access: &str, refresh: Option<&str>, expires_at: u64) -> TokenCredential {
        TokenCredential {
            access_token: access.into(),
            refresh_token: refresh.map(str::to_string),
            account_id: "acct-1".into(),
            expires_at,
        }
    }

    fn id_token(account_id: &str) -> String {
        let payload = json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": account_id}
        });
        format!(
            "e30.{}.signature",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        )
    }

    #[test]
    fn rfc7636_s256_vector_and_generated_entropy_lengths() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );

        let attempt = LoginAttempt::generate(1455).unwrap();
        assert_eq!(decode_urlsafe(&attempt.code_verifier).len(), 64);
        assert_eq!(decode_urlsafe(&attempt.state).len(), 32);
        assert_ne!(attempt.code_verifier, attempt.state);
    }

    #[test]
    fn authorize_url_uses_only_fixed_production_values() {
        let attempt = LoginAttempt::from_parts(1457, "verifier", "challenge", "state").unwrap();
        let url = attempt.authorize_url();
        assert_eq!(url.origin().ascii_serialization(), AUTH_ISSUER);
        assert_eq!(url.path(), AUTH_AUTHORIZE_PATH);
        let query = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(query["client_id"], AUTH_CLIENT_ID);
        assert_eq!(query["redirect_uri"], "http://localhost:1457/auth/callback");
        assert_eq!(query["scope"], AUTH_SCOPE);
        assert_eq!(query["code_challenge"], "challenge");
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["state"], "state");
        assert_eq!(query["originator"], ORIGINATOR);
        assert!(LoginAttempt::from_parts(1456, "v", "c", "s").is_err());
    }

    #[test]
    fn token_forms_endpoint_and_account_claim_are_exact() {
        let attempt = LoginAttempt::from_parts(1455, "verifier", "challenge", "state").unwrap();
        assert_eq!(
            authorization_code_form("auth-code", &attempt),
            vec![
                ("grant_type".into(), "authorization_code".into()),
                ("code".into(), "auth-code".into()),
                (
                    "redirect_uri".into(),
                    "http://localhost:1455/auth/callback".into()
                ),
                ("client_id".into(), AUTH_CLIENT_ID.into()),
                ("code_verifier".into(), "verifier".into()),
            ]
        );
        assert_eq!(
            refresh_form("refresh-old"),
            vec![
                ("grant_type".into(), "refresh_token".into()),
                ("refresh_token".into(), "refresh-old".into()),
                ("client_id".into(), AUTH_CLIENT_ID.into()),
            ]
        );

        let client = Client::new();
        let request = HttpTokenExchange::build_request(&client, refresh_form("r")).unwrap();
        assert_eq!(
            request.url().as_str(),
            "https://auth.openai.com/oauth/token"
        );
        let body = std::str::from_utf8(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert!(body.contains("grant_type=refresh_token"));
        assert!(body.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));

        let credential = token_from_response(
            &json!({
                "access_token":id_token("acct-claim"),
                "refresh_token":"refresh-new",
                "expires_in":3600
            }),
            None,
        )
        .unwrap();
        assert_eq!(credential.account_id, "acct-claim");
    }

    #[tokio::test]
    async fn replace_failure_keeps_old_credential_and_logout_deletes_once() {
        let old = token("access-old", Some("refresh-old"), unix_now() + 3600);
        let backend = Arc::new(FakeBackend::with_token(&old));
        let store = CredentialStore::new(backend.clone());
        *backend.fail_replace.lock().unwrap() = true;
        assert!(store
            .replace(&token("access-new", Some("refresh-new"), unix_now() + 3600))
            .await
            .is_err());
        assert_eq!(
            store.load().await.unwrap().unwrap().access_token,
            "access-old"
        );

        *backend.fail_replace.lock().unwrap() = false;
        store.delete().await.unwrap();
        assert!(store.load().await.unwrap().is_none());
        assert_eq!(backend.deletes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn missing_credential_is_a_typed_auth_failure() {
        let backend = Arc::new(FakeBackend {
            value: StdMutex::new(None),
            fail_replace: StdMutex::new(false),
            deletes: AtomicUsize::new(0),
        });
        let oauth = ChatgptOauth {
            store: CredentialStore::new(backend),
            exchange: Arc::new(FakeExchange {
                calls: AtomicUsize::new(0),
                response: json!({}),
            }),
            refresh_lock: Arc::new(Mutex::new(())),
        };
        let error = match oauth.access().await {
            Ok(_) => panic!("missing credential unexpectedly produced an access grant"),
            Err(error) => error,
        };
        assert_eq!(
            error.downcast_ref::<ProviderFailure>().unwrap().kind(),
            ProviderFailureKind::Auth
        );
    }

    #[tokio::test]
    async fn concurrent_preexpiry_refresh_is_single_flight_and_preserves_refresh_token() {
        let old = token("access-old", Some("refresh-old"), unix_now() + 30);
        let backend = Arc::new(FakeBackend::with_token(&old));
        let exchange = Arc::new(FakeExchange {
            calls: AtomicUsize::new(0),
            response: json!({"access_token":"access-new","expires_in":3600}),
        });
        let oauth = ChatgptOauth {
            store: CredentialStore::new(backend.clone()),
            exchange: exchange.clone(),
            refresh_lock: Arc::new(Mutex::new(())),
        };

        let mut joins = Vec::new();
        for _ in 0..8 {
            let oauth = oauth.clone();
            joins.push(tokio::spawn(async move { oauth.access().await.unwrap() }));
        }
        for join in joins {
            let grant = join.await.unwrap();
            assert_eq!(grant.access_token, "access-new");
            assert_eq!(grant.account_id, "acct-1");
        }
        assert_eq!(exchange.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            CredentialStore::new(backend)
                .load()
                .await
                .unwrap()
                .unwrap()
                .refresh_token
                .as_deref(),
            Some("refresh-old")
        );
    }

    #[tokio::test]
    async fn unauthorized_refreshes_and_retries_once_while_forbidden_stops() {
        for (first_status, expected_sends, expected_refreshes) in [
            (reqwest::StatusCode::UNAUTHORIZED, 2, 1),
            (reqwest::StatusCode::FORBIDDEN, 1, 0),
        ] {
            let old = token("access-old", Some("refresh-old"), unix_now() + 3600);
            let backend = Arc::new(FakeBackend::with_token(&old));
            let exchange = Arc::new(FakeExchange {
                calls: AtomicUsize::new(0),
                response: json!({"access_token":"access-new","expires_in":3600}),
            });
            let oauth = ChatgptOauth {
                store: CredentialStore::new(backend),
                exchange: exchange.clone(),
                refresh_lock: Arc::new(Mutex::new(())),
            };
            let sends = Arc::new(AtomicUsize::new(0));
            let result = oauth
                .with_401_retry({
                    let sends = sends.clone();
                    move |grant| {
                        let sends = sends.clone();
                        async move {
                            let attempt = sends.fetch_add(1, Ordering::SeqCst);
                            let status = if attempt == 0 {
                                first_status
                            } else {
                                reqwest::StatusCode::UNAUTHORIZED
                            };
                            Ok((status, grant.access_token))
                        }
                    }
                })
                .await
                .unwrap();
            assert_eq!(
                result.0,
                if expected_sends == 2 {
                    reqwest::StatusCode::UNAUTHORIZED
                } else {
                    first_status
                }
            );
            assert_eq!(sends.load(Ordering::SeqCst), expected_sends);
            assert_eq!(exchange.calls.load(Ordering::SeqCst), expected_refreshes);
        }
    }

    #[tokio::test]
    async fn callback_timeout_is_bounded() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let error = wait_for_callback(listener, "state".into(), Duration::from_millis(5))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn callback_binding_uses_primary_then_single_fallback() {
        assert_eq!(CALLBACK_PORTS, [1455, 1457]);

        let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let occupied_port = occupied.local_addr().unwrap().port();
        let available = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let available_port = available.local_addr().unwrap().port();
        drop(available);

        let (listener, selected) = bind_callback_listener(&[occupied_port, available_port])
            .await
            .unwrap();
        assert_eq!(selected, available_port);
        drop(listener);
    }

    #[tokio::test]
    async fn successful_token_response_body_is_bounded_before_json_decode() {
        let body = vec![b'x'; MAX_TOKEN_RESPONSE_BYTES + 1];
        let (base_url, server) =
            crate::test_support::serve_one_response(body, None, "200 OK").await;
        let response = Client::new().get(base_url).send().await.unwrap();
        let error = decode_bounded_token_response(response).await.unwrap_err();
        server.await.unwrap();
        assert_eq!(
            error.downcast_ref::<ProviderFailure>().unwrap().kind(),
            ProviderFailureKind::Protocol
        );
    }

    #[test]
    fn callback_rejects_wrong_method_path_state_body_and_oversized_input() {
        for request in [
            "POST /auth/callback?code=x&state=s HTTP/1.1\r\n\r\n",
            "GET /wrong?code=x&state=s HTTP/1.1\r\n\r\n",
            "GET /auth/callback?code=x&state=wrong HTTP/1.1\r\n\r\n",
            "GET /auth/callback?code=&state=s HTTP/1.1\r\n\r\n",
            "GET /auth/callback?code=x&state=s HTTP/1.1\r\nContent-Length: 1\r\n\r\nx",
        ] {
            assert!(parse_callback_request(request.as_bytes(), "s").is_err());
        }
        let oversized = format!(
            "GET /auth/callback?code={} HTTP/1.1\r\n\r\n",
            "x".repeat(MAX_REQUEST_LINE_BYTES)
        );
        assert!(parse_callback_request(oversized.as_bytes(), "s").is_err());
        let headers = format!(
            "GET /auth/callback?code=x&state=s HTTP/1.1\r\nX: {}\r\n\r\n",
            "x".repeat(MAX_HEADER_BYTES)
        );
        assert!(parse_callback_request(headers.as_bytes(), "s").is_err());
    }

    #[test]
    fn callback_accepts_exact_get_and_html_is_escaped_and_bounded() {
        let code = parse_callback_request(
            b"GET /auth/callback?code=abc%2D123&state=expected HTTP/1.1\r\nHost: localhost\r\n\r\n",
            "expected",
        )
        .unwrap();
        assert_eq!(code, "abc-123");

        let html = callback_html("<script>alert('x')</script>");
        assert!(!html.contains("<script>"));
        assert!(html.len() <= MAX_HTML_BYTES);
    }

    fn decode_urlsafe(value: &str) -> Vec<u8> {
        base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, value).unwrap()
    }
}
