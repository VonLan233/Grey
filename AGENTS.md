# 项目规范（AGENTS.md）

本文件是 通用的开发规范，如无特殊要求，所有 AI 工具与开发者提交代码时必须遵守。

## 工程质量优先原则

**用时间和 token 换取工程质量。不允许为了节省 token 而偷懒。**

### 反偷懒铁律

以下行为**绝对禁止**，违者即为偷懒：

1. **禁止跳过流程步骤** — 任何非平凡任务必须走完完整流程：brainstorming → design → plan → implement → review → verify。跳过任一环节 = 偷懒。
2. **禁止无证据断言** — 绝不可以说"测试通过"、"编译成功"、"服务正常"而不贴出实际运行输出。Claims require evidence, always.
3. **禁止代码摘要** — 不可以写"// 同上"、"similar to above"、"依此类推"来省略实际代码。每一步都必须给出完整可运行的代码。
4. **禁止跳过 Review** — Spec compliance review 和 Code quality review 缺一不可。Review 发现的问题必须修复并重新 review，不可跳过 re-review。
5. **禁止假执行** — 不要"假装"运行了命令。每次运行命令必须展示实际输出。如果命令会失败，展示失败并解释。
6. **禁止偷工减料的实现** — 必须按照 spec/plan 完整实现，不可自行删减需求。YAGNI 是去掉不需要的东西，不是去掉需要的东西。

### 流程纪律

- **Brainstorming 必须做** — 任何 feature/功能开发，无论多"简单"，必须先 brainstorming。看似简单的东西最容易因为未检查的假设而浪费大量时间。
- **Plan 必须完整** — 计划中每一个 task 必须包含精确的文件路径、完整的代码块、确切的运行命令和预期输出。TBD/TODO/占位符 = 计划失败。
- **TDD 默认开启** — 除非项目无测试框架，否则先写 failing test → 再写实现 → 验证 pass。
- **Review 不可跳过** — 每个 task 完成后：spec compliance review → fix → re-review → code quality review → fix → re-review。两阶段都要过。

### 验证纪律

- 每次声称"完成"前，必须运行验证命令并贴出输出。
- 每次声称"测试通过"前，必须实际运行测试并贴出结果。
- 每次声称"服务正常"前，必须 curl/wget 并展示 HTTP 状态码和响应。
- 不可仅凭 TypeScript 编译通过就声称"没问题"——lint、build、runtime 行为都需要验证。

### Token 预算态度

- **不设上限** — 用户明确表示愿意用时间和 token 换工程质量。宁可多花 token 把事做对，不可为了省 token 留下隐患。
- **多 agent 并行** — 独立的探索、review、验证任务应该并行派发 agent，而不是串行省 token。
- **完整 review** — Review agent 必须完整读取所有改动文件，不可只看 diff 就下结论。
