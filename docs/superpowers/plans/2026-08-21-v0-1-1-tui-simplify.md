# v0.1.1 TUI 简洁化改版 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 按人工审核意见把 Grey TUI 从边框面板改成 Pi 式无边框文档流：删全屏 splash 改 header、输入区自适应高度、Neovim 式 `/` 补全浮窗、Pi 式 footer 两端布局、按鼠标位置划分的滚轮语义。

**Architecture:** 全部改动集中在 `crates/grey-tui/src/lib.rs`（现有 3336 行）。Commands 注册表消除多处清单重复；CompletionPopup 是独立于渲染的纯逻辑状态机（`sync/filter/navigate/accept`），通过 `reduce_key` 接入；`render()` 去边框并重构 Layout，自适应输入高度与 footer 截断；滚轮通过记录上次渲染的区域 Rect 分流。TDD 每任务独立：失败测试→最小实现→绿灯→commit。

**Tech Stack:** Rust + ratatui 0.29 + crossterm + tokio + unicode-width，TestBackend 单测，现有 `cargo test --workspace` 门禁。

## Global Constraints

- Spec: `docs/superpowers/specs/2026-08-21-v0-1-1-tui-simplify-design.md` 全量有效；taste checklist 为 review 门禁。
- `cargo fmt --check` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo test --workspace` / `cargo build --release` 全绿才算完成。
- `ponytail:` 注释仅在有明确上限的简化处使用（如 40% 终端高度 clamp），其余不引入。
- 持久 UI 仅分隔线与 footer 各一行；无边框无区块标题；错误红仅在错误态出现。
- 每个任务结束前必须贴实际命令输出作为证据，不可假执行。

---

## File Structure

**Modified:**
- `crates/grey-tui/src/lib.rs` — 唯一代码改动文件，所有 7 个功能任务落在此文件内。任务顺序设计为增量可递交：1–3 补全闭环、4 header、5 自适应输入、6 去边框+footer 布局重构、7 滚轮分流。
- `crates/grey-core/src/config.rs` — 仅 Task 8 在 `default_tui_input_lines` 附近加 deprecated 注释与文档说明（不改解析行为）。
- `docs/plans/v0-1-1-tui.md` — Task 8 更新 backlog 状态。
- `README.md` — Task 8 微调 TUI 描述（如涉及 `input_lines` 示例与旧状态栏描述）。

**No new files.** 新增类型（`CommandSpec`、`CompletionPopup`、`CompletionItem`、`LayoutRects`、`format_tokens` 等）就地置于 `lib.rs` 内对应逻辑附近（紧随 `SlashCommand` / `AppState` / `render()`），与既有风格一致，避免为单文件项目新增模块。

**Module map (改后 `lib.rs` 新增/改动的逻辑分区，行号为改前参考):**

| 区域 | 行号附近 | 职责 |
|------|---------|------|
| `ratatui use` 顶部 | `31-38` | 新增 `Clear` 导入 |
| 常量 | `46-51` | `SCROLL_PAGE_LINES/MOUSE_SCROLL_LINES` 保留；新增 `INPUT_PROMPT` / `INPUT_PROMPT_WIDTH` / `COMPLETION_MAX_VISIBLE_ROWS` |
| `CommandSpec + COMMANDS` | 紧随 `SlashCommand` 之后 (`~500`) | 命令注册表 |
| `SlashCommand::parse` | `516` | 改为查表 |
| `CompletionItem + CompletionPopup` | 紧随 `COMMANDS` | 补全状态机 |
| `AppState` 字段 | `795` | 删 `show_splash`，新增 `popup: CompletionPopup`、`last_layout: Option<LayoutRects>`、`input_scroll_manual: bool`、`popup_rect: Option<Rect>` |
| `AppState` 方法 | `1090-1200` | 新增 `accept_completion`、`scroll_at`、`input_area_height`、`rect_contains`、`header_text`，改 `clear_output` 重插 header，改 `input_scroll` 受 `manual` 影响 |
| `run_agent_tui` | `1513` | 删 `show_splash = true` |
| `render()` | `1717` | 重构 Layout + 去边框 + 分隔线 |
| `render_footer` + helpers | 紧随 `render()` | `format_tokens` / `truncate_to_width` / `render_footer` |
| `render_completion_popup` | 紧随 `render_footer` | 浮窗渲染（`Clear` + 高亮选中） |
| `handle` 顶部 | `1655` | 新增 `LayoutRects` |
| `InputMessage` | `2242` | 加坐标 |
| `read_input` | `2292` | 鼠标事件传坐标 |
| `run_loop` | `1596` | `ScrollUp/Down` 携带坐标并路由到 `scroll_at` |
| `crates/grey-core/src/config.rs` | `506` | 注释 deprecated |

---

### Task 1: 斜杠命令注册表（重构，行为不变）

**Files:**
- Modify: `crates/grey-tui/src/lib.rs:489-560`（在 `SlashCommand` 枚举上方新增 `CommandSpec`，紧随其后定义 `COMMANDS`；重写 `SlashCommand::parse` 为查表）

**Interfaces:**
- Consumes: `SlashCommand::parse(&str) -> Self` 现有签名保持不变
- Produces: `struct CommandSpec { name, aliases, args_hint, description }` 与 `const COMMANDS: &[CommandSpec]`，供 Task 2 补全过滤直接复用；对外无新增公开 API

- [ ] **Step 1: 写失败测试 — 注册表完整性与别名解析**

在 `crates/grey-tui/src/lib.rs` 的 `mod tests` 末尾新增一个测试（利用既有 `key` 辅助仍保留，但本测试不依赖输入模拟）：

```rust
#[test]
fn command_registry_covers_every_parsed_command() {
    // 每个注册项的主名都能被 parse 识别为非 Unknown
    for spec in COMMANDS {
        let parsed = SlashCommand::parse(&format!("/{}", spec.name));
        assert_ne!(
            parsed,
            SlashCommand::Unknown(spec.name.to_string()),
            "spec `{}` not matched by parse",
            spec.name
        );
    }
    // 别名
    assert_eq!(SlashCommand::parse("/?"), SlashCommand::Help);
    assert_eq!(SlashCommand::parse("/exit"), SlashCommand::Quit);
    assert_eq!(SlashCommand::parse("/tokens"), SlashCommand::Usage);
    // 现有行为保持：大小写不敏感、前后空白容忍、缺参数 /model 仍识别为 Model { model: "" }
    assert_eq!(SlashCommand::parse("/HELP"), SlashCommand::Help);
    assert_eq!(SlashCommand::parse("  /clear  "), SlashCommand::Clear);
    assert_eq!(
        SlashCommand::parse("/model"),
        SlashCommand::Model { model: String::new() }
    );
    assert_eq!(
        SlashCommand::parse("/model gpt-5"),
        SlashCommand::Model { model: "gpt-5".into() }
    );
    // 未知命令仍 Unknown
    assert_eq!(
        SlashCommand::parse("/bogus"),
        SlashCommand::Unknown("bogus".into())
    );
}
```

- [ ] **Step 2: 运行测试确认失败（类型未定义）**

Run: `cargo test --manifest-path crates/grey-tui/Cargo.toml command_registry_covers_every_parsed_command -- --nocapture`
Expected: FAIL — `cannot find type CommandSpec / cannot find value COMMANDS`

- [ ] **Step 3: 实现注册表与查表化 parse**

在 `crates/grey-tui/src/lib.rs` 中，`SlashCommand` 枚举紧上方新增：

```rust
/// One `/` command: name, aliases, argument hint and description for the completion popup.
#[derive(Debug, Clone)]
struct CommandSpec {
    name: &'static str,
    aliases: &'static [&'static str],
    args_hint: &'static str,
    description: &'static str,
}

