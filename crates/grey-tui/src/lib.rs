//! Grey's incremental terminal UI.
//!
//! The runtime-facing entry point consumes [`grey_core::AgentEvent`] values
//! and sends submitted prompts back over a Tokio channel. Input and runtime
//! events are reduced into [`AppState`] before rendering, which keeps terminal
//! I/O out of the behavior tests. [`run_stream_demo`] retains the P0 streaming
//! benchmark while exercising the same event path as a real agent.

use std::io::{self, Stdout, Write};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use grey_core::{
    AgentEvent, RuntimeConfig, TuiCompletionConfig, TuiConfig, TuiKeysConfig, TuiLayoutConfig,
};
use notify_rust::Notification;
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame, Terminal,
};
use tokio::sync::{mpsc, watch};
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

const DEMO_REPLY: &str = "Grey 是一个轻量、高性能、可扩展的代码 Agent Harness。\n\n这是 Spike A 的模拟流式输出：消息按小块持续流入 TUI 并增量渲染，状态栏实时显示帧耗时与渲染频率。输入内容后回车会触发一轮新的模拟回复，Esc、Ctrl-C 或空输入时按 q 退出。";
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(80);
const SCROLL_PAGE_LINES: u16 = 5;
const PERSISTENT_REMINDER_TICK: Duration = Duration::from_millis(1800);
const TRUNCATED_MARKER: &str = "[cut]";
pub const TUI_INPUT_LINES_MIN: u16 = 1;
pub const TUI_INPUT_LINES_MAX: u16 = 20;

#[derive(Debug, Clone, Copy, PartialEq)]
enum CompletionBell {
    Soft,
    Strong,
}

#[derive(Debug, Clone, Copy)]
struct KeyBinding {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl KeyBinding {
    fn matches(&self, event: KeyEvent) -> bool {
        self.code == event.code && self.modifiers == event.modifiers
    }

    fn label(&self) -> String {
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            if self.code == KeyCode::Char(' ') {
                "ctrl-space".into()
            } else {
                format!(
                    "ctrl-{}",
                    key_code_char(self.code).unwrap_or('?').to_ascii_lowercase()
                )
            }
        } else {
            key_code_char(self.code)
                .map(|c| format!("{c}"))
                .unwrap_or_else(|| format!("{:?}", self.code).to_lowercase())
        }
    }
}

#[derive(Debug, Clone)]
struct RenderTheme {
    border: Color,
    accent: Color,
    prompt: Color,
    status_fg: Color,
    status_bg: Color,
    muted: Color,
    error: Color,
    success: Color,
    warning: Color,
}

#[derive(Debug, Clone)]
struct TuiTheme {
    colors: RenderTheme,
}

impl TuiTheme {
    fn from_config(config: &grey_core::TuiThemeConfig) -> Self {
        let mut theme = match config.preset.as_str() {
            "grey_storm" => RenderTheme {
                border: Color::Rgb(0x1d, 0x55, 0x5a),
                accent: Color::Rgb(0x44, 0xe0, 0xd3),
                prompt: Color::Rgb(0x89, 0xff, 0xf2),
                status_fg: Color::Rgb(0xd7, 0xfa, 0xf7),
                status_bg: Color::Rgb(0x12, 0x38, 0x3b),
                muted: Color::Rgb(0x6f, 0x8f, 0x90),
                error: Color::Rgb(0xff, 0x7b, 0x72),
                success: Color::Green,
                warning: Color::Yellow,
            },
            "slate" => RenderTheme {
                border: Color::Blue,
                accent: Color::LightBlue,
                prompt: Color::LightBlue,
                status_fg: Color::White,
                status_bg: Color::Blue,
                muted: Color::DarkGray,
                error: Color::LightRed,
                success: Color::Green,
                warning: Color::Yellow,
            },
            "sunset" => RenderTheme {
                border: Color::DarkGray,
                accent: Color::LightYellow,
                prompt: Color::Yellow,
                status_fg: Color::Black,
                status_bg: Color::LightYellow,
                muted: Color::Gray,
                error: Color::LightRed,
                success: Color::Green,
                warning: Color::Yellow,
            },
            "mono" => RenderTheme {
                border: Color::White,
                accent: Color::White,
                prompt: Color::White,
                status_fg: Color::Black,
                status_bg: Color::White,
                muted: Color::DarkGray,
                error: Color::LightRed,
                success: Color::Green,
                warning: Color::Yellow,
            },
            _ => RenderTheme {
                border: Color::Blue,
                accent: Color::Green,
                prompt: Color::Green,
                status_fg: Color::Yellow,
                status_bg: Color::Blue,
                muted: Color::DarkGray,
                error: Color::LightRed,
                success: Color::Green,
                warning: Color::Yellow,
            },
        };
        apply_theme_override(&mut theme, &config.overrides);
        Self { colors: theme }
    }
}

#[derive(Debug, Clone)]
struct TuiSettings {
    theme: TuiTheme,
    layout: TuiLayoutConfig,
    completion: TuiCompletionConfig,
    keys: TuiKeyBindings,
    git_branch: Option<String>,
}

#[derive(Debug, Clone)]
struct TuiKeyBindings {
    leader: KeyBinding,
    help: KeyBinding,
    quit: KeyBinding,
    clear: KeyBinding,
    scroll_up: KeyBinding,
    scroll_down: KeyBinding,
}

impl From<&TuiConfig> for TuiSettings {
    fn from(config: &TuiConfig) -> Self {
        Self {
            theme: TuiTheme::from_config(&config.theme),
            layout: TuiLayoutConfig {
                input_lines: config
                    .layout
                    .input_lines
                    .clamp(TUI_INPUT_LINES_MIN, TUI_INPUT_LINES_MAX),
            },
            completion: config.completion.clone(),
            keys: TuiKeyBindings::from_config(&config.keys),
            git_branch: None,
        }
    }
}

impl TuiSettings {
    fn with_git_branch(mut self, git_branch: Option<String>) -> Self {
        self.git_branch = git_branch;
        self
    }

    fn branch_label(&self) -> Option<&str> {
        self.git_branch.as_deref()
    }
}

impl TuiKeyBindings {
    fn from_config(config: &TuiKeysConfig) -> Self {
        Self {
            leader: parse_keybinding(&config.leader).unwrap_or_else(default_leader_key),
            help: parse_keybinding(&config.help).unwrap_or_else(default_help_key),
            quit: parse_keybinding(&config.quit).unwrap_or_else(default_quit_key),
            clear: parse_keybinding(&config.clear).unwrap_or_else(default_clear_key),
            scroll_up: parse_keybinding(&config.scroll_up).unwrap_or_else(default_scroll_up_key),
            scroll_down: parse_keybinding(&config.scroll_down)
                .unwrap_or_else(default_scroll_down_key),
        }
    }

    fn labels(&self) -> KeyBindingLabels {
        KeyBindingLabels {
            leader: self.leader.label(),
            help: self.help.label(),
            quit: self.quit.label(),
            clear: self.clear.label(),
            scroll_up: self.scroll_up.label(),
            scroll_down: self.scroll_down.label(),
        }
    }
}

struct KeyBindingLabels {
    leader: String,
    help: String,
    quit: String,
    clear: String,
    scroll_up: String,
    scroll_down: String,
}

fn parse_keybinding(input: &str) -> Option<KeyBinding> {
    let value = input.trim();
    if value.is_empty() {
        return None;
    }
    if value.eq_ignore_ascii_case("esc") {
        return Some(KeyBinding {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::NONE,
        });
    }
    if value.eq_ignore_ascii_case("space") {
        return Some(KeyBinding {
            code: KeyCode::Char(' '),
            modifiers: KeyModifiers::NONE,
        });
    }
    if value.eq_ignore_ascii_case("pageup") {
        return Some(KeyBinding {
            code: KeyCode::PageUp,
            modifiers: KeyModifiers::NONE,
        });
    }
    if value.eq_ignore_ascii_case("pagedown") {
        return Some(KeyBinding {
            code: KeyCode::PageDown,
            modifiers: KeyModifiers::NONE,
        });
    }
    if value.eq_ignore_ascii_case("home") {
        return Some(KeyBinding {
            code: KeyCode::Home,
            modifiers: KeyModifiers::NONE,
        });
    }
    if value.eq_ignore_ascii_case("end") {
        return Some(KeyBinding {
            code: KeyCode::End,
            modifiers: KeyModifiers::NONE,
        });
    }
    if let Some(rest) = value.strip_prefix("ctrl-") {
        let control_char = if rest.eq_ignore_ascii_case("space") {
            ' '
        } else {
            let mut chars = rest.chars();
            let ch = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            ch
        };
        return Some(KeyBinding {
            code: KeyCode::Char(control_char),
            modifiers: KeyModifiers::CONTROL,
        });
    }
    let mut chars = value.chars();
    let ch = chars.next()?;
    if chars.next().is_none() {
        return Some(KeyBinding {
            code: KeyCode::Char(ch),
            modifiers: KeyModifiers::NONE,
        });
    }
    None
}

/// Returns whether a key string is accepted by the TUI input mapping.
pub fn keybinding_is_valid(input: &str) -> bool {
    parse_keybinding(input).is_some()
}

/// Returns whether an input area height is safe for the bounded TUI layout.
pub fn input_lines_is_valid(input_lines: u16) -> bool {
    (TUI_INPUT_LINES_MIN..=TUI_INPUT_LINES_MAX).contains(&input_lines)
}

fn default_leader_key() -> KeyBinding {
    KeyBinding {
        code: KeyCode::Char('\\'),
        modifiers: KeyModifiers::NONE,
    }
}

fn default_help_key() -> KeyBinding {
    KeyBinding {
        code: KeyCode::Char('k'),
        modifiers: KeyModifiers::NONE,
    }
}

fn default_quit_key() -> KeyBinding {
    KeyBinding {
        code: KeyCode::Char('c'),
        modifiers: KeyModifiers::CONTROL,
    }
}

fn default_clear_key() -> KeyBinding {
    KeyBinding {
        code: KeyCode::Char('l'),
        modifiers: KeyModifiers::CONTROL,
    }
}

fn default_scroll_up_key() -> KeyBinding {
    KeyBinding {
        code: KeyCode::PageUp,
        modifiers: KeyModifiers::NONE,
    }
}

fn default_scroll_down_key() -> KeyBinding {
    KeyBinding {
        code: KeyCode::PageDown,
        modifiers: KeyModifiers::NONE,
    }
}

