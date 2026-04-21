//! Message rendering — converts ChatMessage variants into styled ratatui Lines.
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use super::app::{App, ChatMessage, SPINNER_FRAMES};
use super::theme::THEME;
use super::highlight::{highlight_tool_code, highlight_bash_output, highlight_read_output, try_highlight_grep_line, is_read_tool_output};
use super::markdown::{render_markdown, wrap_text};
use super::draw::{bash_trace, format_tool_name};

impl App {
    pub(crate) fn render_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines: Vec<Line> = Vec::new();
        let m = "   "; // margin

        for (i, tmsg) in self.messages.iter().enumerate() {
            let ts = &tmsg.time;
            match &tmsg.msg {
                ChatMessage::User(text) => {
                    let bg = Style::default().bg(THEME.load().user_bg);
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
                            Style::default().fg(THEME.load().user_color).bg(THEME.load().user_bg).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(ts_str, Style::default().fg(THEME.load().muted).bg(THEME.load().user_bg)),
                    ]));
                    // Content — just render the text (pasted messages already contain "[Pasted N lines]")
                    let style = Style::default().fg(THEME.load().user_color).bg(THEME.load().user_bg);
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
                    // Only add spacing if previous message wasn't a User block
                    // (User blocks already have bottom margin)
                    let prev_was_user = i > 0 && matches!(&self.messages[i - 1].msg, ChatMessage::User(_));
                    if !prev_was_user {
                        lines.push(Line::from(""));
                    }
                    let dim = Style::default().fg(THEME.load().thinking_color);
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
                    // After thinking: just a single blank line (no separator)
                    let prev_was_thinking = i > 0 && matches!(&self.messages[i - 1].msg, ChatMessage::Thinking(_));
                    if prev_was_thinking {
                        lines.push(Line::from(""));
                    } else if i > 0 {
                        lines.push(Line::from(""));
                        let sep_total = width.min(40);
                        let sep_half = sep_total / 2;
                        let sep_left: String = "\u{2500}".repeat(sep_half.saturating_sub(2));
                        let sep_right: String = "\u{2500}".repeat(sep_half.saturating_sub(2));
                        let sep_content_width = sep_left.chars().count() + 3 + sep_right.chars().count();
                        let pad_left = width.saturating_sub(sep_content_width) / 2;
                        lines.push(Line::from(vec![
                            Span::styled(" ".repeat(pad_left), Style::default()),
                            Span::styled(sep_left, Style::default().fg(THEME.load().separator)),
                            Span::styled(" \u{00b7} ", Style::default().fg(Color::Rgb(35, 55, 75))),
                            Span::styled(sep_right, Style::default().fg(THEME.load().separator)),
                        ]));
                        lines.push(Line::from(""));
                    }
                    // Header
                    let label = format!("{}\u{25c8} agent", m);
                    let ts_str = format!("{} ", ts);
                    let gap = width.saturating_sub(label.chars().count() + ts_str.chars().count());
                    // Pulse the agent label when streaming (same sin-wave as header dot)
                    let label_color = if self.streaming && i == self.messages.len() - 1 {
                        let pulse = ((self.spinner_frame as f64 / 20.0).sin() * 0.3 + 0.7).max(0.4);
                        if let Color::Rgb(r, g, b) = THEME.load().claude_label {
                            Color::Rgb(
                                (r as f64 * pulse) as u8,
                                (g as f64 * pulse) as u8,
                                (b as f64 * pulse) as u8,
                            )
                        } else {
                            THEME.load().claude_label
                        }
                    } else {
                        THEME.load().claude_label
                    };
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("{}{}", label, " ".repeat(gap)),
                            Style::default().fg(label_color).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(ts_str, Style::default().fg(THEME.load().muted)),
                    ]));
                    // Body
                    if text.is_empty() {
                        lines.push(Line::from(Span::styled(
                            format!("{}   \u{2026}", m), Style::default().fg(THEME.load().muted),
                        )));
                    } else {
                        lines.extend(render_markdown(text, m, width));
                    }
                }

                ChatMessage::ToolUseStart(tool_name, partial_input) => {
                    // Breathing room before tool block
                    lines.push(Line::from(""));
                    let (icon, display_name, server_tag) = format_tool_name(tool_name);
                    let mut header = vec![
                        Span::styled(format!("{}   {} ", m, icon), Style::default().fg(THEME.load().tool_label)),
                        Span::styled(display_name, Style::default().fg(THEME.load().tool_label).add_modifier(Modifier::BOLD)),
                    ];
                    if let Some(tag) = server_tag {
                        header.push(Span::styled(format!(" [{}]", tag), Style::default().fg(THEME.load().muted)));
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
                            Style::default().fg(THEME.load().status_streaming).add_modifier(Modifier::DIM),
                        ));
                    }
                    lines.push(Line::from(header));
                    // Show accumulated partial input with newlines rendered
                    if !partial_input.is_empty() {
                        let param_style = Style::default().fg(THEME.load().tool_param);
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
                            lines.push(Line::from(Span::styled(omit, Style::default().fg(THEME.load().muted))));
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
                    // Breathing room before tool block
                    lines.push(Line::from(""));
                    // Compact tool header
                    let (icon, display_name, server_tag) = format_tool_name(tool_name);
                    let mut header = vec![
                        Span::styled(format!("{}   {} ", m, icon), Style::default().fg(THEME.load().tool_label)),
                        Span::styled(display_name, Style::default().fg(THEME.load().tool_label).add_modifier(Modifier::BOLD)),
                    ];
                    if let Some(tag) = server_tag {
                        header.push(Span::styled(format!(" [{}]", tag), Style::default().fg(THEME.load().muted)));
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
                                Style::default().fg(THEME.load().status_streaming).add_modifier(Modifier::DIM),
                            ));
                        }
                    }
                    lines.push(Line::from(header));
                    // Params — key:value on one line each, dimmed
                    let param_style = Style::default().fg(THEME.load().tool_param);
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
                                            _ => ("│", THEME.load().muted),
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
                                            let hl_lines = highlight_tool_code(&content_lines[..show], &file_ext, m, marker, marker_color);
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
                                            lines.push(Line::from(Span::styled(omit, Style::default().fg(THEME.load().muted))));
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
                        Style::default().fg(THEME.load().error_color)
                    } else if is_timeout {
                        Style::default().fg(THEME.load().warning_color)
                    } else {
                        Style::default().fg(THEME.load().tool_result_color)
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
                                    Style::default().fg(THEME.load().warning_color),
                                ),
                                Span::styled(
                                    elapsed_str,
                                    Style::default().fg(THEME.load().subagent_time),
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
                                    Style::default().fg(THEME.load().status_streaming).add_modifier(Modifier::DIM),
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
                                    Style::default().fg(THEME.load().tool_result_ok),
                                ),
                                Span::styled(
                                    elapsed_str,
                                    Style::default().fg(THEME.load().subagent_time),
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
                        highlight_read_output(&result_lines[..show], &ext, m)
                    } else if preceding_tool.as_deref() == Some("bash") {
                        Some(highlight_bash_output(&result_lines[..show], m))
                    } else {
                        None
                    };

                    if let Some(hl_lines) = highlighted_lines {
                        if !is_error && !is_timeout {
                            for hl_line in hl_lines {
                                let dimmed_spans: Vec<Span> = hl_line.spans.into_iter().map(|span| {
                                    Span::styled(span.content, span.style.add_modifier(Modifier::DIM))
                                }).collect();
                                lines.push(Line::from(dimmed_spans));
                            }
                        } else {
                            lines.extend(hl_lines);
                        }
                    } else {
                        for line in &result_lines[..show] {
                            // Try to detect and highlight grep output (skip for timeout/error)
                            if !is_timeout && !is_error {
                                if let Some(grep_spans) = try_highlight_grep_line(line, m) {
                                    lines.push(Line::from(grep_spans));
                                    continue;
                                }
                            }
                            let full = format!("{}       {}", m, line);
                            for wline in wrap_text(&full, width) {
                                let body_style = if is_error || is_timeout { style } else { style.add_modifier(Modifier::DIM) };
                                lines.push(Line::from(Span::styled(wline, body_style)));
                            }
                        }
                    }
                    if result_lines.len() > show {
                        lines.push(Line::from(Span::styled(
                            format!("{}       +{} lines", m, result_lines.len() - show),
                            Style::default().fg(THEME.load().muted),
                        )));
                    }
                }

                ChatMessage::Error(err) => {
                    lines.push(Line::from(vec![
                        Span::styled(format!("{}  \u{2718} ", m), Style::default().fg(THEME.load().error_color)),
                        Span::styled(err.clone(), Style::default().fg(THEME.load().error_color)),
                    ]));
                }

                ChatMessage::System(msg) => {
                    lines.push(Line::from(Span::styled(
                        format!("{}  {}", m, msg),
                        Style::default().fg(THEME.load().muted).add_modifier(Modifier::DIM),
                    )));
                }

                ChatMessage::Event { source, severity, text } => {
                    let theme = THEME.load();
                    let (icon, sev_color) = match severity.as_str() {
                        "critical" => ("🔴", theme.event_critical),
                        "high"     => ("🟠", theme.event_icon),
                        "medium"   => ("🟡", theme.event_icon),
                        "low"      => ("🔵", theme.event_source),
                        _          => ("📨", theme.event_icon),
                    };
                    lines.push(Line::from(vec![
                        Span::styled(format!("{}  {} ", m, icon), Style::default().fg(sev_color)),
                        Span::styled(format!("[{}] ", source), Style::default().fg(theme.event_source).add_modifier(Modifier::BOLD)),
                        Span::styled(text.clone(), Style::default().fg(theme.event_text)),
                    ]));
                }
            }
        }

        lines
    }
}
