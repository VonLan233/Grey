# P0 + P1 Coding Harness Brainstorming

Date: 2026-08-16

## Problem framing

The repository contains working P0 spikes, while the README still describes the
project as not started. The referenced roadmap spans P0 through P7 and estimates
five to six months. This increment therefore defines “complete” as the first
usable vertical slice: a provider can stream a response, request bounded tools,
receive their results, and finish a persisted coding session through either the
TUI or the scriptable CLI.

P2–P7 remain roadmap work. Their extension seams must not be blocked, but empty
interfaces are not counted as delivered features.

## Approaches considered

### A. Extend the existing workspace into a P1 vertical slice (selected)

- Keep `grey-core` vendor- and UI-independent.
- Add the agent loop, context policy, and SQLite session store to Core.
- Add `grey-tools` for workspace-scoped built-ins.
- Keep wire-format differences inside `grey-provider`.
- Connect CLI and TUI to the same Core event stream.

This approach matches the documented layering and produces a real coding loop
without prematurely committing to the later MCP/WASM surface.

### B. Put the loop and tools directly in `grey-cli` (rejected)

This is quicker initially, but it couples headless and TUI behavior, prevents
future sub-agents from reusing the loop, and violates the documented Core/UI
boundary.

### C. Implement placeholder crates for all P0–P7 features (rejected)

This creates breadth without the acceptance evidence required by the roadmap.
The project rules explicitly reject incomplete implementations presented as
finished functionality.

## Product decisions

1. Configuration precedence is defaults < TOML < environment < CLI.
2. Unknown providers are errors; there is no silent mock fallback.
3. `grey "prompt"` is the stable headless entrypoint. `--format json` is the
   automation contract.
4. `grey` starts the actual conversation TUI when stdin/stdout are terminals.
5. Read-only tools run without confirmation. `edit_file` and `bash` require an
   approval policy; `--auto-approve` is explicit and visible.
6. All filesystem tools are confined to a canonical workspace root. Symlink
   escapes are rejected.
7. `edit_file` only edits an existing file and requires exactly one
   `old_string` match. The replacement is written atomically.
8. Every provider request passes through a conservative context manager.
9. SQLite stores complete normalized messages, including assistant tool calls
   and tool results, so a session can resume losslessly.
10. Provider streams must surface parse/network errors, buffer fragmented SSE,
    assemble tool calls, and emit exactly one terminal usage event.

## Acceptance slice

- OpenAI-compatible, Anthropic, and deterministic mock providers.
- Streaming text plus structured tool calls.
- `read_file`, `edit_file`, `bash`, `glob`, and `grep`.
- Bounded agent iterations, retry before visible output, Ctrl-C cancellation.
- SQLite create/list/show/resume.
- Real TUI input/output/status connected by an event bus.
- P0 spikes retained as diagnostics.
- Unit, integration, CLI, and protocol-fragmentation tests.
- README quick start, configuration reference, ADR, and CI commands.

