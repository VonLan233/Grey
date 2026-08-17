# P2: Multi-Provider, Token Budget, and Caching — Design Spec

- Date: 2026-08-17
- Status: Draft (pending user review)
- Baseline commit: `P0+P1: initial baseline`

## 1. Goal

Upgrade Grey from a single-hardcoded-provider harness to a dynamic multi-provider
system with routing, failover, token budgeting, request caching, and usage
tracking — all six P2 deliverables from the roadmap.

### Non-goals (deferred to P3+)

- Multi-agent orchestration and MCP (P3)
- Full LSP semantic tools and image input (P4)
- WASM plugins, hooks, Goal/Loop (P6)

## 2. Scope

Maps to the 6 roadmap deliverables, with the provider adapter split into
registry + Gemini for clarity:

1. **Dynamic provider registry** — `[[providers]]` table, any number of
   OpenAI-compatible / Anthropic / Gemini / Mock endpoints.
2. **Gemini adapter** — full independent protocol adapter.
3. **Model routing** — task-type routing (planning/coding/fast/default) +
   manual override via CLI.
4. **Failover** — unified `FallbackChain` abstraction covering provider-level
   and model-level fallback.
5. **Context manager** — token budget allocation, history trimming, rolling
   summaries, tool-output truncation.
6. **Request cache** — SQLite-backed prefix cache keyed by model + message hash.
7. **Usage panel** — per-session token consumption and cost estimation.

## 3. Crate layout (Method A — progressive extension)

No new crates. All P2 work extends existing crates:

```
grey-core/
  src/
    config.rs        # rewritten: dynamic [[providers]] table
    context.rs       # extended: TokenBudget + summary + tool-output trim
    cache.rs         # NEW: SQLite prefix request cache
    usage.rs         # NEW: usage tracking + cost estimation
    token.rs         # NEW: tiktoken-rs + char-approx fallback
    summary.rs       # NEW: rolling conversation summary engine
    provider.rs      # extended: ProviderEvent gains routing metadata
    agent.rs         # extended: router + cache + usage integration
grey-provider/
  src/
    lib.rs           # rewritten: ProviderRouter factory
    router.rs        # NEW: resolve provider+model by task or explicit ref
    fallback.rs      # NEW: FallbackChain (provider + model level unified)
    gemini.rs        # NEW: Gemini provider adapter
    openai.rs        # unchanged except model registry integration
    anthropic.rs     # unchanged except model registry integration
    mock.rs          # unchanged
grey-cli/
  src/main.rs        # CLI gains: --task, usage panel, cache toggle
  tests/p2.rs        # NEW: P2 integration tests
```

## 4. Configuration format

### 4.1 New TOML schema (grey.toml)

```toml
default_provider = "astrdark"
default_model = "glm-5.2"

[[providers]]
id = "mock"
protocol = "mock"

[[providers]]
id = "openai-official"
protocol = "openai"
base_url = "https://api.openai.com/v1"
api_key = "${GREY_OPENAI_API_KEY}"
models = [
  { id = "gpt-4o", name = "GPT-4o", context_limit = 128000, output_limit = 16384 },
  { id = "gpt-4o-mini", name = "GPT-4o Mini", context_limit = 128000, output_limit = 16384 },
]

[[providers]]
id = "anthropic-official"
protocol = "anthropic"
base_url = "https://api.anthropic.com/v1"
api_key = "${GREY_ANTHROPIC_API_KEY}"
version = "2023-06-01"
max_tokens = 4096
models = [
  { id = "claude-sonnet-4-5", name = "Claude Sonnet 4.5", context_limit = 200000, output_limit = 8192 },
]

[[providers]]
id = "gemini-official"
protocol = "gemini"
base_url = "https://generativelanguage.googleapis.com/v1beta"
api_key = "${GREY_GEMINI_API_KEY}"
models = [
  { id = "gemini-2.0-flash", name = "Gemini 2.0 Flash", context_limit = 1000000, output_limit = 8192 },
]

[[providers]]
id = "astrdark"
protocol = "openai"
base_url = "https://api.astrdark.cyou/v1"
api_key = "sk-..."
models = [
  { id = "glm-5.2", name = "GLM 5.2" },
  { id = "claude-opus-4-7", name = "Claude Opus 4.7" },
]

[[routes]]
match = "planning"
provider = "astrdark"
model = "claude-opus-4-7"

[[routes]]
match = "fast"
provider = "astrdark"
model = "glm-5.2"

[[routes]]
match = "default"
provider = "astrdark"
model = "glm-5.2"

[fallback]
providers = ["astrdark", "openai-official", "anthropic-official"]

[fallback.models]
"astrdark/claude-opus-4-7" = ["openai-official/gpt-4o", "anthropic-official/claude-sonnet-4-5"]

[context]
max_tokens = 128000
system_budget = 4096
history_budget = 65536
tool_output_budget = 16384
input_budget = 32768
summary_threshold = 20
summary_max_messages = 5

[cache]
enabled = true
max_entries = 1000
ttl_hours = 24

[usage]
track = true
cost_per_1m_input = { "openai-official/gpt-4o" = 2.50, "anthropic-official/claude-sonnet-4-5" = 3.00 }
cost_per_1m_output = { "openai-official/gpt-4o" = 10.00, "anthropic-official/claude-sonnet-4-5" = 15.00 }
```