fn key_code_char(code: KeyCode) -> Option<char> {
    match code {
        KeyCode::Char(ch) => Some(ch),
        _ => None,
    }
}

impl Default for TuiSettings {
    fn default() -> Self {
        Self::from(&TuiConfig::default())
    }
}

fn parse_color(input: &str) -> Option<Color> {
    let value = input.trim().to_lowercase();
    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() != 6 {
            return None;
        }
        let value = u32::from_str_radix(hex, 16).ok()?;
        let r = u8::try_from((value >> 16) & 0xff).ok()?;
        let g = u8::try_from((value >> 8) & 0xff).ok()?;
        let b = u8::try_from(value & 0xff).ok()?;
        return Some(Color::Rgb(r, g, b));
    }
    match value.as_str() {
        "black" => Some(Color::Black),
        "darkgray" => Some(Color::DarkGray),
        "gray" | "grey" => Some(Color::Gray),
        "red" => Some(Color::Red),
        "lightred" => Some(Color::LightRed),
        "green" => Some(Color::Green),
        "lightgreen" => Some(Color::LightGreen),
        "yellow" => Some(Color::Yellow),
        "lightyellow" => Some(Color::LightYellow),
        "blue" => Some(Color::Blue),
        "lightblue" => Some(Color::LightBlue),
        "magenta" => Some(Color::Magenta),
        "lightmagenta" => Some(Color::LightMagenta),
        "cyan" => Some(Color::Cyan),
        "lightcyan" => Some(Color::LightCyan),
        "white" => Some(Color::White),
        _ => None,
    }
}

/// Validates the limited theme manifest accepted from a command theme plugin.
pub fn theme_config_is_valid(config: &grey_core::TuiThemeConfig) -> bool {
    matches!(
        config.preset.as_str(),
        "default" | "grey_storm" | "slate" | "sunset" | "mono"
    ) && [
        &config.overrides.border,
        &config.overrides.accent,
        &config.overrides.prompt,
        &config.overrides.status_fg,
        &config.overrides.status_bg,
        &config.overrides.muted,
        &config.overrides.error,
        &config.overrides.success,
        &config.overrides.warning,
    ]
    .into_iter()
    .flatten()
    .all(|color| parse_color(color).is_some())
}

fn apply_theme_override(theme: &mut RenderTheme, overrides: &grey_core::TuiColorOverrides) {
    if let Some(value) = &overrides.border {
        if let Some(color) = parse_color(value) {
            theme.border = color;
        }
    }
    if let Some(value) = &overrides.accent {
        if let Some(color) = parse_color(value) {
            theme.accent = color;
        }
    }
    if let Some(value) = &overrides.prompt {
        if let Some(color) = parse_color(value) {
            theme.prompt = color;
        }
    }
    if let Some(value) = &overrides.status_fg {
        if let Some(color) = parse_color(value) {
            theme.status_fg = color;
        }
    }
    if let Some(value) = &overrides.status_bg {
        if let Some(color) = parse_color(value) {
            theme.status_bg = color;
        }
    }
    if let Some(value) = &overrides.muted {
        if let Some(color) = parse_color(value) {
            theme.muted = color;
        }
    }
    if let Some(value) = &overrides.error {
        if let Some(color) = parse_color(value) {
            theme.error = color;
        }
    }
    if let Some(value) = &overrides.success {
        if let Some(color) = parse_color(value) {
            theme.success = color;
        }
    }
    if let Some(value) = &overrides.warning {
        if let Some(color) = parse_color(value) {
            theme.warning = color;
        }
    }
}

/// Prompts submitted by the TUI are sent to the owner of the agent loop.
pub type PromptSender = mpsc::Sender<String>;

/// A side effect requested by the otherwise-pure input reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiAction {
    None,
    Submit {
        prompt: String,
        rejected_input: String,
    },
    SwitchModel {
        model: String,
    },
    Quit,
}

/// A `/`-prefixed command parsed from the input line.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SlashCommand {
    Help,
    Clear,
    Quit,
    Model { model: String },
    Unknown(String),
}

impl SlashCommand {
    fn parse(input: &str) -> Self {
        let body = input.strip_prefix('/').unwrap_or(input).trim();
        let (name, argument) = body.split_once(char::is_whitespace).unwrap_or((body, ""));
        let name = name.trim().to_ascii_lowercase();
        match name.as_str() {
            "help" | "?" => Self::Help,
            "clear" => Self::Clear,
            "quit" | "exit" => Self::Quit,
            "model" => Self::Model {
                model: argument.trim().to_owned(),
            },
            other => Self::Unknown(other.to_owned()),
        }
    }
}

#[derive(Debug, Clone)]
struct CompletionSettings {
    enabled: bool,
    long_running_steps: usize,
    long_running_seconds: u64,
    bell: bool,
    strong_bell: bool,
    notify: bool,
    persistent: bool,
}

impl From<TuiCompletionConfig> for CompletionSettings {
    fn from(config: TuiCompletionConfig) -> Self {
        Self {
            enabled: config.enabled,
            long_running_steps: config.long_running_steps,
            long_running_seconds: config.long_running_seconds,
            bell: config.bell,
            strong_bell: config.strong_bell,
            notify: config.notify,
            persistent: config.persistent,
        }
    }
}

impl Default for CompletionSettings {
    fn default() -> Self {
        Self::from(TuiCompletionConfig::default())
    }
}

#[derive(Debug, Clone, Default)]
struct InputBuffer {
    text: String,
    cursor_chars: usize,
}

impl InputBuffer {
    fn insert(&mut self, character: char) {
        let byte = self.cursor_byte();
        self.text.insert(byte, character);
        self.cursor_chars += 1;
    }

    fn backspace(&mut self) -> bool {
        if self.cursor_chars == 0 {
            return false;
        }
        self.cursor_chars -= 1;
        let start = self.cursor_byte();
        let end = next_char_byte(&self.text, start);
        self.text.replace_range(start..end, "");
        true
    }

    fn delete(&mut self) -> bool {
        let start = self.cursor_byte();
        if start == self.text.len() {
            return false;
        }
        let end = next_char_byte(&self.text, start);
        self.text.replace_range(start..end, "");
        true
    }

    fn move_left(&mut self) -> bool {
        let old = self.cursor_chars;
        self.cursor_chars = self.cursor_chars.saturating_sub(1);
        old != self.cursor_chars
    }

    fn move_right(&mut self) -> bool {
        let old = self.cursor_chars;
        self.cursor_chars = (self.cursor_chars + 1).min(self.text.chars().count());
        old != self.cursor_chars
    }

    fn move_home(&mut self) -> bool {
        let line = self.cursor_line();
        self.move_to(line, 0)
    }

    fn move_end(&mut self) -> bool {
        let line = self.cursor_line();
        let line_len = self.line_len(line);
        self.move_to(line, line_len)
    }

    fn move_up(&mut self) -> bool {
        let line = self.cursor_line();
        if line == 0 {
            return false;
        }
        self.move_to(line - 1, self.cursor_col())
    }

    fn move_down(&mut self) -> bool {
        let line = self.cursor_line();
        if line + 1 >= self.line_count() {
            return false;
        }
        self.move_to(line + 1, self.cursor_col())
    }

    fn insert_newline(&mut self) -> bool {
        self.insert('\n');
        true
    }

    /// Pi-style fallback: a trailing `\` before Enter becomes a newline instead
    /// of submitting, for terminals that cannot send Shift+Enter distinctly.
    fn trailing_backslash_escape(&mut self) -> bool {
        if self.cursor_chars == 0 || !self.text.ends_with('\\') {
            return false;
        }
        self.text.pop();
        self.cursor_chars -= 1;
        self.insert_newline()
    }

    fn line_count(&self) -> usize {
        self.text.matches('\n').count() + 1
    }

    fn cursor_line(&self) -> usize {
        self.text[..self.cursor_byte()].matches('\n').count()
    }

    fn cursor_col(&self) -> usize {
        self.text[..self.cursor_byte()]
            .rsplit('\n')
            .next()
            .unwrap_or_default()
            .chars()
            .count()
    }

    fn line_len(&self, line: usize) -> usize {
        self.text
            .split('\n')
            .nth(line)
            .map(|content| content.chars().count())
            .unwrap_or(0)
    }

    fn move_to(&mut self, line: usize, column: usize) -> bool {
        let lines: Vec<&str> = self.text.split('\n').collect();
        let line = line.min(lines.len().saturating_sub(1));
        let column = column.min(lines[line].chars().count());
        let prefix_chars: usize = lines[..line]
            .iter()
            .map(|content| content.chars().count())
            .sum();
        let target = prefix_chars + line + column;
        let old = self.cursor_chars;
        self.cursor_chars = target.min(self.text.chars().count());
        old != self.cursor_chars
    }

    fn take(&mut self) -> String {
        let input = std::mem::take(&mut self.text);
        self.cursor_chars = 0;
        input
    }

    fn cursor_display_column(&self) -> usize {
        UnicodeWidthStr::width(&self.text[..self.cursor_byte()])
    }

    fn cursor_byte(&self) -> usize {
        self.text
            .char_indices()
            .nth(self.cursor_chars)
            .map_or(self.text.len(), |(byte, _)| byte)
    }
}

fn next_char_byte(text: &str, start: usize) -> usize {
    text[start..]
        .char_indices()
        .nth(1)
        .map_or(text.len(), |(relative, _)| start + relative)
}

fn input_wrapped_rows(content: &str, width: usize) -> usize {
    if width == 0 {
        return content.chars().count().saturating_add(1);
    }
    let mut rows = 1usize;
    let mut column = 0usize;
    for character in content.chars() {
        let character_width = character.width().unwrap_or(0);
        if column + character_width > width {
            rows += 1;
            column = 0;
        }
        column += character_width;
    }
    rows
}

/// Measurements for frames that were actually drawn.
#[derive(Debug, Clone)]
pub struct FrameStats {
    frames: u64,
    last_frame: Duration,
    ema_ms: f64,
}

impl Default for FrameStats {
    fn default() -> Self {
        Self {
            frames: 0,
            last_frame: Duration::ZERO,
            ema_ms: 0.0,
        }
    }
}

