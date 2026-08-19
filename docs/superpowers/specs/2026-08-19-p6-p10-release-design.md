# Grey P6–P10 与 v1.0 发布设计

> 状态：已批准
> 日期：2026-08-19
> 范围：P6 收尾、P7 v1.0 发布、P8 内存治理、P9 TUI 视觉、P10 扩展管理、发布测试与真实 API 验证

## 1. 背景与审计结论

Grey 当前分支只正式规划到 P7。审计确认：

- P6 仅部分完成。Hook、Loop/Goal、命令型 Tool/Hook 插件已有基础实现；Provider/Theme、WASM 与性能门禁仍是未提交或不可验收的 WIP。
- P7 尚未开始。仓库没有 release workflow、跨平台安装、Homebrew、CHANGELOG、SECURITY、发布清单或 v1.0 文档。
- 当前工作树包含用户已有改动，设计与后续实现必须保存并接续这些改动，不能 reset、checkout 或覆盖无关内容。
- 当前发布门禁失败：`cargo fmt --all -- --check`、`git diff --check` 与 clippy 均发现问题；默认 Homebrew Rust 1.86.0 也不满足项目固定的 Rust 1.97.1。
- 插件 CRUD 读取实际配置后却写默认全局路径，并序列化环境变量已展开的完整配置，存在注释丢失、错误文件写入和秘密落盘风险。
- Hook 插件没有进入 `permission_decision`、`pre_tool_call`、`post_tool_call` 三条链。
- TUI、Agent event、输入与提示词使用无界 channel；TUI transcript、单轮模型响应和多个子进程输出缺少硬上限。
- 当前 `McpTools` 是一次性命令 JSON 适配器，不是 MCP initialize/list/call 会话。

本设计先修共享边界，再在同一边界上完成 P6–P10，避免为每种扩展重复实现进程、配置、热重载或内存治理。

## 2. 目标

1. 完整验收 P6：安全扩展配置、八类 Hook、Provider/Theme、版本化 WASM manifest、隔离执行、热更新与真实性能门禁。
2. 完整准备 P7：v1.0 文档、跨平台构件、安装器、Homebrew、发布清单、变更日志与可复现验证。
3. 内建 ChatGPT Plus/Pro 浏览器 OAuth，让 Grey 可使用用户的 OpenAI 订阅服务，同时继续支持 OpenAI API Key。
4. 测试和示例使用火山引擎 Coding Plan 专用端点，不误走普通 Ark 按量端点。
5. 保留 OpenCode Go 与 Zen 的官方 API 形态，包括 Responses、Chat Completions、Messages 与 Gemini-compatible 路径。
6. P8 对所有长期状态和生产者—消费者边界设置硬上限，并以长稳测试证明内存趋于平台而非随时间线性增长。
7. P9 交付简约、冷峻、克制的“灰蛊风暴”青色 TUI，支持用户主题覆盖和运行中热重载。
8. P10 让用户管理 Plugin、Skill、MCP、Hook、Theme：添加、删除、启停、显示、查找；不引入没有真实消费者的万能扩展框架。
9. 提供一键完整测试脚本：离线门禁始终可运行；live 模式按可用凭据验证火山 Coding Plan、OpenAI 订阅和 OpenCode Go/Zen。

## 3. 非目标

- 不把 OpenCode、Node 或完整 OpenAI Codex runtime 作为 Grey 的运行时依赖。
- 不实现第三方插件商城、在线排名或未经定义的远程搜索索引；`find` 只搜索已安装/已配置项。
- 不在 P6 WASM v1 开放网络、任意环境变量或文件系统权限。v1 默认没有 preopen、没有继承环境、没有网络能力。
- 不把命令型 legacy adapter 继续宣称为真实 MCP。
- 不在 Grey 中复制 OpenCode 的全部模型目录或 AI SDK；只实现 Grey 实际需要的协议。
- 不自动发布 crates.io、GitHub Release 或外部 Homebrew tap。远端发布是显式、可审计的外部动作，必须在仓库地址和凭据明确后执行。

