//! Plugins modal — borderless, after Noodle (see `tui/modal_kit.rs`): a
//! dimmed backdrop, a flat surface, a sidebar of sources beside the plugin
//! rows, `┃` bar selection, and borderless popups for details, prompts and
//! install progress.

use super::super::modal_kit::{
    centered, fill, hint_line, open_modal, render_scrolled, row_line, section_title, Palette,
};
use super::super::theme::{ModalKind, THEME};
use super::progress::{ClonePhase, InstallProgressHandle};
use super::state::{Focus, LeftRow, RightMode, RightRow};
use super::PluginsModalState;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Gauge, Paragraph, Wrap};
use ratatui::Frame;

/// Sidebar width (bar + source label + count).
const SIDEBAR_W: u16 = 24;
/// Plugin-name column width in the rows pane.
const NAME_W: usize = 22;

fn palette() -> Palette {
    Palette::for_modal(&THEME.load(), Some(ModalKind::Plugins))
}

const OVERLAY_MAX_WIDTH: u16 = 70;
const OVERLAY_HEIGHT: u16 = 7;

/// Width of the centered overlay rect for a given outer area.
/// Single source of truth — used by both the rect builder and the
/// content-aware height estimators so wrapping can be computed against
/// the same width that will actually be rendered.
fn overlay_outer_width(area: Rect) -> u16 {
    area.width.saturating_sub(4).clamp(24, OVERLAY_MAX_WIDTH)
}

/// Inner content width = outer width minus the popup's 2-column padding on
/// each side.
fn overlay_inner_width(area: Rect) -> u16 {
    overlay_outer_width(area).saturating_sub(4)
}

/// Estimate how many rows `line` will occupy when rendered into a column of
/// `content_width` cells with `Wrap { trim: false }`. Counts characters as
/// a 1:1 proxy for display width — fine for ASCII, slightly over-tall for
/// wide-char content (we'd rather waste a row than clip the y/n footer).
fn estimate_wrapped_rows(line: &str, content_width: u16) -> u16 {
    if content_width == 0 {
        return 1;
    }
    let cw = content_width as usize;
    let chars = line.chars().count().max(1);
    (chars.div_ceil(cw)) as u16
}

/// Estimate total rows for a summary block. Each summary line is prefixed
/// with two spaces of indent (`"  {line}"`), so usable width per line is
/// `inner_width - 2`.
fn estimate_summary_rows(summary: &[String], inner_width: u16) -> u16 {
    let usable = inner_width.saturating_sub(2);
    summary
        .iter()
        .map(|s| estimate_wrapped_rows(s, usable))
        .sum()
}

pub(crate) fn render(frame: &mut Frame, area: Rect, state: &PluginsModalState) {
    let w = (area.width.saturating_mul(8) / 10).max(60).min(area.width);
    let h = (area.height.saturating_mul(7) / 10)
        .max(20)
        .min(area.height);
    let p = palette();
    let body = open_modal(
        frame,
        area,
        centered(area, w, h),
        &p,
        "Plugins",
        "esc close",
    );
    if body.height < 3 || body.width < SIDEBAR_W + 10 {
        return;
    }
    let footer_bar = Rect {
        y: body.bottom() - 1,
        height: 1,
        ..body
    };
    let content = Rect {
        height: body.height - 2,
        ..body
    };
    let sidebar = Rect {
        width: SIDEBAR_W,
        ..content
    };
    let main = Rect {
        x: content.x + SIDEBAR_W + 2,
        width: content.width.saturating_sub(SIDEBAR_W + 2),
        ..content
    };

    render_left(frame, sidebar, state, &p);
    render_right(frame, main, state, &p);
    render_footer(frame, footer_bar, state, &p);
}

