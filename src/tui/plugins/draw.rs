use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Gauge, Paragraph, Wrap};
use super::PluginsModalState;
use super::progress::{ClonePhase, InstallProgressHandle};
use super::state::{Focus, LeftRow, RightMode, RightRow};
use super::super::theme::THEME;

const OVERLAY_MAX_WIDTH: u16 = 70;
const OVERLAY_HEIGHT: u16 = 7;

/// Width of the centered overlay rect for a given outer area.
/// Single source of truth — used by both the rect builder and the
/// content-aware height estimators so wrapping can be computed against
/// the same width that will actually be rendered.
fn overlay_outer_width(area: Rect) -> u16 {
    area.width.saturating_sub(4).clamp(24, OVERLAY_MAX_WIDTH)
}

/// Inner content width = outer width minus 2 for the rounded box borders.
fn overlay_inner_width(area: Rect) -> u16 {
    overlay_outer_width(area).saturating_sub(2)
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
    let h = (area.height.saturating_mul(7) / 10).max(20).min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let modal = Rect { x, y, width: w, height: h };

    frame.render_widget(Clear, modal);
    let block = Block::default()
        .title(" Plugins ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(THEME.load().border_active))
        .style(Style::default().bg(THEME.load().bg));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(20), Constraint::Min(1)])
        .split(outer[0]);

    render_left(frame, panes[0], state);
    render_right(frame, panes[1], state);
    render_footer(frame, outer[1], state);
}

