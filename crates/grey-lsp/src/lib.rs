//! Spike B: minimal LSP client over stdio.
//!
//! Deliberately hand-rolled JSON-RPC instead of a client framework: Grey's
//! "lightweight" stance means the client stays a thin stdio protocol layer
//! (lsp-types only provides the message shapes). Validates: spawn an LSP
//! server, initialize, open a file, and receive real diagnostics.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use lsp_types::{
    notification::{DidOpenTextDocument, Exit, Initialized, Notification, PublishDiagnostics},
    request::{
        DocumentDiagnosticRequest, GotoDefinition, HoverRequest, Initialize, References, Rename,
        Request, Shutdown,
    },
    ClientCapabilities, ClientInfo, Diagnostic, DiagnosticClientCapabilities,
    DidOpenTextDocumentParams, DocumentChangeOperation, DocumentChanges, DocumentDiagnosticParams,
    DocumentDiagnosticReport, DocumentDiagnosticReportResult, GotoDefinitionParams,
    GotoDefinitionResponse, Hover, HoverContents, HoverParams, InitializeParams, InitializedParams,
    Position, PublishDiagnosticsParams, Range, ReferenceContext, ReferenceParams, RenameParams,
    TextDocumentClientCapabilities, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, Uri, WorkspaceEdit, WorkspaceFolder,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_CONTENT_LENGTH: usize = 16 * 1024 * 1024;
const MAX_PENDING_NOTIFICATIONS: usize = 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefinitionLocation {
    pub uri: String,
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
}

impl DefinitionLocation {
    fn from_location(location: lsp_types::Location) -> Self {
        Self::from_range(location.uri, location.range)
    }

    fn from_range(uri: Uri, range: Range) -> Self {
        Self {
            uri: uri.to_string(),
            start_line: range.start.line + 1,
            start_character: range.start.character + 1,
            end_line: range.end.line + 1,
            end_character: range.end.character + 1,
        }
    }
}

struct JsonRpcTransport<R, W> {
    reader: R,
    writer: Option<W>,
}

impl<R, W> JsonRpcTransport<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    async fn send(&mut self, message: &Value) -> Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| anyhow!("LSP server stdin is closed"))?;
        write_framed_message(writer, message).await
    }

    async fn receive(&mut self) -> Result<Value> {
        read_framed_message(&mut self.reader).await
    }

    async fn close(&mut self) -> Result<()> {
        if let Some(mut writer) = self.writer.take() {
            writer
                .shutdown()
                .await
                .context("closing LSP server stdin")?;
        }
        Ok(())
    }
}

pub struct LspClient {
    transport: JsonRpcTransport<ChildStdout, ChildStdin>,
    child: Child,
    next_id: u64,
    pending_notifications: VecDeque<Value>,
}

impl LspClient {
    pub async fn spawn(lsp_path: &Path) -> Result<Self> {
        let mut child = Command::new(lsp_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning LSP server {}", lsp_path.display()))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        Ok(Self {
            transport: JsonRpcTransport {
                reader: stdout,
                writer: Some(stdin),
            },
            child,
            next_id: 1,
            pending_notifications: VecDeque::new(),
        })
    }

    /// Send a typed request and await its matching response (LSP framing).
    pub async fn request<R: Request + 'static>(&mut self, params: R::Params) -> Result<R::Result>
    where
        R::Params: Serialize,
        R::Result: DeserializeOwned,
    {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({"jsonrpc":"2.0","id":id,"method":R::METHOD,"params":params});
        self.send_raw(&msg).await?;
        loop {
            let resp = self.transport.receive().await?;
            if resp.get("method").is_some() {
                if resp.get("method").and_then(Value::as_str).is_none() {
                    bail!("malformed JSON-RPC method: {}", resp["method"]);
                }
                if is_server_request(&resp) {
                    self.respond_to_server_request(&resp).await?;
                } else {
                    if self.pending_notifications.len() >= MAX_PENDING_NOTIFICATIONS {
                        bail!(
                            "LSP notification queue exceeds {MAX_PENDING_NOTIFICATIONS} messages"
                        );
                    }
                    self.pending_notifications.push_back(resp);
                }
                continue;
            }
            if resp.get("id") == Some(&json!(id)) {
                if let Some(err) = resp.get("error") {
                    bail!("LSP request {} failed: {err}", R::METHOD);
                }
                return serde_json::from_value(resp["result"].clone())
                    .map_err(|e| anyhow!("bad LSP result for {}: {e}", R::METHOD));
            }
            // A stale response can follow a timed-out request. Sequential callers
            // have no waiter for it, so keep reading for this request's id.
        }
    }

    /// Send a typed notification (no response expected).
    pub async fn notify<N: Notification>(&mut self, params: N::Params) -> Result<()>
    where
        N::Params: Serialize,
    {
        let msg = json!({"jsonrpc":"2.0","method":N::METHOD,"params":params});
        self.send_raw(&msg).await
    }

    async fn send_raw(&mut self, msg: &Value) -> Result<()> {
        self.transport.send(msg).await
    }

    /// Read one LSP message (Content-Length framed).
    pub async fn read_raw(&mut self) -> Result<Value> {
        if let Some(message) = self.pending_notifications.pop_front() {
            return Ok(message);
        }
        loop {
            let message = self.transport.receive().await?;
            if is_server_request(&message) {
                self.respond_to_server_request(&message).await?;
                continue;
            }
            return Ok(message);
        }
    }

    async fn respond_to_server_request(&mut self, request: &Value) -> Result<()> {
        let response = server_request_response(request)
            .ok_or_else(|| anyhow!("invalid server-to-client JSON-RPC request: {request}"))?;
        self.send_raw(&response).await
    }

    pub async fn shutdown(self) -> Result<()> {
        self.shutdown_with_timeout(SHUTDOWN_TIMEOUT).await
    }

    async fn shutdown_with_timeout(mut self, timeout: Duration) -> Result<()> {
        let mut errors = Vec::new();
        match tokio::time::timeout(timeout, self.request::<Shutdown>(())).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => errors.push(format!("requesting LSP shutdown: {error:#}")),
            Err(_) => errors.push(format!("LSP shutdown request timed out after {timeout:?}")),
        }

        if let Err(error) = self.notify::<Exit>(()).await.context("notifying LSP exit") {
            errors.push(format!("{error:#}"));
        }
        if let Err(error) = self.transport.close().await {
            errors.push(format!("{error:#}"));
        }

        match tokio::time::timeout(timeout, self.child.wait()).await {
            Ok(Ok(status)) if !status.success() => {
                errors.push(format!("LSP server exited with status {status}"));
            }
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                errors.push(format!("waiting for LSP server process: {error}"));
                if let Err(kill_error) = self.child.kill().await {
                    errors.push(format!(
                        "terminating LSP server after wait failure: {kill_error}"
                    ));
                }
            }
            Err(_) => {
                errors.push(format!("LSP server did not exit after {timeout:?}"));
                if let Err(error) = self.child.kill().await {
                    errors.push(format!("terminating unresponsive LSP server: {error}"));
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            bail!(errors.join("; "))
        }
    }
}

