# HANDOFF_P5: TUI 交付交接

> 日期：2026-08-19
> 状态：已完成（交付就绪）

## 一、完成项

- `GreyConfig` 增加 P5 配置段：
  - `theme`：`preset` + `overrides`
  - `layout`：`input_lines`
  - `completion`：`enabled`/`long_running_steps`/`long_running_seconds`/`bell`/`strong_bell`/`notify`/`persistent`
- `grey-core/src/lib.rs` 导出 TUI 配置类型，供 CLI/TUI 共享。
- `grey-tui` 接收配置并驱动：
  - 布局高度（`input_lines`）
  - 主题 preset 与颜色覆盖（含 hex 与命名色）
  - 状态栏（模型、token、任务、分支、错误徽标）与可配置快捷键
  - 长任务完成提醒（软/强、可选终端铃声）
  - 桌面通知（notify-rust）与持久提醒（可选）
- 完成事件测试与行为钩子：
  - 长任务提醒触发逻辑
  - 关闭开关与阈值未命中场景
  - TUI 配置映射测试
- `grey-cli` 将 `config.tui` 注入 TUI 运行时。

## 二、待完善项（P5 计划余量）

- 待确认项：动画与反馈（流式光标/任务切换动效）已进入 P5 外延讨论，当前先不纳入交付。

## 三、证据与入口

- 配置样例与状态已更新：
  - `README.md`
  - `docs/阶段性开发文档.md`
  - `docs/adr/ADR-005-p5-tui-customization.md`

## 四、交付建议

- 下一步：保持此实现为 P5 交付版本，新增动画/动效与桌面常驻状态可作为 P5.1/后续阶段独立条目。