impl FrameStats {
    fn record(&mut self, elapsed: Duration) {
        self.frames = self.frames.saturating_add(1);
        self.last_frame = elapsed;
        let sample_ms = elapsed.as_secs_f64() * 1_000.0;
        self.ema_ms = if self.frames == 1 {
            sample_ms
        } else {
            self.ema_ms * 0.9 + sample_ms * 0.1
        };
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn last_frame(&self) -> Duration {
        self.last_frame
    }

    pub fn ema_frame_ms(&self) -> f64 {
        self.ema_ms
    }

    pub fn fps(&self) -> f64 {
        if self.ema_ms > 0.0 {
            1_000.0 / self.ema_ms
        } else {
            0.0
        }
    }
}

/// Testable state for input, transcript, scroll position, and redraw policy.
#[derive(Debug, Clone)]
pub struct AppState {
    output: String,
    transcript_max_bytes: usize,
    input: InputBuffer,
    status: String,
    current_task: Option<String>,
    current_provider: Option<String>,
    current_model: Option<String>,
    branch: Option<String>,
    status_error: bool,
    total_input_tokens: u64,
    total_output_tokens: u64,
    scroll: u16,
    max_scroll: u16,
    follow_output: bool,
    input_scroll: u16,
    dirty: bool,
    frame_stats: FrameStats,
    settings: TuiSettings,
    turn_started_at: Option<Instant>,
    pending_completion_bell: Option<CompletionBell>,
    pending_completion_message: Option<String>,
    persistent_completion_message: Option<String>,
    next_persistent_tick: Option<Instant>,
    completion: CompletionSettings,
    show_help: bool,
    leader_armed: bool,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            output: String::new(),
            transcript_max_bytes: RuntimeConfig::default().transcript_max_bytes,
            input: InputBuffer::default(),
            status: "ready".into(),
            current_task: None,
            current_provider: None,
            current_model: None,
            branch: None,
            status_error: false,
            total_input_tokens: 0,
            total_output_tokens: 0,
            scroll: 0,
            max_scroll: 0,
            follow_output: true,
            input_scroll: 0,
            dirty: true,
            frame_stats: FrameStats::default(),
            settings: TuiSettings::default(),
            turn_started_at: None,
            pending_completion_bell: None,
            pending_completion_message: None,
            persistent_completion_message: None,
            next_persistent_tick: None,
            completion: CompletionSettings::default(),
            show_help: false,
            leader_armed: false,
        }
    }
}

impl AppState {
    fn with_settings(settings: TuiSettings) -> Self {
        let branch = settings.branch_label().map(str::to_string);
        let completion = CompletionSettings::from(settings.completion.clone());
        Self {
            settings: settings.clone(),
            branch,
            status_error: false,
            completion,
            ..AppState::default()
        }
    }

    fn with_runtime(settings: TuiSettings, runtime: &RuntimeConfig) -> Self {
        Self {
            transcript_max_bytes: runtime.transcript_max_bytes,
            ..Self::with_settings(settings)
        }
    }
}

impl AppState {
    pub fn output(&self) -> &str {
        &self.output
    }

    pub fn input(&self) -> &str {
        &self.input.text
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    fn status_has_error(&self) -> bool {
        self.status_error
    }

    fn total_usage(&self) -> (u64, u64) {
        (self.total_input_tokens, self.total_output_tokens)
    }

    fn model_info(&self) -> Option<(&str, &str)> {
        self.current_provider
            .as_deref()
            .zip(self.current_model.as_deref())
    }

    fn current_task_label(&self) -> &str {
        self.current_task.as_deref().unwrap_or("-")
    }

    fn branch_label(&self) -> &str {
        self.branch.as_deref().unwrap_or("-")
    }

    fn take_completion_bell(&mut self) -> Option<CompletionBell> {
        self.pending_completion_bell.take()
    }

    fn take_completion_message(&mut self) -> Option<String> {
        self.pending_completion_message.take()
    }

    fn clear_completion_notice(&mut self) {
        self.pending_completion_message = None;
        self.persistent_completion_message = None;
        self.next_persistent_tick = None;
    }

    fn append_output(&mut self, addition: &str) {
        append_bounded(&mut self.output, addition, self.transcript_max_bytes);
    }

    fn note_prompt_submitted(&mut self, prompt: &str) {
        let separator = if !self.output.is_empty() && !self.output.ends_with('\n') {
            "\n"
        } else {
            ""
        };
        self.append_output(&format!("{separator}> {prompt}\n\n"));
        self.current_task = Some(prompt.chars().take(60).collect());
        self.follow_output = true;
        self.turn_started_at = Some(Instant::now());
        self.status_error = false;
        self.pending_completion_bell = None;
        self.current_provider = None;
        self.current_model = None;
        self.leader_armed = false;
        self.clear_completion_notice();
        self.status = "prompt submitted".into();
        self.dirty = true;
    }

    fn note_prompt_busy(&mut self, prompt: Option<String>) {
        if let Some(prompt) = prompt {
            self.input.text = prompt;
            self.input.cursor_chars = self.input.text.chars().count();
        }
        self.status = "busy: prompt not submitted".into();
        self.status_error = true;
        self.dirty = true;
    }

    fn clear_output(&mut self) {
        self.clear_completion_notice();
        self.output.clear();
        self.pending_completion_bell = None;
        self.status = "output cleared".into();
        self.scroll = 0;
        self.max_scroll = 0;
        self.status_error = false;
        self.dirty = true;
    }

    fn apply_slash_command(&mut self) -> UiAction {
        let input = self.input.text.clone();
        match SlashCommand::parse(&input) {
            SlashCommand::Help => {
                self.input.take();
                self.show_help = true;
                self.status = "help".into();
                self.dirty = true;
                UiAction::None
            }
            SlashCommand::Clear => {
                self.input.take();
                self.clear_output();
                UiAction::None
            }
            SlashCommand::Quit => {
                self.input.take();
                self.clear_completion_notice();
                self.leader_armed = false;
                UiAction::Quit
            }
            SlashCommand::Model { model } if model.is_empty() => {
                self.status = "usage: /model <name>".into();
                self.status_error = true;
                self.dirty = true;
                UiAction::None
            }
            SlashCommand::Model { model } => {
                self.input.take();
                self.status = format!("switching model to {model}");
                self.status_error = false;
                self.dirty = true;
                UiAction::SwitchModel { model }
            }
            SlashCommand::Unknown(name) => {
                self.status = format!("unknown command /{name}");
                self.status_error = true;
                self.dirty = true;
                UiAction::None
            }
        }
    }

    fn completion_notice(&self) -> Option<&str> {
        self.persistent_completion_message.as_deref()
    }

    fn schedule_completion_notice(&mut self, message: String) {
        self.pending_completion_message =
            (self.completion.notify || self.completion.persistent).then_some(message.clone());
        if self.completion.persistent {
            self.persistent_completion_message = Some(message);
            self.next_persistent_tick = Some(Instant::now() + PERSISTENT_REMINDER_TICK);
        } else {
            self.next_persistent_tick = None;
        }
    }

    fn poll_persistent_notice(&mut self) -> Option<String> {
        let message = self.persistent_completion_message.clone()?;
        let now = Instant::now();
        if self.next_persistent_tick.is_some_and(|tick| now < tick) {
            return None;
        }
        self.next_persistent_tick = Some(now + PERSISTENT_REMINDER_TICK);
        Some(message)
    }

    pub fn scroll(&self) -> u16 {
        self.scroll
    }

    pub fn follows_output(&self) -> bool {
        self.follow_output
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn frame_stats(&self) -> &FrameStats {
        &self.frame_stats
    }

    /// Display cells before the logical input cursor, not Unicode scalar count.
    pub fn input_cursor_column(&self) -> usize {
        self.input.cursor_display_column()
    }

    /// (column, row) of the cursor inside the wrapped input text. The first
    /// logical line renders with a `> ` prefix, so it wraps at `width -
    /// prompt_width`; later lines wrap at `width`.
    pub fn input_cursor_position(&self, width: usize, prompt_width: usize) -> (usize, usize) {
        let text = self.input.text.as_str();
        let cursor_byte = self.input.cursor_byte();
        let before = &text[..cursor_byte];
        let column = match before.rfind('\n') {
            Some(last) => UnicodeWidthStr::width(&before[last + 1..]),
            None => UnicodeWidthStr::width(before),
        };
        let row = match before.rfind('\n') {
            Some(last) => before[..=last].split_inclusive('\n').enumerate().fold(
                0usize,
                |row, (index, line)| {
                    let content = line.trim_end_matches('\n');
                    let effective_width = if index == 0 {
                        width.saturating_sub(prompt_width)
                    } else {
                        width
                    };
                    row.saturating_add(input_wrapped_rows(content, effective_width))
                },
            ),
            None => 0,
        };
        (column, row)
    }

    /// Scroll the input so the cursor's wrapped row stays visible.
    pub fn input_scroll(&mut self, width: usize, prompt_width: usize, visible_rows: usize) {
        let (_, cursor_row) = self.input_cursor_position(width, prompt_width);
        if cursor_row >= visible_rows {
            self.input_scroll = (cursor_row - visible_rows + 1) as u16;
        } else {
            self.input_scroll = 0;
        }
    }

    /// Reduce one terminal key event into state and, optionally, an action.
    pub fn reduce_key(&mut self, key: KeyEvent) -> UiAction {
        if key.kind == KeyEventKind::Release {
            return UiAction::None;
        }
        if self.show_help {
            if self.settings.keys.quit.matches(key) {
                self.show_help = false;
                self.dirty = true;
                return UiAction::Quit;
            }
            if key.code == KeyCode::Esc
                || key.code == KeyCode::Char('q') && key.modifiers.is_empty()
            {
                self.show_help = false;
                self.dirty = true;
            }
            return UiAction::None;
        }

        if self.settings.keys.quit.matches(key) {
            self.clear_completion_notice();
            self.leader_armed = false;
            return UiAction::Quit;
        }

        if self.settings.keys.clear.matches(key) {
            self.clear_output();
            return UiAction::None;
        }

        if self.settings.keys.scroll_up.matches(key) {
            let previous = self.scroll;
            self.scroll = previous.saturating_sub(SCROLL_PAGE_LINES);
            let changed = self.scroll != previous;
            if changed {
                self.follow_output = false;
            }
            self.dirty |= changed;
            return UiAction::None;
        }

        if self.settings.keys.scroll_down.matches(key) {
            self.scroll = self
                .scroll
                .saturating_add(SCROLL_PAGE_LINES)
                .min(self.max_scroll);
            self.follow_output = self.scroll == self.max_scroll;
            self.dirty = true;
            return UiAction::None;
        }

        if self.settings.keys.leader.matches(key)
            && self.input.text.is_empty()
            && self.input.cursor_chars == 0
        {
            self.leader_armed = true;
            self.status = "leader".into();
            self.dirty = true;
            return UiAction::None;
        }

        if self.leader_armed {
            self.leader_armed = false;
            if self.settings.keys.help.matches(key) {
                self.show_help = true;
                self.status = "help".into();
                self.dirty = true;
                return UiAction::None;
            }
            self.status = "unknown leader key".into();
            self.dirty = true;
            return UiAction::None;
        }

        if key.code == KeyCode::Esc && self.input.text.is_empty() {
            return UiAction::Quit;
        }
        if key.code == KeyCode::Char('q') && key.modifiers.is_empty() && self.input.text.is_empty()
        {
            return UiAction::Quit;
        }
        if matches!(key.code, KeyCode::Char('c' | 'C'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            return UiAction::Quit;
        }

        let changed = match key.code {
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.insert(character);
                true
            }
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left => self.input.move_left(),
            KeyCode::Right => self.input.move_right(),
            KeyCode::Up => self.input.move_up(),
            KeyCode::Down => self.input.move_down(),
            KeyCode::Home => self.input.move_home(),
            KeyCode::End => self.input.move_end(),
            KeyCode::Enter => {
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
                {
                    self.input.insert_newline();
                    return UiAction::None;
                }
                if self.input.trailing_backslash_escape() {
                    return UiAction::None;
                }
                if self.turn_started_at.is_some() {
                    self.note_prompt_busy(None);
                    return UiAction::None;
                }
                if self.input.text.starts_with('/') {
                    return self.apply_slash_command();
                }
                let rejected_input = self.input.take();
                let prompt = rejected_input.trim().to_owned();
                self.dirty = true;
                if prompt.is_empty() {
                    return UiAction::None;
                }
                return UiAction::Submit {
                    prompt,
                    rejected_input,
                };
            }
            _ => false,
        };
        self.dirty |= changed;
        UiAction::None
    }

    /// Reduce a normalized Core event into the visible transcript/status.
    pub fn reduce_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Delta(delta) => {
                self.append_output(&delta);
                self.status_error = false;
                self.status = "streaming".into();
            }
            AgentEvent::ToolStarted(call) => {
                self.append_output(&format!(
                    "\n[tool:start] {} {}\n",
                    call.name, call.arguments
                ));
                self.status_error = false;
                self.status = format!("running {}", call.name);
            }
            AgentEvent::ToolFinished(result) => {
                let outcome = if result.success { "ok" } else { "failed" };
                self.append_output(&format!(
                    "[tool:{outcome}] {}\n{}\n",
                    result.name, result.output
                ));
                self.status_error = false;
                self.status = format!("{} {outcome}", result.name);
            }
            AgentEvent::Retry { attempt, error } => {
                self.append_output(&format!("\n[retry {attempt}] {error}\n"));
                self.status_error = false;
                self.status = format!("retry {attempt}: {error}");
            }
            AgentEvent::ContextTrimmed(audit) => {
                self.append_output(&format!(
                    "\n[context] dropped {} messages ({} -> {} chars)\n",
                    audit.dropped_messages, audit.original_chars, audit.retained_chars
                ));
                self.status_error = false;
                self.status = format!("context trimmed: {} dropped", audit.dropped_messages);
            }
            AgentEvent::Completed {
                usage,
                steps,
                provider,
                model,
            } => {
                self.current_provider = Some(provider.clone());
                self.current_model = Some(model.clone());
                self.total_input_tokens =
                    self.total_input_tokens.saturating_add(usage.input_tokens);
                self.total_output_tokens =
                    self.total_output_tokens.saturating_add(usage.output_tokens);
                self.pending_completion_bell = self.completion_bell_for(steps);
                self.schedule_completion_notice(format!(
                    "completed: {provider}/{model} in {steps} steps"
                ));
                self.append_output(&format!(
                    "\n[completed] {steps} steps, {} input / {} output tokens\n",
                    usage.input_tokens, usage.output_tokens
                ));
                self.status_error = false;
                self.status = format!("completed in {steps} steps");
                self.turn_started_at = None;
            }
            AgentEvent::Failed(error) => {
                self.pending_completion_bell = self.completion_bell_for(0);
                self.schedule_completion_notice(format!("failed: {error}"));
                self.append_output(&format!("\n[failed] {error}\n"));
                self.status_error = true;
                self.status = format!("failed: {error}");
                self.turn_started_at = None;
            }
            AgentEvent::ProviderSwitched { from, to, reason } => {
                self.append_output(&format!("\n[switch] {from} → {to}: {reason}\n"));
                self.status_error = false;
                self.status = format!("switched to {to}");
            }
            AgentEvent::CacheHit { model } => {
                self.append_output(&format!("\n[cache] hit for {model}\n"));
                self.status_error = false;
                self.status = "cache hit".into();
            }
            AgentEvent::Warning(warning) => {
                self.append_output(&format!("\n[warning] {warning}\n"));
                self.status_error = true;
                self.status = format!("warning: {warning}");
            }
        }
        self.dirty = true;
    }

