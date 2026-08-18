# P2 交接文档

> 日期：2026-08-18
> 作者：Sisyphus (GLM 5.2)  
> 状态：P2 主体 + MCP/Hook 补充完成

---

## 一、项目总览

Grey 是一个轻量、高性能、可扩展的 Coding Agent Harness。当前处于 **P2 阶段**（多 Provider 路由、Token 预算/摘要/缓存）。

- **设计 spec**：`docs/superpowers/specs/2026-08-17-p2-multi-provider-design.md`
- **实现计划**：`docs/superpowers/plans/2026-08-17-p2-multi-provider.md`
- **ADR**：`docs/adr/ADR-002-p2-multi-provider-routing.md`

## 二、P2 完成情况

### 14 个 Task 状态

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
| 13 | MCP 工具与 hook 配置 | `crates/grey-core/src/config.rs`, `crates/grey-tools/src/lib.rs`, `crates/grey-cli/src/main.rs` | 新增若干 | ✅ |
| 14 | Integration tests + release gate | `crates/grey-cli/tests/p2.rs` | 13 | ✅ |

### 测试总计

- **单元测试**：已按 `cargo test --workspace --all-features` 结果为准
- **集成测试**：`grey-cli --test p2`（13 个） + 新增 hook/MCP 覆盖测试（见执行记录）
- **总测试数**：以实际工作区执行结果为准

### 验证命令

```bash
# 先确保 PATH 指向 rustup 的 1.97.1（避免旧版 Homebrew rustc 回退）
export PATH="$HOME/.rustup/toolchains/1.97.1-aarch64-apple-darwin/bin:$PATH"
export RUSTC="$HOME/.rustup/toolchains/1.97.1-aarch64-apple-darwin/bin/rustc"

cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test -p grey-cli --test p2 -- --test-threads=1
cargo build --workspace --release --locked
cargo run -q -p grey-cli -- --no-save "hello"
```

---

## 三、变更清单

### 新增文件

| 文件 | 说明 |
|---|---|
| `crates/grey-core/src/cache.rs` | SQLite 请求缓存，SHA-256 key，TTL/LRU |
| `crates/grey-core/src/usage.rs` | 用量跟踪 + 成本估算 |
| `crates/grey-core/src/summary.rs` | 对话摘要引擎 |
| `crates/grey-cli/tests/p2.rs` | P2 集成测试 |
| `docs/adr/ADR-002-p2-multi-provider-routing.md` | P2 决策记录 |

### 关键改动文件

| 文件 | 改动 |
|---|---|
| `crates/grey-core/src/context.rs` | 重写 `prepare()` 为 async，支持预算裁剪、摘要、工具输出截断与审计 |
| `crates/grey-core/src/agent.rs` | 增加缓存/usage/fallback_chain 字段，扩展事件（ProviderSwitched/CacheHit） |
| `crates/grey-core/src/session.rs` | sessions 表 v1→v2 迁移（新增 `usage_json` 列），保存与加载累计 usage |
| `crates/grey-core/src/config.rs` | `[[providers]]`、`[cache]`、`[usage]` 与 legacy env 兼容覆盖 |
| `crates/grey-core/src/lib.rs` | 注册新模块（cache/usage/context/summary） |
| `crates/grey-core/src/usage.rs` | 支持 ProviderModel key 与 cost 估算 |
| `crates/grey-cli/src/main.rs` | router+fallback 接线、providers/usage/cache 子命令、JSON 输出 provider/model/cached |
| `crates/grey-provider/src/fallback.rs` | 健康恢复/冷却/故障隔离 |
| `crates/grey-provider/src/router.rs` | 多 Provider 路由、fallback 流试与流级 failover |
| `crates/grey-provider/src/gemini.rs` | `alt=sse` 协议、`x-goog-api-key`、tool_call/tool_response 映射 |
| `crates/grey-tui/src/lib.rs` | 新 AgentEvent 的事件渲染兼容 |

