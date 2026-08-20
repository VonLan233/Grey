//! Provider adapters.

pub mod anthropic;
pub mod chatgpt_oauth;
pub mod fallback;
pub mod gemini;
pub mod mock;
pub mod openai;
mod provider_plugin;
pub mod responses;
pub mod router;
mod sse;

#[cfg(test)]
mod test_support {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    pub(crate) async fn serve_one_sse(
        body: Vec<u8>,
        declared_length: Option<usize>,
    ) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
        serve_one_response(body, declared_length, "200 OK").await
    }

    pub(crate) async fn serve_one_response(
        body: Vec<u8>,
        declared_length: Option<usize>,
        status: &'static str,
    ) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = socket.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }

            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                declared_length.unwrap_or(body.len())
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
            socket.shutdown().await.unwrap();
            request
        });
        (format!("http://{address}"), task)
    }
}

use anyhow::{bail, Result};
use futures_util::StreamExt;
use grey_core::{GreyConfig, Provider, ProviderFailure, ProviderFailureKind};

// Final UTF-8 byte ceiling for the sanitized preview, including its truncation marker.
const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;
const ERROR_BODY_TRUNCATION_MARKER: &str = " … [truncated]";
pub(crate) const MAX_TOOL_CALLS: usize = 128;

pub(crate) async fn send_http(
    client: &reqwest::Client,
    request: reqwest::Request,
    provider: &str,
) -> std::result::Result<reqwest::Response, ProviderFailure> {
    let response = client.execute(request).await.map_err(|error| {
        ProviderFailure::with_source(
            ProviderFailureKind::Transport,
            format!("{provider} request failed before receiving a response"),
            error,
        )
    })?;
    if response.status().is_success() {
        Ok(response)
    } else {
        Err(bounded_http_error(response, provider).await)
    }
}

pub(crate) async fn bounded_http_error(
    response: reqwest::Response,
    provider: &str,
) -> ProviderFailure {
    let status = response.status();
    let kind = match status.as_u16() {
        401 => ProviderFailureKind::Auth,
        403 => ProviderFailureKind::Authorization,
        429 => ProviderFailureKind::RateLimit,
        500..=599 => ProviderFailureKind::Server,
        _ => ProviderFailureKind::Protocol,
    };
    let mut chunks = response.bytes_stream();
    let mut body = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = chunks.next().await {
        match chunk {
            Ok(chunk) => {
                let remaining = MAX_ERROR_BODY_BYTES.saturating_sub(body.len());
                if chunk.len() > remaining {
                    body.extend_from_slice(&chunk[..remaining]);
                    truncated = true;
                    break;
                }
                body.extend_from_slice(&chunk);
            }
            Err(error) => {
                let preview = bounded_error_preview(&body, true);
                let message = format!(
                    "{provider} response body transport failed after {status}. Partial response: {preview}"
                );
                let source = format!(
                    "reading {provider} error response body after {status} failed; partial response: {preview}; cause: {error}"
                );
                return ProviderFailure::with_source(
                    ProviderFailureKind::Transport,
                    message,
                    source,
                );
            }
        }
    }
    let preview = bounded_error_preview(&body, truncated);
    ProviderFailure::new(kind, http_failure_message(provider, status, &preview))
}

fn bounded_error_preview(body: &[u8], body_was_truncated: bool) -> String {
    let lossy = String::from_utf8_lossy(body);
    let sanitized = grey_core::redact_provider_secrets(&lossy);
    if !body_was_truncated && sanitized.len() <= MAX_ERROR_BODY_BYTES {
        return sanitized;
    }

    let content_limit = MAX_ERROR_BODY_BYTES - ERROR_BODY_TRUNCATION_MARKER.len();
    let mut end = sanitized.len().min(content_limit);
    while !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    let mut preview = String::with_capacity(MAX_ERROR_BODY_BYTES);
    preview.push_str(&sanitized[..end]);
    preview.push_str(ERROR_BODY_TRUNCATION_MARKER);
    preview
}