/// The label of a left row, and its count (if any).
fn left_label(state: &PluginsModalState, row: &LeftRow) -> (String, Option<usize>) {
    match row {
        LeftRow::Installed => (
            "Installed".to_string(),
            Some(state.file.installed.len()).filter(|n| *n > 0),
        ),
        LeftRow::Marketplace(name) => {
            let count = state
                .file
                .marketplaces
                .iter()
                .find(|m| &m.name == name)
                .map(|m| m.cached_plugins.len())
                .unwrap_or(0);
            (name.clone(), Some(count).filter(|n| *n > 0))
        }
        LeftRow::AddMarketplace => ("+ Add marketplace".to_string(), None),
    }
}

fn render_left(frame: &mut Frame, area: Rect, state: &PluginsModalState, p: &Palette) {
    let focused = matches!(state.focus, Focus::Left);
    let lines: Vec<Line<'static>> = state
        .left_rows()
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let (label, count) = left_label(state, row);
            let value = count
                .map(|n| vec![Span::styled(n.to_string(), Style::default().fg(p.dim))])
                .unwrap_or_default();
            row_line(
                p,
                area.width,
                0,
                &label,
                value,
                i == state.selected_left,
                focused,
            )
        })
        .collect();
    let sel = state.selected_left;
    render_scrolled(frame, area, lines, (sel, sel + 1));
}

fn render_right(frame: &mut Frame, area: Rect, state: &PluginsModalState, p: &Palette) {
    // Always render the list behind overlays so users see context.
    render_right_list(frame, area, state, p);
    match &state.mode {
        RightMode::List => {}
        RightMode::Installing { progress } => render_installing(frame, area, progress, p),
        RightMode::Detail { row_idx } => render_right_detail(frame, area, state, *row_idx, p),
        RightMode::AddMarketplaceEditor { buffer, error } => {
            render_add_editor(frame, area, buffer, error.as_deref(), p)
        }
        RightMode::TrustPrompt {
            plugin_name,
            host,
            summary,
            ..
        } => render_trust_prompt(frame, area, plugin_name, host, summary, p),
        RightMode::Confirm {
            prompt, summary, ..
        } => render_confirm(frame, area, prompt, summary, p),
        RightMode::PendingInstallConfirm {
            plugin_name,
            summary,
            ..
        } => render_confirm(
            frame,
            area,
            &format!("Install executable plugin '{}' ?", plugin_name).replace("' ?", "'?"),
            summary,
            p,
        ),
        RightMode::PendingUpdateConfirm {
            plugin_name,
            summary,
            ..
        } => render_confirm(
            frame,
            area,
            &format!("Update plugin '{}' ?", plugin_name).replace("' ?", "'?"),
            summary,
            p,
        ),
    }
}

fn installed_row_up_to_date(
    latest_commit: Option<&String>,
    installed_commit: &str,
    checksum_value: Option<&String>,
) -> bool {
    match (latest_commit, checksum_value) {
        (Some(latest), _) if latest == installed_commit => true,
        (None, Some(_)) => true,
        _ => false,
    }
}

