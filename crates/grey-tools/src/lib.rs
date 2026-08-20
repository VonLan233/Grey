//! Workspace-scoped built-in tools with explicit approval for side effects.

#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::Mutex;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use globset::{Glob, GlobMatcher};
use grey_core::{
    process::{run_bounded, CommandSpec},
    HookEvent, HookPayload, HookRunner, HookTool, McpServerConfig, McpToolConfig, PluginConfig,
    PluginRuntime, RuntimeConfig, ToolCall, ToolDefinition, ToolExecutor, ToolResult, ToolRisk,
    WasmPlugin,
};
use ignore::WalkBuilder;
use regex::Regex;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::json;
use serde_json::Value;
use tempfile::NamedTempFile;

pub const BUILTIN_TOOL_NAMES: [&str; 5] = ["read_file", "edit_file", "bash", "glob", "grep"];
const LSP_TOOL_DEFAULT_MAX_ITEMS: usize = 50;
const LSP_TOOL_MAX_ITEMS: usize = 500;
const DEFAULT_TOOL_TIMEOUT: u64 = 5_000;
const MCP_PROTOCOL_VERSION: &str = "2025-11-25";
const MCP_LINE_LIMIT: usize = 1024 * 1024;
const MCP_RESULT_LIMIT: usize = 64 * 1024;
const MCP_TERM_WAIT: Duration = Duration::from_millis(250);
const MCP_REAP_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct McpTool {
    pub name: String,
    pub command: String,
    pub description: Option<String>,
    pub args: Vec<String>,
    pub input_schema: Option<Value>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
struct LspToolOutput<T: Serialize> {
    tool: &'static str,
    path: String,
    count: usize,
    shown: usize,
    truncated: bool,
    compact: Vec<T>,
}

#[derive(Debug, Serialize)]
struct LspDiagnosticOutput {
    line: u32,
    character: u32,
    end_line: u32,
    end_character: u32,
    severity: String,
    code: String,
    source: String,
    message: String,
}

#[derive(Debug, Serialize)]
struct LspLocationOutput {
    uri: String,
    start_line: u32,
    start_character: u32,
    end_line: u32,
    end_character: u32,
}

#[derive(Debug, Serialize)]
struct LspReferenceOutput {
    path: String,
    start_line: u32,
    start_character: u32,
    end_line: u32,
    end_character: u32,
}

#[derive(Debug, Serialize)]
struct LspSymbolOutput {
    name: String,
    kind: String,
    detail: Option<String>,
    container_name: Option<String>,
    start_line: u32,
    start_character: u32,
    end_line: u32,
    end_character: u32,
}

#[derive(Debug, Serialize)]
struct LspTextOutput {
    text: String,
}

pub struct LspTools {
    workspace: PathBuf,
    lsp_binary: String,
}

impl LspTools {
    pub fn new(workspace: &Path, lsp_binary: String) -> Result<Self> {
        let workspace = workspace
            .canonicalize()
            .with_context(|| format!("canonicalizing workspace {}", workspace.display()))?;
        anyhow::ensure!(workspace.is_dir(), "workspace must be a directory");
        Ok(Self {
            workspace,
            lsp_binary,
        })
    }

    async fn dispatch(&self, call: &ToolCall) -> Result<ToolResult> {
        match call.name.as_str() {
            "lsp_diagnostics" => self.lsp_diagnostics(call).await,
            "lsp_definition" => self.lsp_definition(call).await,
            "lsp_references" => self.lsp_references(call).await,
            "lsp_hover" => self.lsp_hover(call).await,
            "lsp_rename" => self.lsp_rename(call).await,
            "lsp_symbols" => self.lsp_symbols(call).await,
            _ => bail!("unknown tool {:?}", call.name),
        }
    }

    async fn lsp_diagnostics(&self, call: &ToolCall) -> Result<ToolResult> {
        let args: LspDiagnosticsArgs = parse_args(call)?;
        let path = self.resolve_existing(&args.path)?;
        let file_path = path.to_string_lossy().into_owned();
        let diagnostics =
            grey_lsp::collect_file_diagnostics(&path, Some(Path::new(&self.lsp_binary)))
                .await
                .with_context(|| format!("running LSP diagnostics for {file_path}"))?;
        let max_items = normalized_max_items(args.max_items);
        let mut compact = Vec::new();
        let mut seen = HashSet::new();
        for diagnostic in diagnostics.iter().take(max_items) {
            let range = diagnostic.range;
            let item = LspDiagnosticOutput {
                line: range.start.line + 1,
                character: range.start.character + 1,
                end_line: range.end.line + 1,
                end_character: range.end.character + 1,
                severity: diagnostic
                    .severity
                    .as_ref()
                    .map(|value| format!("{value:?}"))
                    .unwrap_or_else(|| "Unknown".into()),
                code: diagnostic
                    .code
                    .as_ref()
                    .map(diagnostic_code_to_string)
                    .unwrap_or_default(),
                source: diagnostic.source.clone().unwrap_or_default(),
                message: diagnostic.message.replace('\n', " "),
            };
            let key = (
                item.line,
                item.character,
                item.end_line,
                item.end_character,
                item.severity.clone(),
                item.message.clone(),
            );
            if seen.insert(key) {
                compact.push(item);
            }
        }
        let output =
            compact_tool_output("lsp_diagnostics", &file_path, diagnostics.len(), compact)?;
        Ok(ToolResult::success(call, output))
    }

    async fn lsp_definition(&self, call: &ToolCall) -> Result<ToolResult> {
        let args: LspDefinitionArgs = parse_args(call)?;
        let path = self.resolve_existing(&args.path)?;
        let file_path = path.to_string_lossy().into_owned();
        let definitions = grey_lsp::collect_file_definitions(
            &path,
            Some(Path::new(&self.lsp_binary)),
            args.line,
            args.character,
        )
        .await
        .with_context(|| format!("running LSP definition for {file_path}"))?;
        let max_items = normalized_max_items(args.max_items);
        let total = definitions.len();
        let mut compact = Vec::new();
        let mut seen = HashSet::new();
        for definition in definitions.into_iter().take(max_items) {
            let key = (
                definition.uri.clone(),
                definition.start_line,
                definition.start_character,
                definition.end_line,
                definition.end_character,
            );
            if seen.insert(key) {
                compact.push(LspLocationOutput {
                    uri: definition.uri,
                    start_line: definition.start_line,
                    start_character: definition.start_character,
                    end_line: definition.end_line,
                    end_character: definition.end_character,
                });
            }
        }
        let output = compact_tool_output("lsp_definition", &file_path, total, compact)?;
        Ok(ToolResult::success(call, output))
    }

    async fn lsp_references(&self, call: &ToolCall) -> Result<ToolResult> {
        let args: LspReferencesArgs = parse_args(call)?;
        let path = self.resolve_existing(&args.path)?;
        let file_path = path.to_string_lossy().into_owned();
        let references = grey_lsp::collect_file_references(
            &path,
            Some(Path::new(&self.lsp_binary)),
            args.line,
            args.character,
            args.include_declaration.unwrap_or(true),
        )
        .await
        .with_context(|| format!("running LSP references for {file_path}"))?;
        let max_items = normalized_max_items(args.max_items);
        let total = references.len();
        let mut compact = Vec::new();
        let mut seen = HashSet::new();
        for reference in references.into_iter().take(max_items) {
            let key = (
                reference.uri.clone(),
                reference.start_line,
                reference.start_character,
                reference.end_line,
                reference.end_character,
            );
            if seen.insert(key) {
                compact.push(LspReferenceOutput {
                    path: reference.uri,
                    start_line: reference.start_line,
                    start_character: reference.start_character,
                    end_line: reference.end_line,
                    end_character: reference.end_character,
                });
            }
        }
        let output = compact_tool_output("lsp_references", &file_path, total, compact)?;
        Ok(ToolResult::success(call, output))
    }

    async fn lsp_hover(&self, call: &ToolCall) -> Result<ToolResult> {
        let args: LspPositionArgs = parse_args(call)?;
        let path = self.resolve_existing(&args.path)?;
        let file_path = path.to_string_lossy().into_owned();
        let hover = grey_lsp::collect_file_hover(
            &path,
            Some(Path::new(&self.lsp_binary)),
            args.line,
            args.character,
        )
        .await
        .with_context(|| format!("running LSP hover for {file_path}"))?;
        let output = compact_text_tool_output("lsp_hover", &file_path, hover)?;
        Ok(ToolResult::success(call, output))
    }

    async fn lsp_rename(&self, call: &ToolCall) -> Result<ToolResult> {
        let args: LspRenameArgs = parse_args(call)?;
        let path = self.resolve_existing(&args.path)?;
        let file_path = path.to_string_lossy().into_owned();
        let renamed = grey_lsp::collect_file_rename(
            &path,
            Some(Path::new(&self.lsp_binary)),
            args.line,
            args.character,
            args.new_name,
        )
        .await
        .with_context(|| format!("running LSP rename for {file_path}"))?;
        let output = compact_text_tool_output("lsp_rename", &file_path, renamed)?;
        Ok(ToolResult::success(call, output))
    }

    async fn lsp_symbols(&self, call: &ToolCall) -> Result<ToolResult> {
        let args: LspSymbolsArgs = parse_args(call)?;
        let path = self.resolve_existing(&args.path)?;
        let file_path = path.to_string_lossy().into_owned();
        let symbols = grey_lsp::collect_file_symbols(&path, Some(Path::new(&self.lsp_binary)))
            .await
            .with_context(|| format!("running LSP symbols for {file_path}"))?;
        let max_items = normalized_max_items(args.max_items);
        let total = symbols.len();
        let mut compact = Vec::new();
        let mut seen = HashSet::new();
        for symbol in symbols.into_iter().take(max_items) {
            let key = (
                symbol.name.clone(),
                symbol.kind.clone(),
                symbol.start_line,
                symbol.start_character,
                symbol.end_line,
                symbol.end_character,
            );
            if seen.insert(key) {
                compact.push(LspSymbolOutput {
                    name: symbol.name,
                    kind: symbol.kind,
                    detail: symbol.detail,
                    container_name: symbol.container_name,
                    start_line: symbol.start_line,
                    start_character: symbol.start_character,
                    end_line: symbol.end_line,
                    end_character: symbol.end_character,
                });
            }
        }
        let output = compact_tool_output("lsp_symbols", &file_path, total, compact)?;
        Ok(ToolResult::success(call, output))
    }

    fn resolve_existing(&self, relative: &str) -> Result<PathBuf> {
        ensure_relative_path(relative)?;
        let candidate = self.workspace.join(relative);
        let canonical = candidate
            .canonicalize()
            .with_context(|| format!("resolving workspace path {relative:?}"))?;
        anyhow::ensure!(
            canonical.starts_with(&self.workspace),
            "path must remain inside the workspace"
        );
        Ok(canonical)
    }
}

fn diagnostic_code_to_string(code: &impl serde::Serialize) -> String {
    serde_json::to_string(code)
        .unwrap_or_default()
        .trim_matches('"')
        .to_string()
}

#[async_trait]
impl ToolExecutor for LspTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "lsp_diagnostics".into(),
                description: "Run LSP diagnostics for a workspace file and return findings.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "max_items": {"type": "integer", "minimum": 1, "maximum": 500}
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
                risk: ToolRisk::ReadOnly,
            },
            ToolDefinition {
                name: "lsp_definition".into(),
                description:
                    "Run LSP definition lookup for a workspace file and return target locations."
                        .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "line": {"type": "integer", "minimum": 1},
                        "character": {"type": "integer", "minimum": 1},
                        "max_items": {"type": "integer", "minimum": 1, "maximum": 500}
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
                risk: ToolRisk::ReadOnly,
            },
            ToolDefinition {
                name: "lsp_references".into(),
                description:
                    "Run LSP reference lookup for a workspace file and return project-wide usages."
                        .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "line": {"type": "integer", "minimum": 1},
                        "character": {"type": "integer", "minimum": 1},
                        "include_declaration": {"type": "boolean"},
                        "max_items": {"type": "integer", "minimum": 1, "maximum": 500}
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
                risk: ToolRisk::ReadOnly,
            },
            ToolDefinition {
                name: "lsp_hover".into(),
                description:
                    "Run LSP hover lookup for a workspace file and return hovered symbol documentation."
                        .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "line": {"type": "integer", "minimum": 1},
                        "character": {"type": "integer", "minimum": 1}
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
                risk: ToolRisk::ReadOnly,
            },
            ToolDefinition {
                name: "lsp_rename".into(),
                description: "Run LSP rename preview for a workspace file and return the proposed edit."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "line": {"type": "integer", "minimum": 1},
                        "character": {"type": "integer", "minimum": 1},
                        "new_name": {"type": "string"},
                    },
                    "required": ["path", "new_name"],
                    "additionalProperties": false
                }),
                risk: ToolRisk::ReadOnly,
            },
            ToolDefinition {
                name: "lsp_symbols".into(),
                description: "List symbols in a workspace file from LSP documentSymbol."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "max_items": {"type": "integer", "minimum": 1, "maximum": 500}
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
                risk: ToolRisk::ReadOnly,
            },
        ]
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        match self.dispatch(call).await {
            Ok(result) => result,
            Err(error) => ToolResult::failure(call, format!("{error:#}")),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
struct McpResponse {
    success: Option<bool>,
    output: Option<String>,
    error: Option<String>,
}

const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_SEARCH_RESULTS: usize = 500;

#[async_trait]
pub trait Approver: Send + Sync {
    async fn approve(&self, call: &ToolCall, risk: ToolRisk) -> bool;
}

pub struct AlwaysApprove;

#[async_trait]
impl Approver for AlwaysApprove {
    async fn approve(&self, _call: &ToolCall, _risk: ToolRisk) -> bool {
        true
    }
}

pub struct DenySideEffects;

#[async_trait]
impl Approver for DenySideEffects {
    async fn approve(&self, _call: &ToolCall, _risk: ToolRisk) -> bool {
        false
    }
}

/// Prompt on the controlling terminal. Non-interactive stdin denies safely.
pub struct StdioApprover;

#[async_trait]
impl Approver for StdioApprover {
    async fn approve(&self, call: &ToolCall, risk: ToolRisk) -> bool {
        if !std::io::stdin().is_terminal() {
            return false;
        }
        let summary = format!(
            "Grey requests {risk:?} tool {} with arguments {}. Approve? [y/N] ",
            call.name, call.arguments
        );
        tokio::task::spawn_blocking(move || {
            eprint!("{summary}");
            let _ = std::io::stderr().flush();
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer).is_ok()
                && matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
        })
        .await
        .unwrap_or(false)
    }
}

