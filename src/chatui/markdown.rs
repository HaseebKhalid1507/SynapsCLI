use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use super::theme::THEME;
use super::highlight::highlight_code_block;

pub(crate) fn parse_inline_md(text: &str, base_style: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut chars = text.chars().peekable();
    let mut buf = String::new();

    let bold_style = base_style.add_modifier(Modifier::BOLD);
    let italic_style = base_style.add_modifier(Modifier::ITALIC);
    let code_style = Style::default().fg(THEME.load().code_fg).bg(THEME.load().code_bg);

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

/// Render a markdown table into styled Lines.
///
/// No outer border — clean, breathable layout. Header row is bold with a
/// single ─ rule underneath. For tables >6 rows a faint rule appears every
/// 5th body row for readability. Columns are padded to the widest cell.
pub(crate) fn render_table(table_lines: &[String], prefix: &str, width: usize) -> Vec<Line<'static>> {
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

    // Calculate column widths using display width
    let mut col_widths: Vec<usize> = vec![3; num_cols];
    for row in &rows {
        for (j, cell) in row.iter().enumerate() {
            if j < num_cols {
                col_widths[j] = col_widths[j].max(UnicodeWidthStr::width(cell.as_str()));
            }
        }
    }

    // Shrink columns if total table width exceeds terminal width
    let prefix_overhead = UnicodeWidthStr::width(prefix) + 2; // prefix + "  "
    let per_col_overhead = 3; // " " + cell + " " + gap
    let total_table_width = prefix_overhead + col_widths.iter().sum::<usize>() + num_cols * per_col_overhead;
    if width > 0 && total_table_width > width {
        let available = width.saturating_sub(prefix_overhead + num_cols * per_col_overhead);
        if available > 0 && num_cols > 0 {
            let total_content: usize = col_widths.iter().sum();
            if total_content > available {
                let mut new_widths: Vec<usize> = col_widths.iter()
                    .map(|&w| (w * available / total_content).max(3))
                    .collect();
                let used: usize = new_widths.iter().sum();
                if used < available {
                    let mut remainder = available - used;
                    let mut indices: Vec<usize> = (0..num_cols).collect();
                    indices.sort_by(|a, b| col_widths[*b].cmp(&col_widths[*a]));
                    for &idx in &indices {
                        if remainder == 0 { break; }
                        new_widths[idx] += 1;
                        remainder -= 1;
                    }
                }
                col_widths = new_widths;
            }
        }
    }

    let border_style = Style::default().fg(THEME.load().table_border_color);
    let header_style = Style::default().fg(THEME.load().table_header_color).add_modifier(Modifier::BOLD);
    let cell_style = Style::default().fg(THEME.load().table_cell_color);

    // No top border — let it breathe
    result.push(Line::from("")); // spacing above table

    let body_start = if has_header { 1 } else { 0 };
    let body_count = rows.len().saturating_sub(body_start);

    for (i, row) in rows.iter().enumerate() {
        // Data row: padded cells separated by spaces (no vertical borders)
        let mut spans: Vec<Span> = Vec::new();
        spans.push(Span::styled(format!("{}  ", prefix), Style::default()));

        for (j, cell) in row.iter().enumerate() {
            let w = col_widths[j];
            let display_w = UnicodeWidthStr::width(cell.as_str());
            let display_cell = if display_w > w {
                let mut truncated = String::new();
                let mut tw = 0;
                for ch in cell.chars() {
                    let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                    if tw + cw > w.saturating_sub(1) { break; }
                    truncated.push(ch);
                    tw += cw;
                }
                truncated.push('\u{2026}');
                truncated
            } else {
                cell.clone()
            };
            let truncated_w = UnicodeWidthStr::width(display_cell.as_str());
            let padding = w.saturating_sub(truncated_w);
            let padded = format!(" {}{} ", display_cell, " ".repeat(padding));
            let style = if has_header && i == 0 { header_style } else { cell_style };
            spans.push(Span::styled(padded, style));
            // Light separator between columns (not after last)
            if j < num_cols - 1 {
                spans.push(Span::styled(" ", Style::default()));
            }
        }
        result.push(Line::from(spans));

        // After header row, draw a single ─ rule
        if has_header && i == 0 {
            let rule_width: usize = col_widths.iter().sum::<usize>() + num_cols * 3;
            let sep = format!("{}  {}", prefix, "\u{2500}".repeat(rule_width.min(width.saturating_sub(prefix.len() + 2))));
            result.push(Line::from(Span::styled(sep, border_style)));
        }

        // For tables with >6 rows, add a faint rule every 5th body row
        if has_header && i > 0 && body_count > 6 {
            let body_idx = i - body_start; // 0-indexed body row
            if body_idx > 0 && body_idx % 5 == 0 && i < rows.len() - 1 {
                let rule_width: usize = col_widths.iter().sum::<usize>() + num_cols * 3;
                let sep = format!("{}  {}", prefix, "\u{2500}".repeat(rule_width.min(width.saturating_sub(prefix.len() + 2))));
                result.push(Line::from(Span::styled(
                    sep,
                    Style::default().fg(THEME.load().table_border_color).add_modifier(Modifier::DIM),
                )));
            }
        } else if !has_header && body_count > 6 {
            // No header case — stripe from row 0
            if i > 0 && i % 5 == 0 && i < rows.len() - 1 {
                let rule_width: usize = col_widths.iter().sum::<usize>() + num_cols * 3;
                let sep = format!("{}  {}", prefix, "\u{2500}".repeat(rule_width.min(width.saturating_sub(prefix.len() + 2))));
                result.push(Line::from(Span::styled(
                    sep,
                    Style::default().fg(THEME.load().table_border_color).add_modifier(Modifier::DIM),
                )));
            }
        }
    }

    // No bottom border — let it breathe
    result.push(Line::from("")); // spacing below table

    result
}

