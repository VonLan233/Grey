# v0.1.1 TUI 简洁化改版 — 设计文档

> 日期：2026-08-21
> 状态：已批准（人工审核意见驱动）
> 来源需求：`docs/plans/v0-1-1-tui.md` §人工审核后意见 第 1–4 条

## 背景与目标

人工审核指出四项问题：

1. TUI 冗杂、开屏丑（不如 OpenCode/Pi 简单明了）
2. 输入栏只换页不换行（根因：输入框高度 `Constraint::Length(input_lines)` 固定）
3. `/` 命令无自动补全、无独立选择框（期望 Neovim 式体验）
4. 状态栏不清晰、排版差（期望 Pi footer 式）

目标：把 Grey TUI 从「面板拼盘」改成 Pi 式无边框文档流，同时保留 Grey 已有交互能力
（多行编辑、滚动、斜杠命令、模型切换、markdown 渲染）。

参考实现：本机 pi coding agent 的 `footer.js`（两端布局 + 截断策略）与
`interactive-mode.js` 内置 header（logo 行 + compact 提示行）。

## 决策记录

| 分叉点 | 决策 |
|--------|------|
| 开屏形态 | **A. Pi 式 header**：删除全屏 splash，启动即会话界面，顶部两行 header 随消息上滚 |
| 边框 | **A. 全部去边框**：会话区纯 markdown 流；输入区与一条 dim 横线分隔 |
| 补全 | Neovim 式浮动补全窗（输入框上方弹出） |
| Footer | Pi 式单行两端布局 |

## 设计

### 1. 布局总览（改版后）

```
Grey v0.1.1                                       ← header 行 1：bold accent
Enter 发送 · Shift+Enter 换行 · / 命令 · ? 帮助    ← header 行 2：muted，按 keybindings 动态生成

（markdown 会话流，无边框无标题，滚动行为不变）
...
──────────────────────────────────────────────    ← dim 横线（1 行）
> 输入内容…                                        ← 输入区：高度自适应
↑1.2k ↓345 · task:fix-layout      (openai) gpt-5 (main)   ← footer：1 行两端布局
```

垂直 Layout 从 `[Min(0), Length(1)任务, Length(input_lines)输入, Length(1)状态]`
改为 `[Min(0)会话, Length(1)分隔线, Length(自适应)输入, Length(1)footer]`。

### 2. Header（替代 splash）

- 删除 `show_splash` 状态、`render_splash()`、splash 相关测试与「按任意键开始」逻辑。
- 会话输出流在启动时 prepend 两行：
  - 行 1：`Grey v{version}`（bold + accent 色）
  - 行 2：compact 快捷键提示，用现有 `TuiKeyBindings::labels()` 动态生成，
    `·` 分隔，muted 色。内容固定四项：发送 / 换行 / 斜杠命令 / 帮助。
- header 是会话文本的一部分，随消息自然上滚；`/clear` 后重新插入（保持首屏可发现性）。

### 3. 输入区自适应高度

- 高度 = `clamp(wrap 后视觉行数, MIN=1, MAX=终端高度×40%)`。
- 未达 MAX 时全部输入可见（真换行体验）；超过 MAX 才启用内部滚动
  （复用现有 `input_scroll` / `input_cursor_position` 机制）。
- `input_lines` 配置项废弃：grey-core 保留字段与校验（不破坏既有配置文件），
  grey-tui 不再读取；文档标注 deprecated。

### 4. 斜杠命令注册表 + 补全 UI

注册表：

```rust
struct CommandSpec {
    name: &'static str,          // "help"
    aliases: &'static [&'static str], // ["?"]
    args_hint: &'static str,     // "<name>" 或 ""
    description: &'static str,   // "显示帮助"
}
static COMMANDS: &[CommandSpec] = &[ /* help clear quit exit model usage status models */ ];
```

