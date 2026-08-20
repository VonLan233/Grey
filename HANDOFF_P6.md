# HANDOFF_P6: 扩展系统与性能打磨（阶段性交接）

> 日期：2026-08-19
> 状态：P6 进行中（部分交付）

## 一、阶段内完成项

- Hook 生命周期扩展到 P6 约定事件（`session_start`、`pre_message_send`、`permission_decision`、`completion`、`session_end`）并与现有 `pre_prompt`、`pre_tool_call`、`post_tool_call` 并行。
- `[[plugins]]` 配置支持 `tool` 与 `hook` 两类类型，支持 `enabled`、`command`、`args`、`hook_event`、`timeout_ms`。
- Tool 插件接入：`grey-cli` 在工具链组装时加载 `PluginTools`，与内建工具/`lsp_tools`/MCP 工具统一组合。
- Hook 插件接入：`hook` 类型插件自动加入对应事件链（与命令级 Hook 共用同一配置流）。
- Loop/Goal 命令接入：`grey loop` 与 `grey goal` 已在 CLI 级可用，并在完成时触发完整 completion 钩子负载。
- Hook 决策入口补齐：权限决定可被 `permission_decision` 决绝，`pre_message_send`/`session_*`/`completion` 有实测执行路径。
- `grey plugins` 管理命令接入：`list/show/add/remove/enable/disable`，变更持久写回 `grey.toml`（含集成测试覆盖）。
- `HANDOFF_P6.md` 新建，开始记录 P6 交付边界与待办。

## 二、关键文件清单

- `crates/grey-core/src/config.rs`：`HooksConfig`、`PluginConfig`、`PluginKind`、环境变量展开、序列化/合并与解析测试。
- `crates/grey-tools/src/lib.rs`：`PluginTools`、`HookedApprover`、`PluginTools::new`、插件工具执行在 workspace 的子进程模型。
- `crates/grey-cli/src/main.rs`：CLI `loop/goal/plugins` 命令、Hook 事件与插件 Hook 合并、`run_repeater` 生命周期、`build_agent_and_session` 的工具链组装。
- `crates/grey-cli/tests/p2.rs`：新增/既有 P6 针对事件钩子与 loop/goal 行为测试。
- `crates/grey-tools/tests/tools.rs`：权限决策钩子与插件工具行为回归。
- `docs/adr/ADR-006-p6-hook-lifecycle.md`：P6 Hook 生命周期决策记录。
- `docs/阶段性开发文档.md`：P6 当前状态与已完成/待补齐项同步。
- `scripts/run-grey-p6-perf-gates.sh`：P6 性能基准门禁脚本（启动时间、内存、渲染 FPS、大仓库扫描）。
- `.github/workflows/ci.yml`：CI 门禁入口接入 `run-grey-p6-perf-gates.sh`。

## 三、当前验收证据

- Hook 事件路径：
  - `session_start`/`completion`/`session_end` 在 headless 模式下已执行并产生副作用文件的验证（`crates/grey-cli/tests/p2.rs`）。
  - `pre_prompt` / `pre_message_send` / `permission_decision` 在集成测试和工具测试中有覆盖。
- Loop/Goal 可执行性：
  - `grey_cli/tests/p2.rs` 中增加 `loop_mode_runs_and_reports_iteration_count` 与 `goal_mode_outputs_json_and_respects_no_stop_token_by_default`。
- 性能基准门禁：
  - `scripts/run-grey-p6-perf-gates.sh` 已接入 CI，并覆盖启动时延、内存、TUI 渲染帧率及 10k 文件扫描场景。
- 编译与门禁恢复（Rust 1.97.1）：
  - `RUSTC=~/.rustup/toolchains/1.97.1-aarch64-apple-darwin/bin/rustc ~/.rustup/toolchains/1.97.1-aarch64-apple-darwin/bin/cargo check -p grey-cli`

## 四、未完成项（P6 to-do）

- WASM 插件宿主能力（清单、版本、沙箱）
- Provider/Theme 插件运行时扩展
- [x] 插件安装/更新/卸载生命周期：CLI 已支持 `list/show/add/remove/enable/disable`，配置持久化与测试已补齐
- 性能基准门禁：
  - 已完成（脚本+CI 接入）
- P7 相关发布流程（文档、打包、发布清单）

## 五、交付建议

- 下一步优先补齐 WASM/Provider-Theme 运行时，再将 P6 状态从“进行中”切到“已交付”并补齐 `README` 中插件样例/配置说明。
