mod theme;
mod highlight;

use theme::THEME;
use highlight::{highlight_code_block, highlight_tool_code, highlight_bash_output, highlight_read_output, try_highlight_grep_line, is_read_tool_output};

use synaps_cli::{Runtime, StreamEvent, Result, CancellationToken, Session, list_sessions, latest_session, find_session};
use clap::Parser;
use crossterm::{
    event::{Event, KeyCode, KeyModifiers, MouseEventKind, EnableMouseCapture, DisableMouseCapture, EnableBracketedPaste, DisableBracketedPaste, EventStream},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use unicode_width::UnicodeWidthStr;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Alignment},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, BorderType, Clear, Paragraph, Padding},
    Terminal,
};
use serde_json::{json, Value};
use std::io;
use std::time::Instant;
use chrono::Local;
use tachyonfx::{fx, Effect, Interpolation, Shader};


// -- Theme -------------------------------------------------------------------

// Syntect loaded once, reused forever


// -- Data --------------------------------------------------------------------

#[derive(Clone)]
enum ChatMessage {
    User(String),
    Thinking(String),
    Text(String),
    ToolUseStart(String, String),  // (tool_name, partial_input)
    ToolUse { tool_name: String, input: String },
    ToolResult { content: String, elapsed_ms: Option<u64> },
    Error(String),
    System(String),
}

struct TimestampedMsg {
    msg: ChatMessage,
    time: String,
}

struct App {
    messages: Vec<TimestampedMsg>,
    input: String,
    /// Cursor position as a **char index** (not byte index).
    /// Use `cursor_byte_pos()` to convert to byte offset for String operations.
    cursor_pos: usize,
    scroll_back: u16,
    /// When true, viewport stays pinned to the bottom (auto-scroll).
    /// Set to false when user scrolls up, true when they scroll back to bottom.
    scroll_pinned: bool,
    api_messages: Vec<Value>,
    streaming: bool,
    input_history: Vec<String>,
    history_index: Option<usize>,
    input_stash: String,
    input_tokens: u64,
    output_tokens: u64,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cache_read_tokens: u64,
    total_cache_creation_tokens: u64,
    session_cost: f64,
    session: Session,
    line_cache: Vec<Line<'static>>,
    cache_width: usize,
    dirty: bool,
    show_full_output: bool,
    logo_dismiss_t: Option<f64>,
    logo_build_t: Option<f64>,
    /// Previous rendered line count — used to stabilize scroll when not pinned
    last_line_count: usize,
    /// Active subagent status for the live panel
    subagents: Vec<SubagentState>,
    /// Counter for unique subagent IDs within a session
    next_subagent_id: u32,
    /// Tracks when the current tool started executing (for elapsed time display)
    tool_start_time: Option<std::time::Instant>,
    /// Saved context from an aborted response — injected into the next user message
    abort_context: Option<String>,
    /// Message queued while streaming — auto-sent when current response finishes
    queued_message: Option<String>,
    /// Tracks paste state: snapshot of input before first paste, and total pasted char count
    input_before_paste: Option<String>,
    pasted_char_count: usize,
    /// Spinner frame counter (incremented on tick)
    spinner_frame: usize,
    /// Transient status text shown in the header bar (auto-cleared when streaming starts)
    status_text: Option<String>,
    /// GamblersDen child process — spawned by /gamba, killed when streaming finishes
    gamba_child: Option<std::process::Child>,
}

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Generate a bash execution trace animation string and its pulsing color.
/// Returns (trace_string, Color) for use in Span styling.
fn bash_trace(spinner_frame: usize) -> (String, Color) {
    const CHARS: [char; 8] = [' ', '░', '▒', '▓', '█', '▓', '▒', '░'];
    const WIDTH: usize = 14;
    let offset = (spinner_frame / 2) % (WIDTH + CHARS.len());
    let trace: String = (0..WIDTH).map(|i| {
        let dist = if offset >= i { offset - i } else { WIDTH + CHARS.len() };
        if dist < CHARS.len() { CHARS[dist] } else { ' ' }
    }).collect();
    let pulse = ((spinner_frame as f64 / 15.0).sin() * 0.3 + 0.7) as f64;
    let color = Color::Rgb(
        (50.0 * pulse) as u8,
        (180.0 * pulse) as u8,
        (220.0 * pulse) as u8,
    );
    (trace, color)
}

/// Format a tool name for display. Returns (icon, display_name, optional_server_tag).
/// MCP tools like "mcp__byteray__read_pseudocode" become ("⚡", "read_pseudocode", Some("byteray"))
fn format_tool_name(tool_name: &str) -> (&'static str, String, Option<String>) {
    if tool_name.starts_with("mcp__") {
        let parts: Vec<&str> = tool_name.splitn(3, "__").collect();
        let server = parts.get(1).unwrap_or(&"mcp").to_string();
        let tool = parts.get(2).unwrap_or(&tool_name).to_string();
        ("\u{00bb}", tool, Some(server)) // »
    } else {
        let icon = match tool_name {
            "bash"     => "$",
            "read"     => ">",
            "write"    => "<",
            "edit"     => "~",
            "grep"     => "/",
            "find"     => "?",
            "ls"       => "=",
            "subagent" => "*",
            _          => "-",
        };
        (icon, tool_name.to_string(), None)
    }
}

#[derive(Clone)]
#[allow(dead_code)]
struct SubagentState {
    id: u32,
    name: String,
    status: String,
    start_time: std::time::Instant,
    done: bool,
    duration_secs: Option<f64>,
}

impl App {
    fn new(session: Session) -> Self {
        Self {
            messages: Vec::new(),
            input: String::new(),
            cursor_pos: 0,
            scroll_back: 0,
            scroll_pinned: true,
            api_messages: Vec::new(),
            streaming: false,
            input_history: Vec::new(),
            history_index: None,
            input_stash: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            session_cost: 0.0,
            session,
            line_cache: Vec::new(),
            cache_width: 0,
            dirty: true,
            show_full_output: false,
            logo_dismiss_t: None,
            logo_build_t: Some(0.0),
            last_line_count: 0,
            subagents: Vec::new(),
            next_subagent_id: 0,
            tool_start_time: None,
            abort_context: None,
            queued_message: None,
            input_before_paste: None,
            pasted_char_count: 0,
            spinner_frame: 0,
            status_text: None,
            gamba_child: None,
        }
    }