const COMMANDS: &[CommandSpec] = &[
    CommandSpec { name: "help", aliases: &["?"], args_hint: "", description: "显示帮助" },
    CommandSpec { name: "clear", aliases: &[], args_hint: "", description: "清空输出" },
    CommandSpec { name: "quit", aliases: &["exit"], args_hint: "", description: "退出 Grey" },
    CommandSpec { name: "model", aliases: &[], args_hint: "<name>", description: "切换模型（下一条生效）" },
    CommandSpec { name: "usage", aliases: &["tokens"], args_hint: "", description: "查看累积 token 用量" },
    CommandSpec { name: "status", aliases: &[], args_hint: "", description: "查看版本/模型/分支/token" },
    CommandSpec { name: "models", aliases: &[], args_hint: "", description: "列出可用模型" },
];
```

重写 `impl SlashCommand::parse`（保留原签名与空串/大小写行为）：

```rust
impl SlashCommand {
    fn parse(input: &str) -> Self {
        let body = input.strip_prefix('/').unwrap_or(input).trim();
        let (name, argument) = body.split_once(char::is_whitespace).unwrap_or((body, ""));
        let name = name.trim().to_ascii_lowercase();
        let spec = COMMANDS.iter().find(|spec| {
            spec.name == name || spec.aliases.contains(&name.as_str())
        });
        match spec.map(|spec| spec.name) {
            Some("help") => Self::Help,
            Some("clear") => Self::Clear,
            Some("quit") => Self::Quit,
            Some("model") => Self::Model { model: argument.trim().to_owned() },
            Some("usage") => Self::Usage,
            Some("status") => Self::Status,
            Some("models") => Self::Models,
            _ => Self::Unknown(name),
        }
    }
}
```

- [ ] **Step 4: 运行测试确认通过，现有测试仍绿**

Run: `cargo test --manifest-path crates/grey-tui/Cargo.toml command_registry_covers_every_parsed_command -- --nocapture`
Expected: PASS

Run: `cargo test --manifest-path crates/grey-tui/Cargo.toml slash_commands_dispatch_locally_or_switch_model -- --nocapture`
Expected: PASS（行为未变）

- [ ] **Step 5: Commit**

```bash
git add crates/grey-tui/src/lib.rs
git commit -m "refactor(tui): slash-command registry backed by COMMANDS table"
```

---

### Task 2: 补全状态机 CompletionPopup + reduce_key 接入

**Files:**
- Modify: `crates/grey-tui/src/lib.rs:560-820`（`COMMANDS` 之后新增 `CompletionItem` + `CompletionPopup` + `AppState` 新增字段 `popup` 与方法 `accept_completion`；`AppState` 字段区与 `Default`；`reduce_key` 大段接入；`apply_slash_command` 调用点关窗处理；文件顶部 AppState 定义附近）

**Interfaces:**
- Consumes: Task 1 的 `COMMANDS`，`AppState.available_models: Vec<String>`
- Produces: `CompletionPopup { items, selected, offset, open }` 的 `sync/select_next/select_prev/selected_item`；`AppState::accept_completion(&mut self)`；`reduce_key` 在 popup 开启时拦截 Up/Down/Ctrl+N/P/Tab/Esc/Enter，其余输入后统一 `sync`。后续 Task 3 的浮窗渲染消费 `state.popup`

- [ ] **Step 1: 写失败测试 — 补全过滤、导航、采纳**

在 `mod tests` 末尾新增：

```rust
fn with_popup_state(models: &[&str]) -> AppState {
    let mut state = AppState::default();
    state.available_models = models.iter().map(|s| s.to_string()).collect();
    state
}

#[test]
fn completion_popup_filters_by_prefix_and_wraps_navigation() {
    let mut popup = CompletionPopup::default();
    // 空前缀 "/" 展开全部命令
    popup.sync("/", &[]);
    assert!(popup.open);
    assert_eq!(popup.items.len(), COMMANDS.len());
    // 前缀 "he" 仅命中 help
    popup.sync("/he", &[]);
    assert!(popup.open);
    assert_eq!(popup.items.len(), 1);
    assert_eq!(popup.items[0].label, "/help");
    // 大小写不敏感
    popup.sync("/HE", &[]);
    assert_eq!(popup.items.len(), 1);
    // 别名 "?" 也命中 help
    let mut popup2 = CompletionPopup::default();
    popup2.sync("/?", &[]);
    assert!(popup2.open);
    assert!(popup2.items.iter().any(|item| item.label == "/help"));
    // 未知前缀不展开
    let mut empty = CompletionPopup::default();
    empty.sync("/bogus", &[]);
    assert!(!empty.open);
    assert!(empty.items.is_empty());
    // 含空白时不展开（已在编辑参数）
    let mut spaced = CompletionPopup::default();
    spaced.sync("/help foo", &[]);
    assert!(!spaced.open);
    // 导航循环
    let mut nav = CompletionPopup::default();
    nav.sync("/", &[]);
    let len = nav.items.len();
    assert!(len >= 2);
    assert_eq!(nav.selected, 0);
    nav.select_next();
    assert_eq!(nav.selected, 1);
    nav.select_prev();
    assert_eq!(nav.selected, 0);
    nav.select_prev();
    assert_eq!(nav.selected, len - 1);
}

#[test]
fn completion_accept_replaces_input_and_resyncs() {
    let mut state = with_popup_state(&[]);
    // 输入 "/mod" 应过滤出 /model 与 /models，选中首项
    for ch in "/mod".chars() {
        state.reduce_key(key(KeyCode::Char(ch)));
    }
    assert!(state.popup.open, "popup should be open for /mod");
    let replaces = state.popup.selected_item().unwrap().replaces.clone();
    state.accept_completion();
    assert_eq!(state.input(), replaces);
    // model 命令采纳后应为 "/model " 并触发二级可补全状态
    assert_eq!(state.input(), "/model ");
}

#[test]
fn popup_keys_navigate_accept_and_dismiss() {
    let mut state = with_popup_state(&[]);
    for ch in "/".chars() {
        state.reduce_key(key(KeyCode::Char(ch)));
    }
    assert!(state.popup.open);
    // Down 导航
    state.reduce_key(key(KeyCode::Down));
    assert_eq!(state.popup.selected, 1);
    // Up 导航回 0
    state.reduce_key(key(KeyCode::Up));
    assert_eq!(state.popup.selected, 0);
    // Ctrl+N / Ctrl+P
    state.reduce_key(key_with(KeyCode::Char('n'), KeyModifiers::CONTROL));
    assert_eq!(state.popup.selected, 1);
    state.reduce_key(key_with(KeyCode::Char('p'), KeyModifiers::CONTROL));
    assert_eq!(state.popup.selected, 0);
    // Tab 采纳并关闭或重同步
    let before = state.popup.selected_item().unwrap().replaces.clone();
    state.reduce_key(key(KeyCode::Tab));
    assert_eq!(state.input(), before);
    // Esc 关闭
    for ch in "/h".chars() {
        state.reduce_key(key(KeyCode::Char(ch)));
    }
    // 新输入 "/h" 应重开
    // 清空后重建场景
    let mut state2 = with_popup_state(&[]);
    for ch in "/h".chars() {
        state2.reduce_key(key(KeyCode::Char(ch)));
    }
    assert!(state2.popup.open);
    state2.reduce_key(key(KeyCode::Esc));
    assert!(!state2.popup.open);
}

#[test]
fn enter_accepts_or_executes_exact_match() {
    let mut state = with_popup_state(&[]);
    for ch in "/hel".chars() {
        state.reduce_key(key(KeyCode::Char(ch)));
    }
    assert!(state.popup.open);
    // "/hel" + Enter 应采纳为 "/help" 而非执行
    assert_eq!(state.reduce_key(key(KeyCode::Enter)), UiAction::None);
    assert_eq!(state.input(), "/help");
    // 精确匹配 "/help" + Enter 应执行（Help 会打开 help）
    assert_eq!(state.reduce_key(key(KeyCode::Enter)), UiAction::None);
    assert!(state.show_help);
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path crates/grey-tui/Cargo.toml completion_popup_filters_by_prefix_and_wraps_navigation -- --nocapture`
Expected: FAIL — `cannot find type CompletionPopup` / `no method sync`

- [ ] **Step 3: 实现 CompletionItem + CompletionPopup + AppState 接入**

在 `COMMANDS` 常量之后新增：

```rust
/// One candidate row in the slash-command completion popup.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CompletionItem {
    /// Full new input text after accepting this item.
    replaces: String,
    /// Left column shown in the popup, e.g. `/model <name>`.
    label: String,
    /// Right column shown in the popup.
    description: String,
}

/// Neovim-style popup state for `/` command completion.
#[derive(Debug, Default)]
struct CompletionPopup {
    items: Vec<CompletionItem>,
    selected: usize,
    offset: usize,
    open: bool,
}

