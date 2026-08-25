<p align="center">
  <img src="docs/assets/grey-logo-transparent.png" alt="Grey" height="255" />
</p>

<h1 align="center">Grey</h1>

<p align="center">
  <em>一个轻量、高性能、可扩展的 Coding Agent Harness<br/>
  默认极简，一切按需扩展。快是特性，省是特性，顺是特性。</em>
</p>

<p align="center">
  <a href="https://github.com/VonLan233/Grey/actions/workflows/ci.yml"><img src="https://github.com/VonLan233/Grey/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
  <img src="https://img.shields.io/badge/version-v0.1.1-blue" alt="Version" />
  <img src="https://img.shields.io/badge/license-MIT-green" alt="License" />
  <img src="https://img.shields.io/badge/rust-1.97.1-orange" alt="Rust" />
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-lightgrey" alt="Platform" />
</p>

<div align="center">
  <a href="#快速开始">快速开始</a>
  <span>&nbsp;&nbsp;•&nbsp;&nbsp;</span>
  <a href="https://github.com/VonLan233/Grey/releases">Releases</a>
  <span>&nbsp;&nbsp;•&nbsp;&nbsp;</span>
  <a href="docs/阶段性开发文档.md">路线图</a>
  <span>&nbsp;&nbsp;•&nbsp;&nbsp;</span>
  <a href="LICENSE">License</a>
  <br />
</div>

Grey 是一个用 Rust 写的 Coding Agent Harness：同一套 Core 同时服务单发 CLI 与交互式 TUI，
模型可以流式回答、调用工作区工具、接收结果并继续推理，会话持久化到 SQLite。
它围绕三个原则构建：

- **快** — 启动毫秒级、渲染帧率有硬性门禁、工具调用低延迟。
- **省** — 分区 Token 预算、滚动摘要、LRU 请求缓存、usage 全程记账。
- **顺** — 多 Provider 自动路由与故障切换、MCP 生态、Hook 全链路、可编排的多 Agent。

## 目录

