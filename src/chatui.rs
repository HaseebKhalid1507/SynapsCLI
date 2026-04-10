use agent_runtime::{Runtime, StreamEvent, Result};
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
use tachyonfx::{fx, Effect, Interpolation, Shader};

// -- Theme -------------------------------------------------------------------

const TOOL_RESULT_MAX_LEN: usize = 300;

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

struct App {
    messages: Vec<ChatMessage>,
    input: String,
    cursor_pos: usize,
    scroll_back: u16,
    api_messages: Vec<Value>,
    streaming: bool,
    input_history: Vec<String>,
    history_index: Option<usize>, // None = typing new input, Some(i) = browsing history
    input_stash: String, // saves current input when browsing history
}

impl App {
    fn new() -> Self {
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
        }
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
        if let Some(ChatMessage::Text(ref mut existing)) = self.messages.last_mut() {
            existing.push_str(text);
        } else {
            self.messages.push(ChatMessage::Text(text.to_string()));
        }
    }

    fn append_or_update_thinking(&mut self, text: &str) {
        if let Some(ChatMessage::Thinking(ref mut existing)) = self.messages.last_mut() {
            existing.push_str(text);
        } else {
            self.messages.push(ChatMessage::Thinking(text.to_string()));
        }
    }

    fn render_lines(&self, width: usize) -> Vec<Line<'_>> {
        let mut lines: Vec<Line> = Vec::new();
        let indent = "  ";
        let w = width.saturating_sub(2); // account for indent

        for msg in &self.messages {
            match msg {
                ChatMessage::User(text) => {
                    if !lines.is_empty() {
                        lines.push(Line::from(""));
                    }
                    let bg_style = Style::default().bg(USER_BG);
                    let pad = indent;
                    // Top padding
                    lines.push(Line::from(Span::styled(format!("{:<width$}", "", width = width), bg_style)));
                    // Label line
                    let label = format!("{}{}  \u{25cf} You", pad, pad);
                    let padded_label = format!("{:<width$}", label, width = width);
                    lines.push(Line::from(Span::styled(
                        padded_label,
                        Style::default().fg(USER_COLOR).bg(USER_BG).add_modifier(Modifier::BOLD),
                    )));
                    // Content
                    let style = Style::default().fg(USER_COLOR).bg(USER_BG);
                    for line in text.lines() {
                        for wline in wrap_text(&format!("{}{}    {}", pad, pad, line), width) {
                            let padded = format!("{:<width$}", wline, width = width);
                            lines.push(Line::from(Span::styled(padded, style)));
                        }
                    }
                    // Bottom padding
                    lines.push(Line::from(Span::styled(format!("{:<width$}", "", width = width), bg_style)));
                }
                ChatMessage::Thinking(text) => {
                    let style = Style::default().fg(THINKING_COLOR).add_modifier(Modifier::ITALIC);
                    let label_style = Style::default().fg(THINKING_COLOR);
                    lines.push(Line::from(Span::styled(
                        format!("{}\u{2026} thinking", indent),
                        label_style,
                    )));
                    // Show condensed thinking — first few lines
                    let thinking_lines: Vec<&str> = text.lines().collect();
                    let show = thinking_lines.len().min(4);
                    for line in &thinking_lines[..show] {
                        for wline in wrap_text(&format!("{}  {}", indent, line), width) {
                            lines.push(Line::from(Span::styled(wline, style)));
                        }
                    }
                    if thinking_lines.len() > 4 {
                        lines.push(Line::from(Span::styled(
                            format!("{}  [{} more lines]", indent, thinking_lines.len() - 4),
                            label_style,
                        )));
                    }
                }
                ChatMessage::Text(text) => {
                    if !lines.is_empty() {
                        lines.push(Line::from(""));
                    }
                    // Label line
                    lines.push(Line::from(Span::styled(
                        format!("{}\u{25cf} Agent", indent),
                        Style::default().fg(CLAUDE_LABEL).add_modifier(Modifier::BOLD),
                    )));
                    // Content
                    let style = Style::default().fg(CLAUDE_TEXT);
                    for line in text.lines() {
                        for wline in wrap_text(&format!("{}  {}", indent, line), width) {
                            lines.push(Line::from(Span::styled(wline, style)));
                        }
                    }
                    if text.is_empty() {
                        lines.push(Line::from(Span::styled(
                            format!("{}  ...", indent),
                            Style::default().fg(MUTED),
                        )));
                    }
                }
                ChatMessage::ToolUse { tool_name, input } => {
                    lines.push(Line::from(""));
                    // Tool header with icon
                    lines.push(Line::from(Span::styled(
                        format!("{}  \u{2192} {}", indent, tool_name),
                        Style::default().fg(TOOL_LABEL).add_modifier(Modifier::BOLD),
                    )));
                    // Parse and show params
                    let param_style = Style::default().fg(TOOL_PARAM);
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(input) {
                        if let Some(obj) = parsed.as_object() {
                            for (k, v) in obj {
                                let val = match v.as_str() {
                                    Some(s) => s.to_string(),
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
                    let truncated = if result.chars().count() > TOOL_RESULT_MAX_LEN {
                        let s: String = result.chars().take(TOOL_RESULT_MAX_LEN).collect();
                        format!("{}...", s)
                    } else {
                        result.clone()
                    };
                    let style = Style::default().fg(TOOL_RESULT_COLOR);
                    for line in truncated.lines().take(8) {
                        let full = format!("{}    \u{2502} {}", indent, line);
                        for wline in wrap_text(&full, width) {
                            lines.push(Line::from(Span::styled(wline, style)));
                        }
                    }
                    let total_lines = truncated.lines().count();
                    if total_lines > 8 {
                        lines.push(Line::from(Span::styled(
                            format!("{}    [{} more lines]", indent, total_lines - 8),
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

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars.chunks(width).map(|c| c.iter().collect()).collect()
}

fn draw(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &App,
    model: &str,
    thinking: &str,
    effect: &mut Option<Effect>,
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
                Constraint::Length(model.len() as u16 + 16),
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

        let info = Paragraph::new(Line::from(vec![
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
    })?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut runtime = Runtime::new().await?;
    runtime.set_system_prompt(
        "You are a helpful AI agent running in a terminal. \
         You have access to bash, read, and write tools. \
         Be concise and direct. Use tools when the user asks you to interact with the filesystem or run commands."
        .to_string()
    );

    enable_raw_mode().unwrap();
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture).unwrap();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).unwrap();

    let mut app = App::new();
    let mut event_reader = EventStream::new();
    let mut stream: Option<std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send>>> = None;
    let mut boot_fx: Option<Effect> = Some(
        fx::fade_from_fg(Color::Black, (300, Interpolation::QuadOut))
    );
    let mut last_frame = Instant::now();

    loop {
        if app.streaming {
            app.scroll_back = 0;
        }

        let elapsed = last_frame.elapsed();
        last_frame = Instant::now();
        draw(&mut terminal, &app, runtime.model(), runtime.thinking_level(), &mut boot_fx, elapsed).unwrap();

        tokio::select! {
            // Tick to keep redrawing during animations
            _ = tokio::time::sleep(std::time::Duration::from_millis(16)), if boot_fx.is_some() => {
                continue;
            }
            maybe_event = event_reader.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) => {
                        match (key.code, key.modifiers) {
                            (KeyCode::Char('c'), KeyModifiers::CONTROL) => break,
                            (KeyCode::Esc, _) if app.streaming => {
                                stream = None;
                                app.streaming = false;
                                app.messages.push(ChatMessage::Error("aborted".to_string()));
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
                                    let cmd = parts[0];
                                    let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");
                                    match cmd {
                                        "clear" => {
                                            app.messages.clear();
                                            app.api_messages.clear();
                                            app.messages.push(ChatMessage::System("conversation cleared".to_string()));
                                        }
                                        "model" => {
                                            if arg.is_empty() {
                                                app.messages.push(ChatMessage::System(
                                                    format!("current model: {}", runtime.model())
                                                ));
                                            } else {
                                                runtime.set_model(arg.to_string());
                                                app.messages.push(ChatMessage::System(
                                                    format!("model set to: {}", arg)
                                                ));
                                            }
                                        }
                                        "system" => {
                                            if arg.is_empty() {
                                                app.messages.push(ChatMessage::System(
                                                    "usage: /system <prompt>".to_string()
                                                ));
                                            } else {
                                                runtime.set_system_prompt(arg.to_string());
                                                app.messages.push(ChatMessage::System(
                                                    "system prompt updated".to_string()
                                                ));
                                            }
                                        }
                                        "help" => {
                                            app.messages.push(ChatMessage::System(
                                                "/clear — reset conversation".to_string()
                                            ));
                                            app.messages.push(ChatMessage::System(
                                                "/model [name] — show or set model".to_string()
                                            ));
                                            app.messages.push(ChatMessage::System(
                                                "/system <prompt> — set system prompt".to_string()
                                            ));
                                            app.messages.push(ChatMessage::System(
                                                "/help — show this".to_string()
                                            ));
                                        }
                                        "quit" | "exit" => break,
                                        _ => {
                                            app.messages.push(ChatMessage::Error(
                                                format!("unknown command: /{}", cmd)
                                            ));
                                        }
                                    }
                                } else {
                                    app.messages.push(ChatMessage::User(input.clone()));
                                    app.api_messages.push(json!({"role": "user", "content": input}));
                                    stream = Some(runtime.run_stream_with_messages(app.api_messages.clone()));
                                    app.streaming = true;
                                }
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
                            app.messages.push(ChatMessage::ToolUse { tool_name, input: input_str });
                        }
                        StreamEvent::ToolResult { result, .. } => {
                            app.messages.push(ChatMessage::ToolResult(result));
                        }
                        StreamEvent::MessageHistory(history) => {
                            app.api_messages = history;
                        }
                        StreamEvent::Done => {
                            app.streaming = false;
                            stream = None;
                        }
                        StreamEvent::Error(err) => {
                            app.messages.push(ChatMessage::Error(err));
                            app.streaming = false;
                            stream = None;
                            app.api_messages.pop();
                        }
                    }
                }
            }
        }
    }

    disable_raw_mode().unwrap();
    execute!(terminal.backend_mut(), DisableMouseCapture, LeaveAlternateScreen).unwrap();
    terminal.show_cursor().unwrap();

    Ok(())
}
