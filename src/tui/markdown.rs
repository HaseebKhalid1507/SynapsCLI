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

/// Strip inline markdown markers (`**`, `*`, `` ` ``) from text for width calculation.
/// Returns the visible text without formatting markers.
fn strip_inline_md(text: &str) -> String {
    let mut result = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '`' => {
                // Skip backtick, consume until closing backtick
                while let Some(&c) = chars.peek() {
                    if c == '`' { chars.next(); break; }
                    result.push(c);
                    chars.next();
                }
            }
            '*' => {
                let is_bold = chars.peek() == Some(&'*');
                if is_bold { chars.next(); } // skip second *
                // Consume content until closing delimiter
                let mut found_end = false;
                while let Some(c) = chars.next() {
                    if c == '*' {
                        if is_bold {
                            if chars.peek() == Some(&'*') { chars.next(); found_end = true; break; }
                            result.push(c);
                        } else {
                            found_end = true;
                            break;
                        }
                    } else {
                        result.push(c);
                    }
                }
                let _ = found_end;
            }
            _ => result.push(ch),
        }
    }
    result
}

/// Word-wrap a table cell's content to fit within `max_width` display columns.
/// Breaks on word boundaries where possible; forces a break mid-word if a single
/// word exceeds the column width. Returns one String per visual line.
/// Width calculations strip inline markdown markers so `**bold**` counts as
/// the width of `bold`, not `**bold**`.
fn wrap_cell(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![String::new()];
    }
    let stripped = strip_inline_md(text);
    let display_w = UnicodeWidthStr::width(stripped.as_str());
    if display_w <= max_width {
        return vec![text.to_string()];
    }

    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_w: usize = 0;

    for word in text.split_whitespace() {
        let word_w = UnicodeWidthStr::width(word);
        if current.is_empty() {
            // First word on this line
            if word_w <= max_width {
                current.push_str(word);
                current_w = word_w;
            } else {
                // Word is wider than column — force-break it char by char
                for ch in word.chars() {
                    let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                    if current_w + ch_w > max_width && !current.is_empty() {
                        lines.push(current);
                        current = String::new();
                        current_w = 0;
                    }
                    current.push(ch);
                    current_w += ch_w;
                }
            }
        } else if current_w + 1 + word_w <= max_width {
            // Fits on current line with a space
            current.push(' ');
            current.push_str(word);
            current_w += 1 + word_w;
        } else {
            // Doesn't fit — start a new line
            lines.push(current);
            current = String::new();
            current_w = 0;
            if word_w <= max_width {
                current.push_str(word);
                current_w = word_w;
            } else {
                // Force-break long word
                for ch in word.chars() {
                    let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                    if current_w + ch_w > max_width && !current.is_empty() {
                        lines.push(current);
                        current = String::new();
                        current_w = 0;
                    }
                    current.push(ch);
                    current_w += ch_w;
                }
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
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

    // Calculate column widths using display width of stripped content
    // (markdown formatting like **bold** shouldn't count toward column width)
    let mut col_widths: Vec<usize> = vec![3; num_cols];
    for row in &rows {
        for (j, cell) in row.iter().enumerate() {
            if j < num_cols {
                let stripped = strip_inline_md(cell);
                col_widths[j] = col_widths[j].max(UnicodeWidthStr::width(stripped.as_str()));
            }
        }
    }

    // Shrink columns if total table width exceeds terminal width.
    // Strategy: lock narrow columns at their natural width (they're already
    // compact), then distribute the remaining budget among the wide columns.
    // This prevents short fields like "✅" or "5.0s" from being crushed to
    // "…" while giving long-text columns as much room as possible.
    let prefix_overhead = UnicodeWidthStr::width(prefix) + 2; // prefix + "  "
    let per_col_overhead = 3; // " " + cell + " " + gap
    let total_table_width = prefix_overhead + col_widths.iter().sum::<usize>() + num_cols * per_col_overhead;
    if width > 0 && total_table_width > width {
        let available = width.saturating_sub(prefix_overhead + num_cols * per_col_overhead);
        if available > 0 && num_cols > 0 {
            let total_content: usize = col_widths.iter().sum();
            if total_content > available {
                // Phase 1: columns that fit in ≤ 12 chars keep their natural
                // width — they're already tight and truncating them destroys
                // readability. Everything else is "shrinkable".
                let narrow_threshold = 12;
                let mut new_widths = col_widths.clone();
                let mut locked_total: usize = 0;
                let mut shrinkable_total: usize = 0;
                let mut shrinkable_indices: Vec<usize> = Vec::new();

                for (i, &w) in col_widths.iter().enumerate() {
                    if w <= narrow_threshold {
                        locked_total += w;
                    } else {
                        shrinkable_indices.push(i);
                        shrinkable_total += w;
                    }
                }

                let budget_for_shrinkable = available.saturating_sub(locked_total);

                if shrinkable_indices.is_empty() || budget_for_shrinkable == 0 {
                    // All columns are narrow or no budget left — fall back to
                    // proportional shrink across everything.
                    new_widths = col_widths.iter()
                        .map(|&w| (w * available / total_content).max(3))
                        .collect();
                } else {
                    // Distribute budget proportionally among shrinkable columns
                    for &idx in &shrinkable_indices {
                        let share = (col_widths[idx] * budget_for_shrinkable / shrinkable_total).max(6);
                        new_widths[idx] = share;
                    }
                }

                // Distribute any leftover chars to the widest columns first
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
        let style = if has_header && i == 0 { header_style } else { cell_style };

        // Wrap each cell into multiple visual lines
        let mut wrapped_cols: Vec<Vec<String>> = Vec::new();
        for (j, cell) in row.iter().enumerate() {
            let w = col_widths[j];
            wrapped_cols.push(wrap_cell(cell, w));
        }
        let max_lines = wrapped_cols.iter().map(|c| c.len()).max().unwrap_or(1);

        for line_idx in 0..max_lines {
            let mut spans: Vec<Span> = Vec::new();
            spans.push(Span::styled(format!("{}  ", prefix), Style::default()));

            for (j, col_lines) in wrapped_cols.iter().enumerate() {
                let w = col_widths[j];
                let cell_text = col_lines.get(line_idx).map(|s| s.as_str()).unwrap_or("");
                let stripped_text = strip_inline_md(cell_text);
                let display_w = UnicodeWidthStr::width(stripped_text.as_str());
                let padding = w.saturating_sub(display_w);

                // Leading space
                spans.push(Span::styled(" ".to_string(), style));
                // Parse inline markdown (bold, italic, code) within cell
                spans.extend(parse_inline_md(cell_text, style));
                // Trailing padding + space
                spans.push(Span::styled(format!("{} ", " ".repeat(padding)), style));

                if j < num_cols - 1 {
                    spans.push(Span::styled(" ", Style::default()));
                }
            }
            result.push(Line::from(spans));
        }

        // Add a faint separator after multi-line wrapped body rows
        // so adjacent rows don't blur together
        if max_lines > 1 && i >= body_start && i < rows.len() - 1 {
            let rule_width: usize = col_widths.iter().sum::<usize>() + num_cols * 3;
            let sep = format!("{}  {}", prefix, "\u{2508}".repeat(rule_width.min(width.saturating_sub(UnicodeWidthStr::width(prefix) + 2))));
            result.push(Line::from(Span::styled(
                sep,
                Style::default().fg(THEME.load().table_border_color).add_modifier(Modifier::DIM),
            )));
        }

        // After header row, draw a single ─ rule
        if has_header && i == 0 {
            let rule_width: usize = col_widths.iter().sum::<usize>() + num_cols * 3;
            let sep = format!("{}  {}", prefix, "\u{2500}".repeat(rule_width.min(width.saturating_sub(UnicodeWidthStr::width(prefix) + 2))));
            result.push(Line::from(Span::styled(sep, border_style)));
        }

        // For tables with >6 rows, add a faint rule every 5th body row
        if has_header && i > 0 && body_count > 6 {
            let body_idx = i - body_start; // 0-indexed body row
            if body_idx > 0 && body_idx % 5 == 0 && i < rows.len() - 1 {
                let rule_width: usize = col_widths.iter().sum::<usize>() + num_cols * 3;
                let sep = format!("{}  {}", prefix, "\u{2500}".repeat(rule_width.min(width.saturating_sub(UnicodeWidthStr::width(prefix) + 2))));
                result.push(Line::from(Span::styled(
                    sep,
                    Style::default().fg(THEME.load().table_border_color).add_modifier(Modifier::DIM),
                )));
            }
        } else if !has_header && body_count > 6 {
            // No header case — stripe from row 0
            if i > 0 && i % 5 == 0 && i < rows.len() - 1 {
                let rule_width: usize = col_widths.iter().sum::<usize>() + num_cols * 3;
                let sep = format!("{}  {}", prefix, "\u{2500}".repeat(rule_width.min(width.saturating_sub(UnicodeWidthStr::width(prefix) + 2))));
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
                let block_inner_width = width.saturating_sub(UnicodeWidthStr::width(prefix) + 4); // prefix + "  " + borders
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

/// Tab width used to expand `\t` characters before wrapping.
const TAB_WIDTH: usize = 4;

/// Expand tabs to the next [`TAB_WIDTH`]-column boundary (proper tab-stop
/// expansion, not a fixed 4-space replacement) and report the column
/// position immediately after the *last* tab in the input.
///
/// The returned anchor lets `wrap_text` align continuation rows under the
/// element that was wrapped — e.g. for `"key:\tvalue that wraps a lot"`,
/// continuation rows indent to the column where `value` started, so the
/// wrapped text stays visually under the value column instead of
/// collapsing back to the leading indent.
fn expand_tabs_with_anchor(input: &str) -> (String, Option<usize>) {
    let mut out = String::with_capacity(input.len());
    let mut anchor: Option<usize> = None;
    for ch in input.chars() {
        if ch == '\t' {
            let col = out.chars().count();
            let pad = TAB_WIDTH - (col % TAB_WIDTH);
            for _ in 0..pad {
                out.push(' ');
            }
            anchor = Some(out.chars().count());
        } else {
            out.push(ch);
        }
    }
    (out, anchor)
}

#[allow(unused_assignments)]
pub(crate) fn wrap_text(raw_text: &str, width: usize) -> Vec<String> {
    let (text, tab_anchor) = expand_tabs_with_anchor(raw_text);
    if width == 0 || text.chars().count() <= width {
        return vec![text];
    }

    // Continuation lines normally indent to match the leading whitespace of
    // the source line so wrapped text doesn't bleed back to column 0. If the
    // source contained any tab characters, prefer the column immediately
    // after the *last* tab so tabbed/aligned data stays visually aligned
    // under the element it was wrapped from.
    let leading_indent = text.chars().take_while(|c| *c == ' ').count();
    let cont_indent_len = match tab_anchor {
        // Safety clamp: if the tab anchor would leave fewer than ~16 chars
        // of body width, fall back to the leading indent. Prevents pathological
        // narrow-pane cases from starving content to a few chars per row.
        Some(a) if a + 16 <= width => a,
        _ => leading_indent,
    };
    let indent: String = " ".repeat(cont_indent_len);
    let wrap_width = width.saturating_sub(cont_indent_len);

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: extract all text content from rendered lines
    fn rendered_text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| {
            l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()
        }).collect()
    }

    #[test]
    fn table_narrow_cols_preserved_when_shrinking() {
        // Table with mixed column widths: short status + long description
        // On a narrow terminal, the short columns should keep their size
        let table_lines = vec![
            "| Status | Name | Description |".to_string(),
            "|--------|------|-------------|".to_string(),
            "| ✅ | Spike | The executor workhorse agent that handles grunt work |".to_string(),
            "| ❌ | Chrollo | Deep analyst for recon and complex reasoning tasks |".to_string(),
        ];

        // Render at width=60 — tight squeeze
        let result = render_table(&table_lines, "  ", 60);
        let texts = rendered_text(&result);

        // Status column (✅/❌) should NOT be truncated to "…"
        let has_checkmark = texts.iter().any(|t| t.contains('✅'));
        let has_cross = texts.iter().any(|t| t.contains('❌'));
        assert!(has_checkmark, "Status ✅ was truncated away");
        assert!(has_cross, "Status ❌ was truncated away");

        // Names should survive too (short columns)
        let has_spike = texts.iter().any(|t| t.contains("Spike"));
        let has_chrollo = texts.iter().any(|t| t.contains("Chrollo"));
        assert!(has_spike, "Name 'Spike' was truncated");
        assert!(has_chrollo, "Name 'Chrollo' was truncated");
    }

    #[test]
    fn table_six_columns_dont_all_become_ellipsis() {
        // The exact scenario from S182: wide table with 6 columns
        let table_lines = vec![
            "| # | Test | Status | Quality | Speed | Notes |".to_string(),
            "|---|------|--------|---------|-------|-------|".to_string(),
            "| 1 | example.com | ✅ Yes | Good | 0.13s | Perfect output |".to_string(),
            "| 2 | HN | ✅ Yes | Meh | 0.41s | Table-noisy but works |".to_string(),
        ];

        let result = render_table(&table_lines, "  ", 80);
        let texts = rendered_text(&result);

        // Short columns (#, Status, Speed) must survive intact
        let row1 = texts.iter().find(|t| t.contains("example")).unwrap();
        assert!(row1.contains("0.13s"), "Speed column was truncated in: {}", row1);
        assert!(row1.contains("✅"), "Status was truncated in: {}", row1);
    }

    #[test]
    fn table_fits_width_no_truncation() {
        // Table that fits — nothing should be truncated or wrapped
        let table_lines = vec![
            "| A | B |".to_string(),
            "|---|---|".to_string(),
            "| x | y |".to_string(),
        ];

        let result = render_table(&table_lines, "", 80);
        let texts = rendered_text(&result);
        // No ellipsis anywhere
        assert!(!texts.iter().any(|t| t.contains('\u{2026}')),
            "Small table was unnecessarily truncated");
    }

    #[test]
    fn wrap_cell_fits_returns_single_line() {
        let result = wrap_cell("hello world", 20);
        assert_eq!(result, vec!["hello world"]);
    }

    #[test]
    fn wrap_cell_breaks_on_word_boundary() {
        let result = wrap_cell("hello world foo", 11);
        assert_eq!(result, vec!["hello world", "foo"]);
    }

    #[test]
    fn wrap_cell_force_breaks_long_word() {
        let result = wrap_cell("abcdefghij", 5);
        assert_eq!(result, vec!["abcde", "fghij"]);
    }

    #[test]
    fn table_wraps_instead_of_truncating() {
        let table_lines = vec![
            "| Name | Description |".to_string(),
            "|------|-------------|".to_string(),
            "| Spike | The executor workhorse agent that handles all grunt work |".to_string(),
        ];

        let result = render_table(&table_lines, "", 40);
        let texts = rendered_text(&result);

        // Full description text should be present across wrapped lines
        let all_text: String = texts.join(" ");
        assert!(all_text.contains("grunt work"),
            "Wrapped table lost content: {}", all_text);
        // No ellipsis — we wrap, not truncate
        assert!(!texts.iter().any(|t| t.contains('\u{2026}')),
            "Table used truncation instead of wrapping");
    }

    #[test]
    fn table_renders_bold_markdown_in_cells() {
        let table_lines = vec![
            "| Category | Value |".to_string(),
            "|----------|-------|".to_string(),
            "| **Era** | 130 million years |".to_string(),
        ];

        let result = render_table(&table_lines, "", 80);
        let texts = rendered_text(&result);

        // The ** markers should NOT appear in rendered text
        let all_text: String = texts.join(" ");
        assert!(!all_text.contains("**"), "Bold markers ** still visible in: {}", all_text);
        assert!(all_text.contains("Era"), "Bold content 'Era' missing from: {}", all_text);

        // Check that the Era span actually has BOLD modifier
        let era_line = result.iter().find(|l| {
            l.spans.iter().any(|s| s.content.contains("Era"))
        }).expect("No line contains 'Era'");
        let era_span = era_line.spans.iter().find(|s| s.content.contains("Era")).unwrap();
        assert!(era_span.style.add_modifier == Modifier::BOLD,
            "Era span is not bold: {:?}", era_span.style);
    }

    #[test]
    fn strip_inline_md_removes_bold_markers() {
        assert_eq!(strip_inline_md("**hello**"), "hello");
        assert_eq!(strip_inline_md("**a** and **b**"), "a and b");
        assert_eq!(strip_inline_md("plain text"), "plain text");
        assert_eq!(strip_inline_md("*italic*"), "italic");
        assert_eq!(strip_inline_md("`code`"), "code");
    }

    #[test]
    fn table_bold_cells_dont_waste_width_on_markers() {
        // **Signature Move** is 20 chars raw but only 14 stripped
        // Width calc should use 14, giving the column more room
        let table_lines = vec![
            "| Label | Data |".to_string(),
            "|-------|------|".to_string(),
            "| **Signature Move** | some data |".to_string(),
            "| **Era** | more data |".to_string(),
        ];

        let result = render_table(&table_lines, "", 80);
        let texts = rendered_text(&result);
        let all_text: String = texts.join(" ");
        // Full label should be visible, not truncated
        assert!(all_text.contains("Signature Move"),
            "Label was truncated: {}", all_text);
    }

    // --- tab-anchor wrap tests ---

    #[test]
    fn expand_tabs_fixed_4col_boundary_no_tab_returns_no_anchor() {
        let (out, anchor) = expand_tabs_with_anchor("hello world");
        assert_eq!(out, "hello world");
        assert_eq!(anchor, None);
    }

    #[test]
    fn expand_tabs_to_next_tab_stop() {
        // "ab" is 2 chars → next stop is col 4 → 2 spaces of padding.
        let (out, anchor) = expand_tabs_with_anchor("ab\tcd");
        assert_eq!(out, "ab  cd");
        assert_eq!(anchor, Some(4));
    }

    #[test]
    fn expand_tabs_anchor_uses_last_tab() {
        // Two tabs — anchor must point to the column after the LAST tab.
        let (out, anchor) = expand_tabs_with_anchor("a\tb\tc");
        // "a" → col 1; tab → pad to col 4 → "    "; "b" → col 5;
        // tab → pad to col 8 → "   "; "c" → col 9. Anchor = 8.
        assert_eq!(out, "a   b   c");
        assert_eq!(anchor, Some(8));
    }

    #[test]
    fn wrap_continuation_aligns_after_tab_anchor() {
        // "key:\tlong value with several words that wraps" — width 30.
        // After tab expansion: "key:" (4) + 4 pad → "key:    " then body.
        // Anchor is col 8. Continuation rows must indent 8 spaces.
        let input = "key:\tlong value with several words that should wrap onto a second line";
        let lines = wrap_text(input, 30);
        assert!(lines.len() >= 2, "expected wrap, got: {:?}", lines);
        for cont in lines.iter().skip(1) {
            // Every continuation line starts with 8 spaces (tab anchor),
            // not the leading-indent of 0.
            assert!(
                cont.starts_with("        "),
                "continuation line not aligned to tab anchor: {:?}",
                cont
            );
            // But it shouldn't start with 9 spaces (over-indented).
            assert!(
                !cont.starts_with("         "),
                "continuation over-indented: {:?}",
                cont
            );
        }
    }

    #[test]
    fn wrap_falls_back_to_leading_indent_when_no_tab() {
        // No tab in input → continuation should use leading-whitespace indent
        // (4 spaces here), preserving prior behavior.
        let input = "    bullet item with a fairly long description that will need to wrap";
        let lines = wrap_text(input, 30);
        assert!(lines.len() >= 2);
        for cont in lines.iter().skip(1) {
            assert!(
                cont.starts_with("    "),
                "continuation lost leading indent: {:?}",
                cont
            );
            assert!(
                !cont.starts_with("     "),
                "continuation over-indented: {:?}",
                cont
            );
        }
    }

    #[test]
    fn wrap_clamps_huge_tab_anchor_to_leading_indent() {
        // Pathological: tab anchor would leave only ~6 chars of body width.
        // Safety clamp falls back to leading indent (0 in this case).
        // Without the clamp, every continuation row would be 60 spaces of
        // indent in a 64-wide pane — usable but cramped.
        let padded = format!(
            "{}\tbody content with a few words to force a wrap event",
            "x".repeat(50)
        );
        let lines = wrap_text(&padded, 64);
        assert!(lines.len() >= 2);
        // Continuation should NOT be indented to col 52 (which would leave
        // only 12 chars of body). Clamp falls back to leading indent (0).
        for cont in lines.iter().skip(1) {
            assert!(
                !cont.starts_with("                                "),
                "continuation was over-indented despite safety clamp: {:?}",
                cont
            );
        }
    }
}