    fn completion_bell_for(&self, steps: usize) -> Option<CompletionBell> {
        if !self.completion.enabled {
            return None;
        }
        let by_steps = steps >= self.completion.long_running_steps;
        let by_time = self
            .completion
            .long_running_seconds
            .gt(&0)
            .then_some(())
            .filter(|_| {
                self.turn_started_at
                    .is_some_and(|t| t.elapsed().as_secs() >= self.completion.long_running_seconds)
            })
            .is_some();
        if !(by_steps || by_time) {
            return None;
        }
        if self.completion.strong_bell {
            Some(CompletionBell::Strong)
        } else {
            Some(CompletionBell::Soft)
        }
    }

    /// Recompute the maximum scroll from the exact wrapped paragraph height.
    pub fn update_viewport(&mut self, content_width: u16, content_height: u16) {
        let line_count = Paragraph::new(self.output.as_str())
            .wrap(Wrap { trim: false })
            .line_count(content_width.max(1));
        self.max_scroll = line_count
            .saturating_sub(usize::from(content_height))
            .min(usize::from(u16::MAX)) as u16;
        if self.follow_output {
            self.scroll = self.max_scroll;
        } else {
            self.scroll = self.scroll.min(self.max_scroll);
        }
    }

    fn note_runtime_closed(&mut self) {
        self.status = "agent event channel closed".into();
        self.dirty = true;
    }

    fn note_resize(&mut self) {
        self.dirty = true;
    }

    fn mark_drawn(&mut self, elapsed: Duration) {
        self.frame_stats.record(elapsed);
        self.dirty = false;
    }
}

fn append_bounded(text: &mut String, addition: &str, max_bytes: usize) {
    if max_bytes == 0 {
        text.clear();
        return;
    }
    let had_marker = text.starts_with(TRUNCATED_MARKER);
    if had_marker {
        text.drain(..TRUNCATED_MARKER.len());
    }
    text.push_str(addition);
    if !had_marker && text.len() <= max_bytes {
        return;
    }

    let marker_budget = max_bytes.saturating_sub(TRUNCATED_MARKER.len());
    let marker_start = utf8_suffix_start(text, marker_budget);
    if max_bytes >= TRUNCATED_MARKER.len() && marker_start < text.len() {
        let suffix = text.split_off(marker_start);
        text.clear();
        text.push_str(TRUNCATED_MARKER);
        text.push_str(&suffix);
        return;
    }

    let suffix = text.split_off(utf8_suffix_start(text, max_bytes));
    *text = suffix;
}

fn utf8_suffix_start(text: &str, max_bytes: usize) -> usize {
    let mut start = text.len().saturating_sub(max_bytes);
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    start
}

/// Run the real conversation UI over channels owned by an agent orchestrator.
pub async fn run_agent_tui(
    events: mpsc::Receiver<AgentEvent>,
    prompts: PromptSender,
    model_switch: watch::Sender<Option<String>>,
    tui_config: &TuiConfig,
    runtime_config: &RuntimeConfig,
    git_branch: Option<&str>,
) -> Result<()> {
    let _restore = enter_terminal()?;
    let stdout: Stdout = io::stdout();
    let mut terminal =
        Terminal::new(CrosstermBackend::new(stdout)).context("creating Crossterm terminal")?;
    let (mut input_thread, input_events) = InputThread::spawn(runtime_config.input_queue_capacity)?;
    let settings =
        TuiSettings::from(tui_config).with_git_branch(git_branch.map(ToString::to_string));
    let state = AppState::with_runtime(settings, runtime_config);
    let result = run_loop(
        &mut terminal,
        events,
        prompts,
        model_switch,
        input_events,
        state,
        trigger_completion_notification,
    )
    .await;
    let input_result = input_thread.stop_and_join();
    match result {
        Ok(()) => input_result,
        Err(error) => {
            let _ = input_result;
            Err(error)
        }
    }
}

/// Run the P0 mock stream through the production state/event/render pipeline.
pub async fn run_stream_demo() -> Result<()> {
    let runtime = RuntimeConfig::default();
    let (agent_sender, agent_events) = mpsc::channel(runtime.event_queue_capacity);
    let (prompt_sender, prompt_receiver) = mpsc::channel(runtime.prompt_queue_capacity);
    let demo = tokio::spawn(run_demo_driver(agent_sender, prompt_receiver));
    let (model_switch, _) = watch::channel(None);
    let result = run_agent_tui(
        agent_events,
        prompt_sender,
        model_switch,
        &TuiConfig::default(),
        &runtime,
        None,
    )
    .await;
    let _ = demo.await;
    result
}

