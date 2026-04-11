use synaps_cli::{Runtime, StreamEvent, Result, CancellationToken, Session, list_sessions, latest_session, find_session};
use clap::Parser;
use crossterm::{
    event::{Event, KeyCode, KeyModifiers, MouseEventKind, EnableMouseCapture, DisableMouseCapture, EventStream},
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
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

use std::sync::LazyLock;

// -- Theme -------------------------------------------------------------------

// Syntect loaded once, reused forever
static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(|| SyntaxSet::load_defaults_newlines());
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(|| ThemeSet::load_defaults());

// -- Palette: warm monochrome with a single amber accent --------------------
// The guiding idea: hierarchy through weight and whitespace, not hue.
// One signature accent used deliberately; everything else lives on a
// warm neutral scale so the UI reads as quiet and deliberate.

// Base — warm near-black, no blue cast
const BG: Color           = Color::Rgb(13, 14, 16);   // main background
const BORDER: Color       = Color::Rgb(32, 34, 38);   // dividers, inactive borders

// Text scale — warm neutrals, from primary to ghost
const TEXT: Color         = Color::Rgb(220, 218, 210); // primary text
const TEXT_DIM: Color     = Color::Rgb(140, 138, 132); // labels, secondary text
const TEXT_FAINT: Color   = Color::Rgb(90, 88, 82);    // hints, keybinds
const TEXT_GHOST: Color   = Color::Rgb(60, 58, 54);    // ghost text, thinking

// Signature accent — muted amber
const ACCENT: Color       = Color::Rgb(198, 162, 108);
const ACCENT_DIM: Color   = Color::Rgb(132, 108, 72);

// Semantic
const ERROR_COLOR: Color  = Color::Rgb(208, 110, 100); // desaturated red

// Aliases kept for readability in helpers that still use semantic names
const MUTED: Color              = TEXT_FAINT;
const CLAUDE_TEXT: Color        = TEXT;
const STATUS_STREAMING: Color   = ACCENT;
const STATUS_READY: Color       = TEXT_DIM;
const INPUT_FG: Color           = TEXT;
const PROMPT_FG: Color          = ACCENT;
const BORDER_ACTIVE: Color      = ACCENT_DIM;

// Markdown
const CODE_FG: Color            = Color::Rgb(200, 195, 180);
const CODE_BG: Color            = Color::Rgb(18, 20, 23);
const HEADING_COLOR: Color      = TEXT;
const QUOTE_COLOR: Color        = TEXT_DIM;
const LIST_BULLET_COLOR: Color  = ACCENT;
const TABLE_BORDER_COLOR: Color = TEXT_GHOST;
const TABLE_HEADER_COLOR: Color = ACCENT;
const TABLE_CELL_COLOR: Color   = TEXT;

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
    total_cache_read_tokens: u64,
    total_cache_creation_tokens: u64,
    session_cost: f64,
    session: Session,
    line_cache: Vec<Line<'static>>,
    cache_width: usize,
    dirty: bool,
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
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            session_cost: 0.0,
            session,
            line_cache: Vec::new(),
            cache_width: 0,
            dirty: true,
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
        self.dirty = true;
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
        // Left margin: 2 spaces of breathing room before the accent bar + content.
        let m = "  ";
        // Vertical accent bar used by user blocks: a thin left-edge rule.
        let bar = "\u{258F}"; // ▏ left one-eighth block (sits flush on the left of the cell)

        for (i, tmsg) in self.messages.iter().enumerate() {
            let ts = &tmsg.time;
            match &tmsg.msg {
                // -- User ----------------------------------------------------
                // No more boxy background. Just a quiet left accent bar and
                // an understated "you" label with a right-aligned timestamp.
                ChatMessage::User(text) => {
                    let bar_s = Style::default().fg(ACCENT);
                    let label_s = Style::default().fg(TEXT_DIM);
                    let body_s = Style::default().fg(TEXT);
                    let ts_s = Style::default().fg(TEXT_GHOST);

                    // Breathing room above
                    lines.push(Line::from(""));

                    // Header: "  ▕ you                             14:32"
                    let header_left = format!("{}{} you", m, bar);
                    let ts_str = format!("{} ", ts);
                    let gap = width.saturating_sub(header_left.chars().count() + ts_str.chars().count());
                    lines.push(Line::from(vec![
                        Span::styled(format!("{}{} ", m, bar), bar_s),
                        Span::styled("you", label_s),
                        Span::styled(" ".repeat(gap), Style::default()),
                        Span::styled(ts_str, ts_s),
                    ]));

                    // Body lines — each prefixed by the same bar so the column reads as a block
                    for line in text.lines() {
                        for wline in wrap_text(&format!("{}  {}", m, line), width.saturating_sub(2)) {
                            // wline already includes the m prefix; strip it, re-emit with bar
                            let content = wline.strip_prefix(m).unwrap_or(&wline);
                            lines.push(Line::from(vec![
                                Span::styled(format!("{}{} ", m, bar), bar_s),
                                Span::styled(content.trim_start().to_string(), body_s),
                            ]));
                        }
                    }

                    // Breathing room below
                    lines.push(Line::from(""));
                }

                // -- Thinking ------------------------------------------------
                ChatMessage::Thinking(text) => {
                    let ghost = Style::default().fg(TEXT_GHOST);
                    let ghost_italic = ghost.add_modifier(Modifier::ITALIC);
                    lines.push(Line::from(vec![
                        Span::styled(format!("{}{} ", m, bar), ghost),
                        Span::styled("thinking", ghost),
                    ]));
                    let tlines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
                    let show = tlines.len().min(6);
                    for line in &tlines[..show] {
                        for wline in wrap_text(&format!("{}{}  {}", m, bar, line.trim()), width) {
                            lines.push(Line::from(Span::styled(wline, ghost_italic)));
                        }
                    }
                    if tlines.len() > 6 {
                        lines.push(Line::from(Span::styled(
                            format!("{}{}  +{} lines", m, bar, tlines.len() - 6), ghost,
                        )));
                    }
                }

                // -- Assistant text ------------------------------------------
                // No separator rule, no glyph. A single quiet "agent" label
                // and content wrapped to width. Vertical whitespace frames it.
                ChatMessage::Text(text) => {
                    let label_s = Style::default().fg(TEXT_DIM);
                    let ts_s = Style::default().fg(TEXT_GHOST);

                    // Add a blank line before the assistant block if this isn't
                    // the very first message, for rhythm between turns.
                    if i > 0 {
                        lines.push(Line::from(""));
                    }

                    let label = format!("{}agent", m);
                    let ts_str = format!("{} ", ts);
                    let gap = width.saturating_sub(label.chars().count() + ts_str.chars().count());
                    lines.push(Line::from(vec![
                        Span::styled(label, label_s),
                        Span::styled(" ".repeat(gap), Style::default()),
                        Span::styled(ts_str, ts_s),
                    ]));

                    if text.is_empty() {
                        lines.push(Line::from(Span::styled(
                            format!("{}\u{2026}", m), Style::default().fg(TEXT_GHOST),
                        )));
                    } else {
                        lines.extend(render_markdown(text, m, width));
                    }
                }

                // -- Tool use ------------------------------------------------
                // Single consistent marker in the accent color. No per-tool
                // icon switch. Params indented underneath in quiet neutral.
                ChatMessage::ToolUse { tool_name, input } => {
                    lines.push(Line::from(vec![
                        Span::styled(format!("{}\u{00b7} ", m), Style::default().fg(ACCENT)),
                        Span::styled(tool_name.clone(), Style::default().fg(TEXT_DIM)),
                    ]));
                    let param_style = Style::default().fg(TEXT_FAINT);
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(input) {
                        if let Some(obj) = parsed.as_object() {
                            for (k, v) in obj {
                                let val = match v.as_str() {
                                    Some(s) if s.len() > 120 => {
                                        let p: String = s.chars().take(120).collect();
                                        format!("{}\u{2026}", p)
                                    }
                                    Some(s) => s.to_string(),
                                    None => v.to_string(),
                                };
                                let line_str = format!("{}    {} {}", m, k, val);
                                for wline in wrap_text(&line_str, width) {
                                    lines.push(Line::from(Span::styled(wline, param_style)));
                                }
                            }
                        }
                    }
                }

                // -- Tool result ---------------------------------------------
                // Drop the `└─ ok (N lines)` flourish. Just the content with a
                // quiet left margin; truncation count only when it matters.
                ChatMessage::ToolResult(result) => {
                    let is_error = result.starts_with("Tool execution failed")
                        || result.starts_with("Unknown tool");
                    let style = if is_error {
                        Style::default().fg(ERROR_COLOR)
                    } else {
                        Style::default().fg(TEXT_DIM)
                    };

                    let result_lines: Vec<&str> = result.lines().collect();
                    let max_show = if result_lines.len() > 30 { 15 } else { 12 };
                    let show = result_lines.len().min(max_show);

                    for line in &result_lines[..show] {
                        let full = format!("{}    {}", m, line);
                        for wline in wrap_text(&full, width) {
                            lines.push(Line::from(Span::styled(wline, style)));
                        }
                    }
                    if result_lines.len() > show {
                        lines.push(Line::from(Span::styled(
                            format!("{}    +{} lines", m, result_lines.len() - show),
                            Style::default().fg(TEXT_FAINT),
                        )));
                    }
                }

                // -- Error ---------------------------------------------------
                ChatMessage::Error(err) => {
                    lines.push(Line::from(vec![
                        Span::styled(format!("{}\u{2718} ", m), Style::default().fg(ERROR_COLOR)),
                        Span::styled(err.clone(), Style::default().fg(ERROR_COLOR)),
                    ]));
                }

                // -- System --------------------------------------------------
                ChatMessage::System(msg) => {
                    lines.push(Line::from(Span::styled(
                        format!("{}{}", m, msg),
                        Style::default().fg(TEXT_FAINT).add_modifier(Modifier::ITALIC),
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
fn highlight_code_block(code: &str, lang: &str, prefix: &str) -> Vec<Line<'static>> {
    let ss = &*SYNTAX_SET;
    let ts = &*THEME_SET;
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

    let border_style = Style::default().fg(TABLE_BORDER_COLOR);
    let header_style = Style::default().fg(TABLE_HEADER_COLOR).add_modifier(ratatui::style::Modifier::BOLD);
    let cell_style = Style::default().fg(TABLE_CELL_COLOR);

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
    let base_style = Style::default().fg(CLAUDE_TEXT);
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
                    format!("{}  \u{2500}\u{2500}", prefix)
                } else {
                    format!("{}  \u{2500}\u{2500} {}", prefix, code_lang)
                };
                lines.push(Line::from(Span::styled(label, Style::default().fg(TEXT_GHOST))));
                code_buf.clear();
            } else {
                // End of code block — highlight, flush, no trailing rule.
                lines.extend(highlight_code_block(&code_buf, &code_lang, prefix));
                in_code_block = false;
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

fn boot_effect() -> Effect {
    use tachyonfx::fx::Direction as FxDir;
    fx::parallel(&[
        // CRT-style scanline reveal, top-to-bottom, clean (no randomness) with a tight gradient trail
        fx::sweep_in(FxDir::UpToDown, 10, 0, Color::Rgb(28, 28, 32), (750, Interpolation::QuintOut)),
        // long, slow fade from pure black — elegant deceleration
        fx::fade_from_fg(Color::Black, (750, Interpolation::QuintOut)),
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
        // A single bullet distinguishes status; its color is the only signal.
        let status_span = if app.streaming {
            Span::styled("\u{2022} streaming", Style::default().fg(STATUS_STREAMING))
        } else {
            Span::styled("\u{2022} ready", Style::default().fg(STATUS_READY))
        };
        let header = Paragraph::new(Line::from(vec![
            Span::styled("  synaps", Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
            Span::styled(" cli", Style::default().fg(TEXT_FAINT)),
            Span::styled("  \u{00b7}  ", Style::default().fg(TEXT_GHOST)),
            status_span,
        ]))
        .style(Style::default().bg(BG));
        frame.render_widget(header, outer[0]);

        // -- Messages --------------------------------------------------------
        // No borders — whitespace alone frames the message column.
        let msg_area = outer[1];
        let content_height = msg_area.height as usize;
        let content_width = msg_area.width.saturating_sub(2) as usize; // padding only

        // Rebuild line cache only when content changed or width changed
        if app.dirty || app.cache_width != content_width {
            app.line_cache = app.render_lines(content_width);
            app.cache_width = content_width;
            app.dirty = false;
        }

        let all_lines = &app.line_cache;
        let total = all_lines.len();
        let end = total.saturating_sub(app.scroll_back as usize);
        let start = end.saturating_sub(content_height);
        let visible: Vec<Line> = all_lines[start..end].to_vec();

        let msg_block = Block::default()
            .borders(Borders::NONE)
            .padding(Padding::horizontal(1))
            .style(Style::default().bg(BG));
        let messages_widget = Paragraph::new(visible).block(msg_block);
        frame.render_widget(Clear, msg_area);
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
        // Idle: quiet neutral border. Active: muted amber accent.
        let input_border_color = if app.streaming { BORDER } else { BORDER_ACTIVE };
        let input_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(input_border_color))
            .style(Style::default().bg(BG))
            .padding(Padding::horizontal(1));
        let input_widget = Paragraph::new(Line::from(vec![
            Span::styled("\u{276f} ", Style::default().fg(PROMPT_FG)),
            Span::styled(&app.input, Style::default().fg(INPUT_FG)),
        ]))
        .block(input_block);
        frame.render_widget(input_widget, outer[2]);

        // Cursor — border(1) + padding(1) + "❯ "(2) = 4
        frame.set_cursor_position((
            outer[2].x + 4 + app.cursor_pos as u16,
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

        // Keybind hints: unified dim scale. Key labels in TEXT_FAINT, action
        // names one step quieter in TEXT_GHOST so the whole line recedes.
        let key_s  = Style::default().fg(TEXT_FAINT);
        let act_s  = Style::default().fg(TEXT_GHOST);
        let keybinds = Paragraph::new(Line::from(vec![
            Span::styled("  ctrl+c ",   key_s),
            Span::styled("quit",        act_s),
            Span::styled("   esc ",     key_s),
            Span::styled("abort",       act_s),
            Span::styled("   \u{2191}\u{2193} ", key_s),
            Span::styled("history",     act_s),
            Span::styled("   \u{21e7}\u{2191}\u{2193} ", key_s),
            Span::styled("scroll",      act_s),
            Span::styled("   \u{21b5} ",     key_s),
            Span::styled("send",        act_s),
        ]))
        .style(Style::default().bg(BG));
        frame.render_widget(keybinds, footer_chunks[0]);

        let cost_str = if app.session_cost > 0.0 {
            format!("${:.4} ", app.session_cost)
        } else {
            String::new()
        };
        let token_str = if app.total_input_tokens > 0 || app.total_output_tokens > 0 {
            let mut s = format!(
                "{}in {}out",
                format_tokens(app.total_input_tokens),
                format_tokens(app.total_output_tokens),
            );
            if app.total_cache_read_tokens > 0 || app.total_cache_creation_tokens > 0 {
                s.push_str(&format!(
                    " {}cr {}cw",
                    format_tokens(app.total_cache_read_tokens),
                    format_tokens(app.total_cache_creation_tokens),
                ));
            }
            s.push_str("  ");
            s
        } else {
            String::new()
        };
        // Stats: only cost wears the accent. Everything else is deep dim so
        // the footer sits quietly under the content.
        let info = Paragraph::new(Line::from(vec![
            Span::styled(&cost_str,  Style::default().fg(ACCENT)),
            Span::styled(&token_str, Style::default().fg(TEXT_FAINT)),
            Span::styled("thinking ", Style::default().fg(TEXT_GHOST)),
            Span::styled(format!("{}", thinking), Style::default().fg(TEXT_FAINT)),
            Span::styled("   \u{00b7}   ", Style::default().fg(TEXT_GHOST)),
            Span::styled(model, Style::default().fg(TEXT_DIM)),
            Span::styled("  ", Style::default()),
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
#[command(name = "chatui", about = "Terminal chat UI for SynapsCLI")]
struct Cli {
    /// Continue a previous session. Optionally provide a session ID (partial match supported).
    #[arg(long = "continue", value_name = "SESSION_ID")]
    continue_session: Option<Option<String>>,

    /// System prompt: a string or a path to a file.
    #[arg(long = "system", short = 's', value_name = "PROMPT_OR_FILE")]
    system: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut runtime = Runtime::new().await?;

    // Load config from ~/.synaps-cli/
    let config_dir = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".synaps-cli");
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

    // Load system prompt: --system flag > ~/.synaps-cli/system.md > default
    let system_prompt_path = config_dir.join("system.md");
    let system_prompt = if let Some(ref val) = cli.system {
        let path = std::path::Path::new(val);
        if path.exists() && path.is_file() {
            std::fs::read_to_string(path).unwrap_or_else(|_| val.clone())
        } else {
            val.clone()
        }
    } else if system_prompt_path.exists() {
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
    let mut boot_fx: Option<Effect> = Some(boot_effect());
    let mut exit_fx: Option<Effect> = None;
    let mut last_frame = Instant::now();

    loop {
        let elapsed = last_frame.elapsed();
        last_frame = Instant::now();
        draw(&mut terminal, &mut app, runtime.model(), runtime.thinking_level(), &mut boot_fx, &mut exit_fx, elapsed).unwrap();

        tokio::select! {
            // Tick: redraws during animations AND during streaming (~60fps throttle)
            _ = tokio::time::sleep(std::time::Duration::from_millis(16)), if boot_fx.is_some() || exit_fx.is_some() || app.streaming => {
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
                                exit_fx = Some(quit_effect());
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
                                        }
                                        "quit" | "exit" => {
                                            exit_fx = Some(quit_effect());
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
                                    stream = Some(runtime.run_stream_with_messages(app.api_messages.clone(), ct.clone()).await);
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
                    // Only redraw immediately for structural events (tool calls,
                    // completion, errors). Text/thinking tokens are batched and
                    // rendered by the 16ms tick to avoid hundreds of redraws/sec.
                    let needs_immediate_draw = matches!(&event,
                        StreamEvent::ToolUse { .. }
                        | StreamEvent::ToolResult { .. }
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
                        StreamEvent::Usage {
                            input_tokens,
                            output_tokens,
                            cache_read_input_tokens,
                            cache_creation_input_tokens,
                        } => {
                            app.add_usage(
                                input_tokens,
                                output_tokens,
                                cache_read_input_tokens,
                                cache_creation_input_tokens,
                                runtime.model(),
                            );
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
    execute!(terminal.backend_mut(), DisableMouseCapture, LeaveAlternateScreen).unwrap();
    terminal.show_cursor().unwrap();

    Ok(())
}
