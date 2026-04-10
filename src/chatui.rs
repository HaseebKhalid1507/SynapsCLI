use agent_runtime::{Runtime, StreamEvent, Result, CancellationToken, Session, list_sessions, latest_session, find_session};
use clap::Parser;
use crossterm::{
    event::{Event, KeyCode, KeyModifiers, MouseEventKind, EnableMouseCapture, DisableMouseCapture, EventStream},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Alignment},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, BorderType, Paragraph, Padding},
    Terminal,
};
use serde_json::{json, Value};
use std::io;
use std::time::Instant;
use chrono::Local;
use tachyonfx::{fx, Effect, Interpolation, Shader};
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

// -- Theme -------------------------------------------------------------------

// Markdown
const CODE_FG: Color = Color::Rgb(200, 160, 100);
const CODE_BG: Color = Color::Rgb(30, 30, 40);
const HEADING_COLOR: Color = Color::Rgb(160, 190, 240);
const QUOTE_COLOR: Color = Color::Rgb(130, 130, 160);
const LIST_BULLET_COLOR: Color = Color::Rgb(120, 210, 160);

// Base
const BG: Color = Color::Rgb(18, 18, 24);
const BORDER: Color = Color::Rgb(58, 58, 78);
const BORDER_ACTIVE: Color = Color::Rgb(110, 140, 200);
const MUTED: Color = Color::Rgb(90, 90, 110);

// Messages
const USER_COLOR: Color = Color::Rgb(130, 170, 255);
const USER_BG: Color = Color::Rgb(30, 35, 50);
const CLAUDE_LABEL: Color = Color::Rgb(120, 210, 160);
const CLAUDE_TEXT: Color = Color::Rgb(210, 210, 220);
const THINKING_COLOR: Color = Color::Rgb(90, 90, 110);
const TOOL_LABEL: Color = Color::Rgb(220, 180, 90);
const TOOL_PARAM: Color = Color::Rgb(170, 155, 110);
const TOOL_RESULT_COLOR: Color = Color::Rgb(100, 140, 100);
const ERROR_COLOR: Color = Color::Rgb(230, 90, 90);

// UI
const HEADER_FG: Color = Color::Rgb(160, 160, 180);
const STATUS_STREAMING: Color = Color::Rgb(220, 180, 90);
const STATUS_READY: Color = Color::Rgb(100, 180, 120);
const HELP_FG: Color = Color::Rgb(70, 70, 90);
const INPUT_FG: Color = Color::Rgb(200, 200, 210);
const PROMPT_FG: Color = Color::Rgb(110, 140, 200);

// -- Data --------------------------------------------------------------------

#[derive(Clone)]
enum ChatMessage {
    User(String),
    Thinking(String),
    Text(String),
    ToolUse { tool_name: String, input: String },
    ToolResult(String),
    Error(String),
    System(String),
}

struct TimestampedMsg {
    msg: ChatMessage,
    time: String,
}

const COST_COLOR: Color = Color::Rgb(180, 140, 200);

struct App {
    messages: Vec<TimestampedMsg>,
    input: String,
    cursor_pos: usize,
    scroll_back: u16,
    api_messages: Vec<Value>,
    streaming: bool,
    input_history: Vec<String>,
    history_index: Option<usize>,
    input_stash: String,
    input_tokens: u64,
    output_tokens: u64,
    total_input_tokens: u64,
    total_output_tokens: u64,
    session_cost: f64,
    session: Session,
}

impl App {
    fn new(session: Session) -> Self {
        Self {
            messages: Vec::new(),
            input: String::new(),
            cursor_pos: 0,
            scroll_back: 0,
            api_messages: Vec::new(),
            streaming: false,
            input_history: Vec::new(),
            history_index: None,
            input_stash: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            session_cost: 0.0,
            session,
        }
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
        let _ = self.session.save();
    }