fn server_request_response(request: &Value) -> Option<Value> {
    let id = request.get("id")?.clone();
    let method = request.get("method")?.as_str()?;
    let result = match method {
        "workspace/configuration" => {
            let item_count = request
                .pointer("/params/items")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            Value::Array(vec![Value::Null; item_count])
        }
        "workspace/workspaceFolders" | "window/showMessageRequest" => Value::Null,
        "client/registerCapability"
        | "client/unregisterCapability"
        | "window/workDoneProgress/create"
        | "workspace/codeLens/refresh"
        | "workspace/diagnostic/refresh"
        | "workspace/inlayHint/refresh"
        | "workspace/semanticTokens/refresh" => Value::Null,
        _ => {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("unsupported server request: {method}"),
                }
            }));
        }
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn is_server_request(message: &Value) -> bool {
    message.get("method").and_then(Value::as_str).is_some()
        && message.get("id").is_some_and(|id| !id.is_null())
}

async fn write_framed_message<W>(writer: &mut W, message: &Value) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let body = serde_json::to_vec(message).context("serializing JSON-RPC message")?;
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await
        .context("writing LSP message header")?;
    writer
        .write_all(&body)
        .await
        .context("writing LSP message body")?;
    writer.flush().await.context("flushing LSP message")?;
    Ok(())
}

async fn read_framed_message<R>(reader: &mut R) -> Result<Value>
where
    R: AsyncRead + Unpin,
{
    let mut header = Vec::with_capacity(128);
    loop {
        let mut byte = [0_u8; 1];
        let count = reader
            .read(&mut byte)
            .await
            .context("reading LSP message header")?;
        if count == 0 {
            bail!("LSP server closed the stream while reading a header");
        }
        header.push(byte[0]);
        if header.len() > MAX_HEADER_BYTES {
            bail!("LSP message header exceeds {MAX_HEADER_BYTES} bytes");
        }
        if header.ends_with(b"\r\n\r\n") || header.ends_with(b"\n\n") {
            break;
        }
    }

    let header = std::str::from_utf8(&header).context("LSP message header is not UTF-8")?;
    let mut content_length = None;
    for line in header.lines().filter(|line| !line.trim().is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            bail!("malformed LSP header line: {line}");
        };
        if name.trim().eq_ignore_ascii_case("Content-Length") {
            if content_length.is_some() {
                bail!("duplicate Content-Length header");
            }
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .context("invalid Content-Length header")?,
            );
        }
    }
    let content_length = content_length.ok_or_else(|| anyhow!("missing Content-Length header"))?;
    if content_length > MAX_CONTENT_LENGTH {
        bail!("LSP message body exceeds {MAX_CONTENT_LENGTH} bytes");
    }

    let mut body = vec![0_u8; content_length];
    reader
        .read_exact(&mut body)
        .await
        .context("reading LSP message body")?;
    serde_json::from_slice(&body).map_err(|error| anyhow!("bad JSON-RPC body: {error}"))
}

async fn request_with_timeout<R>(
    client: &mut LspClient,
    params: R::Params,
    timeout: Duration,
) -> Result<R::Result>
where
    R: Request + 'static,
    R::Params: Serialize,
    R::Result: DeserializeOwned,
{
    match tokio::time::timeout(timeout, client.request::<R>(params)).await {
        Ok(result) => result,
        Err(_) => bail!("LSP request {} timed out after {timeout:?}", R::METHOD),
    }
}

/// Spike B entry: run the full initialize -> open -> diagnostics flow against
/// a real language server, then report what came back.
pub async fn run_lsp_spike(file: &Path, lsp_path: Option<&Path>) -> Result<()> {
    let diagnostics = collect_file_diagnostics(file, lsp_path).await?;
    let definitions = collect_file_definitions(file, lsp_path, None, None).await?;
    if diagnostics.is_empty() {
        println!("[spike-b] OK: no problems reported for the file");
    } else {
        println!("[spike-b] OK: {} diagnostic(s):", diagnostics.len());
        for d in &diagnostics {
            print_diagnostic(d);
        }
    }
    if definitions.is_empty() {
        println!("[spike-b] definition: none");
    } else {
        println!("[spike-b] definition(s):");
        for definition in &definitions {
            println!(
                "[spike-b] {}:{}:{} -> {}:{}",
                definition.uri,
                definition.start_line,
                definition.start_character,
                definition.end_line,
                definition.end_character
            );
        }
    }
    Ok(())
}

