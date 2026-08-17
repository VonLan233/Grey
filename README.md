# Grey

> 一个轻量、高性能、可扩展的 Coding Agent Harness

默认极简，一切按需扩展。快是特性，省是特性，顺是特性。

## 当前状态

Grey 已完成 P0 技术验证，并具备 P1 的首个可用纵向闭环：同一套 Core
同时服务单发 CLI 与 TUI，模型可以流式回答、调用工作区工具、接收工具结果并继续推理，
会话可保存到 SQLite 后恢复。

当前版本是 **v0.1/P1 MVP**，不是路线图中的 v1.0。多 Agent、MCP、完整 LSP
语义工具、Token 缓存、WASM 插件、图片、桌面提醒与发布打包仍按 P2–P7 推进。

## 已实现

- 有界 Agent 工具循环与统一事件流
- OpenAI Chat Completions 兼容协议、Anthropic Messages API、离线 Mock Provider
- 可靠 SSE 跨分片解析、流式工具调用聚合和错误传播
- `read_file` / `edit_file` / `bash` / `glob` / `grep`
- 工作区路径隔离、精确单次替换、原子写入、写入/执行审批
- 保守上下文裁剪及可审计裁剪事件
- SQLite 会话保存、列出、查看、按 id 或工作区恢复
- 可脚本化 `grey "prompt"` 与 JSON 输出
- ratatui 对话界面、流式状态、滚动和终端清理
- rust-analyzer 诊断与定义跳转 Spike

完整路线图见[阶段性开发文档](docs/阶段性开发文档.md)，架构背景见[项目计划书](docs/项目计划书.md)。

## 快速开始

需要 Rust 1.97.1。LSP Spike 还需要单独安装 `rust-analyzer`；普通对话不需要它。

一条命令运行离线 Demo（不访问网络、不保存会话）：

```bash
cargo run -p grey-cli -- --no-save "你好 Grey"
```

启动交互 TUI：

```bash
cargo run -p grey-cli
```

脚本化 JSON 输出：

```bash
cargo run -q -p grey-cli -- --no-save --format json "概述这个项目"
```

安装本地二进制：

```bash
cargo install --path crates/grey-cli
grey --help
```

## Provider 配置

创建默认配置：

```bash
grey config init
grey config show
```

默认路径是 `~/.config/grey/grey.toml`，也可用 `GREY_CONFIG` 指定。配置优先级固定为：

```text
内置默认值 < TOML < GREY_* 环境变量 < CLI 参数
```

示例：

```toml
provider = "openai"
model = "grey-default"

[openai]
base_url = "http://localhost:11434/v1"
api_key = "${GREY_OPENAI_API_KEY}"
model = "qwen2.5:7b"

[anthropic]
base_url = "https://api.anthropic.com/v1"
api_key = "${GREY_ANTHROPIC_API_KEY}"
model = "claude-sonnet-4-5"
version = "2023-06-01"
max_tokens = 4096

[lsp]
rust_analyzer = "rust-analyzer"
```

常用覆盖：

```bash
grey --provider openai --model qwen2.5:7b "修复测试失败"
grey --provider anthropic "解释这个 workspace"
GREY_OPENAI_BASE_URL=http://localhost:11434/v1 grey --provider openai "你好"
```

未知 Provider 会直接报错，不会静默降级为 Mock。

## 工具安全

读取与搜索工具默认可用。单发 CLI 中，`edit_file` 与 `bash` 默认逐次询问；非交互环境
和 TUI 默认拒绝副作用（避免与 raw-mode 输入竞争），TUI 需要显式传入 `--auto-approve` 才会执行写入。

```bash
# 明确自动批准写入和命令执行
grey --auto-approve "修复 bug 并运行测试"

# 强制只读分析
grey --read-only "找出 bug，但不要修改"
```

所有文件工具只能访问 `--workspace` 指定的规范化目录，`..`、绝对路径和符号链接逃逸会被拒绝。
`edit_file` 只修改现有文件，且 `old_string` 必须恰好匹配一次。

## 会话

会话默认保存在 `~/.local/share/grey/sessions.db`，测试或便携环境可用
`GREY_SESSION_DB` 覆盖。

```bash
grey "第一轮"
grey sessions list
grey sessions show <SESSION_ID>
grey --session <SESSION_ID> "继续处理"
grey --continue "继续当前工作区最近的会话"
grey --no-save "临时问题"
```

恢复会话时会校验工作区，避免在另一个目录意外执行旧上下文中的工具请求。

## P0 技术 Spike

```bash
grey spike-a
grey spike-b crates/grey-core/src/config.rs
grey spike-c "流式测试"
```

`spike-c` 使用 Mock Provider 时会额外发出一个示例工具调用，但不会执行它；真实 Agent
入口是 `grey "prompt"`。

## 开发与验证

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo build --workspace --release --locked
```

仓库分层与关键决策记录在
[ADR-001](docs/adr/ADR-001-runtime-and-boundaries.md)，本次实现计划见
[P0+P1 Implementation Plan](docs/plans/p0-p1-harness.md)。

## 路线图（规划）

1. P2：多 Provider 路由、故障切换、Token 预算/摘要/缓存
2. P3：多 Agent 编排与 MCP
3. P4：完整 LSP 语义工具、文档与图片
4. P5：可定制布局、主题与完成提醒
5. P6：WASM 插件、Hook、Loop/Goal 和性能门禁
6. P7：跨平台打包与 v1.0 发布
