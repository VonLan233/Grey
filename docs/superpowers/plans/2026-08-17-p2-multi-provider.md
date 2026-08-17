# P2: Multi-Provider, Token Budget, and Caching — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Upgrade Grey from a single-hardcoded-provider harness to a dynamic multi-provider system with routing, failover, token budgeting, request caching, and usage tracking.

**Architecture:** Progressive extension of existing crates (no new crates). grey-core gains token/cache/usage/summary modules; grey-provider gains router/fallback/gemini modules; grey-cli gains new flags and subcommands.

**Tech Stack:** Rust 1.97.1, tokio, reqwest, rusqlite, tiktoken-rs, sha2, serde

**Spec:** `docs/superpowers/specs/2026-08-17-p2-multi-provider-design.md`

## Global Constraints

- Rust toolchain: 1.97.1 (via `rust-toolchain.toml`)
- PATH must include `~/.cargo/bin` before Homebrew cargo
- All tests must pass: `cargo test --workspace --all-features`
- No `#[allow]` for clippy violations
- TDD: write failing test first, then implement, then verify pass
- Each task ends with a commit
- Existing P1 tests (77) must not regress

---

## File Structure

### New files

| File | Responsibility |
|---|---|
| `crates/grey-core/src/token.rs` | Token counting trait + tiktoken-rs + char-approx impls |
| `crates/grey-core/src/cache.rs` | SQLite-backed request prefix cache |
| `crates/grey-core/src/usage.rs` | Usage tracking + cost estimation |
| `crates/grey-core/src/summary.rs` | Rolling conversation summary engine |
| `crates/grey-provider/src/router.rs` | ProviderRouter: resolve provider+model |
| `crates/grey-provider/src/fallback.rs` | FallbackChain: unified failover |
| `crates/grey-provider/src/gemini.rs` | Gemini API streaming adapter |
| `crates/grey-cli/tests/p2.rs` | P2 integration tests |

### Modified files

| File | Changes |
|---|---|
| `Cargo.toml` (workspace) | Add tiktoken-rs, sha2, hex deps |
| `crates/grey-core/Cargo.toml` | Add tiktoken-rs, sha2, hex |
| `crates/grey-core/src/lib.rs` | Export new modules |
| `crates/grey-core/src/config.rs` | Rewrite for dynamic providers table |
| `crates/grey-core/src/context.rs` | Token budget + summary + tool trim |
| `crates/grey-core/src/provider.rs` | Extended ContextAudit |
| `crates/grey-core/src/agent.rs` | Router + cache + usage integration |
| `crates/grey-provider/Cargo.toml` | (no new deps) |
| `crates/grey-provider/src/lib.rs` | ProviderRouter factory |
| `crates/grey-cli/src/main.rs` | New flags + subcommands |

---

## Task 1: Add workspace dependencies (tiktoken-rs, sha2, hex)

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Modify: `crates/grey-core/Cargo.toml`

**Interfaces:**
- Consumes: nothing
- Produces: `tiktoken-rs`, `sha2`, `hex` available as workspace deps

- [ ] **Step 1: Add workspace dependencies**

In `Cargo.toml` under `[workspace.dependencies]`, add after the `uuid` line:

```toml
tiktoken-rs = "0.6"
sha2 = "0.10"
hex = "0.4"
```

- [ ] **Step 2: Add deps to grey-core**

In `crates/grey-core/Cargo.toml` under `[dependencies]`, add:

```toml
sha2 = { workspace = true }
hex = { workspace = true }
tiktoken-rs = { workspace = true }
```

- [ ] **Step 3: Verify build**

Run: `cargo build --workspace 2>&1 | tail -5`
Expected: `Finished` with no errors (deps download + compile)

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock crates/grey-core/Cargo.toml
git commit -m "P2: add tiktoken-rs, sha2, hex workspace deps"
```

---

## Task 2: Token counting (token.rs)

**Files:**
- Create: `crates/grey-core/src/token.rs`
- Modify: `crates/grey-core/src/lib.rs`

**Interfaces:**
- Consumes: `ChatMessage` from `grey-core::provider`
- Produces: `TokenCounter` trait, `TiktokenCounter`, `CharApproxCounter`

- [ ] **Step 1: Write token.rs with tests**

Create `crates/grey-core/src/token.rs`. Full code:

```rust
//! Token counting: tiktoken-rs for OpenAI models, char-approx for others.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::ChatMessage;