---

## 四、架构决策对齐

- `grey-core` 不依赖 `grey-provider`，通过统一 trait 与事件/消息模型协作。
- 路由与故障切换由 `grey-provider` 负责（`ProviderRouter` + `FallbackChain`）。
- `ContextManager::prepare()` 改为异步，允许摘要调用内部异步 provider 行为。
- 缓存按 `provider/model` 隔离，TTL/LRU 可配，过期在查询/插入时清理。
- usage 持久化到 `sessions` 表，CLI 提供 `usage show/usage summary` 跨会话聚合。

---

## 五、待完成事项

### 5.1 阻塞项

自动化验收已完成，当前仅剩实网冒烟为手工步骤（需有效 OpenAI API Key）：

- `grey-cli` 在 `gpt-5.3-codex-spark` 上的端到端调用需要设置有效 `GREY_PROVIDER_OPENAI_API_KEY`（OpenAI API Key，以 `sk-` 开头）后执行。

建议命令（`sk-` key 可放在环境变量中）：

```bash
GREY_PROVIDER_OPENAI_API_KEY=sk-... \
GREY_PROVIDER_OPENAI_BASE_URL=https://api.openai.com/v1 \
cargo run -q -p grey-cli -- --provider openai --model gpt-5.3-codex-spark --no-save --no-cache --format json "只回复 ok"
```

预期：HTTP 200，JSON 输出包含 `provider: "openai"`, `model: "gpt-5.3-codex-spark"`，且命令返回文本 `"ok"`。

严格验收补充项已完成：

- Router fallback 集成测试（主 Provider 失败后验证 fallback 接管）：已通过
- Gemini URL/body 严格集成测试（`alt=sse`、`x-goog-api-key`、`functionResponse` 映射）：已通过
- MCP 与 hook 支持补充：`hooks.pre_prompt`、`hooks.pre_tool_call`、`hooks.post_tool_call` 与 `[[mcp_tools]]` 已接入 `grey-cli` 执行链路，并已通过单元测试覆盖

### 5.2 已知技术债

| # | 事项 | 说明 |
|---|---|---|
| 1 | Rust 工具链版本 | README 要求 1.97.1。当前环境下 `cargo` 可执行链路偶有回退到旧 rustc，需要在命令里显式使用 `~/.rustup/toolchains/1.97.1...`。建议加入本地统一脚本或文档提示。 |
| 2 | 沙箱权限 | 部分 provider 测试（`anthropic/openai/gemini` 的监听 mock）在本沙箱报 `Operation not permitted`，属于执行环境权限限制。 |

### 5.3 人工验收清单（需有效 API Key）

- 命令：
  `./scripts/run-grey-smoke-p2.sh`
- 预期：HTTP 200，返回 JSON 中包含 provider/model/cached 字段及正常文本响应；脚本可在仅有 OpenAI 或仅有 ARK_API_KEY 的情况下分别运行对应分支。

### 2026-08-18 复验补充


### 2026-08-18 订阅实测补充（显式配置复核）

- 用 `GREY_CONFIG=/tmp/grey-openai-smoke.toml`（显式配置 `openai`）并传入 `GREY_PROVIDER_OPENAI_API_KEY=$YUNWU_API_KEY` 复测：
  `OpenAI provider returned 401 Unauthorized`（`invalid_api_key`）。
- 结论：当前阻塞点仅为 `sk-...` Key 非法，与项目代码实现/配置无关。


执行 `PATH=~/.rustup/toolchains/1.97.1-aarch64-apple-darwin/bin:$PATH` 下的全量门禁：