impl CompletionPopup {
    /// Recompute candidates from the current input; closes when nothing matches.
    fn sync(&mut self, input: &str, available_models: &[String]) {
        self.items.clear();
        self.selected = 0;
        self.offset = 0;
        self.open = false;
        // 二级：`/model <arg>` 的模型名补全（仅当 "/model " 后带空格时）
        if let Some(rest) = input.strip_prefix("/model ") {
            let argument = rest.trim_start();
            self.items = available_models
                .iter()
                .filter(|model| model.starts_with(argument))
                .map(|model| CompletionItem {
                    replaces: format!("/model {model}"),
                    label: format!("/model {model}"),
                    description: String::new(),
                })
                .collect();
            self.open = !self.items.is_empty();
            return;
        }
        // 一级：前缀过滤（大小写不敏感），仅在第一个词内触发
        if input.starts_with('/') && !input[1..].contains(char::is_whitespace) {
            let prefix = input[1..].to_ascii_lowercase();
            self.items = COMMANDS
                .iter()
                .filter(|spec| {
                    spec.name.starts_with(&prefix)
                        || spec.aliases.iter().any(|alias| alias.starts_with(&prefix))
                })
                .map(|spec| CompletionItem {
                    replaces: if spec.name == "model" {
                        "/model ".to_string()
                    } else {
                        format!("/{}", spec.name)
                    },
                    label: if spec.args_hint.is_empty() {
                        format!("/{}", spec.name)
                    } else {
                        format!("/{} {}", spec.name, spec.args_hint)
                    },
                    description: spec.description.to_string(),
                })
                .collect();
            self.open = !self.items.is_empty();
        }
    }

    fn select_next(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.items.len();
    }

    fn select_prev(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = (self.selected + self.items.len() - 1) % self.items.len();
    }

    fn selected_item(&self) -> Option<&CompletionItem> {
        self.items.get(self.selected)
    }
}
```

在 `pub struct AppState` 字段区新增（与现有字段并列，避免打散其它字段顺序）：

```rust
popup: CompletionPopup,
```

`impl Default for AppState` 无需手写，`CompletionPopup::default()` 已有，`AppState::default()` 的 `..Default::default()` 会自动初始化为关闭态。

在 `impl AppState` 中新增（紧随 `clear_output` / `apply_slash_command` 附近）：

```rust
fn accept_completion(&mut self) {
    let Some(item) = self.popup.selected_item() else {
        return;
    };
    self.input.text = item.replaces.clone();
    self.input.cursor_chars = self.input.text.chars().count();
    let text = self.input.text.clone();
    self.popup.sync(&text, &self.available_models);
    self.dirty = true;
}
```

修改 `reduce_key`（`crates/grey-tui/src/lib.rs:1188` 起）：

1. 在 `if self.leader_armed { ... }` 块**之后**，新增 popup 拦截分支：

```rust
if self.popup.open {
    match key.code {
        KeyCode::Up => {
            self.popup.select_prev();
            self.dirty = true;
            return UiAction::None;
        }
        KeyCode::Down => {
            self.popup.select_next();
            self.dirty = true;
            return UiAction::None;
        }
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            self.popup.select_prev();
            self.dirty = true;
            return UiAction::None;
        }
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            self.popup.select_next();
            self.dirty = true;
            return UiAction::None;
        }
        KeyCode::Tab => {
            self.accept_completion();
            return UiAction::None;
        }
        KeyCode::Esc => {
            self.popup.open = false;
            self.dirty = true;
            return UiAction::None;
        }
        KeyCode::Enter
            if !key.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
        {
            let exact = self
                .popup
                .selected_item()
                .is_some_and(|item| item.replaces == self.input.text);
            if exact {
                self.popup.open = false;
                // fall through to the normal Enter handling below
            } else {
                self.accept_completion();
                return UiAction::None;
            }
        }
        _ => {}
    }
}
```

2. 在 Enter 分支的斜杠命令分发前关闭 popup：

```rust
if self.input.text.starts_with('/') {
    self.popup.open = false;
    return self.apply_slash_command();
}
```

3. 在 Submit 分支 `let rejected_input = self.input.take();` 之后新增一行 `self.popup.open = false;`（`take()` 后空输入同步也会关，但提前 return 不走末尾 sync）。

4. 函数末尾原：

```rust
        self.dirty |= changed;
        UiAction::None
```

改为：

```rust
        self.dirty |= changed;
        if changed {
            let text = self.input.text.clone();
            self.popup.sync(&text, &self.available_models);
            if self.popup.open {
                self.dirty = true;
            }
        }
        UiAction::None
```

以上改动保持 `Up/Down` 在 popup 关闭时继续落到多行输入的 `move_up/move_down` 原逻辑。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --manifest-path crates/grey-tui/Cargo.toml completion_ -- --nocapture`
Expected: 三个新测试均 PASS

Run: `cargo test --manifest-path crates/grey-tui/Cargo.toml -- --nocapture 2>&1 | tail -20`
Expected: 全部测试 PASS（既有 `input_reducer_edits_submits_and_handles_every_exit_key` 等仍绿）

- [ ] **Step 5: Commit**

```bash
git add crates/grey-tui/src/lib.rs
git commit -m "feat(tui): neovim-style slash-command completion state machine"
```

---

### Task 3: 补全浮窗渲染 + /model 二级补全收尾

**Files:**
- Modify: `crates/grey-tui/src/lib.rs:1-40`（`ratatui::widgets::{..., Clear}` 导入，`use ratatui::layout::Rect` 已有；新增常量 `COMPLETION_MAX_VISIBLE_ROWS`）
- Modify: `crates/grey-tui/src/lib.rs:1717-2120`（`render()` 内新增浮窗渲染调用，新增 `render_completion_popup` 函数；AppState 新增 `popup_rect: Option<Rect>` 字段用于 Task 7 命中判定，此处先以 `None` 占位或直接实现 `render_completion_popup -> Option<Rect>`）

**Interfaces:**
- Consumes: Task 2 的 `state.popup`、`theme: &RenderTheme`
- Produces: `fn render_completion_popup(frame: &mut Frame<'_>, state: &mut AppState, theme: &RenderTheme, input_area: Rect) -> Option<Rect>`（返回 Some 时为浮窗实际 Rect，供 Task 7 复用）

- [ ] **Step 1: 写失败测试 — 浮窗在有候选时渲染于输入区上方并高亮选中**

在 `mod tests` 末尾新增：

```rust
#[test]
fn completion_popup_renders_above_input_and_highlights_selection() {
    let mut state = AppState::default();
    for ch in "/".chars() {
        state.reduce_key(key(KeyCode::Char(ch)));
    }
    assert!(state.popup.open);
    let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
    terminal.draw(|frame| render(frame, &mut state)).unwrap();
    let rows = rendered_rows(&terminal);
    // 浮窗内容应出现在输入区上方，且包含命令标签与描述
    assert!(
        rows.iter().any(|row| row.contains("/help")),
        "popup should render /help label, rows={rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("显示帮助")),
        "popup should render description"
    );
    // 选中项移动后仍可见
    state.reduce_key(key(KeyCode::Down));
    let mut terminal2 = Terminal::new(TestBackend::new(60, 12)).unwrap();
    terminal2.draw(|frame| render(frame, &mut state)).unwrap();
    let rows2 = rendered_rows(&terminal2);
    assert!(rows2.iter().any(|row| row.contains("/help")));
}

#[test]
fn model_secondary_completion_lists_available_models() {
    let mut state = AppState::default();
    state.available_models = vec!["gpt-5".into(), "claude-4".into(), "gemini-2".into()];
    // 输入 "/model " 触发二级补全
    for ch in "/model ".chars() {
        state.reduce_key(key(KeyCode::Char(ch)));
    }
    assert!(state.popup.open, "second-level popup should be open");
    assert_eq!(state.popup.items.len(), 3);
    // 前缀过滤
    state.reduce_key(key(KeyCode::Char('g')));
    assert_eq!(state.popup.items.len(), 2); // gpt-5, gemini-2
    // 采纳
    state.reduce_key(key(KeyCode::Tab));
    assert_eq!(state.input(), "/model gpt-5");
}
```

- [ ] **Step 2: 运行测试确认失败（无渲染）**

Run: `cargo test --manifest-path crates/grey-tui/Cargo.toml completion_popup_renders_above_input -- --nocapture`
Expected: FAIL — 断言 `row.contains("/help")` 为 false（尚未渲染浮窗）

- [ ] **Step 3: 实现渲染**

在文件顶部修改导入：

```rust
// Before:
widgets::{Block, Borders, Paragraph, Wrap},
// After:
widgets::{Block, Borders, Clear, Paragraph, Wrap},
```

在常量区（`const MOUSE_SCROLL_LINES` 之后）新增：

```rust
const COMPLETION_MAX_VISIBLE_ROWS: usize = 6;
```

在 `AppState` 字段区新增（与 `popup` 相邻）：

```rust
popup_rect: Option<Rect>,
```

并在 `impl Default for AppState` 中初始化为 `None`（通过 `..Default::default()` 已隐式为 None，无需手写）。