    fn add_usage(&mut self, input_tokens: u64, output_tokens: u64, model: &str) {
        self.input_tokens = input_tokens;
        self.output_tokens = output_tokens;
        self.total_input_tokens += input_tokens;
        self.total_output_tokens += output_tokens;
        // Pricing per million tokens (as of 2025)
        let (input_price, output_price) = match model {
            m if m.contains("opus") => (15.0, 75.0),
            m if m.contains("sonnet") => (3.0, 15.0),
            m if m.contains("haiku") => (0.80, 4.0),
            _ => (3.0, 15.0), // default to sonnet pricing
        };
        let cost = (input_tokens as f64 / 1_000_000.0) * input_price
                 + (output_tokens as f64 / 1_000_000.0) * output_price;
        self.session_cost += cost;
    }

    fn push_msg(&mut self, msg: ChatMessage) {
        self.messages.push(TimestampedMsg {
            msg,
            time: Local::now().format("%H:%M").to_string(),
        });
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
        self.cursor_pos = self.input.len();
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
                self.cursor_pos = self.input.len();
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
    }

    fn append_or_update_thinking(&mut self, text: &str) {
        if let Some(TimestampedMsg { msg: ChatMessage::Thinking(ref mut existing), .. }) = self.messages.last_mut() {
            existing.push_str(text);
        } else {
            self.push_msg(ChatMessage::Thinking(text.to_string()));
        }
    }

    fn render_lines(&self, width: usize) -> Vec<Line<'_>> {
        let mut lines: Vec<Line> = Vec::new();
        let indent = "  ";
        let w = width.saturating_sub(2); // account for indent

