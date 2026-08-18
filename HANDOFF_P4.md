# HANDOFF_P4: LSP 与编程特化（阶段性交接）

> 日期：2026-08-18  
> 状态：P4 原型推进中

## 一、阶段内完成项

- 在 `grey-tools` 增加 `lsp_diagnostics` 工具，基于 `grey-lsp` 的 `collect_file_diagnostics`。
- 在 `grey-lsp` 增加 `collect_file_definitions`。
- 在 `grey-tools` 增加 `lsp_definition` 工具，支持按位置信息返回定义定位。
- 在 `grey-lsp` 增加 `collect_file_references`，并在 `grey-tools` 增加 `lsp_references` 工具用于返回项目内符号引用位置。
- 工具定义与执行已接入 `grey-cli` 的统一工具链（`build_agent_and_session`）：
  - 在 `--read_only`/主会话下可直接被 agent 调用；
  - 使用 `config.lsp.rust_analyzer` 作为后端命令；
  - 对失败场景返回可读错误。
- 工具测试：
  - `grey-tools` 增加 `lsp_diagnostics` 定义与失败返回测试。
  - `grey-tools` 增加 `lsp_definition` 定义与失败返回测试。

## 二、待完成项（P4 下一步）

- 符号/悬停/重命名 的工具化与语义注入。
- 诊断到会话的实时注入策略（`Agent` 上下文中的结构化语义视图）。
- LSP 工具输出的缓存/去噪与 token 预算融合。

## 三、当前证据

- `crates/grey-lsp/src/lib.rs`：新增 `collect_file_diagnostics`、`collect_file_definitions` 与结果聚合结构。
- `crates/grey-tools/src/lib.rs`：新增 `LspTools` 与 `lsp_diagnostics` / `lsp_definition`。
- `crates/grey-cli/src/main.rs`：工具链组装时挂载 `LspTools`。
- `crates/grey-tools/tests/tools.rs`：新增 4 个 LSP 工具验证测试。