pub async fn collect_file_diagnostics(
    file: &Path,
    lsp_path: Option<&Path>,
) -> Result<Vec<Diagnostic>> {
    let result = collect_file_data(file, lsp_path, None).await?;
    Ok(result.diagnostics)
}

pub async fn collect_file_definitions(
    file: &Path,
    lsp_path: Option<&Path>,
    line: Option<u32>,
    character: Option<u32>,
) -> Result<Vec<DefinitionLocation>> {
    if line.is_some() != character.is_some() {
        bail!("line and character must both be provided");
    }
    let position = match (line, character) {
        (Some(line), Some(character)) => Some(Position {
            line: line.saturating_sub(1),
            character: character.saturating_sub(1),
        }),
        _ => None,
    };
    let result = collect_file_data(file, lsp_path, position).await?;
    Ok(result.definitions)
}

pub async fn collect_file_references(
    file: &Path,
    lsp_path: Option<&Path>,
    line: Option<u32>,
    character: Option<u32>,
    include_declaration: bool,
) -> Result<Vec<DefinitionLocation>> {
    if line.is_some() != character.is_some() {
        bail!("line and character must both be provided");
    }
    let position = match (line, character) {
        (Some(line), Some(character)) => Some(Position {
            line: line.saturating_sub(1),
            character: character.saturating_sub(1),
        }),
        _ => None,
    };
    let result = collect_file_reference_data(file, lsp_path, position, include_declaration).await?;
    Ok(result)
}

pub async fn collect_file_hover(
    file: &Path,
    lsp_path: Option<&Path>,
    line: Option<u32>,
    character: Option<u32>,
) -> Result<String> {
    if line.is_some() != character.is_some() {
        bail!("line and character must both be provided");
    }
    let position = match (line, character) {
        (Some(line), Some(character)) => Some(Position {
            line: line.saturating_sub(1),
            character: character.saturating_sub(1),
        }),
        _ => None,
    };
    let result = collect_file_hover_data(file, lsp_path, position).await?;
    Ok(result)
}

pub async fn collect_file_rename(
    file: &Path,
    lsp_path: Option<&Path>,
    line: Option<u32>,
    character: Option<u32>,
    new_name: String,
) -> Result<String> {
    if line.is_some() != character.is_some() {
        bail!("line and character must both be provided");
    }
    let position = match (line, character) {
        (Some(line), Some(character)) => Some(Position {
            line: line.saturating_sub(1),
            character: character.saturating_sub(1),
        }),
        _ => None,
    };
    let result = collect_file_rename_data(file, lsp_path, position, new_name).await?;
    Ok(result)
}

struct LspCollectResult {
    diagnostics: Vec<Diagnostic>,
    definitions: Vec<DefinitionLocation>,
}

async fn collect_file_data(
    file: &Path,
    lsp_path: Option<&Path>,
    definition_position: Option<Position>,
) -> Result<LspCollectResult> {
    let lsp = lsp_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("rust-analyzer"));
    let file_abs = std::fs::canonicalize(file)
        .with_context(|| format!("canonicalizing {}", file.display()))?;
    let workspace_root = discover_workspace_root(&file_abs)?;
    let root = path_to_uri(&workspace_root)?;
    let file_uri = path_to_uri(&file_abs)?;
    let text = std::fs::read_to_string(&file_abs)
        .with_context(|| format!("reading {}", file_abs.display()))?;
    let definition_position = definition_position.or_else(|| definition_probe_position(&text));
    println!(
        "[spike-b] lsp={} file={} workspace={}",
        lsp.display(),
        file_abs.display(),
        workspace_root.display()
    );

    let mut client = LspClient::spawn(&lsp).await?;
    let session_result = run_lsp_session(
        &mut client,
        root,
        &workspace_root,
        file_uri,
        text,
        definition_position,
    )
    .await;
    let shutdown_result = client.shutdown().await;

    match (session_result, shutdown_result) {
        (Ok(result), Ok(())) => {
            println!("[spike-b] done");
            Ok(result)
        }
        (Err(session_error), Ok(())) => Err(session_error),
        (Ok(_), Err(shutdown_error)) => Err(shutdown_error),
        (Err(session_error), Err(shutdown_error)) => bail!(
            "{session_error:#}; additionally failed to shut down LSP server: {shutdown_error:#}"
        ),
    }
}

async fn collect_file_reference_data(
    file: &Path,
    lsp_path: Option<&Path>,
    definition_position: Option<Position>,
    include_declaration: bool,
) -> Result<Vec<DefinitionLocation>> {
    let lsp = lsp_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("rust-analyzer"));
    let file_abs = std::fs::canonicalize(file)
        .with_context(|| format!("canonicalizing {}", file.display()))?;
    let workspace_root = discover_workspace_root(&file_abs)?;
    let root = path_to_uri(&workspace_root)?;
    let file_uri = path_to_uri(&file_abs)?;
    let text = std::fs::read_to_string(&file_abs)
        .with_context(|| format!("reading {}", file_abs.display()))?;
    let definition_position = definition_position.or_else(|| definition_probe_position(&text));
    println!(
        "[spike-b] lsp={} file={} workspace={}",
        lsp.display(),
        file_abs.display(),
        workspace_root.display()
    );

    let mut client = LspClient::spawn(&lsp).await?;
    let session_result = run_lsp_reference_session(
        &mut client,
        root,
        &workspace_root,
        file_uri,
        text,
        definition_position,
        include_declaration,
    )
    .await;
    let shutdown_result = client.shutdown().await;

    match (session_result, shutdown_result) {
        (Ok(result), Ok(())) => {
            println!("[spike-b] done");
            Ok(result)
        }
        (Err(session_error), Ok(())) => Err(session_error),
        (Ok(_), Err(shutdown_error)) => Err(shutdown_error),
        (Err(session_error), Err(shutdown_error)) => bail!(
            "{session_error:#}; additionally failed to shut down LSP server: {shutdown_error:#}"
        ),
    }
}