在 `render()` 函数末尾（`render_help_overlay` 之前）插入浮窗渲染（保持 help 最后以覆盖）：

```rust
let popup_rect = if state.popup.open {
    render_completion_popup(frame, state, &theme, chunks[2])
} else {
    None
};
state.popup_rect = popup_rect;
if state.show_help {
    render_help_overlay(frame, state, &theme);
}
```

注意 `chunks[2]` 是 Task 5 重构后输入区 Rect；在 Task 3 时点仍是 `Constraint::Length(input_lines)` 的输入区（已存在），两者都可用。

新增函数（置于 `render()` 之后、`markdown_text` 之前）：

```rust
fn render_completion_popup(
    frame: &mut Frame<'_>,
    state: &mut AppState,
    theme: &RenderTheme,
    input_area: Rect,
) -> Option<Rect> {
    let visible = state.popup.items.len().min(COMPLETION_MAX_VISIBLE_ROWS);
    if visible == 0 || input_area.width == 0 || input_area.height == 0 || input_area.y == 0 {
        return None;
    }
    // keep the selected row inside the visible window
    if state.popup.selected < state.popup.offset {
        state.popup.offset = state.popup.selected;
    } else if state.popup.selected >= state.popup.offset + visible {
        state.popup.offset = state.popup.selected + 1 - visible;
    }
    let width = state
        .popup
        .items
        .iter()
        .map(|item| {
            UnicodeWidthStr::width(item.label.as_str())
                + UnicodeWidthStr::width(item.description.as_str())
                + 4
        })
        .max()
        .unwrap_or(0)
        .clamp(12, usize::from(input_area.width)) as u16;
    let area = Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(visible as u16),
        width,
        height: visible as u16,
    };
    if area.y >= frame.area().height {
        return None;
    }
    frame.render_widget(Clear, area);
    let highlight = Style::default()
        .fg(theme.prompt)
        .add_modifier(Modifier::BOLD);
    let normal = Style::default().fg(theme.muted);
    let mut text = Text::default();
    for index in state.popup.offset..state.popup.offset + visible {
        let Some(item) = state.popup.items.get(index) else {
            break;
        };
        let style = if index == state.popup.selected {
            highlight
        } else {
            normal
        };
        let line = if item.description.is_empty() {
            Line::from(Span::styled(format!(" {}", item.label), style))
        } else {
            Line::from(Span::styled(
                format!(" {}  {}", item.label, item.description),
                style,
            ))
        };
        text.push_line(line);
    }
    frame.render_widget(Paragraph::new(text), area);
    Some(area)
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --manifest-path crates/grey-tui/Cargo.toml completion_ -- --nocapture`
Expected: 5 个 completion_* 测试均 PASS

Run: `cargo clippy --manifest-path crates/grey-tui/Cargo.toml -- -D warnings 2>&1 | tail -20`
Expected: 无警告

- [ ] **Step 5: Commit**

```bash
git add crates/grey-tui/src/lib.rs
git commit -m "feat(tui): render slash-command completion popup above input"
```

---

### Task 4: Header 替代全屏 splash

**Files:**
- Modify: `crates/grey-tui/src/lib.rs:795-830`（`AppState` 删 `show_splash: bool`，`Default` 删初始化）
- Modify: `crates/grey-tui/src/lib.rs:1513`（`run_agent_tui` 删 `state.show_splash = true`）
- Modify: `crates/grey-tui/src/lib.rs:1188`（`reduce_key` splash 分支整段删除）
- Modify: `crates/grey-tui/src/lib.rs:1717`（`render()` 开头 `if show_splash` 分支删除）
- Modify: `crates/grey-tui/src/lib.rs:2060`（删除 `render_splash` 整个函数与 `centered_rect` 若无他用——保留 `centered_rect` 因 `render_help_overlay` 仍在用）
- Modify: `crates/grey-tui/src/lib.rs:860-980`（`with_settings`/`with_runtime` 追加 header；`clear_output` 末尾重插 header；新增 `header_text`）

**Interfaces:**
- Produces: `fn header_text(settings: &TuiSettings) -> String`

- [ ] **Step 1: 写失败测试 — 启动 header 存在且 /clear 后重现**

在 `mod tests` 末尾新增：

```rust
#[test]
fn startup_header_precedes_transcript_and_survives_clear() {
    let header_state = AppState::with_settings(TuiSettings::default());
    assert!(
        header_state.output().starts_with("Grey v"),
        "header missing, output={:?}",
        header_state.output()
    );
    assert!(header_state.output().contains("帮助"));
    assert!(header_state.output().contains("/"));

    let mut cleared = AppState::with_settings(TuiSettings::default());
    cleared.append_output("old transcript\n");
    // 触发 /clear
    cleared.input.text = "/clear".into();
    cleared.input.cursor_chars = cleared.input.text.chars().count();
    assert_eq!(cleared.reduce_key(key(KeyCode::Enter)), UiAction::None);
    assert!(
        cleared.output().starts_with("Grey v"),
        "header should be re-inserted after /clear, output={:?}",
        cleared.output()
    );
    assert!(!cleared.output().contains("old transcript"));
}

#[test]
fn no_splash_state_or_render_path_remains() {
    // AppState 不再有 show_splash 字段，render 不应有全屏 splash 边框
    let mut state = AppState::with_settings(TuiSettings::default());
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| render(frame, &mut state)).unwrap();
    let rows = rendered_rows(&terminal);
    // 不应出现旧 splash 的方块头像行
    assert!(
        rows.iter().all(|row| !row.contains("▄▄▄▄▄▄▄")),
        "splash avatar should not be rendered"
    );
    // header 文本应在会话流内可见（非全屏居中面板）
    assert!(rows.iter().any(|row| row.contains("Grey v")));
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path crates/grey-tui/Cargo.toml startup_header_precedes_transcript_and_survives_clear -- --nocapture`
Expected: FAIL — `header missing`（尚未插入 header）

- [ ] **Step 3: 实现 — 删除 splash 并插入 header**

按文件自上而下修改：

1. 删 `pub struct AppState` 中的 `show_splash: bool,` 一行。
2. 删 `impl Default for AppState` 中的 `show_splash: false,`。
3. 删 `run_agent_tui` 中的 `state.show_splash = true;` 一行。
4. 删 `reduce_key` 开头的 splash 块：

```rust
// 删除整块：
        if self.show_splash {
            if self.settings.keys.quit.matches(key) {
                return UiAction::Quit;
            }
            self.show_splash = false;
            self.dirty = true;
            return UiAction::None;
        }
```

5. 删 `render()` 开头的 splash 块：

```rust
// 删除：
    if state.show_splash {
        render_splash(frame, state);
        return;
    }
```

6. 删除 `fn render_splash` 整个函数（保留 `centered_rect` 因 help overlay 仍用）。

7. 在 `impl AppState` 中 `with_settings` / `with_runtime` / `clear_output` 附近新增辅助与改动：

新增函数（置于 `render()` 上方或 `AppState` impl 内均可，置于 impl 外更清晰）：

```rust
fn header_text(settings: &TuiSettings) -> String {
    let labels = settings.keys.labels();
    format!(
        "Grey v{}\nEnter 发送 · Shift+Enter 换行 · / 命令 · {} {} 帮助\n\n",
        env!("CARGO_PKG_VERSION"),
        labels.leader,
        labels.help
    )
}
```

修改 `fn with_settings`：

```rust
fn with_settings(settings: TuiSettings) -> Self {
    let branch = settings.branch_label().map(str::to_string);
    let completion = CompletionSettings::from(settings.completion.clone());
    let mut state = Self {
        settings: settings.clone(),
        branch,
        status_error: false,
        completion,
        ..AppState::default()
    };
    state.append_output(&header_text(&state.settings));
    state
}
```

`with_runtime` 保持透传（内部调用 `with_settings` 已插入 header，无需二次）。

修改 `fn clear_output` 末尾追加一行：

```rust
fn clear_output(&mut self) {
    self.clear_completion_notice();
    self.output.clear();
    self.pending_completion_bell = None;
    self.status = "output cleared".into();
    self.scroll = 0;
    self.max_scroll = 0;
    self.status_error = false;
    self.dirty = true;
    self.append_output(&header_text(&self.settings));
}
```

8. 同步调整既有测试中对 `AppState::default()` 构造后 `output.is_empty()` 的断言：`slash_commands_dispatch_locally_or_switch_model` 内：

