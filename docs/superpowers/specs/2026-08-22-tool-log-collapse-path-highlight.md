# Grey TUI：工具日志折叠 + 裸路径高亮（设计文档）

日期：2026-08-22
状态：设计定稿，待实现

## 背景与需求

用户反馈两个视觉/交互问题（v0.1.1 之后）：

1. **工具日志路径列表折叠**：agent 运行 glob/grep 等工具时，`[tool:ok] glob` 后会列出大量文件路径，这些路径是白色、与 agent 生成的正文混在一起，滚动时难以区分。用户希望路径列表**默认折叠**成一行摘要（如 `[tool:ok] glob (5 files)`），需要时**展开/收起**。
2. **正文裸路径自动高亮**：agent 正文中直接提到的文件路径（未用反引号包裹，如 `src/main.rs`、`crates/grey-tui/src/lib.rs`）当前是白色，希望自动识别并高亮，与正文区分。

交互方式（用户确认）：**鼠标点击 + 键盘快捷键都要**（折叠块的展开/收起）。

## 现状分析

### 数据流
- `AppState.output: String` 是纯文本流，所有内容（header、用户输入、agent delta、工具日志）都 `append_output()` 累积。
- `reduce_agent_event()`（lib.rs:1621）处理事件：
  - `ToolStarted` → `append_output("\n[tool:start] {name} {args}\n")`
  - `ToolFinished` → `append_output("[tool:{outcome}] {name}\n{output}\n")`
- 渲染：
  - `update_viewport()`（lib.rs:1740）用 `Paragraph::new(markdown_text(&self.output)).wrap().line_count(width)` 算 `max_scroll`
  - `render()`（lib.rs:2090）用同样方式渲染 conversation
  - `markdown_text()`（lib.rs:2242）用 pulldown_cmark 把纯文本解析成 `Text`（带样式）

### 关键事实（已通过 pty 实测 + 代码验证）
- 工具日志 `[tool:ok] glob\n{path1}\n{path2}...` 逐行列出，白色，与正文混。
- 正文反引号文件名已是 prompt 色（青绿 `0x89ffcc`）——已有区分。
- 鼠标只有 `ScrollUp/ScrollDown` 事件，**没有点击事件**。
- `[tool:ok]` 会被 pulldown_cmark 解析为 `[` `tool:ok` `]`（触发 link 尝试），现有 markdown_text 会把 `[` `]` 渲染成 accent 色、`tool:ok` 渲染成正文色——视觉上 `[tool:ok]` 的方括号是青色。
- CJK wrap 已有 `wrap_input_line()`（lib.rs:860）。
- 主题 `RenderTheme`（lib.rs:90）含 accent/prompt/muted/error/success/warning 等色。

## 设计

### 1. 折叠机制（工具日志块）

**输出标记**：output 保持纯文本流，但工具日志块用不可见控制字符包裹，方便渲染前提取、定位块边界：

- 块开始标记：`\x01{id}\x01`（STX 包裹十进制块 id）
- 块结束标记：`\x02{id}\x02`
- 块 id 用自增 `u64`（`next_block_id`），每次 ToolStarted 分配。

具体写入（在 `reduce_agent_event` 中）：
```
ToolStarted → append_output("\n\x01{id}\x01[tool:start] {name} {args}\n")
ToolFinished → append_output("[tool:{outcome}] {name}\n{output}\x02{id}\x02\n")
```
其中 `id` 在 ToolStarted 分配并缓存到 `pending_block_id`，ToolFinished 使用。

**折叠状态**：`AppState.expanded_blocks: HashSet<u64>`。默认（新建块）不在集合中 = 折叠。

**折叠文本生成**：新增 `fn folded_output(&self) -> String`：
- 遍历 output，按 `\x01`/`\x02` 标记切块。
- 折叠块（不在 expanded_blocks）：替换为一行摘要 `▸ [tool:ok] {name} (N files)`，其中 N 是该块输出中非空行数（或所有行数）。
- 展开块：保留原文，但块标题行加 `▾` 前缀（方便用户看到状态）。
- 未闭合标记（transcript 截断导致）：容忍，按普通文本处理。

**摘要行格式**：`▸ [tool:ok] glob (5 files)`。展开状态标题行：`▾ [tool:ok] glob`。
（`▸`/`▾` 字符与现有标题前 `▍`、列表 `•` 风格一致。）

**替换渲染调用**：`update_viewport()` 和 `render()` 中，把 `markdown_text(&self.output)` 改为 `markdown_text(&self.folded_output())`。这样 max_scroll 基于折叠后的行数，滚动一致。

### 2. 点击命中检测

**新事件**：`InputMessage::Click { column, row }`，来自 crossterm `MouseEventKind::Down(Left)`。