fn render_right_list(frame: &mut Frame, area: Rect, state: &PluginsModalState, p: &Palette) {
    let title = state
        .left_rows()
        .get(state.selected_left)
        .map(|row| left_label(state, row).0)
        .unwrap_or_default();
    let mut lines = section_title(p, &title);
    let rows = state.right_rows();
    if rows.is_empty() {
        let dim = |t: &str| Span::styled(t.to_string(), Style::default().fg(p.dim));
        let mut empty = match state.left_rows().get(state.selected_left) {
            Some(LeftRow::AddMarketplace) => hint_line(p, "enter to add a marketplace", area.width),
            Some(LeftRow::Installed) => Line::from(dim("No plugins installed yet.")),
            Some(LeftRow::Marketplace(_)) => {
                let mut l = hint_line(p, "r refresh", area.width);
                l.spans.insert(0, dim("No cached plugins.   "));
                l
            }
            None => Line::from(""),
        };
        empty.spans.insert(0, Span::raw("  "));
        lines.push(empty);
        frame.render_widget(Paragraph::new(lines), area);
        return;
    }

    let focused = matches!(state.focus, Focus::Right);
    let header = lines.len();
    for (i, row) in rows.iter().enumerate() {
        let (name, value) = match row {
            RightRow::Installed(ip) => {
                let up_to_date = installed_row_up_to_date(
                    ip.latest_commit.as_ref(),
                    &ip.installed_commit,
                    ip.checksum_value.as_ref(),
                );
                let mut v = vec![Span::styled(
                    "\u{25cf} installed",
                    Style::default().fg(p.value),
                )];
                if !up_to_date {
                    v.push(Span::styled(
                        "   \u{2191} update available",
                        Style::default().fg(p.title),
                    ));
                }
                (ip.name.clone(), v)
            }
            RightRow::Browseable { plugin, installed } => {
                let v = if *installed {
                    Span::styled("\u{25cf} installed", Style::default().fg(p.value))
                } else {
                    Span::styled("\u{25cb} available", Style::default().fg(p.dim))
                };
                (plugin.name.clone(), vec![v])
            }
        };
        let selected = i == state.selected_right && focused;
        lines.push(row_line(
            p, area.width, NAME_W, &name, value, selected, focused,
        ));
    }
    let sel = header + state.selected_right;
    render_scrolled(frame, area, lines, (sel, sel + 1));
}