    /// Restore SynapsCLI's TUI after casino (or failed spawn).
    fn restore_terminal(&self) {
        crossterm::terminal::enable_raw_mode().ok();
        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::EnterAlternateScreen
        ).ok();
    }

    /// Yield terminal to casino — tears down TUI, spawns GamblersDen.
    /// Returns Ok(()) if launched, Err(msg) if failed.
    fn launch_gamba(&mut self) -> std::result::Result<(), String> {
        if self.gamba_child.is_some() {
            return Err("🎰 Casino already running!".to_string());
        }
        let bin = match std::env::var("HOME").ok()
            .map(|h| std::path::PathBuf::from(h).join("Projects/GamblersDen/target/release/gamblers-den"))
            .filter(|p| p.exists())
        {
            Some(b) => b,
            None => return Err("GamblersDen binary not found. Build it: cd ~/Projects/GamblersDen && cargo build --release".to_string()),
        };

        // Tear down our TUI
        crossterm::terminal::disable_raw_mode().ok();
        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen
        ).ok();
        // Spawn the casino (non-blocking)
        match std::process::Command::new(&bin)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .spawn()
        {
            Ok(child) => {
                self.gamba_child = Some(child);
                Ok(())
            }
            Err(e) => {
                self.restore_terminal();
                Err(format!("Failed to launch casino: {}", e))
            }
        }
    }

    /// Kill the GamblersDen child process and reclaim the terminal.
    /// Returns a message to display, or None if no casino was running.
    fn reclaim_gamba(&mut self) -> Option<String> {
        if let Some(mut child) = self.gamba_child.take() {
            child.kill().ok();
            child.wait().ok();
            self.restore_terminal();
            Some("🎰 Back from the casino. Response ready.".to_string())
        } else {
            None
        }
    }

    /// Check if the GamblersDen child exited on its own (user quit the casino).
    /// Returns a message if it did.
    fn check_gamba_exited(&mut self) -> Option<String> {
        if let Some(ref mut child) = self.gamba_child {
            match child.try_wait() {
                Ok(Some(_status)) => {
                    self.gamba_child = None;
                    self.restore_terminal();
                    Some("🎰 Back from the casino. How'd you do, degen?".to_string())
                }
                _ => None,
            }
        } else {
            None
        }
    }

    /// Convert char-based cursor_pos to byte offset in self.input.
    fn cursor_byte_pos(&self) -> usize {
        self.input.char_indices()
            .nth(self.cursor_pos)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }

    /// Number of chars in self.input (for bounds checking cursor_pos).
    fn input_char_count(&self) -> usize {
        self.input.chars().count()
    }

    /// Calculate the number of visual lines the input needs, given an inner width.
    /// Returns (total_lines, cursor_row, cursor_col) for layout and cursor placement.
    fn input_wrap_info(&self, inner_width: u16) -> (u16, u16, u16) {
        use unicode_width::UnicodeWidthChar;
        let w = inner_width.max(1) as usize;
        // prefix "❯ " is 2 display columns (only on first line)
        let prefix_width: usize = 2;

        let mut row: u16 = 0;
        let mut col: usize = prefix_width;
        let mut cursor_row: u16 = 0;
        let mut cursor_col: u16 = prefix_width as u16;

        for (i, ch) in self.input.chars().enumerate() {
            if i == self.cursor_pos {
                cursor_row = row;
                cursor_col = col as u16;
            }
            if ch == '\n' {
                row += 1;
                col = prefix_width; // continuation lines also have 2-char indent
                continue;
            }
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if col + cw > w {
                row += 1;
                col = 0;
            }
            col += cw;
        }
        // If cursor is at the end
        if self.cursor_pos == self.input_char_count() {
            cursor_row = row;
            cursor_col = col as u16;
            // If cursor is exactly at the wrap boundary
            if col >= w {
                cursor_row += 1;
                cursor_col = 0;
            }
        }

        let total_lines = row + 1;
        (total_lines, cursor_row, cursor_col)
    }

    fn save_session(&mut self) {
        if self.api_messages.is_empty() {
            return;
        }
        self.session.api_messages = self.api_messages.clone();
        self.session.total_input_tokens = self.total_input_tokens;
        self.session.total_output_tokens = self.total_output_tokens;
        self.session.session_cost = self.session_cost;
        self.session.updated_at = chrono::Utc::now();
        self.session.auto_title();
        if let Err(e) = self.session.save() {
            eprintln!("\x1b[31m[ERROR] Failed to save session: {}\x1b[0m", e);
        }
    }

    fn add_usage(
        &mut self,
        input_tokens: u64,
        output_tokens: u64,
        cache_read: u64,
        cache_creation: u64,
        model: &str,
    ) {
        self.input_tokens = input_tokens;
        self.output_tokens = output_tokens;
        self.total_input_tokens += input_tokens;
        self.total_output_tokens += output_tokens;
        self.total_cache_read_tokens += cache_read;
        self.total_cache_creation_tokens += cache_creation;
        // Pricing per million tokens (as of 2025)
        let (input_price, output_price) = match model {
            m if m.contains("opus") => (15.0, 75.0),
            m if m.contains("sonnet") => (3.0, 15.0),
            m if m.contains("haiku") => (0.80, 4.0),
            _ => (3.0, 15.0), // default to sonnet pricing
        };
        // Cache reads bill at 0.1x input price; cache writes at 1.25x
        let cost = (input_tokens as f64 / 1_000_000.0) * input_price
                 + (cache_read as f64 / 1_000_000.0) * input_price * 0.1
                 + (cache_creation as f64 / 1_000_000.0) * input_price * 1.25
                 + (output_tokens as f64 / 1_000_000.0) * output_price;
        self.session_cost += cost;
    }

    fn push_msg(&mut self, msg: ChatMessage) {
        self.messages.push(TimestampedMsg {
            msg,
            time: Local::now().format("%H:%M").to_string(),
        });
        // Auto-scroll only when pinned to bottom
        if self.scroll_pinned {
            self.scroll_back = 0;
        }
        self.dirty = true;
    }

    /// Find the file extension from the ToolUse message preceding a ToolResult at index `idx`.
    fn find_preceding_read_extension(&self, idx: usize) -> String {
        // Walk backwards from idx to find the preceding ToolUse
        if idx == 0 { return String::new(); }
        for i in (0..idx).rev() {
            if let ChatMessage::ToolUse { ref tool_name, ref input } = self.messages[i].msg {
                if tool_name == "read" {
                    // Extract path from the JSON input
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(input) {
                        if let Some(path) = parsed["path"].as_str() {
                            // Get the extension
                            if let Some(ext) = std::path::Path::new(path).extension() {
                                return ext.to_string_lossy().to_string();
                            }
                        }
                    }
                }
                break; // Stop at first ToolUse regardless
            }
        }
        String::new()
    }

    /// Find the tool name from the ToolUse message preceding a ToolResult at index `idx`.
    fn find_preceding_tool_name(&self, idx: usize) -> Option<String> {
        if idx == 0 { return None; }
        for i in (0..idx).rev() {
            if let ChatMessage::ToolUse { ref tool_name, .. } = self.messages[i].msg {
                return Some(tool_name.clone());
            }
        }
        None
    }

    /// Capture all assistant output since the last user message as abort context.
    /// This gets injected into the next user message so the model knows what it
    /// was doing before the abort.
    fn capture_abort_context(&mut self) {
        let mut parts: Vec<String> = Vec::new();

        // Walk backwards from the end to find content since last user message
        for tmsg in self.messages.iter().rev() {
            match &tmsg.msg {
                ChatMessage::User(_) => break, // stop at the last user message
                ChatMessage::Thinking(t) => {
                    if !t.is_empty() {
                        // Truncate thinking to avoid bloating context
                        let preview: String = t.chars().take(500).collect();
                        parts.push(format!("[thinking]: {}", preview));
                    }
                }
                ChatMessage::Text(t) => {
                    if !t.is_empty() {
                        parts.push(format!("[response]: {}", t));
                    }
                }
                ChatMessage::ToolUse { tool_name, input } => {
                    // Truncate input to keep context lean
                    let input_preview: String = input.chars().take(200).collect();
                    parts.push(format!("[tool_use]: {} — {}", tool_name, input_preview));
                }
                ChatMessage::ToolResult { content, .. } => {
                    if !content.is_empty() {
                        let preview: String = content.chars().take(300).collect();
                        parts.push(format!("[tool_result]: {}", preview));
                    }
                }
                _ => {}
            }
        }

        if parts.is_empty() {
            self.abort_context = None;
            return;
        }

        parts.reverse(); // chronological order
        self.abort_context = Some(format!(
            "[ABORT CONTEXT — your previous response was interrupted. Here's what you completed before the abort:]\n\n{}\n\n[END ABORT CONTEXT — continue from where you left off or adjust based on the user's new message]",
            parts.join("\n")
        ));
    }

    fn history_up(&mut self) {
        if self.input_history.is_empty() { return; }
        match self.history_index {
            None => {
                self.input_stash = self.input.clone();
                self.history_index = Some(self.input_history.len() - 1);
            }
            Some(i) if i > 0 => {
                self.history_index = Some(i - 1);
            }
            _ => return,
        }
        self.input = self.input_history[self.history_index.unwrap()].clone();
        self.cursor_pos = self.input.chars().count();
    }

    fn history_down(&mut self) {
        match self.history_index {
            Some(i) => {
                if i + 1 < self.input_history.len() {
                    self.history_index = Some(i + 1);
                    self.input = self.input_history[i + 1].clone();
                } else {
                    self.history_index = None;
                    self.input = self.input_stash.clone();
                    self.input_stash.clear();
                }
                self.cursor_pos = self.input.chars().count();
            }
            None => {}
        }
    }

    fn append_or_update_text(&mut self, text: &str) {
        if let Some(TimestampedMsg { msg: ChatMessage::Text(ref mut existing), .. }) = self.messages.last_mut() {
            existing.push_str(text);
        } else {
            self.push_msg(ChatMessage::Text(text.to_string()));
        }
        self.dirty = true;
    }

    fn append_or_update_thinking(&mut self, text: &str) {
        if let Some(TimestampedMsg { msg: ChatMessage::Thinking(ref mut existing), .. }) = self.messages.last_mut() {
            existing.push_str(text);
        } else {
            self.push_msg(ChatMessage::Thinking(text.to_string()));
        }
        self.dirty = true;
    }

    fn render_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines: Vec<Line> = Vec::new();
        let m = "   "; // margin

        for (i, tmsg) in self.messages.iter().enumerate() {
            let ts = &tmsg.time;
            match &tmsg.msg {
                ChatMessage::User(text) => {
                    let bg = Style::default().bg(THEME.user_bg);
                    // Top margin
                    lines.push(Line::from(""));
                    // Top padding
                    lines.push(Line::from(Span::styled(format!("{:<width$}", "", width = width), bg)));
                    // Header: chevron + name + timestamp right-aligned
                    let label = format!("{}\u{276f} you", m);
                    let ts_str = format!("{} ", ts);
                    let gap = width.saturating_sub(label.chars().count() + ts_str.chars().count());
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("{}{}", label, " ".repeat(gap)),
                            Style::default().fg(THEME.user_color).bg(THEME.user_bg).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(ts_str, Style::default().fg(THEME.muted).bg(THEME.user_bg)),
                    ]));
                    // Content — just render the text (pasted messages already contain "[Pasted N lines]")
                    let style = Style::default().fg(THEME.user_color).bg(THEME.user_bg);
                    for line in text.lines() {
                        for wline in wrap_text(&format!("{}  {}", m, line), width) {
                            lines.push(Line::from(Span::styled(
                                format!("{:<width$}", wline, width = width), style,
                            )));
                        }
                    }
                    // Bottom padding
                    lines.push(Line::from(Span::styled(format!("{:<width$}", "", width = width), bg)));
                    // Bottom margin
                    lines.push(Line::from(""));
                }

                ChatMessage::Thinking(text) => {
                    let dim = Style::default().fg(THEME.thinking_color);
                    let dim_italic = dim.add_modifier(Modifier::ITALIC);
                    // Header
                    let thinking_label = if text == "…" {
                        let braille = ['\u{28fe}','\u{28f7}','\u{28ef}','\u{28df}','\u{287f}','\u{28bf}','\u{28fb}','\u{28fd}'];
                        let idx = (self.spinner_frame / 4) % braille.len();
                        let wave: String = (0..3).map(|i| braille[(idx + i) % braille.len()]).collect();
                        format!("{} thinking", wave)
                    } else {
                        "thinking".to_string()
                    };
                    lines.push(Line::from(vec![
                        Span::styled(format!("{}╭─ ", m), dim),
                        Span::styled(thinking_label, dim.add_modifier(Modifier::DIM)),
                    ]));
                    // Body — structured with visual hierarchy
                    let tlines: Vec<&str> = text.lines().collect();
                    let non_empty: Vec<&&str> = tlines.iter().filter(|l| !l.trim().is_empty()).collect();
                    let show = non_empty.len().min(8);
                    // Calculate usable width for thinking content
                    let prefix_len = m.len() + 4; // margin + "│ · " or "│ "
                    let content_width = width.saturating_sub(prefix_len);

                    for (i, line) in non_empty[..show].iter().enumerate() {
                        let trimmed = line.trim();
                        let is_last = i == show - 1 && non_empty.len() <= 8;
                        let connector = if is_last { "╰" } else { "│" };
                        let continuation = "│";

                        // Detect structure in thinking
                        let (prefix_char, line_style) = if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
                            ("· ", dim_italic)
                        } else if trimmed.ends_with(':') || trimmed.starts_with('#') {
                            ("", dim.add_modifier(Modifier::BOLD))
                        } else if trimmed.starts_with("```") {
                            ("", dim.add_modifier(Modifier::DIM))
                        } else {
                            ("", dim_italic)
                        };

                        // Wrap manually to preserve connector on each line
                        let first_prefix = format!("{}{} {}", m, connector, prefix_char);
                        let cont_prefix = format!("{}{} {}", m, continuation, " ".repeat(prefix_char.len()));

                        if content_width > 10 {
                            let chars: Vec<char> = trimmed.chars().collect();
                            let mut pos = 0;
                            let mut is_first = true;
                            while pos < chars.len() {
                                let chunk_len = content_width.min(chars.len() - pos);
                                let chunk: String = chars[pos..pos + chunk_len].iter().collect();
                                let prefix = if is_first { &first_prefix } else { &cont_prefix };
                                lines.push(Line::from(Span::styled(
                                    format!("{}{}", prefix, chunk),
                                    line_style,
                                )));
                                pos += chunk_len;
                                is_first = false;
                            }
                        } else {
                            lines.push(Line::from(Span::styled(
                                format!("{}{}", first_prefix, trimmed),
                                line_style,
                            )));
                        }
                    }
                    if non_empty.len() > 8 {
                        lines.push(Line::from(Span::styled(
                            format!("{}╰ +{} lines", m, non_empty.len() - 8), dim,
                        )));
                    }
                }

                ChatMessage::Text(text) => {
                    // Separator between user block and agent response
                    if i > 0 {
                        let sep_half = width.min(40) / 2;
                        let sep_left: String = "\u{2500}".repeat(sep_half.saturating_sub(2));
                        let sep_right: String = "\u{2500}".repeat(sep_half.saturating_sub(2));
                        lines.push(Line::from(vec![
                            Span::styled(format!("{}{}", m, sep_left), Style::default().fg(THEME.separator)),
                            Span::styled(" \u{00b7} ", Style::default().fg(Color::Rgb(35, 55, 75))),
                            Span::styled(sep_right, Style::default().fg(THEME.separator)),
                        ]));
                    }
                    // Header
                    let label = format!("{}\u{25c8} agent", m);
                    let ts_str = format!("{} ", ts);
                    let gap = width.saturating_sub(label.chars().count() + ts_str.chars().count());
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("{}{}", label, " ".repeat(gap)),
                            Style::default().fg(THEME.claude_label).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(ts_str, Style::default().fg(THEME.muted)),
                    ]));
                    // Body
                    if text.is_empty() {
                        lines.push(Line::from(Span::styled(
                            format!("{}   \u{2026}", m), Style::default().fg(THEME.muted),
                        )));
                    } else {
                        lines.extend(render_markdown(text, m, width));
                    }
                }

                ChatMessage::ToolUseStart(tool_name, partial_input) => {
                    let (icon, display_name, server_tag) = format_tool_name(tool_name);
                    let mut header = vec![
                        Span::styled(format!("{}   {} ", m, icon), Style::default().fg(THEME.tool_label)),
                        Span::styled(display_name, Style::default().fg(THEME.tool_label).add_modifier(Modifier::BOLD)),
                    ];
                    if let Some(tag) = server_tag {
                        header.push(Span::styled(format!(" [{}]", tag), Style::default().fg(THEME.muted)));
                    }
                    // Show elapsed time while tool is running
                    let elapsed_str = if let Some(start) = self.tool_start_time {
                        let secs = start.elapsed().as_secs_f64();
                        if secs >= 1.0 {
                            format!(" {:.1}s", secs)
                        } else {
                            format!(" {}ms", (secs * 1000.0) as u64)
                        }
                    } else {
                        String::new()
                    };
                    let spinner_idx = (self.spinner_frame / 3) % SPINNER_FRAMES.len();
                    // Bash gets a special animated execution trace
                    if tool_name == "bash" {
                        let (trace, color) = bash_trace(self.spinner_frame);
                        header.push(Span::styled(
                            format!(" {}{}", trace, elapsed_str),
                            Style::default().fg(color),
                        ));
                    } else {
                        header.push(Span::styled(
                            format!(" {} running{}", SPINNER_FRAMES[spinner_idx], elapsed_str),
                            Style::default().fg(THEME.status_streaming).add_modifier(Modifier::DIM),
                        ));
                    }
                    lines.push(Line::from(header));
                    // Show accumulated partial input with newlines rendered
                    if !partial_input.is_empty() {
                        let param_style = Style::default().fg(THEME.tool_param);
                        // Unescape \n in JSON string to real newlines for display
                        let unescaped = partial_input.replace("\\n", "\n").replace("\\t", "  ");

                        // Try to extract just the content value if this is a write tool
                        let display = if let Some(idx) = unescaped.find("\"content\": \"") {
                            let content_start = idx + "\"content\": \"".len();
                            &unescaped[content_start..]
                        } else if let Some(idx) = unescaped.find("\"content\":\"") {
                            let content_start = idx + "\"content\":\"".len();
                            &unescaped[content_start..]
                        } else {
                            &unescaped
                        };

                        let content_lines: Vec<&str> = display.lines().collect();
                        let total = content_lines.len();
                        let max_show = 12;
                        // Show last N lines (tail) so you see what's being written now
                        let skip = total.saturating_sub(max_show);
                        if skip > 0 {
                            let omit = format!("{}     … {} lines above", m, skip);
                            lines.push(Line::from(Span::styled(omit, Style::default().fg(THEME.muted))));
                        }
                        for cline in content_lines.iter().skip(skip) {
                            let line_str = format!("{}       {}", m, cline);
                            for wline in wrap_text(&line_str, width) {
                                lines.push(Line::from(Span::styled(wline, param_style)));
                            }
                        }
                    }
                }

                ChatMessage::ToolUse { tool_name, input } => {
                    // Compact tool header
                    let (icon, display_name, server_tag) = format_tool_name(tool_name);
                    let mut header = vec![
                        Span::styled(format!("{}   {} ", m, icon), Style::default().fg(THEME.tool_label)),
                        Span::styled(display_name, Style::default().fg(THEME.tool_label).add_modifier(Modifier::BOLD)),
                    ];
                    if let Some(tag) = server_tag {
                        header.push(Span::styled(format!(" [{}]", tag), Style::default().fg(THEME.muted)));
                    }
                    // If this is the last message and a tool is executing, show animation
                    let is_last = i == self.messages.len() - 1;
                    if is_last && self.tool_start_time.is_some() {
                        let elapsed_str = if let Some(start) = self.tool_start_time {
                            let secs = start.elapsed().as_secs_f64();
                            if secs >= 1.0 { format!(" {:.1}s", secs) }
                            else { format!(" {}ms", (secs * 1000.0) as u64) }
                        } else { String::new() };

                        if tool_name == "bash" {
                            let (trace, color) = bash_trace(self.spinner_frame);
                            header.push(Span::styled(
                                format!(" {}{}", trace, elapsed_str),
                                Style::default().fg(color),
                            ));
                        } else {
                            let spinner_idx = (self.spinner_frame / 3) % SPINNER_FRAMES.len();
                            header.push(Span::styled(
                                format!(" {} running{}", SPINNER_FRAMES[spinner_idx], elapsed_str),
                                Style::default().fg(THEME.status_streaming).add_modifier(Modifier::DIM),
                            ));
                        }
                    }
                    lines.push(Line::from(header));
                    // Params — key:value on one line each, dimmed
                    let param_style = Style::default().fg(THEME.tool_param);
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(input) {
                        if let Some(obj) = parsed.as_object() {
                            // Extract file extension from "path" param if present (for syntax highlighting)
                            let file_ext = obj.get("path")
                                .and_then(|v| v.as_str())
                                .and_then(|p| std::path::Path::new(p).extension())
                                .map(|e| e.to_string_lossy().to_string())
                                .unwrap_or_default();

                            for (k, v) in obj {
                                if let Some(s) = v.as_str() {
                                    if s.contains('\n') {
                                        // Multi-line content: syntax highlight if we know the language
                                        let content_lines: Vec<&str> = s.lines().collect();
                                        let total = content_lines.len();
                                        let max_preview = 12;
                                        let show = total.min(max_preview);

                                        // Diff-style markers for edit tool
                                        let (marker, marker_color) = match k.as_str() {
                                            "old_string" => ("−", Color::Rgb(200, 60, 60)),
                                            "new_string" => ("+", Color::Rgb(60, 200, 80)),
                                            _ => ("│", THEME.muted),
                                        };

                                        let label = match k.as_str() {
                                            "old_string" => "old",
                                            "new_string" => "new",
                                            _ => k.as_str(),
                                        };
                                        let header = format!("{}     {}: ({} lines)", m, label, total);
                                        lines.push(Line::from(Span::styled(header, param_style)));

                                        // Syntax highlight the code
                                        let is_code_param = k == "content" || k == "old_string" || k == "new_string";
                                        if is_code_param && !file_ext.is_empty() {
                                            let hl_lines = highlight_tool_code(&content_lines[..show], &file_ext, &m, marker, marker_color);
                                            lines.extend(hl_lines);
                                        } else {
                                            for (ci, cline) in content_lines.iter().take(show).enumerate() {
                                                lines.push(Line::from(vec![
                                                    Span::styled(format!("{}    {:>3} {} ", m, ci + 1, marker), Style::default().fg(marker_color)),
                                                    Span::styled(cline.to_string(), param_style),
                                                ]));
                                            }
                                        }
                                        if total > max_preview {
                                            let omit = format!("{}       … +{} more lines", m, total - max_preview);
                                            lines.push(Line::from(Span::styled(omit, Style::default().fg(THEME.muted))));
                                        }
                                    } else {
                                        let val = if s.len() > 120 {
                                            let p: String = s.chars().take(120).collect();
                                            format!("{}\u{2026}", p)
                                        } else {
                                            s.to_string()
                                        };
                                        let line_str = format!("{}     {}: {}", m, k, val);
                                        for wline in wrap_text(&line_str, width) {
                                            lines.push(Line::from(Span::styled(wline, param_style)));
                                        }
                                    }
                                } else {
                                    let val = v.to_string();
                                    let line_str = format!("{}     {}: {}", m, k, val);
                                    for wline in wrap_text(&line_str, width) {
                                        lines.push(Line::from(Span::styled(wline, param_style)));
                                    }
                                }
                            }
                        }
                    }
                }

                ChatMessage::ToolResult { ref content, elapsed_ms } => {
                    let result = content;
                    let is_error = result.starts_with("Tool execution failed")
                        || result.starts_with("Unknown tool");
                    let style = if is_error {
                        Style::default().fg(THEME.error_color)
                    } else {
                        Style::default().fg(THEME.tool_result_color)
                    };

                    let result_lines: Vec<&str> = result.lines().collect();
                    let show = if self.show_full_output {
                        result_lines.len()
                    } else {
                        let max_show = if result_lines.len() > 30 { 15 } else { 12 };
                        result_lines.len().min(max_show)
                    };

                    // Success/fail indicator with elapsed time
                    if !is_error && show > 0 {
                        if elapsed_ms.is_none() && self.tool_start_time.is_some() {
                            // Tool still executing — show animation
                            let preceding_tool_name = self.find_preceding_tool_name(i);
                            let elapsed_str = if let Some(start) = self.tool_start_time {
                                let secs = start.elapsed().as_secs_f64();
                                if secs >= 1.0 { format!(" {:.1}s", secs) }
                                else { format!(" {}ms", (secs * 1000.0) as u64) }
                            } else { String::new() };

                            if preceding_tool_name.as_deref() == Some("bash") {
                                let (trace, color) = bash_trace(self.spinner_frame);
                                lines.push(Line::from(vec![
                                    Span::styled(format!("{}     ", m), Style::default()),
                                    Span::styled(format!("{}{}", trace, elapsed_str), Style::default().fg(color)),
                                ]));
                            } else {
                                let spinner_idx = (self.spinner_frame / 3) % SPINNER_FRAMES.len();
                                lines.push(Line::from(Span::styled(
                                    format!("{}     {} running{}", m, SPINNER_FRAMES[spinner_idx], elapsed_str),
                                    Style::default().fg(THEME.status_streaming).add_modifier(Modifier::DIM),
                                )));
                            }
                        } else {
                            let elapsed_str = match elapsed_ms {
                                Some(ms) if *ms >= 1000 => format!(" {:.1}s", *ms as f64 / 1000.0),
                                Some(ms) => format!(" {}ms", ms),
                                None => String::new(),
                            };
                            lines.push(Line::from(vec![
                                Span::styled(
                                    format!("{}     \u{2514}\u{2500} ok ({} lines)", m, result_lines.len()),
                                    Style::default().fg(THEME.tool_result_ok),
                                ),
                                Span::styled(
                                    elapsed_str,
                                    Style::default().fg(THEME.subagent_time),
                                ),
                            ]));
                        }
                    }

                    // Detect which tool produced this result
                    let preceding_tool = self.find_preceding_tool_name(i);

                    // Check if this is read tool output (line-numbered) and try syntax highlighting
                    let highlighted_lines = if !is_error && is_read_tool_output(&result_lines) {
                        let ext = self.find_preceding_read_extension(i);
                        highlight_read_output(&result_lines[..show], &ext, &m)
                    } else if !is_error && preceding_tool.as_deref() == Some("bash") {
                        Some(highlight_bash_output(&result_lines[..show], &m))
                    } else {
                        None
                    };

                    if let Some(hl_lines) = highlighted_lines {
                        lines.extend(hl_lines);
                    } else {
                        for line in &result_lines[..show] {
                            // Try to detect and highlight grep output (filepath:linenum:content)
                            if let Some(grep_spans) = try_highlight_grep_line(line, &m) {
                                lines.push(Line::from(grep_spans));
                            } else {
                                let full = format!("{}       {}", m, line);
                                for wline in wrap_text(&full, width) {
                                    lines.push(Line::from(Span::styled(wline, style)));
                                }
                            }
                        }
                    }
                    if result_lines.len() > show {
                        lines.push(Line::from(Span::styled(
                            format!("{}       +{} lines", m, result_lines.len() - show),
                            Style::default().fg(THEME.muted),
                        )));
                    }
                }

                ChatMessage::Error(err) => {
                    lines.push(Line::from(vec![
                        Span::styled(format!("{}  \u{2718} ", m), Style::default().fg(THEME.error_color)),
                        Span::styled(err.clone(), Style::default().fg(THEME.error_color)),
                    ]));
                }

                ChatMessage::System(msg) => {
                    lines.push(Line::from(Span::styled(
                        format!("{}  {}", m, msg),
                        Style::default().fg(THEME.muted).add_modifier(Modifier::DIM),
                    )));
                }
            }
        }

        lines
    }
}