async fn collect_file_hover_data(
    file: &Path,
    lsp_path: Option<&Path>,
    position: Option<Position>,
) -> Result<String> {
    let lsp = lsp_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("rust-analyzer"));
    let file_abs = std::fs::canonicalize(file)
        .with_context(|| format!("canonicalizing {}", file.display()))?;
    let workspace_root = discover_workspace_root(&file_abs)?;
    let root = path_to_uri(&workspace_root)?;
    let file_uri = path_to_uri(&file_abs)?;
    let text = std::fs::read_to_string(&file_abs)
        .with_context(|| format!("reading {}", file_abs.display()))?;
    let position = position.or_else(|| definition_probe_position(&text));
    println!(
        "[spike-b] lsp={} file={} workspace={}",
        lsp.display(),
        file_abs.display(),
        workspace_root.display()
    );

    let mut client = LspClient::spawn(&lsp).await?;
    let session_result =
        run_lsp_hover_session(&mut client, root, &workspace_root, file_uri, text, position).await;
    let shutdown_result = client.shutdown().await;

    match (session_result, shutdown_result) {
        (Ok(result), Ok(())) => {
            println!("[spike-b] done");
            Ok(result)
        }
        (Err(session_error), Ok(())) => Err(session_error),
        (Ok(_), Err(shutdown_error)) => Err(shutdown_error),
        (Err(session_error), Err(shutdown_error)) => bail!(
            "{session_error:#}; additionally failed to shut down LSP server: {shutdown_error:#}"
        ),
    }
}

async fn collect_file_rename_data(
    file: &Path,
    lsp_path: Option<&Path>,
    position: Option<Position>,
    new_name: String,
) -> Result<String> {
    let lsp = lsp_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("rust-analyzer"));
    let file_abs = std::fs::canonicalize(file)
        .with_context(|| format!("canonicalizing {}", file.display()))?;
    let workspace_root = discover_workspace_root(&file_abs)?;
    let root = path_to_uri(&workspace_root)?;
    let file_uri = path_to_uri(&file_abs)?;
    let text = std::fs::read_to_string(&file_abs)
        .with_context(|| format!("reading {}", file_abs.display()))?;
    let position = position.or_else(|| definition_probe_position(&text));
    println!(
        "[spike-b] lsp={} file={} workspace={}",
        lsp.display(),
        file_abs.display(),
        workspace_root.display()
    );

    let mut client = LspClient::spawn(&lsp).await?;
    let session_result = run_lsp_rename_session(
        &mut client,
        root,
        &workspace_root,
        file_uri,
        text,
        position,
        new_name,
    )
    .await;
    let shutdown_result = client.shutdown().await;

    match (session_result, shutdown_result) {
        (Ok(result), Ok(())) => {
            println!("[spike-b] done");
            Ok(result)
        }
        (Err(session_error), Ok(())) => Err(session_error),
        (Ok(_), Err(shutdown_error)) => Err(shutdown_error),
        (Err(session_error), Err(shutdown_error)) => bail!(
            "{session_error:#}; additionally failed to shut down LSP server: {shutdown_error:#}"
        ),
    }
}

#[allow(deprecated)] // root_uri + workspace_folders both sent for maximal server compat
async fn run_lsp_session(
    client: &mut LspClient,
    root: Uri,
    workspace_root: &Path,
    file_uri: Uri,
    text: String,
    definition_position: Option<Position>,
) -> Result<LspCollectResult> {
    let init = request_with_timeout::<Initialize>(
        client,
        InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: Some(root.clone()),
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: root,
                name: workspace_root
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| workspace_root.display().to_string()),
            }]),
            capabilities: ClientCapabilities {
                text_document: Some(TextDocumentClientCapabilities {
                    diagnostic: Some(DiagnosticClientCapabilities {
                        dynamic_registration: None,
                        related_document_support: Some(true),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            client_info: Some(ClientInfo {
                name: "grey-spike".into(),
                version: None,
            }),
            ..Default::default()
        },
        REQUEST_TIMEOUT,
    )
    .await?;
    println!(
        "[spike-b] initialized: server={:?}",
        init.server_info
            .map(|i| format!("{} {}", i.name, i.version.unwrap_or_default()))
    );

    client.notify::<Initialized>(InitializedParams {}).await?;
    client
        .notify::<DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: file_uri.clone(),
                language_id: "rust".into(),
                version: 1,
                text,
            },
        })
        .await?;

    let diags = match tokio::time::timeout(
        Duration::from_secs(15),
        wait_for_diagnostics(client, &file_uri),
    )
    .await
    {
        Ok(Ok(diags)) => diags,
        Ok(Err(e)) => bail!("LSP error while waiting: {e}"),
        Err(_) => {
            println!("[spike-b] no push diagnostics; pulling textDocument/diagnostic...");
            pull_diagnostics(client, &file_uri).await?
        }
    };
    let definitions = probe_definition(client, &file_uri, definition_position).await?;
    Ok(LspCollectResult {
        diagnostics: diags,
        definitions,
    })
}

