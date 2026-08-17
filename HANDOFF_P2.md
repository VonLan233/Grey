# P2 交接文档

> 日期：2026-08-17  
> 作者：Sisyphus (GLM 5.2)  
> 状态：P2 主体功能完成，1 个集成测试需要串行运行，README 待更新

---

## 一、项目总览

Grey 是一个轻量、高性能、可扩展的 Coding Agent Harness。当前处于 **P2 阶段**（多 Provider 路由、Token 预算/摘要/缓存）。

- **设计 spec**：`docs/superpowers/specs/2026-08-17-p2-multi-provider-design.md`（606 行，18 节）
- **实现计划**：`docs/superpowers/plans/2026-08-17-p2-multi-provider.md`（870 行，13 tasks）
- **Git 基线**：`3a22257` (P0+P1) → `12ca10b` (P2 最新提交)

---

## 二、P2 完成情况

### 13 个 Task 状态

| Task | 内容 | 文件 | 测试数 | 状态 |
|---|---|---|---|---|
| 1 | 依赖：tiktoken-rs, sha2, hex | `Cargo.toml` | — | ✅ |
| 2 | Token counting | `crates/grey-core/src/token.rs` | 5 | ✅ |
| 3 | 动态 `[[providers]]` config + legacy migration | `crates/grey-core/src/config.rs` | 13 | ✅ |
| 4 | FallbackChain (provider+model 级 failover) | `crates/grey-provider/src/fallback.rs` | 8 | ✅ |
| 5 | ProviderRouter (task routing + failover) | `crates/grey-provider/src/router.rs` | 8 | ✅ |
| 6 | Gemini adapter (独立协议) | `crates/grey-provider/src/gemini.rs` | 6 | ✅ |
| 7 | SQLite request prefix cache | `crates/grey-core/src/cache.rs` | 8 | ✅ |
| 8 | Usage tracking + cost estimation + session schema v2 | `crates/grey-core/src/usage.rs`, `crates/grey-core/src/session.rs` | 12+2 | ✅ |
| 9 | Rolling conversation summary engine | `crates/grey-core/src/summary.rs` | 10 | ✅ |
| 10 | Context manager extension (token budget + summary + tool trim) | `crates/grey-core/src/context.rs` | 10 | ✅ |
| 11 | Agent loop integration (router + cache + usage) | `crates/grey-core/src/agent.rs` | 7 | ✅ |
| 12 | CLI integration (--task, providers, cache, usage subcommands) | `crates/grey-cli/src/main.rs` | 0 (集成测试在 Task 13) | ✅ |
| 13 | Integration tests + release gate | `crates/grey-cli/tests/p2.rs` | 11 | ✅ (部分) |

### 测试总计

- **单元测试**：144 个（全部通过）
- **集成测试**：11 个（需要 `--test-threads=1` 串行运行）
- **总测试数**：155 个

### 验证命令

```bash
# 必须使用 rustup 的 1.97.1，而非 Homebrew 的 1.86.0
export PATH="$HOME/.cargo/bin:$PATH"

# 格式检查
cargo fmt --all -- --check

# Clippy（零警告）
cargo clippy --workspace --all-targets --all-features -- -D warnings

# 单元测试
cargo test --workspace --all-features

# 集成测试（必须串行）
cargo test -p grey-cli --test p2 -- --test-threads=1

# 发布构建
cargo build --workspace --release --locked

# 快速验证
cargo run -q -p grey-cli -- --no-save "hello"
```

---

## 三、新增文件清单

| 文件 | 说明 |
|---|---|
| `crates/grey-core/src/cache.rs` | SQLite 请求缓存，SHA-256 key，TTL/LRU |
| `crates/grey-core/src/usage.rs` | 用量跟踪 + 成本估算 |
| `crates/grey-core/src/summary.rs` | 对话摘要引擎 |
| `crates/grey-cli/tests/p2.rs` | P2 集成测试 |

### 修改的关键文件