- `SlashCommand::parse()` 改为查注册表，消除手写 match 与命令清单多处重复。
- 补全激活条件：输入以 `/` 开头且光标位于第一个空白前的词内。
- 浮动窗渲染在输入区上方（ratatui `Clear` widget 防穿透），每项格式
  `/{name} {args_hint}  — {description}`；前缀过滤（ASCII case-insensitive）；
  选中项 accent 高亮。
- 按键：`Up/Down`（及 `Ctrl+N/P`）导航并循环；`Tab/Enter` 采纳（替换当前词，
  `/model ` 采纳后带尾随空格）；`Esc` 关闭；继续输入实时过滤；无匹配不渲染。
- 二级补全：`/model ` 后空格触发时列出可用模型名（数据源同 `/models`），
  同一浮动窗组件复用。

### 5. Footer（Pi 式）

- 左段（dim）：`↑{in} ↓{out}`（<1000 原样，≥1000 缩写 `1.2k`）；
  有任务标签时追加 `· task:{name}`；仅 `status_has_error()` 时追加红色 `ERR`。
- 右段（dim）：`({provider}) {model} ({branch})`；provider 仅在多 provider 时显示
  （沿用 pi 规则）；branch 为空则省略括号段。
- 宽度不足：左段优先完整，右段截断加 `…`；仍不足则只渲染左段。
- 版本号只在 header 与 `/status` 出现；快捷键提示只在 header 与 help overlay。

### 6. 滚轮支持（按鼠标位置划分语义）

- 会话区：保持现状 —— 滚轮滚动历史 + PageUp/PageDown + auto-follow。
- 补全浮动窗：候选数超过窗高时，滚轮滚动候选列表，选中项始终跟随可见
  （窗口内偏移，不影响会话区滚动）。
- 输入区：仅当内容超过 MAX 高度、处于内部滚动态时，滚轮滚动输入内容；
  否则该区域滚轮事件穿透给会话区。
- 判定依据为鼠标事件坐标落在哪个区域，与视觉分区一致。

### 7. Taste 审核 checklist（review 门禁）

1. 无边框、无区块标题；持久 UI 仅分隔线 + footer 各一行。
2. 装饰色只允许 accent/dim(muted)/prompt 三种用途；错误红仅在错误态出现。
3. 任何新 UI 元素必须能回答「删掉它会怎样」；答不清就删。
4. 信息默认隐藏，细节由 help overlay / 斜杠命令按需唤出。

## 错误处理

- 终端极窄/极矮（< 6 行）：补全窗与输入区让位给会话区，footer 保底 1 行；
  所有布局计算使用 saturating 减法防 panic（沿用现状约定）。
- 补全采纳时若输入已被用户改动，以当前 InputBuffer 为准重新定位替换区间，
  不缓存过期偏移。

## 测试策略（TDD）

- 单元测试先行（红→绿）：
  - 补全：前缀过滤、case-insensitive、导航循环、采纳替换、无匹配关闭。
  - Footer：k 缩写、两端布局 padding、右段截断、错误态 ERR、缺 branch/provider。
  - 输入高度：clamp 边界（1 行、恰好 MAX、超 MAX）、MAX 随终端高度变化。
  - Header：文本生成含动态 keybinding labels；`/clear` 后重现。
  - 滚轮：补全窗列表滚动边界、输入区内部滚动与穿透判定、会话区 auto-follow 不回归。
- 迁移：删除 splash 断言，新增 header/footer 断言。
- 验证门禁：`cargo fmt --check`、`cargo clippy -- -D warnings`、
  `cargo test --workspace`、release build 全绿并贴实际输出。

## 明确不做（本次范围外）

- 审核意见第 5 条：其余功能等用户体验后再议。
- 树状会话历史、prompt 模板、`/reload`、ccusage 外部对接、LSP/Skills 自动发现
  （维持 v0.1.1 计划中的「待定」状态）。
- 主题系统扩展、鼠标交互增强。