#[allow(deprecated)] // root_uri + workspace_folders both sent for maximal server compat
async fn run_lsp_reference_session(
    client: &mut LspClient,
    root: Uri,
    workspace_root: &Path,
    file_uri: Uri,
    text: String,
    definition_position: Option<Position>,
    include_declaration: bool,
) -> Result<Vec<DefinitionLocation>> {
    let init = request_with_timeout::<Initialize>(
        client,
        InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: Some(root.clone()),
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: root,
                name: workspace_root
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| workspace_root.display().to_string()),
            }]),
            capabilities: ClientCapabilities {
                text_document: Some(TextDocumentClientCapabilities {
                    diagnostic: Some(DiagnosticClientCapabilities {
                        dynamic_registration: None,
                        related_document_support: Some(true),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            client_info: Some(ClientInfo {
                name: "grey-spike".into(),
                version: None,
            }),
            ..Default::default()
        },
        REQUEST_TIMEOUT,
    )
    .await?;
    println!(
        "[spike-b] initialized: server={:?}",
        init.server_info
            .map(|i| format!("{} {}", i.name, i.version.unwrap_or_default()))
    );

    client.notify::<Initialized>(InitializedParams {}).await?;
    client
        .notify::<DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: file_uri.clone(),
                language_id: "rust".into(),
                version: 1,
                text,
            },
        })
        .await?;

    let _ = tokio::time::timeout(
        Duration::from_secs(15),
        wait_for_diagnostics(client, &file_uri),
    )
    .await;

    probe_references(client, &file_uri, definition_position, include_declaration).await
}

#[allow(deprecated)] // root_uri + workspace_folders both sent for maximal server compat
async fn run_lsp_hover_session(
    client: &mut LspClient,
    root: Uri,
    workspace_root: &Path,
    file_uri: Uri,
    text: String,
    position: Option<Position>,
) -> Result<String> {
    let init = request_with_timeout::<Initialize>(
        client,
        InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: Some(root.clone()),
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: root,
                name: workspace_root
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| workspace_root.display().to_string()),
            }]),
            capabilities: ClientCapabilities {
                text_document: Some(TextDocumentClientCapabilities {
                    diagnostic: Some(DiagnosticClientCapabilities {
                        dynamic_registration: None,
                        related_document_support: Some(true),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            client_info: Some(ClientInfo {
                name: "grey-spike".into(),
                version: None,
            }),
            ..Default::default()
        },
        REQUEST_TIMEOUT,
    )
    .await?;
    println!(
        "[spike-b] initialized: server={:?}",
        init.server_info
            .map(|i| format!("{} {}", i.name, i.version.unwrap_or_default()))
    );

    client.notify::<Initialized>(InitializedParams {}).await?;
    client
        .notify::<DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: file_uri.clone(),
                language_id: "rust".into(),
                version: 1,
                text,
            },
        })
        .await?;

    let _ = tokio::time::timeout(
        Duration::from_secs(15),
        wait_for_diagnostics(client, &file_uri),
    )
    .await;

    probe_hover(client, &file_uri, position).await
}

#[allow(deprecated)] // root_uri + workspace_folders both sent for maximal server compat
async fn run_lsp_rename_session(
    client: &mut LspClient,
    root: Uri,
    workspace_root: &Path,
    file_uri: Uri,
    text: String,
    position: Option<Position>,
    new_name: String,
) -> Result<String> {
    let init = request_with_timeout::<Initialize>(
        client,
        InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: Some(root.clone()),
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: root,
                name: workspace_root
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| workspace_root.display().to_string()),
            }]),
            capabilities: ClientCapabilities {
                text_document: Some(TextDocumentClientCapabilities {
                    diagnostic: Some(DiagnosticClientCapabilities {
                        dynamic_registration: None,
                        related_document_support: Some(true),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            client_info: Some(ClientInfo {
                name: "grey-spike".into(),
                version: None,
            }),
            ..Default::default()
        },
        REQUEST_TIMEOUT,
    )
    .await?;
    println!(
        "[spike-b] initialized: server={:?}",
        init.server_info
            .map(|i| format!("{} {}", i.name, i.version.unwrap_or_default()))
    );

    client.notify::<Initialized>(InitializedParams {}).await?;
    client
        .notify::<DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: file_uri.clone(),
                language_id: "rust".into(),
                version: 1,
                text,
            },
        })
        .await?;

    let _ = tokio::time::timeout(
        Duration::from_secs(15),
        wait_for_diagnostics(client, &file_uri),
    )
    .await;

    probe_rename(client, &file_uri, position, new_name).await
}

async fn wait_for_diagnostics(client: &mut LspClient, uri: &Uri) -> Result<Vec<Diagnostic>> {
    loop {
        let msg = client.read_raw().await?;
        if msg.get("method") == Some(&json!(PublishDiagnostics::METHOD)) {
            let params: PublishDiagnosticsParams = serde_json::from_value(msg["params"].clone())?;
            if &params.uri == uri {
                return Ok(params.diagnostics);
            }
        }
    }
}

/// LSP 3.17 pull-style diagnostics: works even when the server is lazy about
/// pushing publishDiagnostics. Retries while the server is still indexing.
async fn pull_diagnostics(client: &mut LspClient, uri: &Uri) -> Result<Vec<Diagnostic>> {
    const ATTEMPTS: u32 = 12;
    const INTERVAL: Duration = Duration::from_secs(5);
    let params = DocumentDiagnosticParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        identifier: None,
        previous_result_id: None,
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    let mut last = Vec::new();
    for i in 0..ATTEMPTS {
        let report = request_with_timeout::<DocumentDiagnosticRequest>(
            client,
            params.clone(),
            REQUEST_TIMEOUT,
        )
        .await?;
        // Unchanged/partial: nothing new; keep polling briefly.
        if let DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(r)) = report {
            last = r.full_document_diagnostic_report.items;
            if !last.is_empty() {
                return Ok(last);
            }
        }
        if i + 1 < ATTEMPTS {
            println!(
                "[spike-b]   ...still indexing (attempt {}/{ATTEMPTS})",
                i + 1
            );
            tokio::time::sleep(INTERVAL).await;
        }
    }
    Ok(last)
}