## 4. 总体架构

```text
grey CLI/TUI
   │
   ├─ raw config editor ── toml_edit + lock + atomic replace
   │
   ├─ effective config ─── env expansion + validation
   │
   ├─ auth commands ────── ChatGPT OAuth + OS keyring
   │
   ├─ ProviderRouter
   │    ├─ OpenAI Chat Completions
   │    ├─ OpenAI Responses
   │    ├─ ChatGPT subscription Responses
   │    ├─ Anthropic / Gemini
   │    └─ bounded command/WASM provider plugin
   │
   ├─ CombinedTools
   │    ├─ built-ins
   │    ├─ legacy command tools
   │    ├─ real MCP stdio sessions
   │    └─ bounded command/WASM tool plugins
   │
   ├─ Hook runtime ─────── eight typed events + stable policy
   │
   └─ TUI
        ├─ bounded channels/transcript
        ├─ one config reload task
        └─ Gray Tempest theme
```

原则：

- Provider/Agent/Tool 的既有 trait 和正常化事件继续使用。
- 新增抽象必须至少有两个真实调用方。受限子进程 helper 被 Tool、Hook、MCP legacy、Provider plugin、Theme plugin、WASM 共同使用，因此进入 `grey-core` 合理。
- 配置管理命令操作原始 TOML；运行时操作已合并、已验证配置；两者不得混用。
- P8 的限制参数由所有后续扩展复用，P9/P10 不另建队列和进程边界。

## 5. P6 设计

### 5.1 安全配置写回

新增 raw config 编辑层：

- 编辑目标优先级：已设置的 `GREY_CONFIG`（文件可尚不存在）→ 当前项目已有 `grey.toml` → 用户默认配置路径。
- 使用 `toml_edit::DocumentMut` 只修改目标 array-of-tables，不序列化 `GreyConfig`。
- 保留注释、未知字段、顺序和 `${ENV}` 字面引用。
- 修改前拒绝 symlink 和非普通文件。
- 使用独立 lock file 排他锁；同目录临时文件写入、`sync_all`、原子 rename；失败不得损坏旧文件。
- 新文件在 Unix 权限不宽于 `0600`。
- `show` 输出经过统一秘密字段递归脱敏；插件参数、MCP env 与认证字段不得原样打印。

管理对象共享同一编辑器，但不共享一个臃肿的 `Service` trait：

- `[[plugins]]`
- `[[skills]]`
- `[[mcp_servers]]`
- `[tui]`

### 5.2 Hook 语义

八类事件：

| 事件 | 策略 |
|---|---|
| `pre_message_send` | 可阻断；可返回受限 prompt 改写 |
| `pre_prompt` | 可阻断；可返回受限 prompt 改写 |
| `permission_decision` | 可拒绝；永远不能把原拒绝提升为允许 |
| `pre_tool_call` | 可阻断工具执行 |
| `post_tool_call` | 观察型；失败不伪造已执行结果 |
| `session_start` | best effort，不阻断主流程 |
| `completion` | best effort，不覆盖真实完成状态 |
| `session_end` | best effort，正常退出和取消各执行一次 |

每个 payload 使用版本化结构，并按适用性包含：

```json
{
  "schema_version": 1,
  "event": "pre_tool_call",
  "workspace": "/workspace",
  "provider": "volcano-coding-plan",
  "model": "ark-code-latest",
  "prompt": "...",
  "tool": { "name": "read_file", "risk": "read" },
  "success": true,
  "error": null
}
```

不得包含 API key、OAuth token、完整有效配置或无关会话历史。配置 Hook 在前，启用的 Hook plugin 在后，顺序稳定。所有八条链通过同一个 Hook runner；CLI 不再维护平行实现。