**命中逻辑**：`fn handle_click(&mut self, column, row) -> bool`：
1. 用 `last_layout` 判断点击是否在 conversation 区域（`rect_contains`）。
2. 把屏幕行 `row` 转成 conversation 内相对行 `row - rect.y`，加 `self.scroll` 得到折叠文本的绝对行。
3. **重跑折叠+wrap 行号映射**：`fn fold_line_map(&self, width) -> Vec<(start_row, end_row, block_id)>`，遍历 `folded_output()` 的逻辑行，用 `wrap_input_line(line, width).len()` 累计每个摘要行的屏幕行范围。
4. 找到包含目标行的块 id，toggle 其展开状态，返回 true。

> ponytail: 点击时重算映射（O(n)），不缓存。终端宽度变化/滚动都会触发重算，命中逻辑与渲染严格一致；只有当频繁点击成为性能问题时再缓存映射。

**wrap 一致性**：折叠文本的 markdown 渲染会把一个 Paragraph 的多行文本合成一行 Text（SoftBreak 不换行，实际 render 用 `wrap(Wrap{trim:false})` 才会折行）。为简化映射，`fold_line_map` 使用**纯文本逻辑行**（按 `\n` 拆分 folded_output）+ `wrap_input_line`，与 `update_viewport` 的 `Paragraph.line_count(width)` 一致（ratatui 对 Paragraph 文本按宽度折行，与 wrap_input_line 行为等价——两者都按字符宽度折）。

**键盘快捷键**：新增 `TuiKeyBinding`（如默认 `Ctrl+E`）：toggle 最近一个块（`last_block_id`）。也支持 leader 组合。具体绑定待与用户确认键位，先用 `Ctrl+E`（无冲突，现有 Ctrl+P/N/C 等均不同）。

### 3. 裸路径高亮

**位置**：`markdown_text()` 的 `Event::Text` 分支（目前直接 `Span::styled(content, style)`）。

**路径识别**：`fn highlight_paths(content: &str, style: Style, path_style: Style) -> Vec<Span>`：
- 把 content 按空白（或非路径字符）拆 token。
- 判定 token 是路径：含 `/`（如 `crates/grey-core/src/lib.rs`、`src/main.rs`）**或** 以常见源码扩展名结尾（`.rs .py .ts .js .go .toml .json .md .lua .sh .zsh .yaml .yml .css .html .lisp .cpp .c .h`）且 token 不含空白。
- 排除纯数字/版本号（如 `v0.1.1`）：要求 token 含 `/`，或扩展名前有至少 1 个非标点字符。
- 路径 token 用 `path_style`（新主题色），其余保留原 style。

**主题色**：`RenderTheme` 加 `path: Color`。grey_storm 用橙黄 `0xf0c674`（醒目但区别于 prompt 青绿与正文白），其他 preset 用 warning（黄色）近似。

> 注意：只有 `Event::Text`（正文裸文本）做路径高亮；`Event::Code`（反引号内）已是 prompt 色，保持不动。

### 4. 交互汇总
| 操作 | 绑定 | 行为 |
|------|------|------|
| 点击折叠摘要行 | 鼠标左键 | toggle 该块展开/收起 |
| 折叠/展开 | `Ctrl+E`（默认） | toggle 最近一个工具块 |
| 滚动 | 滚轮（已有） | 不变 |

## 测试计划（TDD）

1. **折叠标记与文本生成**：
   - `reduce_agent_event(ToolStarted/Finished)` 后 output 含 `\x01...\x01` 包裹。
   - `folded_output()` 默认折叠 → 摘要行 `▸ [tool:ok] glob (N files)`，展开块 → `▾` 前缀+完整路径。
   - transcript 截断后未闭合标记容忍。
2. **折叠状态切换**：toggle 后 folded_output 变化；HashSet 成员。
3. **点击命中**：构造 output+折叠，`handle_click` 命中摘要行 toggle；非 conversation 区域忽略。
4. **路径高亮**：`highlight_paths` 对 `crates/a/b.rs`、`src/main.rs` 高亮；对 `v0.1.1`、普通句子不高亮；CJK 中文不高亮。
5. **渲染回归**：现有 53 个 TUI 测试保持全绿（folded_output 默认折叠不影响无工具日志的 output）。
6. **pty 实测**：真实对话，glob 输出折叠成一行；点击/快捷键展开；正文路径高亮。

## 风险与取舍
- `[tool:ok]` 的 `[` `]` 被 markdown 拆成 link 尝试——折叠摘要行视觉上 `[tool:ok]` 方括号会是 accent 色，但整体仍清晰可读，接受。
- 点击重算映射 O(n)，n 为折叠文本行数（通常数百行内），可接受。
- 摘要行 `(N files)` 的 N：按块输出中的行数计（含空行忽略）。