async fn run_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    mut agent_events: mpsc::Receiver<AgentEvent>,
    prompts: PromptSender,
    model_switch: watch::Sender<Option<String>>,
    mut input_events: mpsc::Receiver<InputMessage>,
    mut state: AppState,
    notify: fn(&CompletionSettings, String) -> Result<()>,
) -> Result<()> {
    let mut agent_channel_open = true;

    loop {
        draw_if_dirty(terminal, &mut state)?;

        tokio::select! {
            input = input_events.recv() => {
                match input.context("terminal input thread stopped")? {
                    InputMessage::Key(key) => {
                        let action = state.reduce_key(key);
                        if dispatch_action(action, &prompts, &model_switch, &mut state)? {
                            return Ok(());
                        }
                    }
                    InputMessage::Resize => state.note_resize(),
                    InputMessage::Error(error) => anyhow::bail!("reading terminal input: {error}"),
                }
            }
            event = agent_events.recv(), if agent_channel_open => {
                match event {
                    Some(event) => {
                        state.reduce_agent_event(event);
                        while let Some(bell) = state.take_completion_bell() {
                            trigger_completion_bell(&state.completion, bell)?;
                        }
                        if let Some(message) = state.take_completion_message() {
                            if state.completion.notify {
                                if let Err(error) = notify(&state.completion, message) {
                                    let _ = writeln!(io::stderr(), "notification failed: {error}");
                                }
                            }
                        }
                    }
                    None => {
                        agent_channel_open = false;
                        state.note_runtime_closed();
                    }
                }
            }
            _ = wait_for_persistent_tick(state.next_persistent_tick) => {
                if let Some(message) = state.poll_persistent_notice() {
                    if state.completion.notify {
                        if let Err(error) = notify(&state.completion, message) {
                            let _ = writeln!(io::stderr(), "notification failed: {error}");
                        }
                    }
                }
            }
        }
    }
}

async fn wait_for_persistent_tick(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

fn draw_if_dirty<B: Backend>(terminal: &mut Terminal<B>, state: &mut AppState) -> Result<bool> {
    if !state.is_dirty() {
        return Ok(false);
    }
    let frame_started = Instant::now();
    terminal
        .draw(|frame| render(frame, state))
        .context("drawing TUI frame")?;
    state.mark_drawn(frame_started.elapsed());
    Ok(true)
}

fn dispatch_action(
    action: UiAction,
    prompts: &PromptSender,
    model_switch: &watch::Sender<Option<String>>,
    state: &mut AppState,
) -> Result<bool> {
    match action {
        UiAction::None => Ok(false),
        UiAction::Quit => Ok(true),
        UiAction::SwitchModel { model } => {
            model_switch.send_replace(Some(model.clone()));
            state.current_model = Some(model);
            Ok(false)
        }
        UiAction::Submit {
            prompt,
            rejected_input,
        } => match prompts.try_send(prompt.clone()) {
            Ok(()) => {
                state.note_prompt_submitted(&prompt);
                Ok(false)
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                state.note_prompt_busy(Some(rejected_input));
                Ok(false)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                state.note_prompt_busy(Some(rejected_input));
                Err(anyhow::anyhow!("agent prompt receiver closed"))
            }
        },
    }
}

fn trigger_completion_bell(config: &CompletionSettings, bell: CompletionBell) -> Result<()> {
    if !config.bell {
        return Ok(());
    }
    let repetitions = match bell {
        CompletionBell::Soft => 1,
        CompletionBell::Strong => 4,
    };
    for _ in 0..repetitions {
        let mut stdout = io::stdout();
        stdout.write_all(b"\x07")?;
        stdout.flush()?;
        if matches!(bell, CompletionBell::Strong) {
            thread::sleep(Duration::from_millis(80));
        }
    }
    Ok(())
}

fn trigger_completion_notification(_config: &CompletionSettings, message: String) -> Result<()> {
    Notification::new()
        .summary("Grey")
        .body(&message)
        .show()
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!(error))
}

fn render(frame: &mut Frame<'_>, state: &mut AppState) {
    let theme = state.settings.theme.colors.clone();
    let input_lines = state.settings.layout.input_lines.max(1);
    let chunks = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(input_lines),
        Constraint::Length(1),
    ])
    .split(frame.area());

    let conversation_block = Block::default()
        .borders(Borders::ALL)
        .title(" Grey ")
        .border_style(Style::default().fg(theme.border));
    let conversation_inner = conversation_block.inner(chunks[0]);
    state.update_viewport(conversation_inner.width, conversation_inner.height);
    let conversation = Paragraph::new(state.output.as_str())
        .block(conversation_block)
        .wrap(Wrap { trim: false })
        .scroll((state.scroll, 0));
    frame.render_widget(conversation, chunks[0]);

    let input_block = Block::default()
        .borders(Borders::ALL)
        .title(" input ")
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().fg(theme.accent));
    let input_inner = input_block.inner(chunks[1]);
    let prompt = Span::styled(
        "> ",
        Style::default()
            .fg(theme.prompt)
            .add_modifier(Modifier::BOLD),
    );
    let prompt_width = UnicodeWidthStr::width("> ");
    let input_width = usize::from(input_inner.width);
    let input_visible_rows = usize::from(input_inner.height);
    state.input_scroll(input_width, prompt_width, input_visible_rows);
    let mut input_text = Text::default();
    let mut input_lines = state.input.text.split('\n');
    if let Some(first) = input_lines.next() {
        input_text.push_line(Line::from(vec![prompt, Span::raw(first)]));
    }
    for rest in input_lines {
        input_text.push_line(Line::from(Span::raw(rest)));
    }
    let input = Paragraph::new(input_text)
        .block(input_block)
        .style(Style::default().fg(Color::White))
        .wrap(Wrap { trim: false })
        .scroll((state.input_scroll, 0));
    frame.render_widget(input, chunks[1]);
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

    render_status_line(frame, state, &theme, chunks[2]);
    if state.show_help {
        render_help_overlay(frame, state, &theme);
    }
}

fn render_status_line(frame: &mut Frame<'_>, state: &AppState, theme: &RenderTheme, area: Rect) {
    let stats = state.frame_stats();
    let labels = state.settings.keys.labels();
    let (total_input_tokens, total_output_tokens) = state.total_usage();
    let (provider_label, model_label) = state
        .model_info()
        .map_or(("-", "-"), |(provider, model)| (provider, model));
    let status = Line::from(vec![
        Span::styled(
            " GREY ",
            Style::default()
                .fg(theme.status_fg)
                .bg(theme.status_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            " {:5.1}ms {:4.0}fps frames:{} scroll {}/{} ",
            stats.ema_frame_ms(),
            stats.fps(),
            stats.frames(),
            state.scroll,
            state.max_scroll,
        )),
        Span::styled(
            format!(
                " task:{} model:{provider_label}/{model_label} branch:{} i:{} o:{} ",
                state.current_task_label(),
                state.branch_label(),
                total_input_tokens,
                total_output_tokens
            ),
            Style::default().fg(theme.status_fg),
        ),
        if state.status_has_error() {
            Span::styled(
                " [ERR] ",
                Style::default()
                    .fg(theme.error)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(" [OK] ", Style::default().fg(theme.success))
        },
        Span::styled(
            format!(" {} ", state.status),
            Style::default().fg(theme.status_fg),
        ),
        if state.completion_notice().is_some() {
            Span::styled(
                " [HOLD] ",
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("")
        },
        Span::styled(
            format!(
                " {}+{}:help  {}:quit  {}:clear  {}:up  {}:down ",
                labels.leader,
                labels.help,
                labels.quit,
                labels.clear,
                labels.scroll_up,
                labels.scroll_down
            ),
            Style::default().fg(theme.muted),
        ),
    ]);
    frame.render_widget(Paragraph::new(status), area);
}

fn render_help_overlay(frame: &mut Frame<'_>, state: &AppState, theme: &RenderTheme) {
    let area = centered_rect(60, 16, frame.area());
    let labels = state.settings.keys.labels();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Help ")
        .style(Style::default().fg(theme.accent))
        .border_style(Style::default().fg(theme.border));
    let body = Text::from(vec![
        Line::from("快捷键"),
        Line::from(""),
        Line::from(format!(
            " {} {}  显示/关闭快捷键帮助",
            labels.leader, labels.help
        )),
        Line::from(" Enter     发送输入"),
        Line::from(format!(" {}        退出", labels.quit)),
        Line::from(format!(" {}        清空输出", labels.clear)),
        Line::from(format!(
            " {} / {}  滚动输出",
            labels.scroll_up, labels.scroll_down
        )),
        Line::from(""),
        Line::from("状态栏含义"),
        Line::from(" task: 当前任务名".to_string()),
        Line::from(" model: 当前模型 (provider/model)".to_string()),
        Line::from(" branch: 当前仓库分支".to_string()),
        Line::from(" i/o: 累积输入输出 token".to_string()),
        Line::from(" [ERR]/[OK]: 最近事件状态"),
        Line::from("[HOLD]: 完成提醒等待回执".to_string()),
    ]);
    frame.render_widget(
        Paragraph::new(body).block(block).wrap(Wrap { trim: false }),
        area,
    );
}

fn centered_rect(width_percent: u16, height_percent: u16, area: Rect) -> Rect {
    let width = area.width.saturating_mul(width_percent).div_ceil(100);
    let height = area.height.saturating_mul(height_percent).div_ceil(100);
    let x = area
        .x
        .saturating_add((area.width.saturating_sub(width)).saturating_div(2));
    let y = area
        .y
        .saturating_add((area.height.saturating_sub(height)).saturating_div(2));
    Rect {
        x,
        y,
        width,
        height,
    }
}

async fn run_demo_driver(events: mpsc::Sender<AgentEvent>, mut prompts: mpsc::Receiver<String>) {
    if !send_demo_stream(&events, DEMO_REPLY).await {
        return;
    }
    if !finish_demo_turn(&events).await {
        return;
    }
    while let Some(prompt) = prompts.recv().await {
        let reply = format!("（模拟回复）{prompt}\n");
        if !send_demo_stream(&events, &reply).await {
            return;
        }
        if !finish_demo_turn(&events).await {
            return;
        }
    }
}

async fn finish_demo_turn(events: &mpsc::Sender<AgentEvent>) -> bool {
    events
        .send(AgentEvent::Completed {
            usage: grey_core::Usage::default(),
            steps: 1,
            provider: "mock".into(),
            model: "mock".into(),
        })
        .await
        .is_ok()
}

async fn send_demo_stream(events: &mpsc::Sender<AgentEvent>, text: &str) -> bool {
    let characters: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < characters.len() {
        let chunk_len = (index % 5) + 2;
        let end = (index + chunk_len).min(characters.len());
        let chunk: String = characters[index..end].iter().collect();
        if events.send(AgentEvent::Delta(chunk)).await.is_err() {
            return false;
        }
        index = end;
        tokio::time::sleep(Duration::from_millis(18)).await;
    }
    true
}

type RestoreFn = fn() -> io::Result<()>;

#[must_use = "dropping the guard restores the terminal"]
struct TerminalRestoreGuard {
    restore: Option<RestoreFn>,
}

impl TerminalRestoreGuard {
    fn new() -> Self {
        Self {
            restore: Some(restore_terminal),
        }
    }

    #[cfg(test)]
    fn with_restore(restore: RestoreFn) -> Self {
        Self {
            restore: Some(restore),
        }
    }
}

impl Drop for TerminalRestoreGuard {
    fn drop(&mut self) {
        if let Some(restore) = self.restore.take() {
            let _ = restore();
        }
    }
}

fn enter_terminal() -> Result<TerminalRestoreGuard> {
    enable_raw_mode().context("enabling terminal raw mode")?;
    let guard = TerminalRestoreGuard::new();
    execute!(io::stdout(), EnterAlternateScreen, Hide)
        .context("entering terminal alternate screen")?;
    Ok(guard)
}

fn restore_terminal() -> io::Result<()> {
    let screen_result = execute!(io::stdout(), Show, LeaveAlternateScreen);
    let raw_result = disable_raw_mode();
    screen_result.and(raw_result)
}

#[derive(Debug)]
enum InputMessage {
    Key(KeyEvent),
    Resize,
    Error(String),
}

struct InputThread {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl InputThread {
    fn spawn(capacity: usize) -> Result<(Self, mpsc::Receiver<InputMessage>)> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (sender, receiver) = mpsc::channel(capacity);
        let handle = std::thread::Builder::new()
            .name("grey-tui-input".into())
            .spawn(move || read_input(thread_stop, sender))
            .context("spawning terminal input thread")?;
        Ok((
            Self {
                stop,
                handle: Some(handle),
            },
            receiver,
        ))
    }

    fn stop_and_join(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("terminal input thread panicked"))?;
        }
        Ok(())
    }
}