```rust
// 原：
        assert!(clear.output.is_empty());
// 改为：
        assert!(
            clear.output().starts_with("Grey v"),
            "clear should leave header"
        );
        assert!(!clear.output().contains("old transcript"));
```

若其它测试也断言 `output.is_empty()`，同理改为 starts_with header。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --manifest-path crates/grey-tui/Cargo.toml startup_header_precedes_transcript_and_survives_clear -- --nocapture`
Expected: PASS

Run: `cargo test --manifest-path crates/grey-tui/Cargo.toml slash_commands_dispatch_locally_or_switch_model -- --nocapture`
Expected: PASS

Run: `cargo test --manifest-path crates/grey-tui/Cargo.toml no_splash -- --nocapture`
Expected: PASS

Run: `cargo test --manifest-path crates/grey-tui/Cargo.toml splash -- --nocapture`
Expected: 编译期无 `splash_renders_and_dismisses_on_any_key`（已删除）或 0 tests；若仍存在旧测试引用 `show_splash` 则编译失败需同步删除旧测试 `splash_renders_and_dismisses_on_any_key`

补删旧测试：删除 `#[test] fn splash_renders_and_dismisses_on_any_key` 整个函数。

Run: `cargo test --manifest-path crates/grey-tui/Cargo.toml -- --nocapture 2>&1 | tail -20`
Expected: 全绿

- [ ] **Step 5: Commit**

```bash
git add crates/grey-tui/src/lib.rs
git commit -m "feat(tui): replace splash screen with inline header"
```

---

### Task 5: 输入区自适应高度

**Files:**
- Modify: `crates/grey-tui/src/lib.rs:171-212`（`TuiSettings` 删 `layout: TuiLayoutConfig` 字段，`From<&TuiConfig>` 删 layout 初始化，顶部 `use grey_core::TuiLayoutConfig` 删除，顶部 `use grey_core::{..., TuiLayoutConfig}` 移除该项；`TUI_INPUT_LINES_MIN/MAX` 常量保留供校验函数但 render 不再读取）
- Modify: `crates/grey-tui/src/lib.rs:1717`（`render()` 内输入高度计算改为内容驱动 clamp）
- Modify: `crates/grey-tui/src/lib.rs:2726`（测试 `tui_settings_apply_tui_config_layout_and_completion` 删 layout 断言相关行）

**Interfaces:**
- Produces: `fn input_area_height(visual_rows: usize, frame_height: u16) -> u16`（纯函数，可单测）

- [ ] **Step 1: 写失败测试 — 输入高度随内容增长并在 40% 上限 clamp**

在 `mod tests` 末尾新增：

```rust
#[test]
fn input_area_height_clamps_to_frame_40_percent() {
    assert_eq!(input_area_height(0, 20), 1);
    assert_eq!(input_area_height(1, 20), 1);
    assert_eq!(input_area_height(5, 20), 5);
    // 20 行终端 40% = 8
    assert_eq!(input_area_height(12, 20), 8);
    assert_eq!(input_area_height(8, 20), 8);
    // 极小终端
    assert_eq!(input_area_height(10, 1), 1);
    assert_eq!(input_area_height(10, 5), 2);
}

#[test]
fn input_area_grows_with_content_and_scrolls_beyond_max() {
    let mut state = AppState::with_settings(TuiSettings::default());
    // 单行内容 → 会话区可见
    state.input.text = "hello".into();
    state.input.cursor_chars = 5;
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    terminal.draw(|frame| render(frame, &mut state)).unwrap();
    let rows = rendered_rows(&terminal);
    let sep_index = rows.iter().position(|row| row.starts_with('─')).expect("separator");
    let prompt_index = rows.iter().position(|row| row.contains("> ")).expect("prompt");
    assert_eq!(prompt_index, sep_index + 1, "single line input right below separator");

    // 多行内容（逻辑换行 + 视觉换行）应使输入区长高
    let mut tall = AppState::with_settings(TuiSettings::default());
    tall.input.text = (0..12).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
    tall.input.cursor_chars = tall.input.text.chars().count();
    let mut terminal2 = Terminal::new(TestBackend::new(40, 14)).unwrap();
    terminal2.draw(|frame| render(frame, &mut tall)).unwrap();
    let rows2 = rendered_rows(&terminal2);
    let sep_index2 = rows2.iter().position(|row| row.starts_with('─')).expect("separator");
    // 40% * 14 = 5（整数除），实际输入区高度应为 5 且内部可滚
    assert!(sep_index2 < 8, "separator moved up because input grew");
    // 超长时输入区被 clamp 到 40%，内容通过 scroll 可见
    assert!(tall.input_scroll > 0 || rows2.iter().any(|row| row.contains("line")), "overflow scroll");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path crates/grey-tui/Cargo.toml input_area_height -- --nocapture`
Expected: FAIL — `cannot find function input_area_height`

- [ ] **Step 3: 实现**

在常量区或 `wrap_input_line` 附近新增纯函数：

```rust
/// Input area height: content-driven, clamped to 40% of the terminal height.
/// `ponytail: clamp upper bound is 40% of frame; raise only when users request more visible input rows.`
fn input_area_height(visual_rows: usize, frame_height: u16) -> u16 {
    let max = (u32::from(frame_height) * 40 / 100).max(1) as u16;
    (visual_rows as u16).clamp(1, max)
}
```

修改 `TuiSettings`：

```rust
// Before:
struct TuiSettings {
    theme: TuiTheme,
    completion: TuiCompletionConfig,
    keys: TuiKeyBindings,
    layout: TuiLayoutConfig,
}
// After:
struct TuiSettings {
    theme: TuiTheme,
    completion: TuiCompletionConfig,
    keys: TuiKeyBindings,
}
```

`impl From<&TuiConfig> for TuiSettings` 删除：

```rust
            layout: TuiLayoutConfig {
                input_lines: config
                    .layout
                    .input_lines
                    .clamp(TUI_INPUT_LINES_MIN, TUI_INPUT_LINES_MAX),
            },
```

文件顶部 `use grey_core::{..., TuiLayoutConfig}` 移除 `TuiLayoutConfig`。

修改 `render()` 内高度计算：

```rust
// 删除：
//    let input_lines = state.settings.layout.input_lines.max(1);
    let prompt_width = UnicodeWidthStr::width("> ");
    let frame_width = usize::from(frame.area().width);
    let visual_rows = state.input_visual_lines(frame_width, prompt_width).len();
    let input_height = input_area_height(visual_rows, frame.area().height);
    let chunks = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(input_height),
        Constraint::Length(1),
    ])
    .split(frame.area());
```

测试 `tui_settings_apply_tui_config_layout_and_completion` 中删除：

```rust
        config.layout.input_lines = 6;
        assert_eq!(settings.layout.input_lines, 6);
```

改为仅保留 completion 相关断言：

```rust
    #[test]
    fn tui_settings_apply_tui_config_layout_and_completion() {
        let mut config = TuiConfig::default();
        config.completion.enabled = false;
        let settings = TuiSettings::from(&config);
        assert!(!settings.completion.enabled);
    }
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --manifest-path crates/grey-tui/Cargo.toml input_area_ -- --nocapture`
Expected: PASS

Run: `cargo test --manifest-path crates/grey-tui/Cargo.toml -- --nocapture 2>&1 | tail -20`
Expected: 全绿

- [ ] **Step 5: Commit**

```bash
git add crates/grey-tui/src/lib.rs
git commit -m "feat(tui): adaptive input height clamped to 40% of frame"
```

---

### Task 6: 视觉简化 — 去边框、分隔线、Footer 两端布局

**Files:**
- Modify: `crates/grey-tui/src/lib.rs:1717-2060`（`render()` 全面重构：会话区/输入区去 `Block` 边框，分隔线 `─`，调用 `render_footer`；新增 `format_tokens` / `truncate_to_width` / `render_footer`；删除 `render_task_line` / `render_status_line`；`render_help_overlay` 保留）

**Interfaces:**
- Produces: `fn format_tokens(u64) -> String`, `fn truncate_to_width(&str, usize) -> String`, `fn render_footer(Frame, &AppState, &RenderTheme, Rect)`

- [ ] **Step 1: 写失败测试 — 无边框、分隔线、footer 两端布局与截断**

在 `mod tests` 末尾新增：