### 5.3 Plugin manifest 与 WASM

命令插件和 WASM 插件显式区分，不按 `.wasm` 后缀猜测。

WASM v1 manifest：

```toml
schema_version = 1
api_version = 1
id = "example-theme"
name = "Example Theme"
kind = "theme"
version = "1.0.0"
runtime = "wasmtime"
entry = "plugin.wasm"
sha256 = "<64 lowercase hex characters>"
```

约束：

- `id`、kind、version、API version、相对路径、文件大小与 SHA-256 在安装和加载时均校验。
- `entry` 必须留在插件目录内，拒绝绝对路径、`..` 和 symlink 逃逸。
- manifest 最大 64 KiB，WASM 模块默认最大 16 MiB。
- v1 调用外部 `wasmtime run <module>`，不使用不存在的 `--allow-wasi`。
- 不传 `--dir`，`env_clear()` 后只传 Grey 协议元数据；stdin/stdout 是大小受限的 JSON。
- 无 Wasmtime、版本不兼容、超时、非零退出、超限输出和无效 JSON 都返回可操作错误，绝不降级为 shell。
- 外部 Wasmtime 避免把大型运行时链接进 Grey，只有调用 WASM 插件时才产生内存成本。

安装：

- `grey plugins install PATH` 安装本地 manifest/目录。
- `grey plugins install HTTPS_URL --sha256 ...` 可安装远程 manifest；HTTPS 和显式完整性值必需。
- 先下载到 managed plugin root 的 staging 目录，校验完成后原子改名。
- `uninstall` 只删除 managed plugin root 内经 ID 精确解析的目录；命令插件的外部可执行文件不删除。
- TUI 在下一条 prompt 边界重新加载扩展注册表，实现不重启 Core 生效；正在执行的调用继续使用其启动时快照。

### 5.4 Provider 与 Theme plugin

- Provider plugin v1 可返回单个完整 JSON 响应；文档明确它不是增量流，不冒充 streaming。
- 输出映射为正常化 `ProviderEvent`，并受响应、tool-call 和子进程输出上限控制。
- Theme plugin 必须由 `tui.theme.plugin = "id"` 精确选择，不再由“第一个启用插件”隐式获胜。
- Theme 输出只接受 preset 和允许的颜色 token；未知字段、无效色值或错误 kind 被拒绝，并保留上一份有效主题。

### 5.5 Provider failure 分类

将无类别字符串错误提升为：

- `Auth`
- `Authorization`
- `RateLimit`
- `Transport`
- `Server`
- `Protocol`

行为：

- Auth/Authorization 不重试、不跨 provider fallback，并给出精确登录/密钥指令。
- RateLimit、Transport、Server 仅在产生可见输出前按既有策略重试/fallback。
- Protocol 错误不盲目重试；输出已可见时永远不切换 provider。

这既避免无效重试，也防止 ChatGPT OAuth 失败后把用户提示词自动发送给火山、OpenCode 或其他插件。

### 5.6 性能门禁

重建现有 P6 脚本：

- release 启动时间门槛恢复为 `<300ms`，使用多次采样中位数并区分冷/热启动。
- RSS 按 macOS bytes 与 GNU time KiB 分别转换；无法解析必须失败。
- 大仓库 fixture 为 100,000 文件，测 Grey 自己的 workspace discovery/index 操作，不测系统 `find`。
- TUI 使用固定后端、尺寸和帧数，输出机器可读 JSON；PR CI 使用回归上限，稳定 runner/nightly 使用绝对门槛。
- 门禁不能以 16 GiB 作为“低内存”阈值。

## 6. OpenAI ChatGPT Plus/Pro OAuth

### 6.1 选择

采用 Grey 原生 Rust 实现，行为参考 OpenCode 的 Codex auth plugin，并以 OpenAI 官方 Codex Rust 登录实现为首要协议来源。OpenCode 和 Codex 当前使用同一 public client ID。