#[derive(Clone)]
pub struct HookedApprover {
    inner: Arc<dyn Approver>,
    hooks: HookRunner,
    workspace: PathBuf,
    provider: String,
    model: String,
}

impl HookedApprover {
    pub fn new(
        inner: Arc<dyn Approver>,
        hooks: HookRunner,
        workspace: &Path,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            inner,
            hooks,
            workspace: workspace.to_path_buf(),
            provider: provider.into(),
            model: model.into(),
        }
    }
}

#[async_trait]
impl Approver for HookedApprover {
    async fn approve(&self, call: &ToolCall, risk: ToolRisk) -> bool {
        let approved = self.inner.approve(call, risk).await;
        let mut payload = HookPayload::new(HookEvent::PermissionDecision, &self.workspace);
        payload.provider = Some(&self.provider);
        payload.model = Some(&self.model);
        payload.tool = Some(HookTool {
            name: &call.name,
            risk,
        });
        let hook_approved = self.hooks.run_gate(payload).await.unwrap_or(false);
        approved && hook_approved
    }
}

#[derive(Clone)]
pub struct CombinedTools {
    executors: Vec<Arc<dyn ToolExecutor>>,
}

impl CombinedTools {
    pub fn new(executors: Vec<Arc<dyn ToolExecutor>>) -> Self {
        Self { executors }
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.executors
            .iter()
            .flat_map(|executor| executor.definitions())
            .map(|definition| definition.name)
            .collect()
    }
}