### 4.2 Backward compatibility

If the TOML file contains legacy `[openai]` / `[anthropic]` sections instead of
`[[providers]]`, `config::load()` auto-migrates them into `[[providers]]`
entries at parse time. A deprecation warning is printed to stderr.

### 4.3 Environment variable mapping

Each provider's fields can be overridden by env vars using the pattern
`GREY_PROVIDER_<ID>_<FIELD>`, e.g. `GREY_PROVIDER_ASTRDARK_API_KEY`. The legacy
`GREY_OPENAI_*` / `GREY_ANTHROPIC_*` vars still work for backward compatibility.

## 5. ProviderRouter

```rust
pub struct ProviderRouter {
    providers: HashMap<String, Arc<dyn Provider>>,
    models: HashMap<String, ModelRegistry>,
    routes: Vec<RouteRule>,
    fallback: FallbackChain,
}

pub struct ModelRegistry {
    entries: HashMap<String, ModelEntry>,
}

pub struct ModelEntry {
    id: String,
    name: String,
    context_limit: u64,
    output_limit: u64,
}

pub struct RouteRule {
    match_kind: TaskKind,
    provider: String,
    model: String,
}

pub enum TaskKind {
    Planning,
    Coding,
    Fast,
    Default,
}

pub struct ResolvedProvider {
    provider: Arc<dyn Provider>,
    model: String,
    fallback_chain: Vec<ProviderModelRef>,
}

impl ProviderRouter {
    pub fn resolve(&self, task: &TaskKind) -> Result<ResolvedProvider>;
    pub fn resolve_explicit(&self, provider: &str, model: &str) -> Result<ResolvedProvider>;
    pub async fn stream_chat(
        &self,
        request: &ChatRequest,
        resolved: &ResolvedProvider,
    ) -> Result<BoxStream<ProviderEvent>>;
}
```

`router.stream_chat()` tries the primary provider/model first. On a recoverable
error (connection refused, 5xx, 429 rate limit) and no visible output yet, it
advances to the next entry in the fallback chain. Errors after visible output
are surfaced immediately (preserving P1 behavior).

## 6. FallbackChain

```rust
pub struct ProviderModelRef {
    pub provider: String,
    pub model: String,
}

pub struct FallbackChain {
    provider_order: Vec<String>,
    model_fallbacks: HashMap<ProviderModelRef, Vec<ProviderModelRef>>,
    health: Mutex<HashMap<ProviderModelRef, HealthState>>,
}

pub struct HealthState {
    consecutive_failures: u32,
    cooldown_until: Option<Instant>,
}
```

Chain resolution order:
1. The primary provider+model (if healthy).
2. Model-level fallbacks for that primary (if configured and healthy).
3. Provider-order fallbacks (if healthy).

Each ref is tried at most once per request. Cooldown: 3 consecutive failures
triggers 60s cooldown, exponential backoff (cap 30 min).

