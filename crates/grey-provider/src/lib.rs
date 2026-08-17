//! Provider adapters.

pub mod anthropic;
pub mod fallback;
pub mod mock;
pub mod openai;
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
use grey_core::{GreyConfig, Provider};

const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;
pub(crate) const MAX_TOOL_CALLS: usize = 128;
pub(crate) const MAX_TOOL_DATA_BYTES: usize = 4 * 1024 * 1024;

pub(crate) async fn bounded_http_error(
    response: reqwest::Response,
    provider: &str,
) -> anyhow::Error {
    let status = response.status();
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
                let preview = String::from_utf8_lossy(&body);
                return anyhow::anyhow!(
                    "{provider} returned {status}; failed reading error body: {error}; partial body: {preview}"
                );
            }
        }
    }
    let preview = String::from_utf8_lossy(&body);
    let suffix = if truncated { "… [truncated]" } else { "" };
    anyhow::anyhow!("{provider} returned {status}: {preview}{suffix}")
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