#[async_trait]
impl ToolExecutor for CombinedTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.executors
            .iter()
            .flat_map(|executor| executor.definitions().into_iter())
            .collect()
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        for executor in &self.executors {
            if executor
                .definitions()
                .iter()
                .any(|definition| definition.name == call.name)
            {
                return executor.execute(call).await;
            }
        }
        ToolResult::failure(call, format!("unknown tool {}", call.name))
    }
}

pub struct HookedTools {
    inner: Arc<dyn ToolExecutor>,
    approver: Arc<dyn Approver>,
    hooks: HookRunner,
    workspace: PathBuf,
    provider: String,
    model: String,
}

impl HookedTools {
    pub fn new(
        inner: Arc<dyn ToolExecutor>,
        approver: Arc<dyn Approver>,
        hooks: HookRunner,
        workspace: &Path,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            inner,
            approver,
            hooks,
            workspace: workspace.to_path_buf(),
            provider: provider.into(),
            model: model.into(),
        }
    }
}

#[async_trait]
impl ToolExecutor for HookedTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.inner.definitions()
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        let risk = self
            .inner
            .definitions()
            .into_iter()
            .find(|definition| definition.name == call.name)
            .map(|definition| definition.risk)
            .unwrap_or(ToolRisk::ReadOnly);
        if risk != ToolRisk::ReadOnly && !self.approver.approve(call, risk).await {
            return ToolResult::failure(call, format!("{} denied by approval policy", call.name));
        }

        let mut pre_payload = HookPayload::new(HookEvent::PreToolCall, &self.workspace);
        pre_payload.provider = Some(&self.provider);
        pre_payload.model = Some(&self.model);
        pre_payload.tool = Some(HookTool {
            name: &call.name,
            risk,
        });
        match self.hooks.run_gate(pre_payload).await {
            Ok(true) => {}
            Ok(false) => {
                return ToolResult::failure(
                    call,
                    format!("pre_tool_call hook denied tool {}", call.name),
                )
            }
            Err(error) => {
                return ToolResult::failure(
                    call,
                    format!("pre_tool_call hook denied tool {}: {error}", call.name),
                )
            }
        }

        let result = self.inner.execute(call).await;
        let mut post_payload = HookPayload::new(HookEvent::PostToolCall, &self.workspace);
        post_payload.provider = Some(&self.provider);
        post_payload.model = Some(&self.model);
        post_payload.tool = Some(HookTool {
            name: &call.name,
            risk,
        });
        post_payload.success = Some(result.success);
        if let Err(error) = self.hooks.run_best_effort(post_payload).await {
            eprintln!("post_tool_call hook failed: {error:#}");
        }
        result
    }
}

