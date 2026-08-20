use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::stream;
use futures_util::stream::BoxStream;
use grey_core::{
    process::{run_bounded, CommandSpec, DEFAULT_STDIN_LIMIT},
    ChatRequest, PluginConfig, PluginRuntime, Provider, ProviderEvent, ProviderFailure,
    ProviderFailureKind, RuntimeConfig, ToolCall, Usage, WasmPlugin,
};
use serde::{Deserialize, Serialize};

const DEFAULT_PROVIDER_PLUGIN_TIMEOUT_MS: u64 = 30_000;
const PLUGIN_PROTOCOL: &str = "grey.command-provider.v1";
const PLUGIN_SCHEMA_VERSION: u32 = 1;

#[derive(Debug)]
pub struct PluginProvider {
    id: String,
    command: String,
    args: Vec<String>,
    version: Option<String>,
    timeout: Duration,
    runtime: RuntimeConfig,
    workspace: PathBuf,
    wasm: Option<WasmPlugin>,
}

impl PluginProvider {
    pub fn new(
        id: impl Into<String>,
        command: impl Into<String>,
        args: Vec<String>,
        version: Option<String>,
        timeout_ms: Option<u64>,
        runtime: &RuntimeConfig,
        workspace: impl Into<PathBuf>,
    ) -> Self {
        Self {
            id: id.into(),
            command: command.into(),
            args,
            version,
            timeout: Duration::from_millis(
                timeout_ms.unwrap_or(DEFAULT_PROVIDER_PLUGIN_TIMEOUT_MS),
            ),
            runtime: runtime.clone(),
            workspace: workspace.into(),
            wasm: None,
        }
    }

    pub fn from_plugin(
        plugin: &PluginConfig,
        runtime: &RuntimeConfig,
        workspace: impl Into<PathBuf>,
        config_dir: &std::path::Path,
    ) -> anyhow::Result<Self> {
        if plugin.runtime == PluginRuntime::Command {
            return Ok(Self::new(
                &plugin.id,
                plugin.command.clone(),
                plugin.args.clone(),
                plugin.version.clone(),
                plugin.timeout_ms,
                runtime,
                workspace,
            ));
        }
        let wasm = WasmPlugin::from_config(plugin, config_dir, runtime).map_err(|error| {
            anyhow::anyhow!("invalid wasm provider plugin `{}`: {error}", plugin.id)
        })?;
        Ok(Self {
            id: plugin.id.clone(),
            command: String::new(),
            args: Vec::new(),
            version: plugin.version.clone(),
            timeout: Duration::from_millis(
                plugin
                    .timeout_ms
                    .unwrap_or(DEFAULT_PROVIDER_PLUGIN_TIMEOUT_MS),
            ),
            runtime: runtime.clone(),
            workspace: workspace.into(),
            wasm: Some(wasm),
        })
    }

    async fn run_plugin(&self, request: &ChatRequest) -> Result<PluginResponse, ProviderFailure> {
        let payload = serde_json::to_vec(&PluginRequest {
            schema_version: PLUGIN_SCHEMA_VERSION,
            protocol: PLUGIN_PROTOCOL,
            provider_id: &self.id,
            version: self.version.as_deref(),
            request,
        })
        .map_err(|error| {
            ProviderFailure::with_source(
                ProviderFailureKind::Protocol,
                "could not encode provider plugin request",
                error,
            )
        })?;
        if payload.len() > self.runtime.response_max_bytes.min(DEFAULT_STDIN_LIMIT) {
            return Err(ProviderFailure::new(
                ProviderFailureKind::Protocol,
                "provider plugin request exceeds configured command input limit",
            ));
        }

        let (stdout, truncated) = if let Some(wasm) = &self.wasm {
            let output = wasm.invoke(payload).await.map_err(|error| {
                ProviderFailure::new(
                    ProviderFailureKind::Protocol,
                    format!("wasm provider plugin `{}` could not run: {error}", self.id),
                )
            })?;
            (
                output.stdout,
                output.stdout_truncated || output.stderr_truncated,
            )
        } else {
            let output = run_bounded(self.command_spec(payload))
                .await
                .map_err(|error| plugin_process_failure(&self.id, error))?;
            if !output.status.success() {
                return Err(ProviderFailure::new(
                    ProviderFailureKind::Protocol,
                    format!("provider plugin `{}` exited unsuccessfully", self.id),
                ));
            }
            (
                output.stdout,
                output.stdout_truncated || output.stderr_truncated,
            )
        };
        if truncated {
            return Err(ProviderFailure::new(
                ProviderFailureKind::Protocol,
                format!(
                    "provider plugin `{}` exceeded configured output limit",
                    self.id
                ),
            ));
        }
        if stdout.len() > self.runtime.response_max_bytes {
            return Err(ProviderFailure::new(
                ProviderFailureKind::Protocol,
                "provider plugin response exceeds configured response limit",
            ));
        }
        serde_json::from_slice(&stdout).map_err(|error| {
            ProviderFailure::with_source(
                ProviderFailureKind::Protocol,
                format!("provider plugin `{}` returned invalid JSON", self.id),
                error,
            )
        })
    }