/// Parse inline markdown: **bold**, *italic*, `code`
fn parse_inline_md(text: &str, base_style: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut chars = text.chars().peekable();
    let mut buf = String::new();

    let bold_style = base_style.add_modifier(Modifier::BOLD);
    let italic_style = base_style.add_modifier(Modifier::ITALIC);
    let code_style = Style::default().fg(THEME.code_fg).bg(THEME.code_bg);

    while let Some(ch) = chars.next() {
        match ch {
            '`' => {
                if !buf.is_empty() {
                    spans.push(Span::styled(buf.clone(), base_style));
                    buf.clear();
                }
                let mut code = String::new();
                while let Some(&c) = chars.peek() {
                    if c == '`' { chars.next(); break; }
                    code.push(c);
                    chars.next();
                }
                if !code.is_empty() {
                    spans.push(Span::styled(format!(" {} ", code), code_style));
                }
            }
            '*' => {
                if !buf.is_empty() {
                    spans.push(Span::styled(buf.clone(), base_style));
                    buf.clear();
                }
                let is_bold = chars.peek() == Some(&'*');
                if is_bold { chars.next(); }
                let delim = if is_bold { "**" } else { "*" };
                let mut inner = String::new();
                loop {
                    match chars.next() {
                        Some('*') if is_bold => {
                            if chars.peek() == Some(&'*') { chars.next(); break; }
                            inner.push('*');
                        }
                        Some('*') if !is_bold => break,
                        Some(c) => inner.push(c),
                        None => { inner = format!("{}{}", delim, inner); break; }
                    }
                }
                let style = if is_bold { bold_style } else { italic_style };
                spans.push(Span::styled(inner, style));
            }
            _ => buf.push(ch),
        }
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, base_style));
    }
    spans
}

/// Render a markdown table into styled Lines with box-drawing borders.
///
/// Parses the collected table lines into a grid, calculates column widths,
/// and renders with Unicode box-drawing characters. The separator row
/// (|---|---|) is detected and skipped — it just confirms we have a header.
fn render_table(table_lines: &[String], prefix: &str, _width: usize) -> Vec<Line<'static>> {
    let mut result: Vec<Line> = Vec::new();
    if table_lines.is_empty() {
        return result;
    }

    // Parse each line into cells
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut has_header = false;

    for (i, line) in table_lines.iter().enumerate() {
        let stripped = line.trim().trim_matches('|');
        // Detect separator row: all cells are just dashes/colons/spaces
        let is_separator = stripped.split('|').all(|cell| {
            let c = cell.trim();
            !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':' || ch == ' ')
        });

        if is_separator {
            // Separator after first row means row 0 is the header
            if i == 1 {
                has_header = true;
            }
            continue;
        }

        let cells: Vec<String> = stripped
            .split('|')
            .map(|c| c.trim().to_string())
            .collect();
        rows.push(cells);
    }

    if rows.is_empty() {
        return result;
    }

    // Normalize column count
    let num_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    for row in &mut rows {
        while row.len() < num_cols {
            row.push(String::new());
        }
    }

    // Calculate column widths using display width (not byte length)
    // This correctly handles emojis, CJK chars, etc. that take 2 terminal columns
    let mut col_widths: Vec<usize> = vec![3; num_cols];
    for row in &rows {
        for (j, cell) in row.iter().enumerate() {
            if j < num_cols {
                col_widths[j] = col_widths[j].max(UnicodeWidthStr::width(cell.as_str()));
            }
        }
    }

    let border_style = Style::default().fg(THEME.table_border_color);
    let header_style = Style::default().fg(THEME.table_header_color).add_modifier(ratatui::style::Modifier::BOLD);
    let cell_style = Style::default().fg(THEME.table_cell_color);

    // Top border: ┌───┬───┐
    let mut top = format!("{}  \u{250C}", prefix);
    for (j, w) in col_widths.iter().enumerate() {
        top.push_str(&"\u{2500}".repeat(w + 2));
        if j < num_cols - 1 {
            top.push('\u{252C}');
        }
    }
    top.push('\u{2510}');
    result.push(Line::from(Span::styled(top, border_style)));

    for (i, row) in rows.iter().enumerate() {
        // Data row: │ cell │ cell │
        let mut spans: Vec<Span> = Vec::new();
        spans.push(Span::styled(format!("{}  \u{2502}", prefix), border_style));

        for (j, cell) in row.iter().enumerate() {
            let w = col_widths[j];
            let display_w = UnicodeWidthStr::width(cell.as_str());
            let padding = w.saturating_sub(display_w);
            let padded = format!(" {}{} ", cell, " ".repeat(padding));
            let style = if has_header && i == 0 { header_style } else { cell_style };
            spans.push(Span::styled(padded, style));
            if j < num_cols - 1 {
                spans.push(Span::styled("\u{2502}", border_style));
            }
        }
        spans.push(Span::styled("\u{2502}", border_style));
        result.push(Line::from(spans));

        // After header row, draw separator: ├───┼───┤
        if has_header && i == 0 {
            let mut sep = format!("{}  \u{251C}", prefix);
            for (j, w) in col_widths.iter().enumerate() {
                sep.push_str(&"\u{2500}".repeat(w + 2));
                if j < num_cols - 1 {
                    sep.push('\u{253C}');
                }
            }
            sep.push('\u{2524}');
            result.push(Line::from(Span::styled(sep, border_style)));
        }
    }

    // Bottom border: └───┴───┘
    let mut bot = format!("{}  \u{2514}", prefix);
    for (j, w) in col_widths.iter().enumerate() {
        bot.push_str(&"\u{2500}".repeat(w + 2));
        if j < num_cols - 1 {
            bot.push('\u{2534}');
        }
    }
    bot.push('\u{2518}');
    result.push(Line::from(Span::styled(bot, border_style)));

    result
}