## 7. Token counting (token.rs)

```rust
pub trait TokenCounter: Send + Sync {
    fn count(&self, text: &str, model: &str) -> u64;
    fn count_messages(&self, messages: &[ChatMessage], model: &str) -> u64;
}

pub struct TiktokenCounter {
    encoders: Mutex<HashMap<String, tiktoken_rs::CoreBPE>>,
}

pub struct CharApproxCounter;
```

- OpenAI models (`gpt-*`): use `tiktoken_rs::cl100k_base` or `o200k_base`.
- Anthropic models: char approx (4 chars ~ 1 token).
- Gemini models: char approx.
- Unknown models: char approx.

A `TokenCounterFactory` maps model name to the appropriate counter.

## 8. Context manager (context.rs extension)

```rust
pub struct TokenBudget {
    pub system: u64,
    pub history: u64,
    pub tools: u64,
    pub input: u64,
    pub total: u64,
}

pub struct ContextConfig {
    pub max_tokens: u64,
    pub system_budget: u64,
    pub history_budget: u64,
    pub tool_output_budget: u64,
    pub input_budget: u64,
    pub summary_threshold: usize,
    pub summary_max_messages: usize,
}

pub struct ContextManager {
    budget: TokenBudget,
    counter: Arc<dyn TokenCounter>,
    summarizer: Option<SummaryEngine>,
    config: ContextConfig,
    model: String,
}
```

### 8.1 Preparation pipeline

1. Count tokens of all messages.
2. If total <= budget, return as-is.
3. If total > budget:
   a. Truncate tool outputs to `tool_output_budget` (keep diff + key lines).
   b. If still over: summarize oldest messages beyond `summary_threshold`
      into a single `system` message via the provider. If summarizer
      unavailable, drop oldest non-system messages.
   c. If still over: drop oldest messages (keeping system + summary + last N).
4. Emit `ContextAudit` with dropped_messages, retained_tokens, summary_created.

### 8.2 ContextAudit (extended)

```rust
pub struct ContextAudit {
    pub dropped_messages: usize,
    pub retained_chars: usize,
    pub retained_tokens: u64,
    pub budget: TokenBudget,
    pub summary_created: bool,
    pub tool_outputs_truncated: usize,
}
```

## 9. Summary engine (summary.rs)

```rust
pub struct SummaryEngine {
    provider: Arc<dyn Provider>,
    model: String,
    max_messages: usize,
}

impl SummaryEngine {
    pub async fn summarize(&self, messages: &[ChatMessage]) -> Result<ChatMessage>;
}
```

Sends a dedicated request to the provider asking it to compress the given
messages into a brief summary. Called by the context manager when history
exceeds `summary_threshold`. The summary is cached in-memory per session to
avoid re-summarizing the same prefix.

If the provider is offline or summarization fails, the context manager falls
back to dropping oldest messages (degraded mode, logged as audit event).

## 10. Request cache (cache.rs)

```rust
pub struct RequestCache {
    connection: Mutex<Connection>,
    config: CacheConfig,
}

pub struct CacheConfig {
    pub enabled: bool,
    pub max_entries: usize,
    pub ttl_hours: u64,
}

pub struct CachedResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub cached_at: i64,
}

impl RequestCache {
    pub fn open(path: &Path, config: CacheConfig) -> Result<Self>;
    pub fn get(&self, model: &str, messages: &[ChatMessage]) -> Option<CachedResponse>;
    pub fn put(&self, model: &str, messages: &[ChatMessage], response: &CachedResponse) -> Result<()>;
    pub fn evict(&self) -> Result<usize>;
    pub fn clear(&self) -> Result<()>;
}
```

### 10.1 Cache key design

Key = `SHA256(model || JSON(messages[..prefix_len]))` where `prefix_len` is the
number of messages forming the cacheable prefix. Only completed turns (no
partial streams) are cached. Cache hits return as a single `ProviderEvent::Done`
stream (no streaming deltas for cached responses).

### 10.2 Cache invalidation