不调用 OpenCode 进程，不读取 OpenCode 私有 auth 文件，不链接完整 Codex runtime。

### 6.2 CLI

```text
grey auth login openai
grey auth status openai
grey auth logout openai
```

普通模型调用缺凭据时只提示登录命令，不自动弹浏览器。`login`：

1. 生成独立 CSPRNG state 与 PKCE verifier。
2. SHA-256 得到 S256 challenge，URL-safe base64 no padding。
3. 仅绑定 loopback；先尝试官方/兼容回调端口 1455，必要时尝试官方备用端口 1457。
4. 构造固定 issuer 的授权 URL并设置 `originator=grey`，不冒充 OpenCode。
5. 用系统原生命令打开浏览器；失败时打印 URL 并继续等待。
6. 最多等待 5 分钟，只接受一次 `GET /auth/callback`。
7. 严格校验 path、state、code、请求行/header 大小；错误页面 HTML 转义。
8. 用 PKCE code verifier 交换 token，解析 account ID 与过期时间。
9. 保存至 OS keyring。

OAuth 固定 profile：

- issuer：`https://auth.openai.com`
- authorize/token endpoint、public client ID、scope、redirect policy 编译期固定
- ChatGPT Codex endpoint：`https://chatgpt.com/backend-api/codex/responses`

这些值不能由 `grey.toml` 或 provider `base_url` 覆盖，避免把授权码/refresh token 发给恶意端点。

### 6.3 凭据

- access token、refresh token、account ID 与 expiry 只存 OS keyring。
- 不回退到 TOML、Session SQLite、response cache 或明文 auth 文件。
- keyring 不可用时登录明确失败并给出平台诊断；API Key provider 不受影响。
- `status` 只显示登录状态、账户的非秘密标识和过期状态。
- `logout` 删除 keyring entry；若协议支持撤销则 best effort revoke，但本地删除不得依赖网络成功。
- 所有错误正文受限且脱敏；日志永远不打印 code、token、Authorization 或完整 callback URL query。

### 6.4 刷新

- provider 发请求前以过期提前量检查 token。
- refresh 使用 single-flight，多个请求只执行一次刷新。
- 首次 401 可强制 refresh 后重试一次；再次 401/403 分类为 Auth/Authorization。
- refresh 返回的新 refresh token 原子替换旧值；刷新失败不能覆盖仍可恢复的旧凭据。

### 6.5 Provider 隔离

`ProviderEntry` 增加小型 `auth` 枚举：

```toml
[[providers]]
id = "openai-subscription"
protocol = "openai_responses"
auth = "chatgpt_oauth"
models = [{ id = "gpt-5.4", name = "GPT-5.4" }]
```

规则：

- `chatgpt_oauth` 只允许 `openai_responses`，并要求 `api_key`、`base_url` 为空。
- `api_key` 模式不读取 keyring。
- 自定义 OpenAI-compatible provider、火山和 OpenCode 永远不能继承 ChatGPT token。
- OAuth token 不进入 provider JSON、插件环境或 hook payload。

## 7. OpenAI Responses

新增 Responses adapter，复用现有 Provider trait、SSE decoder、bounded HTTP error 和 Agent tool loop。

请求映射：

- System/developer context 不丢弃，保留所有后续 LSP/system summary。
- User/assistant 历史映射为 Responses input message。
- 既有 assistant tool call 映射为 `function_call`。
- Tool result 映射为 `function_call_output`。
- Grey `ToolDefinition` 映射为 Responses function tool。
- `stream = true`；只在协议/profile 支持时发送 temperature。

事件映射：

- `response.output_text.delta` → `Delta`
- function call add/delta/done → 按 item/call ID 累积；完整 JSON 后发送一个 `ToolCall`
- `response.completed` → 验证未完成调用并发送一次 `Done(Usage)`
- `response.failed`、`response.incomplete`、`error`、畸形 JSON、重复 terminal → 分类错误

