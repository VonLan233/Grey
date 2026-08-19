# ADR-005: P5 TUI customization and completion feedback scope

## Context

P5 的目标强调“UI 定制与完成反馈”。在交付阶段，已完成：
- 配置化布局（`[tui.layout]`）
- 主题外观（`[tui.theme]` 与颜色覆盖）
- 键位体系（`[tui.keys]`）与帮助覆盖（`<leader>k`）
- 状态栏（任务/模型/分支/token/错误徽标）
- 长任务完成提醒（时间/步数阈值、终端铃声、桌面通知、可选持续提醒）

## Decision

1. **配置入口统一放在 `GreyConfig.tui`**
   - `theme.preset`：`default`、`slate`、`sunset`、`mono`。
   - `theme.overrides`：支持 `border/accent/prompt/status_fg/status_bg/muted`。
   - `layout.input_lines`：配置输入区高度（最小 1 行）。
   - `completion`：`enabled/long_running_steps/long_running_seconds/bell/strong_bell/notify/persistent`。
   - `keys`：`leader/help/quit/clear/scroll_up/scroll_down`。
2. **渲染与输入循环使用该配置直接驱动**
   - TUI 运行时接收 `&TuiConfig` 并转为运行时 `TuiSettings`，保持兼容默认值。
   - 键位映射失败回退到安全默认（leader/help/quit/clear/分页上下键）。
3. **完成提醒采用终端铃声与桌面提醒**
   - 与 `long_running_steps` 或 `long_running_seconds` 任一达标即触发。
   - `strong_bell` 控制“多次鸣铃”。
   - `notify` 打开后发送 `notify-rust` 桌面提醒（完成/失败）；`persistent` 开启后按周期重复提醒（直到用户清理/开启下一轮任务）。

## Consequences

- 交付了 P5 的关键目标：布局、主题、键位、状态栏与完成提醒完整打通。
- 强提醒与桌面提醒采用 `notify-rust`，具备“长任务完成/失败提醒 + 可选持久提示”能力。
- 后续可在不改配置结构前提下扩展桌面常驻状态与更细粒度提醒策略（如优先级分级）。
