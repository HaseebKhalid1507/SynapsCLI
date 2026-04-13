use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use std::sync::LazyLock;

use super::theme::THEME;

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(|| SyntaxSet::load_defaults_newlines());
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(|| ThemeSet::load_defaults());

/// Highlight a code block using syntect
pub(crate) fn highlight_code_block(code: &str, lang: &str, prefix: &str) -> Vec<Line<'static>> {
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
            Style::default().fg(THEME.muted),
        ));
        for (style, text) in ranges {
            let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
            let content = text.trim_end_matches('\n').to_string();
            if !content.is_empty() {
                spans.push(Span::styled(content, Style::default().fg(fg).bg(THEME.code_bg)));
            }
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// Try to syntax-highlight a single tool output line.
/// Highlight code lines for tool params (write content, edit old/new) — clean style matching read output
pub(crate) fn highlight_tool_code(lines: &[&str], ext: &str, margin: &str, marker: &str, marker_color: Color) -> Vec<Line<'static>> {
    let ss = &*SYNTAX_SET;
    let ts = &*THEME_SET;
    let theme = &ts.themes["base16-ocean.dark"];

    let syntax = ss.find_syntax_by_extension(ext)
        .unwrap_or_else(|| ss.find_syntax_plain_text());

    let mut h = HighlightLines::new(syntax, theme);
    let mut result = Vec::new();

    // Determine tint based on marker (red for old, green for new, neutral for content)
    let tint = match marker {
        "−" => (40i16, -60i16, -60i16),     // shift toward red: boost red, crush green/blue
        "+" => (-15i16, 10i16, -15i16),      // shift toward green: reduce red/blue
        _ => (0i16, 0i16, 0i16),             // neutral for write content
    };

    for (i, line) in lines.iter().enumerate() {
        let code_with_nl = format!("{}\n", line);
        let ranges = h.highlight_line(&code_with_nl, ss).unwrap_or_default();

        let mut spans = vec![
            Span::styled(
                format!("{}    {:>3} {} ", margin, i + 1, marker),
                Style::default().fg(marker_color),
            ),
        ];
        for (sty, text) in ranges {
            let r = (sty.foreground.r as i16 + tint.0).clamp(0, 255) as u8;
            let g = (sty.foreground.g as i16 + tint.1).clamp(0, 255) as u8;
            let b = (sty.foreground.b as i16 + tint.2).clamp(0, 255) as u8;
            let fg = Color::Rgb(r, g, b);
            let content = text.trim_end_matches('\n').to_string();
            if !content.is_empty() {
                spans.push(Span::styled(content, Style::default().fg(fg)));
            }
        }
        result.push(Line::from(spans));
    }

    result
}

/// Highlight bash tool output with blue tint and pattern detection
pub(crate) fn highlight_bash_output(lines: &[&str], margin: &str) -> Vec<Line<'static>> {
    let mut result = Vec::new();

    for raw_line in lines {
        // Replace tabs with spaces — ratatui doesn't handle \t correctly and causes overlap artifacts
        let line = raw_line.replace('\t', "    ");
        let trimmed = line.trim();
        let mut spans = vec![
            Span::styled(format!("{}       ", margin), Style::default().fg(THEME.tool_result_color)),
        ];

        if trimmed.is_empty() {
            result.push(Line::from(spans));
            continue;
        }

        // Detect patterns and colorize
        if trimmed.starts_with("error") || trimmed.starts_with("Error") || trimmed.starts_with("ERROR")
            || trimmed.starts_with("fatal") || trimmed.starts_with("FATAL") {
            // Errors → red
            spans.push(Span::styled(line.to_string(), Style::default().fg(THEME.error_color)));
        } else if trimmed.starts_with("warning") || trimmed.starts_with("Warning") || trimmed.starts_with("WARN") {
            // Warnings → yellow
            spans.push(Span::styled(line.to_string(), Style::default().fg(THEME.warning_color)));
        } else if trimmed.starts_with("✅") || trimmed.starts_with("ok") || trimmed.starts_with("OK")
            || trimmed.starts_with("done") || trimmed.starts_with("Done") || trimmed.starts_with("success") {
            // Success → green with blue tint
            spans.push(Span::styled(line.to_string(), Style::default().fg(THEME.tool_result_ok)));
        } else {
            // Default: blue-tinted with smart coloring
            let mut remaining = line.as_str();
            while !remaining.is_empty() {
                // Find paths (contain /)
                if let Some(slash_pos) = remaining.find('/') {
                    // Output text before the path
                    if slash_pos > 0 {
                        let before = &remaining[..slash_pos];
                        // Find the start of the path (walk back to whitespace)
                        let path_start = before.rfind(|c: char| c.is_whitespace()).map(|p| p + 1).unwrap_or(0);
                        if path_start > 0 {
                            spans.push(Span::styled(
                                remaining[..path_start].to_string(),
                                Style::default().fg(THEME.tool_result_color),
                            ));
                        }
                        // Path portion
                        let after_slash = &remaining[path_start..];
                        let path_end = after_slash.find(|c: char| c.is_whitespace() || c == ':' || c == ')' || c == ']')
                            .unwrap_or(after_slash.len());
                        // Guard: if path_end is 0, we'd loop forever — consume at least 1 char
                        if path_end == 0 {
                            spans.push(Span::styled(
                                after_slash[..1].to_string(),
                                Style::default().fg(THEME.tool_result_color),
                            ));
                            remaining = &after_slash[1..];
                        } else {
                            spans.push(Span::styled(
                                after_slash[..path_end].to_string(),
                                Style::default().fg(THEME.tool_label),
                            ));
                            remaining = &after_slash[path_end..];
                        }
                    } else {
                        let path_end = remaining.find(|c: char| c.is_whitespace() || c == ':' || c == ')' || c == ']')
                            .unwrap_or(remaining.len());
                        // Guard: if path_end is 0, consume at least 1 char to avoid infinite loop
                        if path_end == 0 {
                            spans.push(Span::styled(
                                remaining[..1].to_string(),
                                Style::default().fg(THEME.tool_result_color),
                            ));
                            remaining = &remaining[1..];
                        } else {
                            spans.push(Span::styled(
                                remaining[..path_end].to_string(),
                                Style::default().fg(THEME.tool_label),
                            ));
                            remaining = &remaining[path_end..];
                        }
                    }
                } else {
                    // No more paths — output the rest with blue tint
                    spans.push(Span::styled(
                        remaining.to_string(),
                        Style::default().fg(THEME.tool_result_color),
                    ));
                    break;
                }
            }
        }

        result.push(Line::from(spans));
    }

    result
}