fn render_left(frame: &mut Frame, area: Rect, state: &PluginsModalState) {
    let rows = state.left_rows();
    let installed_count = state.file.installed.len();
    let mut lines = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let selected = i == state.selected_left;
        let marker = if selected { "▸ " } else { "  " };
        let style = if selected && matches!(state.focus, Focus::Left) {
            Style::default().fg(THEME.load().claude_label)
        } else if selected {
            Style::default().fg(THEME.load().claude_text)
        } else {
            Style::default().fg(THEME.load().help_fg)
        };
        let label = match row {
            LeftRow::Installed => {
                if installed_count > 0 {
                    format!("Installed ({})", installed_count)
                } else {
                    "Installed".to_string()
                }
            }
            LeftRow::Marketplace(name) => {
                let count = state.file.marketplaces.iter()
                    .find(|m| &m.name == name)
                    .map(|m| m.cached_plugins.len())
                    .unwrap_or(0);
                if count > 0 {
                    format!("{} ({})", name, count)
                } else {
                    name.clone()
                }
            }
            LeftRow::AddMarketplace => "+ Add Marketplace…".to_string(),
        };
        lines.push(Line::from(vec![Span::styled(format!("{}{}", marker, label), style)]));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_right(frame: &mut Frame, area: Rect, state: &PluginsModalState) {
    // Always render the list behind overlays so users see context.
    render_right_list(frame, area, state);
    match &state.mode {
        RightMode::List => {}
        RightMode::Installing { progress } => render_installing(frame, area, progress),
        RightMode::Detail { row_idx } => render_right_detail(frame, area, state, *row_idx),
        RightMode::AddMarketplaceEditor { buffer, error } => {
            render_add_editor(frame, area, buffer, error.as_deref())
        }
        RightMode::TrustPrompt { plugin_name, host, summary, .. } => {
            render_trust_prompt(frame, area, plugin_name, host, summary)
        }
        RightMode::Confirm { prompt, summary, .. } => render_confirm(frame, area, prompt, summary),
        RightMode::PendingInstallConfirm { plugin_name, summary, .. } => {
            render_confirm(frame, area, &format!("Install executable plugin '{}' ?", plugin_name).replace("' ?", "'?"), summary)
        }
        RightMode::PendingUpdateConfirm { plugin_name, summary, .. } => {
            render_confirm(frame, area, &format!("Update plugin '{}' ?", plugin_name).replace("' ?", "'?"), summary)
        }
    }
}

fn installed_row_up_to_date(latest_commit: Option<&String>, installed_commit: &str, checksum_value: Option<&String>) -> bool {
    match (latest_commit, checksum_value) {
        (Some(latest), _) if latest == installed_commit => true,
        (None, Some(_)) => true,
        _ => false,
    }
}

fn render_right_list(frame: &mut Frame, area: Rect, state: &PluginsModalState) {
    let rows = state.right_rows();
    if rows.is_empty() {
        let hint = match state.left_rows().get(state.selected_left) {
            Some(LeftRow::AddMarketplace) => "  Press Enter to add a marketplace.",
            Some(LeftRow::Installed) => "  No plugins installed.",
            Some(LeftRow::Marketplace(_)) => "  No cached plugins. Press r to refresh.",
            None => "",
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint,
                Style::default().fg(THEME.load().help_fg),
            ))),
            area,
        );
        return;
    }

    let mut lines = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let selected = i == state.selected_right && matches!(state.focus, Focus::Right);
        let style = if selected {
            Style::default().fg(THEME.load().claude_label)
        } else if i == state.selected_right {
            Style::default().fg(THEME.load().claude_text)
        } else {
            Style::default().fg(THEME.load().help_fg)
        };
        let (name, status) = match row {
            RightRow::Installed(ip) => {
                let mut s = String::from("installed");
                let up_to_date = installed_row_up_to_date(
                    ip.latest_commit.as_ref(),
                    &ip.installed_commit,
                    ip.checksum_value.as_ref(),
                );
                if !up_to_date {
                    s.push_str(" (update)");
                }
                (ip.name.clone(), s)
            }
            RightRow::Browseable { plugin, installed } => {
                let status = if *installed { "installed" } else { "available" };
                (plugin.name.clone(), status.to_string())
            }
        };
        lines.push(Line::from(vec![Span::styled(
            format!("  {:<20} {}", name, status),
            style,
        )]));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_right_detail(frame: &mut Frame, area: Rect, state: &PluginsModalState, row_idx: usize) {
    // Inset overlay panel for detail content.
    let rect = inset_rect(area, 2, 1);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .title(" Detail ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(THEME.load().border_active))
        .style(Style::default().bg(THEME.load().bg));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let rows = state.right_rows();
    let Some(row) = rows.get(row_idx) else {
        frame.render_widget(
            Paragraph::new("(no selection)")
                .style(Style::default().fg(THEME.load().help_fg)),
            inner,
        );
        return;
    };

    let label_style = Style::default().fg(THEME.load().help_fg);
    let value_style = Style::default().fg(THEME.load().claude_text);
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
                    ip.marketplace.clone().unwrap_or_else(|| "(direct)".to_string()),
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
                        format!("{}:{}", ip.checksum_algorithm.clone().unwrap_or_else(|| "sha256".to_string()), value),
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
                    plugin.description.clone().unwrap_or_else(|| "no description".to_string()),
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
                    Span::styled(format!("{}:{}", index.checksum_algorithm, index.checksum_value), value_style),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("compatible:  ", label_style),
                    Span::styled(format!(
                        "Synaps {}, extension protocol {}",
                        index.compatibility_synaps.clone().unwrap_or_else(|| "unspecified".to_string()),
                        index.compatibility_extension_protocol.clone().unwrap_or_else(|| "unspecified".to_string())
                    ), value_style),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("executable:  ", label_style),
                    Span::styled(if index.has_extension { "yes" } else { "no" }, value_style),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("permissions: ", label_style),
                    Span::styled(if index.permissions.is_empty() { "none".to_string() } else { index.permissions.join(", ") }, value_style),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("hooks:       ", label_style),
                    Span::styled(if index.hooks.is_empty() { "none".to_string() } else { index.hooks.join(", ") }, value_style),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("commands:    ", label_style),
                    Span::styled(if index.commands.is_empty() { "none".to_string() } else { index.commands.join(", ") }, value_style),
                ]));
                if !index.providers.is_empty() {
                    lines.push(Line::from(vec![
                        Span::styled("providers:   ", label_style),
                        Span::styled(index.providers.iter().map(|p| format!("{} ({})", p.id, p.models.join(", "))).collect::<Vec<_>>().join("; "), value_style),
                    ]));
                }
                if index.permissions.iter().any(|permission| permission == "providers.register") {
                    lines.push(Line::from(vec![
                        Span::styled("provider UX: ", label_style),
                        Span::styled("high impact — selected provider models receive conversation content", Style::default().fg(THEME.load().error_color)),
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
                    Span::styled("fetched manifest is re-inspected before final install", value_style),
                ]));
            }
        }
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn centered_overlay_with_height(frame: &mut Frame, area: Rect, title: &str, height: u16) -> Rect {
    let w = overlay_outer_width(area);
    let h = height;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let rect = Rect { x, y, width: w, height: h.min(area.height) };
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .title(title.to_string())
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(THEME.load().border_active))
        .style(Style::default().bg(THEME.load().bg));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    inner
}