fn http_failure_message(provider: &str, status: reqwest::StatusCode, preview: &str) -> String {
    match status.as_u16() {
        401 => format!(
            "{provider} authentication failed (HTTP 401). Check its API key, or run `grey auth login openai` for ChatGPT OAuth. Response: {preview}"
        ),
        403 => format!(
            "{provider} authorization failed (HTTP 403). Check the API key scope and account/project permissions; for ChatGPT OAuth, run `grey auth login openai`. Response: {preview}"
        ),
        _ => format!("{provider} returned {status}: {preview}"),
    }
}

#[cfg(test)]
mod failure_tests {
    use std::error::Error;

    use grey_core::ProviderFailureKind;

    use super::{
        bounded_http_error, send_http, test_support::serve_one_response, MAX_ERROR_BODY_BYTES,
    };

    async fn http_failure_bytes(
        status: &'static str,
        body: Vec<u8>,
        declared_length: Option<usize>,
    ) -> grey_core::ProviderFailure {
        let (base_url, server) = serve_one_response(body, declared_length, status).await;
        let response = reqwest::Client::new().get(base_url).send().await.unwrap();
        server.await.unwrap();
        bounded_http_error(response, "test provider").await
    }

    async fn http_failure(status: &'static str, body: &str) -> grey_core::ProviderFailure {
        http_failure_bytes(status, body.as_bytes().to_vec(), None).await
    }

    #[tokio::test]
    async fn http_failure_statuses_have_precise_kinds() {
        for (status, expected) in [
            ("401 Unauthorized", ProviderFailureKind::Auth),
            ("403 Forbidden", ProviderFailureKind::Authorization),
            ("429 Too Many Requests", ProviderFailureKind::RateLimit),
            ("503 Service Unavailable", ProviderFailureKind::Server),
            ("400 Bad Request", ProviderFailureKind::Protocol),
        ] {
            assert_eq!(http_failure(status, "failure").await.kind(), expected);
        }

        let auth = http_failure("401 Unauthorized", "login required").await;
        assert!(auth.to_string().contains("API key"));
        assert!(auth.to_string().contains("grey auth login openai"));
        let authorization = http_failure("403 Forbidden", "denied").await;
        assert!(authorization.to_string().contains("permissions"));
    }

    #[tokio::test]
    async fn io_failure_is_transport() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let client = reqwest::Client::new();
        let request = client.get(format!("http://{address}")).build().unwrap();

        let failure = send_http(&client, request, "test provider")
            .await
            .unwrap_err();