fn render_right_detail(
    frame: &mut Frame,
    area: Rect,
    state: &PluginsModalState,
    row_idx: usize,
    p: &Palette,
) {
    let rows = state.right_rows();
    let name = match rows.get(row_idx) {
        Some(RightRow::Installed(ip)) => ip.name.clone(),
        Some(RightRow::Browseable { plugin, .. }) => plugin.name.clone(),
        None => "Detail".to_string(),
    };
    // Borderless popup over the list, inset 1 column.
    let rect = inset_rect(area, 1, 0);
    let inner = popup_surface(frame, rect, &name, p);

    let Some(row) = rows.get(row_idx) else {
        frame.render_widget(
            Paragraph::new("no selection").style(Style::default().fg(p.dim).bg(p.popup)),
            inner,
        );
        return;
    };

    let label_style = Style::default().fg(p.dim);
    let value_style = Style::default().fg(p.text);
    let mut lines: Vec<Line> = Vec::new();
    match row {
        RightRow::Installed(ip) => {
            lines.push(Line::from(vec![
                Span::styled("name:        ", label_style),
                Span::styled(ip.name.clone(), value_style),
            ]));
            lines.push(Line::from(vec![
                Span::styled("source:      ", label_style),
                Span::styled(ip.source_url.clone(), value_style),
            ]));
            lines.push(Line::from(vec![
                Span::styled("marketplace: ", label_style),
                Span::styled(
                    ip.marketplace
                        .clone()
                        .unwrap_or_else(|| "(direct)".to_string()),
                    value_style,
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("commit:      ", label_style),
                Span::styled(ip.installed_commit.clone(), value_style),
            ]));
            let latest = ip.latest_commit.clone().unwrap_or_else(|| {
                if ip.checksum_value.is_some() {
                    "index-verified".to_string()
                } else {
                    "?".to_string()
                }
            });
            let up_to_date = installed_row_up_to_date(
                ip.latest_commit.as_ref(),
                &ip.installed_commit,
                ip.checksum_value.as_ref(),
            );
            let mut latest_line = latest;
            if !up_to_date {
                latest_line.push_str("  (update available)");
            }
            lines.push(Line::from(vec![
                Span::styled("latest:      ", label_style),
                Span::styled(latest_line, value_style),
            ]));
            lines.push(Line::from(vec![
                Span::styled("installed:   ", label_style),
                Span::styled(ip.installed_at.clone(), value_style),
            ]));
            if let Some(value) = &ip.checksum_value {
                lines.push(Line::from(vec![
                    Span::styled("checksum:    ", label_style),
                    Span::styled(
                        format!(
                            "{}:{}",
                            ip.checksum_algorithm
                                .clone()
                                .unwrap_or_else(|| "sha256".to_string()),
                            value
                        ),
                        value_style,
                    ),
                ]));
            }
        }
        RightRow::Browseable { plugin, installed } => {
            lines.push(Line::from(vec![
                Span::styled("name:        ", label_style),
                Span::styled(plugin.name.clone(), value_style),
            ]));
            lines.push(Line::from(vec![
                Span::styled("source:      ", label_style),
                Span::styled(plugin.source.clone(), value_style),
            ]));
            lines.push(Line::from(vec![
                Span::styled("version:     ", label_style),
                Span::styled(
                    plugin.version.clone().unwrap_or_else(|| "?".to_string()),
                    value_style,
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("description: ", label_style),
                Span::styled(
                    plugin
                        .description
                        .clone()
                        .unwrap_or_else(|| "no description".to_string()),
                    value_style,
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("status:      ", label_style),
                Span::styled(
                    if *installed { "installed" } else { "available" }.to_string(),
                    value_style,
                ),
            ]));
            if let Some(index) = &plugin.index {
                lines.push(Line::from(vec![
                    Span::styled("repository:  ", label_style),
                    Span::styled(index.repository.clone(), value_style),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("checksum:    ", label_style),
                    Span::styled(
                        format!("{}:{}", index.checksum_algorithm, index.checksum_value),
                        value_style,
                    ),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("compatible:  ", label_style),
                    Span::styled(
                        format!(
                            "Synaps {}, extension protocol {}",
                            index
                                .compatibility_synaps
                                .clone()
                                .unwrap_or_else(|| "unspecified".to_string()),
                            index
                                .compatibility_extension_protocol
                                .clone()
                                .unwrap_or_else(|| "unspecified".to_string())
                        ),
                        value_style,
                    ),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("executable:  ", label_style),
                    Span::styled(if index.has_extension { "yes" } else { "no" }, value_style),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("permissions: ", label_style),
                    Span::styled(
                        if index.permissions.is_empty() {
                            "none".to_string()
                        } else {
                            index.permissions.join(", ")
                        },
                        value_style,
                    ),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("hooks:       ", label_style),
                    Span::styled(
                        if index.hooks.is_empty() {
                            "none".to_string()
                        } else {
                            index.hooks.join(", ")
                        },
                        value_style,
                    ),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("commands:    ", label_style),
                    Span::styled(
                        if index.commands.is_empty() {
                            "none".to_string()
                        } else {
                            index.commands.join(", ")
                        },
                        value_style,
                    ),
                ]));
                if !index.providers.is_empty() {
                    lines.push(Line::from(vec![
                        Span::styled("providers:   ", label_style),
                        Span::styled(
                            index
                                .providers
                                .iter()
                                .map(|p| format!("{} ({})", p.id, p.models.join(", ")))
                                .collect::<Vec<_>>()
                                .join("; "),
                            value_style,
                        ),
                    ]));
                }
                if index
                    .permissions
                    .iter()
                    .any(|permission| permission == "providers.register")
                {
                    lines.push(Line::from(vec![
                        Span::styled("provider UX: ", label_style),
                        Span::styled(
                            "high impact — selected provider models receive conversation content",
                            Style::default().fg(p.error),
                        ),
                    ]));
                }
                if let Some(publisher) = &index.trust_publisher {
                    lines.push(Line::from(vec![
                        Span::styled("publisher:   ", label_style),
                        Span::styled(publisher.clone(), value_style),
                    ]));
                }
                if let Some(homepage) = &index.trust_homepage {
                    lines.push(Line::from(vec![
                        Span::styled("homepage:    ", label_style),
                        Span::styled(homepage.clone(), value_style),
                    ]));
                }
                lines.push(Line::from(vec![
                    Span::styled("install:     ", label_style),
                    Span::styled(
                        "fetched manifest is re-inspected before final install",
                        value_style,
                    ),
                ]));
            }
        }
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// A borderless popup at `rect`: the popup surface, 2 columns / 1 row of
/// padding, `title` bold on the first line and a blank line under it.
/// Returns the content area below the title.
fn popup_surface(frame: &mut Frame, rect: Rect, title: &str, p: &Palette) -> Rect {
    frame.render_widget(Clear, rect);
    fill(frame.buffer_mut(), rect, p.popup);
    let inner = Rect {
        x: rect.x + 2,
        y: rect.y + 1,
        width: rect.width.saturating_sub(4),
        height: rect.height.saturating_sub(2),
    };
    if inner.height == 0 {
        return inner;
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            title.to_string(),
            Style::default()
                .fg(p.title)
                .bg(p.popup)
                .add_modifier(Modifier::BOLD),
        ))),
        Rect { height: 1, ..inner },
    );
    Rect {
        y: inner.y + 2,
        height: inner.height.saturating_sub(2),
        ..inner
    }
}

/// A centered borderless popup whose content needs `height` rows including
/// the 2 rows the old boxed overlay spent on borders; the popup spends them
/// on padding, plus 2 more for its title line.
fn centered_overlay_with_height(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    height: u16,
    p: &Palette,
) -> Rect {
    let w = overlay_outer_width(area);
    let h = height.saturating_add(2);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let rect = Rect {
        x,
        y,
        width: w.min(area.width),
        height: h.min(area.height),
    };
    popup_surface(frame, rect, title, p)
}

fn centered_overlay(frame: &mut Frame, area: Rect, title: &str, p: &Palette) -> Rect {
    centered_overlay_with_height(frame, area, title, OVERLAY_HEIGHT, p)
}

fn render_add_editor(
    frame: &mut Frame,
    area: Rect,
    buffer: &str,
    error: Option<&str>,
    p: &Palette,
) {
    let inner = centered_overlay(frame, area, "Add marketplace", p);
    let bg = p.popup;
    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            "Marketplace URL",
            Style::default().fg(p.dim).bg(bg),
        )),
        Line::from(vec![
            Span::styled(buffer.to_string(), Style::default().fg(p.value).bg(bg)),
            Span::styled("\u{2588}", Style::default().fg(p.accent).bg(bg)),
        ]),
    ];
    if let Some(err) = error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            err.to_string(),
            Style::default().fg(p.error).bg(bg),
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// Prompt text, its summary lines (dim, indented) and a y/n hint.
fn prompt_lines(
    p: &Palette,
    prompt: &str,
    summary: &[String],
    hint: &str,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(
            prompt.to_string(),
            Style::default().fg(p.text),
        )),
        Line::from(""),
    ];
    for line in summary {
        lines.push(Line::from(Span::styled(
            format!("  {}", line),
            Style::default().fg(p.dim),
        )));
    }
    lines.push(Line::from(""));
    lines.push(hint_line(p, hint, width));
    lines
}

