use std::array;
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::Value;

use crate::config::{HooksConfig, PluginConfig, PluginKind, RuntimeConfig};
use crate::process::{run_bounded, CommandSpec};
use crate::ToolRisk;

const HOOK_EVENT_COUNT: usize = 8;
const DEFAULT_HOOK_TIMEOUT_MS: u64 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    PreMessageSend,
    PrePrompt,
    PermissionDecision,
    PreToolCall,
    PostToolCall,
    SessionStart,
    Completion,
    SessionEnd,
}

impl HookEvent {
    pub const ALL: [Self; HOOK_EVENT_COUNT] = [
        Self::PreMessageSend,
        Self::PrePrompt,
        Self::PermissionDecision,
        Self::PreToolCall,
        Self::PostToolCall,
        Self::SessionStart,
        Self::Completion,
        Self::SessionEnd,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreMessageSend => "pre_message_send",
            Self::PrePrompt => "pre_prompt",
            Self::PermissionDecision => "permission_decision",
            Self::PreToolCall => "pre_tool_call",
            Self::PostToolCall => "post_tool_call",
            Self::SessionStart => "session_start",
            Self::Completion => "completion",
            Self::SessionEnd => "session_end",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::PreMessageSend => 0,
            Self::PrePrompt => 1,
            Self::PermissionDecision => 2,
            Self::PreToolCall => 3,
            Self::PostToolCall => 4,
            Self::SessionStart => 5,
            Self::Completion => 6,
            Self::SessionEnd => 7,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct HookTool<'a> {
    pub name: &'a str,
    pub risk: ToolRisk,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct HookPayload<'a> {
    pub schema_version: u8,
    pub event: HookEvent,
    pub workspace: &'a Path,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<HookTool<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<&'a str>,
}

impl<'a> HookPayload<'a> {
    pub fn new(event: HookEvent, workspace: &'a Path) -> Self {
        Self {
            schema_version: 1,
            event,
            workspace,
            provider: None,
            model: None,
            prompt: None,
            tool: None,
            success: None,
            error: None,
        }
    }
}

#[derive(Debug, Clone)]
enum HookCommand {
    Config(String),
    Plugin {
        program: String,
        args: Vec<String>,
        timeout_ms: u64,
    },
}

#[derive(Debug, Clone)]
pub struct HookRunner {
    commands: Arc<[Vec<HookCommand>; HOOK_EVENT_COUNT]>,
    stdout_limit: usize,
    stderr_limit: usize,
}

impl HookRunner {
    pub fn new(hooks: &HooksConfig, plugins: &[PluginConfig], runtime: &RuntimeConfig) -> Self {
        let mut commands: [Vec<HookCommand>; HOOK_EVENT_COUNT] = array::from_fn(|_| Vec::new());
        for event in HookEvent::ALL {
            commands[event.index()].extend(
                config_commands(hooks, event)
                    .iter()
                    .cloned()
                    .map(HookCommand::Config),
            );
            commands[event.index()].extend(
                plugins
                    .iter()
                    .filter(|plugin| plugin_matches(plugin, event))
                    .map(|plugin| HookCommand::Plugin {
                        program: plugin.command.clone(),
                        args: plugin.args.clone(),
                        timeout_ms: plugin.timeout_ms.unwrap_or(DEFAULT_HOOK_TIMEOUT_MS),
                    }),
            );
        }
        Self {
            commands: Arc::new(commands),
            stdout_limit: runtime.command_stdout_max_bytes,
            stderr_limit: runtime.command_stderr_max_bytes,
        }
    }

