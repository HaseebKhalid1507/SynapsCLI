use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use serde_json::Value;
use chrono::Local;
use synaps_cli::Session;

use super::theme::THEME;
use super::highlight::{highlight_tool_code, highlight_bash_output, highlight_read_output, try_highlight_grep_line, is_read_tool_output};
use super::markdown::{render_markdown, wrap_text};
use super::draw::{bash_trace, format_tool_name};

#[derive(Clone)]
pub(crate) enum ChatMessage {
    User(String),
    Thinking(String),
    Text(String),
    ToolUseStart(String, String),  // (tool_name, partial_input)
    ToolUse { tool_name: String, input: String },
    ToolResult { content: String, elapsed_ms: Option<u64> },
    Error(String),
    System(String),
}

pub(crate) struct TimestampedMsg {
    pub(crate) msg: ChatMessage,
    pub(crate) time: String,
}

pub(crate) struct App {
    pub(crate) messages: Vec<TimestampedMsg>,
    pub(crate) input: String,
    /// Cursor position as a **char index** (not byte index).
    /// Use `cursor_byte_pos()` to convert to byte offset for String operations.
    pub(crate) cursor_pos: usize,
    pub(crate) scroll_back: u16,
    /// When true, viewport stays pinned to the bottom (auto-scroll).
    /// Set to false when user scrolls up, true when they scroll back to bottom.
    pub(crate) scroll_pinned: bool,
    pub(crate) api_messages: Vec<Value>,
    pub(crate) streaming: bool,
    pub(crate) input_history: Vec<String>,
    pub(crate) history_index: Option<usize>,
    pub(crate) input_stash: String,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) total_input_tokens: u64,
    pub(crate) total_output_tokens: u64,
    pub(crate) total_cache_read_tokens: u64,
    pub(crate) total_cache_creation_tokens: u64,
    pub(crate) session_cost: f64,
    pub(crate) session: Session,
    pub(crate) line_cache: Vec<Line<'static>>,
    pub(crate) cache_width: usize,
    pub(crate) dirty: bool,
    pub(crate) show_full_output: bool,
    pub(crate) logo_dismiss_t: Option<f64>,
    pub(crate) logo_build_t: Option<f64>,
    /// Previous rendered line count — used to stabilize scroll when not pinned
    pub(crate) last_line_count: usize,
    /// Active subagent status for the live panel
    pub(crate) subagents: Vec<SubagentState>,
    /// Counter for unique subagent IDs within a session
    pub(crate) next_subagent_id: u32,
    /// Tracks when the current tool started executing (for elapsed time display)
    pub(crate) tool_start_time: Option<std::time::Instant>,
    /// Saved context from an aborted response — injected into the next user message
    pub(crate) abort_context: Option<String>,
    /// Message queued while streaming — auto-sent when current response finishes
    pub(crate) queued_message: Option<String>,
    /// Tracks paste state: snapshot of input before first paste, and total pasted char count
    pub(crate) input_before_paste: Option<String>,
    pub(crate) pasted_char_count: usize,
    /// Spinner frame counter (incremented on tick)
    pub(crate) spinner_frame: usize,
    /// Transient status text shown in the header bar (auto-cleared when streaming starts)
    pub(crate) status_text: Option<String>,
    /// GamblersDen child process — spawned by /gamba, killed when streaming finishes
    pub(crate) gamba_child: Option<std::process::Child>,
}

pub(crate) const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];


#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct SubagentState {
    pub(crate) id: u32,
    pub(crate) name: String,
    pub(crate) status: String,
    pub(crate) start_time: std::time::Instant,
    pub(crate) done: bool,
    pub(crate) duration_secs: Option<f64>,
}

impl App {
    pub(crate) fn new(session: Session) -> Self {
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
    pub(crate) fn restore_terminal(&self) {
        crossterm::terminal::enable_raw_mode().ok();
        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::EnterAlternateScreen
        ).ok();
    }