fn centered_overlay(frame: &mut Frame, area: Rect, title: &str) -> Rect {
    centered_overlay_with_height(frame, area, title, OVERLAY_HEIGHT)
}

fn render_add_editor(frame: &mut Frame, area: Rect, buffer: &str, error: Option<&str>) {
    let inner = centered_overlay(frame, area, " Add Marketplace ");

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Enter marketplace URL:",
        Style::default().fg(THEME.load().help_fg),
    )));
    lines.push(Line::from(Span::styled(
        format!("[{}_]", buffer),
        Style::default().fg(THEME.load().claude_label),
    )));
    if let Some(err) = error {
        lines.push(Line::from(Span::raw("")));
        lines.push(Line::from(Span::styled(
            format!("! {}", err),
            Style::default().fg(THEME.load().error_color),
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn render_trust_prompt(frame: &mut Frame, area: Rect, plugin_name: &str, host: &str, summary: &[String]) {
    let inner_w = overlay_inner_width(area);
    let prompt = format!("Trust source {} and install {}?", host, plugin_name);
    let prompt_rows = estimate_wrapped_rows(&prompt, inner_w);
    let summary_rows = estimate_summary_rows(summary, inner_w);
    // Layout (content): prompt + blank + summary + blank + y/n
    // Plus 2 rows for the rounded box borders.
    let needed = 2 + prompt_rows + 1 + summary_rows + 1 + 1;
    let height = needed.max(OVERLAY_HEIGHT).min(area.height.max(1));
    let inner = centered_overlay_with_height(frame, area, " Trust Plugin ", height);

    let mut lines = vec![
        Line::from(Span::styled(
            prompt,
            Style::default().fg(THEME.load().claude_text),
        )),
        Line::from(Span::raw("")),
    ];
    for line in summary {
        lines.push(Line::from(Span::styled(
            format!("  {}", line),
            Style::default().fg(THEME.load().help_fg),
        )));
    }
    lines.push(Line::from(Span::raw("")));
    lines.push(Line::from(Span::styled(
        "  [y]es  [n]o",
        Style::default().fg(THEME.load().help_fg),
    )));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn render_confirm(frame: &mut Frame, area: Rect, prompt: &str, summary: &[String]) {
    let inner_w = overlay_inner_width(area);
    let prompt_rows = estimate_wrapped_rows(prompt, inner_w);
    let summary_rows = estimate_summary_rows(summary, inner_w);
    // Layout (content): prompt + blank + summary + blank + y/n
    // Plus 2 rows for the rounded box borders.
    let needed = 2 + prompt_rows + 1 + summary_rows + 1 + 1;
    let height = needed.max(OVERLAY_HEIGHT).min(area.height.max(1));
    let inner = centered_overlay_with_height(frame, area, " Confirm ", height);

    let mut lines = vec![
        Line::from(Span::styled(
            prompt.to_string(),
            Style::default().fg(THEME.load().claude_text),
        )),
        Line::from(Span::raw("")),
    ];
    for line in summary {
        lines.push(Line::from(Span::styled(
            format!("  {}", line),
            Style::default().fg(THEME.load().help_fg),
        )));
    }
    lines.push(Line::from(Span::raw("")));
    lines.push(Line::from(Span::styled(
        "  [y]es  [n]o",
        Style::default().fg(THEME.load().help_fg),
    )));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// Animated frames for the spinner shown next to "Downloading…" while the
/// background `git clone` is in flight. Braille frames give a smooth feel
/// at 60fps without competing with the gauge for attention.
const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn render_installing(frame: &mut Frame, area: Rect, progress: &InstallProgressHandle) {
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
    let elapsed_str = format!("{:>2}.{:02}s", elapsed.as_secs(), elapsed.subsec_millis() / 10);

    // Layout: title + blank + gauge (1 row) + status line + (optional error line)
    // Content rows fixed at 5 + optional error line; +2 borders.
    let has_error = matches!(phase, ClonePhase::Failed) && last_raw.is_some();
    let needed = 5 + if has_error { 1 } else { 0 } + 2;
    let height = (needed as u16).max(OVERLAY_HEIGHT).min(area.height.max(1));
    let inner = centered_overlay_with_height(frame, area, " Installing ", height);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(1), // blank
            Constraint::Length(1), // gauge
            Constraint::Length(1), // status line
            Constraint::Min(0),    // error / spacer
        ])
        .split(inner);

    let spinner_ch = SPINNER_FRAMES[spinner_frame % SPINNER_FRAMES.len()];
    let title_line = Line::from(vec![
        Span::styled(
            format!("{} ", spinner_ch),
            Style::default()
                .fg(THEME.load().claude_label)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("Downloading {}", plugin_name),
            Style::default().fg(THEME.load().claude_text),
        ),
        Span::styled(
            format!("   {}", elapsed_str),
            Style::default().fg(THEME.load().help_fg),
        ),
    ]);
    frame.render_widget(Paragraph::new(title_line), layout[0]);

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
                    THEME.load().error_color
                } else {
                    THEME.load().claude_label
                })
                .bg(THEME.load().bg),
        )
        .ratio(pct_ratio)
        .label(gauge_label);
    frame.render_widget(gauge, layout[2]);

    // Status line: phase label + throughput
    let mut status_spans = vec![Span::styled(
        phase.label(),
        Style::default().fg(THEME.load().help_fg),
    )];
    if let Some(tp) = throughput {
        status_spans.push(Span::styled(
            format!("    {}", tp),
            Style::default().fg(THEME.load().help_fg),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(status_spans)), layout[3]);

    if has_error {
        if let Some(msg) = last_raw {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("! {}", msg),
                    Style::default().fg(THEME.load().error_color),
                )))
                .wrap(Wrap { trim: false }),
                layout[4],
            );
        }
    }
}