        assert_eq!(failure.kind(), ProviderFailureKind::Transport);
    }

    #[tokio::test]
    async fn http_failure_body_is_bounded_and_redacted() {
        let secret = "secret-value-must-not-appear";
        let body = format!(
            r#"{{"Authorization":"Bearer {secret}","token":"{secret}","api_key":"{secret}","detail":"{}"}}"#,
            "x".repeat(20 * 1024)
        );
        let failure = http_failure("500 Internal Server Error", &body).await;
        let diagnostic = format!("{failure:#}");

        assert_eq!(failure.kind(), ProviderFailureKind::Server);
        assert!(!diagnostic.contains(secret));
        assert!(diagnostic.contains("***"));
        assert!(diagnostic.contains("truncated"));
    }

    #[tokio::test]
    async fn truncated_http_error_body_is_transport_with_redacted_partial_diagnostics() {
        let secret = "partial-secret-must-not-appear";
        let body = format!(r#"{{"detail":"Bearer {secret}"}}"#).into_bytes();
        let failure =
            http_failure_bytes("401 Unauthorized", body.clone(), Some(body.len() + 64)).await;
        let display = failure.to_string();
        let source = failure.source().unwrap().to_string();

        assert_eq!(failure.kind(), ProviderFailureKind::Transport);
        for diagnostic in [&display, &source] {
            assert!(diagnostic.contains("401"));
            assert!(diagnostic.to_ascii_lowercase().contains("partial"));
            assert!(diagnostic.contains("***"));
            assert!(!diagnostic.contains(secret));
        }
    }

    #[tokio::test]
    async fn invalid_utf8_expansion_keeps_final_preview_within_byte_limit() {
        let failure = http_failure_bytes(
            "500 Internal Server Error",
            vec![0xff; MAX_ERROR_BODY_BYTES],
            None,
        )
        .await;
        let display = failure.to_string();
        let preview = display
            .split_once(": ")
            .expect("HTTP diagnostic should contain a preview")
            .1;

        assert!(preview.len() <= MAX_ERROR_BODY_BYTES);
        assert!(preview.ends_with("… [truncated]"));
    }

    #[tokio::test]
    async fn redaction_expansion_keeps_final_preview_within_byte_limit() {
        let body = "token=z ".repeat(2_000);
        assert!(body.len() < MAX_ERROR_BODY_BYTES);
        let failure = http_failure("500 Internal Server Error", &body).await;
        let display = failure.to_string();
        let preview = display
            .split_once(": ")
            .expect("HTTP diagnostic should contain a preview")
            .1;

        assert!(preview.len() <= MAX_ERROR_BODY_BYTES);
        assert!(preview.ends_with("… [truncated]"));
        assert!(!preview.contains("token=z"));
    }
}

/// Build the provider selected by config (or a CLI override).
pub fn build_provider(
    cfg: &GreyConfig,
    override_provider: Option<&str>,
) -> Result<Box<dyn Provider>> {
    match selected_provider(cfg, override_provider)? {
        "mock" => Ok(Box::new(mock::MockProvider::new(cfg.model.clone()))),
        "openai" => Ok(Box::new(openai::OpenAiCompatibleProvider::from_config(
            cfg,
        )?)),
        "anthropic" => Ok(Box::new(anthropic::AnthropicProvider::from_config(cfg)?)),
        _ => unreachable!("selected_provider validates provider identifiers"),
    }
}

/// Resolve the model using CLI override > provider-specific config > global default.
pub fn model_for_provider(
    cfg: &GreyConfig,
    override_provider: Option<&str>,
    override_model: Option<&str>,
) -> Result<String> {
    let provider = selected_provider(cfg, override_provider)?;
    if let Some(model) = override_model.filter(|model| !model.trim().is_empty()) {
        return Ok(model.to_string());
    }
    let configured = match provider {
        "mock" => &cfg.model,
        "openai" => &cfg.openai.model,
        "anthropic" => &cfg.anthropic.model,
        _ => unreachable!("selected_provider validates provider identifiers"),
    };
    if configured.trim().is_empty() {
        Ok(cfg.model.clone())
    } else {
        Ok(configured.clone())
    }
}

fn selected_provider<'a>(
    cfg: &'a GreyConfig,
    override_provider: Option<&'a str>,
) -> Result<&'a str> {
    let provider = override_provider.unwrap_or(&cfg.provider);
    match provider {
        "mock" | "openai" | "anthropic" => Ok(provider),
        unknown => bail!("unknown provider `{unknown}`; expected one of: mock, openai, anthropic"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_an_unknown_provider_instead_of_falling_back_to_mock() {
        let cfg = GreyConfig::default();
        let result = build_provider(&cfg, Some("definitely-not-a-provider"));

        assert!(result.is_err());
    }

    #[test]
    fn builds_anthropic_and_selects_provider_specific_models() {
        let cfg = GreyConfig::default();
        assert_eq!(
            build_provider(&cfg, Some("anthropic")).unwrap().id(),
            "anthropic"
        );
        assert_eq!(
            model_for_provider(&cfg, Some("anthropic"), None).unwrap(),
            cfg.anthropic.model
        );
        assert_eq!(
            model_for_provider(&cfg, Some("openai"), Some("override-model")).unwrap(),
            "override-model"
        );
    }
}