| 文件 | 改动 |
|---|---|
| `crates/grey-core/src/context.rs` | 重写：async prepare()，token budget，summary，tool trim |
| `crates/grey-core/src/agent.rs` | 新增 cache/usage/fallback_chain 字段，AgentEvent 变体 |
| `crates/grey-core/src/session.rs` | schema v2 迁移，usage_json 列，save_usage/load_usage |
| `crates/grey-core/src/config.rs` | `[[providers]]` 表，RouteRule，TaskKind derive Default |
| `crates/grey-core/src/lib.rs` | 注册新模块 |
| `crates/grey-cli/src/main.rs` | ProviderRouter 替换 build_provider，新 CLI flags/subcommands |
| `crates/grey-tui/src/lib.rs` | 处理新 AgentEvent 变体 |
| `crates/grey-provider/src/fallback.rs` | clippy 修复 |
| `crates/grey-provider/src/router.rs` | clippy 修复 |

---

## 四、架构决策

### 4.1 crate 依赖方向

`grey-core` 不能依赖 `grey-provider`（ADR-001）。因此：

- `ProviderRouter` 和 `FallbackChain` 在 `grey-provider` 中
- `Agent` 在 `grey-core` 中，通过 `Arc<dyn Provider>` 和 `Vec<ProviderModelRef>` 抽象
- CLI 层（`grey-cli`）同时依赖两者，负责将 Router 解析结果注入 Agent

### 4.2 ContextManager::prepare() 变为 async

因为 `SummaryEngine::summarize()` 需要调用 provider（async），`prepare()` 从同步变为异步。Agent 的 `continue_messages()` 中的调用已更新为 `.await`。

### 4.3 缓存命中计数器是进程内的

`RequestCache` 的 `hits`/`misses` 计数器是 `Mutex<u64>`，不持久化到 SQLite。每个 CLI 进程从 0 开始计数。缓存条目本身（SQLite 行）是持久化的。

### 4.4 Schema 迁移

`sessions` 表从 v1 → v2 自动迁移：`ALTER TABLE sessions ADD COLUMN usage_json TEXT`。旧 session 的 usage_json 为 NULL，读取时视为零。

---

## 五、待完成事项

### 5.1 阻塞项（需要接手者处理）

| # | 事项 | 严重性 | 说明 |
|---|---|---|---|
| 1 | 集成测试并行失败 | **中** | `tests/p2.rs` 中的 `std::env::set_var("HOME", ...)` 是进程全局的，并行测试互相覆盖。当前 workaround 是 `--test-threads=1`。正确修复方案：改用 per-test 的独立环境变量传递（不使用 `std::env::set_var`），或使用 `cargo-nextest` 的进程隔离。 |
| 2 | README 未更新 | **低** | 计划 Task 13 Step 4 要求更新 README.md 的"当前状态"section，添加 `--task`、`providers`、`cache`、`usage` 快速开始说明。 |
| 3 | ADR-002 未写 | **低** | 计划要求在 Task 13 写 ADR-002 记录 P2 的架构决策（crate 依赖方向、async prepare、缓存计数器策略）。 |
| 4 | `cargo build --workspace --release --locked` 未验证 | **低** | 因 rustc 版本问题（见下方），release locked build 未跑通。 |

### 5.2 已知技术债

| # | 事项 | 说明 |
|---|---|---|
| 1 | rustc 版本冲突 | 系统 Homebrew rustc 1.86.0 无法编译部分依赖（需要 1.88+）。必须使用 `rustup` 的 1.97.1：`export PATH="$HOME/.cargo/bin:$PATH"`。建议在 README 和 CI 中明确。 |
| 2 | Agent 的 usage session_id | `Agent::session_id_for_usage()` 当前硬编码返回 `"default"`。CLI 应在构建 Agent 时传入真正的 session_id，或在 `continue_messages` 完成后用真实 session_id 调用 `UsageTracker::record`。 |
| 3 | Fallback 实际切换未实现 | Agent 有 `fallback_chain` 字段和 `ProviderSwitched` 事件，但 `stream_turn()` 中的实际 provider 切换逻辑（失败后尝试 fallback_chain 中的下一个 provider）尚未实现。当前只是将 chain 传递给了 Agent。 |
| 4 | Usage 持久化未在 CLI 主流程中接线 | `build_agent_and_session()` 创建了 `UsageTracker` 并注入 Agent，但 Agent 完成后没有调用 `store.save_usage()` 将 usage 写入 session DB。需要在 `persist_outcome()` 中增加 usage 持久化。 |