工具数量、参数累计字节和单 SSE event 继续有硬上限。由于 live API 可能在 `function_call_arguments.done` 缺少 name，解析器必须用早先的 output item 关联，不能只信 done event。

## 8. 火山 Coding Plan 与 OpenCode Go/Zen

### 8.1 火山 Coding Plan

使用 Coding Plan 专用 OpenAI-compatible base URL：

```toml
[[providers]]
id = "volcano-coding-plan"
protocol = "openai"
base_url = "https://ark.cn-beijing.volces.com/api/coding/v3"
api_key = "${ARK_API_KEY}"
models = [{ id = "ark-code-latest", name = "Ark Code Latest" }]
```

不得在 Coding Plan live test 中使用普通 `/api/v3`。现有 `ARK_API_KEY`/`VOLCANO_API_KEY` fallback 保留，并补动态 provider 自动创建/错误提示的一致性测试。

### 8.2 OpenCode Go 与 Zen

OpenCode 当前按模型混合使用 Responses、Chat Completions、Anthropic Messages 和 Gemini-compatible endpoint。本设计不新增猜测型 `opencode` 协议，而是新增/复用明确协议：

- `openai_responses`
- `openai`
- `anthropic`
- `gemini`

文档提供 Go 与 Zen 的分协议 provider 模板。同一 API key 可通过 `${OPENCODE_API_KEY}` 引用，但每个 provider 只拥有一种明确 wire protocol。模型不会在运行时靠名称猜端点。

## 9. P7 v1.0 发布

### 9.1 仓库内交付

- 工作区版本在所有门禁通过后统一提升为 `1.0.0`。
- 更新 package 描述、readme、MSRV、license 与发布元数据。
- 使用固定版本的 cargo-dist 生成并审查 release workflow，不手写无法验证的生成器结构。
- 目标：macOS x86_64/aarch64、Linux GNU x86_64/aarch64、Windows MSVC x86_64。
- 产出 archive、checksum、shell installer、PowerShell installer；在支持时产出 SBOM/attestation。
- release verify workflow 安装刚构建的 artifact 并运行 `grey --version`、`--help` 与 mock smoke。
- Homebrew formula 由 release 元数据生成，在 macOS runner 执行安装验证。

文档与资产：

- `CHANGELOG.md`
- `LICENSE-MIT`
- `SECURITY.md`
- `docs/quickstart.md`
- `docs/config-reference.md`
- `docs/plugin-api-v1.md`
- `docs/troubleshooting.md`
- `docs/release-checklist.md`
- `examples/grey.toml`
- Plugin、Theme、MCP、Skill 示例

### 9.2 外部发布门禁

当前仓库没有 git remote、公开 repository URL、Homebrew tap 或发布 token。仓库内可以完成并验证所有构件，但以下动作必须在所有者提供明确目标后单独批准：

- 推送 tag/release
- crates.io publish
- 创建或写入外部 Homebrew tap
- 使用 GitHub/HOMEBREW token

发布脚本默认只 dry-run；`--publish` 必须显式给出并再次验证 clean tree、tag、CHANGELOG、secret scan 与 artifact checksum。

## 10. P8 内存封闭与生命周期

### 10.1 有界状态

新增 `[runtime]` 实际可用限制，均有安全默认值和 clamp：

- `event_queue_capacity`
- `input_queue_capacity`
- `prompt_queue_capacity`
- `transcript_max_bytes`
- `response_max_bytes`
- `command_stdout_max_bytes`
- `command_stderr_max_bytes`
- `skill_max_bytes`

行为：