pub trait TokenCounter: Send + Sync {
    fn count(&self, text: &str, model: &str) -> u64;
    fn count_messages(&self, messages: &[ChatMessage], model: &str) -> u64 {
        messages.iter().map(|m| self.count(&m.content, model)).sum()
    }
}

pub struct TiktokenCounter {
    encoders: Mutex<HashMap<String, tiktoken_rs::CoreBPE>>,
}

impl TiktokenCounter {
    pub fn new() -> Self {
        Self { encoders: Mutex::new(HashMap::new()) }
    }

    fn encoder_for(&self, model: &str) -> Option<tiktoken_rs::CoreBPE> {
        let mut cache = self.encoders.lock().unwrap();
        if let Some(enc) = cache.get(model) {
            return Some(enc.clone());
        }
        let enc = if model.starts_with("gpt-4o") {
            tiktoken_rs::o200k_base().ok()
        } else if model.starts_with("gpt-4") || model.starts_with("gpt-3.5") {
            tiktoken_rs::cl100k_base().ok()
        } else {
            None
        };
        if let Some(ref e) = enc {
            cache.insert(model.to_string(), e.clone());
        }
        enc
    }
}

impl TokenCounter for TiktokenCounter {
    fn count(&self, text: &str, model: &str) -> u64 {
        match self.encoder_for(model) {
            Some(enc) => enc.encode_with_special_tokens(text).len() as u64,
            None => CharApproxCounter.count(text, model),
        }
    }
}

pub struct CharApproxCounter;

