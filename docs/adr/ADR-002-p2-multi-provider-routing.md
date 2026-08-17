# ADR-002: P2 Multi-Provider Routing, Fallback, Context Budget, Cache, and Usage

- Status: Accepted
- Date: 2026-08-17

## Context

P2 requires: multi-provider routing by task, deterministic provider/model fallback,
provider-level health-based cooldown, per-provider/model request cache, and
cross-session usage persistence/cost tracking.
The design must keep `grey-core` independent from concrete provider vendors while still allowing end-to-end integration in the CLI.

## Decision

1. `grey-provider` owns runtime-provider orchestration.

   - `ProviderRouter` resolves route/task to primary `provider_id + model`.
   - `ProviderRouter::stream_chat` owns provider/model candidate iteration and
     executes fallback attempts in order.
   - `ProviderRouter` emits no user-visible behavior beyond provider selection and
     candidate streams; it only depends on `grey-core` traits/types.

2. `FallbackChain` remains in `grey-provider`.

   - Supports provider-level and model-level fallback.
   - Tracks cooldown and health transitions on success/failure.
   - Only healthy candidates are returned by `healthy_refs`.

3. `grey-core` owns conversation memory and invocation policy.

   - `Agent` stores a `fallback_chain` candidate list but does not hardcode
     provider-specific behavior.
   - `Agent` attempts one provider attempt then follows the candidate list on
     stream-level failure (if no visible output occurred).
   - Visible output (any delta or tool call) freezes provider switching to avoid
     user-visible partial responses from two providers in one turn.

4. `ContextManager::prepare` changed to async.

   - Enables on-demand summarization after heavy context growth without blocking
     the caller synchronously.
   - Keeps budgets for system/history/tool output/input; emits `ContextAudit`
     with retained tokens, dropped messages, and truncation counts.

5. Cache scope and lifecycle are provider-isolated.

   - Cache key includes provider-model request identity and request fingerprint.
   - TTL (`ttl_hours`) and LRU size (`max_entries`) are config-driven.
   - Expired entries are removed at load/query boundaries; counters (`hits`,
     `misses`) are process-local and not persisted.

6. Usage is persisted at the session level.

   - `Usage` is stored in `sessions` table as JSON (`usage_json`) and included in
     `usage show / usage summary`.
   - Default cost model keys are provider/model names such as
     `provider/model` to match P2 `ProviderModelRef`.

7. Configuration and environment merging stays deterministic.

   - Precedence remains: built-in defaults < TOML < `GREY_*` env vars < CLI flags.
   - `GREY_PROVIDER_<ID>_<FIELD>` overlays provider entries without touching global
     legacy keys.

## Consequences

- Provider add/change work is now mostly file-editable and can avoid runtime code
  changes.
- CLI owns both router construction and final Provider chain injection to Agent.
- Fallback decisions are observable through `ProviderSwitched` events and cache
  behavior through CLI cache stats.
- P2 now has deterministic cross-session persistence for usage/cost visibility.

