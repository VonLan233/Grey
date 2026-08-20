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

| # | 需求 | 说明 |
|---|------|------|
| 1 | 开屏页 | 类 Claude Code / Crush，不照抄；Grey + Gray 风格侧面头像 |
| 2 | 输入框换行 | 自动 + 手动换行，能看全命令再修改（多行编辑） |
| 3 | Agent 消息区上下滚动 | 查看历史消息 |
| 4 | `/` 斜杠命令 | `/model`、`/reload`、prompt 模板 `/name` 等（Pi 骨架） |
| 5 | Token 消耗监测 | 结合 ccusage；`usage` 记账已存在，缺 TUI 内实时显示 |
| 6 | Markdown 渲染 | ratatui 需自建 markdown → TUI 渲染层 |
| 7 | 状态栏改版 | 任务名移到输入框上方 / Agent 框下方，解耦拥挤 |
| 8 | TUI 内模型配置 | 会话中切换模型（`/model` + `Ctrl+L/P`）；`[[providers]]` 已存在 |
| 9 | MCP / LSP / Skills 自动添加 | 自动发现并接入；会牵扯 MCP 运行时接线（此前搁置） |

## 工作流拆解

- **A. 布局 / 信息架构**：#7 状态栏、#1 开屏、#3 滚动 —— 改动最可控
- **B. 输入体验**：#2 多行编辑、#4 斜杠命令 —— 核心交互（Pi 骨架）
- **C. 渲染**：#6 Markdown —— 独立可测
- **D. 集成**：#5 ccusage、#9 自动发现、#8 模型切换 —— 最重

## 建议顺序

**B 优先**：`/` 斜杠命令是 Pi 式交互的骨架，`/model` 跑通后 #8、prompt 模板、`/reload` 顺势长上。
其次 A（快速见效）。C、D 随后。

## 待定项（等体验 / 头脑风暴后再决定）

- 树状会话历史（`/tree`、`/export`、`/share`）→ 可能放 v0.2
- 扩展 package 体系（Pi 式 extensions + 主题 + prompt 模板打包共享）
- `/reload` 自我修改
- 其余体验中产生的新需求