/// Render markdown text into Lines, handling code blocks, headings, lists, quotes, tables
pub(crate) fn render_markdown(text: &str, prefix: &str, width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    let base_style = Style::default().fg(THEME.load().claude_text);
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
                code_lang = trimmed.strip_prefix("```").unwrap_or("").trim().to_string();
                code_buf.clear();
            } else {
                // End of code block — render with language tag chip + border frame
                let border_style = Style::default().fg(THEME.load().border);
                let lang_style = Style::default().fg(THEME.load().muted).add_modifier(Modifier::DIM);

                // Calculate block width: use a reasonable portion of available width
                let block_inner_width = width.saturating_sub(prefix.len() + 4); // prefix + "  " + borders
                let rule_width = block_inner_width.min(60).max(20);

                // Top rule with optional language tag
                lines.push(Line::from("")); // breathing room above code block
                if !code_lang.is_empty() {
                    // Language label line (above the top rule)
                    let lang_upper = code_lang.to_uppercase();
                    let spaced: String = lang_upper.chars()
                        .enumerate()
                        .map(|(i, c)| if i > 0 { format!(" {}", c) } else { c.to_string() })
                        .collect();
                    lines.push(Line::from(vec![
                        Span::styled(format!("{}  ", prefix), Style::default()),
                        Span::styled(spaced, lang_style),
                    ]));
                }
                // Top border rule
                lines.push(Line::from(Span::styled(
                    format!("{}  {}", prefix, "\u{2500}".repeat(rule_width)),
                    border_style,
                )));

                // Code body (highlight_code_block already adds prefix + │ per line)
                for hl_line in highlight_code_block(&code_buf, &code_lang, prefix) {
                    lines.push(super::highlight::clamp_line(hl_line, width));
                }

                // Bottom border rule
                lines.push(Line::from(Span::styled(
                    format!("{}  {}", prefix, "\u{2500}".repeat(rule_width)),
                    border_style,
                )));

                lines.push(Line::from("")); // breathing room below code block
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
            // Spacing above heading (unless it's the first line)
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            let full = format!("{}  {}", prefix, heading_text);
            for wline in wrap_text(&full, width) {
                lines.push(Line::from(Span::styled(
                    wline,
                    Style::default().fg(THEME.load().heading_color).add_modifier(Modifier::BOLD),
                )));
            }
            continue;
        }

        // Blockquotes
        if trimmed.starts_with('>') {
            let quote_text = trimmed.strip_prefix('>').unwrap_or("").trim();
            let full = format!("{}  \u{2502} {}", prefix, quote_text);
            for wline in wrap_text(&full, width) {
                lines.push(Line::from(Span::styled(wline, Style::default().fg(THEME.load().quote_color).add_modifier(Modifier::ITALIC))));
            }
            continue;
        }

        // List items
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            let item_text = &trimmed[2..];
            let bullet_prefix = format!("{}  \u{2022} ", prefix);
            let cont_prefix = format!("{}    ", prefix);
            let flat = format!("{}{}", bullet_prefix, item_text);
            if flat.chars().count() <= width {
                let bullet_span = Span::styled(bullet_prefix, Style::default().fg(THEME.load().list_bullet_color));
                let mut item_spans = parse_inline_md(item_text, base_style);
                let mut all_spans = vec![bullet_span];
                all_spans.append(&mut item_spans);
                lines.push(Line::from(all_spans));
            } else {
                for (li, wline) in wrap_text(&flat, width).into_iter().enumerate() {
                    if li == 0 {
                        let inner = if wline.starts_with(&bullet_prefix) {
                            &wline[bullet_prefix.len()..]
                        } else {
                            &wline
                        };
                        let bullet_span = Span::styled(bullet_prefix.clone(), Style::default().fg(THEME.load().list_bullet_color));
                        let mut all_spans = vec![bullet_span];
                        all_spans.extend(parse_inline_md(inner, base_style));
                        lines.push(Line::from(all_spans));
                    } else {
                        let mut all_spans = vec![Span::styled(cont_prefix.clone(), base_style)];
                        all_spans.extend(parse_inline_md(wline.trim_start(), base_style));
                        lines.push(Line::from(all_spans));
                    }
                }
            }
            continue;
        }

        // Numbered lists
        if trimmed.len() > 2 {
            let num_end = trimmed.find(". ");
            if let Some(pos) = num_end {
                if pos <= 3 && trimmed[..pos].chars().all(|c| c.is_ascii_digit()) {
                    let item_text = &trimmed[pos + 2..];
                    let num_prefix = format!("{}  {}. ", prefix, &trimmed[..pos]);
                    let cont_prefix = format!("{}     ", prefix);
                    let flat = format!("{}{}", num_prefix, item_text);
                    if flat.chars().count() <= width {
                        let num_span = Span::styled(num_prefix, Style::default().fg(THEME.load().list_bullet_color));
                        let mut item_spans = parse_inline_md(item_text, base_style);
                        let mut all_spans = vec![num_span];
                        all_spans.append(&mut item_spans);
                        lines.push(Line::from(all_spans));
                    } else {
                        for (li, wline) in wrap_text(&flat, width).into_iter().enumerate() {
                            if li == 0 {
                                let inner = if wline.starts_with(&num_prefix) {
                                    &wline[num_prefix.len()..]
                                } else {
                                    &wline
                                };
                                let num_span = Span::styled(num_prefix.clone(), Style::default().fg(THEME.load().list_bullet_color));
                                let mut all_spans = vec![num_span];
                                all_spans.extend(parse_inline_md(inner, base_style));
                                lines.push(Line::from(all_spans));
                            } else {
                                let mut all_spans = vec![Span::styled(cont_prefix.clone(), base_style)];
                                all_spans.extend(parse_inline_md(wline.trim_start(), base_style));
                                lines.push(Line::from(all_spans));
                            }
                        }
                    }
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
            // wrap_text carries the leading indent forward on continuation lines
            for wline in wrap_text(&full, width) {
                let (prefix_part, inner) = if wline.starts_with(&full_prefix) {
                    (full_prefix.clone(), &wline[full_prefix.len()..])
                } else {
                    // Continuation line — extract the indent wrap_text added
                    let indent_len = wline.chars().take_while(|c| *c == ' ').count();
                    let indent: String = " ".repeat(indent_len);
                    let rest = &wline[indent.len()..];
                    (indent, rest)
                };
                let parsed = parse_inline_md(inner, base_style);
                let mut line_spans = vec![Span::styled(prefix_part, base_style)];
                line_spans.extend(parsed);
                lines.push(Line::from(line_spans));
            }
        }
    }

    lines
}