async fn probe_definition(
    client: &mut LspClient,
    uri: &Uri,
    position: Option<Position>,
) -> Result<Vec<DefinitionLocation>> {
    let Some(position) = position else {
        return Ok(Vec::new());
    };
    let params = GotoDefinitionParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position,
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };

    match tokio::time::timeout(
        Duration::from_secs(10),
        client.request::<GotoDefinition>(params),
    )
    .await
    {
        Ok(Ok(Some(response))) => Ok(match response {
            GotoDefinitionResponse::Scalar(location) => {
                vec![DefinitionLocation::from_location(location)]
            }
            GotoDefinitionResponse::Array(locations) => locations
                .into_iter()
                .map(DefinitionLocation::from_location)
                .collect(),
            GotoDefinitionResponse::Link(links) => links
                .into_iter()
                .map(|link| DefinitionLocation::from_range(link.target_uri, link.target_range))
                .collect(),
        }),
        Ok(Ok(None)) => Ok(Vec::new()),
        Ok(Err(error)) => Err(anyhow!("definition unavailable: {error}")),
        Err(_) => Err(anyhow!("definition unavailable: request timed out")),
    }
}

async fn probe_hover(
    client: &mut LspClient,
    uri: &Uri,
    position: Option<Position>,
) -> Result<String> {
    let Some(position) = position else {
        return Ok("hover unavailable: no valid symbol position".into());
    };
    let params = HoverParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position,
        },
        work_done_progress_params: Default::default(),
    };

    match tokio::time::timeout(
        Duration::from_secs(10),
        client.request::<HoverRequest>(params),
    )
    .await
    {
        Ok(Ok(Some(hover))) => Ok(hover_contents_to_text(hover)),
        Ok(Ok(None)) => Ok("hover unavailable".into()),
        Ok(Err(error)) => Err(anyhow!("hover unavailable: {error}")),
        Err(_) => Err(anyhow!("hover unavailable: request timed out")),
    }
}

fn hover_contents_to_text(hover: Hover) -> String {
    let mut output = match hover.contents {
        HoverContents::Scalar(content) => hover_content_to_text(content),
        HoverContents::Array(contents) => contents
            .into_iter()
            .map(hover_content_to_text)
            .collect::<Vec<_>>()
            .join("\n"),
        HoverContents::Markup(markup) => markup.value,
    };
    if output.is_empty() {
        output.push_str("hover available but empty");
    }
    if let Some(range) = hover.range {
        output.push_str(&format!(
            " [range {}:{}-{}:{}]",
            range.start.line + 1,
            range.start.character + 1,
            range.end.line + 1,
            range.end.character + 1
        ));
    }
    output
}

fn hover_content_to_text(content: lsp_types::MarkedString) -> String {
    match content {
        lsp_types::MarkedString::String(value) => value,
        lsp_types::MarkedString::LanguageString(language) => {
            format!("```{}\n{}\n```", language.language, language.value)
        }
    }
}

async fn probe_rename(
    client: &mut LspClient,
    uri: &Uri,
    position: Option<Position>,
    new_name: String,
) -> Result<String> {
    let Some(position) = position else {
        return Ok("rename unavailable: no valid symbol position".into());
    };
    let params = RenameParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position,
        },
        new_name,
        work_done_progress_params: Default::default(),
    };

    match tokio::time::timeout(Duration::from_secs(10), client.request::<Rename>(params)).await {
        Ok(Ok(Some(edit))) => Ok(format_workspace_edit(edit)),
        Ok(Ok(None)) => Ok("rename unavailable".into()),
        Ok(Err(error)) => Err(anyhow!("rename unavailable: {error}")),
        Err(_) => Err(anyhow!("rename unavailable: request timed out")),
    }
}

fn format_workspace_edit(edit: WorkspaceEdit) -> String {
    let mut change_count: usize = 0;
    let mut file_count: usize = 0;
    if let Some(changes) = &edit.changes {
        file_count += changes.len();
        change_count += changes.values().map(|edits| edits.len()).sum::<usize>();
    }
    if let Some(document_changes) = &edit.document_changes {
        match document_changes {
            DocumentChanges::Edits(changes) => {
                file_count += changes.len();
                change_count += changes
                    .iter()
                    .map(|change| change.edits.len())
                    .sum::<usize>();
            }
            DocumentChanges::Operations(changes) => {
                file_count += changes.len();
                change_count += changes
                    .iter()
                    .map(|change| match change {
                        DocumentChangeOperation::Edit(text_edit) => text_edit.edits.len(),
                        DocumentChangeOperation::Op(_) => 1,
                    })
                    .sum::<usize>();
            }
        }
    }
    if change_count == 0 {
        return "rename unavailable: no changes".into();
    }
    let details = serde_json::to_string_pretty(&edit).unwrap_or_else(|_| String::new());
    let mut output = format!(
        "rename preview: {} file(s), {} change(s)\n",
        file_count, change_count
    );
    output.push_str(&details);
    output
}

async fn probe_references(
    client: &mut LspClient,
    uri: &Uri,
    position: Option<Position>,
    include_declaration: bool,
) -> Result<Vec<DefinitionLocation>> {
    let Some(position) = position else {
        return Ok(Vec::new());
    };
    let params = ReferenceParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position,
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: ReferenceContext {
            include_declaration,
        },
    };

    match tokio::time::timeout(
        Duration::from_secs(10),
        client.request::<References>(params),
    )
    .await
    {
        Ok(Ok(Some(locations))) => Ok(locations
            .into_iter()
            .map(DefinitionLocation::from_location)
            .collect()),
        Ok(Ok(None)) => Ok(Vec::new()),
        Ok(Err(error)) => Err(anyhow!("references unavailable: {error}")),
        Err(_) => Err(anyhow!("references unavailable: request timed out")),
    }
}