- Agent→TUI 使用 bounded `mpsc::Sender` 并 await，产生背压且不丢 delta。
- prompt queue 很小；繁忙时第二条提交给出明确状态，不无限排队。
- input thread 使用 bounded sender；UI 关闭后 sender 被唤醒并退出。
- transcript 统一通过一个 UTF-8 安全 append/trim helper，只保留最新内容并插入单个截断标记。
- 单轮 provider 文本和 tool-call 参数超过上限即中止，不继续增长。
- Session/ContextManager 必须验证长期消息经过既有 token budget trim，不保留双份无界历史。

### 10.2 受限子进程

一个共享 helper 负责：

- direct program + args 或明确的 legacy shell
- cwd 与受控 env
- bounded stdin
- 并行 drain stdout/stderr
- 达到上限后继续 drain 并丢弃，避免 pipe deadlock
- timeout
- kill process group / Windows Job，并 wait/reap
- 输出 UTF-8 lossy 解码、截断标记和退出状态

Provider、Theme、Tool、Hook、legacy command tool、MCP stderr 与 Wasmtime 都使用它或相同的底层受限 reader。禁止先 `wait_with_output()` 再截断。

### 10.3 取消和定时器

- TUI worker 接受 shutdown signal。
- UI 退出时关闭 prompt sender、发送 shutdown、等待 worker 有界清理；只在超时后 abort。
- inflight provider/tool 取消后进入统一清理，并固定执行一次 `session_end`。
- persistent completion reminder 成为 `tokio::select!` 的真实 timer 分支，不依赖新输入唤醒。
- config reload task、MCP child 和 input thread 均属于同一 shutdown 域并被 join。

### 10.4 长稳证据

- 单元测试证明容量 1 的 event channel 产生背压且不丢事件。
- transcript/response/tool 参数/子进程 stdout/stderr 分别做超限测试。
- 子进程 timeout 后证明已 reap。
- release soak fixture 产生至少 10,000 delta 与 1,000 tool event，采样 RSS、队列水位和 child 数。
- CI 短 soak 验证平台；一键脚本 `--long` 支持至少 1 小时持续测试。
- 通过条件基于稳定窗口的 RSS 斜率与硬上限，不用“运行结束没崩溃”代替无泄漏证据。

## 11. P9 Gray Tempest TUI

### 11.1 Taste 设计读数

- 产品：暗色开发者 TUI。
- 气质：克制、冷峻、工业感；不是霓虹赛博朋克。
- 参考：“灰蛊风暴”的纳米舰队青色，但不复制游戏 UI 或素材。
- 设计旋钮：`DESIGN_VARIANCE=4`、`MOTION_INTENSITY=2`、`VISUAL_DENSITY=5`。
- 不使用渐变、阴影、发光外框、装饰动画或多彩状态条。

### 11.2 主题

内置 `gray-tempest` preset，默认使用终端背景以尊重用户环境：

| token | TrueColor | 语义 |
|---|---|---|
| border | `#1D555A` | 冷灰青分隔 |
| accent | `#44E0D3` | 唯一主强调 |
| prompt | `#89FFF2` | 输入焦点 |
| status_fg | `#D7FAF7` | 状态正文 |
| status_bg | `#12383B` | 深青状态底 |
| muted | `#6F8F90` | 次级元数据 |
| error | `#FF7B72` | 唯一暖色错误 |

成功状态使用 accent，警告使用 muted + 明确文本/符号，不再硬编码 green/blue/yellow/white。每个 token 提供 256 色和 ANSI 16 色 fallback。

### 11.3 布局

- 顶部单行 header：Grey、provider/model、branch；不使用完整 box。
- transcript 占主要空间，不画第二层厚边框。
- 输入区只用一条细分隔线和清晰 prompt，不再嵌套 box。
- 底部一行 status/usage/help hint。
- 错误不能只靠颜色表达，必须同时有文本或符号。
- help overlay 保留，但遵循同一 token。

### 11.4 热重载