#[derive(Debug, Clone)]
pub struct McpTools {
    tools: Vec<McpTool>,
}

impl McpTools {
    pub fn new(configured: Vec<McpToolConfig>) -> Self {
        Self {
            tools: configured
                .into_iter()
                .filter(|tool| !tool.name.is_empty() && !tool.command.is_empty())
                .map(|tool| McpTool {
                    name: tool.name,
                    command: tool.command,
                    description: tool.description,
                    args: tool.args,
                    input_schema: tool.input_schema,
                    timeout_ms: tool.timeout_ms,
                })
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[derive(Debug, Clone)]
struct McpProtocolTool {
    server: String,
    name: String,
    description: String,
    input_schema: Value,
}

struct McpConnection {
    child: Option<Box<dyn ChildWrapper>>,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    timeout: Duration,
}

/// Stdio-only MCP client. One mutex per server intentionally serializes JSON-RPC
/// requests, because a single stdio stream has no safe concurrent reader.
pub struct McpServers {
    connections: HashMap<String, Arc<Mutex<McpConnection>>>,
    tools: Vec<McpProtocolTool>,
}

impl McpServers {
    pub async fn connect(workspace: &Path, configured: Vec<McpServerConfig>) -> Result<Self> {
        let workspace = workspace
            .canonicalize()
            .context("canonicalizing MCP workspace")?;
        anyhow::ensure!(workspace.is_dir(), "MCP workspace must be a directory");
        let mut connections = HashMap::new();
        let mut tools = Vec::new();
        for server in configured {
            validate_mcp_server(&server)?;
            anyhow::ensure!(
                !connections.contains_key(&server.id),
                "duplicate MCP server id: {}",
                server.id
            );
            let mut command = CommandWrap::with_new(&server.command, |command| {
                command
                    .args(&server.args)
                    .current_dir(&workspace)
                    .env_clear()
                    .envs(&server.env)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null());
            });
            command.wrap(KillOnDrop);
            #[cfg(unix)]
            command.wrap(ProcessGroup::leader());
            #[cfg(windows)]
            command.wrap(JobObject);
            let mut child = command
                .spawn()
                .with_context(|| format!("spawning MCP server {}", server.id))?;
            let stdin = child.stdin().take().context("opening MCP server stdin")?;
            let stdout =
                BufReader::new(child.stdout().take().context("opening MCP server stdout")?);
            let timeout = Duration::from_millis(server.timeout_ms.unwrap_or(DEFAULT_TOOL_TIMEOUT));
            let mut connection = McpConnection {
                child: Some(child),
                stdin,
                stdout,
                next_id: 1,
                timeout,
            };
            let initialized = connection
                .request(
                    "initialize",
                    json!({
                        "protocolVersion": MCP_PROTOCOL_VERSION,
                        "capabilities": {},
                        "clientInfo": { "name": "grey", "version": env!("CARGO_PKG_VERSION") }
                    }),
                )
                .await
                .with_context(|| format!("initializing MCP server {}", server.id))?;
            let version = initialized
                .get("protocolVersion")
                .and_then(Value::as_str)
                .context("MCP initialize response lacks protocolVersion")?;
            anyhow::ensure!(
                version == MCP_PROTOCOL_VERSION,
                "MCP server {} selected unsupported protocol version {version}",
                server.id
            );
            connection
                .notify("notifications/initialized", json!({}))
                .await?;
            let connection = Arc::new(Mutex::new(connection));
            let discovered = list_mcp_tools(&connection)
                .await
                .with_context(|| format!("listing MCP tools for {}", server.id))?;
            for tool in discovered {
                tools.push(McpProtocolTool {
                    name: format!("{}__{}", server.id, tool.name),
                    server: server.id.clone(),
                    description: tool.description.unwrap_or_else(|| "MCP tool".into()),
                    input_schema: tool.input_schema,
                });
            }
            connections.insert(server.id, connection);
        }
        Ok(Self { connections, tools })
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[derive(Deserialize)]
struct McpListedTool {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(rename = "inputSchema", default = "empty_schema")]
    input_schema: Value,
}
fn empty_schema() -> Value {
    json!({"type":"object","properties":{}})
}

async fn list_mcp_tools(connection: &Arc<Mutex<McpConnection>>) -> Result<Vec<McpListedTool>> {
    let mut cursor: Option<String> = None;
    let mut all = Vec::new();
    loop {
        let response = connection
            .lock()
            .await
            .request("tools/list", json!({"cursor": cursor}))
            .await?;
        let mut page: Vec<McpListedTool> =
            serde_json::from_value(response.get("tools").cloned().unwrap_or(Value::Null))
                .context("invalid MCP tools/list tools")?;
        all.append(&mut page);
        cursor = response
            .get("nextCursor")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if cursor.is_none() {
            return Ok(all);
        }
    }
}

fn validate_mcp_server(server: &McpServerConfig) -> Result<()> {
    anyhow::ensure!(
        !server.id.trim().is_empty(),
        "MCP server id must not be empty"
    );
    anyhow::ensure!(
        server.transport == "stdio",
        "MCP server {}: only stdio transport is supported",
        server.id
    );
    anyhow::ensure!(
        !server.command.trim().is_empty(),
        "MCP server {} command must not be empty",
        server.id
    );
    anyhow::ensure!(
        !server.command.contains("://"),
        "MCP server {} must be a direct command, not a URL",
        server.id
    );
    anyhow::ensure!(
        server.timeout_ms.unwrap_or(DEFAULT_TOOL_TIMEOUT) > 0,
        "MCP server {} timeout must be positive",
        server.id
    );
    Ok(())
}

impl McpConnection {
    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.write(json!({"jsonrpc":"2.0", "method": method, "params": params}))
            .await
    }
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(json!({"jsonrpc":"2.0", "id": id, "method": method, "params": params}))
            .await?;
        let response = match tokio::time::timeout(self.timeout, self.read_response(id)).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                self.stop().await;
                return Err(error);
            }
            Err(_) => {
                self.stop().await;
                bail!(
                    "MCP request {method} timed out after {}ms",
                    self.timeout.as_millis()
                );
            }
        };
        if let Some(error) = response.get("error") {
            bail!("MCP {method} failed: {error}");
        }
        response
            .get("result")
            .cloned()
            .context("MCP response lacks result")
    }
    async fn write(&mut self, value: Value) -> Result<()> {
        let text = serde_json::to_vec(&value)?;
        anyhow::ensure!(
            text.len() <= MCP_LINE_LIMIT,
            "MCP request exceeds {MCP_LINE_LIMIT} bytes"
        );
        self.stdin.write_all(&text).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(())
    }
    async fn read_response(&mut self, id: u64) -> Result<Value> {
        let mut line = Vec::new();
        loop {
            line.clear();
            let count = (&mut self.stdout)
                .take((MCP_LINE_LIMIT + 1) as u64)
                .read_until(b'\n', &mut line)
                .await?;
            anyhow::ensure!(count != 0, "MCP server closed stdout");
            anyhow::ensure!(
                line.len() <= MCP_LINE_LIMIT,
                "MCP response exceeds {MCP_LINE_LIMIT} bytes"
            );
            let response: Value =
                serde_json::from_slice(&line).context("malformed MCP JSONL response")?;
            if response.get("id").and_then(Value::as_u64) == Some(id) {
                return Ok(response);
            }
        }
    }
    async fn stop(&mut self) {
        let _ = self.stdin.shutdown().await;
        if let Some(child) = self.child.take() {
            let _ = terminate_and_reap_mcp(child).await;
        }
    }
}