/// Render markdown text into Lines, handling code blocks, headings, lists, quotes, tables
fn render_markdown(text: &str, prefix: &str, width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    let base_style = Style::default().fg(THEME.claude_text);
    let mut in_code_block = false;
    let mut code_lang = String::new();
    let mut code_buf = String::new();
    let mut table_buf: Vec<String> = Vec::new();

    let all_lines: Vec<&str> = text.lines().collect();

    for (line_idx, line) in all_lines.iter().enumerate() {
        let trimmed = line.trim();

        // Code block toggle
        if trimmed.starts_with("```") {
            // Flush any pending table
            if !table_buf.is_empty() {
                lines.extend(render_table(&table_buf, prefix, width));
                table_buf.clear();
            }
            if !in_code_block {
                in_code_block = true;
                code_lang = trimmed[3..].trim().to_string();
                let label = if code_lang.is_empty() {
                    format!("{}  \u{2500}\u{2500}\u{2500}", prefix)
                } else {
                    format!("{}  \u{2500}\u{2500} {} \u{2500}\u{2500}", prefix, code_lang)
                };
                lines.push(Line::from(Span::styled(label, Style::default().fg(THEME.muted))));
                code_buf.clear();
            } else {
                // End of code block — highlight and flush
                lines.extend(highlight_code_block(&code_buf, &code_lang, prefix));
                in_code_block = false;
                lines.push(Line::from(Span::styled(
                    format!("{}  \u{2500}\u{2500}\u{2500}", prefix),
                    Style::default().fg(THEME.muted),
                )));
            }
            continue;
        }

        if in_code_block {
            code_buf.push_str(line);
            code_buf.push('\n');
            continue;
        }

        // Table detection: line contains | and is not inside a code block
        // A table line has at least one | that's not at the very start/end only
        let is_table_line = trimmed.contains('|') && {
            let stripped = trimmed.trim_matches('|').trim();
            // Separator rows (|---|---|) or data rows (| foo | bar |)
            !stripped.is_empty()
        };

        if is_table_line {
            table_buf.push(trimmed.to_string());
            // Check if next line is NOT a table line (or we're at the end) — flush
            let next_is_table = if line_idx + 1 < all_lines.len() {
                let next = all_lines[line_idx + 1].trim();
                next.contains('|') && {
                    let s = next.trim_matches('|').trim();
                    !s.is_empty()
                }
            } else {
                false
            };
            if !next_is_table {
                lines.extend(render_table(&table_buf, prefix, width));
                table_buf.clear();
            }
            continue;
        }

        // Flush any pending table (shouldn't happen, but safety)
        if !table_buf.is_empty() {
            lines.extend(render_table(&table_buf, prefix, width));
            table_buf.clear();
        }

        // Headings
        if trimmed.starts_with('#') {
            let level = trimmed.chars().take_while(|&c| c == '#').count();
            let heading_text = trimmed[level..].trim();
            let full = format!("{}  {}", prefix, heading_text);
            for wline in wrap_text(&full, width) {
                lines.push(Line::from(Span::styled(
                    wline,
                    Style::default().fg(THEME.heading_color).add_modifier(Modifier::BOLD),
                )));
            }
            continue;
        }

        // Blockquotes
        if trimmed.starts_with('>') {
            let quote_text = trimmed[1..].trim();
            let full = format!("{}  \u{2502} {}", prefix, quote_text);
            for wline in wrap_text(&full, width) {
                lines.push(Line::from(Span::styled(wline, Style::default().fg(THEME.quote_color).add_modifier(Modifier::ITALIC))));
            }
            continue;
        }

        // List items
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            let item_text = &trimmed[2..];
            let bullet_span = Span::styled(format!("{}  \u{2022} ", prefix), Style::default().fg(THEME.list_bullet_color));
            let mut item_spans = parse_inline_md(item_text, base_style);
            let mut all_spans = vec![bullet_span];
            all_spans.append(&mut item_spans);
            lines.push(Line::from(all_spans));
            continue;
        }

        // Numbered lists
        if trimmed.len() > 2 {
            let num_end = trimmed.find(". ");
            if let Some(pos) = num_end {
                if pos <= 3 && trimmed[..pos].chars().all(|c| c.is_ascii_digit()) {
                    let item_text = &trimmed[pos + 2..];
                    let num_span = Span::styled(
                        format!("{}  {}. ", prefix, &trimmed[..pos]),
                        Style::default().fg(THEME.list_bullet_color),
                    );
                    let mut item_spans = parse_inline_md(item_text, base_style);
                    let mut all_spans = vec![num_span];
                    all_spans.append(&mut item_spans);
                    lines.push(Line::from(all_spans));
                    continue;
                }
            }
        }

        // Empty lines
        if trimmed.is_empty() {
            lines.push(Line::from(""));
            continue;
        }

        // Regular text with inline markdown
        let full_prefix = format!("{}  ", prefix);
        let spans = parse_inline_md(line, base_style);
        // For simplicity, flatten spans into a string for wrapping, then re-parse
        // This loses some formatting on wrap boundaries but keeps it simple
        let flat: String = spans.iter().map(|s| s.content.as_ref()).collect();
        let full = format!("{}{}", full_prefix, flat);
        if full.chars().count() <= width {
            let mut line_spans = vec![Span::styled(full_prefix, base_style)];
            line_spans.extend(spans);
            lines.push(Line::from(line_spans));
        } else {
            // Wrap and re-parse each wrapped line
            for wline in wrap_text(&full, width) {
                let inner = if wline.starts_with(&full_prefix) {
                    &wline[full_prefix.len()..]
                } else {
                    &wline
                };
                let parsed = parse_inline_md(inner, base_style);
                if wline.starts_with(&full_prefix) {
                    let mut line_spans = vec![Span::styled(full_prefix.clone(), base_style)];
                    line_spans.extend(parsed);
                    lines.push(Line::from(line_spans));
                } else {
                    lines.push(Line::from(parsed));
                }
            }
        }
    }

    lines
}