- 只启动一个轻量 config mtime poller，不新增 watcher 依赖。
- 修改有效配置后通过 `watch<TuiSettings>` 应用主题、布局、键位和 runtime limits。
- reload 不清 transcript、usage、scroll、inflight 状态。
- 无效 TOML、无效颜色或 theme plugin 错误保留上一份有效配置并给出状态提示。
- shutdown 必须停止并 join poller。

使用 Ratatui `TestBackend` 对常见终端尺寸做确定性布局测试；人工 taste 审查补充截图/录屏，但不能替代结构测试。

## 12. P10 扩展性与可修改性

### 12.1 Plugin、Hook、Theme

```text
grey plugins list [--kind KIND]
grey plugins find QUERY [--kind KIND]
grey plugins show ID
grey plugins add ...
grey plugins install SOURCE
grey plugins remove ID
grey plugins uninstall ID
grey plugins enable ID
grey plugins disable ID

grey hooks list|find|show|add|remove|enable|disable ...
```

`hooks` 是 `kind=hook` 的清晰 CLI 视图，底层仍是同一 `[[plugins]]`，不复制 registry。

`find` 对已配置/已安装条目的 id、name、description、kind 做不区分大小写搜索。没有外部索引时不声称在线搜索。

### 12.2 Skill

Skill 使用受控目录中的 `SKILL.md`，不是可执行插件：

```text
grey skills list
grey skills find QUERY
grey skills show ID
grey skills add PATH
grey skills remove ID
grey skills enable ID
grey skills disable ID
grey --skill ID "prompt"
```

- `add` 验证目录、ID、`SKILL.md`、UTF-8 与大小后复制到 managed skill root。
- `remove` 只删除 managed root 内精确 ID；拒绝 symlink/path traversal。
- `find` 搜索已安装 metadata 与说明。
- `--skill` 可重复，仅加载 enabled Skill，按命令顺序追加到明确标记的 system context。
- Skill 不获得 tool 权限、不运行命令、不改变 approval policy。
- Skill 总内容受 P8 上限控制；不自动加载所有 Skill，避免 prompt 和内存膨胀。

### 12.3 真实 MCP stdio

新增：

```toml
[[mcp_servers]]
id = "filesystem"
command = "mcp-server-filesystem"
args = ["/workspace"]
enabled = true
timeout_ms = 5000
```

CLI：

```text
grey mcp list|find|show|add|remove|enable|disable ...
```

最小真实 MCP 会话：

1. 启动一个持久 stdio child。
2. 发送 `initialize`，校验 protocol/version/capabilities。
3. 发送 `notifications/initialized`。
4. 调用 `tools/list`，处理 pagination 并缓存 definitions。
5. Agent 调用时发送 `tools/call`，按 JSON-RPC ID 匹配响应。
6. 跳过允许的 notification；畸形、超限或未知 response ID 是协议错误。
7. shutdown 时关闭 stdin、等待 child，有界超时后回收进程组。

P10 只承诺 stdio + tools。resources、prompts、sampling、SSE/Streamable HTTP 在有真实需求时另行扩展。当前 `[[mcp_tools]]` 重命名/标注为 deprecated legacy command tools，并提供迁移文档。

### 12.4 TUI 可修改性

- 内置 preset、颜色 overrides、layout、keys、completion、runtime limits 都可配置。
- 外部 Theme plugin 精确选择并热重载。
- 用户可以删除/禁用 Theme plugin 并在下一次 reload 回退到 preset。
- 不允许 Theme plugin 修改 tool 权限、Provider 或 auth。

## 13. 一键测试与发布验证

### 13.1 入口

```text
scripts/test-all.sh
scripts/test-all.sh --live
scripts/test-all.sh --live-all
scripts/test-all.sh --long
scripts/verify-release.sh
```

`test-all.sh`：