- TTL-based: entries older than `ttl_hours` are evicted on next `get()`.
- Manual: `grey cache clear` CLI command.
- Size-based: when `max_entries` exceeded, oldest entries evicted (LRU).

## 11. Usage tracking (usage.rs)

```rust
pub struct UsageTracker {
    sessions: Mutex<HashMap<String, SessionUsage>>,
    cost_table: HashMap<ProviderModelRef, CostRate>,
}

pub struct SessionUsage {
    pub session_id: String,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost_usd: f64,
    pub turns: Vec<TurnUsage>,
}

pub struct TurnUsage {
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub cached: bool,
    pub timestamp: i64,
}

pub struct CostRate {
    pub input_per_1m: f64,
    pub output_per_1m: f64,
}

impl UsageTracker {
    pub fn record(&self, session_id: &str, turn: TurnUsage);
    pub fn format_panel(&self, session_id: &str) -> String;
}
```

In-memory for CLI process lifetime. Also persisted to SQLite session database
(new `usage_json` column) so it survives across CLI invocations.
`grey usage show <session_id>` reads from the database.

## 12. Gemini adapter (gemini.rs)

```rust
pub struct GeminiProvider {
    base_url: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

#[async_trait]
impl Provider for GeminiProvider {
    fn id(&self) -> &str { "gemini" }
    async fn stream_chat<'a>(
        &'a self,
        request: &'a ChatRequest,
    ) -> Result<BoxStream<'a, ProviderEvent>>;
}
```

Gemini uses a different protocol from OpenAI/Anthropic:
- Endpoint: `POST {base_url}/models/{model}:streamGenerateContent`
- Auth: `?key={api_key}` query param (not Bearer header)
- Request body: `contents` array with `role`/`parts`, `systemInstruction` field
- Streaming: Server-Sent Events with `candidates[].content.parts[].text`
- Tool calls: `functionCall` / `functionResponse` parts
- Usage: `usageMetadata` in final chunk

The adapter converts Grey's normalized `ChatMessage`/`ToolCall` to/from
Gemini's format, reusing the existing `SseDecoder` for stream framing.

## 13. CLI integration

### 13.1 New CLI flags

```text
--task <KIND>      Task type for routing: planning|coding|fast|default
--no-cache         Disable request cache for this invocation
--no-fallback      Disable failover (use primary provider only)
```

### 13.2 New subcommands

```text
grey providers list                          List configured providers + models
grey providers show <ID>                     Show one provider's config (masked)
grey cache clear                             Clear all cached responses
grey cache stats                             Show cache hit/miss stats
grey usage show <SESSION_ID>                Show token usage + cost for a session
grey usage summary                           Show aggregate usage across sessions
```

### 13.3 Agent loop changes

`run_headless()` and `run_tui()` now:
1. Build `ProviderRouter` from config instead of calling `build_provider()`.
2. Resolve provider+model via `router.resolve(task)` or `router.resolve_explicit()`.
3. Pass `FallbackChain` to the agent loop.
4. Check `RequestCache` before each `stream_chat()` call.
5. Record `TurnUsage` after each turn completes.
6. Persist usage to the session database on save.

### 13.4 JSON output extension

`HeadlessOutput` gains a `usage` object with `input_tokens`, `output_tokens`,
`cost_usd`, `cached` (bool), and `provider`/`model` used.

## 14. Testing strategy

### 14.1 Unit tests (per module)

- **config.rs**: parse `[[providers]]` table, legacy migration, env var override.
- **token.rs**: tiktoken accuracy vs known OpenAI counts, char-approx fallback.
- **context.rs**: budget enforcement, summary trigger threshold, tool-output trim.
- **summary.rs**: summarize with mock provider, failure fallback to drop.
- **cache.rs**: put/get roundtrip, TTL eviction, LRU eviction, key collision.
- **usage.rs**: cost calculation, per-session accumulation, persistence.
- **fallback.rs**: chain resolution order, cooldown, health state transitions.
- **router.rs**: task routing, explicit override, unknown provider error.
- **gemini.rs**: request/response conversion, SSE fragmentation, tool calls.

### 14.2 Integration tests (grey-cli/tests/p2.rs)