```rust
#[test]
fn format_tokens_abbreviates_thousands() {
    assert_eq!(format_tokens(0), "0");
    assert_eq!(format_tokens(999), "999");
    assert_eq!(format_tokens(1000), "1.0k");
    assert_eq!(format_tokens(1532), "1.5k");
    assert_eq!(format_tokens(10000), "10.0k");
}

#[test]
fn truncate_to_width_keeps_within_limit() {
    assert_eq!(truncate_to_width("hello", 10), "hello");
    assert_eq!(truncate_to_width("hello", 5), "hello");
    let truncated = truncate_to_width("hello world", 8);
    assert!(UnicodeWidthStr::width(truncated.as_str()) <= 8);
    assert!(truncated.ends_with('…'));
}

#[test]
fn footer_shows_usage_model_branch_two_end_layout() {
    let mut state = AppState::with_settings(TuiSettings::default());
    state.total_input_tokens = 1500;
    state.total_output_tokens = 42;
    state.current_provider = Some("openai".into());
    state.current_model = Some("gpt-5".into());
    state.branch = Some("main".into());
    let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
    terminal.draw(|frame| render(frame, &mut state)).unwrap();
    let rows = rendered_rows(&terminal);
    let footer = rows.last().unwrap();
    assert!(footer.contains("↑1.5k ↓42"), "left usage stats, footer={footer:?}");
    assert!(
        footer.contains("(openai) gpt-5 (main)"),
        "right identity, footer={footer:?}"
    );
}

#[test]
fn footer_truncates_right_side_on_narrow_terminal() {
    let mut state = AppState::with_settings(TuiSettings::default());
    state.current_provider = Some("openai".into());
    state.current_model = Some("a-very-long-model-name-that-overflows".into());
    state.branch = Some("main".into());
    let mut terminal = Terminal::new(TestBackend::new(30, 10)).unwrap();
    terminal.draw(|frame| render(frame, &mut state)).unwrap();
    let rows = rendered_rows(&terminal);
    let footer = rows.last().unwrap();
    assert!(footer.contains('…'), "right side truncated, footer={footer:?}");
    assert!(footer.contains("↑0 ↓0"), "left side kept");
}

#[test]
fn conversation_has_no_borders_and_separator_divides_input() {
    let mut state = AppState::with_settings(TuiSettings::default());
    state.append_output("hello transcript");
    let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
    terminal.draw(|frame| render(frame, &mut state)).unwrap();
    let rows = rendered_rows(&terminal);
    assert!(
        rows.iter().all(|row| !row.contains('┌') && !row.contains('┐')),
        "no box-drawing borders, rows={rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.starts_with('─')),
        "separator line, rows={rows:?}"
    );
    assert!(
        rows.iter().all(|row| !row.contains(" fps") && !row.contains("GREY")),
        "status decluttered"
    );
}
```

同时，旧测试 `task_line_sits_above_input_and_status_is_decluttered` 的语义与新布局不再一致，需同步更新 — 在 Step 3 将其重写为对 footer/header 的断言（见 Step 3 说明）。

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path crates/grey-tui/Cargo.toml footer_shows_usage -- --nocapture`
Expected: FAIL — `cannot find function format_tokens` / `render produces bordered layout`

- [ ] **Step 3: 实现**

在 `render()` 上方或下方新增纯函数（置于 `render()` 之后紧邻）：

```rust
fn format_tokens(count: u64) -> String {
    if count < 1000 {
        count.to_string()
    } else {
        format!("{:.1}k", count as f64 / 1000.0)
    }
}

fn truncate_to_width(text: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    let mut truncated = String::new();
    let mut used = 1usize; // reserve one column for the ellipsis
    for character in text.chars() {
        let width = UnicodeWidthStr::width(character.to_string().as_str());
        if used + width > max_width {
            break;
        }
        truncated.push(character);
        used += width;
    }
    format!("{truncated}…")
}