/// Try to highlight a grep output line (filepath:linenum:content)
pub(crate) fn try_highlight_grep_line(line: &str, margin: &str) -> Option<Vec<Span<'static>>> {
    // Grep format: filepath:linenum:content  or  filepath-linenum-content (context)
    // Also: filepath:linenum:  (empty match line)
    let first_colon = line.find(':')?;
    let filepath = &line[..first_colon];

    // Filepath should look like a path (contain / or .)
    if !filepath.contains('/') && !filepath.contains('.') {
        return None;
    }

    let rest = &line[first_colon + 1..];
    let second_sep = rest.find(|c: char| c == ':' || c == '-')?;
    let linenum = &rest[..second_sep];

    // Line number should be numeric
    if !linenum.chars().all(|c| c.is_ascii_digit()) || linenum.is_empty() {
        return None;
    }

    let sep_char = rest.as_bytes()[second_sep] as char;
    let content = if second_sep + 1 < rest.len() { &rest[second_sep + 1..] } else { "" };

    let is_context = sep_char == '-';

    Some(vec![
        Span::styled(format!("{}       ", margin), Style::default().fg(THEME.tool_result_color)),
        Span::styled(filepath.to_string(), Style::default().fg(THEME.tool_label)),
        Span::styled(":", Style::default().fg(THEME.muted)),
        Span::styled(linenum.to_string(), Style::default().fg(THEME.list_bullet_color)),
        Span::styled(format!("{}", sep_char), Style::default().fg(THEME.muted)),
        Span::styled(
            content.to_string(),
            if is_context {
                Style::default().fg(THEME.muted)
            } else {
                Style::default().fg(THEME.tool_result_color)
            },
        ),
    ])
}

/// Check if tool output looks like read tool output (line-numbered with tabs)
pub(crate) fn is_read_tool_output(lines: &[&str]) -> bool {
    if lines.is_empty() { return false; }
    // Check first few non-empty lines for "number\tcontent" pattern
    let mut checked = 0;
    let mut matches = 0;
    for line in lines.iter().take(10) {
        if line.trim().is_empty() { continue; }
        checked += 1;
        if let Some(tab_idx) = line.find('\t') {
            if line[..tab_idx].trim().chars().all(|c| c.is_ascii_digit()) && !line[..tab_idx].trim().is_empty() {
                matches += 1;
            }
        }
    }
    checked > 0 && matches * 2 >= checked // At least half the lines match
}

/// Highlight read tool output as a code block using syntect
pub(crate) fn highlight_read_output(lines: &[&str], ext: &str, margin: &str) -> Option<Vec<Line<'static>>> {
    let ss = &*SYNTAX_SET;
    let ts = &*THEME_SET;
    let theme = &ts.themes["base16-ocean.dark"];

    let syntax = if !ext.is_empty() {
        ss.find_syntax_by_extension(ext).unwrap_or_else(|| ss.find_syntax_plain_text())
    } else {
        ss.find_syntax_plain_text()
    };

    // If plain text, don't bother highlighting
    if syntax.name == "Plain Text" && ext.is_empty() {
        return None;
    }

    let mut h = HighlightLines::new(syntax, theme);
    let mut result = Vec::new();

    for line in lines {
        let (line_num, code) = if let Some(tab_idx) = line.find('\t') {
            let num = line[..tab_idx].trim();
            let code = &line[tab_idx + 1..];
            (num.to_string(), code)
        } else {
            (String::new(), *line)
        };

        let code_with_nl = format!("{}\n", code);
        let ranges = h.highlight_line(&code_with_nl, ss).unwrap_or_default();

        let mut spans = vec![
            Span::styled(
                format!("{}     {:>4} \u{2502} ", margin, line_num),
                Style::default().fg(THEME.muted),
            ),
        ];
        for (sty, text) in ranges {
            // Slight cool tint for read output to differentiate from edit
            let r = (sty.foreground.r as i16 - 5).clamp(0, 255) as u8;
            let g = (sty.foreground.g as i16).clamp(0, 255) as u8;
            let b = (sty.foreground.b as i16 + 10).clamp(0, 255) as u8;
            let fg = Color::Rgb(r, g, b);
            let content = text.trim_end_matches('\n').to_string();
            if !content.is_empty() {
                spans.push(Span::styled(content, Style::default().fg(fg)));
            }
        }
        result.push(Line::from(spans));
    }

    Some(result)
}