- [特性](#特性)
- [快速开始](#快速开始)
- [CLI 速览](#cli-速览)
- [配置](#配置)
- [工具与安全](#工具与安全)
- [Hook 与插件](#hook-与插件)
- [MCP](#mcp)
- [会话](#会话)
- [架构](#架构)
- [开发与发布](#开发与发布)
- [路线图](#路线图)
- [许可证](#许可证)

## 特性

**核心对话**
- 有界 Agent 工具循环与统一事件流，支持流式输出、工具调用聚合与错误传播
- OpenAI Chat Completions 兼容协议、Anthropic Messages API、离线 Mock Provider
- 可靠 SSE 跨分片解析、流式工具调用聚合和错误传播

**省 Token 与成本（v0.1.1 实测）**
- 空载 `hello`：`1387→1325` input（-4.5%），黑洞页 `24264→10411` input（**-57%**），比同模型 Pi 少 13% input / 70% output；工具描述精简 + `include_usage` 默认开，`deepseek-v4-flash` 现正确计费
- system / history / tool / input 分区预算、工具输出截断、滚动摘要与可审计裁剪事件
- SQLite 请求缓存（TTL / LRU / provider 隔离）与 `--no-cache` 控制
- 每会话 token/cost usage 记录，跨调用累积，`usage show/summary` 查询（`volcano` 已补 `cost_per_1m`）

**多 Provider 与路由**
- 动态 `[[providers]]` 注册表、planning / coding / fast / default 路由、CLI 覆盖
- Provider/model fallback：只在尚未产生可见输出时切换，带失败冷却与恢复
- `GREY_PROVIDER_<ID>_<FIELD>` 环境变量覆盖；未知/重复/无效引用直接报错，不静默降级

**工具与安全**
- 内建 `read_file` / `edit_file` / `bash` / `glob` / `grep`
- 工作区路径隔离（拒绝 `..` / 绝对路径 / 符号链接逃逸）、精确单次替换、原子写入、写入/执行审批
- LSP 语义视图注入：`lsp_*` 结果按路径写入紧凑上下文摘要并按 tool/path 去重

**扩展性**
- MCP 生态：`[[mcp_servers]]` stdio 协议客户端（`id__tool` 注册）+ `[[mcp_tools]]` 兼容层
- Hook 全链路：`pre_prompt`、`pre_message_send`、`pre_tool_call` / `post_tool_call`、`permission_decision`、`completion`、`session_start` / `session_end`
- 插件系统：`grey plugins` 管理 tool / hook 插件（含 sealed WASM 插件运行时）
- 多 Agent 编排：`grey orchestrate` 并行子 agent + `grey loop` / `grey goal`

**界面**
- ratatui 对话界面：流式状态、主题（`slate` / `grey_storm` 等）、Markdown 渲染（无边框文档流，分隔线 + footer 两端布局）
- Pi 式 header：`Grey vX.Y.Z` + 快捷键提示（`Enter 发送 · Shift+Enter 换行 · / 命令 · \k 帮助`），随消息上滚，`/clear` 后重现
- 输入区自适应高度（最大 40% 终端高度，超出内部滚动）；`Shift+Enter` / `Alt+Enter` 换行（行尾 `\` + Enter 兜底）、Up/Down 行内移动
- 斜杠命令：`/help` `/clear` `/quit` `/exit` `/model <name>` `/status` `/usage` `/models`；`/` 触发补全浮窗（`↑/↓` 或 `Ctrl+N/P` 导航，`Tab/Enter` 采纳，`Esc` 关闭；`/model ` 后空格触发模型名二级补全）
- 消息区滚动：滚轮 / `PageUp` / `PageDown`，自动跟随；补全浮窗与输入溢出时按鼠标位置分流滚轮（popup/input/会话）
- 底部 footer 两端布局：左 `↑in ↓out · task`，右 `(provider) model (branch)`，超宽截断；仅错误时红色 `ERR`
- 长任务完成提醒（终端鸣铃 / 强鸣铃 / 系统通知）

## 快速开始

> 需要 Rust 1.97.1（仓库 `rust-toolchain.toml` 已固定版本）。仅 LSP Spike 需要额外安装 `rust-analyzer`。

**一条命令运行离线 Demo**（不访问网络、不保存会话）：

```bash
cargo run -p grey-cli -- --no-save "你好 Grey"
```

**启动交互式 TUI**：

```bash
cargo run -p grey-cli
```

**脚本化 JSON 输出**：

```bash
cargo run -q -p grey-cli -- --no-save --format json "概述这个项目"
```

**安装本地二进制**：

```bash
cargo install --path crates/grey-cli
grey --help
```

也可以直接从 [GitHub Releases](https://github.com/VonLan233/Grey/releases) 下载发布包：

```bash
tar -xzf grey-0.1.1-darwin-aarch64.tar.gz
cp grey-0.1.1-darwin-aarch64/bin/grey /usr/local/bin/grey
```

## CLI 速览

```text
grey [OPTIONS] <COMMAND>

Commands:
  config       配置管理（init / show / path）
  providers    Provider 与模型管理（list / show）
  sessions     会话管理（list / show）
  plugins      插件管理（list / show / find / add / remove / enable / disable）
  hooks        Hook 插件管理
  skills       本地 SKILL.md 管理
  mcp          MCP 服务器管理（list / show / find / add / remove）
  cache        请求缓存（stats / clear）
  usage        用量与成本（show / summary）
  auth         ChatGPT OAuth 登录（login / status / logout）
  tui          TUI 偏好（theme / layout / keys）
  orchestrate  多 Agent 编排
  loop         固定轮次迭代
  goal         目标驱动迭代
  spike-a/b/c  P0 技术验证 Spike
```

常用全局参数：`--provider`、`--model`、`--workspace`、`--session`、`--continue`、`--no-save`、`--no-cache`、`--format json`、`--auto-approve`、`--read-only`。

## 配置

创建默认配置：

```bash
grey config init
grey config show
```

默认路径是 `~/.config/grey/grey.toml`，可用 `GREY_CONFIG` 指定。配置优先级固定为：

```text
内置默认值 < TOML < GREY_* 环境变量 < CLI 参数
```

### 动态 Provider 示例

```toml
default_provider = "local"
default_model = "qwen2.5:7b"

[[providers]]
id = "local"
protocol = "openai"
base_url = "http://localhost:11434/v1"
models = [{ id = "qwen2.5:7b", name = "Qwen 2.5 7B" }]

[[providers]]
id = "offline"
protocol = "mock"

[[providers]]
id = "volcano"
protocol = "openai"
base_url = "https://ark.cn-beijing.volces.com/api/v3"
models = [{ id = "deepseek-v4-flash-ga-260731", name = "DeepSeek V4 Flash" }]

[[routes]]
match = "coding"
provider = "local"
model = "qwen2.5:7b"

[fallback]
providers = ["local", "offline"]

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
cost_per_1m_input = { "local/qwen2.5:7b" = 0.15 }
cost_per_1m_output = { "local/qwen2.5:7b" = 0.60 }

[hooks]
pre_prompt = ["cat"]
pre_message_send = []
session_start = []
session_end = []
permission_decision = []
completion = []

[[mcp_tools]]
name = "ls"
command = "ls"
args = [".", "-la"]

[[mcp_servers]]
id = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
# transport 固定为 "stdio"；env 可注入环境变量（支持 ${VAR} 展开），timeout_ms 可选

[tui]
# layout.input_lines 已废弃（v0.1.1 起输入区自适应高度，最大 40% 终端高度，保留仅为兼容）
theme = { preset = "slate", overrides = { border = "#1f2937", accent = "#60a5fa", prompt = "yellow", status_fg = "black", status_bg = "#60a5fa" } }
completion = { enabled = true, long_running_steps = 4, long_running_seconds = 120, bell = true, strong_bell = true, notify = true, persistent = true }
keys = { leader = "\\", help = "k", quit = "ctrl-c", clear = "ctrl-l", scroll_up = "pageup", scroll_down = "pagedown" }
```

底部 footer 两端布局：左侧 `↑in ↓out · task`（`1.5k` 缩写），右侧 `(provider) model (branch)`，超宽右侧截断为 `…`；仅错误时显示红色 `ERR`。快捷键帮助由 `<leader>k` 打开（默认 leader 为 `\`），输入区以 `─` 分隔线与会话区分隔。

### 旧版配置兼容

旧版配置仍兼容，但会输出迁移提示：

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

### 环境变量覆盖

```bash
GREY_PROVIDER_OPENAI_API_KEY=sk-xxx GREY_PROVIDER_OPENAI_BASE_URL=https://api.openai.com/v1 grey --provider openai --model gpt-5.3-codex-spark "Hello"
GREY_OPENAI_BASE_URL=http://localhost:11434/v1 grey --provider openai "你好"
ARK_API_KEY=ark-xxx GREY_PROVIDER_VOLCANO_BASE_URL=https://ark.cn-beijing.volces.com/api/v3 grey --provider volcano --model deepseek-v4-flash-ga-260731 --no-cache --no-save "请只回复 ok"
grey --task coding --no-cache "修复测试失败"
```

- 每个 Provider 可用 `GREY_PROVIDER_<ID>_<FIELD>` 覆盖（如 `GREY_PROVIDER_LOCAL_BASE_URL`）。
- `volcano` 分支支持 `ARK_API_KEY`（或 `VOLCANO_API_KEY`）兜底；旧 `GREY_OPENAI_*` / `GREY_ANTHROPIC_*` 变量继续有效（legacy 兼容）。
- 实网验证脚本 `scripts/run-grey-smoke-p2.sh` 优先执行 OpenAI `gpt-5.3-codex-spark` 分支，有 `VOLCANO/ARK` key 时补充执行火山方舟验证；OpenAI key 用 `sk-` 前缀（非 ChatGPT 会话 token）。
- 未知 Provider、重复 Provider ID 和无效 fallback 引用会直接报错，不会静默降级为 Mock。

## 工具与安全

读取与搜索工具默认可用。单发 CLI 中 `edit_file` 与 `bash` 默认逐次询问；非交互环境和 TUI 默认拒绝副作用（避免与 raw-mode 输入竞争），TUI 需要显式传入 `--auto-approve` 才会执行写入。

```bash
# 明确自动批准写入和命令执行
grey --auto-approve "修复 bug 并运行测试"

# 强制只读分析
grey --read-only "找出 bug，但不要修改"
```

- 所有文件工具只能访问 `--workspace` 指定的规范化目录，`..`、绝对路径和符号链接逃逸会被拒绝。
- `edit_file` 只修改现有文件，且 `old_string` 必须恰好匹配一次。

## Hook 与插件

### Hook 约定

- `session_start`：会话开始时执行一次。
- `pre_message_send`：每次消息发送前执行，可改写 `prompt`（返回 JSON `{"prompt":"..."}` 或纯文本按原样替换）。
- `pre_prompt`：兼容历史行为，每次消息处理前再次触发。
- `pre_tool_call` / `post_tool_call`：工具调用前后执行；`pre_tool_call` 失败会阻断对应工具。
- `permission_decision`：权限决策钩子，副作用工具执行前给出最终批准（可返回 `{"approved":false}`）。
- `completion`：每次交互成功或失败后执行。
- `session_end`：会话结束时执行一次（TUI 与 headless）。

### 插件管理

```bash
grey plugins list
grey plugins add rewrite-hook --kind hook --command printf --arg from_hook --hook-event pre_prompt
grey plugins show rewrite-hook
grey plugins disable rewrite-hook
grey plugins enable rewrite-hook
grey plugins remove rewrite-hook
grey plugins add tool-check --kind tool --command printf --arg hello
```

`add`/`remove` 会将变更落盘到 `GREY_CONFIG`（或默认 `~/.config/grey/grey.toml`）对应的 `[[plugins]]` 配置。

## MCP

Grey 支持两类 MCP 配置，二者可共存：

- **`[[mcp_tools]]`（兼容层，保留）**：每个条目是单条 shell 命令工具，不执行 MCP JSON-RPC 协议，直接注册为同名工具。
- **`[[mcp_servers]]`（持久化 stdio MCP 协议客户端，推荐）**：Agent 启动时自动连接，执行 `initialize` → `tools/list` → `tools/call`，发现的工具以 `id__tool` 命名注册并可用；单连接串行处理请求，超时（默认 5s，`timeout_ms` 覆盖）后对进程组先 `TERM` 再 `KILL` 回收。

**迁移说明**：若某个 `[[mcp_tools]]` 条目本身就是一个会讲 MCP stdio JSON-RPC 的服务进程，把它迁移到 `[[mcp_servers]]`（补一个稳定 `id`）即可走协议握手；纯 shell 命令工具继续留在 `[[mcp_tools]]`。`[[mcp_servers]]` 只支持 `stdio` 传输，`command` 必须是直接命令而非 URL。

**管理命令**（落盘到 `GREY_CONFIG` 的 `[[mcp_servers]]`）：

```bash
grey mcp list
grey mcp show filesystem
grey mcp find filesystem
grey mcp add filesystem --command npx --arg=-y --arg=@modelcontextprotocol/server-filesystem
grey mcp remove filesystem
```

`add` 以 `id` 为准执行 upsert（更新时保留已有 `args`/`timeout_ms`/`env`）；`env` 暂由配置文件手写，`show` 会脱敏 `args` 与常见密钥字段。

## 会话

会话默认保存在 `~/.local/share/grey/sessions.db`，测试或便携环境可用 `GREY_SESSION_DB` 覆盖。

```bash
grey "第一轮"
grey sessions list
grey sessions show <SESSION_ID>
grey --session <SESSION_ID> "继续处理"
grey --continue "继续当前工作区最近的会话"
grey --no-save "临时问题"
```

恢复会话时会校验工作区，避免在另一个目录意外执行旧上下文中的工具请求。

## 架构

Rust workspace，分层清晰，单向依赖：

| Crate | 职责 |
| --- | --- |
| `grey-core` | 语言无关契约：协议、Agent 循环、上下文预算、会话、配置、Hook 运行时 |
| `grey-provider` | Provider 适配：OpenAI / Anthropic / Mock / SSE / OAuth / 路由与 fallback |
| `grey-tools` | 内建工具 + LSP + MCP 客户端 + 插件工具，统一 `ToolExecutor` |
| `grey-tui` | ratatui 交互界面、主题、完成提醒 |
| `grey-lsp` | rust-analyzer 诊断与定义跳转集成 |
| `grey-cli` | CLI 入口：所有子命令、工具链组装、会话与 TUI 编排 |

关键架构决策记录在 [ADR](docs/adr/)：

- [ADR-001 运行时与边界](docs/adr/ADR-001-runtime-and-boundaries.md)
- [ADR-002 P2 多 Provider 路由](docs/adr/ADR-002-p2-multi-provider-routing.md)
- [ADR-003 P3 Agent 编排](docs/adr/ADR-003-p3-agent-orchestration.md)
- [ADR-004 P4 LSP 语义上下文](docs/adr/ADR-004-p4-lsp-semantic-context.md)
- [ADR-005 P5 TUI 定制](docs/adr/ADR-005-p5-tui-customization.md)
- [ADR-006 P6 Hook 生命周期](docs/adr/ADR-006-p6-hook-lifecycle.md)

完整路线图见[阶段性开发文档](docs/阶段性开发文档.md)，架构背景见[项目计划书](docs/项目计划书.md)。

## 开发与发布

### 开发

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo build --workspace --release --locked
```

> 若环境里的 cargo/clippy 偶发回退到旧 rustc，可显式使用 rustup 1.97.1：
> `~/.rustup/toolchains/1.97.1-aarch64-apple-darwin/bin/cargo-clippy --workspace --all-targets --all-features -- -D warnings`

### 发布门禁与脚本

- `scripts/test-all.sh` — 完整离线 release gate：fmt、clippy、workspace 测试、doc 测试、release 构建、性能门禁、soak。
- `scripts/run-grey-p6-perf-gates.sh` — 启动时延 / 内存 / 渲染 FPS / 大仓库扫描门禁（已接入 CI）。
- `scripts/run-grey-p8-soak.sh` — RSS 峰值与斜率、子进程水位长时 soak（`--long` 一小时）。
- `scripts/run-grey-p7-release.sh` — 构建并打包 GitHub Release 产物（tar.gz + SHA256SUMS + RELEASE_NOTES）。
- `scripts/run-grey-smoke-p2.sh` — OpenAI / Volcano 实网 smoke。

## 路线图

1. **P2** ✅ 多 Provider 路由、故障切换、Token 预算 / 摘要 / 缓存
2. **P3** ✅ 多 Agent 编排与会话化记忆
3. **P4** LSP 语义工具、文档与图片
4. **P5** ✅ 可定制布局、主题与完成提醒
5. **P6** ✅ Hook 生命周期、Loop / Goal、插件与性能门禁
6. **P7** 🚧 跨平台打包与 v1.0 发布（当前 v0.1.1）

## 许可证

[MIT](LICENSE) © Grey Team