#[allow(unused_assignments)]
fn wrap_text(raw_text: &str, width: usize) -> Vec<String> {
    let text = raw_text.replace('\t', "    ");
    if width == 0 || text.chars().count() <= width {
        return vec![text];
    }

    let mut lines = Vec::new();
    let mut current = String::new();

    for word in text.split_inclusive(' ') {
        let wlen = word.chars().count();
        let col = current.chars().count();
        if col + wlen > width && col > 0 {
            lines.push(current.trim_end().to_string());
            current = String::new();
        }
        // Word longer than width — hard break it
        if wlen > width {
            let chars: Vec<char> = word.chars().collect();
            for chunk in chars.chunks(width) {
                if !current.is_empty() {
                    lines.push(current.trim_end().to_string());
                    current = String::new();
                }
                current = chunk.iter().collect::<String>();
            }
        } else {
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current.trim_end().to_string());
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 { format!("{:.1}M", n as f64 / 1_000_000.0) }
    else if n >= 1_000 { format!("{:.1}k", n as f64 / 1_000.0) }
    else { format!("{}", n) }
}

fn boot_effect() -> Effect {
    use tachyonfx::fx::Direction as FxDir;
    fx::parallel(&[
        // CRT-style scanline reveal, top-to-bottom, clean (no randomness) with a tight gradient trail
        fx::sweep_in(FxDir::UpToDown, 10, 0, Color::Rgb(12, 22, 35), (750, Interpolation::QuintOut)),
        // long, slow fade from pure black — elegant deceleration
        fx::fade_from_fg(Color::Rgb(5, 8, 18), (750, Interpolation::QuintOut)),
    ])
}

fn quit_effect() -> Effect {
    use tachyonfx::fx::Direction as FxDir;
    fx::sequence(&[
        fx::hsl_shift_fg([180.0, -40.0, 0.0], (180, Interpolation::QuadOut)),
        fx::parallel(&[
            fx::sweep_out(FxDir::DownToUp, 18, 12, Color::Rgb(40, 40, 44), (650, Interpolation::QuadIn)),
            fx::dissolve((650, Interpolation::QuadIn)),
            fx::fade_to_fg(Color::Black, (650, Interpolation::QuadIn)),
        ]),
    ])
}

fn draw(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    model: &str,
    thinking: &str,
    effect: &mut Option<Effect>,
    exit_effect: &mut Option<Effect>,
    elapsed: std::time::Duration,
) -> io::Result<()> {
    // Don't draw while casino owns the terminal
    if app.gamba_child.is_some() {
        return Ok(());
    }
    terminal.draw(|frame| {
        // Subagent panel height: 2 (border top/bottom) + 1 per active agent
        let has_subagents = !app.subagents.is_empty();
        let subagent_height = if has_subagents {
            (app.subagents.len() as u16 + 2).min(8) // cap at 6 agents visible
        } else {
            0
        };

        // Dynamic input height: borders (2) + wrapped text lines, capped at 10
        let frame_width = frame.area().width;
        let input_inner_width = frame_width.saturating_sub(2); // subtract border left+right
        let (input_lines, _, _) = app.input_wrap_info(input_inner_width);
        let max_input_lines: u16 = 10;
        let input_height = (input_lines.min(max_input_lines)) + 2; // +2 for borders

        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),              // header
                Constraint::Min(1),                 // messages
                Constraint::Length(subagent_height), // subagent panel (0 when inactive)
                Constraint::Length(input_height),   // input (expands with content)
                Constraint::Length(1),              // footer
            ])
            .split(frame.area());

        // -- Header ----------------------------------------------------------
        let status_span = if let Some(ref status) = app.status_text {
            let spinner_idx = (app.spinner_frame / 3) % SPINNER_FRAMES.len();
            Span::styled(
                format!(" {} {} ", SPINNER_FRAMES[spinner_idx], status),
                Style::default().fg(THEME.status_streaming),
            )
        } else if !app.subagents.is_empty() {
            let active = app.subagents.iter().filter(|s| !s.done).count();
            let done = app.subagents.iter().filter(|s| s.done).count();
            let spinner_idx = (app.spinner_frame / 3) % SPINNER_FRAMES.len();
            let spinner = if active > 0 { SPINNER_FRAMES[spinner_idx] } else { "\u{2714}" };
            Span::styled(
                format!(" {} {} agent{} ({} done) ", spinner, active, if active != 1 { "s" } else { "" }, done),
                Style::default().fg(THEME.subagent_name),
            )
        } else if app.streaming {
            let pulse = ((app.spinner_frame as f64 / 20.0).sin() * 0.3 + 0.7).max(0.4);
            let r = (220.0 * pulse) as u8;
            let g = (175.0 * pulse) as u8;
            let b = (60.0 * pulse) as u8;
            Span::styled(" \u{25cf} streaming ", Style::default().fg(Color::Rgb(r, g, b)))
        } else {
            Span::styled(" \u{25cb} ready ", Style::default().fg(THEME.status_ready))
        };
        let header = Paragraph::new(Line::from(vec![
            Span::styled("  Synaps", Style::default().fg(THEME.header_fg).add_modifier(Modifier::BOLD)),
            Span::styled("CLI ", Style::default().fg(THEME.muted)),
            Span::styled("\u{2502}", Style::default().fg(THEME.border)),
            status_span,
        ]))
        .style(Style::default().bg(THEME.bg));
        frame.render_widget(header, outer[0]);

        // -- Messages --------------------------------------------------------
        let msg_area = outer[1];
        let content_height = msg_area.height.saturating_sub(2) as usize;
        let content_width = msg_area.width.saturating_sub(2) as usize; // horizontal padding only (no left/right borders)

        // Rebuild line cache only when content changed or width changed
        if app.dirty || app.cache_width != content_width {
            app.line_cache = app.render_lines(content_width);
            app.cache_width = content_width;
            app.dirty = false;
        }

        let all_lines = &app.line_cache;
        let total = all_lines.len();

        // When pinned, always show the latest content (scroll_back = 0).
        // When unpinned, compensate for new content so viewport stays stationary.
        if app.scroll_pinned {
            app.scroll_back = 0;
        } else {
            // Content grew while user was scrolled up — increase scroll_back
            // by the delta so the viewport doesn't slide down.
            let prev = app.last_line_count;
            if total > prev && prev > 0 {
                let growth = (total - prev) as u16;
                app.scroll_back = app.scroll_back.saturating_add(growth);
            }
            // Clamp so we don't scroll past the beginning
            let max_back = total.saturating_sub(content_height).min(u16::MAX as usize) as u16;
            if app.scroll_back > max_back {
                app.scroll_back = max_back;
            }
        }
        app.last_line_count = total;

        let end = total.saturating_sub(app.scroll_back as usize);
        let start = end.saturating_sub(content_height);
        let visible: Vec<Line> = all_lines[start..end].to_vec();

        let msg_block = Block::default()
            .borders(Borders::TOP | Borders::BOTTOM)
            .border_type(BorderType::Plain)
            .border_style(Style::default().fg(THEME.border))
            .padding(Padding::horizontal(1));
        let messages_widget = Paragraph::new(visible.clone()).block(msg_block);
        frame.render_widget(Clear, msg_area);
        frame.render_widget(messages_widget, msg_area);

        // Empty state: SYNAPS with CRT dismiss animation
        let show_logo = app.messages.is_empty() || app.logo_dismiss_t.is_some();
        if show_logo && visible.is_empty() {
            let ascii_art: Vec<&str> = vec![
                r" ███████ ██    ██ ███    ██  █████  ██████  ███████",
                r" ██       ██  ██  ████   ██ ██   ██ ██   ██ ██    ",
                r" ███████   ████   ██ ██  ██ ███████ ██████  ███████",
                r"      ██    ██    ██  ██ ██ ██   ██ ██           ██",
                r" ███████    ██    ██   ████ ██   ██ ██      ███████",
            ];

            let art_char_widths: Vec<usize> = ascii_art.iter()
                .map(|l| l.chars().count())
                .collect();
            let max_art_width = art_char_widths.iter().copied().max().unwrap_or(0);
            let avail_w = msg_area.width as usize;
            let avail_h = msg_area.height as usize;

            let art_height = ascii_art.len();
            let sub_text = "neural interface ready";
            let sub_width = sub_text.chars().count();
            let total_block = art_height + 3;

            if avail_h >= total_block && avail_w >= max_art_width + 2 {
                let center_y = msg_area.y + msg_area.height / 2;
                let dismiss_t = app.logo_dismiss_t.unwrap_or(0.0);

                // Time for breathing (only when not dismissing)
                let t = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();

                if dismiss_t < 0.001 {
                    // Normal state / Boot-in
                    let phase1 = ((t % 4000) as f64 / 4000.0 * std::f64::consts::PI * 2.0).sin();
                    let phase2 = ((t % 6500) as f64 / 6500.0 * std::f64::consts::PI * 2.0).sin();
                    let breathe = phase1 * 0.7 + phase2 * 0.3;
                    let r = (20.0 + 15.0 * breathe) as u8;
                    let g = (160.0 + 50.0 * breathe) as u8;
                    let b = (210.0 + 35.0 * breathe) as u8;
                    let art_style = Style::default().fg(Color::Rgb(r, g, b)).add_modifier(Modifier::BOLD);
                    let sub_style = Style::default().fg(Color::Rgb(
                        (35.0 + 10.0 * breathe) as u8,
                        (70.0 + 20.0 * breathe) as u8,
                        (95.0 + 15.0 * breathe) as u8,
                    ));

                    let build_t = app.logo_build_t.unwrap_or(1.0);
                    let start_y = center_y.saturating_sub((total_block as u16) / 2);

                    for (j, line) in ascii_art.iter().enumerate() {
                        let char_w = art_char_widths[j];
                        let x = msg_area.x + (avail_w as u16).saturating_sub(char_w as u16) / 2;
                        let y = start_y + j as u16;
                        if y >= msg_area.y && y < msg_area.y + msg_area.height {
                            let clamped_w = char_w.min(avail_w);

                            if build_t >= 1.0 {
                                let area = ratatui::layout::Rect { x, y, width: clamped_w as u16, height: 1 };
                                frame.render_widget(Paragraph::new(Span::styled(line.to_string(), art_style)), area);
                            } else {
                                // Diagonal assemby: Bottom-Right to Top-Left
                                let mut built = String::with_capacity(clamped_w);
                                let build_chars: &[char] = &['░', '▒', '▓'];
                                for (ci, ch) in line.chars().take(clamped_w).enumerate() {
                                    let inv_row = (art_height - 1 - j) as f64;
                                    let inv_col = (max_art_width.saturating_sub(ci + 1)) as f64;
                                    let diag = (inv_row + inv_col) / (art_height as f64 + max_art_width as f64);
                                    
                                    if build_t >= diag {
                                        let lp = ((build_t - diag) / 0.15).min(1.0);
                                        if lp < 1.0 && ch != ' ' {
                                            built.push(build_chars[(lp * build_chars.len() as f64) as usize]);
                                        } else { built.push(ch); }
                                    } else { built.push(' '); }
                                }
                                let area = ratatui::layout::Rect { x, y, width: clamped_w as u16, height: 1 };
                                frame.render_widget(Paragraph::new(Span::styled(built, art_style)), area);
                            }
                        }
                    }

                    if build_t >= 1.0 {
                        let sub_y = start_y + art_height as u16 + 1;
                        if sub_y >= msg_area.y && sub_y < msg_area.y + msg_area.height && avail_w >= sub_width {
                            let sub_x = msg_area.x + (avail_w as u16).saturating_sub(sub_width as u16) / 2;
                            let area = ratatui::layout::Rect { x: sub_x, y: sub_y, width: sub_width as u16, height: 1 };
                            frame.render_widget(Paragraph::new(Span::styled(sub_text, sub_style)), area);
                        }
                    }
                } else {
                    // Clean Dismiss: Top-Left to Bottom-Right (reverse of build-in)
                    let art_style = Style::default().fg(Color::Rgb(30, 80, 120)).add_modifier(Modifier::BOLD);
                    let start_y = center_y.saturating_sub((total_block as u16) / 2);

                    for (j, line) in ascii_art.iter().enumerate() {
                        let char_w = art_char_widths[j];
                        let x = msg_area.x + (avail_w as u16).saturating_sub(char_w as u16) / 2;
                        let y = start_y + j as u16;
                        
                        if y >= msg_area.y && y < msg_area.y + msg_area.height {
                            let clamped_w = char_w.min(avail_w);
                            let mut dis = String::with_capacity(clamped_w);
                            let dis_chars: &[char] = &['▓', '▒', '░'];

                            for (ci, ch) in line.chars().take(clamped_w).enumerate() {
                                // Diagonal dismantling: Top-Left to Bottom-Right
                                let row = j as f64;
                                let col = ci as f64;
                                let diag = (row + col) / (art_height as f64 + max_art_width as f64);
                                
                                // dismiss_t goes 0 -> 1. 1.0 - diag is when it starts dismantled.
                                let threshold = diag; 
                                if dismiss_t < (1.0 - threshold) {
                                    // Still visible, but check if starting to fade
                                    let rem = (1.0 - threshold) - dismiss_t;
                                    if rem < 0.15 && ch != ' ' {
                                        let idx = ((1.0 - rem/0.15) * dis_chars.len() as f64) as usize;
                                        dis.push(dis_chars[idx.min(dis_chars.len()-1)]);
                                    } else { dis.push(ch); }
                                } else { dis.push(' '); }
                            }
                            let area = ratatui::layout::Rect { x, y, width: clamped_w as u16, height: 1 };
                            frame.render_widget(Paragraph::new(Span::styled(dis, art_style)), area);
                        }
                    }
                }
            }
        }

        // Scroll indicator
        if app.scroll_back > 0 {
            let indicator = format!(" \u{2191}{} ", app.scroll_back);
            let indicator_widget = Paragraph::new(Span::styled(
                indicator,
                Style::default().fg(THEME.muted),
            ))
            .alignment(Alignment::Right);
            let indicator_area = ratatui::layout::Rect {
                x: msg_area.x,
                y: msg_area.y,
                width: msg_area.width,
                height: 1,
            };
            frame.render_widget(indicator_widget, indicator_area);
        }

        // -- Subagent Panel ---------------------------------------------------
        if has_subagents {
            let spinner_idx = (app.spinner_frame / 3) % SPINNER_FRAMES.len();
            let mut agent_lines: Vec<Line> = Vec::new();

            for sa in &app.subagents {
                let elapsed = sa.duration_secs.unwrap_or_else(|| sa.start_time.elapsed().as_secs_f64());
                let time_str = if elapsed < 60.0 {
                    format!("{:.1}s", elapsed)
                } else {
                    format!("{}m{:.0}s", (elapsed / 60.0) as u32, elapsed % 60.0)
                };

                if sa.done {
                    agent_lines.push(Line::from(vec![
                        Span::styled("  \u{2714} ", Style::default().fg(THEME.subagent_done)),
                        Span::styled(
                            format!("{} ", sa.name),
                            Style::default().fg(THEME.subagent_name).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            &sa.status,
                            Style::default().fg(THEME.subagent_done).add_modifier(Modifier::DIM),
                        ),
                        Span::styled(
                            format!("  {}", time_str),
                            Style::default().fg(THEME.subagent_time),
                        ),
                    ]));
                } else {
                    let spinner = SPINNER_FRAMES[spinner_idx];
                    agent_lines.push(Line::from(vec![
                        Span::styled(
                            format!("  {} ", spinner),
                            Style::default().fg(THEME.subagent_name),
                        ),
                        Span::styled(
                            format!("{} ", sa.name),
                            Style::default().fg(THEME.subagent_name).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            &sa.status,
                            Style::default().fg(THEME.subagent_status),
                        ),
                        Span::styled(
                            format!("  {}", time_str),
                            Style::default().fg(THEME.subagent_time),
                        ),
                    ]));
                }
            }

            let active = app.subagents.iter().filter(|s| !s.done).count();
            let done = app.subagents.iter().filter(|s| s.done).count();
            let title = if done > 0 && active > 0 {
                format!(" \u{25c8} {} running, {} done ", active, done)
            } else if active > 0 {
                format!(" \u{25c8} {} agent{} ", active, if active != 1 { "s" } else { "" })
            } else {
                format!(" \u{2714} {} done ", done)
            };

            let agent_block = Block::default()
                .title(Span::styled(
                    title,
                    Style::default().fg(THEME.subagent_name).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(THEME.subagent_border))
                .style(Style::default().bg(THEME.bg));
            let agent_widget = Paragraph::new(agent_lines).block(agent_block);
            frame.render_widget(agent_widget, outer[2]);
        }

        // -- Input -----------------------------------------------------------
        let input_border_color = if app.streaming { THEME.border } else { THEME.border_active };
        let input_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(input_border_color))
            .style(Style::default().bg(THEME.bg));
        // Build pre-wrapped input lines using char-level wrapping (must match input_wrap_info exactly)
        let input_lines_vec: Vec<Line> = {
            use unicode_width::UnicodeWidthChar;
            let w = input_inner_width.max(1) as usize;
            let prefix_width: usize = 2;
            let prompt_style = Style::default().fg(THEME.prompt_fg);
            let input_style = Style::default().fg(THEME.input_fg);

            let mut rows: Vec<Vec<Span>> = Vec::new();
            let mut current_row: Vec<Span> = vec![Span::styled("\u{276f} ", prompt_style)];
            let mut col: usize = prefix_width;

            for ch in app.input.chars() {
                if ch == '\n' {
                    rows.push(std::mem::take(&mut current_row));
                    current_row = vec![Span::styled("  ", prompt_style)];
                    col = prefix_width;
                    continue;
                }
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                if col + cw > w {
                    rows.push(std::mem::take(&mut current_row));
                    current_row = Vec::new();
                    col = 0;
                }
                // Accumulate character into a string span
                let mut s = String::new();
                s.push(ch);
                current_row.push(Span::styled(s, input_style));
                col += cw;
            }
            rows.push(current_row);
            rows.into_iter().map(Line::from).collect()
        };

        // Scroll offset: keep cursor visible when input exceeds max visible lines
        let (_, cursor_row, cursor_col) = app.input_wrap_info(input_inner_width);
        let visible_lines = max_input_lines;
        let input_scroll: u16 = if cursor_row >= visible_lines {
            cursor_row - visible_lines + 1
        } else {
            0
        };

        let input_widget = Paragraph::new(input_lines_vec)
        .scroll((input_scroll, 0))
        .block(input_block);
        frame.render_widget(input_widget, outer[3]);

        // Cursor — position relative to scroll offset
        frame.set_cursor_position((
            outer[3].x + 1 + cursor_col,
            outer[3].y + 1 + cursor_row - input_scroll,
        ));

        // -- Footer ----------------------------------------------------------
        let footer_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(model.len() as u16 + 75),
            ])
            .split(outer[4]);

        let key_style = Style::default().fg(Color::Rgb(60, 75, 95));
        let label_style = Style::default().fg(THEME.help_fg);
        let dot_style = Style::default().fg(Color::Rgb(30, 38, 52));

        let keybinds = Paragraph::new(Line::from(vec![
            Span::styled(" ctrl+c ", key_style),
            Span::styled("quit", label_style),
            Span::styled(" \u{00b7} ", dot_style),
            Span::styled("esc ", key_style),
            Span::styled("abort", label_style),
            Span::styled(" \u{00b7} ", dot_style),
            Span::styled("shift+\u{2191}\u{2193} ", key_style),
            Span::styled("scroll", label_style),
            Span::styled(" \u{00b7} ", dot_style),
            Span::styled("ctrl+o ", key_style),
            Span::styled(
                if app.show_full_output { "full" } else { "compact" },
                label_style,
            ),
            Span::styled(" \u{00b7} ", dot_style),
            Span::styled("enter ", key_style),
            Span::styled("send", label_style),
        ]))
        .style(Style::default().bg(THEME.bg));
        frame.render_widget(keybinds, footer_chunks[0]);

        let cost_str = if app.session_cost > 0.0 {
            format!("${:.4} ", app.session_cost)
        } else {
            String::new()
        };
        let cache_rate = if app.total_cache_read_tokens + app.total_cache_creation_tokens > 0 {
            let total_cacheable = app.total_cache_read_tokens + app.total_cache_creation_tokens;
            let rate = (app.total_cache_read_tokens as f64 / total_cacheable as f64 * 100.0) as u32;
            format!(" {}%↺", rate)
        } else {
            String::new()
        };
        let token_str = if app.total_input_tokens > 0 || app.total_output_tokens > 0 {
            format!(
                "{}\u{2191} {}\u{2193}{}  ",
                format_tokens(app.total_input_tokens),
                format_tokens(app.total_output_tokens),
                cache_rate,
            )
        } else {
            String::new()
        };
        let info = Paragraph::new(Line::from(vec![
            Span::styled(&cost_str, Style::default().fg(THEME.cost_color)),
            Span::styled(&token_str, Style::default().fg(THEME.muted)),
            {
                // Context usage bar — shows how much of the 200k context window is consumed
                let total_used = app.total_input_tokens + app.total_output_tokens;
                if total_used > 0 {
                    let context_window: u64 = 200_000;
                    let usage_ratio = (total_used as f64 / context_window as f64).min(1.0);
                    let bar_width: usize = 14;
                    let filled = (usage_ratio * bar_width as f64).round() as usize;
                    let empty = bar_width.saturating_sub(filled);
                    let bar_color = if usage_ratio < 0.5 {
                        Color::Rgb(50, 180, 210)
                    } else if usage_ratio < 0.75 {
                        Color::Rgb(210, 175, 60)
                    } else {
                        Color::Rgb(220, 70, 70)
                    };
                    let pct = (usage_ratio * 100.0) as u32;
                    Span::styled(
                        format!("{}{} {}% ", "\u{2593}".repeat(filled), "\u{2591}".repeat(empty), pct),
                        Style::default().fg(bar_color),
                    )
                } else {
                    Span::raw("")
                }
            },
            Span::styled("\u{03b8}:", Style::default().fg(THEME.muted)),
            Span::styled(format!("{}", thinking), Style::default().fg(THEME.help_fg)),
            Span::styled(" \u{2502} ", Style::default().fg(THEME.border)),
            Span::styled(model, Style::default().fg(THEME.header_fg)),
            Span::styled(" ", Style::default()),
        ]))
        .alignment(Alignment::Right)
        .style(Style::default().bg(THEME.bg));
        frame.render_widget(info, footer_chunks[1]);

        if let Some(ref mut fx) = effect {
            let area = frame.area();
            fx.process(elapsed.into(), frame.buffer_mut(), area);
            if fx.done() {
                *effect = None;
            }
        }
        if let Some(ref mut fx) = exit_effect {
            let area = frame.area();
            fx.process(elapsed.into(), frame.buffer_mut(), area);
        }
    })?;
    Ok(())
}