async fn terminate_and_reap_mcp(mut child: Box<dyn ChildWrapper>) -> Result<()> {
    #[cfg(unix)]
    if child.try_wait()?.is_none() {
        if let Err(error) = child.signal(15) {
            if child.try_wait()?.is_none() {
                return Err(error).context("terminating MCP process group");
            }
        }
        match tokio::time::timeout(MCP_TERM_WAIT, child.wait()).await {
            Ok(Ok(_)) => return Ok(()),
            Ok(Err(error)) => return Err(error).context("reaping MCP process group after SIGTERM"),
            Err(_) => {}
        }
    }
    if child.try_wait()?.is_none() {
        child.start_kill().context("killing MCP process tree")?;
    }
    tokio::time::timeout(MCP_REAP_WAIT, child.wait())
        .await
        .context("reaping MCP process tree timed out")??;
    Ok(())
}

impl Drop for McpConnection {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = terminate_and_reap_mcp(child).await;
                });
            } else {
                let _ = child.start_kill();
            }
        }
    }
}

#[async_trait]
impl ToolExecutor for McpServers {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .map(|tool| ToolDefinition {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: tool.input_schema.clone(),
                risk: ToolRisk::ReadOnly,
            })
            .collect()
    }
    async fn execute(&self, call: &ToolCall) -> ToolResult {
        let Some(tool) = self.tools.iter().find(|tool| tool.name == call.name) else {
            return ToolResult::failure(call, format!("unknown MCP tool {}", call.name));
        };
        let Some(connection) = self.connections.get(&tool.server) else {
            return ToolResult::failure(call, "MCP server connection unavailable");
        };
        let result = connection.lock().await.request("tools/call", json!({"name": tool.name.split_once("__").map(|(_, name)| name).unwrap_or(&tool.name), "arguments": call.arguments})).await;
        match result {
            Ok(value) => match serde_json::to_string(&value) {
                Ok(output) if output.len() <= MCP_RESULT_LIMIT => ToolResult::success(call, output),
                Ok(_) => ToolResult::failure(
                    call,
                    format!("MCP tool result exceeds {MCP_RESULT_LIMIT} bytes"),
                ),
                Err(error) => ToolResult::failure(call, format!("serializing MCP result: {error}")),
            },
            Err(error) => {
                ToolResult::failure(call, format!("MCP tool {} failed: {error:#}", call.name))
            }
        }
    }
}

#[derive(Debug, Clone)]
struct PluginTool {
    id: String,
    name: String,
    command: String,
    args: Vec<String>,
    description: String,
    timeout_ms: Option<u64>,
    risk: ToolRisk,
    wasm: Option<WasmPlugin>,
}

#[derive(Clone)]
pub struct PluginTools {
    workspace: PathBuf,
    approver: Arc<dyn Approver>,
    tools: Vec<PluginTool>,
}

impl PluginTools {
    pub fn new(
        workspace: &Path,
        configured: Vec<PluginConfig>,
        approver: Arc<dyn Approver>,
    ) -> Self {
        let configured = configured
            .into_iter()
            .filter(|plugin| {
                plugin.runtime == PluginRuntime::Command && !plugin.command.trim().is_empty()
            })
            .collect();
        Self::new_with_runtime(
            workspace,
            configured,
            approver,
            &RuntimeConfig::default(),
            workspace,
        )
        .expect("filtered command plugin configuration must be valid")
    }