fn render_trust_prompt(
    frame: &mut Frame,
    area: Rect,
    plugin_name: &str,
    host: &str,
    summary: &[String],
    p: &Palette,
) {
    let inner_w = overlay_inner_width(area);
    let prompt = format!("Trust source {} and install {}?", host, plugin_name);
    let prompt_rows = estimate_wrapped_rows(&prompt, inner_w);
    let summary_rows = estimate_summary_rows(summary, inner_w);
    // Layout (content): prompt + blank + summary + blank + y/n, plus the 2
    // rows `centered_overlay_with_height` budgets for its padding.
    let needed = 2 + prompt_rows + 1 + summary_rows + 1 + 1;
    let height = needed.max(OVERLAY_HEIGHT).min(area.height.max(1));
    let inner = centered_overlay_with_height(frame, area, "Trust plugin", height, p);
    let lines = prompt_lines(p, &prompt, summary, "y trust  n cancel", inner.width);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn render_confirm(frame: &mut Frame, area: Rect, prompt: &str, summary: &[String], p: &Palette) {
    let inner_w = overlay_inner_width(area);
    let prompt_rows = estimate_wrapped_rows(prompt, inner_w);
    let summary_rows = estimate_summary_rows(summary, inner_w);
    // Layout (content): prompt + blank + summary + blank + y/n, plus the 2
    // rows `centered_overlay_with_height` budgets for its padding.
    let needed = 2 + prompt_rows + 1 + summary_rows + 1 + 1;
    let height = needed.max(OVERLAY_HEIGHT).min(area.height.max(1));
    let inner = centered_overlay_with_height(frame, area, "Confirm", height, p);
    let lines = prompt_lines(p, prompt, summary, "y yes  n no", inner.width);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// Animated frames for the spinner shown next to "Downloading…" while the
/// background `git clone` is in flight. Braille frames give a smooth feel
/// at 60fps without competing with the gauge for attention.
const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn render_installing(frame: &mut Frame, area: Rect, progress: &InstallProgressHandle, p: &Palette) {
    // Snapshot the shared state under a short-lived lock; never hold the
    // lock across rendering calls.
    let snap = match progress.lock() {
        Ok(p) => (
            p.plugin_name.clone(),
            p.phase,
            p.percent,
            p.counts,
            p.throughput.clone(),
            p.spinner_frame as usize,
            p.started_at,
            p.last_raw_line.clone(),
        ),
        Err(_) => return,
    };
    let (plugin_name, phase, percent, counts, throughput, spinner_frame, started_at, last_raw) =
        snap;

    let elapsed = started_at.elapsed();
    let elapsed_str = format!(
        "{:>2}.{:02}s",
        elapsed.as_secs(),
        elapsed.subsec_millis() / 10
    );

    // Layout: title + blank + gauge (1 row) + status line + (optional error line)
    // Content rows fixed at 5 + optional error line; +2 borders.
    let has_error = matches!(phase, ClonePhase::Failed) && last_raw.is_some();
    let needed = 5 + if has_error { 1 } else { 0 } + 2;
    let height = (needed as u16).max(OVERLAY_HEIGHT).min(area.height.max(1));
    let inner = centered_overlay_with_height(frame, area, "Installing", height, p);

    let [title_row, _blank, gauge_row, status_row, error_row] = Layout::vertical([
        Constraint::Length(1), // title
        Constraint::Length(1), // blank
        Constraint::Length(1), // gauge
        Constraint::Length(1), // status line
        Constraint::Min(0),    // error / spacer
    ])
    .areas(inner);

    let spinner_ch = SPINNER_FRAMES[spinner_frame % SPINNER_FRAMES.len()];
    let title_line = Line::from(vec![
        Span::styled(
            format!("{} ", spinner_ch),
            Style::default().fg(p.value).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("Downloading {}", plugin_name),
            Style::default().fg(p.text),
        ),
        Span::styled(format!("   {}", elapsed_str), Style::default().fg(p.dim)),
    ]);
    frame.render_widget(Paragraph::new(title_line), title_row);

    // Gauge — show indeterminate spinner-style fill while connecting,
    // real percentage once we have one.
    let pct = percent.unwrap_or(0).min(100);
    let pct_ratio = (pct as f64) / 100.0;
    let gauge_label = match (phase, counts) {
        (ClonePhase::Connecting, _) => "connecting…".to_string(),
        (ClonePhase::SetupRunning, _) => "running setup script…".to_string(),
        (ClonePhase::Done, _) => "complete".to_string(),
        (ClonePhase::Failed, _) => "failed".to_string(),
        (_, Some((a, b))) => format!("{:>3}%  ({}/{})", pct, a, b),
        (_, None) => format!("{:>3}%", pct),
    };
    let gauge = Gauge::default()
        .gauge_style(
            Style::default()
                .fg(if matches!(phase, ClonePhase::Failed) {
                    p.error
                } else {
                    p.value
                })
                .bg(p.selected),
        )
        .ratio(pct_ratio)
        .label(gauge_label);
    frame.render_widget(gauge, gauge_row);

    // Status line: phase label + throughput
    let mut status_spans = vec![Span::styled(phase.label(), Style::default().fg(p.dim))];
    if let Some(tp) = throughput {
        status_spans.push(Span::styled(
            format!("    {}", tp),
            Style::default().fg(p.dim),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(status_spans)), status_row);

    if has_error {
        if let Some(msg) = last_raw {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    msg.to_string(),
                    Style::default().fg(p.error),
                )))
                .wrap(Wrap { trim: false }),
                error_row,
            );
        }
    }
}