### 5.3 非 P2 范围但值得注意

- `run_spike_c()` 也已迁移到 ProviderRouter
- grey-tui 的事件处理已更新以支持新 AgentEvent 变体
- Gemini adapter 已实现但未在集成测试中覆盖

---

## 六、关键数据结构速查

### AgentOutcome (扩展后)

```rust
pub struct AgentOutcome {
    pub messages: Vec<ChatMessage>,
    pub response: String,
    pub usage: Usage,
    pub steps: usize,
    pub cached: bool,        // NEW
    pub provider_id: String, // NEW
    pub model: String,       // NEW
}
```

### AgentEvent (扩展后)

```rust
pub enum AgentEvent {
    Delta(String),
    ToolStarted(ToolCall),
    ToolFinished(ToolResult),
    Retry { attempt: usize, error: String },
    ContextTrimmed(ContextAudit),
    ProviderSwitched { from: String, to: String, reason: String }, // NEW
    CacheHit { model: String },                                      // NEW
    Completed { usage: Usage, steps: usize },
    Failed(String),
}
```

### ContextAudit (扩展后)

```rust
pub struct ContextAudit {
    pub original_chars: usize,
    pub retained_chars: usize,
    pub dropped_messages: usize,
    pub retained_tokens: u64,       // NEW
    pub summary_created: bool,       // NEW
    pub tool_outputs_truncated: usize, // NEW
}
```

### CLI 新增 flags/subcommands

```
grey --task <planning|coding|fast|default>  # Task routing
grey --no-cache                              # 禁用缓存
grey --no-fallback                           # 禁用故障切换

grey providers list                           # 列出所有 provider
grey providers show <ID>                      # 查看 provider 详情
grey cache clear                              # 清空缓存
grey cache stats                              # 缓存统计
grey usage show <SESSION_ID>                 # 查看 session 用量
grey usage summary                            # 汇总用量
```

---

## 七、Git 提交历史

```
12ca10b P2 Task 13 (partial): integration tests for CLI subcommands
5381b6f P2 Task 12: CLI with --task, providers, cache, usage subcommands
b02b830 P2 Task 11: agent loop with router, cache, usage integration
97c0407 P2 Task 10: context manager with token budget, summary, tool trim
2af812e P2 Task 9: rolling conversation summary engine
d71c96d P2 Task 8: usage tracking + cost estimation + session schema v2
45192eb P2 Task 7: SQLite request prefix cache
1650eb2 P2 Task 6: Gemini provider adapter
4f01c0e P2 Task 5: ProviderRouter with task routing and failover
7ed5e47 P2 Task 4: FallbackChain with provider+model level failover
5526f38 P2 Task 3: dynamic [[providers]] config with legacy migration
6cde82a P2 Task 2: token counting with tiktoken-rs + char-approx fallback
01878cb P2 Task 1: add tiktoken-rs, sha2, hex workspace deps
71bade5 P2 plan: add review findings + fixes
40e6410 P2 implementation plan: 13 tasks, TDD, spec-mapped
2426d1c P2 design spec: multi-provider, token budget, caching
3a22257 P0+P1: initial baseline — harness MVP with bounded agent loop, tools, sessions, TUI
```

---

## 八、接手建议

1. **先跑测试**：`export PATH="$HOME/.cargo/bin:$PATH" && cargo test --workspace --all-features && cargo test -p grey-cli --test p2 -- --test-threads=1`
2. **优先修复集成测试并行问题**（第五节 5.1 #1）
3. **完成 README 更新**（第五节 5.1 #2）
4. **接线 usage 持久化**（第五节 5.2 #4）—— 在 `persist_outcome()` 中调用 `tracker.persist_json()` + `store.save_usage()`
5. **实现 fallback 实际切换**（第五节 5.2 #3）—— 在 `stream_turn()` 失败后尝试 chain 中的下一个 provider
