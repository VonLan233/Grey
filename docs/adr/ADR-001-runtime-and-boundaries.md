# ADR-001: Runtime, protocol, and crate boundaries

- Status: Accepted
- Date: 2026-08-16

## Context

Grey prioritizes a small binary, fast startup, provider portability, and a UI
that can be replaced without rewriting the runtime. The P0 spikes validate Rust,
ratatui, async HTTP streaming, and a thin LSP client, but they do not yet form an
agent harness.

## Decision

Grey uses Rust stable, Tokio, ratatui/crossterm, reqwest with rustls, serde, and
SQLite through rusqlite. The crate dependency direction is:

```text
grey-cli -> grey-tui
grey-cli -> grey-tools -> grey-core
grey-cli -> grey-provider -> grey-core
grey-cli -> grey-lsp
grey-tui -> grey-core
grey-core -> no Grey integration crate
```

`grey-core` owns normalized chat messages, provider/tool contracts, the bounded
agent loop, context budgeting, session persistence, and runtime events.

`grey-provider` owns vendor request/response conversion. OpenAI-compatible and
Anthropic streams are converted into the same `ProviderEvent` contract. A
provider factory rejects unknown identifiers.

`grey-tools` owns the five P1 tools and enforces workspace confinement and
approval before side effects. The Core only sees a `ToolExecutor` trait, which is
the future common seam for built-ins and MCP.

`grey-tui` is an event consumer/command producer. It never calls a provider or a
tool directly. The CLI is the composition root that wires the event channels.

LSP remains a thin Tokio JSON-RPC client using `lsp-types`. `tower-lsp` is a good
server framework but does not by itself solve Grey's client transport needs. The
existing P0 spike is retained and hardened separately from the P1 loop.

Configuration precedence is deterministic:

```text
built-in defaults < selected TOML file < GREY_* environment variables < CLI flags
```

## Safety model

- Paths are resolved under one canonical workspace.
- Read/search tools are side-effect free and pre-approved.
- Edit and shell tools require `ask`, `read-only`, or explicit `auto` policy.
- Edits require one exact match and use an atomic same-directory replacement.
- Tool output, file reads, and context are capped to prevent unbounded token use.
- Agent iterations and provider retries are capped.
- Secrets are redacted from display and never written to session messages.

## Consequences

The runtime is reusable by headless CLI, TUI, and future sub-agents. Adding MCP
does not require changing the loop: an MCP registry can implement
`ToolExecutor`. Adding a provider only requires wire conversion.

SQLite and HTTP add binary weight, accepted for the P1 persistence and provider
requirements. WASM plugins, multi-agent orchestration, advanced LSP semantics,
image input, desktop notifications, and the full P2 context/caching system are
explicit later decisions rather than placeholder claims in this ADR.