fn render_footer(frame: &mut Frame<'_>, state: &AppState, theme: &RenderTheme, area: Rect) {
    let dim = Style::default().fg(theme.muted);
    let (input_tokens, output_tokens) = state.total_usage();
    let mut left_spans = vec![Span::styled(
        format!("↑{} ↓{}", format_tokens(input_tokens), format_tokens(output_tokens)),
        dim,
    )];
    if let Some(task) = state.current_task.as_deref() {
        left_spans.push(Span::styled(format!(" · task:{task}"), dim));
    }
    if state.status_has_error() {
        left_spans.push(Span::styled(
            " ERR",
            Style::default()
                .fg(theme.error)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let left_width: usize = left_spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    let (provider, model_label) = state
        .model_info()
        .map_or((None, "-".to_string()), |(provider, model)| {
            (Some(provider.to_string()), model.to_string())
        });
    let mut right = match provider {
        Some(provider) => format!("({provider}) {model_label}"),
        None => model_label,
    };
    if let Some(branch) = state.branch.as_deref() {
        right.push_str(&format!(" ({branch})"));
    }
    let width = usize::from(area.width);
    let min_gap = 2usize;
    let right_width = UnicodeWidthStr::width(right.as_str());
    let right_final = if left_width + min_gap + right_width <= width {
        right
    } else {
        let available = width.saturating_sub(left_width + min_gap);
        truncate_to_width(&right, available)
    };
    let right_actual_width = UnicodeWidthStr::width(right_final.as_str());
    let padding = width.saturating_sub(left_width + right_actual_width);
    let mut spans = left_spans;
    spans.push(Span::raw(" ".repeat(padding)));
    spans.push(Span::styled(right_final, dim));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}
```

重写 `render()`：

```rust
fn render(frame: &mut Frame<'_>, state: &mut AppState) {
    let theme = state.settings.theme.colors.clone();
    let prompt_width = UnicodeWidthStr::width("> ");
    let frame_width = usize::from(frame.area().width);
    let visual_rows = state.input_visual_lines(frame_width, prompt_width).len();
    let input_height = input_area_height(visual_rows, frame.area().height);
    let chunks = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(input_height),
        Constraint::Length(1),
    ])
    .split(frame.area());

    // 0: 会话区 — 无边框纯 markdown 流
    state.update_viewport(chunks[0].width, chunks[0].height);
    let conversation = Paragraph::new(markdown_text(state.output.as_str(), &theme))
        .wrap(Wrap { trim: false })
        .scroll((state.scroll, 0));
    frame.render_widget(conversation, chunks[0]);

    // 1: 分隔线
    let separator = Paragraph::new(Line::from(Span::styled(
        "─".repeat(usize::from(chunks[1].width)),
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(separator, chunks[1]);

    // 2: 输入区 — 无边框
    let input_inner = chunks[2];
    let prompt = Span::styled(
        "> ",
        Style::default()
            .fg(theme.prompt)
            .add_modifier(Modifier::BOLD),
    );
    let input_width = usize::from(input_inner.width);
    let input_visible_rows = usize::from(input_inner.height);
    // input_scroll 的自动跟随受 manual 标志影响（Task 7 引入，Task 6 复用该分支）
    if !state.input_scroll_manual {
        state.input_scroll(input_width, prompt_width, input_visible_rows);
    }
    let mut input_text = Text::default();
    for (index, visual_line) in state
        .input_visual_lines(input_width, prompt_width)
        .iter()
        .enumerate()
    {
        if index == 0 {
            input_text.push_line(Line::from(vec![
                prompt.clone(),
                Span::raw(visual_line.clone()),
            ]));
        } else {
            input_text.push_line(Line::from(Span::raw(visual_line.clone())));
        }
    }
    let input = Paragraph::new(input_text)
        .style(Style::default().fg(Color::White))
        .scroll((state.input_scroll, 0));
    frame.render_widget(input, chunks[2]);
    if input_inner.width > 0 && input_inner.height > 0 {
        let (cursor_column, cursor_row) = state.input_cursor_position(input_width, prompt_width);
        let cursor_row = cursor_row.saturating_sub(usize::from(state.input_scroll));
        let cursor_y = input_inner.y.saturating_add(
            cursor_row.min(usize::from(input_inner.height.saturating_sub(1))) as u16,
        );
        let cursor_x = input_inner.x.saturating_add(
            (prompt_width + cursor_column).min(usize::from(input_inner.width.saturating_sub(1)))
                as u16,
        );
        frame.set_cursor_position((cursor_x, cursor_y));
    }

    // 3: footer
    render_footer(frame, state, &theme, chunks[3]);

    // 浮窗（若开）— Task 3 的调用在 Task 6 时已存在，保持在此
    let popup_rect = if state.popup.open {
        render_completion_popup(frame, state, &theme, chunks[2])
    } else {
        None
    };
    state.popup_rect = popup_rect;
    state.last_layout = Some(LayoutRects {
        conversation: chunks[0],
        input: chunks[2],
        popup: popup_rect,
    });

    if state.show_help {
        render_help_overlay(frame, state, &theme);
    }
}
```

注意：此 `render()` 已包含 Task 3 的 `popup_rect`/`last_layout` 写入与 Task 7 的 `input_scroll_manual` 检查，向后兼容 — Task 7 时扩展即可，无需二次重构。

删除旧函数：

```rust
// 删除整段：
fn render_task_line(frame: &mut Frame<'_>, state: &AppState, theme: &RenderTheme, area: Rect) { ... }
// 删除整段：
fn render_status_line(frame: &mut Frame<'_>, state: &AppState, theme: &RenderTheme, area: Rect) { ... }
```

同步更新旧测试 `task_line_sits_above_input_and_status_is_decluttered`：替换为对 header/footer 分隔关系的新断言，或直接删除该测试（其断言已被本任务新测试覆盖）。建议**删除并以新测试替代**：

```bash
# 删除：
    #[test]
    fn task_line_sits_above_input_and_status_is_decluttered() { ... }
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --manifest-path crates/grey-tui/Cargo.toml footer_ format_tokens truncate_to_width conversation_has_no_borders -- --nocapture`
Expected: 均 PASS

Run: `cargo test --manifest-path crates/grey-tui/Cargo.toml -- --nocapture 2>&1 | tail -20`
Expected: 全绿

Run: `cargo clippy --manifest-path crates/grey-tui/Cargo.toml -- -D warnings 2>&1 | tail -10`
Expected: 无警告

- [ ] **Step 5: Commit**

```bash
git add crates/grey-tui/src/lib.rs
git commit -m "feat(tui): borderless layout with separator and pi-style footer"
```

---

### Task 7: 滚轮按区域分流

**Files:**
- Modify: `crates/grey-tui/src/lib.rs:2242`（`enum InputMessage` 变体加坐标）
- Modify: `crates/grey-tui/src/lib.rs:2285-2320`（`read_input` 鼠标事件传 `column/row`）
- Modify: `crates/grey-tui/src/lib.rs:1573-1620`（`run_loop` 匹配携带坐标并路由到 `scroll_at`）
- Modify: `crates/grey-tui/src/lib.rs:795-860`（`AppState` 已在 Task 3/5 引入 `last_layout/popup_rect/input_scroll_manual`，此处新增 `LayoutRects` 定义与 `scroll_at`/`rect_contains` 实现；`input_scroll` 受 `manual` 影响；编辑路径重置 `manual`）

**Interfaces:**
- Consumes: Task 3/6 的 `popup_rect` / `last_layout` / `popup`
- Produces: `fn scroll_at(&mut self, column: u16, row: u16, lines: i16)`，`struct LayoutRects`，`fn rect_contains(Rect, u16, u16) -> bool`

- [ ] **Step 1: 写失败测试 — 三区域滚轮分流与输入溢出/穿透**

在 `mod tests` 末尾新增：

```rust
#[test]
fn wheel_routes_by_pointer_region_and_input_overflow() {
    // 会话区滚动
    let mut state = AppState::with_settings(TuiSettings::default());
    state.total_input_tokens = 0;
    state.total_output_tokens = 0;
    state.last_layout = Some(LayoutRects {
        conversation: Rect { x: 0, y: 0, width: 40, height: 6 },
        input: Rect { x: 0, y: 7, width: 40, height: 2 },
        popup: None,
    });
    state.scroll = 0;
    state.max_scroll = 20;
    state.scroll_at(5, 2, MOUSE_SCROLL_LINES);
    assert_eq!(state.scroll, 3, "wheel over conversation scrolls transcript");

    // 输入区未溢出 → 穿透给会话区
    state.input.text = "hi".into();
    state.input.cursor_chars = 2;
    state.input_scroll = 0;
    state.input_scroll_manual = false;
    state.scroll_at(5, 8, MOUSE_SCROLL_LINES);
    assert_eq!(state.scroll, 6, "wheel over non-overflow input falls through");
    assert_eq!(state.input_scroll, 0);

    // 输入区溢出 → 滚动输入内容本身
    let mut over = AppState::with_settings(TuiSettings::default());
    over.input.text = (0..6).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
    over.input.cursor_chars = over.input.text.chars().count();
    over.last_layout = Some(LayoutRects {
        conversation: Rect { x: 0, y: 0, width: 40, height: 6 },
        input: Rect { x: 0, y: 7, width: 40, height: 2 },
        popup: None,
    });
    over.scroll = 5;
    over.max_scroll = 10;
    over.input_scroll = 0;
    over.input_scroll_manual = false;
    over.scroll_at(5, 8, MOUSE_SCROLL_LINES); // down over input
    assert_eq!(over.input_scroll, 3, "wheel over overflow input scrolls input");
    assert_eq!(over.scroll, 5, "conversation untouched");
    // 上滚回到顶部
    over.scroll_at(5, 8, -MOUSE_SCROLL_LINES);
    assert_eq!(over.input_scroll, 0);
}

#[test]
fn wheel_over_popup_navigates_selection() {
    let mut state = AppState::with_settings(TuiSettings::default());
    state.available_models = vec!["a".into(), "b".into(), "c".into()];
    for ch in "/".chars() {
        state.reduce_key(key(KeyCode::Char(ch)));
    }
    assert!(state.popup.open);
    let popup_rect = Rect { x: 0, y: 4, width: 20, height: 3 };
    state.last_layout = Some(LayoutRects {
        conversation: Rect { x: 0, y: 0, width: 40, height: 4 },
        input: Rect { x: 0, y: 7, width: 40, height: 2 },
        popup: Some(popup_rect),
    });
    assert_eq!(state.popup.selected, 0);
    state.scroll_at(2, 5, MOUSE_SCROLL_LINES); // down over popup → next
    assert_eq!(state.popup.selected, 1);
    state.scroll_at(2, 5, -MOUSE_SCROLL_LINES); // up over popup → prev
    assert_eq!(state.popup.selected, 0);
}

#[test]
fn editing_resets_input_manual_scroll() {
    let mut state = AppState::with_settings(TuiSettings::default());
    state.input_scroll_manual = true;
    state.reduce_key(key(KeyCode::Char('a')));
    assert!(!state.input_scroll_manual, "editing should reset manual flag");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path crates/grey-tui/Cargo.toml wheel_routes_by_pointer_region -- --nocapture`
Expected: FAIL — `no method scroll_at` / `no field last_layout`

- [ ] **Step 3: 实现 — InputMessage、read_input、run_loop、AppState 方法**

在顶部常量附近新增：

```rust
const INPUT_PROMPT: &str = "> ";
const INPUT_PROMPT_WIDTH: usize = 2; // matches "> ".width()
```

在 `AppState` 字段区（Task 3/5 已引入以下三项，若尚未则新增；若已在则跳过重复新增，保留一处）：

```rust
popup: CompletionPopup,
popup_rect: Option<Rect>,
last_layout: Option<LayoutRects>,
input_scroll_manual: bool,
```

在 `render()` 上方新增：

```rust
#[derive(Debug, Clone, Copy)]
struct LayoutRects {
    conversation: Rect,
    input: Rect,
    popup: Option<Rect>,
}

fn rect_contains(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}
```

在 `impl AppState` 中新增 `scroll_at`（紧随 `scroll_mouse`）：

```rust
/// Dispatch a mouse scroll to the region under the pointer; falls back to the
/// conversation scroll when the pointer is over no known region.
fn scroll_at(&mut self, column: u16, row: u16, lines: i16) {
    if let Some(rects) = self.last_layout {
        if let Some(popup) = rects.popup {
            if rect_contains(popup, column, row) {
                if lines < 0 {
                    self.popup.select_prev();
                } else {
                    self.popup.select_next();
                }
                self.dirty = true;
                return;
            }
        }
        if rect_contains(rects.input, column, row) {
            let visual_rows =
                self.input_visual_lines(usize::from(rects.input.width), INPUT_PROMPT_WIDTH)
                    .len();
            if visual_rows > usize::from(rects.input.height) {
                if lines < 0 {
                    self.input_scroll = self
                        .input_scroll
                        .saturating_sub(lines.unsigned_abs() as u16);
                } else {
                    let max_offset =
                        (visual_rows - usize::from(rects.input.height)) as u16;
                    self.input_scroll = (self.input_scroll + lines as u16).min(max_offset);
                }
                self.input_scroll_manual = true;
                self.dirty = true;
                return;
            }
            // 未溢出：穿透给会话区
        }
    }
    self.scroll_mouse(lines);
}
```

修改 `pub fn input_scroll(&mut self, width: usize, prompt_width: usize, visible_rows: usize)` 开头加入：

```rust
    pub fn input_scroll(&mut self, width: usize, prompt_width: usize, visible_rows: usize) {
        if self.input_scroll_manual {
            return;
        }
        let (_, cursor_row) = self.input_cursor_position(width, prompt_width);
        // ... 原有逻辑
```

在 `reduce_key` 的末尾 `if changed { ... sync ... }` 块内追加一行：

```rust
        if changed {
            self.input_scroll_manual = false;
            let text = self.input.text.clone();
            self.popup.sync(&text, &self.available_models);
            if self.popup.open {
                self.dirty = true;
            }
        }
```

注意 `changed` 为 false 的滚轮/导航路径不应重置 manual；仅编辑性输入重置。

修改 `enum InputMessage`：

```rust
#[derive(Debug)]
enum InputMessage {
    Key(KeyEvent),
    ScrollUp { column: u16, row: u16 },
    ScrollDown { column: u16, row: u16 },
    Resize,
    Error(String),
}
```

修改 `read_input` 中鼠标分支：

```rust
// Before:
                        MouseEventKind::ScrollUp => Some(InputMessage::ScrollUp),
                        MouseEventKind::ScrollDown => Some(InputMessage::ScrollDown),
// After:
                        MouseEventKind::ScrollUp => Some(InputMessage::ScrollUp {
                            column: mouse.column,
                            row: mouse.row,
                        }),
                        MouseEventKind::ScrollDown => Some(InputMessage::ScrollDown {
                            column: mouse.column,
                            row: mouse.row,
                        }),
```

修改 `run_loop` 中匹配：

```rust
// Before:
                    InputMessage::ScrollUp => state.scroll_mouse(-MOUSE_SCROLL_LINES),
                    InputMessage::ScrollDown => state.scroll_mouse(MOUSE_SCROLL_LINES),
// After:
                    InputMessage::ScrollUp { column, row } => {
                        state.scroll_at(column, row, -MOUSE_SCROLL_LINES)
                    }
                    InputMessage::ScrollDown { column, row } => {
                        state.scroll_at(column, row, MOUSE_SCROLL_LINES)
                    }
```

`render()` 内对 `prompt_width` 的重复计算统一改为 `INPUT_PROMPT_WIDTH`，`"> "` 字面量统一改为 `INPUT_PROMPT`（可选，保持一致性）。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --manifest-path crates/grey-tui/Cargo.toml wheel_ editing_resets_input_manual -- --nocapture`
Expected: 均 PASS

Run: `cargo test --manifest-path crates/grey-tui/Cargo.toml -- --nocapture 2>&1 | tail -20`
Expected: 全绿

- [ ] **Step 5: Commit**

```bash
git add crates/grey-tui/src/lib.rs
git commit -m "feat(tui): mouse wheel per-region routing (popup/input/conversation)"
```

---

### Task 8: 门禁、文档与收尾

**Files:**
- Modify: `crates/grey-core/src/config.rs:500-590`（在 `default_tui_input_lines` 定义与 `TuiLayoutConfig` 结构体处加 `#[deprecated]` 注释与文档说明）
- Modify: `docs/plans/v0-1-1-tui.md`（Backlog 状态更新，人工审核意见标记已落地）
- Modify: `README.md:228-242`（`tui = { layout = ... }` 示例更新，不再引导配置 `input_lines`；状态栏描述改为 Pi 式 footer；提及 `/` 补全）

**Interfaces:**
- 无新增接口，纯文档与门禁

- [ ] **Step 1: 更新 grey-core 注释（不改行为）**

在 `crates/grey-core/src/config.rs` 中：

```rust
fn default_tui_input_lines() -> u16 {
    6
}
// 上方或 TuiLayoutConfig 定义处加：
/// `tui.layout.input_lines` is deprecated since v0.1.1: the input area is now
/// content-driven and clamped to 40% of the frame (see `input_area_height`).
/// The field is kept for compatibility and ignored at render time.
pub struct TuiLayoutConfig {
    #[serde(default = "default_tui_input_lines")]
    pub input_lines: u16,
}
```

若 `#[deprecated]` 会触发 workspace `-D warnings`，则改用文档注释而非属性。

- [ ] **Step 2: 更新 docs/plans/v0-1-1-tui.md**

将 Backlog 表中已受审核意见影响的项补充一行“人工审核后 v0.1.1 落地：...”，并在文件末尾 `## 人工审核后意见` 每条后追加 `✅ 已落地（commit ...）`（commit hash 由执行者填入实际 hash）。

示例：

```markdown
## 人工审核后意见（已落地）
1. TUI 冗杂、开屏丑 → ✅ borderless + header + footer（Task 4/6）
2. 输入只换页 → ✅ 自适应高度 40% clamp（Task 5）
3. `/` 补全 → ✅ Neovim 式浮窗（Task 2/3）
4. 状态栏排版 → ✅ Pi 式两端布局（Task 6）
5. 滚轮支持 → ✅ 按区域分流（Task 7）
```

- [ ] **Step 3: 更新 README.md**

```markdown
// Before:
layout = { input_lines = 6 }
// After:
# layout.input_lines 已废弃（v0.1.1 起输入区自适应高度，最大 40% 终端高度）
```

状态栏描述改为：`footer 左 ↑in ↓out · task，右 (provider) model (branch)，两端布局超宽截断`。

补全描述：`输入以 / 开头时弹出补全浮窗（↑/↓ 或 Ctrl+N/P 导航，Tab/Enter 采纳，Esc 关闭）；/model 后空格触发模型名二级补全`。

- [ ] **Step 4: 全量门禁（贴实际输出）**

Run: `cargo fmt --check`
Expected: 无输出（0 exit）

Run: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -20`
Expected: 无警告，exit 0

Run: `cargo test --workspace 2>&1 | tail -30`
Expected: 所有 crate 测试 PASS，exit 0

Run: `cargo build --release 2>&1 | tail -20`
Expected: 构建成功，exit 0

上述四条命令的实际输出必须贴入 PR/commit 描述或执行日志作为证据。

- [ ] **Step 5: Commit**

```bash
git add crates/grey-core/src/config.rs docs/plans/v0-1-1-tui.md README.md
git commit -m "docs: deprecate layout.input_lines and update docs for v0.1.1 TUI simplify"
```

---

## Self-Review

**1. Spec coverage:**
- §2 Header（删 splash，启动 prepend，会话流内随消息上滚，/clear 重现）→ Task 4 覆盖
- §3 输入自适应高度（clamp 40%，溢出才内部滚动，input_lines deprecated）→ Task 5 覆盖
- §4 注册表（消除多处清单重复，parse 查表）→ Task 1 覆盖
- §4 补全状态机（前缀过滤、大小写不敏感、导航循环、采纳、Esc、Enter exact 分支，二级别 /model 模型名）→ Task 2+3 覆盖
- §5 Footer（k 缩写、两端布局、右截断、错误态 ERR、provider/branch 缺省省略）→ Task 6 覆盖
- §6 滚轮（会话/补全/输入三档，按坐标分流，输入溢出判定与 manual 标志）→ Task 7 覆盖
- §6 Taste checklist（无边框、持久 UI ≤2 行、装饰色收敛）→ Task 4+6 覆盖
- 测试策略、错误处理、明确不做 → Task 8 门禁与文档覆盖

**2. Placeholder scan:** 本计划无 `TBD/TODO/占位符/similar to above`，每步含完整代码与精确命令。

**3. Type consistency:**
- `CommandSpec` name/aliases/args_hint/description 在 Task 1 定义，Task 2/3 复用 `COMMANDS` 与字段名一致。
- `CompletionPopup { items: Vec<CompletionItem>, selected, offset, open }` 在 Task 2 定义，Task 3/7 复用 `selected/offset/open/items` 与 `CompletionItem { replaces, label, description }`。
- `InputMessage::ScrollUp { column, row }` 在 Task 7 定义，`read_input` 与 `run_loop` 签名一致。
- `LayoutRects { conversation, input, popup }` 在 Task 6 render 中写入，Task 7 `scroll_at` 读取。
- `input_area_height(usize, u16) -> u16` 在 Task 5 定义，Task 6 render 调用签名一致。

若发现 spec 与实现数据源偏差（provider 显示）已在 Task 6 中对齐并在 spec 补丁中说明。

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-08-21-v0-1-1-tui-simplify.md`. Two execution options:

**1. Subagent-Driven (recommended)** - I dispatch a fresh subagent per task, review between tasks, fast iteration

**2. Inline Execution** - Execute tasks in this session using executing-plans, batch execution with checkpoints

Which approach?