    fn command_spec(&self, stdin: Vec<u8>) -> CommandSpec {
        CommandSpec::direct(&self.command, self.args.iter().cloned())
            .current_dir(&self.workspace)
            .env("GREY_PLUGIN_PROTOCOL", PLUGIN_PROTOCOL)
            .env("GREY_PLUGIN_KIND", "provider")
            .env("GREY_PLUGIN_PROVIDER_ID", &self.id)
            .stdin(stdin)
            .timeout(self.timeout)
            .stdout_limit(self.runtime.command_stdout_max_bytes)
            .stderr_limit(self.runtime.command_stderr_max_bytes)
    }
}

fn plugin_process_failure(id: &str, error: anyhow::Error) -> ProviderFailure {
    let message = error.to_string();
    let kind = if message.contains("timed out") || message.contains("cancelled") {
        ProviderFailureKind::Transport
    } else {
        ProviderFailureKind::Protocol
    };
    ProviderFailure::with_source(kind, format!("provider plugin `{id}` could not run"), error)
}

#[derive(Serialize)]
struct PluginRequest<'a> {
    schema_version: u32,
    protocol: &'static str,
    provider_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<&'a str>,
    request: &'a ChatRequest,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginResponse {
    schema_version: u32,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    usage: Option<PluginUsage>,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginUsage {
    input_tokens: u64,
    output_tokens: u64,
}