fn definition_probe_position(text: &str) -> Option<Position> {
    let mut declaration = None;
    'lines: for (line_number, line) in text.lines().enumerate() {
        for (index, _) in line.match_indices("fn") {
            let before = line[..index].chars().next_back();
            let after = line[index + 2..].chars().next();
            if before.is_some_and(is_identifier_continue)
                || after.is_some_and(is_identifier_continue)
            {
                continue;
            }

            let Some(name_offset) =
                line[index + 2..].find(|character: char| !character.is_whitespace())
            else {
                continue;
            };
            let name_start = index + 2 + name_offset;
            let name = line[name_start..]
                .chars()
                .take_while(|character| is_identifier_continue(*character))
                .collect::<String>();
            if !name.is_empty() {
                declaration = Some((
                    Position {
                        line: u32::try_from(line_number).ok()?,
                        character: u32::try_from(line[..name_start].encode_utf16().count()).ok()?,
                    },
                    name,
                    line_number,
                ));
                break 'lines;
            }
        }
    }
    let (declaration_position, name, declaration_line) = declaration?;

    for (line_number, line) in text.lines().enumerate().skip(declaration_line + 1) {
        for (index, _) in line.match_indices(&name) {
            let before = line[..index].chars().next_back();
            let after_name = &line[index + name.len()..];
            if before.is_some_and(is_identifier_continue)
                || after_name
                    .chars()
                    .next()
                    .is_some_and(is_identifier_continue)
            {
                continue;
            }
            let following = after_name.trim_start();
            if following.starts_with('(') || following.starts_with("::<") {
                return Some(Position {
                    line: u32::try_from(line_number).ok()?,
                    character: u32::try_from(line[..index].encode_utf16().count()).ok()?,
                });
            }
        }
    }
    Some(declaration_position)
}

fn is_identifier_continue(character: char) -> bool {
    character == '_' || character.is_alphanumeric()
}

fn discover_workspace_root(file: &Path) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(file)
        .with_context(|| format!("canonicalizing {}", file.display()))?;
    let start = if canonical.is_dir() {
        canonical.as_path()
    } else {
        canonical
            .parent()
            .ok_or_else(|| anyhow!("{} has no parent directory", canonical.display()))?
    };
    let mut nearest_project = None;

    for directory in start.ancestors() {
        if directory.join(".git").exists() || directory.join(".hg").exists() {
            return Ok(directory.to_path_buf());
        }

        let manifest = directory.join("Cargo.toml");
        if manifest.is_file() {
            nearest_project.get_or_insert_with(|| directory.to_path_buf());
            if manifest_declares_workspace(&manifest) {
                return Ok(directory.to_path_buf());
            }
        }
        if directory.join("rust-project.json").is_file() {
            nearest_project.get_or_insert_with(|| directory.to_path_buf());
        }
    }

    Ok(nearest_project.unwrap_or_else(|| start.to_path_buf()))
}

fn manifest_declares_workspace(manifest: &Path) -> bool {
    std::fs::read_to_string(manifest).is_ok_and(|contents| {
        contents.lines().any(|line| {
            let line = line.trim();
            line == "[workspace]" || line.starts_with("[workspace.")
        })
    })
}

fn path_to_uri(p: &Path) -> Result<Uri> {
    if !p.is_absolute() {
        bail!("file URI requires an absolute path: {}", p.display());
    }

    let mut encoded = String::with_capacity(p.as_os_str().len());
    for byte in p.as_os_str().as_encoded_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                encoded.push(char::from(*byte))
            }
            b'\\' if cfg!(windows) => encoded.push('/'),
            byte => {
                use std::fmt::Write;
                write!(&mut encoded, "%{byte:02X}").expect("writing to String cannot fail");
            }
        }
    }
    let uri = if cfg!(windows) && encoded.starts_with("//") {
        format!("file:{encoded}")
    } else {
        let separator = if encoded.starts_with('/') { "" } else { "/" };
        format!("file://{separator}{encoded}")
    };
    Uri::from_str(&uri).map_err(|e| anyhow!("invalid URI for {}: {e}", p.display()))
}