#[allow(unused_assignments)]
pub(crate) fn wrap_text(raw_text: &str, width: usize) -> Vec<String> {
    let text = raw_text.replace('\t', "    ");
    if width == 0 || text.chars().count() <= width {
        return vec![text];
    }

    // Detect leading whitespace to carry forward on continuation lines.
    // This prevents wrapped text from bleeding to column 0 and leaving
    // scroll artifacts in the TUI.
    let indent_len = text.chars().take_while(|c| *c == ' ').count();
    let indent: String = " ".repeat(indent_len);
    let wrap_width = width.saturating_sub(indent_len);

    let mut lines = Vec::new();
    let mut current = String::new();
    let mut is_first_line = true;

    for word in text.split_inclusive(' ') {
        let wlen = word.chars().count();
        let col = current.chars().count();
        let effective_width = if is_first_line { width } else { wrap_width };
        if col + wlen > effective_width && col > 0 {
            lines.push(current.trim_end().to_string());
            current = indent.clone();
            is_first_line = false;
        }
        // Word longer than effective width — hard break it
        let effective_width = if is_first_line { width } else { wrap_width };
        if wlen > effective_width {
            let chars: Vec<char> = word.chars().collect();
            let chunk_size = effective_width.max(1); // Prevent panic on zero-width terminal
            for chunk in chars.chunks(chunk_size) {
                if !current.is_empty() && current != indent {
                    lines.push(current.trim_end().to_string());
                    current = indent.clone();
                    is_first_line = false;
                }
                current.push_str(&chunk.iter().collect::<String>());
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

pub(crate) fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 { format!("{:.1}M", n as f64 / 1_000_000.0) }
    else if n >= 1_000 { format!("{:.1}k", n as f64 / 1_000.0) }
    else { format!("{}", n) }
}