fn rebuild_display_messages(api_messages: &[Value], app: &mut App) {
    for msg in api_messages {
        match msg["role"].as_str() {
            Some("user") => {
                if let Some(content) = msg["content"].as_str() {
                    app.push_msg(ChatMessage::User(content.to_string()));
                }
            }
            Some("assistant") => {
                if let Some(content) = msg["content"].as_array() {
                    for block in content {
                        match block["type"].as_str() {
                            Some("thinking") => {
                                if let Some(text) = block["thinking"].as_str() {
                                    app.push_msg(ChatMessage::Thinking(text.to_string()));
                                }
                            }
                            Some("text") => {
                                if let Some(text) = block["text"].as_str() {
                                    app.push_msg(ChatMessage::Text(text.to_string()));
                                }
                            }
                            Some("tool_use") => {
                                let name = block["name"].as_str().unwrap_or("").to_string();
                                let input = serde_json::to_string(&block["input"]).unwrap_or_default();
                                app.push_msg(ChatMessage::ToolUse { tool_name: name, input });
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

#[derive(Parser)]
#[command(name = "chatui", about = "Terminal chat UI for SynapsCLI")]
struct Cli {
    /// Continue a previous session. Optionally provide a session ID (partial match supported).
    #[arg(long = "continue", value_name = "SESSION_ID")]
    continue_session: Option<Option<String>>,

    /// System prompt: a string or a path to a file.
    #[arg(long = "system", short = 's', value_name = "PROMPT_OR_FILE")]
    system: Option<String>,

    #[arg(long, global = true)]
    profile: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(ref prof) = cli.profile {
        synaps_cli::config::set_profile(Some(prof.clone()));
    }

    let _log_guard = synaps_cli::logging::init_logging();
    let mut runtime = Runtime::new().await?;

    // Load config and apply
    let config = synaps_cli::config::load_config();
    synaps_cli::config::apply_config(&mut runtime, &config);

    // Load system prompt
    let system_prompt = synaps_cli::config::resolve_system_prompt(cli.system.as_deref());

    // Auto-load skills specified in config (injected into system prompt)
    let mut final_prompt = system_prompt;
    if let Some(ref skill_names) = config.skills {
        let auto_skills = synaps_cli::skills::load_skills(Some(skill_names));
        if !auto_skills.is_empty() {
            let names: Vec<&str> = auto_skills.iter().map(|s| s.name.as_str()).collect();
            eprintln!("\x1b[2m  📚 {} skills auto-loaded: {}\x1b[0m", auto_skills.len(), names.join(", "));
            final_prompt.push_str(&synaps_cli::skills::format_skills_for_prompt(&auto_skills));
        }
    }
    runtime.set_system_prompt(final_prompt);

    // Register load_skill tool for on-demand activation of any skill
    let skill_count = synaps_cli::skills::setup_skill_tool(&runtime.tools_shared()).await;
    if skill_count > 0 {
        eprintln!("\x1b[2m  📚 {} skills available on demand (load_skill tool)\x1b[0m", skill_count);
    }

    // Set up lazy MCP loading (if configured in ~/.synaps-cli/mcp.json)
    // Only registers the mcp_connect gateway tool — servers connect on demand.
    let mcp_server_count = synaps_cli::mcp::setup_lazy_mcp(&runtime.tools_shared()).await;
    if mcp_server_count > 0 {
        eprintln!("\x1b[2m  ⚡ {} MCP servers available (use mcp_connect to activate)\x1b[0m", mcp_server_count);
    }

    // Keep reference to system prompt path for save functionality
    let system_prompt_path = synaps_cli::config::resolve_read_path("system.md");

    // Session: continue existing or create new
    let mut app = match cli.continue_session {
        Some(maybe_id) => {
            let session = match maybe_id {
                Some(id) => find_session(&id).unwrap_or_else(|e| {
                    eprintln!("Failed to load session '{}': {}", id, e);
                    std::process::exit(1);
                }),
                None => latest_session().unwrap_or_else(|e| {
                    eprintln!("No sessions to continue: {}", e);
                    std::process::exit(1);
                }),
            };
            // Restore runtime settings from session
            runtime.set_model(session.model.clone());
            if let Some(ref sp) = session.system_prompt {
                runtime.set_system_prompt(sp.clone());
            }
            let mut app = App::new(session.clone());
            app.api_messages = session.api_messages.clone();
            app.total_input_tokens = session.total_input_tokens;
            app.total_output_tokens = session.total_output_tokens;
            app.session_cost = session.session_cost;
            // Rebuild display messages from api_messages
            rebuild_display_messages(&session.api_messages, &mut app);
            app.push_msg(ChatMessage::System(format!("resumed session {}", session.id)));
            app
        }
        None => {
            App::new(Session::new(runtime.model(), runtime.thinking_level(), runtime.system_prompt()))
        }
    };

    enable_raw_mode().unwrap();
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste).unwrap();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut event_reader = EventStream::new();
    let mut stream: Option<std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send>>> = None;
    let mut cancel_token: Option<CancellationToken> = None;
    let mut steer_tx: Option<tokio::sync::mpsc::UnboundedSender<String>> = None;
    let mut boot_fx: Option<Effect> = Some(boot_effect());
    let mut exit_fx: Option<Effect> = None;
    let mut last_frame = Instant::now();

    loop {
        let elapsed = last_frame.elapsed();
        last_frame = Instant::now();
        draw(&mut terminal, &mut app, runtime.model(), runtime.thinking_level(), &mut boot_fx, &mut exit_fx, elapsed).unwrap();

        tokio::select! {
            // Tick: redraws during animations AND during streaming (~60fps throttle)
            _ = tokio::time::sleep(std::time::Duration::from_millis(16)), if boot_fx.is_some() || exit_fx.is_some() || app.streaming || app.messages.is_empty() || app.logo_dismiss_t.is_some() || app.logo_build_t.is_some() || app.gamba_child.is_some() => {
                // Progress logo build-in animation
                if let Some(ref mut t) = app.logo_build_t {
                    *t += 0.025; // ~40 frames = ~0.66s
                    if *t >= 1.0 {
                        app.logo_build_t = None;
                    }
                }
                // Progress logo dismiss animation
                if let Some(ref mut t) = app.logo_dismiss_t {
                    *t += 0.04; // ~25 frames at 60fps = ~0.4s
                    if *t >= 1.0 {
                        app.logo_dismiss_t = None;
                    }
                }
                // Advance spinner for active subagents or streaming (~6 fps visual)
                if !app.subagents.is_empty() || app.streaming {
                    app.spinner_frame = app.spinner_frame.wrapping_add(1);
                    // Only dirty every 3rd tick (~20fps spinner, smooth but not crazy)
                    if app.spinner_frame % 3 == 0 {
                        app.dirty = true;
                        app.line_cache.clear();
                    }
                }
                // Check if casino exited on its own (user quit)
                if let Some(msg) = app.check_gamba_exited() {
                    terminal.clear().ok();
                    app.push_msg(ChatMessage::System(msg));
                    app.dirty = true;
                    app.line_cache.clear();
                    // Force immediate redraw so user doesn't see bare tmux
                    let elapsed = last_frame.elapsed();
                    last_frame = Instant::now();
                    draw(&mut terminal, &mut app, runtime.model(), runtime.thinking_level(), &mut boot_fx, &mut exit_fx, elapsed).unwrap();
                }
                if exit_fx.as_ref().map_or(false, |fx| fx.done()) {
                    break;
                }
                continue;
            }
            maybe_event = event_reader.next(), if app.gamba_child.is_none() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) => {
                        match (key.code, key.modifiers) {
                            (KeyCode::Char('c'), KeyModifiers::CONTROL) if exit_fx.is_none() => {
                                exit_fx = Some(quit_effect());
                            }
                            (KeyCode::Esc, _) if app.streaming => {
                                if let Some(ref ct) = cancel_token {
                                    ct.cancel();
                                }
                                // Capture partial work before clearing state
                                app.capture_abort_context();
                                // Clear queued message — user can retype or use input history
                                if let Some(ref q) = app.queued_message.take() {
                                    app.push_msg(ChatMessage::System(format!("dequeued: {}", q)));
                                }
                                stream = None;
                                cancel_token = None;
                                steer_tx = None;
                                app.streaming = false;
                                app.subagents.clear();
                                let abort_msg = if app.abort_context.is_some() {
                                    "aborted — context saved for next message"
                                } else {
                                    "aborted"
                                };
                                app.push_msg(ChatMessage::Error(abort_msg.to_string()));
                            }
                            (KeyCode::Enter, KeyModifiers::SHIFT) if !app.streaming => {
                                // Shift+Enter inserts a literal newline
                                let byte_pos = app.cursor_byte_pos();
                                app.input.insert(byte_pos, '\n');
                                app.cursor_pos += 1;
                            }
                            (KeyCode::Enter, _) if !app.streaming && !app.input.is_empty() => {
                                // Trigger CRT dismiss if this is the first message
                                if app.messages.is_empty() {
                                    app.logo_dismiss_t = Some(0.001);
                                }
                                let input = app.input.clone();
                                app.input_history.push(input.clone());
                                app.history_index = None;
                                app.input_stash.clear();
                                app.input.clear();
                                app.cursor_pos = 0;
                                app.scroll_back = 0;
                                app.scroll_pinned = true;

                                if input.starts_with('/') && input.len() > 1 {
                                    let parts: Vec<&str> = input[1..].splitn(2, ' ').collect();
                                    let raw_cmd = parts[0];
                                    let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");
                                    let all_cmds = ["clear", "model", "system", "thinking", "sessions", "resume", "theme", "gamba", "help", "quit", "exit"];
                                    // Resolve prefix: exact match first, then unique prefix
                                    let cmd = if all_cmds.contains(&raw_cmd) {
                                        raw_cmd.to_string()
                                    } else {
                                        let matches: Vec<&&str> = all_cmds.iter().filter(|c| c.starts_with(raw_cmd)).collect();
                                        if matches.len() == 1 {
                                            matches[0].to_string()
                                        } else {
                                            raw_cmd.to_string()
                                        }
                                    };
                                    match cmd.as_str() {
                                        "clear" => {
                                            app.save_session();
                                            app.messages.clear();
                                            app.dirty = true;
                                            app.api_messages.clear();
                                            app.total_input_tokens = 0;
                                            app.total_output_tokens = 0;
                                            app.total_cache_read_tokens = 0;
                                            app.total_cache_creation_tokens = 0;
                                            app.session_cost = 0.0;
                                            app.input_tokens = 0;
                                            app.output_tokens = 0;
                                            app.session = Session::new(runtime.model(), runtime.thinking_level(), runtime.system_prompt());
                                            app.push_msg(ChatMessage::System("new session started".to_string()));
                                        }
                                        "model" => {
                                            if arg.is_empty() {
                                                app.push_msg(ChatMessage::System(
                                                    format!("current model: {}", runtime.model())
                                                ));
                                            } else {
                                                runtime.set_model(arg.to_string());
                                                app.push_msg(ChatMessage::System(
                                                    format!("model set to: {}", arg)
                                                ));
                                            }
                                        }
                                        "system" => {
                                            if arg.is_empty() {
                                                app.push_msg(ChatMessage::System(
                                                    "usage: /system <prompt>  |  /system save  |  /system show".to_string()
                                                ));
                                            } else if arg == "save" {
                                                let _ = std::fs::create_dir_all(synaps_cli::config::get_active_config_dir());
                                                match std::fs::write(&system_prompt_path, runtime.system_prompt().unwrap_or("")) {
                                                    Ok(_) => app.push_msg(ChatMessage::System(
                                                        format!("saved to {}", system_prompt_path.display())
                                                    )),
                                                    Err(e) => app.push_msg(ChatMessage::Error(
                                                        format!("failed to save: {}", e)
                                                    )),
                                                }
                                            } else if arg == "show" {
                                                let prompt = runtime.system_prompt().unwrap_or("(none)");
                                                app.push_msg(ChatMessage::System(prompt.to_string()));
                                            } else {
                                                runtime.set_system_prompt(arg.to_string());
                                                app.push_msg(ChatMessage::System(
                                                    "system prompt updated".to_string()
                                                ));
                                            }
                                        }
                                        "thinking" => {
                                            match arg {
                                                "low" => { runtime.set_thinking_budget(2048); }
                                                "medium" | "med" => { runtime.set_thinking_budget(4096); }
                                                "high" => { runtime.set_thinking_budget(16384); }
                                                "xhigh" => { runtime.set_thinking_budget(32768); }
                                                "" => {
                                                    app.push_msg(ChatMessage::System(
                                                        format!("thinking: {} ({})", runtime.thinking_level(), runtime.thinking_budget())
                                                    ));
                                                }
                                                _ => {
                                                    app.push_msg(ChatMessage::Error(
                                                        "usage: /thinking low|medium|high|xhigh".to_string()
                                                    ));
                                                }
                                            }
                                            if !arg.is_empty() && ["low", "medium", "med", "high", "xhigh"].contains(&arg) {
                                                app.push_msg(ChatMessage::System(
                                                    format!("thinking set to: {}", runtime.thinking_level())
                                                ));
                                            }
                                        }
                                        "sessions" => {
                                            match list_sessions() {
                                                Ok(sessions) if sessions.is_empty() => {
                                                    app.push_msg(ChatMessage::System("no saved sessions".to_string()));
                                                }
                                                Ok(sessions) => {
                                                    app.push_msg(ChatMessage::System(format!("{} session(s):", sessions.len())));
                                                    for s in sessions.iter().take(20) {
                                                        let title = if s.title.is_empty() { "(untitled)" } else { &s.title };
                                                        let active = if s.id == app.session.id { " *" } else { "" };
                                                        app.push_msg(ChatMessage::System(format!(
                                                            "  {} — {} [{}] ${:.4}{}",
                                                            &s.id, title, s.model, s.session_cost, active
                                                        )));
                                                    }
                                                }
                                                Err(e) => {
                                                    app.push_msg(ChatMessage::Error(format!("failed to list sessions: {}", e)));
                                                }
                                            }
                                        }
                                        "resume" => {
                                            if arg.is_empty() {
                                                app.push_msg(ChatMessage::System("usage: /resume <session_id>".to_string()));
                                            } else {
                                                match find_session(arg) {
                                                    Ok(session) => {
                                                        runtime.set_model(session.model.clone());
                                                        if let Some(ref sp) = session.system_prompt {
                                                            runtime.set_system_prompt(sp.clone());
                                                        }
                                                        // Save current session before switching
                                                        app.save_session();
                                                        let old_id = app.session.id.clone();
                                                        // Rebuild app state from loaded session
                                                        app.messages.clear();
                                            app.dirty = true;
                                                        app.api_messages = session.api_messages.clone();
                                                        app.total_input_tokens = session.total_input_tokens;
                                                        app.total_output_tokens = session.total_output_tokens;
                                                        app.session_cost = session.session_cost;
                                                        // Rebuild display messages
                                                        rebuild_display_messages(&session.api_messages, &mut app);
                                                        app.session = session;
                                                        app.push_msg(ChatMessage::System(
                                                            format!("switched from {} to {}", old_id, app.session.id)
                                                        ));
                                                    }
                                                    Err(e) => {
                                                        app.push_msg(ChatMessage::Error(format!("failed to load session: {}", e)));
                                                    }
                                                }
                                            }
                                        }
                                        "help" => {
                                            app.push_msg(ChatMessage::System(
                                                "/clear — reset conversation".to_string()
                                            ));
                                            app.push_msg(ChatMessage::System(
                                                "/model [name] — show or set model".to_string()
                                            ));
                                            app.push_msg(ChatMessage::System(
                                                "/system <prompt|show|save> — system prompt".to_string()
                                            ));
                                            app.push_msg(ChatMessage::System(
                                                "/thinking [low|medium|high|xhigh] — thinking budget".to_string()
                                            ));
                                            app.push_msg(ChatMessage::System(
                                                "/sessions — list saved sessions".to_string()
                                            ));
                                            app.push_msg(ChatMessage::System(
                                                "/resume <id> — switch to a different session".to_string()
                                            ));
                                            app.push_msg(ChatMessage::System(
                                                "/help — show this".to_string()
                                            ));
                                            app.push_msg(ChatMessage::System(
                                                "/theme — list available themes".to_string()
                                            ));
                                            app.push_msg(ChatMessage::System(
                                                "/gamba — open the casino 🎰".to_string()
                                            ));
                                        }
                                        "quit" | "exit" => {
                                            exit_fx = Some(quit_effect());
                                        }
                                        "theme" => {
                                            let descriptions: &[(&str, &str)] = &[
                                                ("default",        "cool teal on dark blue-gray"),
                                                ("neon-rain",      "cyberpunk hot pink + cyan"),
                                                ("amber",          "warm CRT retro terminal"),
                                                ("phosphor",       "green monochrome CRT"),
                                                ("solarized-dark", "Ethan Schoonover's classic"),
                                                ("blood",          "dark red, Doom/horror"),
                                                ("ocean",          "deep sea bioluminescence"),
                                                ("rose-pine",      "elegant muted purples/pinks"),
                                                ("nord",           "arctic frost blues"),
                                                ("dracula",        "purple/pink/cyan vibrant"),
                                                ("monokai",        "classic orange/pink/green"),
                                                ("gruvbox",        "warm earthy tones"),
                                                ("catppuccin",     "soft pastels, cozy dark"),
                                                ("tokyo-night",    "dark blue-purple, soft accents"),
                                                ("sunset",         "warm oranges/pinks dusk"),
                                                ("ice",            "frozen arctic pale blues"),
                                                ("forest",         "deep greens and browns"),
                                                ("lavender",       "rich purple/violet"),
                                            ];
                                            app.push_msg(ChatMessage::System(
                                                "Available themes:".to_string()
                                            ));
                                            for (name, desc) in descriptions {
                                                app.push_msg(ChatMessage::System(
                                                    format!("  {:<15} — {}", name, desc)
                                                ));
                                            }
                                            // Also list any custom user themes
                                            let themes_dir = synaps_cli::config::base_dir().join("themes");
                                            if let Ok(entries) = std::fs::read_dir(&themes_dir) {
                                                let mut custom: Vec<String> = entries
                                                    .filter_map(|e| e.ok())
                                                    .map(|e| e.file_name().to_string_lossy().to_string())
                                                    .filter(|n| !descriptions.iter().any(|(d, _)| *d == n.as_str()))
                                                    .collect();
                                                custom.sort();
                                                for name in &custom {
                                                    app.push_msg(ChatMessage::System(
                                                        format!("  {:<15} — custom", name)
                                                    ));
                                                }
                                            }
                                            app.push_msg(ChatMessage::System(String::new()));
                                            app.push_msg(ChatMessage::System(
                                                "Set in ~/.synaps-cli/config: theme = <name>".to_string()
                                            ));
                                            app.push_msg(ChatMessage::System(
                                                "Edit themes in ~/.synaps-cli/themes/ — no rebuild needed.".to_string()
                                            ));
                                            app.push_msg(ChatMessage::System(
                                                "Restart to apply.".to_string()
                                            ));
                                        }
                                        "gamba" => {
                                            // Drop event reader so crossterm stops consuming stdin
                                            drop(event_reader);
                                            match app.launch_gamba() {
                                                Ok(()) => {} // casino running, terminal yielded
                                                Err(msg) => {
                                                    terminal.clear().ok();
                                                    app.push_msg(ChatMessage::Error(msg));
                                                }
                                            }
                                            // Recreate event reader (casino may still be running — that's ok,
                                            // the select! guard prevents polling until gamba exits)
                                            event_reader = EventStream::new();
                                        }
                                        _ => {
                                            app.push_msg(ChatMessage::Error(
                                                format!("unknown command: /{}", cmd)
                                            ));
                                        }
                                    }
                                } else {
                                    // Display: typed text + paste indicator
                                    let display_text = if app.pasted_char_count > 0 {
                                        let typed = app.input_before_paste.as_deref().unwrap_or("");
                                        // Use char boundary from char count, not byte len
                                        let typed_char_count = typed.chars().count();
                                        let pasted_char_count = input.chars().count().saturating_sub(typed_char_count);
                                        let paste_byte_start = input.char_indices()
                                            .nth(typed_char_count)
                                            .map(|(i, _)| i)
                                            .unwrap_or(input.len());
                                        let paste_content = &input[paste_byte_start..];
                                        let line_count = paste_content.lines().count();
                                        let paste_label = if line_count > 1 {
                                            format!("[Pasted {} lines]", line_count)
                                        } else {
                                            format!("[Pasted {} chars]", pasted_char_count)
                                        };
                                        if typed.is_empty() {
                                            paste_label
                                        } else {
                                            format!("{} {}", typed.trim(), paste_label)
                                        }
                                    } else {
                                        input.clone()
                                    };
                                    app.push_msg(ChatMessage::User(display_text));
                                    app.input_before_paste = None;
                                    app.pasted_char_count = 0;
                                    // Inject abort context if previous response was interrupted
                                    let api_content = if let Some(ref ctx) = app.abort_context {
                                        let combined = format!("{}\n\n{}", ctx, input);
                                        app.abort_context = None;
                                        combined
                                    } else {
                                        input
                                    };
                                    app.api_messages.push(json!({"role": "user", "content": api_content}));
                                    let ct = CancellationToken::new();
                                    let (s_tx, s_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                                    // Show auth status in header during token refresh
                                    app.status_text = Some("connecting…".to_string());
                                    app.streaming = true;  // Start spinner immediately
                                    app.spinner_frame = 0;
                                    let elapsed = last_frame.elapsed();
                                    last_frame = Instant::now();
                                    draw(&mut terminal, &mut app, runtime.model(), runtime.thinking_level(), &mut boot_fx, &mut exit_fx, elapsed).unwrap();
                                    stream = Some(runtime.run_stream_with_messages(app.api_messages.clone(), ct.clone(), Some(s_rx)).await);
                                    app.status_text = None;
                                    // Show thinking placeholder until first real token arrives
                                    app.push_msg(ChatMessage::Thinking("…".to_string()));
                                    cancel_token = Some(ct);
                                    steer_tx = Some(s_tx);
                                }
                            }
                            (KeyCode::Enter, _) if app.streaming && !app.input.is_empty() => {
                                let input = app.input.clone();
                                app.input_history.push(input.clone());
                                app.history_index = None;
                                app.input_stash.clear();
                                app.input.clear();
                                app.cursor_pos = 0;
                                app.input_before_paste = None;
                                app.pasted_char_count = 0;

                                // Intercept slash commands during streaming
                                if input.starts_with('/') {
                                    let raw_cmd = input[1..].split_whitespace().next().unwrap_or("");
                                    // Use same prefix resolution as non-streaming path
                                    let streaming_cmds = ["gamba", "quit", "exit"];
                                    let cmd = if streaming_cmds.contains(&raw_cmd) {
                                        raw_cmd.to_string()
                                    } else {
                                        let matches: Vec<&&str> = streaming_cmds.iter().filter(|c| c.starts_with(raw_cmd)).collect();
                                        if matches.len() == 1 { matches[0].to_string() } else { raw_cmd.to_string() }
                                    };
                                    match cmd.as_str() {
                                        "gamba" => {
                                            drop(event_reader);
                                            match app.launch_gamba() {
                                                Ok(()) => {}
                                                Err(msg) => {
                                                    terminal.clear().ok();
                                                    app.push_msg(ChatMessage::Error(msg));
                                                }
                                            }
                                            event_reader = EventStream::new();
                                        }
                                        "quit" | "exit" => {
                                            exit_fx = Some(quit_effect());
                                        }
                                        _ => {
                                            // Unknown slash — steer/queue as normal
                                            let steered = steer_tx.as_ref()
                                                .map(|tx| tx.send(input.clone()).is_ok())
                                                .unwrap_or(false);
                                            if steered {
                                                app.push_msg(ChatMessage::System(format!("→ steering: {}", input)));
                                            } else {
                                                app.push_msg(ChatMessage::System(format!("queued: {}", input)));
                                            }
                                            app.queued_message = Some(input);
                                        }
                                    }
                                } else {
                                    // Normal text — steer/queue
                                    let steered = steer_tx.as_ref()
                                        .map(|tx| tx.send(input.clone()).is_ok())
                                        .unwrap_or(false);
                                    if steered {
                                        app.push_msg(ChatMessage::System(format!("→ steering: {}", input)));
                                    } else {
                                        app.push_msg(ChatMessage::System(format!("queued: {}", input)));
                                    }
                                    app.queued_message = Some(input);
                                }
                            }
                            (KeyCode::Tab, _) if app.input.starts_with('/') && app.input.len() > 1 => {
                                let partial = &app.input[1..];
                                let commands = ["clear", "model", "system", "thinking", "sessions", "resume", "theme", "gamba", "help", "quit", "exit"];
                                let matches: Vec<&&str> = commands.iter()
                                    .filter(|c| c.starts_with(partial))
                                    .collect();
                                if matches.len() == 1 {
                                    app.input = format!("/{}", matches[0]);
                                    app.cursor_pos = app.input.chars().count();
                                } else if matches.len() > 1 {
                                    // Find common prefix
                                    let first = matches[0];
                                    let common_len = (0..first.len())
                                        .take_while(|&i| matches.iter().all(|m| m.as_bytes().get(i) == first.as_bytes().get(i)))
                                        .count();
                                    if common_len > partial.len() {
                                        app.input = format!("/{}", &first[..common_len]);
                                        app.cursor_pos = app.input.chars().count();
                                    }
                                }
                            }
                            (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
                                app.cursor_pos = 0;
                            }
                            (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
                                app.cursor_pos = app.input.chars().count();
                            }
                            (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                                // Delete word backward (same as Alt+Backspace)
                                let chars: Vec<char> = app.input.chars().collect();
                                let mut pos = app.cursor_pos;
                                while pos > 0 && chars[pos - 1] == ' ' { pos -= 1; }
                                while pos > 0 && chars[pos - 1] != ' ' { pos -= 1; }
                                let byte_start = app.input.char_indices().nth(pos).map(|(i, _)| i).unwrap_or(app.input.len());
                                let byte_end = app.cursor_byte_pos();
                                app.input.drain(byte_start..byte_end);
                                app.cursor_pos = pos;
                            }
                            (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                                // Clear input line
                                app.input.clear();
                                app.cursor_pos = 0;
                            }
                            (KeyCode::Home, _) => {
                                app.cursor_pos = 0;
                            }
                            (KeyCode::End, _) => {
                                app.cursor_pos = app.input.chars().count();
                            }
                            (KeyCode::Left, KeyModifiers::ALT) => {
                                // Jump word left
                                let chars: Vec<char> = app.input.chars().collect();
                                let mut pos = app.cursor_pos;
                                while pos > 0 && chars[pos - 1] == ' ' { pos -= 1; }
                                while pos > 0 && chars[pos - 1] != ' ' { pos -= 1; }
                                app.cursor_pos = pos;
                            }
                            (KeyCode::Right, KeyModifiers::ALT) => {
                                // Jump word right
                                let chars: Vec<char> = app.input.chars().collect();
                                let len = chars.len();
                                let mut pos = app.cursor_pos;
                                while pos < len && chars[pos] != ' ' { pos += 1; }
                                while pos < len && chars[pos] == ' ' { pos += 1; }
                                app.cursor_pos = pos;
                            }
                            (KeyCode::Backspace, KeyModifiers::ALT) => {
                                // Delete word backward
                                let chars: Vec<char> = app.input.chars().collect();
                                let mut pos = app.cursor_pos;
                                while pos > 0 && chars[pos - 1] == ' ' { pos -= 1; }
                                while pos > 0 && chars[pos - 1] != ' ' { pos -= 1; }
                                let byte_start = app.input.char_indices().nth(pos).map(|(i, _)| i).unwrap_or(app.input.len());
                                let byte_end = app.cursor_byte_pos();
                                app.input.drain(byte_start..byte_end);
                                app.cursor_pos = pos;
                            }
                            (KeyCode::Char('o'), KeyModifiers::CONTROL) => {
                                app.show_full_output = !app.show_full_output;
                                app.dirty = true;
                                app.line_cache.clear();
                            }
                            (KeyCode::Char(c), _) => {
                                let byte_pos = app.cursor_byte_pos();
                                app.input.insert(byte_pos, c);
                                app.cursor_pos += 1;
                            }
                            (KeyCode::Backspace, _) if app.cursor_pos > 0 => {
                                app.cursor_pos -= 1;
                                let byte_pos = app.cursor_byte_pos();
                                app.input.remove(byte_pos);
                            }
                            (KeyCode::Left, _) if app.cursor_pos > 0 => {
                                app.cursor_pos -= 1;
                            }
                            (KeyCode::Right, _) if app.cursor_pos < app.input_char_count() => {
                                app.cursor_pos += 1;
                            }
                            (KeyCode::Up, KeyModifiers::SHIFT) => {
                                app.scroll_back = app.scroll_back.saturating_add(1);
                                app.scroll_pinned = false;
                            }
                            (KeyCode::Down, KeyModifiers::SHIFT) => {
                                app.scroll_back = app.scroll_back.saturating_sub(1);
                                if app.scroll_back == 0 {
                                    app.scroll_pinned = true;
                                }
                            }
                            (KeyCode::Up, _) => {
                                app.history_up();
                            }
                            (KeyCode::Down, _) => {
                                app.history_down();
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(Event::Mouse(mouse))) => {
                        match mouse.kind {
                            MouseEventKind::ScrollUp => {
                                app.scroll_back = app.scroll_back.saturating_add(3);
                                app.scroll_pinned = false;
                            }
                            MouseEventKind::ScrollDown => {
                                app.scroll_back = app.scroll_back.saturating_sub(3);
                                if app.scroll_back == 0 {
                                    app.scroll_pinned = true;
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(Event::Paste(text))) => {
                        // Bracketed paste — insert the full text (newlines included)
                        // into the input buffer at cursor position
                        const MAX_PASTE_CHARS: usize = 100_000;
                        if !app.streaming || !app.input.is_empty() {
                            // Cap paste size to prevent OOM
                            let text = if text.chars().count() > MAX_PASTE_CHARS {
                                let truncated: String = text.chars().take(MAX_PASTE_CHARS).collect();
                                app.push_msg(ChatMessage::System(
                                    format!("Paste truncated to {} chars (was {})", MAX_PASTE_CHARS, text.chars().count())
                                ));
                                truncated
                            } else {
                                text
                            };
                            // Snapshot input before first paste so we can show typed text separately
                            if app.input_before_paste.is_none() {
                                app.input_before_paste = Some(app.input.clone());
                            }
                            let byte_pos = app.cursor_byte_pos();
                            app.input.insert_str(byte_pos, &text);
                            app.cursor_pos += text.chars().count();
                            app.pasted_char_count += text.chars().count();
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }

            maybe_event = async {
                if let Some(ref mut s) = stream {
                    s.next().await
                } else {
                    std::future::pending().await
                }
            } => {
                if let Some(event) = maybe_event {
                    // Only redraw immediately for structural events (tool calls,
                    // completion, errors). Text/thinking tokens are batched and
                    // rendered by the 16ms tick to avoid hundreds of redraws/sec.
                    let needs_immediate_draw = matches!(&event,
                        StreamEvent::ToolUse { .. }
                        | StreamEvent::ToolResult { .. }
                        | StreamEvent::SubagentStart { .. }
                        | StreamEvent::SubagentUpdate { .. }
                        | StreamEvent::SubagentDone { .. }
                        | StreamEvent::SteeringDelivered { .. }
                        | StreamEvent::Done
                        | StreamEvent::Error(_)
                    );

                    match event {
                        StreamEvent::Thinking(text) => {
                            app.append_or_update_thinking(&text);
                        }
                        StreamEvent::Text(text) => {
                            app.append_or_update_text(&text);
                        }
                        StreamEvent::ToolUseStart(name) => {
                            app.tool_start_time = Some(std::time::Instant::now());
                            app.push_msg(ChatMessage::ToolUseStart(name, String::new()));
                        }
                        StreamEvent::ToolUseDelta(delta) => {
                            if let Some(last) = app.messages.last_mut() {
                                if let ChatMessage::ToolUseStart(_, ref mut partial) = last.msg {
                                    partial.push_str(&delta);
                                    app.dirty = true;
                                    app.line_cache.clear();
                                    continue;
                                }
                            }
                        }
                        StreamEvent::ToolUse { tool_name, input, .. } => {
                            app.tool_start_time = Some(std::time::Instant::now());
                            let input_str = serde_json::to_string(&input).unwrap_or_default();
                            if let Some(last) = app.messages.last_mut() {
                                if let ChatMessage::ToolUseStart(name, _) = &last.msg {
                                    if name == &tool_name {
                                        last.msg = ChatMessage::ToolUse { tool_name, input: input_str };
                                        app.dirty = true;
                                        app.line_cache.clear();
                                        continue;
                                    }
                                }
                            }
                            app.push_msg(ChatMessage::ToolUse { tool_name, input: input_str });
                        }
                        StreamEvent::ToolResultDelta { delta, .. } => {
                            if let Some(last) = app.messages.last_mut() {
                                if let ChatMessage::ToolResult { ref mut content, .. } = last.msg {
                                    content.push_str(&delta);
                                    app.dirty = true;
                                    app.line_cache.clear();
                                    continue;
                                }
                            }
                            app.push_msg(ChatMessage::ToolResult { content: delta, elapsed_ms: None });
                        }
                        StreamEvent::ToolResult { result, .. } => {
                            let elapsed = app.tool_start_time.take()
                                .map(|t| t.elapsed().as_millis() as u64);
                            if let Some(last) = app.messages.last_mut() {
                                if let ChatMessage::ToolResult { ref mut content, elapsed_ms: ref mut el, .. } = last.msg {
                                    *content = result;
                                    *el = elapsed;
                                    app.dirty = true;
                                    app.line_cache.clear();
                                    continue;
                                }
                            }
                            app.push_msg(ChatMessage::ToolResult { content: result, elapsed_ms: elapsed });
                        }
                        StreamEvent::MessageHistory(history) => {
                            app.api_messages = history;
                            app.save_session();
                        }
                        StreamEvent::SubagentStart { agent_name, task_preview } => {
                            let id = app.next_subagent_id;
                            app.next_subagent_id += 1;
                            app.subagents.push(SubagentState {
                                id,
                                name: agent_name,
                                status: format!("starting: {}", task_preview),
                                start_time: std::time::Instant::now(),
                                done: false,
                                duration_secs: None,
                            });
                            app.dirty = true;
                            app.line_cache.clear();
                        }
                        StreamEvent::SubagentUpdate { agent_name, status } => {
                            // Find the last (most recent) non-done agent with this name
                            if let Some(sa) = app.subagents.iter_mut().rev().find(|s| s.name == agent_name && !s.done) {
                                sa.status = status;
                            }
                            app.dirty = true;
                            app.line_cache.clear();
                        }
                        StreamEvent::SubagentDone { agent_name, result_preview, duration_secs } => {
                            if let Some(sa) = app.subagents.iter_mut().rev().find(|s| s.name == agent_name && !s.done) {
                                sa.done = true;
                                sa.duration_secs = Some(duration_secs);
                                let preview: String = result_preview.chars().take(40).collect();
                                sa.status = format!("\u{2714} {}", preview);
                            }
                            app.dirty = true;
                            app.line_cache.clear();
                        }
                        StreamEvent::SteeringDelivered { message } => {
                            app.push_msg(ChatMessage::User(message.clone()));
                            // Steering delivered — clear queue so Done doesn't double-send
                            if app.queued_message.as_ref() == Some(&message) {
                                app.queued_message = None;
                            }
                            app.scroll_back = 0;
                            app.scroll_pinned = true;
                            app.dirty = true;
                            app.line_cache.clear();
                        }
                        StreamEvent::Usage {
                            input_tokens,
                            output_tokens,
                            cache_read_input_tokens,
                            cache_creation_input_tokens,
                            model: usage_model,
                        } => {
                            let model_for_pricing = usage_model.as_deref().unwrap_or(runtime.model());
                            app.add_usage(
                                input_tokens,
                                output_tokens,
                                cache_read_input_tokens,
                                cache_creation_input_tokens,
                                model_for_pricing,
                            );
                        }
                        StreamEvent::Done => {
                            app.streaming = false;
                            // Clear completed subagents panel
                            app.subagents.clear();
                            stream = None;
                            cancel_token = None;
                            steer_tx = None;

                            // Reclaim terminal from casino if running
                            if let Some(msg) = app.reclaim_gamba() {
                                terminal.clear().ok();
                                app.push_msg(ChatMessage::System(msg));
                                app.dirty = true;
                                app.line_cache.clear();
                                let elapsed = last_frame.elapsed();
                                last_frame = Instant::now();
                                draw(&mut terminal, &mut app, runtime.model(), runtime.thinking_level(), &mut boot_fx, &mut exit_fx, elapsed).unwrap();
                            }

                            // Auto-send queued message if one was typed during streaming
                            if let Some(queued) = app.queued_message.take() {
                                app.push_msg(ChatMessage::User(queued.clone()));
                                app.scroll_back = 0;
                                app.scroll_pinned = true;

                                let api_content = if let Some(ref ctx) = app.abort_context {
                                    let combined = format!("{}\n\n{}", ctx, queued);
                                    app.abort_context = None;
                                    combined
                                } else {
                                    queued
                                };
                                app.api_messages.push(json!({"role": "user", "content": api_content}));
                                let ct = CancellationToken::new();
                                let (s_tx, s_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                                // Show auth status in header during token refresh
                                app.status_text = Some("connecting…".to_string());
                                app.streaming = true;  // Start spinner immediately
                                app.spinner_frame = 0;
                                let elapsed = last_frame.elapsed();
                                last_frame = Instant::now();
                                draw(&mut terminal, &mut app, runtime.model(), runtime.thinking_level(), &mut boot_fx, &mut exit_fx, elapsed).unwrap();
                                stream = Some(runtime.run_stream_with_messages(app.api_messages.clone(), ct.clone(), Some(s_rx)).await);
                                app.status_text = None;
                                // Show thinking placeholder until first real token arrives
                                app.push_msg(ChatMessage::Thinking("…".to_string()));
                                cancel_token = Some(ct);
                                steer_tx = Some(s_tx);
                            }
                        }
                        StreamEvent::Error(err) => {
                            app.push_msg(ChatMessage::Error(err));
                            app.streaming = false;
                            app.subagents.clear();
                            stream = None;
                            cancel_token = None;
                            steer_tx = None;
                            // Reclaim terminal from casino if running
                            if let Some(msg) = app.reclaim_gamba() {
                                terminal.clear().ok();
                                app.push_msg(ChatMessage::System(msg));
                                app.dirty = true;
                                app.line_cache.clear();
                                let elapsed = last_frame.elapsed();
                                last_frame = Instant::now();
                                draw(&mut terminal, &mut app, runtime.model(), runtime.thinking_level(), &mut boot_fx, &mut exit_fx, elapsed).unwrap();
                            }
                            // Restore a valid trailing state. The runtime guarantees that
                            // each tool_use has a matching tool_result, so we only need to
                            // drop an unmatched trailing assistant message or a trailing
                            // plain-text user message (so the user can retry cleanly).
                            // We must NOT pop a trailing user tool_result message, because
                            // that would orphan the preceding assistant tool_use blocks.
                            if let Some(last) = app.api_messages.last() {
                                let role = last["role"].as_str().unwrap_or("");
                                let is_text_user = role == "user" && last["content"].is_string();
                                let is_assistant = role == "assistant";
                                if is_text_user || is_assistant {
                                    app.api_messages.pop();
                                }
                            }
                        }
                    }
                    if needs_immediate_draw {
                        let elapsed = last_frame.elapsed();
                        last_frame = Instant::now();
                        draw(&mut terminal, &mut app, runtime.model(), runtime.thinking_level(), &mut boot_fx, &mut exit_fx, elapsed).unwrap();
                    }
                }
            }
        }
    }

    // Save session on exit
    app.save_session();

    disable_raw_mode().unwrap();
    execute!(terminal.backend_mut(), DisableBracketedPaste, DisableMouseCapture, LeaveAlternateScreen).unwrap();
    terminal.show_cursor().unwrap();

    Ok(())
}
