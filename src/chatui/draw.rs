use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Alignment},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, BorderType, Clear, Paragraph, Padding},
    Terminal,
};
use std::io;
use tachyonfx::{fx, Effect, Interpolation, Shader};

use super::theme::THEME;
use super::markdown::format_tokens;
use super::app::{App, SPINNER_FRAMES};

/// Generate a bash execution trace animation string and its pulsing color.
/// Returns (trace_string, Color) for use in Span styling.
pub(crate) fn bash_trace(spinner_frame: usize) -> (String, Color) {
    const CHARS: [char; 8] = [' ', '░', '▒', '▓', '█', '▓', '▒', '░'];
    const WIDTH: usize = 14;
    let offset = (spinner_frame / 2) % (WIDTH + CHARS.len());
    let trace: String = (0..WIDTH).map(|i| {
        let dist = if offset >= i { offset - i } else { WIDTH + CHARS.len() };
        if dist < CHARS.len() { CHARS[dist] } else { ' ' }
    }).collect();
    let pulse = (spinner_frame as f64 / 15.0).sin() * 0.3 + 0.7;
    let Color::Rgb(br, bg, bb) = THEME.load().border_active else { return (trace, Color::Reset) };
    let color = Color::Rgb(
        (br as f64 * pulse) as u8,
        (bg as f64 * pulse) as u8,
        (bb as f64 * pulse) as u8,
    );
    (trace, color)
}

