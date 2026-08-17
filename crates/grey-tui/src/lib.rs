//! Grey's incremental terminal UI.
//!
//! The runtime-facing entry point consumes [`grey_core::AgentEvent`] values
//! and sends submitted prompts back over a Tokio channel. Input and runtime
//! events are reduced into [`AppState`] before rendering, which keeps terminal
//! I/O out of the behavior tests. [`run_stream_demo`] retains the P0 streaming
//! benchmark while exercising the same event path as a real agent.

use std::io::{self, Stdout};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use grey_core::AgentEvent;
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame, Terminal,
};
use tokio::sync::mpsc;
use unicode_width::UnicodeWidthStr;

const DEMO_REPLY: &str = "Grey 是一个轻量、高性能、可扩展的代码 Agent Harness。\n\n这是 Spike A 的模拟流式输出：消息按小块持续流入 TUI 并增量渲染，状态栏实时显示帧耗时与渲染频率。输入内容后回车会触发一轮新的模拟回复，Esc、Ctrl-C 或空输入时按 q 退出。";
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(80);
const SCROLL_PAGE_LINES: u16 = 5;

/// Prompts submitted by the TUI are sent to the owner of the agent loop.
pub type PromptSender = mpsc::UnboundedSender<String>;

/// A side effect requested by the otherwise-pure input reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiAction {
    None,
    Submit(String),
    Quit,
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
        let changed = self.cursor_chars != 0;
        self.cursor_chars = 0;
        changed
    }

    fn move_end(&mut self) -> bool {
        let end = self.text.chars().count();
        let changed = self.cursor_chars != end;
        self.cursor_chars = end;
        changed
    }

    fn take_trimmed(&mut self) -> String {
        let prompt = self.text.trim().to_owned();
        self.text.clear();
        self.cursor_chars = 0;
        prompt
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
    input: InputBuffer,
    status: String,
    scroll: u16,
    max_scroll: u16,
    follow_output: bool,
    dirty: bool,
    frame_stats: FrameStats,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            output: String::new(),
            input: InputBuffer::default(),
            status: "ready".into(),
            scroll: 0,
            max_scroll: 0,
            follow_output: true,
            dirty: true,
            frame_stats: FrameStats::default(),
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

    /// Reduce one terminal key event into state and, optionally, an action.
    pub fn reduce_key(&mut self, key: KeyEvent) -> UiAction {
        if key.kind == KeyEventKind::Release {
            return UiAction::None;
        }
        if key.code == KeyCode::Esc
            || key.code == KeyCode::Char('q')
                && key.modifiers.is_empty()
                && self.input.text.is_empty()
            || matches!(key.code, KeyCode::Char('c' | 'C'))
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
            KeyCode::Home => self.input.move_home(),
            KeyCode::End => self.input.move_end(),
            KeyCode::PageUp => {
                let previous = self.scroll;
                self.scroll = previous.saturating_sub(SCROLL_PAGE_LINES);
                let changed = self.scroll != previous;
                if changed {
                    self.follow_output = false;
                }
                changed
            }
            KeyCode::PageDown => {
                self.scroll = self
                    .scroll
                    .saturating_add(SCROLL_PAGE_LINES)
                    .min(self.max_scroll);
                self.follow_output = self.scroll == self.max_scroll;
                true
            }
            KeyCode::Enter => {
                let prompt = self.input.take_trimmed();
                self.dirty = true;
                if prompt.is_empty() {
                    return UiAction::None;
                }
                if !self.output.is_empty() && !self.output.ends_with('\n') {
                    self.output.push('\n');
                }
                self.output.push_str("> ");
                self.output.push_str(&prompt);
                self.output.push_str("\n\n");
                self.follow_output = true;
                self.status = "prompt submitted".into();
                return UiAction::Submit(prompt);
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
                self.output.push_str(&delta);
                self.status = "streaming".into();
            }
            AgentEvent::ToolStarted(call) => {
                self.output.push_str(&format!(
                    "\n[tool:start] {} {}\n",
                    call.name, call.arguments
                ));
                self.status = format!("running {}", call.name);
            }
            AgentEvent::ToolFinished(result) => {
                let outcome = if result.success { "ok" } else { "failed" };
                self.output.push_str(&format!(
                    "[tool:{outcome}] {}\n{}\n",
                    result.name, result.output
                ));
                self.status = format!("{} {outcome}", result.name);
            }
            AgentEvent::Retry { attempt, error } => {
                self.output
                    .push_str(&format!("\n[retry {attempt}] {error}\n"));
                self.status = format!("retry {attempt}: {error}");
            }
            AgentEvent::ContextTrimmed(audit) => {
                self.output.push_str(&format!(
                    "\n[context] dropped {} messages ({} -> {} chars)\n",
                    audit.dropped_messages, audit.original_chars, audit.retained_chars
                ));
                self.status = format!("context trimmed: {} dropped", audit.dropped_messages);
            }
            AgentEvent::Completed { usage, steps } => {
                self.output.push_str(&format!(
                    "\n[completed] {steps} steps, {} input / {} output tokens\n",
                    usage.input_tokens, usage.output_tokens
                ));
                self.status = format!("completed in {steps} steps");
            }
            AgentEvent::Failed(error) => {
                self.output.push_str(&format!("\n[failed] {error}\n"));
                self.status = format!("failed: {error}");
            }
        }
        self.dirty = true;
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

/// Run the real conversation UI over channels owned by an agent orchestrator.
pub async fn run_agent_tui(
    events: mpsc::UnboundedReceiver<AgentEvent>,
    prompts: PromptSender,
) -> Result<()> {
    let _restore = enter_terminal()?;
    let stdout: Stdout = io::stdout();
    let mut terminal =
        Terminal::new(CrosstermBackend::new(stdout)).context("creating Crossterm terminal")?;
    let (input_thread, input_events) = InputThread::spawn()?;
    let result = run_loop(&mut terminal, events, prompts, input_events).await;
    drop(input_thread);
    result
}

/// Run the P0 mock stream through the production state/event/render pipeline.
pub async fn run_stream_demo() -> Result<()> {
    let (agent_sender, agent_events) = mpsc::unbounded_channel();
    let (prompt_sender, prompt_receiver) = mpsc::unbounded_channel();
    let demo = tokio::spawn(run_demo_driver(agent_sender, prompt_receiver));
    let result = run_agent_tui(agent_events, prompt_sender).await;
    let _ = demo.await;
    result
}

async fn run_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    mut agent_events: mpsc::UnboundedReceiver<AgentEvent>,
    prompts: PromptSender,
    mut input_events: mpsc::UnboundedReceiver<InputMessage>,
) -> Result<()> {
    let mut state = AppState::default();
    let mut agent_channel_open = true;

    loop {
        draw_if_dirty(terminal, &mut state)?;

        tokio::select! {
            input = input_events.recv() => {
                match input.context("terminal input thread stopped")? {
                    InputMessage::Key(key) => {
                        if dispatch_action(state.reduce_key(key), &prompts)? {
                            return Ok(());
                        }
                    }
                    InputMessage::Resize => state.note_resize(),
                    InputMessage::Error(error) => anyhow::bail!("reading terminal input: {error}"),
                }
            }
            event = agent_events.recv(), if agent_channel_open => {
                match event {
                    Some(event) => state.reduce_agent_event(event),
                    None => {
                        agent_channel_open = false;
                        state.note_runtime_closed();
                    }
                }
            }
        }
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

fn dispatch_action(action: UiAction, prompts: &PromptSender) -> Result<bool> {
    match action {
        UiAction::None => Ok(false),
        UiAction::Quit => Ok(true),
        UiAction::Submit(prompt) => {
            prompts
                .send(prompt)
                .map_err(|_| anyhow::anyhow!("agent prompt receiver closed"))?;
            Ok(false)
        }
    }
}

fn render(frame: &mut Frame<'_>, state: &mut AppState) {
    let chunks = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(frame.area());

    let conversation_block = Block::default().borders(Borders::ALL).title(" Grey ");
    let conversation_inner = conversation_block.inner(chunks[0]);
    state.update_viewport(conversation_inner.width, conversation_inner.height);
    let conversation = Paragraph::new(state.output.as_str())
        .block(conversation_block)
        .wrap(Wrap { trim: false })
        .scroll((state.scroll, 0));
    frame.render_widget(conversation, chunks[0]);

    let input_block = Block::default().borders(Borders::ALL).title(" input ");
    let input_inner = input_block.inner(chunks[1]);
    let prompt = Span::styled(
        "> ",
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
    );
    let input = Paragraph::new(Line::from(vec![
        prompt,
        Span::raw(state.input.text.as_str()),
    ]))
    .block(input_block)
    .style(Style::default().fg(Color::White));
    frame.render_widget(input, chunks[1]);
    if input_inner.width > 0 && input_inner.height > 0 {
        let prompt_width = UnicodeWidthStr::width("> ");
        let cursor_offset = prompt_width.saturating_add(state.input_cursor_column());
        let cursor_x = input_inner
            .x
            .saturating_add(cursor_offset.min(usize::from(u16::MAX)) as u16)
            .min(input_inner.right().saturating_sub(1));
        frame.set_cursor_position((cursor_x, input_inner.y));
    }

    let stats = state.frame_stats();
    let status = Line::from(vec![
        Span::styled(
            " GREY ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            " {:5.1}ms {:4.0}fps frames {} scroll {}/{} ",
            stats.ema_frame_ms(),
            stats.fps(),
            stats.frames(),
            state.scroll,
            state.max_scroll,
        )),
        Span::styled(
            format!(" {} ", state.status),
            Style::default().fg(Color::Yellow),
        ),
        Span::styled(
            " Esc/Ctrl-C 退出 · 空输入 q 退出 · Enter 发送 · PgUp/PgDn 滚动 ",
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(Paragraph::new(status), chunks[2]);
}

async fn run_demo_driver(
    events: mpsc::UnboundedSender<AgentEvent>,
    mut prompts: mpsc::UnboundedReceiver<String>,
) {
    if !send_demo_stream(&events, DEMO_REPLY).await {
        return;
    }
    if !finish_demo_turn(&events) {
        return;
    }
    while let Some(prompt) = prompts.recv().await {
        let reply = format!("（模拟回复）{prompt}\n");
        if !send_demo_stream(&events, &reply).await {
            return;
        }
        if !finish_demo_turn(&events) {
            return;
        }
    }
}

fn finish_demo_turn(events: &mpsc::UnboundedSender<AgentEvent>) -> bool {
    events
        .send(AgentEvent::Completed {
            usage: grey_core::Usage::default(),
            steps: 1,
        })
        .is_ok()
}

async fn send_demo_stream(events: &mpsc::UnboundedSender<AgentEvent>, text: &str) -> bool {
    let characters: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < characters.len() {
        let chunk_len = (index % 5) + 2;
        let end = (index + chunk_len).min(characters.len());
        let chunk: String = characters[index..end].iter().collect();
        if events.send(AgentEvent::Delta(chunk)).is_err() {
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
    fn spawn() -> Result<(Self, mpsc::UnboundedReceiver<InputMessage>)> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (sender, receiver) = mpsc::unbounded_channel();
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
}

impl Drop for InputThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn read_input(stop: Arc<AtomicBool>, sender: mpsc::UnboundedSender<InputMessage>) {
    while !stop.load(Ordering::Acquire) {
        match event::poll(INPUT_POLL_INTERVAL) {
            Ok(false) => {}
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) => {
                    if sender.send(InputMessage::Key(key)).is_err() {
                        return;
                    }
                }
                Ok(Event::Resize(_, _)) => {
                    if sender.send(InputMessage::Resize).is_err() {
                        return;
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = sender.send(InputMessage::Error(error.to_string()));
                    return;
                }
            },
            Err(error) => {
                let _ = sender.send(InputMessage::Error(error.to_string()));
                return;
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

    #[test]
    fn input_reducer_edits_submits_and_handles_every_exit_key() {
        let mut state = AppState::default();
        assert_eq!(state.reduce_key(key(KeyCode::Char('h'))), UiAction::None);
        assert_eq!(state.reduce_key(key(KeyCode::Char('i'))), UiAction::None);
        assert_eq!(state.input(), "hi");
        assert_eq!(
            state.reduce_key(key(KeyCode::Enter)),
            UiAction::Submit("hi".into())
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
            tool_outputs_truncated: 0,
        }));
        state.reduce_agent_event(AgentEvent::Completed {
            usage: grey_core::Usage {
                input_tokens: 7,
                output_tokens: 11,
            },
            steps: 2,
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

    #[tokio::test]
    async fn submit_action_is_forwarded_through_the_prompt_sender() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        assert!(!dispatch_action(UiAction::Submit("inspect".into()), &sender).unwrap());
        assert_eq!(receiver.recv().await.as_deref(), Some("inspect"));
        assert!(dispatch_action(UiAction::Quit, &sender).unwrap());
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
}