    pub fn new_with_runtime(
        workspace: &Path,
        configured: Vec<PluginConfig>,
        approver: Arc<dyn Approver>,
        runtime: &RuntimeConfig,
        config_dir: &Path,
    ) -> Result<Self> {
        let workspace = workspace.to_path_buf();
        let tools = configured
            .into_iter()
            .filter(|plugin| {
                plugin.enabled
                    && matches!(plugin.kind, grey_core::PluginKind::Tool)
                    && !plugin.id.trim().is_empty()
            })
            .map(|plugin| -> Result<_> {
                let wasm = if plugin.runtime == PluginRuntime::Wasm {
                    Some(
                        WasmPlugin::from_config(&plugin, config_dir, runtime).map_err(|error| {
                            anyhow::anyhow!("invalid wasm tool plugin `{}`: {error}", plugin.id)
                        })?,
                    )
                } else {
                    anyhow::ensure!(
                        !plugin.command.trim().is_empty(),
                        "tool plugin `{}` has no command",
                        plugin.id
                    );
                    None
                };
                let name = plugin
                    .name
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| plugin.id.clone());
                Ok(PluginTool {
                    id: plugin.id,
                    name,
                    command: plugin.command,
                    args: plugin.args,
                    description: plugin
                        .description
                        .unwrap_or_else(|| "Plugin tool".to_string()),
                    timeout_ms: plugin.timeout_ms,
                    risk: ToolRisk::Execute,
                    wasm,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            workspace,
            approver,
            tools,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[async_trait]
impl ToolExecutor for PluginTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .map(|tool| ToolDefinition {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": true,
                }),
                risk: tool.risk,
            })
            .collect()
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        let Some(tool) = self.tools.iter().find(|tool| tool.name == call.name) else {
            return ToolResult::failure(call, format!("unknown plugin tool {}", call.name));
        };

        if !self.approver.approve(call, tool.risk).await {
            return ToolResult::failure(call, "plugin tool denied by approval policy");
        }

        let request = json!({
            "event": "plugin_call",
            "plugin_id": &tool.id,
            "tool": call.name,
            "id": call.id,
            "arguments": call.arguments,
        })
        .to_string();

        let raw_result: Result<String> = if let Some(wasm) = &tool.wasm {
            match wasm.invoke(request.into_bytes()).await {
                Ok(output) if output.stdout_truncated || output.stderr_truncated => Err(
                    anyhow::anyhow!("wasm plugin output exceeds configured limit"),
                ),
                Ok(output) => String::from_utf8(output.stdout).map_err(Into::into),
                Err(error) => Err(error.into()),
            }
        } else {
            execute_command_in_dir(
                &tool.command,
                &tool.args,
                Some(&request),
                tool.timeout_ms.unwrap_or(DEFAULT_TOOL_TIMEOUT),
                &self.workspace,
            )
            .await
        };
        let raw = match raw_result {
            Ok(output) => output,
            Err(error) => {
                return ToolResult::failure(
                    call,
                    format!("plugin {} command failed: {}", tool.name, error),
                )
            }
        };

        let parsed = serde_json::from_str::<McpResponse>(&raw).unwrap_or_else(|_| McpResponse {
            success: None,
            output: Some(raw.clone()),
            error: None,
        });

        if parsed.success.unwrap_or(true) {
            ToolResult::success(call, truncate_output(parsed.output.unwrap_or(raw)))
        } else {
            ToolResult::failure(call, parsed.error.unwrap_or(raw))
        }
    }
}

impl McpTools {
    fn schema_for(tool: &McpTool) -> Value {
        tool.input_schema.clone().unwrap_or_else(|| {
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": true,
            })
        })
    }
}

#[async_trait]
impl ToolExecutor for McpTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .map(|tool| ToolDefinition {
                name: tool.name.clone(),
                description: tool
                    .description
                    .clone()
                    .unwrap_or_else(|| "MCP command tool".into()),
                input_schema: Self::schema_for(tool),
                risk: ToolRisk::ReadOnly,
            })
            .collect()
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        let tool = self.tools.iter().find(|tool| tool.name == call.name);
        let Some(tool) = tool else {
            return ToolResult::failure(call, format!("unknown MCP tool {}", call.name));
        };
        let request = json!({
            "id": call.id,
            "name": call.name,
            "arguments": call.arguments,
        })
        .to_string();

        let output = match execute_command(
            &tool.command,
            &tool.args,
            Some(&request),
            tool.timeout_ms.unwrap_or(DEFAULT_TOOL_TIMEOUT),
        )
        .await
        {
            Ok(output) => output,
            Err(error) => {
                return ToolResult::failure(
                    call,
                    format!("MCP tool {} command failed: {}", call.name, error),
                )
            }
        };
        let parsed = serde_json::from_str::<McpResponse>(&output).unwrap_or_else(|_| McpResponse {
            success: None,
            output: Some(output.clone()),
            error: None,
        });
        if parsed.success.unwrap_or(true) {
            ToolResult::success(call, parsed.output.unwrap_or(output))
        } else {
            ToolResult::failure(
                call,
                parsed
                    .error
                    .unwrap_or_else(|| parsed.output.unwrap_or(output)),
            )
        }
    }
}

pub struct BuiltinTools {
    workspace: PathBuf,
    approver: Arc<dyn Approver>,
    max_command_duration: Duration,
}

