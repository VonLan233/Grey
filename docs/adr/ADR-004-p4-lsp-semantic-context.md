# ADR-004: P4 LSP Semantic Context Injection and Budget-aware Dedup

## Status

- Status: Accepted
- Date: 2026-08-18

## Context

P4 已完成 LSP 工具链接入（`lsp_diagnostics`/`lsp_definition`/`lsp_references`/
`lsp_hover`/`lsp_rename`/`lsp_symbols`），但 `lsp_*` 的结构化结果直接进入工具
消息，导致两类问题：

1. 模型上下文里重复出现同文件路径的工具快照，难以形成“语义视图”；
2. 会话内重复调用会放大上下文体积，虽然有全局 token 预算，但无法区分“语义噪声”。

本 ADR 固定 `P4` 里语义视图的注入策略。

## Decision

1. 在 `Agent` 工具循环内增加 LSP 语义摘要注入。

   - 当工具结果 `result.success == true` 且 `result.output` 可解析为
     `tool/path/count/shown/truncated/compact` 的 compact JSON 时，agent 不再仅保留
     `ToolResult`。
   - 同时在 `messages` 中追加一条系统级 `System` 消息：
     - 前缀统一为 `[semantic-view]`；
     - 由 `tool` 与 `path` 命名（例如 `lsp_diagnostics`、`src/main.rs`）；
     - 同一轮运行内保留同一 `(tool, path)` 的最新摘要。

2. 在 `ContextManager::prepare` 增加语义视图去重与计数。

   - 每次 `prepare` 前扫描系统消息里的语义视图，按 `tool/path` 去重，保留最后出现
     的一条，移除历史重复。
   - 去重量写入 `ContextAudit.tool_outputs_deduplicated`。
   - 去重后继续执行既有分区裁剪、tool 输出截断、摘要和丢弃 oldest 流程。

3. 保持现有 `ContextAudit` 可见性。

   - 引入 `tool_outputs_deduplicated` 作为预算审计字段；保留既有 `tool_outputs_truncated`
     字段，支持区分“去噪”与“截断”。

4. 保证最小可行实现。

   - 复用 `ToolResult` 与现有 `ToolExecutor` 接口，不新增新的 provider/工具 trait；
   - 只在语义视图层面做轻量 compact 归一化，不改变底层工具返回格式；
   - 通过缓存键 `(tool, path)` 做去重，避免重复注入造成上下文膨胀。

## Consequences

- 上下文预算更稳定：重复路径工具调用不再重复喂给模型。
- 诊断与引用等 LSP 工具可以形成“文件级语义摘要”，减少逐条 raw JSON 注入。
- 去重计数使排障可观测：`ContextTrimmed` 事件能反映是否发生了语义层去噪。
- 需要后续再补齐的 P4 范围（文档图片/图片语义等）未受影响，本 ADR 仅约束
  `lsp_*` 会话注入与预算融合。