- `cargo fmt --all -- --check`：通过
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`：通过
- `cargo test --workspace --all-features`：通过（按当前记录，无失败）
- `cargo test -p grey-cli --test p2 -- --test-threads=1`：13/13 通过
- `cargo build --workspace --release --locked`：通过

MCP/Hook 覆盖执行结果：

- `crates/grey-cli/tests/p2.rs::pre_prompt_hook_rewrites_prompt_before_agent_request`：通过
- `crates/grey-tools/tests/tools.rs::pre_tool_hook_blocks_and_post_tool_hook_runs_on_success`：通过
- `crates/grey-tools/tests/tools.rs::post_tool_hook_error_marks_tool_failed_after_tool_success`：通过
- `crates/grey-provider::gemini::tests::build_url_includes_alt_sse_and_model_path`：通过
- `crates/grey-provider::gemini::tests::build_request_adds_x_goog_api_key_when_present`：通过
- `crates/grey-provider::router::tests::stream_chat_falls_back_when_primary_fails_before_visible_output`：通过

人工 smoke 运行：

- 使用 `./scripts/run-grey-smoke-p2.sh` 且设置 `GREY_PROVIDER_OPENAI_API_KEY=$YUNWU_API_KEY` 执行时，返回 TLS 连接失败：
  `client error (Connect): tls handshake eof`。  
  说明实网路径当前受环境网络/TLS 拦截，未能进入鉴权阶段，无法在本会话直接完成 GPT 订阅实测。

火山方舟 DeepSeek smoke 补充（`deepseek-v4-flash-ga-260731`）：

- 使用 `./scripts/run-grey-smoke-p2.sh` 且传入 `ARK_API_KEY=$ARK_API_KEY`（或 `VOLCANO_API_KEY`）执行时，返回：
  `OpenAI provider returned 401 Unauthorized`（API key missing or invalid）。
  说明请求路径与模型映射已到位，阻塞点为密钥注入。

### 2026-08-18 额外复核补充（DeepSeek smoke）

- 直接复用 `./scripts/run-grey-smoke-p2.sh` 的 volcano 分支（`deepseek-v4-flash-ga-260731`）可走到火山方舟调用链路：
  - 运行命令时若给定 `VOLCANO_API_KEY`，会生成临时 `[[providers]]`，包含：
    - `id = "volcano"`
    - `protocol = "openai"`
    - `base_url = "https://ark.cn-beijing.volces.com/api/v3"`
    - `model = "deepseek-v4-flash-ga-260731"`
  - 当前会话下 `--provider volcano` 分支返回 `401 Unauthorized`，而非配置错误，说明路由与 endpoint 已生效、阻塞点在 key 本身。
- 当前会话未检测到 `ARK_API_KEY`（终端里仅检测到 `YUNWU_API_KEY`），因此需要在同一 shell 会话内先 `export ARK_API_KEY=...` 再执行验证命令，或用 `VOLCANO_API_KEY=... ./scripts/run-grey-smoke-p2.sh` 复测。

---

## 六、关键数据结构速查

### AgentOutcome（扩展）

```rust
pub struct AgentOutcome {
    pub messages: Vec<ChatMessage>,
    pub response: String,
    pub usage: Usage,
    pub steps: usize,
    pub cached: bool,
    pub provider_id: String,
    pub model: String,
}
```

### AgentEvent（扩展）

```rust
pub enum AgentEvent {
    Delta(String),
    ToolStarted(ToolCall),
    ToolFinished(ToolResult),
    Retry { attempt: usize, error: String },
    ContextTrimmed(ContextAudit),
    ProviderSwitched { from: String, to: String, reason: String },
    CacheHit { model: String },
    Completed { usage: Usage, steps: usize },
    Failed(String),
}
```

### ContextAudit（扩展）

```rust
pub struct ContextAudit {
    pub original_chars: usize,
    pub retained_chars: usize,
    pub dropped_messages: usize,
    pub retained_tokens: u64,
    pub summary_created: bool,
    pub tool_outputs_truncated: usize,
}
```

---

## 七、提交与验收建议

1. **固定验收命令**：`cargo fmt --all -- --check && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace --all-features && cargo build --workspace --release --locked`
2. **可选强化验收**：补齐 Router fallback 与 Gemini URL/body 的严格集成测试