impl BuiltinTools {
    pub fn new(workspace: &Path, approver: Arc<dyn Approver>) -> Result<Self> {
        let workspace = workspace
            .canonicalize()
            .with_context(|| format!("canonicalizing workspace {}", workspace.display()))?;
        anyhow::ensure!(workspace.is_dir(), "workspace must be a directory");
        Ok(Self {
            workspace,
            approver,
            max_command_duration: Duration::from_secs(120),
        })
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn set_max_command_duration(&mut self, duration: Duration) {
        self.max_command_duration = duration.max(Duration::from_millis(1));
    }

    async fn dispatch(&self, call: &ToolCall) -> Result<ToolResult> {
        match call.name.as_str() {
            "read_file" => self.read_file(call),
            "edit_file" => self.edit_file(call).await,
            "bash" => self.bash(call).await,
            "glob" => self.glob(call),
            "grep" => self.grep(call),
            _ => bail!("unknown tool {:?}", call.name),
        }
    }

    fn read_file(&self, call: &ToolCall) -> Result<ToolResult> {
        let args: ReadFileArgs = parse_args(call)?;
        let path = self.resolve_existing(&args.path)?;
        anyhow::ensure!(path.is_file(), "read_file path is not a file");
        let mut file = std::fs::File::open(&path)?;
        let mut content = String::new();
        file.read_to_string(&mut content)
            .with_context(|| format!("reading UTF-8 file {}", path.display()))?;
        let offset = args.offset.unwrap_or(1).max(1);
        let limit = args.limit.unwrap_or(2_000).min(10_000);
        let selected = content
            .split_inclusive('\n')
            .skip(offset - 1)
            .take(limit)
            .collect::<String>();
        Ok(ToolResult::success(call, truncate_output(selected)))
    }

    async fn edit_file(&self, call: &ToolCall) -> Result<ToolResult> {
        let args: EditFileArgs = parse_args(call)?;
        anyhow::ensure!(!args.old_string.is_empty(), "old_string must not be empty");
        if !self.approver.approve(call, ToolRisk::Write).await {
            return Ok(ToolResult::failure(
                call,
                "edit_file denied by approval policy",
            ));
        }

        let path = self.resolve_existing(&args.path)?;
        anyhow::ensure!(path.is_file(), "edit_file path is not a file");
        let original = std::fs::read_to_string(&path)
            .with_context(|| format!("reading UTF-8 file {}", path.display()))?;
        let matches = original.match_indices(&args.old_string).count();
        anyhow::ensure!(
            matches == 1,
            "old_string must occur exactly once, found {matches} matches"
        );
        let updated = original.replacen(&args.old_string, &args.new_string, 1);
        let parent = path
            .parent()
            .context("edited file has no parent directory")?;
        let mut temporary = NamedTempFile::new_in(parent)?;
        temporary.as_file_mut().write_all(updated.as_bytes())?;
        temporary.as_file_mut().flush()?;
        temporary.as_file().sync_all()?;
        let permissions = std::fs::metadata(&path)?.permissions();
        std::fs::set_permissions(temporary.path(), permissions)?;
        temporary
            .persist(&path)
            .map_err(|error| error.error)
            .with_context(|| format!("atomically replacing {}", path.display()))?;
        Ok(ToolResult::success(
            call,
            format!("updated {} with one exact replacement", args.path),
        ))
    }

    async fn bash(&self, call: &ToolCall) -> Result<ToolResult> {
        let args: BashArgs = parse_args(call)?;
        anyhow::ensure!(!args.command.trim().is_empty(), "command must not be empty");
        if !self.approver.approve(call, ToolRisk::Execute).await {
            return Ok(ToolResult::failure(call, "bash denied by approval policy"));
        }
        let requested = Duration::from_millis(args.timeout_ms.unwrap_or(u64::MAX));
        let timeout = requested.min(self.max_command_duration);
        let spec = command_with_runtime_env(CommandSpec::legacy_shell(&args.command))
            .current_dir(&self.workspace)
            .timeout(timeout);
        let output = run_bounded(spec).await.context("running shell command")?;
        let text = truncate_output(output.combined_lossy());
        if output.status.success() {
            Ok(ToolResult::success(call, text))
        } else {
            Ok(ToolResult::failure(
                call,
                format!("command exited with {}\n{text}", output.status),
            ))
        }
    }

    fn glob(&self, call: &ToolCall) -> Result<ToolResult> {
        let args: GlobArgs = parse_args(call)?;
        let base = self.resolve_search_base(args.path.as_deref())?;
        let matcher = compile_glob(&args.pattern)?;
        let mut paths = self
            .walk_files(&base)
            .filter_map(Result::ok)
            .filter_map(|entry| self.relative_path(entry.path()).ok())
            .filter(|relative| matcher.is_match(relative))
            .map(|relative| display_relative(&relative))
            .collect::<Vec<_>>();
        paths.sort();
        paths.truncate(MAX_SEARCH_RESULTS);
        let output = paths.into_iter().map(|path| format!("{path}\n")).collect();
        Ok(ToolResult::success(call, truncate_output(output)))
    }

    fn grep(&self, call: &ToolCall) -> Result<ToolResult> {
        let args: GrepArgs = parse_args(call)?;
        let regex = Regex::new(&args.pattern).context("invalid grep regular expression")?;
        let base = self.resolve_search_base(args.path.as_deref())?;
        let glob = args.glob.as_deref().map(compile_glob).transpose()?;
        let mut matches = Vec::new();

        for entry in self.walk_files(&base).filter_map(Result::ok) {
            let relative = self.relative_path(entry.path())?;
            if glob
                .as_ref()
                .is_some_and(|matcher| !matcher.is_match(&relative))
            {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            for (index, line) in content.lines().enumerate() {
                if regex.is_match(line) {
                    matches.push(format!(
                        "{}:{}:{}\n",
                        display_relative(&relative),
                        index + 1,
                        line
                    ));
                    if matches.len() >= MAX_SEARCH_RESULTS {
                        break;
                    }
                }
            }
            if matches.len() >= MAX_SEARCH_RESULTS {
                break;
            }
        }
        Ok(ToolResult::success(call, truncate_output(matches.concat())))
    }

    fn resolve_existing(&self, relative: &str) -> Result<PathBuf> {
        ensure_relative_path(relative)?;
        let candidate = self.workspace.join(relative);
        let canonical = candidate
            .canonicalize()
            .with_context(|| format!("resolving workspace path {relative:?}"))?;
        anyhow::ensure!(
            canonical.starts_with(&self.workspace),
            "path must remain inside the workspace"
        );
        Ok(canonical)
    }

    fn resolve_search_base(&self, relative: Option<&str>) -> Result<PathBuf> {
        match relative {
            None | Some("") | Some(".") => Ok(self.workspace.clone()),
            Some(relative) => self.resolve_existing(relative),
        }
    }

    fn walk_files(
        &self,
        base: &Path,
    ) -> impl Iterator<Item = std::result::Result<ignore::DirEntry, ignore::Error>> {
        WalkBuilder::new(base)
            .hidden(false)
            .follow_links(false)
            .standard_filters(true)
            .build()
            .filter(|entry| {
                entry
                    .as_ref()
                    .is_ok_and(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
            })
    }

    fn relative_path(&self, path: &Path) -> Result<PathBuf> {
        path.strip_prefix(&self.workspace)
            .map(Path::to_path_buf)
            .context("search result escaped the workspace")
    }
}

#[async_trait]
impl ToolExecutor for BuiltinTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        definitions()
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        match self.dispatch(call).await {
            Ok(result) => result,
            Err(error) => ToolResult::failure(call, format!("{error:#}")),
        }
    }
}

fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "read_file".into(),
            description: "Read a UTF-8 file inside the workspace, optionally by line range.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer", "minimum": 1},
                    "limit": {"type": "integer", "minimum": 1}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            risk: ToolRisk::ReadOnly,
        },
        ToolDefinition {
            name: "edit_file".into(),
            description: "Atomically replace one exact string in an existing workspace file."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"}
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }),
            risk: ToolRisk::Write,
        },
        ToolDefinition {
            name: "bash".into(),
            description: "Run a shell command in the workspace with a bounded timeout and output."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout_ms": {"type": "integer", "minimum": 1}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
            risk: ToolRisk::Execute,
        },
        ToolDefinition {
            name: "glob".into(),
            description: "List workspace files matching a glob while respecting ignore files."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string"}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
            risk: ToolRisk::ReadOnly,
        },
        ToolDefinition {
            name: "grep".into(),
            description: "Search UTF-8 workspace files with a regular expression and line numbers."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string"},
                    "glob": {"type": "string"}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
            risk: ToolRisk::ReadOnly,
        },
    ]
}