    /// Yield terminal to casino — tears down TUI, spawns GamblersDen.
    /// Returns Ok(()) if launched, Err(msg) if failed.
    pub(crate) fn launch_gamba(&mut self) -> std::result::Result<(), String> {
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
    pub(crate) fn reclaim_gamba(&mut self) -> Option<String> {
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
    pub(crate) fn check_gamba_exited(&mut self) -> Option<String> {
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
    pub(crate) fn cursor_byte_pos(&self) -> usize {
        self.input.char_indices()
            .nth(self.cursor_pos)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }

    /// Number of chars in self.input (for bounds checking cursor_pos).
    pub(crate) fn input_char_count(&self) -> usize {
        self.input.chars().count()
    }

    /// Calculate the number of visual lines the input needs, given an inner width.
    /// Returns (total_lines, cursor_row, cursor_col) for layout and cursor placement.
    pub(crate) fn input_wrap_info(&self, inner_width: u16) -> (u16, u16, u16) {
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

    pub(crate) fn save_session(&mut self) {
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

    pub(crate) fn add_usage(
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

    pub(crate) fn push_msg(&mut self, msg: ChatMessage) {
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
    pub(crate) fn find_preceding_read_extension(&self, idx: usize) -> String {
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
    pub(crate) fn find_preceding_tool_name(&self, idx: usize) -> Option<String> {
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
    pub(crate) fn capture_abort_context(&mut self) {
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

    pub(crate) fn history_up(&mut self) {
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

    pub(crate) fn history_down(&mut self) {
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

    pub(crate) fn append_or_update_text(&mut self, text: &str) {
        if let Some(TimestampedMsg { msg: ChatMessage::Text(ref mut existing), .. }) = self.messages.last_mut() {
            existing.push_str(text);
        } else {
            self.push_msg(ChatMessage::Text(text.to_string()));
        }
        self.dirty = true;
    }

    pub(crate) fn append_or_update_thinking(&mut self, text: &str) {
        if let Some(TimestampedMsg { msg: ChatMessage::Thinking(ref mut existing), .. }) = self.messages.last_mut() {
            existing.push_str(text);
        } else {
            self.push_msg(ChatMessage::Thinking(text.to_string()));
        }
        self.dirty = true;
    }

    pub(crate) fn render_lines(&self, width: usize) -> Vec<Line<'static>> {
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
                    let is_timeout = result.contains("[TIMED OUT");
                    let style = if is_error {
                        Style::default().fg(THEME.error_color)
                    } else if is_timeout {
                        Style::default().fg(THEME.warning_color)
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
                        if is_timeout {
                            let elapsed_str = match elapsed_ms {
                                Some(ms) if *ms >= 1000 => format!(" {:.1}s", *ms as f64 / 1000.0),
                                Some(ms) => format!(" {}ms", ms),
                                None => String::new(),
                            };
                            lines.push(Line::from(vec![
                                Span::styled(
                                    format!("{}     \u{2514}\u{2500} \u{26a0} timed out ({} lines)", m, result_lines.len()),
                                    Style::default().fg(THEME.warning_color),
                                ),
                                Span::styled(
                                    elapsed_str,
                                    Style::default().fg(THEME.subagent_time),
                                ),
                            ]));
                        } else if elapsed_ms.is_none() && self.tool_start_time.is_some() {
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
                    // Skip fancy highlighting for timeouts — render everything in warning style
                    let highlighted_lines = if is_timeout || is_error {
                        None
                    } else if is_read_tool_output(&result_lines) {
                        let ext = self.find_preceding_read_extension(i);
                        highlight_read_output(&result_lines[..show], &ext, &m)
                    } else if preceding_tool.as_deref() == Some("bash") {
                        Some(highlight_bash_output(&result_lines[..show], &m))
                    } else {
                        None
                    };

                    if let Some(hl_lines) = highlighted_lines {
                        lines.extend(hl_lines);
                    } else {
                        for line in &result_lines[..show] {
                            // Try to detect and highlight grep output (skip for timeout/error)
                            if !is_timeout && !is_error {
                                if let Some(grep_spans) = try_highlight_grep_line(line, &m) {
                                    lines.push(Line::from(grep_spans));
                                    continue;
                                }
                            }
                            let full = format!("{}       {}", m, line);
                            for wline in wrap_text(&full, width) {
                                lines.push(Line::from(Span::styled(wline, style)));
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
