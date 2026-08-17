//! Workspace-scoped built-in tools with explicit approval for side effects.

use std::io::{IsTerminal, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use globset::{Glob, GlobMatcher};
use grey_core::{ToolCall, ToolDefinition, ToolExecutor, ToolResult, ToolRisk};
use ignore::WalkBuilder;
use regex::Regex;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::json;
use tempfile::NamedTempFile;
use tokio::process::Command;

pub const BUILTIN_TOOL_NAMES: [&str; 5] = ["read_file", "edit_file", "bash", "glob", "grep"];

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
