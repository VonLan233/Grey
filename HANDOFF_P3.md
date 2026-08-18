# HANDOFF_P3: 多 Agent 编排与 MCP 接口（阶段性交接）

> 日期：2026-08-18  
> 状态：P3 原型实现完成，尚未进入阶段交付完成状态

## 一、交付摘要

本阶段完成了最小可运行的多 Agent 编排闭环（CLI 级）：

- `grey orchestrate` 子命令引入，支持并行 `--agent name:task` 子代理；
- 子代理默认角色：`researcher/coder/reviewer`，可覆盖自定义角色；
- 子代理结果统一按结构化 contract 解析（JSON 优先，明文 fallback）；
- 协调器基于子代理输出生成汇总 prompt 并返回最终合成；
- 文本/JSON 两种输出路径均可见；
- 子代理上下文隔离验证测试已补齐。

## 二、关键文件

- `crates/grey-cli/src/main.rs`
- `crates/grey-cli/tests/p2.rs`
- `docs/adr/ADR-003-p3-agent-orchestration.md`

## 三、完成情况（按 P3 任务）

| 子任务 | 状态 | 证据 |
|---|---|---|
| 子 Agent 并行执行 | ✅ | `run_orchestrate` + `join_all` |
| 子 Agent 协作回填 | ✅ | `build_coordinator_prompt` + `synthesis` |
| 结构化契约解析 | ✅ | `OrchestrateAgentContract` + `parse_orchestrate_contract` |
| 上下文隔离验证 | ✅ | `orchestrate_subagents_do_not_leak_other_agents_context` |
| MCP/Hook 接入复用 | ✅ | `build_agent_and_session` 与现有工具链复用 |

## 四、尚未完成（P3 to-do）

1. 子 Agent 共享上下文策略（显式白名单、摘要共享）；
2. 并发失败重试/降级策略细化（当前为基础超时+错误短路）；
3. 子 Agent contract 与主协调器输出的 schema 校验增强（如 JSON Schema）；
4. 子 agent 面板渲染（TUI）与会话化记忆持久化。

## 五、建议下一步

在不改动现有 `grey-core` 边界的前提下，先补齐 1)共享上下文白名单、2)子 Agent 失败恢复策略、3)P3 文档里勾选验收项，再切换到 P4 的 LSP 语义视图接入。