impl TokenCounter for CharApproxCounter {
    fn count(&self, text: &str, _model: &str) -> u64 {
        (text.len() as u64).div_ceil(4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiktoken_counts_known_openai_string() {
        let c = TiktokenCounter::new();
        let n = c.count("hello world", "gpt-4o");
        assert!(n > 0 && n <= 3, "got {n}");
    }

    #[test]
    fn char_approx_is_len_div_4() {
        assert_eq!(CharApproxCounter.count("hello world!", "x"), 3);
    }

    #[test]
    fn tiktoken_falls_back_for_unknown_model() {
        let c = TiktokenCounter::new();
        assert_eq!(c.count("hello world", "claude-sonnet-4-5"), 3);
    }
}
```

- [ ] **Step 2: Register module in lib.rs**

In `crates/grey-core/src/lib.rs`, add after the existing `pub mod` lines:

```rust
pub mod token;
pub use token::{CharApproxCounter, TiktokenCounter, TokenCounter};
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p grey-core token 2>&1 | tail -10`
Expected: 3 tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/grey-core/src/token.rs crates/grey-core/src/lib.rs
git commit -m "P2: token counting with tiktoken-rs + char-approx fallback"
```

---

## Task 3: Dynamic provider config (config.rs rewrite)

**Files:**
- Modify: `crates/grey-core/src/config.rs`
- Modify: `crates/grey-core/src/lib.rs`

**Interfaces:**
- Consumes: nothing (self-contained)
- Produces: `ProviderEntry`, `ModelEntry`, `RouteRule`, `TaskKind`, `FallbackConfig`, `ContextConfig`, `CacheConfig`, `UsageConfig`, rewritten `GreyConfig`

- [ ] **Step 1: Write failing tests for new config parsing**

Append to the `#[cfg(test)] mod tests` block in `crates/grey-core/src/config.rs`:

```rust
#[test]
fn parses_dynamic_providers_table() {
    let toml_str = r#"
default_provider = "astrdark"
default_model = "glm-5.2"

[[providers]]
id = "mock"
protocol = "mock"

[[providers]]
id = "astrdark"
protocol = "openai"
base_url = "https://api.astrdark.cyou/v1"
api_key = "sk-test"
models = [
  { id = "glm-5.2", name = "GLM 5.2" },
  { id = "claude-opus-4-7", name = "Claude Opus 4.7" },
]

[[routes]]
match = "planning"
provider = "astrdark"
model = "claude-opus-4-7"
"#;
    let cfg: GreyConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(cfg.default_provider, "astrdark");
    assert_eq!(cfg.providers.len(), 2);
    assert_eq!(cfg.providers[1].id, "astrdark");
    assert_eq!(cfg.providers[1].models.len(), 2);
    assert_eq!(cfg.routes.len(), 1);
    assert_eq!(cfg.routes[0].match_kind, TaskKind::Planning);
}

#[test]
fn migrates_legacy_openai_section() {
    let toml_str = r#"
provider = "openai"
model = "gpt-4o"

[openai]
base_url = "https://api.openai.com/v1"
api_key = "sk-test"
model = "gpt-4o"
"#;
    let cfg: GreyConfig = toml::from_str(toml_str).unwrap();
    // Legacy section produces a [[providers]] entry
    assert!(cfg.providers.iter().any(|p| p.id == "openai"));
    assert_eq!(cfg.default_provider, "openai");
}

#[test]
fn parses_fallback_and_context_config() {
    let toml_str = r#"
default_provider = "mock"
default_model = "m"

[[providers]]
id = "mock"
protocol = "mock"

[fallback]
providers = ["mock"]

[context]
max_tokens = 65536
system_budget = 2048
history_budget = 32768
tool_output_budget = 8192
input_budget = 16384
summary_threshold = 10
summary_max_messages = 3

[cache]
enabled = true
max_entries = 500
ttl_hours = 12
"#;
    let cfg: GreyConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(cfg.fallback.providers, vec!["mock"]);
    assert_eq!(cfg.context.max_tokens, 65536);
    assert_eq!(cfg.cache.enabled, true);
}
```

- [ ] **Step 2: Rewrite config.rs structs and merge logic**

Replace the structs in `crates/grey-core/src/config.rs` with:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GreyConfig {
    pub default_provider: String,
    pub default_model: String,
    pub providers: Vec<ProviderEntry>,
    pub routes: Vec<RouteRule>,
    pub fallback: FallbackConfig,
    pub context: ContextConfig,
    pub cache: CacheConfig,
    pub usage: UsageConfig,
    // Legacy fields for backward-compat migration
    pub provider: String,
    pub model: String,
    pub openai: OpenAiConfig,
    pub anthropic: AnthropicConfig,
    pub lsp: LspConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderEntry {
    pub id: String,
    pub protocol: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub max_tokens: u32,
    #[serde(default)]
    pub include_usage: bool,
    #[serde(default)]
    pub models: Vec<ModelEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelEntry {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub context_limit: u64,
    #[serde(default)]
    pub output_limit: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskKind {
    Planning,
    Coding,
    Fast,
    Default,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RouteRule {
    pub match_kind: TaskKind,
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FallbackConfig {
    #[serde(default)]
    pub providers: Vec<String>,
    #[serde(default)]
    pub models: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    pub max_tokens: u64,
    pub system_budget: u64,
    pub history_budget: u64,
    pub tool_output_budget: u64,
    pub input_budget: u64,
    pub summary_threshold: usize,
    pub summary_max_messages: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    pub enabled: bool,
    pub max_entries: usize,
    pub ttl_hours: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageConfig {
    pub track: bool,
    #[serde(default)]
    pub cost_per_1m_input: HashMap<String, f64>,
    #[serde(default)]
    pub cost_per_1m_output: HashMap<String, f64>,
}
```

Add `use std::collections::HashMap;` at the top. Implement `Default` for all new structs with sensible defaults. The `load()` function must call `migrate_legacy(&mut cfg)` after parsing.

- [ ] **Step 3: Run tests**

Run: `cargo test -p grey-core config 2>&1 | tail -10`
Expected: all config tests pass (new + legacy).

- [ ] **Step 4: Commit**

```bash
git add crates/grey-core/src/config.rs crates/grey-core/src/lib.rs
git commit -m "P2: dynamic [[providers]] config with legacy migration"
```

---

## Task 4: FallbackChain (fallback.rs)

**Files:**
- Create: `crates/grey-provider/src/fallback.rs`
- Modify: `crates/grey-provider/src/lib.rs`

**Interfaces:**
- Consumes: `FallbackConfig` from `grey-core::config`
- Produces: `ProviderModelRef`, `FallbackChain`, `HealthState`

Per spec §6. Key behaviors:
- `ProviderModelRef { provider, model }` with `Display` as `"provider/model"`.
- `resolve(primary)` returns ordered list: primary → model-level fallbacks → provider-order fallbacks.
- `mark_failed(pmr, error)`: 3 consecutive failures → 60s cooldown, exponential backoff (cap 30 min).
- `is_healthy(pmr)`: true if not in cooldown.
- Each ref tried at most once per request.

- [ ] **Step 1: Write fallback.rs with tests**

Tests: chain order, cooldown activation, exponential backoff, each-ref-once.

- [ ] **Step 2: Register module**

In `crates/grey-provider/src/lib.rs`: `pub mod fallback;`

- [ ] **Step 3: Run tests**

Run: `cargo test -p grey-provider fallback 2>&1 | tail -10`
Expected: all fallback tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/grey-provider/src/fallback.rs crates/grey-provider/src/lib.rs
git commit -m "P2: FallbackChain with provider+model level failover"
```

---

## Task 5: ProviderRouter (router.rs)

**Files:**
- Create: `crates/grey-provider/src/router.rs`
- Modify: `crates/grey-provider/src/lib.rs`

**Interfaces:**
- Consumes: `Provider` trait, `GreyConfig`, `FallbackChain`, `ProviderModelRef`
- Produces: `ProviderRouter`, `ResolvedProvider`

Per spec §5. Key behaviors:
- `from_config(cfg)` builds all providers from `[[providers]]` entries.
- `resolve(task)` picks provider+model via `[[routes]]` or default.
- `resolve_explicit(provider, model)` for CLI overrides.
- `stream_chat(request, resolved)` tries primary, then fallback chain.

- [ ] **Step 1: Write router.rs with tests**

Tests: task routing, explicit override, unknown provider error, fallback on primary failure, no-fallback after visible output.

- [ ] **Step 2: Register module** in `crates/grey-provider/src/lib.rs`

- [ ] **Step 3: Run** `cargo test -p grey-provider router 2>&1 | tail -10`

- [ ] **Step 4: Commit**

```bash
git add crates/grey-provider/src/router.rs crates/grey-provider/src/lib.rs
git commit -m "P2: ProviderRouter with task routing and failover"
```

---

## Task 6: Gemini adapter (gemini.rs)

**Files:**
- Create: `crates/grey-provider/src/gemini.rs`
- Modify: `crates/grey-provider/src/lib.rs` (register module + add "gemini" arm to build_provider)

**Interfaces:**
- Consumes: `Provider` trait, `ChatRequest`, `SseDecoder`
- Produces: `GeminiProvider` implementing `Provider`

Per spec §12. Key behaviors:
- `POST {base_url}/models/{model}:streamGenerateContent?key={api_key}`
- Converts `ChatRequest` → Gemini `contents` array with `role`/`parts`
- SSE parsing reuses `SseDecoder`
- `candidates[].content.parts[].text` → `Delta`, `functionCall` → `ToolCall`, `usageMetadata` → `Done`

- [ ] **Step 1: Write gemini.rs with tests**

Tests: request body conversion, SSE fragmentation, tool call parsing, usage extraction. Use `serve_one_sse` helper.

- [ ] **Step 2: Register module** + add `"gemini"` arm to `build_provider` in lib.rs

- [ ] **Step 3: Run** `cargo test -p grey-provider gemini 2>&1 | tail -10`

- [ ] **Step 4: Commit**

```bash
git add crates/grey-provider/src/gemini.rs crates/grey-provider/src/lib.rs
git commit -m "P2: Gemini provider adapter"
```

---

## Task 7: Request cache (cache.rs)

**Files:**
- Create: `crates/grey-core/src/cache.rs`
- Modify: `crates/grey-core/src/lib.rs`

**Interfaces:**
- Consumes: `ChatMessage`, `ToolCall`, `Usage` from `grey-core::provider`
- Produces: `RequestCache`, `CacheConfig`, `CachedResponse`

Per spec §10. Key behaviors:
- SQLite schema: `cache_entries (key TEXT PK, model TEXT, response_json TEXT, created_at INT, last_accessed INT)`
- `get(model, messages)`: SHA256(model || JSON(messages)), TTL check, returns CachedResponse
- `put(model, messages, response)`: store + LRU evict if over max_entries
- `evict()`: remove expired. `clear()`: remove all.

- [ ] **Step 1: Write cache.rs with tests**

Tests: put/get roundtrip, TTL expiry, LRU eviction, key collision, clear.

- [ ] **Step 2: Register module** in lib.rs: `pub mod cache; pub use cache::{RequestCache, CacheConfig, CachedResponse};`

- [ ] **Step 3: Run** `cargo test -p grey-core cache 2>&1 | tail -10`

- [ ] **Step 4: Commit**

```bash
git add crates/grey-core/src/cache.rs crates/grey-core/src/lib.rs
git commit -m "P2: SQLite request prefix cache with TTL+LRU"
```

---

## Task 8: Usage tracking (usage.rs)

**Files:**
- Create: `crates/grey-core/src/usage.rs`
- Modify: `crates/grey-core/src/lib.rs`
- Modify: `crates/grey-core/src/session.rs` (add usage_json column, schema v2)

**Interfaces:**
- Consumes: `ProviderModelRef`, `UsageConfig`, `Usage`
- Produces: `UsageTracker`, `SessionUsage`, `TurnUsage`, `CostRate`

Per spec §11. Key behaviors:
- `UsageTracker::record(session_id, turn)`: accumulate per-session
- `format_panel(session_id)`: human-readable token + cost summary
- Persist to `sessions.usage_json` column (schema v2 migration)
- Cost = `(input_tokens/1M * input_per_1m) + (output_tokens/1M * output_per_1m)`

- [ ] **Step 1: Write usage.rs with tests**

Tests: cost calculation, per-session accumulation, persistence to SQLite.

- [ ] **Step 2: Register module** in lib.rs: `pub mod usage; pub use usage::{UsageTracker, SessionUsage, TurnUsage, CostRate};`

- [ ] **Step 3: Add usage_json to session schema**

In `crates/grey-core/src/session.rs`: bump `SCHEMA_VERSION` to 2, add `usage_json TEXT` column, migration `ALTER TABLE sessions ADD COLUMN usage_json TEXT`.

- [ ] **Step 4: Run** `cargo test -p grey-core usage 2>&1 | tail -10` + `cargo test -p grey-core session 2>&1 | tail -10`

- [ ] **Step 5: Commit**

```bash
git add crates/grey-core/src/usage.rs crates/grey-core/src/lib.rs crates/grey-core/src/session.rs
git commit -m "P2: usage tracking + cost estimation + session schema v2"
```

---

## Task 9: Summary engine (summary.rs)

**Files:**
- Create: `crates/grey-core/src/summary.rs`
- Modify: `crates/grey-core/src/lib.rs`

**Interfaces:**
- Consumes: `Provider` trait, `ChatMessage`
- Produces: `SummaryEngine`

Per spec §9. Key behaviors:
- `summarize(messages)`: sends a dedicated request to the provider asking to compress messages into a summary
- Summary cached in-memory per session to avoid re-summarizing same prefix
- If provider offline/fails, fall back to dropping oldest (degraded mode)

- [ ] **Step 1: Write summary.rs with tests**

Tests: summarize with mock provider, failure fallback to drop, in-memory cache.

- [ ] **Step 2: Register module** in lib.rs: `pub mod summary; pub use summary::SummaryEngine;`

- [ ] **Step 3: Run** `cargo test -p grey-core summary 2>&1 | tail -10`

- [ ] **Step 4: Commit**

```bash
git add crates/grey-core/src/summary.rs crates/grey-core/src/lib.rs
git commit -m "P2: rolling conversation summary engine"
```

---

## Task 10: Context manager extension (context.rs)

**Files:**
- Modify: `crates/grey-core/src/context.rs`

**Interfaces:**
- Consumes: `TokenCounter`, `SummaryEngine`, `ContextConfig`
- Produces: Extended `ContextManager`, `TokenBudget`, extended `ContextAudit`

Per spec §8. Key behaviors:
- `prepare(messages)`: count tokens → if over budget: truncate tool outputs → summarize old messages → drop oldest
- `ContextAudit` gains `retained_tokens`, `summary_created`, `tool_outputs_truncated`
- Budget allocation: system/history/tools/input/total from config

- [ ] **Step 1: Write failing tests for budget enforcement**

Tests: under-budget returns as-is, tool-output truncation triggers audit, summary trigger at threshold, drop-oldest fallback, ContextAudit fields.

- [ ] **Step 2: Extend ContextManager struct and prepare()**

Add `TokenBudget`, `TokenCounter`, `SummaryEngine`, `ContextConfig` fields. Rewrite `prepare()` per spec §8.1 pipeline.

- [ ] **Step 3: Run** `cargo test -p grey-core context 2>&1 | tail -10`

- [ ] **Step 4: Commit**

```bash
git add crates/grey-core/src/context.rs
git commit -m "P2: context manager with token budget, summary, tool trim"
```

---

## Task 11: Agent loop integration (agent.rs)

**Files:**
- Modify: `crates/grey-core/src/agent.rs`

**Interfaces:**
- Consumes: `ProviderRouter`, `RequestCache`, `UsageTracker`, `FallbackChain`
- Produces: Extended `Agent` with router/cache/usage, `AgentEvent::ProviderSwitched`

Per spec §13.3. Key behaviors:
- Agent constructor gains `router` and `cache` params
- `Agent::new_legacy(provider, tools, context, options)` wraps single provider in no-op router (backward compat)
- Before each `stream_chat()`: check cache → if hit, return cached response as single Done stream
- After each turn: record `TurnUsage`
- On fallback: emit `AgentEvent::ProviderSwitched { from, to, reason }`

- [ ] **Step 1: Write failing tests for cache hit and fallback events**

Tests: cached response returned without provider call, fallback emits ProviderSwitched, usage recorded after turn.

- [ ] **Step 2: Extend Agent struct and stream_turn()**

Add `router: ProviderRouter`, `cache: Option<RequestCache>`, `usage: Option<UsageTracker>` fields. Rewrite `stream_turn()` to use router + cache + usage.

- [ ] **Step 3: Add Agent::new_legacy() for backward compat**

Wraps a single `Arc<dyn Provider>` in a no-op router (one entry, no fallback).

- [ ] **Step 4: Run** `cargo test -p grey-core agent 2>&1 | tail -10`

- [ ] **Step 5: Commit**

```bash
git add crates/grey-core/src/agent.rs
git commit -m "P2: agent loop with router, cache, usage integration"
```

---

## Task 12: CLI integration (main.rs)

**Files:**
- Modify: `crates/grey-cli/src/main.rs`

**Interfaces:**
- Consumes: `ProviderRouter`, `RequestCache`, `UsageTracker`, `TaskKind`
- Produces: New CLI flags + subcommands

Per spec §13. Key changes:
- New flags: `--task <KIND>`, `--no-cache`, `--no-fallback`
- New subcommands: `providers list/show`, `cache clear/stats`, `usage show/summary`
- `build_agent_and_session()` uses `ProviderRouter::from_config()` instead of `build_provider()`
- `HeadlessOutput` gains `usage` object with `input_tokens`, `output_tokens`, `cost_usd`, `cached`, `provider`, `model`

- [ ] **Step 1: Add new CLI flags and subcommands**

Add `--task`, `--no-cache`, `--no-fallback` to `Cli` struct. Add `Providers`, `Cache`, `Usage` to `Command` enum.

- [ ] **Step 2: Rewrite build_agent_and_session()**

Replace `build_provider()` + `model_for_provider()` with `ProviderRouter::from_config()` + `router.resolve(task)` or `router.resolve_explicit()`.

- [ ] **Step 3: Implement new subcommands**

`providers list`: print all providers + models. `providers show <ID>`: print config (masked). `cache clear/stats`. `usage show <SID>/summary`.

- [ ] **Step 4: Extend HeadlessOutput JSON**

Add `usage` object to JSON output.

- [ ] **Step 5: Run** `cargo test -p grey-cli 2>&1 | tail -15`

- [ ] **Step 6: Commit**

```bash
git add crates/grey-cli/src/main.rs
git commit -m "P2: CLI with --task, providers, cache, usage subcommands"
```

---

## Task 13: Integration tests + release gate

**Files:**
- Create: `crates/grey-cli/tests/p2.rs`
- Modify: `README.md` (update status + quick start)

**Interfaces:**
- Consumes: All P2 modules
- Produces: Integration test coverage + updated docs

Per spec §14.2 + §15.

- [ ] **Step 1: Write integration tests in p2.rs**

Tests:
- Multi-provider config loads and routes correctly
- Fallback: primary fails, secondary succeeds
- Fallback: all providers fail, error surfaced
- Cache: second identical request returns cached response
- Cache: `--no-cache` bypasses cache
- Usage: `grey usage show` prints token counts and cost
- Legacy config auto-migrates
- `grey providers list` shows all configured providers
- `grey --task planning` routes to planning model

- [ ] **Step 2: Run** `cargo test -p grey-cli --test p2 2>&1 | tail -15`

- [ ] **Step 3: Run full release gate**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo build --workspace --release --locked
cargo run -q -p grey-cli -- --no-save "hello"
```
Expected: all pass, 0 failures.

- [ ] **Step 4: Update README.md**

Update "当前状态" section to reflect P2 completion. Add `--task`, `providers`, `cache`, `usage` to quick start.

- [ ] **Step 5: Commit**

```bash
git add crates/grey-cli/tests/p2.rs README.md
git commit -m "P2: integration tests + release gate + docs"
```

---

## Self-Review

### Spec coverage

| Spec section | Task(s) |
|---|---|
| §4 Config format | Task 3 |
| §5 ProviderRouter | Task 5 |
| §6 FallbackChain | Task 4 |
| §7 Token counting | Task 2 |
| §8 Context manager | Task 10 |
| §9 Summary engine | Task 9 |
| §10 Request cache | Task 7 |
| §11 Usage tracking | Task 8 |
| §12 Gemini adapter | Task 6 |
| §13 CLI integration | Task 12 |
| §14 Testing strategy | Task 13 (integration) + each task (unit) |
| §15 Acceptance criteria | Task 13 (release gate) |
| §16 Migration plan | Task 3 (config) + Task 8 (session DB) |

No gaps. All 6 deliverables covered.

### Placeholder scan

No TBD/TODO/FIXME in the plan. All tasks reference exact spec sections for full signatures.

### Type consistency

- `ProviderModelRef` used in Task 4 (defined) → Task 5 (consumed) → Task 8 (consumed) ✓
- `FallbackChain` used in Task 4 (defined) → Task 5 (consumed) ✓
- `TokenCounter` used in Task 2 (defined) → Task 10 (consumed) ✓
- `SummaryEngine` used in Task 9 (defined) → Task 10 (consumed) ✓
- `RequestCache` used in Task 7 (defined) → Task 11 (consumed) ✓
- `UsageTracker` used in Task 8 (defined) → Task 11 (consumed) → Task 12 (consumed) ✓

## Review Findings (Sisyphus review, 2026-08-17)

### Issues found

| # | Severity | Issue | Fix |
|---|---|---|---|
| 1 | High | Tasks 4-9 lack full test+impl code (only behavior descriptions) | Acceptable: spec sections provide full signatures; implementer fills code during TDD. Not a blocker. |
| 2 | Medium | `RouteRule.match_kind` field name vs TOML `match` key — serde rename needed | Add `#[serde(rename = "match")]` to `match_kind` field in Task 3. |
| 3 | Medium | Task 3 test asserts `match_kind` but TOML uses `match` | Fixed by issue 2 fix. |
| 4 | Medium | `ProviderModelRef` defined in grey-provider (Task 4) but grey-core's usage.rs (Task 8) needs it — crate dependency direction violation | Move `ProviderModelRef` to grey-core (it's a plain value type, belongs in core contracts). |
| 5 | Low | `Agent::new_legacy` needs `ProviderRouter` from grey-provider, but grey-core can't depend on grey-provider (ADR-001) | Agent takes `Arc<dyn Provider>` + `Option<FallbackChain>` via trait injection, not `ProviderRouter` directly. Router stays in CLI layer. |
| 6 | Low | No ADR-002 task | Add as part of Task 13 (docs). |

### Resolution

Issues 2-3: serde rename, trivial fix during Task 3 implementation.
Issue 4: `ProviderModelRef` moves to `grey-core/src/provider.rs`, re-exported. Task 4 defines `FallbackChain` in grey-provider, importing `ProviderModelRef` from grey-core.
Issue 5: Agent struct gains `fallback: Option<FallbackChain>` field. `FallbackChain` trait/interface defined in grey-core, concrete impl in grey-provider. CLI wires router+fallback into Agent.
Issue 6: ADR-002 written in Task 13.

**Conclusion: Plan is sound. Issues are fixable during implementation. Proceeding to TDD.**
