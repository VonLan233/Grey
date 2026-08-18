# ADR-003: P3 Agent Orchestration and MCP Integration Boundary

## Context

Grey 已完成 P2 的 Provider/上下文/缓存/usage 能力。P3 目标是从单 Agent 交互升级为可编排多 Agent 的闭环，同时保持现有核心边界不被突破：

- `grey-core` 仍不依赖具体 Provider 实现；
- 现有 `Agent` 继续作为工具循环与 Provider 抽象单元；
- `grey-cli` 负责路由/编排与执行策略挂载。

## Decision

1. P3 编排不在 `grey-core` 层新增 Agent trait 新接口；改为 CLI 层的 Orchestrate 调度器（`grey-cli`）按任务并行实例化多个子 agent。
2. 子 agent 采用独立会话上下文运行（`read_only + no_save`），以避免上下文污染；主任务与合成结果由 coordinator 子循环串起来。
3. 子 agent 输出通过 JSON 合约（`status/summary/recommendations/risks/artifacts`）归一化，失败/降级时回退到明文提取。
4. 当前阶段将子 agent 的 MCP 与 Hook 沿用现有工具链，不在 CLI 层新增 MCP 协议栈；只补足子 agent 并发调度与结果结构化的稳定输出。
5. Orchestrate 结果在文本模式下以面板化方式展示子 agent 状态；会话默认持久化，`--no-save` 时保持不落盘。

## Consequences

- 低耦合：P3 可以在不改 `grey-core` 的情况下演进；
- 可扩展性：子 agent 的角色和任务是数据驱动（`--agent name:task`）；
- 可审计性：每个子 agent 和 coordinator 都产出可验证的结构化字段；
- 风险：当前实现仍未覆盖复杂协调算法（重试与共享上下文裁剪策略）与任务级资源仲裁。

## Status

- Orchestrate 子 agent 并行与协作：已实现（`crates/grey-cli/src/main.rs`）
- 子 agent 合约解析与容错：已实现（`crates/grey-cli/src/main.rs`，`parse_orchestrate_contract`）
- 上下文隔离验证测试：已实现（`crates/grey-cli/tests/p2.rs`）
- 子 agent 聚合输出：已实现（`crates/grey-cli/src/main.rs`）
- 子 agent 会话化记忆持久化：已实现（`crates/grey-cli/src/main.rs`）