impl Drop for InputThread {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

fn read_input(stop: Arc<AtomicBool>, sender: mpsc::Sender<InputMessage>) {
    while !stop.load(Ordering::Acquire) {
        match event::poll(INPUT_POLL_INTERVAL) {
            Ok(false) => {}
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) => {
                    if !send_input(&stop, &sender, InputMessage::Key(key)) {
                        return;
                    }
                }
                Ok(Event::Resize(_, _)) => {
                    if !send_input(&stop, &sender, InputMessage::Resize) {
                        return;
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = send_input(&stop, &sender, InputMessage::Error(error.to_string()));
                    return;
                }
            },
            Err(error) => {
                let _ = send_input(&stop, &sender, InputMessage::Error(error.to_string()));
                return;
            }
        }
    }
}

fn send_input(
    stop: &AtomicBool,
    sender: &mpsc::Sender<InputMessage>,
    mut message: InputMessage,
) -> bool {
    loop {
        if stop.load(Ordering::Acquire) {
            return false;
        }
        match sender.try_send(message) {
            Ok(()) => return true,
            Err(mpsc::error::TrySendError::Closed(_)) => return false,
            Err(mpsc::error::TrySendError::Full(pending)) => {
                message = pending;
                std::thread::park_timeout(INPUT_POLL_INTERVAL);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crossterm::event::{KeyEvent, KeyEventKind};
    use grey_core::{ContextAudit, ToolCall, ToolResult};
    use ratatui::{backend::TestBackend, Terminal};

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn key_with(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn model_switch() -> watch::Sender<Option<String>> {
        let (sender, _) = watch::channel(None);
        sender
    }

    #[test]
    fn input_reducer_edits_submits_and_handles_every_exit_key() {
        let mut state = AppState::default();
        assert_eq!(state.reduce_key(key(KeyCode::Char('h'))), UiAction::None);
        assert_eq!(state.reduce_key(key(KeyCode::Char('i'))), UiAction::None);
        assert_eq!(state.input(), "hi");
        assert_eq!(
            state.reduce_key(key(KeyCode::Enter)),
            UiAction::Submit {
                prompt: "hi".into(),
                rejected_input: "hi".into(),
            }
        );
        assert_eq!(state.input(), "");

        assert_eq!(
            AppState::default().reduce_key(key(KeyCode::Char('q'))),
            UiAction::Quit
        );
        let mut q_in_prompt = AppState::default();
        q_in_prompt.reduce_key(key(KeyCode::Char('a')));
        assert_eq!(
            q_in_prompt.reduce_key(key(KeyCode::Char('q'))),
            UiAction::None
        );
        assert_eq!(q_in_prompt.input(), "aq");
        assert_eq!(
            AppState::default().reduce_key(key(KeyCode::Esc)),
            UiAction::Quit
        );
        assert_eq!(
            AppState::default()
                .reduce_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL,)),
            UiAction::Quit
        );

        let mut released = key(KeyCode::Char('x'));
        released.kind = KeyEventKind::Release;
        assert_eq!(state.reduce_key(released), UiAction::None);
        assert_eq!(state.input(), "");
    }

    fn type_text(state: &mut AppState, text: &str) {
        for character in text.chars() {
            assert_eq!(
                state.reduce_key(key(KeyCode::Char(character))),
                UiAction::None
            );
        }
    }

    #[test]
    fn slash_commands_dispatch_locally_or_switch_model() {
        let mut help = AppState::default();
        type_text(&mut help, "/help");
        assert_eq!(help.reduce_key(key(KeyCode::Enter)), UiAction::None);
        assert!(help.show_help);
        assert_eq!(help.input(), "");

        let mut clear = AppState::default();
        clear.output.push_str("old transcript");
        type_text(&mut clear, "/clear");
        assert_eq!(clear.reduce_key(key(KeyCode::Enter)), UiAction::None);
        assert!(clear.output.is_empty());

        assert_eq!(
            AppState::default().reduce_key(key(KeyCode::Enter)),
            UiAction::None,
            "empty input stays inert"
        );

        for command in ["/quit", "/exit"] {
            let mut quitting = AppState::default();
            type_text(&mut quitting, command);
            assert_eq!(quitting.reduce_key(key(KeyCode::Enter)), UiAction::Quit);
        }

        let mut model = AppState::default();
        type_text(&mut model, "/model deepseek-v4-flash-ga-260731");
        assert_eq!(
            model.reduce_key(key(KeyCode::Enter)),
            UiAction::SwitchModel {
                model: "deepseek-v4-flash-ga-260731".into()
            }
        );
        assert_eq!(model.input(), "");

        let mut missing = AppState::default();
        type_text(&mut missing, "/model");
        assert_eq!(missing.reduce_key(key(KeyCode::Enter)), UiAction::None);
        assert!(missing.status_error);
        assert_eq!(missing.input(), "/model", "kept for editing");

        let mut unknown = AppState::default();
        type_text(&mut unknown, "/nope");
        assert_eq!(unknown.reduce_key(key(KeyCode::Enter)), UiAction::None);
        assert!(unknown.status.contains("unknown command /nope"));
        assert_eq!(unknown.input(), "/nope", "kept for editing");

        let mut normal = AppState::default();
        type_text(&mut normal, "not a command");
        assert_eq!(
            normal.reduce_key(key(KeyCode::Enter)),
            UiAction::Submit {
                prompt: "not a command".into(),
                rejected_input: "not a command".into(),
            }
        );
    }

    #[test]
    fn slash_command_parse_normalizes_names_and_arguments() {
        use SlashCommand as Command;
        assert_eq!(Command::parse("/HELP"), Command::Help);
        assert_eq!(Command::parse("/?"), Command::Help);
        assert_eq!(Command::parse("/clear   "), Command::Clear);
        assert_eq!(Command::parse("/exit"), Command::Quit);
        assert_eq!(
            Command::parse("/model  deepseek-v4"),
            Command::Model {
                model: "deepseek-v4".into()
            }
        );
        assert_eq!(
            Command::parse("/model"),
            Command::Model {
                model: String::new()
            }
        );
        assert_eq!(Command::parse("/wat"), Command::Unknown("wat".into()));
    }

    #[test]
    fn multiline_input_supports_newlines_and_line_navigation() {
        let mut state = AppState::default();
        type_text(&mut state, "first");
        assert_eq!(
            state.reduce_key(key_with(KeyCode::Enter, KeyModifiers::SHIFT)),
            UiAction::None
        );
        type_text(&mut state, "second");
        assert_eq!(state.input(), "first\nsecond");

        assert_eq!(
            state.reduce_key(key_with(KeyCode::Enter, KeyModifiers::ALT)),
            UiAction::None
        );
        type_text(&mut state, "third");
        assert_eq!(state.input(), "first\nsecond\nthird");
        assert_eq!(state.input_cursor_position(100, 2), (5, 2));

        assert_eq!(state.reduce_key(key(KeyCode::Up)), UiAction::None);
        assert_eq!(state.input_cursor_position(100, 2), (5, 1));
        assert_eq!(state.reduce_key(key(KeyCode::Up)), UiAction::None);
        assert_eq!(state.input_cursor_position(100, 2), (5, 0));
        assert_eq!(state.reduce_key(key(KeyCode::Up)), UiAction::None);

        assert_eq!(state.reduce_key(key(KeyCode::Home)), UiAction::None);
        assert_eq!(state.input_cursor_position(100, 2), (0, 0));
        assert_eq!(state.reduce_key(key(KeyCode::End)), UiAction::None);
        assert_eq!(state.input_cursor_position(100, 2), (5, 0));

        assert_eq!(
            state.reduce_key(key(KeyCode::Enter)),
            UiAction::Submit {
                prompt: "first\nsecond\nthird".into(),
                rejected_input: "first\nsecond\nthird".into(),
            }
        );
    }

    #[test]
    fn trailing_backslash_enters_newline_instead_of_submitting() {
        let mut state = AppState::default();
        type_text(&mut state, "path/to\\");
        assert_eq!(state.reduce_key(key(KeyCode::Enter)), UiAction::None);
        assert_eq!(state.input(), "path/to\n");
        assert_eq!(
            state.reduce_key(key(KeyCode::Enter)),
            UiAction::Submit {
                prompt: "path/to".into(),
                rejected_input: "path/to\n".into(),
            }
        );
    }

    #[test]
    fn multiline_cursor_position_wraps_long_first_line() {
        let mut state = AppState::default();
        type_text(&mut state, "01234567890123456789012345\nsecond");
        // width 10, prompt 2: first line effective width 8 -> 4 wrapped rows
        assert_eq!(state.input_cursor_position(10, 2), (6, 4));
    }

    #[test]
    fn input_cursor_uses_unicode_display_columns_and_supports_midline_edits() {
        let mut state = AppState::default();
        state.reduce_key(key(KeyCode::Char('a')));
        state.reduce_key(key(KeyCode::Char('界')));
        assert_eq!(state.input_cursor_column(), 3);

        state.reduce_key(key(KeyCode::Left));
        assert_eq!(state.input_cursor_column(), 1);
        state.reduce_key(key(KeyCode::Char('b')));
        assert_eq!(state.input(), "ab界");
        assert_eq!(state.input_cursor_column(), 2);
        state.reduce_key(key(KeyCode::Delete));
        assert_eq!(state.input(), "ab");
    }

    #[test]
    fn agent_event_reducer_consumes_core_events_without_loss() {
        let call = ToolCall {
            id: "call-1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "src/lib.rs"}),
        };
        let mut state = AppState::default();
        state.mark_drawn(Duration::from_millis(4));
        assert!(!state.is_dirty());

        state.reduce_agent_event(AgentEvent::Delta("你好".into()));
        state.reduce_agent_event(AgentEvent::ToolStarted(call.clone()));
        state.reduce_agent_event(AgentEvent::ToolFinished(ToolResult::success(
            &call, "contents",
        )));
        state.reduce_agent_event(AgentEvent::Retry {
            attempt: 2,
            error: "temporary".into(),
        });
        state.reduce_agent_event(AgentEvent::ContextTrimmed(ContextAudit {
            original_chars: 2000,
            retained_chars: 1000,
            dropped_messages: 3,
            retained_tokens: 1000,
            summary_created: false,
            tool_outputs_deduplicated: 0,
            tool_outputs_truncated: 0,
            budget: grey_core::context::TokenBudget::default(),
        }));
        state.reduce_agent_event(AgentEvent::Completed {
            usage: grey_core::Usage {
                input_tokens: 7,
                output_tokens: 11,
            },
            steps: 2,
            provider: "provider".into(),
            model: "model".into(),
        });
        state.reduce_agent_event(AgentEvent::Failed("fatal".into()));

        assert!(state.output().contains("你好"));
        assert!(state.output().contains("read_file"));
        assert!(state.output().contains("contents"));
        assert!(state.output().contains("3 messages"));
        assert!(state.output().contains("7 input / 11 output"));
        assert!(state.status().contains("fatal"));
        assert!(state.is_dirty());
    }

    #[test]
    fn scroll_is_clamped_follows_new_output_and_is_applied_to_rendering() {
        let mut empty = AppState::default();
        empty.reduce_key(key(KeyCode::PageUp));
        assert!(empty.follows_output());

        let mut state = AppState::default();
        state.reduce_agent_event(AgentEvent::Delta(
            (0..9)
                .map(|line| format!("line-{line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        state.update_viewport(28, 4);
        assert_eq!(state.scroll(), 5);

        state.reduce_key(key(KeyCode::PageUp));
        assert_eq!(state.scroll(), 0);
        assert!(!state.follows_output());
        state.reduce_key(key(KeyCode::PageDown));
        assert_eq!(state.scroll(), 5);
        assert!(state.follows_output());

        let backend = TestBackend::new(30, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut state)).unwrap();
        let tail_row: String = terminal.backend().buffer().content[31..59]
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            tail_row.contains("line-5"),
            "non-zero scroll offset was not rendered: {tail_row:?}"
        );

        state.reduce_key(key(KeyCode::PageUp));
        terminal.draw(|frame| render(frame, &mut state)).unwrap();
        let row: String = terminal.backend().buffer().content[31..59]
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            row.contains("line-0"),
            "scroll offset was not rendered: {row:?}"
        );
    }

    #[test]
    fn tui_settings_apply_tui_config_layout_and_completion() {
        let mut config = TuiConfig::default();
        config.theme.preset = "slate".into();
        config.theme.overrides.border = Some("#1f2937".into());
        config.theme.overrides.prompt = Some("red".into());
        config.layout.input_lines = 6;
        config.completion.enabled = false;
        config.completion.long_running_steps = 2;
        config.completion.long_running_seconds = 45;

        let settings = TuiSettings::from(&config);
        assert_eq!(settings.layout.input_lines, 6);
        assert!(!settings.completion.enabled);
        assert_eq!(settings.completion.long_running_steps, 2);
        assert_eq!(settings.completion.long_running_seconds, 45);
        assert_eq!(settings.theme.colors.border, Color::Rgb(31, 41, 55));
        assert_eq!(settings.theme.colors.prompt, Color::Red);

        let mut state = AppState::with_settings(settings);
        state.turn_started_at = Some(Instant::now());
        state.reduce_agent_event(AgentEvent::Completed {
            usage: grey_core::Usage::default(),
            steps: 3,
            provider: "provider".into(),
            model: "model".into(),
        });
        assert_eq!(state.take_completion_bell(), None);
    }

    #[test]
    fn grey_storm_default_and_overrides_use_exact_tokens() {
        let mut config = TuiConfig::default();
        let theme = TuiTheme::from_config(&config.theme);
        assert_eq!(theme.colors.border, Color::Rgb(0x1d, 0x55, 0x5a));
        assert_eq!(theme.colors.accent, Color::Rgb(0x44, 0xe0, 0xd3));
        assert_eq!(theme.colors.prompt, Color::Rgb(0x89, 0xff, 0xf2));
        assert_eq!(theme.colors.status_fg, Color::Rgb(0xd7, 0xfa, 0xf7));
        assert_eq!(theme.colors.status_bg, Color::Rgb(0x12, 0x38, 0x3b));
        assert_eq!(theme.colors.muted, Color::Rgb(0x6f, 0x8f, 0x90));
        assert_eq!(theme.colors.error, Color::Rgb(0xff, 0x7b, 0x72));
        assert_eq!(theme.colors.success, Color::Green);
        assert_eq!(theme.colors.warning, Color::Yellow);

        config.theme.overrides.error = Some("#010203".into());
        config.theme.overrides.success = Some("#040506".into());
        config.theme.overrides.warning = Some("#070809".into());
        assert_eq!(
            TuiTheme::from_config(&config.theme).colors.error,
            Color::Rgb(1, 2, 3)
        );
        assert_eq!(
            TuiTheme::from_config(&config.theme).colors.success,
            Color::Rgb(4, 5, 6)
        );
        assert_eq!(
            TuiTheme::from_config(&config.theme).colors.warning,
            Color::Rgb(7, 8, 9)
        );
    }

    #[test]
    fn legacy_presets_and_theme_manifest_validation_remain_compatible() {
        let mut config = TuiConfig::default();
        config.theme.preset = "slate".into();
        let slate = TuiTheme::from_config(&config.theme);
        assert_eq!(slate.colors.border, Color::Blue);
        assert_eq!(slate.colors.error, Color::LightRed);
        assert!(theme_config_is_valid(&config.theme));

        config.theme.preset = "grey_storm".into();
        config.theme.overrides.error = Some("#ff7b72".into());
        assert!(theme_config_is_valid(&config.theme));
        config.theme.overrides.error = Some("not-a-colour".into());
        assert!(!theme_config_is_valid(&config.theme));
    }

    #[test]
    fn status_indicators_render_theme_tokens() {
        let mut state = AppState::default();
        let mut terminal = Terminal::new(TestBackend::new(100, 10)).unwrap();
        terminal.draw(|frame| render(frame, &mut state)).unwrap();
        assert!(terminal
            .backend()
            .buffer()
            .content
            .iter()
            .any(|cell| cell.symbol() == "[" && cell.fg == Color::Green));

        let mut config = TuiConfig::default();
        config.completion.persistent = true;
        let mut held = AppState::with_settings(TuiSettings::from(&config));
        held.schedule_completion_notice("done".into());
        terminal.draw(|frame| render(frame, &mut held)).unwrap();
        assert!(terminal
            .backend()
            .buffer()
            .content
            .iter()
            .any(|cell| cell.symbol() == "[" && cell.fg == Color::Yellow));

        state.reduce_agent_event(AgentEvent::Failed("boom".into()));
        terminal.draw(|frame| render(frame, &mut state)).unwrap();
        let error = Color::Rgb(0xff, 0x7b, 0x72);
        assert!(terminal
            .backend()
            .buffer()
            .content
            .iter()
            .any(|cell| cell.symbol() == "[" && cell.fg == error));
    }

    #[test]
    fn completion_reminder_respects_steps_threshold() {
        let mut config = TuiConfig::default();
        config.completion.enabled = true;
        config.completion.strong_bell = false;
        config.completion.long_running_steps = 2;

        let mut state = AppState::with_settings(TuiSettings::from(&config));
        state.turn_started_at = Some(Instant::now());
        state.reduce_agent_event(AgentEvent::Completed {
            usage: grey_core::Usage::default(),
            steps: 3,
            provider: "provider".into(),
            model: "model".into(),
        });
        assert_eq!(state.take_completion_bell(), Some(CompletionBell::Soft));
    }

    #[test]
    fn completion_reminder_ignored_without_threshold_hit() {
        let mut config = TuiConfig::default();
        config.completion.enabled = true;
        config.completion.long_running_steps = 99999;
        config.completion.long_running_seconds = 0;

        let mut state = AppState::with_settings(TuiSettings::from(&config));
        state.turn_started_at = Some(Instant::now() - Duration::from_secs(2));
        state.reduce_agent_event(AgentEvent::Failed("oops".into()));
        assert_eq!(state.take_completion_bell(), None);
    }

    #[test]
    fn completion_notice_persistence_controls_hold_and_repeat_interval() {
        let mut config = TuiConfig::default();
        config.completion.notify = true;
        config.completion.persistent = true;
        let mut state = AppState::with_settings(TuiSettings::from(&config));
        state.schedule_completion_notice("done".into());
        assert!(state.completion_notice().is_some());

        assert!(state.poll_persistent_notice().is_none());

        state.next_persistent_tick = Some(Instant::now() - Duration::from_millis(1));
        assert_eq!(state.poll_persistent_notice(), Some("done".to_string()));
        assert_eq!(state.poll_persistent_notice(), None);
    }

    #[test]
    fn completion_notice_is_cleared_on_new_turn() {
        let mut config = TuiConfig::default();
        config.completion.notify = true;
        config.completion.persistent = true;
        let mut state = AppState::with_settings(TuiSettings::from(&config));
        state.schedule_completion_notice("done".into());
        assert!(state.completion_notice().is_some());

        state.clear_completion_notice();
        assert!(state.completion_notice().is_none());
        assert!(state.poll_persistent_notice().is_none());
    }

    #[test]
    fn parse_keybinding_supports_special_keys() {
        let ctrl_c = parse_keybinding("ctrl-c").expect("ctrl-c");
        assert!(ctrl_c.matches(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));

        let page_up = parse_keybinding("pageup").expect("pageup");
        assert!(page_up.matches(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)));

        let leader_backslash = parse_keybinding("\\").expect("leader");
        assert!(leader_backslash.matches(KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::NONE)));
    }

    #[test]
    fn dirty_redraw_and_frame_metrics_are_explicit_and_testable() {
        let mut state = AppState::default();
        assert!(state.is_dirty());
        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        assert!(draw_if_dirty(&mut terminal, &mut state).unwrap());
        assert!(!state.is_dirty());
        assert_eq!(state.frame_stats().frames(), 1);
        assert!(state.frame_stats().fps().is_finite());
        assert!(!draw_if_dirty(&mut terminal, &mut state).unwrap());
        assert_eq!(state.frame_stats().frames(), 1);

        state.reduce_agent_event(AgentEvent::Delta("x".into()));
        assert!(state.is_dirty());
        assert!(draw_if_dirty(&mut terminal, &mut state).unwrap());
        assert_eq!(state.frame_stats().frames(), 2);

        let mut metrics = FrameStats::default();
        metrics.record(Duration::from_millis(10));
        assert_eq!(metrics.last_frame(), Duration::from_millis(10));
        assert!((metrics.fps() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn bounded_transcript_keeps_latest_utf8_with_one_marker_for_all_events() {
        let cap = grey_core::RuntimeConfig::default().transcript_max_bytes;
        let mut state = AppState::default();
        state.reduce_agent_event(AgentEvent::Delta("甲".repeat(cap / 3 + 1)));
        let call = ToolCall {
            id: "call-1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({}),
        };
        state.reduce_agent_event(AgentEvent::ToolFinished(ToolResult::success(
            &call,
            "乙".repeat(cap / 3 + 1),
        )));
        state.reduce_agent_event(AgentEvent::Failed("最终错误".repeat(cap / 12 + 1)));

        assert!(state.output().len() <= cap);
        assert!(state.output().is_char_boundary(state.output().len()));
        assert_eq!(state.output().matches("[cut]").count(), 1);
        assert!(state.output().ends_with("最终错误\n"));
    }

    #[tokio::test]
    async fn bounded_prompt_busy_rejects_second_submission_and_preserves_input() {
        let (sender, mut receiver) = mpsc::channel(1);
        let mut state = AppState::default();
        for character in "first".chars() {
            state.reduce_key(key(KeyCode::Char(character)));
        }
        let first = state.reduce_key(key(KeyCode::Enter));
        assert!(!dispatch_action(first, &sender, &model_switch(), &mut state).unwrap());

        for character in "retry".chars() {
            state.reduce_key(key(KeyCode::Char(character)));
        }
        let second = state.reduce_key(key(KeyCode::Enter));
        assert!(!dispatch_action(second, &sender, &model_switch(), &mut state).unwrap());

        assert_eq!(state.input(), "retry");
        assert!(state.status().contains("busy"));
        assert_eq!(receiver.recv().await.as_deref(), Some("first"));
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn bounded_prompt_full_race_restores_rejected_input() {
        let (sender, _receiver) = mpsc::channel(1);
        sender.try_send("already queued".into()).unwrap();
        let mut state = AppState::default();

        assert!(!dispatch_action(
            UiAction::Submit {
                prompt: "retry me".into(),
                rejected_input: "retry me".into(),
            },
            &sender,
            &model_switch(),
            &mut state,
        )
        .unwrap());

        assert_eq!(state.input(), "retry me");
        assert!(state.status().contains("busy"));
    }

    #[tokio::test]
    async fn bounded_prompt_rejection_preserves_typed_whitespace() {
        let (sender, _receiver) = mpsc::channel(1);
        sender.try_send("already queued".into()).unwrap();
        let mut state = AppState::default();
        for character in " retry ".chars() {
            state.reduce_key(key(KeyCode::Char(character)));
        }

        let action = state.reduce_key(key(KeyCode::Enter));
        assert!(!dispatch_action(action, &sender, &model_switch(), &mut state).unwrap());

        assert_eq!(state.input(), " retry ");
    }

    #[tokio::test]
    async fn ancillary_warning_never_releases_a_newer_turn_busy_guard() {
        let (sender, mut receiver) = mpsc::channel(1);
        let mut state = AppState::default();
        dispatch_action(
            UiAction::Submit {
                prompt: "turn-a".into(),
                rejected_input: "turn-a".into(),
            },
            &sender,
            &model_switch(),
            &mut state,
        )
        .unwrap();
        assert_eq!(receiver.recv().await.as_deref(), Some("turn-a"));

        state.reduce_agent_event(AgentEvent::Warning("usage save failed".into()));
        for character in "turn-b".chars() {
            state.reduce_key(key(KeyCode::Char(character)));
        }
        assert_eq!(state.reduce_key(key(KeyCode::Enter)), UiAction::None);
        assert_eq!(state.input(), "turn-b");

        state.reduce_agent_event(AgentEvent::Completed {
            usage: grey_core::Usage::default(),
            steps: 1,
            provider: "provider".into(),
            model: "model".into(),
        });
        let turn_b = state.reduce_key(key(KeyCode::Enter));
        dispatch_action(turn_b, &sender, &model_switch(), &mut state).unwrap();
        assert_eq!(receiver.recv().await.as_deref(), Some("turn-b"));

        state.reduce_agent_event(AgentEvent::Warning("completion hook failed".into()));
        for character in "turn-c".chars() {
            state.reduce_key(key(KeyCode::Char(character)));
        }
        assert_eq!(state.reduce_key(key(KeyCode::Enter)), UiAction::None);
        assert_eq!(state.input(), "turn-c");
    }

    static REMINDER_NOTIFICATIONS: AtomicUsize = AtomicUsize::new(0);

    fn count_notification(_config: &CompletionSettings, _message: String) -> Result<()> {
        REMINDER_NOTIFICATIONS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    #[tokio::test]
    async fn persistent_reminder_timer_wakes_without_input_or_agent_event() {
        REMINDER_NOTIFICATIONS.store(0, Ordering::SeqCst);
        let mut config = TuiConfig::default();
        config.completion.notify = true;
        config.completion.persistent = true;
        let mut state = AppState::with_settings(TuiSettings::from(&config));
        state.schedule_completion_notice("done".into());
        state.next_persistent_tick = Some(Instant::now() + Duration::from_millis(5));

        let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
        let (_agent_sender, agent_events) = mpsc::channel(1);
        let (prompt_sender, _prompt_receiver) = mpsc::channel(1);
        let (input_sender, input_events) = mpsc::channel(1);
        let quit = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            input_sender
                .send(InputMessage::Key(key(KeyCode::Char('q'))))
                .await
                .unwrap();
        });

        run_loop(
            &mut terminal,
            agent_events,
            prompt_sender,
            model_switch(),
            input_events,
            state,
            count_notification,
        )
        .await
        .unwrap();
        quit.await.unwrap();
        assert_eq!(REMINDER_NOTIFICATIONS.load(Ordering::SeqCst), 1);
    }

    #[test]
    #[ignore = "P6-performance-smoke"]
    fn p6_render_pipeline_smoke_fps_baseline() {
        let mut state = AppState::default();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let iterations = 400u32;
        let mut longest_delta_ms = 0.0_f64;
        let start = std::time::Instant::now();

        for i in 0..iterations {
            state.reduce_agent_event(AgentEvent::Delta(format!("line-{i}")));
            state.update_viewport(100, 30);
            assert!(draw_if_dirty(&mut terminal, &mut state).unwrap());
            let frame_ms = state.frame_stats().last_frame().as_secs_f64() * 1000.0;
            if frame_ms > longest_delta_ms {
                longest_delta_ms = frame_ms;
            }
        }
        let elapsed = start.elapsed();
        let fps = iterations as f64 / elapsed.as_secs_f64();
        let avg_ms = elapsed.as_secs_f64() * 1000.0 / iterations as f64;
        println!("P6_RENDER_FPS={fps}");
        println!("P6_RENDER_AVG_MS={avg_ms}");
        println!("P6_RENDER_MAX_FRAME_MS={longest_delta_ms}");
        assert!(fps >= 15.0);
        assert!(avg_ms <= 66.0);
        assert!(longest_delta_ms <= 1000.0);
    }

    #[tokio::test]
    async fn submit_action_is_forwarded_through_the_prompt_sender() {
        let (sender, mut receiver) = mpsc::channel(1);
        let mut state = AppState::default();
        assert!(!dispatch_action(
            UiAction::Submit {
                prompt: "inspect".into(),
                rejected_input: "inspect".into(),
            },
            &sender,
            &model_switch(),
            &mut state,
        )
        .unwrap());
        assert_eq!(receiver.recv().await.as_deref(), Some("inspect"));
        assert!(dispatch_action(UiAction::Quit, &sender, &model_switch(), &mut state).unwrap());
    }

    static RESTORE_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn count_restore() -> io::Result<()> {
        RESTORE_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    #[test]
    fn terminal_restore_is_owned_by_an_raii_guard() {
        RESTORE_CALLS.store(0, Ordering::SeqCst);
        {
            let _guard = TerminalRestoreGuard::with_restore(count_restore);
            assert_eq!(RESTORE_CALLS.load(Ordering::SeqCst), 0);
        }
        assert_eq!(RESTORE_CALLS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn input_thread_shutdown_sets_stop_and_joins() {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let exited = Arc::new(AtomicBool::new(false));
        let thread_exited = Arc::clone(&exited);
        let handle = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            thread_exited.store(true, Ordering::Release);
        });
        let mut input = InputThread {
            stop,
            handle: Some(handle),
        };

        input.stop_and_join().unwrap();

        assert!(exited.load(Ordering::Acquire));
        assert!(input.handle.is_none());
    }

    #[test]
    fn full_input_queue_still_stops_without_waiting_for_receiver_drop() {
        let (sender, _receiver) = mpsc::channel(1);
        sender.try_send(InputMessage::Resize).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (done, finished) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let sent = send_input(&thread_stop, &sender, InputMessage::Resize);
            done.send(sent).unwrap();
        });
        std::thread::sleep(Duration::from_millis(10));

        stop.store(true, Ordering::Release);

        assert!(!finished.recv_timeout(Duration::from_millis(250)).unwrap());
        handle.join().unwrap();
    }
}