fn print_diagnostic(d: &Diagnostic) {
    let pos = d.range.start;
    println!(
        "  [{}] {}:{} {}",
        d.severity.map(|s| format!("{s:?}")).unwrap_or_default(),
        pos.line + 1,
        pos.character + 1,
        d.message
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncWriteExt};

    #[tokio::test]
    async fn framing_round_trips_multiple_utf8_messages() {
        let (mut writer, mut reader) = duplex(1024);
        let first = json!({"jsonrpc": "2.0", "method": "grey/测试", "params": {"text": "你好"}});
        let second = json!({"jsonrpc": "2.0", "id": 7, "result": null});

        write_framed_message(&mut writer, &first).await.unwrap();
        write_framed_message(&mut writer, &second).await.unwrap();

        assert_eq!(read_framed_message(&mut reader).await.unwrap(), first);
        assert_eq!(read_framed_message(&mut reader).await.unwrap(), second);
    }

    #[tokio::test]
    async fn framing_accepts_case_insensitive_headers_and_fragmented_body() {
        let (mut writer, mut reader) = duplex(1024);
        let body = br#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        let header = format!(
            "content-type: application/vscode-jsonrpc; charset=utf-8\r\ncontent-length: {}\r\n\r\n",
            body.len()
        );

        let write = tokio::spawn(async move {
            for chunk in header.as_bytes().chunks(3) {
                writer.write_all(chunk).await.unwrap();
            }
            for chunk in body.chunks(2) {
                writer.write_all(chunk).await.unwrap();
            }
        });

        let message = read_framed_message(&mut reader).await.unwrap();
        write.await.unwrap();
        assert_eq!(message["result"]["ok"], json!(true));
    }

    #[tokio::test]
    async fn framing_rejects_missing_or_duplicate_content_length() {
        let (mut missing_writer, mut missing_reader) = duplex(128);
        missing_writer
            .write_all(b"Content-Type: application/json\r\n\r\n{}")
            .await
            .unwrap();
        missing_writer.shutdown().await.unwrap();
        assert!(read_framed_message(&mut missing_reader)
            .await
            .unwrap_err()
            .to_string()
            .contains("missing Content-Length"));

        let (mut duplicate_writer, mut duplicate_reader) = duplex(128);
        duplicate_writer
            .write_all(b"Content-Length: 2\r\ncontent-length: 2\r\n\r\n{}")
            .await
            .unwrap();
        duplicate_writer.shutdown().await.unwrap();
        assert!(read_framed_message(&mut duplicate_reader)
            .await
            .unwrap_err()
            .to_string()
            .contains("duplicate Content-Length"));

        let (mut oversized_writer, mut oversized_reader) = duplex(128);
        oversized_writer
            .write_all(format!("Content-Length: {}\r\n\r\n", MAX_CONTENT_LENGTH + 1).as_bytes())
            .await
            .unwrap();
        assert!(read_framed_message(&mut oversized_reader)
            .await
            .unwrap_err()
            .to_string()
            .contains("message body exceeds"));

        let (mut truncated_writer, mut truncated_reader) = duplex(128);
        truncated_writer
            .write_all(b"Content-Length: 5\r\n\r\n{}")
            .await
            .unwrap();
        truncated_writer.shutdown().await.unwrap();
        assert!(read_framed_message(&mut truncated_reader)
            .await
            .unwrap_err()
            .to_string()
            .contains("reading LSP message body"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_waits_for_the_server_process_to_exit() {
        let marker =
            std::env::temp_dir().join(format!("grey-lsp-shutdown-marker-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let client = shell_lsp_client(
            "printf 'Content-Length: 38\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":null}'; \
             cat >/dev/null; touch \"$GREY_LSP_TEST_MARKER\"",
            Some(&marker),
        );

        client
            .shutdown_with_timeout(Duration::from_secs(2))
            .await
            .unwrap();

        assert!(marker.is_file());
        std::fs::remove_file(marker).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_bounds_an_unresponsive_protocol_request_and_reaps_the_process() {
        let client = shell_lsp_client("cat >/dev/null", None);

        let error = client
            .shutdown_with_timeout(Duration::from_millis(25))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("shutdown request timed out"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_dispatches_a_server_request_with_the_same_numeric_id() {
        let client = shell_lsp_client(
            r#"
read_frame() {
    IFS= read -r header || exit 2
    length=${header#Content-Length: }
    length=$(printf '%s' "$length" | tr -d '\r')
    IFS= read -r blank || exit 3
    BODY=$(dd bs=1 count="$length" 2>/dev/null)
}
read_frame
server_request='{"jsonrpc":"2.0","id":1,"method":"workspace/configuration","params":{"items":[{},{}]}}'
printf 'Content-Length: %s\r\n\r\n%s' "${#server_request}" "$server_request"
read_frame
case "$BODY" in
    *'"result":[null,null]'*) ;;
    *) exit 9 ;;
esac
response='{"jsonrpc":"2.0","id":1,"result":null}'
printf 'Content-Length: %s\r\n\r\n%s' "${#response}" "$response"
cat >/dev/null
"#,
            None,
        );

        client
            .shutdown_with_timeout(Duration::from_secs(2))
            .await
            .unwrap();
    }

    #[test]
    fn server_configuration_request_gets_one_null_per_requested_item() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "workspace/configuration",
            "params": {"items": [{"section": "rust-analyzer"}, {}]},
        });

        assert!(is_server_request(&request));
        assert_eq!(
            server_request_response(&request).unwrap(),
            json!({"jsonrpc": "2.0", "id": 1, "result": [null, null]})
        );
        assert!(!is_server_request(
            &json!({"jsonrpc": "2.0", "id": 1, "result": null})
        ));
    }

    #[test]
    fn file_uri_percent_encodes_reserved_and_non_ascii_bytes() {
        let uri = path_to_uri(Path::new("/tmp/Grey #100%/中文?.rs")).unwrap();
        assert_eq!(
            uri.as_str(),
            "file:///tmp/Grey%20%23100%25/%E4%B8%AD%E6%96%87%3F.rs"
        );
    }

    #[test]
    fn workspace_root_prefers_the_workspace_manifest_over_crate_manifest() {
        let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let file = crate_dir.join("src/lib.rs");
        let expected = crate_dir.parent().and_then(Path::parent).unwrap();

        assert_eq!(discover_workspace_root(&file).unwrap(), expected);
    }

    #[test]
    fn symbol_position_targets_a_function_name_in_utf16_coordinates() {
        let source = "// 🦀\nfn café() {}\n";
        let position = definition_probe_position(source).unwrap();

        assert_eq!(position.line, 1);
        assert_eq!(position.character, 3);
    }

    #[test]
    fn symbol_position_prefers_a_call_site_over_the_declaration() {
        let source = "fn target() {}\nfn caller() { target(); }\n";
        let position = definition_probe_position(source).unwrap();

        assert_eq!(position.line, 1);
        assert_eq!(position.character, 14);
    }

    #[cfg(unix)]
    fn shell_lsp_client(script: &str, marker: Option<&Path>) -> LspClient {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Some(marker) = marker {
            command.env("GREY_LSP_TEST_MARKER", marker);
        }
        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        LspClient {
            transport: JsonRpcTransport {
                reader: stdout,
                writer: Some(stdin),
            },
            child,
            next_id: 1,
            pending_notifications: VecDeque::new(),
        }
    }
}