        for tmsg in &self.messages {
            let ts = &tmsg.time;
            match &tmsg.msg {
                ChatMessage::User(text) => {
                    let bg_style = Style::default().bg(USER_BG);
                    // Top padding
                    lines.push(Line::from(Span::styled(format!("{:<width$}", "", width = width), bg_style)));
                    // Label line with timestamp
                    let label = format!("{}  \u{25cf} You", indent);
                    let ts_str = format!(" {} ", ts);
                    let label_chars = label.chars().count();
                    let ts_chars = ts_str.chars().count();
                    let gap = width.saturating_sub(label_chars + ts_chars);
                    let padded = format!("{}{}", label, " ".repeat(gap));
                    lines.push(Line::from(vec![
                        Span::styled(padded, Style::default().fg(USER_COLOR).bg(USER_BG).add_modifier(Modifier::BOLD)),
                        Span::styled(ts_str, Style::default().fg(MUTED).bg(USER_BG)),
                    ]));
                    // Content
                    let style = Style::default().fg(USER_COLOR).bg(USER_BG);
                    for line in text.lines() {
                        for wline in wrap_text(&format!("{}    {}", indent, line), width) {
                            let padded = format!("{:<width$}", wline, width = width);
                            lines.push(Line::from(Span::styled(padded, style)));
                        }
                    }
                    // Bottom padding
                    lines.push(Line::from(Span::styled(format!("{:<width$}", "", width = width), bg_style)));
                }
                ChatMessage::Thinking(text) => {
                    // Thin left border style for thinking
                    let border_style = Style::default().fg(THINKING_COLOR);
                    let text_style = Style::default().fg(THINKING_COLOR).add_modifier(Modifier::ITALIC);
                    lines.push(Line::from(vec![
                        Span::styled(format!("{}  \u{2502} ", indent), border_style),
                        Span::styled("thinking\u{2026}", Style::default().fg(THINKING_COLOR).add_modifier(Modifier::BOLD)),
                    ]));
                    let thinking_lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
                    let show = thinking_lines.len().min(6);
                    for line in &thinking_lines[..show] {
                        for wline in wrap_text(&format!("{}  \u{2502}   {}", indent, line.trim()), width) {
                            lines.push(Line::from(Span::styled(wline, text_style)));
                        }
                    }
                    if thinking_lines.len() > 6 {
                        lines.push(Line::from(Span::styled(
                            format!("{}  \u{2502}   [{} more lines]", indent, thinking_lines.len() - 6),
                            border_style,
                        )));
                    }
                    lines.push(Line::from(Span::styled(
                        format!("{}  \u{2502}", indent),
                        border_style,
                    )));
                }
                ChatMessage::Text(text) => {
                    let agent_label = format!("{}\u{25cf} Agent", indent);
                    let ts_str = format!(" {} ", ts);
                    let label_chars = agent_label.chars().count();
                    let ts_chars = ts_str.chars().count();
                    let gap = width.saturating_sub(label_chars + ts_chars);
                    let padded = format!("{}{}", agent_label, " ".repeat(gap));
                    lines.push(Line::from(vec![
                        Span::styled(padded, Style::default().fg(CLAUDE_LABEL).add_modifier(Modifier::BOLD)),
                        Span::styled(ts_str, Style::default().fg(MUTED)),
                    ]));
                    if text.is_empty() {
                        lines.push(Line::from(Span::styled(
                            format!("{}  ...", indent),
                            Style::default().fg(MUTED),
                        )));
                    } else {
                        lines.extend(render_markdown(text, indent, width));
                    }
                }
                ChatMessage::ToolUse { tool_name, input } => {
                    lines.push(Line::from(""));
                    // Tool icon based on type
                    let icon = match tool_name.as_str() {
                        "bash" => "\u{276f}",    // ❯
                        "read" => "\u{25b7}",    // ▷
                        "write" => "\u{25c1}",   // ◁
                        "edit" => "\u{0394}",    // Δ
                        "grep" => "\u{2315}",    // ⌕
                        "find" => "\u{2302}",    // ⌂
                        "ls" => "\u{2261}",      // ≡
                        _ => "\u{2192}",         // →
                    };
                    lines.push(Line::from(Span::styled(
                        format!("{}  {} {}", indent, icon, tool_name),
                        Style::default().fg(TOOL_LABEL).add_modifier(Modifier::BOLD),
                    )));
                    // Parse and show params — compact for simple tools
                    let param_style = Style::default().fg(TOOL_PARAM);
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(input) {
                        if let Some(obj) = parsed.as_object() {
                            for (k, v) in obj {
                                let val = match v.as_str() {
                                    Some(s) => {
                                        // Truncate very long values (e.g. file content in write)
                                        if s.len() > 200 {
                                            let preview: String = s.chars().take(200).collect();
                                            format!("{}...", preview)
                                        } else {
                                            s.to_string()
                                        }
                                    }
                                    None => v.to_string(),
                                };
                                let param = format!("{}    {} \u{2502} {}", indent, k, val);
                                for wline in wrap_text(&param, width) {
                                    lines.push(Line::from(Span::styled(wline, param_style)));
                                }
                            }
                        } else {
                            for wline in wrap_text(&format!("{}    {}", indent, input), w) {
                                lines.push(Line::from(Span::styled(wline, param_style)));
                            }
                        }
                    } else {
                        for wline in wrap_text(&format!("{}    {}", indent, input), w) {
                            lines.push(Line::from(Span::styled(wline, param_style)));
                        }
                    }
                }
                ChatMessage::ToolResult(result) => {
                    // Determine if result looks like a success or failure
                    let is_error = result.starts_with("Tool execution failed")
                        || result.starts_with("Unknown tool");
                    let style = if is_error {
                        Style::default().fg(ERROR_COLOR)
                    } else {
                        Style::default().fg(TOOL_RESULT_COLOR)
                    };

                    // Smart truncation: show more lines for code/file reads, fewer for noise
                    let max_lines = if result.lines().count() > 30 { 15 } else { 12 };
                    let result_lines: Vec<&str> = result.lines().collect();
                    let show = result_lines.len().min(max_lines);

                    for line in &result_lines[..show] {
                        let full = format!("{}    \u{2502} {}", indent, line);
                        for wline in wrap_text(&full, width) {
                            lines.push(Line::from(Span::styled(wline, style)));
                        }
                    }
                    if result_lines.len() > show {
                        lines.push(Line::from(Span::styled(
                            format!("{}    \u{2502} [{} more lines]", indent, result_lines.len() - show),
                            Style::default().fg(MUTED),
                        )));
                    }
                }
                ChatMessage::Error(err) => {
                    lines.push(Line::from(""));
                    let full = format!("{}  \u{2716} {}", indent, err);
                    for wline in wrap_text(&full, width) {
                        lines.push(Line::from(Span::styled(
                            wline,
                            Style::default().fg(ERROR_COLOR),
                        )));
                    }
                }
                ChatMessage::System(msg) => {
                    let full = format!("{}  \u{2022} {}", indent, msg);
                    for wline in wrap_text(&full, width) {
                        lines.push(Line::from(Span::styled(
                            wline,
                            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
                        )));
                    }
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
    let code_style = Style::default().fg(CODE_FG).bg(CODE_BG);

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

/// Highlight a code block using syntect
fn highlight_code_block<'a>(code: &str, lang: &str, prefix: &str) -> Vec<Line<'a>> {
    let ss = SyntaxSet::load_defaults_newlines();
    let ts = ThemeSet::load_defaults();
    let theme = &ts.themes["base16-ocean.dark"];

    let syntax = ss.find_syntax_by_token(lang)
        .unwrap_or_else(|| ss.find_syntax_plain_text());

    let mut h = HighlightLines::new(syntax, theme);
    let mut lines: Vec<Line> = Vec::new();

    for line in LinesWithEndings::from(code) {
        let ranges = h.highlight_line(line, &ss).unwrap_or_default();
        let mut spans: Vec<Span> = Vec::new();
        spans.push(Span::styled(
            format!("{}  \u{2502} ", prefix),
            Style::default().fg(MUTED).bg(CODE_BG),
        ));
        for (style, text) in ranges {
            let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
            let content = text.trim_end_matches('\n').to_string();
            if !content.is_empty() {
                spans.push(Span::styled(content, Style::default().fg(fg).bg(CODE_BG)));
            }
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// Render markdown text into Lines, handling code blocks, headings, lists, quotes
fn render_markdown<'a>(text: &str, prefix: &str, width: usize) -> Vec<Line<'a>> {
    let mut lines: Vec<Line> = Vec::new();
    let base_style = Style::default().fg(CLAUDE_TEXT);
    let mut in_code_block = false;
    let mut code_lang = String::new();
    let mut code_buf = String::new();

    for line in text.lines() {
        let trimmed = line.trim();

        // Code block toggle
        if trimmed.starts_with("```") {
            if !in_code_block {
                in_code_block = true;
                code_lang = trimmed[3..].trim().to_string();
                let label = if code_lang.is_empty() {
                    format!("{}  \u{2500}\u{2500}\u{2500}", prefix)
                } else {
                    format!("{}  \u{2500}\u{2500} {} \u{2500}\u{2500}", prefix, code_lang)
                };
                lines.push(Line::from(Span::styled(label, Style::default().fg(MUTED))));
                code_buf.clear();
            } else {
                // End of code block — highlight and flush
                lines.extend(highlight_code_block(&code_buf, &code_lang, prefix));
                in_code_block = false;
                lines.push(Line::from(Span::styled(
                    format!("{}  \u{2500}\u{2500}\u{2500}", prefix),
                    Style::default().fg(MUTED),
                )));
            }
            continue;
        }

        if in_code_block {
            code_buf.push_str(line);
            code_buf.push('\n');
            continue;
        }

        // Headings
        if trimmed.starts_with('#') {
            let level = trimmed.chars().take_while(|&c| c == '#').count();
            let heading_text = trimmed[level..].trim();
            let full = format!("{}  {}", prefix, heading_text);
            for wline in wrap_text(&full, width) {
                lines.push(Line::from(Span::styled(
                    wline,
                    Style::default().fg(HEADING_COLOR).add_modifier(Modifier::BOLD),
                )));
            }
            continue;
        }

        // Blockquotes
        if trimmed.starts_with('>') {
            let quote_text = trimmed[1..].trim();
            let full = format!("{}  \u{2502} {}", prefix, quote_text);
            for wline in wrap_text(&full, width) {
                lines.push(Line::from(Span::styled(wline, Style::default().fg(QUOTE_COLOR).add_modifier(Modifier::ITALIC))));
            }
            continue;
        }

        // List items
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            let item_text = &trimmed[2..];
            let bullet_span = Span::styled(format!("{}  \u{2022} ", prefix), Style::default().fg(LIST_BULLET_COLOR));
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
                        Style::default().fg(LIST_BULLET_COLOR),
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
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 || text.chars().count() <= width {
        return vec![text.to_string()];
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

fn draw(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &App,
    model: &str,
    thinking: &str,
    effect: &mut Option<Effect>,
    exit_effect: &mut Option<Effect>,
    elapsed: std::time::Duration,
) -> io::Result<()> {
    terminal.draw(|frame| {
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),  // header
                Constraint::Min(1),    // messages
                Constraint::Length(3), // input
                Constraint::Length(1), // footer
            ])
            .split(frame.area());

        // -- Header ----------------------------------------------------------
        let status_span = if app.streaming {
            Span::styled(" streaming ", Style::default().fg(STATUS_STREAMING))
        } else {
            Span::styled(" ready ", Style::default().fg(STATUS_READY))
        };
        let header = Paragraph::new(Line::from(vec![
            Span::styled(" agent-runtime ", Style::default().fg(HEADER_FG).add_modifier(Modifier::BOLD)),
            Span::styled("\u{2502}", Style::default().fg(BORDER)),
            status_span,
        ]))
        .style(Style::default().bg(BG));
        frame.render_widget(header, outer[0]);

        // -- Messages --------------------------------------------------------
        let msg_area = outer[1];
        let content_height = msg_area.height.saturating_sub(2) as usize;
        let content_width = msg_area.width.saturating_sub(4) as usize; // borders + padding

        let all_lines = app.render_lines(content_width);
        let total = all_lines.len();
        let end = total.saturating_sub(app.scroll_back as usize);
        let start = end.saturating_sub(content_height);
        let visible: Vec<Line> = all_lines[start..end].to_vec();

        let msg_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(Style::default().fg(BORDER))
            .padding(Padding::horizontal(1));
        let messages_widget = Paragraph::new(visible).block(msg_block);
        frame.render_widget(messages_widget, msg_area);

        // Scroll indicator
        if app.scroll_back > 0 {
            let indicator = format!(" \u{2191}{} ", app.scroll_back);
            let indicator_widget = Paragraph::new(Span::styled(
                indicator,
                Style::default().fg(MUTED),
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

        // -- Input -----------------------------------------------------------
        let input_border_color = if app.streaming { BORDER } else { BORDER_ACTIVE };
        let input_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(Style::default().fg(input_border_color))
            .style(Style::default().bg(BG));
        let input_widget = Paragraph::new(Line::from(vec![
            Span::styled("\u{276f} ", Style::default().fg(PROMPT_FG)),
            Span::styled(&app.input, Style::default().fg(INPUT_FG)),
        ]))
        .block(input_block);
        frame.render_widget(input_widget, outer[2]);

        // Cursor
        frame.set_cursor_position((
            outer[2].x + 3 + app.cursor_pos as u16,
            outer[2].y + 1,
        ));

        // -- Footer ----------------------------------------------------------
        let footer_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(model.len() as u16 + 50),
            ])
            .split(outer[3]);

        let keybinds = Paragraph::new(Line::from(vec![
            Span::styled(" ctrl+c ", Style::default().fg(MUTED)),
            Span::styled("quit", Style::default().fg(HELP_FG)),
            Span::styled("  esc ", Style::default().fg(MUTED)),
            Span::styled("abort", Style::default().fg(HELP_FG)),
            Span::styled("  \u{2191}\u{2193} ", Style::default().fg(MUTED)),
            Span::styled("history", Style::default().fg(HELP_FG)),
            Span::styled("  shift+\u{2191}\u{2193} ", Style::default().fg(MUTED)),
            Span::styled("scroll", Style::default().fg(HELP_FG)),
            Span::styled("  enter ", Style::default().fg(MUTED)),
            Span::styled("send", Style::default().fg(HELP_FG)),
        ]))
        .style(Style::default().bg(BG));
        frame.render_widget(keybinds, footer_chunks[0]);

        let cost_str = if app.session_cost > 0.0 {
            format!("${:.4} ", app.session_cost)
        } else {
            String::new()
        };
        let token_str = if app.total_input_tokens > 0 || app.total_output_tokens > 0 {
            format!("{}in {}out  ", format_tokens(app.total_input_tokens), format_tokens(app.total_output_tokens))
        } else {
            String::new()
        };
        let info = Paragraph::new(Line::from(vec![
            Span::styled(&cost_str, Style::default().fg(COST_COLOR)),
            Span::styled(&token_str, Style::default().fg(MUTED)),
            Span::styled("thinking:", Style::default().fg(MUTED)),
            Span::styled(format!("{} ", thinking), Style::default().fg(HELP_FG)),
            Span::styled(" ", Style::default().fg(MUTED)),
            Span::styled(model, Style::default().fg(HEADER_FG)),
            Span::styled(" ", Style::default()),
        ]))
        .alignment(Alignment::Right)
        .style(Style::default().bg(BG));
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
#[command(name = "chatui", about = "Terminal chat UI for agent-runtime")]
struct Cli {
    /// Continue a previous session. Optionally provide a session ID (partial match supported).
    #[arg(long = "continue", value_name = "SESSION_ID")]
    continue_session: Option<Option<String>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut runtime = Runtime::new().await?;

    // Load config from ~/.agent-runtime/
    let config_dir = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".agent-runtime");
    let config_path = config_dir.join("config");

    // Parse config file (key=value, one per line)
    if config_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&config_path) {
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') { continue; }
                if let Some((key, val)) = line.split_once('=') {
                    let key = key.trim();
                    let val = val.trim();
                    match key {
                        "model" => runtime.set_model(val.to_string()),
                        "thinking" => {
                            match val {
                                "low" => runtime.set_thinking_budget(2048),
                                "medium" => runtime.set_thinking_budget(4096),
                                "high" => runtime.set_thinking_budget(16384),
                                "xhigh" => runtime.set_thinking_budget(32768),
                                _ => {
                                    if let Ok(n) = val.parse::<u32>() {
                                        runtime.set_thinking_budget(n);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // Load system prompt: ~/.agent-runtime/system.md > default
    let system_prompt_path = config_dir.join("system.md");
    let system_prompt = if system_prompt_path.exists() {
        std::fs::read_to_string(&system_prompt_path)
            .unwrap_or_default()
    } else {
        "You are a helpful AI agent running in a terminal. \
         You have access to bash, read, and write tools. \
         Be concise and direct. Use tools when the user asks you to interact with the filesystem or run commands."
        .to_string()
    };
    runtime.set_system_prompt(system_prompt);

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
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture).unwrap();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut event_reader = EventStream::new();
    let mut stream: Option<std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send>>> = None;
    let mut cancel_token: Option<CancellationToken> = None;
    let mut boot_fx: Option<Effect> = Some(
        fx::fade_from_fg(Color::Black, (300, Interpolation::QuadOut))
    );
    let mut exit_fx: Option<Effect> = None;
    let mut last_frame = Instant::now();

    loop {
        let elapsed = last_frame.elapsed();
        last_frame = Instant::now();
        draw(&mut terminal, &app, runtime.model(), runtime.thinking_level(), &mut boot_fx, &mut exit_fx, elapsed).unwrap();

        tokio::select! {
            // Tick to keep redrawing during animations
            _ = tokio::time::sleep(std::time::Duration::from_millis(16)), if boot_fx.is_some() || exit_fx.is_some() => {
                if exit_fx.as_ref().map_or(false, |fx| fx.done()) {
                    break;
                }
                continue;
            }
            maybe_event = event_reader.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) => {
                        match (key.code, key.modifiers) {
                            (KeyCode::Char('c'), KeyModifiers::CONTROL) if exit_fx.is_none() => {
                                exit_fx = Some(fx::dissolve((800, Interpolation::QuadIn)));
                            }
                            (KeyCode::Esc, _) if app.streaming => {
                                if let Some(ref ct) = cancel_token {
                                    ct.cancel();
                                }
                                stream = None;
                                cancel_token = None;
                                app.streaming = false;
                                app.push_msg(ChatMessage::Error("aborted".to_string()));
                            }
                            (KeyCode::Enter, _) if !app.streaming && !app.input.is_empty() => {
                                let input = app.input.clone();
                                app.input_history.push(input.clone());
                                app.history_index = None;
                                app.input_stash.clear();
                                app.input.clear();
                                app.cursor_pos = 0;
                                app.scroll_back = 0;

                                if input.starts_with('/') {
                                    let parts: Vec<&str> = input[1..].splitn(2, ' ').collect();
                                    let raw_cmd = parts[0];
                                    let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");
                                    let all_cmds = ["clear", "model", "system", "thinking", "sessions", "resume", "help", "quit", "exit"];
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
                                            app.api_messages.clear();
                                            app.total_input_tokens = 0;
                                            app.total_output_tokens = 0;
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
                                                let _ = std::fs::create_dir_all(&config_dir);
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
                                        }
                                        "quit" | "exit" => {
                                            exit_fx = Some(fx::dissolve((800, Interpolation::QuadIn)));
                                        }
                                        _ => {
                                            app.push_msg(ChatMessage::Error(
                                                format!("unknown command: /{}", cmd)
                                            ));
                                        }
                                    }
                                } else {
                                    app.push_msg(ChatMessage::User(input.clone()));
                                    app.api_messages.push(json!({"role": "user", "content": input}));
                                    let ct = CancellationToken::new();
                                    stream = Some(runtime.run_stream_with_messages(app.api_messages.clone(), ct.clone()));
                                    cancel_token = Some(ct);
                                    app.streaming = true;
                                }
                            }
                            (KeyCode::Tab, _) if app.input.starts_with('/') => {
                                let partial = &app.input[1..];
                                let commands = ["clear", "model", "system", "thinking", "sessions", "resume", "help", "quit", "exit"];
                                let matches: Vec<&&str> = commands.iter()
                                    .filter(|c| c.starts_with(partial))
                                    .collect();
                                if matches.len() == 1 {
                                    app.input = format!("/{}", matches[0]);
                                    app.cursor_pos = app.input.len();
                                } else if matches.len() > 1 {
                                    // Find common prefix
                                    let first = matches[0];
                                    let common_len = (0..first.len())
                                        .take_while(|&i| matches.iter().all(|m| m.as_bytes().get(i) == first.as_bytes().get(i)))
                                        .count();
                                    if common_len > partial.len() {
                                        app.input = format!("/{}", &first[..common_len]);
                                        app.cursor_pos = app.input.len();
                                    }
                                }
                            }
                            (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
                                app.cursor_pos = 0;
                            }
                            (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
                                app.cursor_pos = app.input.len();
                            }
                            (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                                // Delete word backward (same as Alt+Backspace)
                                let mut pos = app.cursor_pos;
                                let bytes = app.input.as_bytes();
                                while pos > 0 && bytes[pos - 1] == b' ' { pos -= 1; }
                                while pos > 0 && bytes[pos - 1] != b' ' { pos -= 1; }
                                app.input.drain(pos..app.cursor_pos);
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
                                app.cursor_pos = app.input.len();
                            }
                            (KeyCode::Left, KeyModifiers::ALT) => {
                                // Jump word left
                                let bytes = app.input.as_bytes();
                                let mut pos = app.cursor_pos;
                                while pos > 0 && bytes[pos - 1] == b' ' { pos -= 1; }
                                while pos > 0 && bytes[pos - 1] != b' ' { pos -= 1; }
                                app.cursor_pos = pos;
                            }
                            (KeyCode::Right, KeyModifiers::ALT) => {
                                // Jump word right
                                let bytes = app.input.as_bytes();
                                let len = bytes.len();
                                let mut pos = app.cursor_pos;
                                while pos < len && bytes[pos] != b' ' { pos += 1; }
                                while pos < len && bytes[pos] == b' ' { pos += 1; }
                                app.cursor_pos = pos;
                            }
                            (KeyCode::Backspace, KeyModifiers::ALT) => {
                                // Delete word backward
                                let mut pos = app.cursor_pos;
                                let bytes = app.input.as_bytes();
                                while pos > 0 && bytes[pos - 1] == b' ' { pos -= 1; }
                                while pos > 0 && bytes[pos - 1] != b' ' { pos -= 1; }
                                app.input.drain(pos..app.cursor_pos);
                                app.cursor_pos = pos;
                            }
                            (KeyCode::Char(c), _) => {
                                app.input.insert(app.cursor_pos, c);
                                app.cursor_pos += 1;
                            }
                            (KeyCode::Backspace, _) if app.cursor_pos > 0 => {
                                app.cursor_pos -= 1;
                                app.input.remove(app.cursor_pos);
                            }
                            (KeyCode::Left, _) if app.cursor_pos > 0 => {
                                app.cursor_pos -= 1;
                            }
                            (KeyCode::Right, _) if app.cursor_pos < app.input.len() => {
                                app.cursor_pos += 1;
                            }
                            (KeyCode::Up, KeyModifiers::SHIFT) => {
                                app.scroll_back = app.scroll_back.saturating_add(1);
                            }
                            (KeyCode::Down, KeyModifiers::SHIFT) => {
                                app.scroll_back = app.scroll_back.saturating_sub(1);
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
                            }
                            MouseEventKind::ScrollDown => {
                                app.scroll_back = app.scroll_back.saturating_sub(3);
                            }
                            _ => {}
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
                    match event {
                        StreamEvent::Thinking(text) => {
                            app.append_or_update_thinking(&text);
                        }
                        StreamEvent::Text(text) => {
                            app.append_or_update_text(&text);
                        }
                        StreamEvent::ToolUse { tool_name, input, .. } => {
                            let input_str = serde_json::to_string(&input).unwrap_or_default();
                            app.push_msg(ChatMessage::ToolUse { tool_name, input: input_str });
                        }
                        StreamEvent::ToolResult { result, .. } => {
                            app.push_msg(ChatMessage::ToolResult(result));
                        }
                        StreamEvent::MessageHistory(history) => {
                            app.api_messages = history;
                            app.save_session();
                        }
                        StreamEvent::Usage { input_tokens, output_tokens } => {
                            app.add_usage(input_tokens, output_tokens, runtime.model());
                        }
                        StreamEvent::Done => {
                            app.streaming = false;
                            stream = None;
                            cancel_token = None;
                        }
                        StreamEvent::Error(err) => {
                            app.push_msg(ChatMessage::Error(err));
                            app.streaming = false;
                            stream = None;
                            cancel_token = None;
                            app.api_messages.pop();
                        }
                    }
                }
            }
        }
    }

    // Save session on exit
    app.save_session();

    disable_raw_mode().unwrap();
    execute!(terminal.backend_mut(), DisableMouseCapture, LeaveAlternateScreen).unwrap();
    terminal.show_cursor().unwrap();

    Ok(())
}