#[async_trait]
impl Provider for PluginProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn stream_chat<'a>(
        &'a self,
        request: &'a ChatRequest,
    ) -> anyhow::Result<BoxStream<'a, ProviderEvent>> {
        let response = match self.run_plugin(request).await {
            Ok(response) if response.schema_version == PLUGIN_SCHEMA_VERSION => response,
            Ok(_) => {
                return Ok(Box::pin(stream::iter(vec![ProviderEvent::Error(
                    ProviderFailure::new(
                        ProviderFailureKind::Protocol,
                        "provider plugin returned an unsupported schema version",
                    ),
                )])))
            }
            Err(failure) => return Ok(Box::pin(stream::iter(vec![ProviderEvent::Error(failure)]))),
        };
        if let Some(error) = response.error {
            return Ok(Box::pin(stream::iter(vec![ProviderEvent::Error(
                ProviderFailure::new(ProviderFailureKind::Protocol, error),
            )])));
        }
        if response.tool_calls.len() > crate::MAX_TOOL_CALLS {
            return Ok(Box::pin(stream::iter(vec![ProviderEvent::Error(
                ProviderFailure::new(
                    ProviderFailureKind::Protocol,
                    "provider plugin returned too many tool calls",
                ),
            )])));
        }
        let mut events = Vec::with_capacity(response.tool_calls.len().saturating_add(2));
        if let Some(text) = response.text.filter(|text| !text.is_empty()) {
            if text.len() > self.runtime.response_max_bytes {
                return Ok(Box::pin(stream::iter(vec![ProviderEvent::Error(
                    ProviderFailure::new(
                        ProviderFailureKind::Protocol,
                        "provider plugin text exceeds configured response limit",
                    ),
                )])));
            }
            events.push(ProviderEvent::Delta(text));
        }
        events.extend(response.tool_calls.into_iter().map(ProviderEvent::ToolCall));
        let usage = response.usage.unwrap_or(PluginUsage {
            input_tokens: 0,
            output_tokens: 0,
        });
        events.push(ProviderEvent::Done(Usage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
        }));
        Ok(Box::pin(stream::iter(events)))
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;
    use grey_core::{ChatMessage, PluginConfig, PluginKind, PluginRuntime, Provider};
    use sha2::{Digest, Sha256};

    use super::*;

    fn provider(
        id: &str,
        command: &str,
        args: Vec<String>,
        runtime: &RuntimeConfig,
        timeout_ms: u64,
    ) -> PluginProvider {
        PluginProvider::new(id, command, args, None, Some(timeout_ms), runtime, ".")
    }

    fn request() -> ChatRequest {
        ChatRequest::new("test", vec![ChatMessage::new(grey_core::Role::User, "hi")])
    }

    async fn first_event(provider: &PluginProvider) -> ProviderEvent {
        provider
            .stream_chat(&request())
            .await
            .unwrap()
            .next()
            .await
            .unwrap()
    }

    fn sealed_wasm_provider() -> (tempfile::TempDir, PluginConfig) {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("plugins/provider");
        std::fs::create_dir_all(&dir).unwrap();
        let output = br#"{"schema_version":1,"text":"wasm"}"#;
        let mut bytes = Vec::from([8, 0, 0, 0]);
        bytes.extend_from_slice(&(output.len() as u32).to_le_bytes());
        bytes.extend_from_slice(output);
        let data = bytes
            .iter()
            .map(|byte| format!("\\{:02x}", byte))
            .collect::<String>();
        let wat = format!(
            r#"(module (import "wasi_snapshot_preview1" "fd_write" (func $w (param i32 i32 i32 i32) (result i32))) (memory 1) (export "memory" (memory 0)) (data (i32.const 0) "{data}") (func (export "_start") i32.const 1 i32.const 0 i32.const 1 i32.const 64 call $w drop))"#
        );
        let module = wat::parse_str(wat).unwrap();
        std::fs::write(dir.join("module.wasm"), &module).unwrap();
        let manifest = format!(
            r#"{{"schema_version":1,"id":"sealed-provider","kind":"provider","protocol":"grey.wasm-plugin.v1","wasi":"preview1-stdio","module":"module.wasm","module_sha256":"{}"}}"#,
            hex::encode(Sha256::digest(&module))
        );
        std::fs::write(dir.join("plugin.json"), &manifest).unwrap();
        (
            root,
            PluginConfig {
                id: "sealed-provider".into(),
                kind: PluginKind::Provider,
                enabled: true,
                runtime: PluginRuntime::Wasm,
                manifest: Some("plugins/provider/plugin.json".into()),
                manifest_sha256: Some(hex::encode(Sha256::digest(manifest.as_bytes()))),
                ..Default::default()
            },
        )
    }

    #[tokio::test]
    async fn sealed_wasm_provider_executes_without_command_fallback() {
        let (root, mut plugin) = sealed_wasm_provider();
        plugin.command = "false".into();
        assert!(PluginProvider::from_plugin(
            &plugin,
            &RuntimeConfig::default(),
            root.path(),
            root.path()
        )
        .is_err());
        plugin.command.clear();
        let provider = PluginProvider::from_plugin(
            &plugin,
            &RuntimeConfig::default(),
            root.path(),
            root.path(),
        )
        .unwrap();
        assert!(
            matches!(first_event(&provider).await, ProviderEvent::Delta(ref text) if text == "wasm")
        );
    }

    #[tokio::test]
    async fn command_provider_uses_explicit_protocol_and_safe_environment() {
        let provider = provider(
            "success",
            "printf",
            vec![r#"{"schema_version":1,"text":"ok"}"#.into()],
            &RuntimeConfig::default(),
            1_000,
        );
        let spec = provider.command_spec(Vec::new());
        assert_eq!(spec.env.len(), 3);
        assert!(spec
            .env
            .iter()
            .all(|(key, _)| !key.to_string_lossy().contains("HOME")));
        let event = first_event(&provider).await;
        assert!(
            matches!(event, ProviderEvent::Delta(ref text) if text == "ok"),
            "{event:?}"
        );
    }

    #[tokio::test]
    async fn malformed_timeout_and_overflow_are_typed_failures() {
        for (id, command, args) in [
            ("malformed", "printf", vec!["not-json".into()]),
            ("sleep", "sleep", vec!["1".into()]),
            ("overflow", "yes", vec![]),
        ] {
            let runtime = RuntimeConfig {
                command_stdout_max_bytes: 1024,
                ..RuntimeConfig::default()
            };
            let event = first_event(&provider(id, command, args, &runtime, 20)).await;
            assert!(matches!(event, ProviderEvent::Error(_)), "{id}: {event:?}");
        }
    }

    #[tokio::test]
    async fn too_many_tool_calls_fail_before_event_allocation() {
        let calls = (0..=crate::MAX_TOOL_CALLS)
            .map(|index| serde_json::json!({"id": index.to_string(), "name": "n", "arguments": {}}))
            .collect::<Vec<_>>();
        let response = serde_json::json!({"schema_version": 1, "tool_calls": calls}).to_string();
        let event = first_event(&provider(
            "calls",
            "printf",
            vec![response],
            &RuntimeConfig::default(),
            1_000,
        ))
        .await;
        assert!(matches!(event, ProviderEvent::Error(_)), "{event:?}");
    }

    #[test]
    fn command_provider_never_interprets_wasm_suffixes() {
        let provider = PluginProvider::new(
            "p",
            "plugin.wasm",
            vec![],
            None,
            None,
            &RuntimeConfig::default(),
            ".",
        );
        assert_eq!(provider.command, "plugin.wasm");
    }
}