fn render_footer(frame: &mut Frame, area: Rect, state: &PluginsModalState, p: &Palette) {
    let hint = match (&state.focus, &state.mode) {
        (_, RightMode::Detail { .. }) => "esc back  i install  u update  U uninstall",
        (_, RightMode::AddMarketplaceEditor { .. }) => "type url  enter submit  esc cancel",
        (_, RightMode::TrustPrompt { .. }) => "y trust  n cancel",
        (_, RightMode::Confirm { .. }) => "y yes  n no  esc cancel",
        (_, RightMode::PendingInstallConfirm { .. }) => "y install  n cancel  esc cancel",
        (_, RightMode::PendingUpdateConfirm { .. }) => "y update  n cancel  esc cancel",
        (_, RightMode::Installing { .. }) => "downloading\u{2026}  please wait",
        (Focus::Left, RightMode::List) => {
            "\u{2191}\u{2193} navigate  tab switch  enter select  r refresh  R remove  esc close"
        }
        (Focus::Right, RightMode::List) => {
            "\u{2191}\u{2193} navigate  enter detail  i install  e/d enable/disable  u update  U uninstall  r refresh  R remove  esc close"
        }
    };

    if let Some(err) = &state.row_error {
        let mut line = hint_line(p, hint, area.width.saturating_sub(err.len() as u16 + 3));
        line.spans.insert(
            0,
            Span::styled(format!("{}   ", err), Style::default().fg(p.error)),
        );
        frame.render_widget(Paragraph::new(line), area);
    } else {
        frame.render_widget(Paragraph::new(hint_line(p, hint, area.width)), area);
    }
}

