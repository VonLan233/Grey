//! Workspace-scoped built-in tools with explicit approval for side effects.

use std::io::{IsTerminal, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use globset::{Glob, GlobMatcher};
use grey_core::{McpToolConfig, ToolCall, ToolDefinition, ToolExecutor, ToolResult, ToolRisk};
use ignore::WalkBuilder;
use regex::Regex;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use tempfile::NamedTempFile;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub const BUILTIN_TOOL_NAMES: [&str; 5] = ["read_file", "edit_file", "bash", "glob", "grep"];
const DEFAULT_HOOK_TIMEOUT: u64 = 10_000;
const DEFAULT_TOOL_TIMEOUT: u64 = 5_000;

#[derive(Debug, Clone)]
pub struct McpTool {
    pub name: String,
    pub command: String,
    pub description: Option<String>,
    pub args: Vec<String>,
    pub input_schema: Option<Value>,
    pub timeout_ms: Option<u64>,
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
        let mut output = String::new();
        output.push_str(&format!(
            "lsp diagnostics for {} ({}):\n",
            file_path,
            diagnostics.len()
        ));
        let max_items = args.max_items.unwrap_or(50);
        for diagnostic in diagnostics.into_iter().take(max_items) {
            let range = diagnostic.range;
            let severity = diagnostic
                .severity
                .map(|severity| format!("{severity:?}"))
                .unwrap_or_else(|| "unknown".into());
            output.push_str(&format!(
                "[{severity}] {}:{} {}\n",
                range.start.line + 1,
                range.start.character + 1,
                diagnostic.message.replace('\n', " ")
            ));
        }
        if output.ends_with('\n') {
            output.pop();
        }
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
        let mut output = String::new();
        output.push_str(&format!(
            "lsp definition for {} ({}):\n",
            file_path,
            definitions.len()
        ));
        let max_items = args.max_items.unwrap_or(50);
        for definition in definitions.into_iter().take(max_items) {
            output.push_str(&format!(
                "{}:{}:{} -> {}:{}\n",
                definition.uri,
                definition.start_line,
                definition.start_character,
                definition.end_line,
                definition.end_character
            ));
        }
        if output.ends_with('\n') {
            output.pop();
        }
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
    pre_tool_hooks: Vec<String>,
    post_tool_hooks: Vec<String>,
}

impl HookedTools {
    pub fn new(
        inner: Arc<dyn ToolExecutor>,
        pre_tool_hooks: Vec<String>,
        post_tool_hooks: Vec<String>,
    ) -> Self {
        Self {
            inner,
            pre_tool_hooks,
            post_tool_hooks,
        }
    }
}

#[async_trait]
impl ToolExecutor for HookedTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.inner.definitions()
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        if let Err(error) = run_hook_chain(&self.pre_tool_hooks, "pre_tool_call", call).await {
            return ToolResult::failure(
                call,
                format!("pre_tool_call hook denied tool {}: {error}", call.name),
            );
        }

        let mut result = self.inner.execute(call).await;
        if let Err(error) = run_hook_chain(&self.post_tool_hooks, "post_tool_call", call).await {
            result.success = false;
            result.output = format!("{error}\n{}", result.output);
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
        let mut command = Command::new("sh");
        command
            .arg("-lc")
            .arg(&args.command)
            .current_dir(&self.workspace)
            .kill_on_drop(true);
        let output = match tokio::time::timeout(timeout, command.output()).await {
            Ok(output) => output.context("running shell command")?,
            Err(_) => {
                return Ok(ToolResult::failure(
                    call,
                    format!("command timed out after {}ms", timeout.as_millis()),
                ))
            }
        };
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        if !output.stderr.is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        let text = truncate_output(text);
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

async fn run_hook_chain(commands: &[String], event: &str, call: &ToolCall) -> Result<(), String> {
    if commands.is_empty() {
        return Ok(());
    }
    let payload = json!({
        "event": event,
        "tool": {
            "id": call.id,
            "name": call.name,
            "arguments": call.arguments,
        }
    })
    .to_string();
    for command in commands {
        if let Err(error) = run_shell_command(command, Some(&payload), DEFAULT_HOOK_TIMEOUT).await {
            return Err(format!(
                "{event} hook failed for tool {} with command `{command}`: {error}",
                call.name
            ));
        }
    }
    Ok(())
}

async fn run_shell_command(command: &str, input: Option<&str>, timeout_ms: u64) -> Result<String> {
    run_command_inner("sh", &["-lc", command], input, timeout_ms).await
}

async fn execute_command(
    command: &str,
    args: &[String],
    input: Option<&str>,
    timeout_ms: u64,
) -> Result<String> {
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    run_command_inner(command, &args, input, timeout_ms).await
}

async fn run_command_inner(
    command: &str,
    args: &[&str],
    input: Option<&str>,
    timeout_ms: u64,
) -> Result<String> {
    let mut command = Command::new(command);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .kill_on_drop(true);
    let mut child = command.spawn().context("spawning command")?;
    if let Some(input) = input {
        let mut stdin = child.stdin.take().context("opening command stdin")?;
        stdin
            .write_all(input.as_bytes())
            .await
            .context("writing hook or tool input")?;
    }

    let output = tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait_with_output())
        .await
        .map_err(|_| anyhow::anyhow!("command timed out after {timeout_ms}ms"))?
        .context("waiting for command")?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    if output.status.success() {
        Ok(text)
    } else {
        bail!("{}", text.trim());
    }
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