- Multi-provider config loads and routes correctly.
- Fallback: primary fails, secondary succeeds, response returned.
- Fallback: all providers fail, error surfaced.
- Cache: second identical request returns cached response.
- Cache: `--no-cache` bypasses cache.
- Usage: `grey usage show` prints token counts and cost.
- Legacy config: old `[openai]` section auto-migrates.
- `grey providers list` shows all configured providers.
- `grey --task planning` routes to the planning model.
- Gemini adapter: offline SSE test with mock server.

## 15. Acceptance criteria

### 15.1 Functional

- AC-1: `grey.toml` with 5+ `[[providers]]` entries loads; `grey providers list` shows all.
- AC-2: `grey --task planning "..."` routes to the planning model from `[[routes]]`.
- AC-3: `grey --provider X --model Y "..."` explicitly overrides routing.
- AC-4: Primary provider down → fallback provider serves the request transparently.
- AC-5: All providers down → actionable error message listing attempted providers.
- AC-6: Second identical request within TTL returns cached response (no network).
- AC-7: `grey --no-cache` always hits the provider.
- AC-8: `grey usage show <SID>` prints input/output tokens and cost.
- AC-9: Legacy `[openai]` config auto-migrates with deprecation warning.
- AC-10: Gemini provider streams responses with tool calls and usage.

### 15.2 Token budget (roadmap: ≥40% reduction on 100+ turn conversations)

- AC-11: 100-turn conversation with context manager uses <60% of unmanaged token count.
- AC-12: Tool outputs exceeding `tool_output_budget` are truncated with audit event.
- AC-13: History exceeding `summary_threshold` triggers summary; summary is cached.
- AC-14: `ContextAudit` is emitted for every trim/summary/truncate action.

### 15.3 Quality gate (release gate from P1)

- AC-15: `cargo fmt --all -- --check` clean.
- AC-16: `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- AC-17: `cargo test --workspace --all-features` all pass (P1 tests preserved).
- AC-18: `cargo build --workspace --release --locked` succeeds.
- AC-19: `grey --no-save "hello"` smoke test works with mock provider.
- AC-20: No existing P1 test regresses.

## 16. Migration plan

### 16.1 Config migration

`config::load()` detects legacy format by checking for `[openai]`/`[anthropic]`
top-level keys without a matching `[[providers]]` entry. If detected:
1. Auto-migrate to `[[providers]]` entries in memory (id="openai"/"anthropic").
2. Print deprecation warning to stderr.
3. `grey config init --force` writes the new format.
4. `grey config migrate` subcommand explicitly rewrites the file in place.

### 16.2 Session database migration

Add `usage_json TEXT` column to `sessions` table (schema version 2). Existing
sessions get `NULL` usage (treated as zero on read). Migration is automatic on
first open with the new binary.

### 16.3 API stability

- `Provider` trait: unchanged (P1 adapters work without modification).
- `ToolExecutor` trait: unchanged.
- `Agent` struct: constructor gains `router` and `cache` params; old callers
  use `Agent::new_legacy()` which wraps a single provider in a no-op router.
- `AgentEvent`: gains `ProviderSwitched { from, to, reason }` variant.
- `ChatRequest`/`ChatMessage`: unchanged.

## 17. New dependencies

| Crate | Version | Purpose |
|---|---|---|
| `tiktoken-rs` | 0.6 | BPE token counting for OpenAI models |
| `sha2` | 0.10 | SHA-256 cache keys |
| `hex` | 0.4 | Hex encoding of hashes |

`rusqlite`, `reqwest`, `tokio`, `serde`, `async-trait`, `anyhow` already in the
workspace dependency tree. No new transitive dependencies that increase binary
size significantly (tiktoken-rs includes BPE merge tables, ~1.5MB).

## 18. ADR-002 (to be written)

A new ADR will document:
- Decision: dynamic `[[providers]]` table over hardcoded provider slots.
- Decision: unified `FallbackChain` for provider + model level failover.
- Decision: tiktoken-rs for OpenAI, char-approx for others.
- Decision: SQLite for request cache (not in-memory LRU).
- Decision: summary engine uses the active provider (not a fixed model).