fn render_footer(frame: &mut Frame, area: Rect, state: &PluginsModalState) {
    let hint = match (&state.focus, &state.mode) {
        (_, RightMode::Detail { .. }) => "Esc back  i install  u update  U uninstall",
        (_, RightMode::AddMarketplaceEditor { .. }) => "Type URL  Enter submit  Esc cancel",
        (_, RightMode::TrustPrompt { .. }) => "y trust  n cancel",
        (_, RightMode::Confirm { .. }) => "y yes  n no  Esc cancel",
        (_, RightMode::PendingInstallConfirm { .. }) => "y install  n cancel  Esc cancel",
        (_, RightMode::PendingUpdateConfirm { .. }) => "y update  n cancel  Esc cancel",
        (_, RightMode::Installing { .. }) => "downloading…  please wait",
        (Focus::Left, RightMode::List) => {
            "↑↓ nav  Tab switch  Enter select  r refresh  R remove  Esc close"
        }
        (Focus::Right, RightMode::List) => {
            "↑↓ nav  Tab  Enter detail  i install  e/d enable/disable  u update  U uninstall  r refresh  R remove mkt  Esc close"
        }
    };

    if let Some(err) = &state.row_error {
        let spans = vec![
            Span::styled(format!("! {}  ", err), Style::default().fg(THEME.load().error_color)),
            Span::styled(hint.to_string(), Style::default().fg(THEME.load().help_fg)),
        ];
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    } else {
        frame.render_widget(
            Paragraph::new(hint).style(Style::default().fg(THEME.load().help_fg)),
            area,
        );
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
        estimate_summary_rows, estimate_wrapped_rows, installed_row_up_to_date,
        OVERLAY_HEIGHT,
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