fn parse_args<T: DeserializeOwned>(call: &ToolCall) -> Result<T> {
    serde_json::from_value(call.arguments.clone())
        .with_context(|| format!("invalid arguments for {}", call.name))
}

fn ensure_relative_path(path: &str) -> Result<()> {
    let path = Path::new(path);
    anyhow::ensure!(!path.as_os_str().is_empty(), "path must not be empty");
    anyhow::ensure!(!path.is_absolute(), "path must remain inside the workspace");
    anyhow::ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir)),
        "path must remain inside the workspace"
    );
    Ok(())
}

fn compile_glob(pattern: &str) -> Result<GlobMatcher> {
    Ok(Glob::new(pattern)
        .context("invalid glob pattern")?
        .compile_matcher())
}

fn display_relative(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn truncate_output(mut output: String) -> String {
    if output.len() <= MAX_OUTPUT_BYTES {
        return output;
    }
    let mut boundary = MAX_OUTPUT_BYTES;
    while !output.is_char_boundary(boundary) {
        boundary -= 1;
    }
    output.truncate(boundary);
    output.push_str("\n[output truncated by Grey]\n");
    output
}

fn normalized_max_items(value: Option<usize>) -> usize {
    value
        .unwrap_or(LSP_TOOL_DEFAULT_MAX_ITEMS)
        .clamp(1, LSP_TOOL_MAX_ITEMS)
}

fn compact_tool_output<T: Serialize>(
    tool: &'static str,
    path: &str,
    total: usize,
    compact: Vec<T>,
) -> Result<String> {
    serde_json::to_string(&LspToolOutput {
        tool,
        path: path.to_string(),
        count: total,
        shown: compact.len(),
        truncated: compact.len() < total,
        compact,
    })
    .map_err(|error| anyhow::anyhow!("failed to serialize {tool} output: {error}"))
}

fn compact_text_tool_output(
    tool: &'static str,
    path: &str,
    text: impl Into<String>,
) -> Result<String> {
    let output = compact_tool_output(
        tool,
        path,
        1,
        vec![LspTextOutput {
            text: truncate_output(text.into()),
        }],
    )?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_tool_output_marks_truncated_items() {
        let output = compact_tool_output(
            "lsp_symbols",
            "src/main.rs",
            12,
            vec![LspTextOutput {
                text: "symbol".into(),
            }],
        )
        .expect("tool output should serialize");
        let value: serde_json::Value =
            serde_json::from_str(&output).expect("output should be JSON");
        assert_eq!(value["tool"], "lsp_symbols");
        assert_eq!(value["path"], "src/main.rs");
        assert_eq!(value["count"], 12);
        assert_eq!(value["shown"], 1);
        assert_eq!(value["truncated"], true);
    }

    #[test]
    fn normalized_max_items_clamps_input() {
        assert_eq!(normalized_max_items(None), 50);
        assert_eq!(normalized_max_items(Some(0)), 1);
        assert_eq!(normalized_max_items(Some(1000)), 500);
    }
}

async fn execute_command(
    command: &str,
    args: &[String],
    input: Option<&str>,
    timeout_ms: u64,
) -> Result<String> {
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    run_command_inner(command, &args, input, timeout_ms, None).await
}

async fn execute_command_in_dir(
    command: &str,
    args: &[String],
    input: Option<&str>,
    timeout_ms: u64,
    workspace: &Path,
) -> Result<String> {
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    run_command_inner(command, &args, input, timeout_ms, Some(workspace)).await
}

async fn run_command_inner(
    command: &str,
    args: &[&str],
    input: Option<&str>,
    timeout_ms: u64,
    workspace: Option<&Path>,
) -> Result<String> {
    let mut spec = command_with_runtime_env(CommandSpec::direct(command, args.iter().copied()))
        .timeout(Duration::from_millis(timeout_ms));
    if let Some(workspace) = workspace {
        spec = spec.current_dir(workspace);
    }
    if let Some(input) = input {
        spec = spec.stdin(input.as_bytes().to_vec());
    }
    run_command_spec(spec).await
}

async fn run_command_spec(spec: CommandSpec) -> Result<String> {
    let output = run_bounded(spec).await?;
    let text = output.combined_lossy();
    if output.status.success() {
        Ok(text)
    } else {
        bail!("{}", text.trim());
    }
}

fn command_with_runtime_env(mut spec: CommandSpec) -> CommandSpec {
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
    spec
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadFileArgs {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditFileArgs {
    path: String,
    old_string: String,
    new_string: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashArgs {
    command: String,
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobArgs {
    pattern: String,
    path: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrepArgs {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LspDiagnosticsArgs {
    path: String,
    max_items: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LspDefinitionArgs {
    path: String,
    line: Option<u32>,
    character: Option<u32>,
    max_items: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LspPositionArgs {
    path: String,
    line: Option<u32>,
    character: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LspReferencesArgs {
    path: String,
    line: Option<u32>,
    character: Option<u32>,
    include_declaration: Option<bool>,
    max_items: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LspRenameArgs {
    path: String,
    line: Option<u32>,
    character: Option<u32>,
    new_name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LspSymbolsArgs {
    path: String,
    max_items: Option<usize>,
}
