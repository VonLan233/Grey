# v0.1.1 开发计划：TUI 大改（Pi 式可定制 Harness 方向）

> 日期：2026-08-21
> 状态：规划中（未开始实现）

## 方向对齐

- 参考：**Pi（pi.dev）** —— "There are many agent harnesses, but this one is yours."
  Grey 的本质 = 一套能按用户方式驾驭的极简 harness，而非封死的产品。
- 学习对象：**OpenCode TUI**（交互实现主参考）、**Crush**（开屏参考，不照抄）、**ccusage**（token 监测）。
- 保留 Grey 自身特性：Rust、MCP 客户端、`orchestrate` 多 Agent、`goal/loop`、Grey + Gray 头像开屏、安全模型。
  与 Pi 刻意「无 MCP / 无 sub-agent / 无 plan mode」不同——这些正是 Grey 的差异化优势，保留。

## Backlog（9 项）

| # | 需求 | 说明 | 状态 |
|---|------|------|------|
| 1 | 开屏页 | 类 Claude Code / Crush，不照抄；Grey + Gray 风格侧面头像 | ✅ `38cb3b5` 可一键关闭，含按键/斜杠提示 |
| 2 | 输入框换行 | 自动 + 手动换行，能看全命令再修改（多行编辑） | ✅ `1b409a2` Shift/Alt+Enter 换行、Up/Down、行级 Home/End、自动滚动 |
| 3 | Agent 消息区上下滚动 | 查看历史消息 | ✅ `778044a` 滚轮 + PageUp/PageDown，auto-follow |
| 4 | `/` 斜杠命令 | `/model`、`/reload`、prompt 模板 `/name` 等（Pi 骨架） | 🚧 `/help /clear /quit /exit /model /usage` 已上线（`45391d2` `78f3a1d`）；`/reload` 与模板待定 |
| 5 | Token 消耗监测 | 结合 ccusage；`usage` 记账已存在，缺 TUI 内实时显示 | 🚧 状态栏 i/o 已显示，`/usage` 已上线；ccusage 外部对接待定 |
| 6 | Markdown 渲染 | ratatui 需自建 markdown → TUI 渲染层 | ✅ `4e26e4c` pulldown-cmark：标题/代码/列表/引用/粗斜体 |
| 7 | 状态栏改版 | 任务名移到输入框上方 / Agent 框下方，解耦拥挤 | ✅ `d4ca946` 独立任务行，状态栏去帧率/header |
| 8 | TUI 内模型配置 | 会话中切换模型（`/model` + `Ctrl+L/P`）；`[[providers]]` 已存在 | 🚧 `/model <name>` 下一条 prompt 生效（`45391d2` + `203a6b8`）；Ctrl+L/P 快捷切换待定 |
| 9 | MCP / LSP / Skills 自动添加 | 自动发现并接入；会牵扯 MCP 运行时接线（此前搁置） | 🚧 MCP 运行时接线已通（`a7aeeee`，配置的 `[[mcp_servers]]` 启动即连接）；LSP/Skills 自动发现待定 |

## 工作流拆解

- **A. 布局 / 信息架构**：#7 状态栏、#1 开屏、#3 滚动 —— 改动最可控
- **B. 输入体验**：#2 多行编辑、#4 斜杠命令 —— 核心交互（Pi 骨架）
- **C. 渲染**：#6 Markdown —— 独立可测
- **D. 集成**：#5 ccusage、#9 自动发现、#8 模型切换 —— 最重

## 建议顺序

**B 优先**：`/` 斜杠命令是 Pi 式交互的骨架，`/model` 跑通后 #8、prompt 模板、`/reload` 顺势长上。
其次 A（快速见效）。C、D 随后。

## 实施进度（2026-08-21）

- **已完成并提交**：开屏页、多行输入、滚动（滚轮/PageUp）、斜杠命令（/help /clear /quit /exit /model /usage）、
  会话内 `/model` 切换、Markdown 渲染、状态栏改版、MCP 运行时接线。完整 release gate（fmt/clippy/tests/构建/perf/soak）全绿。
- **待定（等体验后决定）**：`/reload` 自我修改、prompt 模板（`/name`）、Ctrl+L/P 模型快捷切换、
  ccusage 外部对接、LSP/Skills 自动发现、树状会话历史（v0.2）。

## 待定项（等体验 / 头脑风暴后再决定）

- 树状会话历史（`/tree`、`/export`、`/share`）→ 可能放 v0.2
- 扩展 package 体系（Pi 式 extensions + 主题 + prompt 模板打包共享）
- `/reload` 自我修改
- 其余体验中产生的新需求

## 人工审核后意见（已落地 2026-08-21）

1. TUI冗杂，不够简洁清新清晰；开屏页面很丑，不如Opencode和Pi那样简单明了 → ✅ 已落地：删全屏 splash 改 Pi 式 header（`a272ecf`），去边框+分隔线+Pi式footer（`fb78b5b`）
2. 输入栏目没有换行 之有换页，感觉根因是固定了输入栏和输出栏的大小，可以调整 → ✅ 已落地：输入区自适应高度 40% clamp（`67ee511`）
3. '/'命令没有类似LSP的自动补全和提示，需添加；‘/’没有独立选择框，我要的是和Neovim差不多的使用体验 → ✅ 已落地：Neovim式浮窗补全+`/model`二级（`2c61014` `0c43427`）
4. 状态栏不清晰，没有显示模型，分支，输入token和输出token；这个状态的排版也有问题，可以参考Pi的插件？ → ✅ 已落地：Pi式两端footer `↑in ↓out` 左、` (provider) model (branch)` 右，超宽截断（`fb78b5b`）
5. 滚轮支持 → ✅ 已落地：按区域分流 popup/input/会话（`f02dae0`）
6. 其他功能还没有尝试，等我尝试过后再说； → ⏳ 待体验

> 关联 spec：`docs/superpowers/specs/2026-08-21-v0-1-1-tui-simplify-design.md`
> 关联 plan：`docs/superpowers/plans/2026-08-21-v0-1-1-tui-simplify.md`