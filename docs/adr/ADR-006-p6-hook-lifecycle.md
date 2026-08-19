# ADR-006: P6 Hook lifecycle v1 event expansion

- Status: Accepted
- Date: 2026-08-19

## Context

P6 要求扩展 `Hook` 生命周期，至少覆盖会话、消息发送、权限决策与完成事件。
当前实现只提供了 `pre_prompt` 与工具前后钩子。

## Decision

1. 在配置层补齐钩子事件入口

- 在 `HooksConfig` 中新增：
  - `session_start`
  - `pre_message_send`
  - `permission_decision`
  - `completion`
  - `session_end`
- 保留兼容的 `pre_prompt`, `pre_tool_call`, `post_tool_call`。

2. 统一命令执行模型

- CLI 使用统一的 hook 执行器，默认超时 `DEFAULT_HOOK_TIMEOUT_MS`。
- 以 JSON 结构体注入事件上下文（`event`, `prompt`, `provider`, `model`, `workspace`,
  `success`, `error` 等）。

3. 钩子能力分层

- `pre_message_send`/`pre_prompt`：可返回 JSON `{"prompt": "..."}` 或
  纯文本，按顺序改写 prompt。
- `permission_decision`：在内置审批后再执行，`{"approved": false}` 或
  `{"allow": false}` 可拒绝操作；非 0 命令退出也直接拒绝。
- `session_start`/`session_end`/`completion`：当前不阻塞主流程；失败仅记录 stderr。

4. 测试与验收

- 为新钩子入口补充 CLI 与工具层测试，覆盖 prompt 重写、会话开始/结束、
  completion 触发与权限决策拒绝。

## Consequences

- P6 生命周期钩子从配置层可直接控制，无需变更代码即可接入
  会话与消息发送相关策略。
- 仍保留旧配置/行为兼容，`pre_prompt` 行为不变。
- `permission_decision` 让副作用工具具备可扩展批准策略，
  同时不会绕过现有交互/自动批准器。
