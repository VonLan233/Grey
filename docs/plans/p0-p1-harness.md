# P0 + P1 Coding Harness Implementation Plan

Date: 2026-08-16

This plan is test-first. Each task names exact files, executable checks, and the
observable result that closes the task.

## Task 1: Normalize Core protocol and implement the bounded agent loop

Files:

- `crates/grey-core/src/provider.rs`
- `crates/grey-core/src/tool.rs`
- `crates/grey-core/src/context.rs`
- `crates/grey-core/src/agent.rs`
- `crates/grey-core/src/lib.rs`
- `crates/grey-core/Cargo.toml`

Write failing tests first for message serialization, tool-result preservation,
context retention, a two-turn tool loop, iteration limits, surfaced provider
errors, and retry-before-output. The public runtime contract will be:

```rust
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    async fn stream_chat<'a>(
        &'a self,
        request: &'a ChatRequest,
    ) -> anyhow::Result<futures_util::stream::BoxStream<'a, ProviderEvent>>;
}

#[async_trait::async_trait]
pub trait ToolExecutor: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;
    async fn execute(&self, call: &ToolCall) -> ToolResult;
}

pub struct Agent<P, T> {
    provider: P,
    tools: T,
    options: AgentOptions,
    context: ContextManager,
}
```

Command:

```bash
cargo test -p grey-core
```

Expected result: all Core tests pass; the scripted provider proves the exact
`assistant -> tool call -> tool result -> assistant` sequence.

## Task 2: Add lossless SQLite session persistence

Files:

- `crates/grey-core/src/session.rs`
- `crates/grey-core/src/config.rs`
- `crates/grey-core/src/lib.rs`
- `crates/grey-core/Cargo.toml`

Write failing temporary-database tests for create/save/load/list/latest and for
assistant tool-call/tool-result JSON round trips. Use schema migration version 1
and transactions for message replacement.

Command:

```bash
cargo test -p grey-core session
```

Expected result: a session saved to a temporary SQLite file reopens with the
same id, ordered messages, tool calls, timestamps, title, and workspace.

## Task 3: Implement workspace-scoped built-in tools

Files:

- `crates/grey-tools/Cargo.toml`
- `crates/grey-tools/src/lib.rs`
- `crates/grey-tools/tests/tools.rs`
- workspace `Cargo.toml`

The registry will expose exactly these definitions:

```rust
pub const BUILTIN_TOOL_NAMES: [&str; 5] = [
    "read_file",
    "edit_file",
    "bash",
    "glob",
    "grep",
];
```

Tests must begin failing for traversal (`../`), symlink escape, output caps,
missing and duplicate edit matches, atomic successful replacement, read-only
denial, approval denial, command timeout, glob ignore behavior, and grep line
numbers.

Command:

```bash
cargo test -p grey-tools
```

Expected result: all five tools return structured `ToolResult`s; invalid or
denied operations leave the filesystem unchanged.

## Task 4: Repair Provider streaming and add Anthropic

Files:

- `crates/grey-provider/src/lib.rs`
- `crates/grey-provider/src/mock.rs`
- `crates/grey-provider/src/openai.rs`
- `crates/grey-provider/src/anthropic.rs`
- `crates/grey-provider/Cargo.toml`
- `crates/grey-core/src/config.rs`

Write failing offline tests for byte-by-byte SSE fragmentation, CRLF framing,
multiple events per chunk, malformed payloads, error propagation, OpenAI
incremental tool-call assembly, Anthropic `text_delta` plus
`input_json_delta`, usage, message conversion, auth headers, and unknown
provider rejection.

Command:

```bash
cargo test -p grey-provider
```

Expected result: protocol tests pass without API keys or internet access; every
successful stream ends once, and malformed/incomplete streams return an error.

## Task 5: Connect the real CLI and TUI to Core

Files:

- `crates/grey-cli/src/main.rs`
- `crates/grey-cli/tests/cli.rs`
- `crates/grey-cli/Cargo.toml`
- `crates/grey-tui/src/lib.rs`
- `crates/grey-tui/Cargo.toml`

Add failing CLI tests for `grey "prompt"`, JSON output, invalid provider,
nonexistent resume id, no-save behavior, and session list/show. Refactor TUI
state into testable input/event reducers; verify scroll is applied and terminal
cleanup uses RAII.

Commands:

```bash
cargo test -p grey-cli -p grey-tui
cargo run -q -p grey-cli -- --no-save --format json "hello"
```

Expected JSON shape:

```json
{
  "response": "（mock grey-default）收到你的消息：hello\n\n这是 Grey 模拟流式输出。真实 Provider 接入后，这里会出现模型回复。",
  "session_id": null,
  "usage": {
    "input_tokens": 0,
    "output_tokens": 0
  }
}
```

The exact deterministic token counts are asserted by tests rather than treated
as provider-independent values in the CLI contract.

## Task 6: Harden P0 LSP/TUI spikes and measure the demo path

Files:

- `crates/grey-lsp/src/lib.rs`
- `crates/grey-tui/src/lib.rs`
- relevant crate tests

Add framing tests using in-memory async streams where possible, retain the child
process for shutdown, discover the workspace root, and make the stream demo's
frame statistics testable. Fix the TUI scroll wiring and documented keys.

Commands:

```bash
cargo test -p grey-lsp -p grey-tui
cargo run -q -p grey-cli -- spike-c "demo"
```

Expected result: unit tests pass and the mock spike prints streaming text, one
assembled sample tool call, usage, and a successful completion line.

## Task 7: Documentation, CI, spec review, quality review, and verification

Files:

- `README.md`
- `docs/阶段性开发文档.md`
- `.github/workflows/ci.yml`
- all files changed by Tasks 1–6

Update status and quick start without marking P2–P7 complete. Run independent
spec and quality reviews over complete files, fix findings, and repeat both
reviews. Then run the exact release gate:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo build --workspace --release --locked
cargo run -q -p grey-cli -- --help
cargo run -q -p grey-cli -- --no-save "hello"
```

Expected result: each command exits 0; fmt emits no diff, clippy emits no
warning, tests print zero failures, release build finishes, help documents the
headless/session/permission options, and the mock smoke test prints a complete
response without needing a network or API key.