    pub async fn run_best_effort(&self, payload: HookPayload<'_>) -> Result<()> {
        let mut first_error = None;
        for command in &self.commands[payload.event.index()] {
            if let Err(error) = self.execute(command, &payload).await {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    pub async fn run_prompt(&self, payload: HookPayload<'_>) -> Result<String> {
        let mut prompt = payload
            .prompt
            .context("prompt hook payload is missing prompt")?
            .to_string();
        for command in &self.commands[payload.event.index()] {
            let current_payload = HookPayload {
                prompt: Some(&prompt),
                ..payload
            };
            let output = self.execute(command, &current_payload).await?;
            match parse_prompt_output(&output)? {
                PromptAction::Continue => {}
                PromptAction::Rewrite(next) => prompt = next,
                PromptAction::Deny(reason) => {
                    bail!("{} hook denied prompt: {reason}", payload.event.as_str())
                }
            }
        }
        Ok(prompt)
    }

    pub async fn run_gate(&self, payload: HookPayload<'_>) -> Result<bool> {
        let mut allowed = true;
        for command in &self.commands[payload.event.index()] {
            let output = self.execute(command, &payload).await?;
            if let Some(next) = parse_gate_output(&output)? {
                allowed &= next;
            }
        }
        Ok(allowed)
    }

    async fn execute(&self, command: &HookCommand, payload: &HookPayload<'_>) -> Result<String> {
        let mut spec = match command {
            HookCommand::Config(command) => CommandSpec::legacy_shell(command)
                .timeout(Duration::from_millis(DEFAULT_HOOK_TIMEOUT_MS)),
            HookCommand::Plugin {
                program,
                args,
                timeout_ms,
            } => CommandSpec::direct(program, args.iter().map(OsString::from))
                .timeout(Duration::from_millis(*timeout_ms)),
        }
        .current_dir(payload.workspace)
        .stdin(serde_json::to_vec(payload).context("serializing hook payload")?)
        .stdout_limit(self.stdout_limit)
        .stderr_limit(self.stderr_limit);
        for key in ["PATH", "HOME"] {
            if let Some(value) = std::env::var_os(key) {
                spec = spec.env(key, value);
            }
        }
        #[cfg(windows)]
        for key in ["PATHEXT", "SYSTEMROOT", "USERPROFILE"] {
            if let Some(value) = std::env::var_os(key) {
                spec = spec.env(key, value);
            }
        }
        let output = run_bounded(spec)
            .await
            .with_context(|| format!("running {} hook", payload.event.as_str()))?;
        if !output.status.success() {
            bail!(
                "{} hook exited with {}: {}",
                payload.event.as_str(),
                output.status,
                output.combined_lossy().trim()
            );
        }
        Ok(output.stdout_lossy())
    }
}

enum PromptAction {
    Continue,
    Rewrite(String),
    Deny(String),
}

fn parse_prompt_output(output: &str) -> Result<PromptAction> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return Ok(PromptAction::Continue);
    }
    let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
        return Ok(PromptAction::Rewrite(trimmed.to_string()));
    };
    if let Some(prompt) = value.as_str() {
        return Ok(PromptAction::Rewrite(prompt.to_string()));
    }
    let object = value
        .as_object()
        .context("prompt hook output must be a string or object")?;
    if object.get("deny").and_then(Value::as_bool) == Some(true)
        || object.get("allow").and_then(Value::as_bool) == Some(false)
    {
        return Ok(PromptAction::Deny(
            object
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("denied")
                .to_string(),
        ));
    }
    match object.get("prompt") {
        Some(Value::String(prompt)) => Ok(PromptAction::Rewrite(prompt.clone())),
        Some(_) => bail!("prompt hook rewrite must be a string"),
        None if object.is_empty() => Ok(PromptAction::Continue),
        None => bail!("prompt hook output contains no supported decision"),
    }
}

fn parse_gate_output(output: &str) -> Result<Option<bool>> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.eq_ignore_ascii_case("true") || trimmed.eq_ignore_ascii_case("false") {
        return Ok(Some(trimmed.eq_ignore_ascii_case("true")));
    }
    if trimmed == "approved=true" || trimmed == "allow=true" {
        return Ok(Some(true));
    }
    if trimmed == "approved=false" || trimmed == "allow=false" {
        return Ok(Some(false));
    }
    let value: Value = serde_json::from_str(trimmed).context("parsing hook gate output")?;
    let object = value
        .as_object()
        .context("hook gate output must be an object")?;
    if object.get("deny").and_then(Value::as_bool) == Some(true) {
        return Ok(Some(false));
    }
    match object.get("approved").or_else(|| object.get("allow")) {
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => bail!("hook gate decision must be boolean"),
        None => bail!("hook gate output contains no supported decision"),
    }
}

fn plugin_matches(plugin: &PluginConfig, event: HookEvent) -> bool {
    plugin.enabled
        && plugin.kind == PluginKind::Hook
        && !plugin.command.trim().is_empty()
        && plugin.hook_event.as_deref() == Some(event.as_str())
}

fn config_commands(hooks: &HooksConfig, event: HookEvent) -> &[String] {
    match event {
        HookEvent::PreMessageSend => &hooks.pre_message_send,
        HookEvent::PrePrompt => &hooks.pre_prompt,
        HookEvent::PermissionDecision => &hooks.permission_decision,
        HookEvent::PreToolCall => &hooks.pre_tool_call,
        HookEvent::PostToolCall => &hooks.post_tool_call,
        HookEvent::SessionStart => &hooks.session_start,
        HookEvent::Completion => &hooks.completion,
        HookEvent::SessionEnd => &hooks.session_end,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_serialization_exposes_only_the_typed_contract() {
        let workspace = Path::new("/workspace");
        let mut payload = HookPayload::new(HookEvent::PreToolCall, workspace);
        payload.tool = Some(HookTool {
            name: "bash",
            risk: ToolRisk::Execute,
        });
        let value = serde_json::to_value(payload).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["event"], "pre_tool_call");
        assert_eq!(value["tool"]["name"], "bash");
        assert_eq!(value["tool"]["risk"], "execute");
        assert!(value.get("prompt").is_none());
        assert!(value.get("arguments").is_none());
        assert!(value.get("id").is_none());
    }

    #[test]
    fn prompt_output_rejects_non_string_rewrites_and_supports_deny() {
        assert!(parse_prompt_output(r#"{"prompt": 42}"#).is_err());
        assert!(parse_prompt_output(r#"{"unsupported": true}"#).is_err());
        assert!(matches!(
            parse_prompt_output(r#"{"deny":true,"reason":"policy"}"#).unwrap(),
            PromptAction::Deny(reason) if reason == "policy"
        ));
    }

    #[test]
    fn gate_output_rejects_unsupported_objects() {
        assert!(parse_gate_output(r#"{"unsupported":true}"#).is_err());
    }
}