/// Format a tool name for display. Returns (icon, display_name, optional_server_tag).
/// MCP tools like "mcp__byteray__read_pseudocode" become ("⚡", "read_pseudocode", Some("byteray"))
pub(crate) fn format_tool_name(tool_name: &str) -> (&'static str, String, Option<String>) {
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

pub(crate) fn boot_effect() -> Effect {
    use tachyonfx::fx::Direction as FxDir;
    let Color::Rgb(r, g, b) = THEME.load().bg else { return fx::sleep(0) };
    fx::parallel(&[
        // CRT-style scanline reveal, top-to-bottom, clean (no randomness) with a tight gradient trail
        fx::sweep_in(FxDir::UpToDown, 10, 0, Color::Rgb(r.saturating_add(10), g.saturating_add(15), b.saturating_add(20)), (750, Interpolation::QuintOut)),
        // long, slow fade from pure black — elegant deceleration
        fx::fade_from_fg(Color::Rgb(r.saturating_add(2), g.saturating_add(3), b.saturating_add(5)), (750, Interpolation::QuintOut)),
    ])
}

pub(crate) fn quit_effect() -> Effect {
    use tachyonfx::fx::Direction as FxDir;
    let Color::Rgb(r, g, b) = THEME.load().muted else { return fx::sleep(0) };
    fx::sequence(&[
        fx::hsl_shift_fg([180.0, -40.0, 0.0], (180, Interpolation::QuadOut)),
        fx::parallel(&[
            fx::sweep_out(FxDir::DownToUp, 18, 12, Color::Rgb(r, g, b), (650, Interpolation::QuadIn)),
            fx::dissolve((650, Interpolation::QuadIn)),
            fx::fade_to_fg(Color::Black, (650, Interpolation::QuadIn)),
        ]),
    ])
}

pub(crate) fn draw(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    runtime: &synaps_cli::Runtime,
    effect: &mut Option<Effect>,
    exit_effect: &mut Option<Effect>,
    elapsed: std::time::Duration,
    registry: &std::sync::Arc<synaps_cli::skills::registry::CommandRegistry>,
) -> io::Result<()> {
    let model = runtime.model();
    let thinking = runtime.thinking_level();
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
                Style::default().fg(THEME.load().status_streaming),
            )
        } else if !app.subagents.is_empty() {
            let active = app.subagents.iter().filter(|s| !s.done).count();
            let done = app.subagents.iter().filter(|s| s.done).count();
            let spinner_idx = (app.spinner_frame / 3) % SPINNER_FRAMES.len();
            let spinner = if active > 0 { SPINNER_FRAMES[spinner_idx] } else { "\u{2714}" };
            Span::styled(
                format!(" {} {} agent{} ({} done) ", spinner, active, if active != 1 { "s" } else { "" }, done),
                Style::default().fg(THEME.load().subagent_name),
            )
        } else if app.streaming {
            let pulse = ((app.spinner_frame as f64 / 20.0).sin() * 0.3 + 0.7).max(0.4);
            let Color::Rgb(sr, sg, sb) = THEME.load().status_streaming else { unreachable!() };
            let r = (sr as f64 * pulse) as u8;
            let g = (sg as f64 * pulse) as u8;
            let b = (sb as f64 * pulse) as u8;
            Span::styled(" \u{25cf} streaming ", Style::default().fg(Color::Rgb(r, g, b)))
        } else {
            Span::styled(" \u{25cb} ready ", Style::default().fg(THEME.load().status_ready))
        };
        let header = Paragraph::new(Line::from(vec![
            Span::styled("  Synaps", Style::default().fg(THEME.load().header_fg).add_modifier(Modifier::BOLD)),
            Span::styled("CLI ", Style::default().fg(THEME.load().muted)),
            Span::styled("\u{2502}", Style::default().fg(THEME.load().border)),
            status_span,
        ]))
        .style(Style::default().bg(THEME.load().bg));
        frame.render_widget(header, outer[0]);

        // -- Messages --------------------------------------------------------
        let msg_area = outer[1];
        let content_height = msg_area.height.saturating_sub(2) as usize;
        let content_width = msg_area.width.saturating_sub(2) as usize; // horizontal padding only (no left/right borders)

        // Rebuild line cache when missing (invalidated) or width has changed.
        let needs_rebuild = app
            .line_cache
            .as_ref()
            .map_or(true, |(w, _)| *w != content_width);
        if needs_rebuild {
            let lines = app.render_lines(content_width);
            app.line_cache = Some((content_width, lines));
        }
        let all_lines: &[Line<'static>] = &app.line_cache.as_ref().unwrap().1;
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
            .border_style(Style::default().fg(THEME.load().border))
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
                    let Color::Rgb(ar, ag, ab) = THEME.load().border_active else { unreachable!() };
                    let breathe_scale = 0.7 + 0.3 * breathe; // breathe is -1..1, scale is 0.4..1.0
                    let r = (ar as f64 * breathe_scale) as u8;
                    let g = (ag as f64 * breathe_scale) as u8;
                    let b = (ab as f64 * breathe_scale) as u8;
                    let art_style = Style::default().fg(Color::Rgb(r, g, b)).add_modifier(Modifier::BOLD);
                    let Color::Rgb(mr, mg, mb) = THEME.load().muted else { unreachable!() };
                    let sub_style = Style::default().fg(Color::Rgb(
                        (mr as f64 * breathe_scale) as u8,
                        (mg as f64 * breathe_scale) as u8,
                        (mb as f64 * breathe_scale) as u8,
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
                    let art_style = Style::default().fg(THEME.load().muted).add_modifier(Modifier::BOLD);
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
                Style::default().fg(THEME.load().muted),
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
                    let is_timeout = sa.status.contains("timed out");
                    let is_error = sa.status.starts_with("\u{2718}");
                    let done_color = if is_timeout {
                        THEME.load().warning_color
                    } else if is_error {
                        THEME.load().error_color
                    } else {
                        THEME.load().subagent_done
                    };
                    let icon = if is_timeout { "  \u{26a0} " } else if is_error { "  \u{2718} " } else { "  \u{2714} " };
                    agent_lines.push(Line::from(vec![
                        Span::styled(icon, Style::default().fg(done_color)),
                        Span::styled(
                            format!("{} ", sa.name),
                            Style::default().fg(THEME.load().subagent_name).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            &sa.status,
                            Style::default().fg(done_color).add_modifier(Modifier::DIM),
                        ),
                        Span::styled(
                            format!("  {}", time_str),
                            Style::default().fg(THEME.load().subagent_time),
                        ),
                    ]));
                } else {
                    let spinner = SPINNER_FRAMES[spinner_idx];
                    agent_lines.push(Line::from(vec![
                        Span::styled(
                            format!("  {} ", spinner),
                            Style::default().fg(THEME.load().subagent_name),
                        ),
                        Span::styled(
                            format!("{} ", sa.name),
                            Style::default().fg(THEME.load().subagent_name).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            &sa.status,
                            Style::default().fg(THEME.load().subagent_status),
                        ),
                        Span::styled(
                            format!("  {}", time_str),
                            Style::default().fg(THEME.load().subagent_time),
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
                    Style::default().fg(THEME.load().subagent_name).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(THEME.load().subagent_border))
                .style(Style::default().bg(THEME.load().bg));
            let agent_widget = Paragraph::new(agent_lines).block(agent_block);
            frame.render_widget(agent_widget, outer[2]);
        }

        // -- Input -----------------------------------------------------------
        let input_border_color = if app.streaming { THEME.load().border } else { THEME.load().border_active };
        let input_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(input_border_color))
            .style(Style::default().bg(THEME.load().bg));
        // Build pre-wrapped input lines using char-level wrapping (must match input_wrap_info exactly)
        let input_lines_vec: Vec<Line> = {
            use unicode_width::UnicodeWidthChar;
            let w = input_inner_width.max(1) as usize;
            let prefix_width: usize = 2;
            let prompt_style = Style::default().fg(THEME.load().prompt_fg);
            let input_style = Style::default().fg(THEME.load().input_fg);

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

        let key_style = Style::default().fg(THEME.load().muted);
        let label_style = Style::default().fg(THEME.load().help_fg);
        let dot_style = Style::default().fg(THEME.load().help_fg);

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
        .style(Style::default().bg(THEME.load().bg));
        frame.render_widget(keybinds, footer_chunks[0]);

        let cost_str = if app.session_cost > 0.0 {
            format!("${:.4} ", app.session_cost)
        } else {
            String::new()
        };
        let cache_rate = {
            // Total input = uncached input + cache reads + cache writes
            // This shows what % of all input tokens were served from cache
            let total_input = app.total_input_tokens + app.total_cache_read_tokens + app.total_cache_creation_tokens;
            if total_input > 0 && app.total_cache_read_tokens > 0 {
                let rate = (app.total_cache_read_tokens as f64 / total_input as f64 * 100.0) as u32;
                format!(" {}%↺", rate)
            } else {
                String::new()
            }
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
            Span::styled(&cost_str, Style::default().fg(THEME.load().cost_color)),
            Span::styled(&token_str, Style::default().fg(THEME.load().muted)),
            {
                // Context usage bar — per-turn occupancy as a fraction of the
                // model's own context window. Numerator `last_turn_context`
                // is reassigned each usage callback (uncached input + cache
                // read + cache creation). Denominator `last_turn_context_window`
                // adapts when the answering model changes (main Opus → subagent
                // Sonnet, etc.). See app.rs for the update site and
                // `synaps_cli::models::context_window_for_model` for values.
                let turn_context = app.last_turn_context;
                let context_window = app.last_turn_context_window.max(1);
                if turn_context > 0 {
                    let usage_ratio = (turn_context as f64 / context_window as f64).min(1.0);
                    let bar_width: usize = 14;
                    let filled = (usage_ratio * bar_width as f64).round() as usize;
                    let empty = bar_width.saturating_sub(filled);
                    let bar_color = if usage_ratio < 0.5 {
                        THEME.load().border_active
                    } else if usage_ratio < 0.75 {
                        THEME.load().status_streaming
                    } else {
                        THEME.load().error_color
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
            Span::styled("\u{03b8}:", Style::default().fg(THEME.load().muted)),
            Span::styled(thinking.to_string(), Style::default().fg(THEME.load().help_fg)),
            Span::styled(" \u{2502} ", Style::default().fg(THEME.load().border)),
            Span::styled(model, Style::default().fg(THEME.load().header_fg)),
            Span::styled(" ", Style::default()),
        ]))
        .alignment(Alignment::Right)
        .style(Style::default().bg(THEME.load().bg));
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

        if let Some(ref state) = app.settings {
            let snap = crate::settings::RuntimeSnapshot::from_runtime(runtime, registry);
            crate::settings::render(frame, frame.area(), state, &snap);
        }
        if let Some(ref state) = app.plugins {
            crate::plugins::render(frame, frame.area(), state);
        }
    })?;
    Ok(())
}