fn inset_rect(area: Rect, dx: u16, dy: u16) -> Rect {
    let w = area.width.saturating_sub(dx * 2);
    let h = area.height.saturating_sub(dy * 2);
    Rect {
        x: area.x + dx.min(area.width),
        y: area.y + dy.min(area.height),
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        estimate_summary_rows, estimate_wrapped_rows, installed_row_up_to_date, OVERLAY_HEIGHT,
    };

    #[test]
    fn index_verified_row_without_remote_head_is_current() {
        let checksum = "f".repeat(64);
        assert!(installed_row_up_to_date(None, "abc", Some(&checksum)));
    }

    #[test]
    fn legacy_row_without_remote_head_is_not_current() {
        assert!(!installed_row_up_to_date(None, "abc", None));
    }

    #[test]
    fn matching_remote_head_is_current() {
        let latest = "abc".to_string();
        assert!(installed_row_up_to_date(Some(&latest), "abc", None));
    }

    #[test]
    fn empty_line_estimates_one_row() {
        assert_eq!(estimate_wrapped_rows("", 40), 1);
    }

    #[test]
    fn short_line_estimates_one_row() {
        assert_eq!(estimate_wrapped_rows("hello", 40), 1);
    }

    #[test]
    fn line_at_exact_width_is_one_row() {
        let s: String = "x".repeat(40);
        assert_eq!(estimate_wrapped_rows(&s, 40), 1);
    }

    #[test]
    fn line_one_over_width_wraps_to_two() {
        let s: String = "x".repeat(41);
        assert_eq!(estimate_wrapped_rows(&s, 40), 2);
    }

    #[test]
    fn zero_width_falls_back_to_single_row() {
        assert_eq!(estimate_wrapped_rows("anything", 0), 1);
    }

    #[test]
    fn summary_rows_account_for_two_space_indent() {
        // Inner width 40 -> usable 38 after the "  " indent.
        // A 38-char line stays on one row; 39 wraps to two.
        let lines = vec!["x".repeat(38), "x".repeat(39)];
        assert_eq!(estimate_summary_rows(&lines, 40), 1 + 2);
    }

    #[test]
    fn summary_rows_for_typical_install_summary() {
        // Realistic permissions summary: 2-3 short lines, all fit.
        let lines: Vec<String> = vec![
            "executable extension: yes".into(),
            "permissions: tools.intercept, privacy.llm_content".into(),
            "hooks: 5".into(),
        ];
        assert_eq!(estimate_summary_rows(&lines, 60), 3);
    }

    /// Regression: previously `render_confirm` used `5 + N` for height which
    /// clipped the y/n footer when the summary had two or more lines on a
    /// terminal large enough that OVERLAY_HEIGHT (7) wasn't the floor.
    /// The corrected formula is `2 + prompt_rows + 1 + summary_rows + 1 + 1`
    /// (= 5 + summary_rows + prompt_rows). For a 1-row prompt and 3-row
    /// summary that's 9, and `.max(OVERLAY_HEIGHT)` keeps shorter cases at 7.
    #[test]
    fn confirm_height_fits_three_line_summary() {
        let summary = vec![
            "executable extension: yes".to_string(),
            "permissions: 3".to_string(),
            "hooks: 5".to_string(),
        ];
        let inner_w = 60;
        let prompt_rows = estimate_wrapped_rows("Install plugin 'x'?", inner_w);
        let summary_rows = estimate_summary_rows(&summary, inner_w);
        let needed = 2 + prompt_rows + 1 + summary_rows + 1 + 1;
        assert_eq!(needed, 9, "1-row prompt + 3-row summary needs 9 cells");
        let height = needed.max(OVERLAY_HEIGHT);
        assert!(
            height >= needed,
            "computed height {height} must accommodate content {needed}"
        );
    }

    #[test]
    fn confirm_height_floors_to_overlay_minimum_for_tiny_summary() {
        let summary: Vec<String> = vec![];
        let inner_w = 60;
        let prompt_rows = estimate_wrapped_rows("ok?", inner_w);
        let summary_rows = estimate_summary_rows(&summary, inner_w);
        let needed = 2 + prompt_rows + 1 + summary_rows + 1 + 1;
        assert_eq!(needed, 6);
        assert_eq!(needed.max(OVERLAY_HEIGHT), OVERLAY_HEIGHT);
    }
}

#[cfg(test)]
mod noodle_tests {
    //! The plugins modal is borderless (after Noodle): no frame, a sidebar of
    //! sources with `┃` bar selection, hints key-bright/word-dim.
    use crate::tui::testing::TestHarness;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn frame_glyphs(s: &str) -> usize {
        s.lines()
            .skip(2)
            .flat_map(str::chars)
            .filter(|c| "╭╮╰╯│┌┐└┘".contains(*c))
            .count()
    }

    #[test]
    fn plugins_modal_is_borderless_with_bar_selection() {
        let mut h = TestHarness::boot_with_size(110, 32);
        h.open_plugins_modal();
        let f = h.snapshot();
        assert!(f.contains("Plugins") && f.contains("esc close"), "{f}");
        assert_eq!(frame_glyphs(&f), 0, "no frame: {f}");
        assert!(
            f.contains("\u{2503} Installed"),
            "bar on the selected source: {f}"
        );
        h.key(KeyCode::Down, KeyModifiers::empty());
        let f = h.snapshot();
        assert!(f.contains("\u{2503} + Add marketplace"), "{f}");
        assert!(f.contains("enter to add a marketplace"), "{f}");
    }
}