1. 验证 Rust 1.97.1、rustfmt、clippy 与必要平台工具。
2. `git diff --check`。
3. fmt、clippy、workspace tests、doc tests。
4. release build。
5. mock CLI/TUI、配置 CRUD、OAuth mock、WASM shim/真实 CI fixture、MCP fixture。
6. P6 perf gate。
7. P8 soak gate。
8. release dry-run/package/dist verify。
9. 汇总 PASS/FAIL/SKIP，任何必需门禁 SKIP 均使“完全测试”失败。

`--live`：

- 要求 `ARK_API_KEY` 与 `ARK_MODEL`，调用火山 Coding Plan `/api/coding/v3`。
- 如果 keyring 已有 ChatGPT OAuth，则运行一个最小订阅请求；无凭据时提示 SKIP，不自动登录。
- 按已设置的 OpenCode key 测试 Go/Zen 配置。

`--live-all`：

- 所有 live 凭据都必需。
- ChatGPT 未登录时显式调用 `grey auth login openai`，用户在浏览器完成后继续。
- 缺任一 provider 凭据或 live 验证失败即整体失败。

`--long`：

- 运行至少一小时的内存 soak；可与 `--live-all` 组合。

所有脚本使用临时配置和临时数据目录，不写用户真实配置；trap 清理 child；日志脱敏 token、Authorization 与 API key。

### 13.2 发布验收

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo test --workspace --all-features --locked`
- `cargo test --workspace --all-features --locked --doc`
- `cargo build --workspace --release --locked`
- 所有离线 fixtures 与短 soak 通过
- live 测试由用户凭据运行并保存脱敏报告
- cargo-dist plan/build 与每平台安装 smoke 通过
- 文档、配置字段、示例和 CHANGELOG 与实际行为一致
- secret scan 无明文凭据

## 14. TDD、Review 与实施顺序

严格顺序：

1. 保存本设计并提交独立 design commit。
2. 编写逐文件实施计划。
3. 收尾当前 WIP 的格式、clippy 与明确缺陷。
4. P8 基础边界：受限进程、bounded channels、response/transcript caps、shutdown。
5. P6 配置写回与 Hook。
6. Responses + ChatGPT OAuth + 火山/OpenCode provider templates。
7. P6 manifest/WASM/Provider/Theme/热更新与性能门禁。
8. P10 MCP/Skill/管理命令。
9. P9 TUI 视觉与热重载。
10. P7 文档、版本、dist、安装与发布脚本。
11. 每个任务执行 failing test → 最小实现 → targeted pass。
12. 每个阶段执行 spec compliance review → 修复 → re-review。
13. 再执行 code quality review → 修复 → re-review。
14. 最终全量、release、soak、live/manual 与完成审计。

实现不能因为 live 凭据或远端发布缺失而伪造通过；仓库内功能可继续完成，外部步骤明确报告为等待用户执行/授权。

## 15. 参考

- OpenCode ChatGPT OAuth implementation: <https://github.com/anomalyco/opencode/blob/dev/packages/opencode/src/plugin/openai/codex.ts>
- OpenCode providers: <https://dev.opencode.ai/docs/providers/>
- OpenCode Go: <https://dev.opencode.ai/docs/go/>
- OpenCode Zen: <https://dev.opencode.ai/docs/zen/>
- OpenAI Codex login server: <https://github.com/openai/codex/blob/main/codex-rs/login/src/server.rs>
- OpenAI Codex auth manager: <https://github.com/openai/codex/blob/main/codex-rs/login/src/auth/manager.rs>
- OpenAI Responses streaming: <https://platform.openai.com/docs/api-reference/responses-streaming>
- Volcengine Coding Plan: <https://www.volcengine.com/docs/82379/2165245?lang=zh>
- Wasmtime CLI: <https://docs.wasmtime.dev/cli-options.html>
- Wasmtime security: <https://docs.wasmtime.dev/security.html>
- cargo-dist workspace guide: <https://axodotdev.github.io/cargo-dist/book/workspaces/simple-guide.html>
- cargo-dist configuration: <https://axodotdev.github.io/cargo-dist/book/reference/config.html>
