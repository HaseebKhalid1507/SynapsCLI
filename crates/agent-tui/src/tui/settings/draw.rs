//! Settings modal — borderless, after Noodle's settings view
//! (github.com/wilfredinni/noodle, `src/ui/settings/SettingsView.tsx`).
//!
//! The screen behind dims (Noodle's modal backdrop); the modal is a plain
//! chrome-coloured surface with no box. A sidebar of categories sits beside
//! the category's rows. Selection is a raised surface plus a heavy `┃` bar on
//! the left — in the accent colour for the focused pane, muted for the other.
//! The selected setting's description sits under it; hints follow the
//! footer's rule: key bright, word dim.

use super::super::theme::{ModalKind, Theme, THEME};
use super::schema::{visible_categories, EditorKind, SettingDef};
use super::{ActiveEditor, Focus, RuntimeSnapshot, SettingsState};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

/// Sidebar width (bar + label).
const SIDEBAR_W: u16 = 24;
/// Label column width in the rows pane.
const LABEL_W: usize = 22;
/// How far the selection surface stands off the chrome.
const SELECTED_STEP: f64 = 1.18;
/// How far popups (pickers, custom editors) stand off the chrome.
const POPUP_STEP: f64 = 1.32;
/// Share of the way each colour behind the modal moves toward black.
const BACKDROP_DIM: f32 = 0.5;

/// The palette the modal draws with.
struct Palette {
    panel: Color,
    selected: Color,
    popup: Color,
    accent: Color,
    title: Color,
    text: Color,
    value: Color,
    dim: Color,
    error: Color,
}

impl Palette {
    fn from_theme(t: &Theme) -> Self {
        Self {
            panel: t.bg,
            selected: t.raised_surface(SELECTED_STEP),
            popup: t.raised_surface(POPUP_STEP),
            // P19.1 per-part overrides keep working: `settings.border` is the
            // accent (there is no border any more), `settings.title` the title.
            accent: t.modal_border(ModalKind::Settings),
            title: t.modal_title(ModalKind::Settings).unwrap_or(t.claude_label),
            text: t.claude_text,
            value: t.claude_label,
            dim: t.chrome_dim(),
            error: t.error_color,
        }
    }
}

/// Darken every cell of `area` outside `keep` toward black: the modal's
/// backdrop, so the modal reads as in front without a frame around it.
fn dim_backdrop(buf: &mut Buffer, area: Rect, keep: Rect) {
    let dim = |c: Color| match c {
        Color::Rgb(r, g, b) => {
            let f = |v: u8| (f32::from(v) * (1.0 - BACKDROP_DIM)).round() as u8;
            Color::Rgb(f(r), f(g), f(b))
        }
        other => other,
    };
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if keep.contains((x, y).into()) {
                continue;
            }
            if let Some(cell) = buf.cell_mut((x, y)) {
                let (fg, bg) = (dim(cell.fg), dim(cell.bg));
                cell.set_fg(fg).set_bg(bg);
            }
        }
    }
}

/// Fill `rect` with `bg`.
fn fill(buf: &mut Buffer, rect: Rect, bg: Color) {
    buf.set_style(rect, Style::default().bg(bg));
    for y in rect.top()..rect.bottom() {
        for x in rect.left()..rect.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_symbol(" ");
            }
        }
    }
}

/// One selectable row: `┃` bar (selected only), label column, value.
/// Selected rows sit on the raised surface across the full `width`.
fn row_line(
    p: &Palette,
    width: u16,
    label: &str,
    value: Vec<Span<'static>>,
    selected: bool,
    focused: bool,
) -> Line<'static> {
    let bg = if selected { p.selected } else { p.panel };
    let bar = if selected {
        Span::styled(
            "\u{2503} ",
            Style::default()
                .fg(if focused { p.accent } else { p.dim })
                .bg(bg),
        )
    } else {
        Span::styled("  ", Style::default().bg(bg))
    };
    let label_style = if selected && focused {
        Style::default()
            .fg(p.text)
            .bg(bg)
            .add_modifier(Modifier::BOLD)
    } else if selected {
        Style::default().fg(p.text).bg(bg)
    } else {
        Style::default().fg(p.dim).bg(bg)
    };
    let mut spans = vec![
        bar,
        Span::styled(format!("{label:<LABEL_W$} "), label_style),
    ];
    spans.extend(value.into_iter().map(|s| {
        let st = s.style.bg(bg);
        s.style(st)
    }));
    pad_to(&mut spans, width, bg);
    Line::from(spans)
}

/// Right-pad a line's spans with `bg` to `width` cells.
fn pad_to(spans: &mut Vec<Span<'static>>, width: u16, bg: Color) {
    let used: usize = spans
        .iter()
        .map(|s| super::super::text_metrics::width(&s.content))
        .sum();
    let pad = usize::from(width).saturating_sub(used);
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
    }
}

/// A plain value in the value colour (selected) or text colour.
fn value_span(p: &Palette, v: String, selected: bool) -> Vec<Span<'static>> {
    let fg = if selected { p.value } else { p.text };
    vec![Span::styled(v, Style::default().fg(fg))]
}

/// Cycler value while selected: `‹ value ›` with dim arrows.
fn cycler_spans(p: &Palette, v: String) -> Vec<Span<'static>> {
    vec![
        Span::styled("\u{2039} ", Style::default().fg(p.dim)),
        Span::styled(v, Style::default().fg(p.value).add_modifier(Modifier::BOLD)),
        Span::styled(" \u{203a}", Style::default().fg(p.dim)),
    ]
}

/// An inline text editor: the buffer, a block cursor, and an optional error.
fn editor_spans(p: &Palette, buffer: &str, error: Option<&String>) -> Vec<Span<'static>> {
    let mut v = vec![
        Span::styled(buffer.to_string(), Style::default().fg(p.value)),
        Span::styled("\u{2588}", Style::default().fg(p.accent)),
    ];
    if let Some(err) = error {
        v.push(Span::styled(
            format!("  {err}"),
            Style::default().fg(p.error),
        ));
    }
    v
}

/// A detail line under the selected row (description, note, error),
/// indented to the label column and wrapped to `width`.
fn detail_lines(text: &str, fg: Color, width: u16, bg: Color) -> Vec<Line<'static>> {
    let indent = "  ";
    let w = usize::from(width).saturating_sub(indent.len() + 1).max(10);
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > w {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    out.into_iter()
        .map(|l| {
            let mut spans = vec![Span::styled(
                format!("{indent}{l}"),
                Style::default().fg(fg).bg(bg),
            )];
            pad_to(&mut spans, width, bg);
            Line::from(spans)
        })
        .collect()
}

/// Render `lines` into `area`, scrolled so rows `focus.0..focus.1` (the
/// selected row and its details) stay in view.
fn render_scrolled(
    frame: &mut Frame,
    area: Rect,
    lines: Vec<Line<'static>>,
    focus: (usize, usize),
) {
    let h = usize::from(area.height);
    let offset = if focus.1 > h {
        (focus.1 - h).min(focus.0)
    } else {
        0
    };
    frame.render_widget(Paragraph::new(lines).scroll((offset as u16, 0)), area);
}

/// Hints in the footer's style: segments separated by two spaces in the
/// source strings; the first word of each is the key (bright), the rest dim.
fn hint_line(p: &Palette, hint: &str, width: u16) -> Line<'static> {
    let mut spans = Vec::new();
    let mut used = 0usize;
    for (i, seg) in hint.split("  ").filter(|s| !s.is_empty()).enumerate() {
        let (key, word) = seg.split_once(' ').unwrap_or((seg, ""));
        let w = super::super::text_metrics::width(seg) + if i > 0 { 3 } else { 0 };
        if used + w > usize::from(width) {
            break; // drop whole segments when narrow
        }
        if i > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled(key.to_string(), Style::default().fg(p.text)));
        if !word.is_empty() {
            spans.push(Span::styled(format!(" {word}"), Style::default().fg(p.dim)));
        }
        used += w;
    }
    Line::from(spans)
}

pub(crate) fn render(frame: &mut Frame, area: Rect, state: &SettingsState, snap: &RuntimeSnapshot) {
    let w = (area.width.saturating_mul(8) / 10).max(60).min(area.width);
    let h = (area.height.saturating_mul(7) / 10)
        .max(20)
        .min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let modal = Rect {
        x,
        y,
        width: w,
        height: h,
    };

    let theme = THEME.load();
    let p = Palette::from_theme(&theme);
    dim_backdrop(frame.buffer_mut(), area, modal);
    frame.render_widget(Clear, modal);
    fill(frame.buffer_mut(), modal, p.panel);

    // Padding: 2 columns, 1 row. Header, blank, body, blank, hints.
    let inner = Rect {
        x: modal.x + 2,
        y: modal.y + 1,
        width: modal.width.saturating_sub(4),
        height: modal.height.saturating_sub(2),
    };
    if inner.height < 5 || inner.width < SIDEBAR_W + 10 {
        return;
    }
    let header = Rect { height: 1, ..inner };
    let footer = Rect {
        y: inner.bottom() - 1,
        height: 1,
        ..inner
    };
    let body = Rect {
        y: inner.y + 2,
        height: inner.height.saturating_sub(4),
        ..inner
    };
    let sidebar = Rect {
        width: SIDEBAR_W,
        ..body
    };
    let main = Rect {
        x: body.x + SIDEBAR_W + 2,
        width: body.width.saturating_sub(SIDEBAR_W + 2),
        ..body
    };

    let title = Line::from(vec![Span::styled(
        "Settings",
        Style::default().fg(p.title).add_modifier(Modifier::BOLD),
    )]);
    frame.render_widget(Paragraph::new(title), header);
    let close = hint_line(&p, "esc close", header.width);
    frame.render_widget(
        Paragraph::new(close).alignment(ratatui::layout::Alignment::Right),
        header,
    );

    render_categories(frame, sidebar, state, snap, &p);
    render_settings(frame, main, state, snap, &p);
    render_footer(frame, footer, state, snap, &p);

    if let Some(ActiveEditor::PluginCustom { render, .. }) = &state.edit_mode {
        render_plugin_custom_editor(frame, main, render, &p);
    }
}

fn render_categories(
    frame: &mut Frame,
    area: Rect,
    state: &SettingsState,
    snap: &RuntimeSnapshot,
    p: &Palette,
) {
    let focused = state.focus == Focus::Left;
    let mut lines = Vec::new();
    let cats = visible_categories(&snap.lifecycle_claims);
    let n_builtin = cats.len();
    for (i, cat) in cats.iter().enumerate() {
        lines.push(row_line(
            p,
            area.width,
            cat.label(),
            Vec::new(),
            i == state.category_idx,
            focused,
        ));
    }
    for (i, pcat) in snap.plugin_categories.iter().enumerate() {
        let selected = n_builtin + i == state.category_idx;
        // Source label: the plugin that owns it, dim, so users can audit.
        let mut line = row_line(p, area.width, &pcat.label, Vec::new(), selected, focused);
        let owner = format!(" {}", pcat.plugin);
        if let Some(label) = line.spans.get_mut(1) {
            let trimmed = format!("{} ", pcat.label);
            *label = Span::styled(trimmed, label.style);
        }
        line.spans.insert(
            2,
            Span::styled(
                owner,
                Style::default()
                    .fg(p.dim)
                    .bg(if selected { p.selected } else { p.panel }),
            ),
        );
        lines.push(line);
    }
    let sel = state.category_idx;
    render_scrolled(frame, area, lines, (sel, sel + 1));
}

/// The category title at the top of the rows pane, and a blank line.
fn section_title(p: &Palette, title: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![Span::styled(
            title.to_string(),
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
    ]
}

/// A note / error attached to a row (`row_error`): notes dim, errors red.
fn note_lines(p: &Palette, msg: &str, width: u16) -> Vec<Line<'static>> {
    let color = if msg.starts_with("saved") {
        p.dim
    } else {
        p.error
    };
    detail_lines(msg, color, width, p.panel)
}

fn render_settings(
    frame: &mut Frame,
    area: Rect,
    state: &SettingsState,
    snap: &RuntimeSnapshot,
    p: &Palette,
) {
    if state.is_plugin_category(snap) {
        render_plugin_category(frame, area, state, snap, p);
        return;
    }
    let current_cat = visible_categories(&snap.lifecycle_claims)
        .get(state.category_idx)
        .copied()
        .unwrap_or(super::schema::Category::Plugins);
    if current_cat == super::schema::Category::Plugins {
        render_plugins_list(frame, area, state, snap, p);
        return;
    }
    if current_cat == super::schema::Category::Providers {
        render_providers_list(frame, area, state, snap, p);
        return;
    }
    let focused = state.focus == Focus::Right;
    let settings = state.current_settings(snap);
    let selected_key = settings.get(state.setting_idx).map(|d| d.key);
    let mut lines = section_title(p, current_cat.label());
    let mut focus = (0, 0);
    for (i, def) in settings.iter().enumerate() {
        let selected = i == state.setting_idx && focused;
        let current_value = current_value_for(def, snap);
        let value = if selected && focused {
            match (&state.edit_mode, &def.editor) {
                (
                    Some(ActiveEditor::Text {
                        buffer,
                        setting_key,
                        error,
                        ..
                    }),
                    _,
                ) if *setting_key == def.key => editor_spans(p, buffer, error.as_ref()),
                (
                    Some(ActiveEditor::CustomModel {
                        buffer,
                        setting_key,
                    }),
                    _,
                ) if *setting_key == def.key => editor_spans(p, buffer, None),
                (None, EditorKind::Cycler(_)) | (None, EditorKind::DynamicCycler) => {
                    cycler_spans(p, current_value)
                }
                _ => value_span(p, current_value, true),
            }
        } else {
            value_span(p, current_value, false)
        };
        let start = lines.len();
        lines.push(row_line(p, area.width, def.label, value, selected, focused));
        if selected {
            if !def.help.is_empty() {
                lines.extend(detail_lines(def.help, p.dim, area.width, p.panel));
            }
            if let Some((key, msg)) = &state.row_error {
                if selected_key == Some(key.as_str()) {
                    lines.extend(note_lines(p, msg, area.width));
                }
            }
            focus = (start, lines.len());
        }
    }
    render_scrolled(frame, area, lines, focus);

    if let Some(ActiveEditor::Picker {
        options, cursor, ..
    }) = &state.edit_mode
    {
        render_picker(frame, area, options, *cursor, p);
    }
}

fn render_plugin_category(
    frame: &mut Frame,
    area: Rect,
    state: &SettingsState,
    snap: &RuntimeSnapshot,
    p: &Palette,
) {
    use super::input::plugin_field_current_value;
    let cat = match state.current_plugin_category(snap) {
        Some(c) => c,
        None => return,
    };
    let focused = state.focus == Focus::Right;
    let mut lines = section_title(p, &cat.label);
    // The source, explicit, so users can audit what they're changing.
    lines.insert(
        1,
        Line::from(vec![
            Span::styled("from ", Style::default().fg(p.dim)),
            Span::styled(cat.plugin.clone(), Style::default().fg(p.text)),
        ]),
    );
    let mut focus = (0, 0);
    for (i, field) in cat.fields.iter().enumerate() {
        let selected = i == state.setting_idx && focused;
        let current = plugin_field_current_value(&cat.plugin, field);
        use synaps_cli::skills::registry::PluginSettingsEditor as PE;
        let value = if selected && focused {
            match (&state.edit_mode, &field.editor) {
                (
                    Some(super::ActiveEditor::PluginText {
                        plugin_id,
                        key,
                        buffer,
                        error,
                        ..
                    }),
                    _,
                ) if *plugin_id == cat.plugin && *key == field.key => {
                    editor_spans(p, buffer, error.as_ref())
                }
                (None, PE::Cycler { .. }) => cycler_spans(p, current),
                (
                    Some(super::ActiveEditor::PluginCustom {
                        plugin_id,
                        field: active_field,
                        ..
                    }),
                    PE::Custom,
                ) if *plugin_id == cat.plugin && *active_field == field.key => {
                    value_span(p, "editing\u{2026}".to_string(), true)
                }
                (_, PE::Custom) => vec![
                    Span::styled("enter", Style::default().fg(p.text)),
                    Span::styled(" to edit", Style::default().fg(p.dim)),
                ],
                _ => value_span(p, current.clone(), true),
            }
        } else if matches!(field.editor, PE::Custom) {
            vec![Span::styled("custom", Style::default().fg(p.dim))]
        } else {
            value_span(p, current.clone(), false)
        };
        let start = lines.len();
        lines.push(row_line(
            p,
            area.width,
            &field.label,
            value,
            selected,
            focused,
        ));
        if selected {
            if let Some((rk, msg)) = &state.row_error {
                if rk == &format!("plugin.{}.{}", cat.plugin, field.key) {
                    lines.extend(note_lines(p, msg, area.width));
                }
            }
            focus = (start, lines.len());
        }
    }
    render_scrolled(frame, area, lines, focus);
}

fn render_plugins_list(
    frame: &mut Frame,
    area: Rect,
    state: &SettingsState,
    snap: &RuntimeSnapshot,
    p: &Palette,
) {
    let focused = state.focus == Focus::Right;
    let mut lines = section_title(p, "Plugins");

    // Row 0 — the marketplace action, styled as an action, not a plugin.
    let start = lines.len();
    let action_selected = state.setting_idx == 0 && focused;
    lines.push(row_line(
        p,
        area.width,
        "+ Plugin marketplace",
        if action_selected {
            vec![
                Span::styled("enter", Style::default().fg(p.text)),
                Span::styled(" to open", Style::default().fg(p.dim)),
            ]
        } else {
            Vec::new()
        },
        action_selected,
        focused,
    ));
    // Load errors / notes attached to the action row.
    if let Some((key, msg)) = &state.row_error {
        if key == "plugins" {
            lines.extend(note_lines(p, msg, area.width));
        }
    }
    let mut focus = if action_selected {
        (start, lines.len())
    } else {
        (0, 0)
    };

    // Rows 1..=n — installed plugins at snap.plugins[idx - 1].
    for (i, plug) in snap.plugins.iter().enumerate() {
        let row_idx = i + 1;
        let disabled = snap.disabled_plugins.iter().any(|d| d == &plug.name);
        let selected = row_idx == state.setting_idx && focused;
        let mut value = vec![if disabled {
            Span::styled("\u{25cb} disabled", Style::default().fg(p.dim))
        } else {
            Span::styled("\u{25cf} enabled", Style::default().fg(p.value))
        }];
        if plug.skill_count > 0 {
            value.push(Span::styled(
                format!("   {} skills", plug.skill_count),
                Style::default().fg(p.dim),
            ));
        }
        if selected {
            focus = (lines.len(), lines.len() + 1);
        }
        lines.push(row_line(
            p, area.width, &plug.name, value, selected, focused,
        ));
    }

    render_scrolled(frame, area, lines, focus);
}

fn render_providers_list(
    frame: &mut Frame,
    area: Rect,
    state: &SettingsState,
    snap: &RuntimeSnapshot,
    p: &Palette,
) {
    let focused = state.focus == Focus::Right;
    let providers = synaps_cli::runtime::openai::registry::providers();
    let mut lines = section_title(p, "Providers");
    let mut focus = (0, 0);

    // Row 0: local models.
    let selected = state.setting_idx == 0 && focused;
    let local_url = snap
        .local_url_explicit
        .clone()
        .unwrap_or_else(|| "localhost:11434".to_string());
    let value = match &state.edit_mode {
        Some(ActiveEditor::ApiKey {
            provider_id,
            buffer,
        }) if provider_id == "local.url" => editor_spans(p, buffer, None),
        _ if snap.local_url_explicit.is_some() => vec![
            Span::styled("\u{25cf} ", Style::default().fg(p.value)),
            Span::styled(local_url, Style::default().fg(p.text)),
        ],
        _ => vec![
            Span::styled("\u{25cb} default ", Style::default().fg(p.dim)),
            Span::styled(local_url, Style::default().fg(p.dim)),
        ],
    };
    let start = lines.len();
    lines.push(row_line(
        p,
        area.width,
        "Local (Ollama/etc)",
        value,
        selected,
        focused,
    ));
    if selected {
        if let Some((key, msg)) = &state.row_error {
            if key == "provider.local.url" {
                lines.extend(note_lines(p, msg, area.width));
            }
        }
        focus = (start, lines.len());
    }

    // Rows 1..=N: registry providers.
    for (i, prov) in providers.iter().enumerate() {
        let selected = i + 1 == state.setting_idx && focused;
        let value = match &state.edit_mode {
            Some(ActiveEditor::ApiKey {
                provider_id,
                buffer,
            }) if provider_id == prov.key => {
                let masked: String = "*".repeat(buffer.len().min(32));
                editor_spans(p, &masked, None)
            }
            _ => provider_status_spans(p, &provider_status(prov, snap)),
        };
        let start = lines.len();
        lines.push(row_line(p, area.width, prov.name, value, selected, focused));
        if selected {
            if let Some((key, msg)) = &state.row_error {
                if key == &format!("provider.{}", prov.key) {
                    lines.extend(note_lines(p, msg, area.width));
                }
            }
            focus = (start, lines.len());
        }
    }
    render_scrolled(frame, area, lines, focus);
}

fn provider_status(
    p: &synaps_cli::runtime::openai::registry::ProviderSpec,
    snap: &RuntimeSnapshot,
) -> String {
    // Broker-sourced, non-secret status: the TUI renders a masked preview and
    // never holds the key value itself.
    let key_status = match snap.provider_key_status.get(p.key) {
        Some(synaps_cli::auth::StaticKeyStatus::Configured { masked }) => format!("✅ {}", masked),
        Some(synaps_cli::auth::StaticKeyStatus::FromEnv) => "✅ (from env)".to_string(),
        _ => return "⬚ not set".to_string(), // No key = no ping data relevant
    };

    // Append ping summary if available — count online/total models for this provider
    let models: Vec<_> = p
        .models
        .iter()
        .filter_map(|(id, _, _)| {
            let full_key = format!("{}/{}", p.key, id);
            snap.model_health.get(&full_key).map(|(s, ms)| (s, *ms))
        })
        .collect();

    if models.is_empty() {
        return key_status;
    }

    let online = models
        .iter()
        .filter(|(s, _)| matches!(s, synaps_cli::runtime::openai::ping::PingStatus::Online))
        .count();
    let total = models.len();
    let fastest = models
        .iter()
        .filter(|(s, _)| matches!(s, synaps_cli::runtime::openai::ping::PingStatus::Online))
        .map(|(_, ms)| *ms)
        .min();

    let ping_str = if let Some(ms) = fastest {
        format!("  ({}/{} online, fastest {}ms)", online, total, ms)
    } else {
        format!("  (0/{} online)", total)
    };

    format!("{}{}", key_status, ping_str)
}

/// `provider_status` text as spans: set / from env / not set as a dot, the
/// ping summary dim.
fn provider_status_spans(p: &Palette, status: &str) -> Vec<Span<'static>> {
    if let Some(rest) = status.strip_prefix("\u{2705} ") {
        let (main, ping) = rest.split_once("  ").unwrap_or((rest, ""));
        let mut v = vec![
            Span::styled("\u{25cf} ", Style::default().fg(p.value)),
            Span::styled(main.to_string(), Style::default().fg(p.text)),
        ];
        if !ping.is_empty() {
            v.push(Span::styled(
                format!("   {ping}"),
                Style::default().fg(p.dim),
            ));
        }
        v
    } else {
        vec![Span::styled(
            status.replace("\u{2b1a}", "\u{25cb}"),
            Style::default().fg(p.dim),
        )]
    }
}

/// A floating surface over the rows pane (pickers, custom editors): no
/// border — one step brighter than the selection surface.
fn popup_rect(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(2)).max(1);
    let h = height.min(area.height.saturating_sub(1)).max(1);
    Rect {
        x: area.x + 2.min(area.width.saturating_sub(w)),
        y: area.y + 2.min(area.height.saturating_sub(h)),
        width: w,
        height: h,
    }
}

/// Popup rows: selected gets the accent bar and bright text.
fn popup_lines<'a>(
    p: &Palette,
    labels: impl Iterator<Item = (usize, &'a str, bool)>,
    cursor: usize,
    width: u16,
) -> Vec<Line<'static>> {
    labels
        .map(|(i, label, selectable)| {
            let selected = i == cursor;
            let fg = if !selectable {
                p.dim
            } else if selected {
                p.value
            } else {
                p.text
            };
            let mut spans = vec![
                Span::styled(
                    if selected { "\u{2503} " } else { "  " },
                    Style::default().fg(p.accent).bg(p.popup),
                ),
                Span::styled(label.to_string(), Style::default().fg(fg).bg(p.popup)),
            ];
            pad_to(&mut spans, width, p.popup);
            Line::from(spans)
        })
        .collect()
}

fn render_plugin_custom_editor(
    frame: &mut Frame,
    area: Rect,
    session: &super::plugin_editor::PluginEditorSession,
    p: &Palette,
) {
    let rows = &session.render.rows;
    let cursor = session.render.cursor.unwrap_or(0);
    let footer_lines: u16 = u16::from(session.render.footer.is_some());
    let avail_w = area.width.saturating_sub(4).max(1);
    let w = avail_w.clamp(avail_w.min(40), 100); // clamp min to avail so narrow terminals can't overflow (#tui-safety fix 3)
    let needed = rows.len() as u16 + 3 + footer_lines;
    let rect = popup_rect(area, w, needed.max(4));
    frame.render_widget(Clear, rect);
    fill(frame.buffer_mut(), rect, p.popup);
    // Padding 1 row / 1 column; a title line; rows; optional footer.
    let inner = Rect {
        x: rect.x + 1,
        y: rect.y + 1,
        width: rect.width.saturating_sub(2),
        height: rect.height.saturating_sub(2),
    };
    let title = Line::from(vec![
        Span::styled(
            session.plugin_id.clone(),
            Style::default()
                .fg(p.title)
                .bg(p.popup)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", session.field),
            Style::default().fg(p.dim).bg(p.popup),
        ),
    ]);
    frame.render_widget(Paragraph::new(title), Rect { height: 1, ..inner });
    let list = Rect {
        y: inner.y + 1,
        height: inner.height.saturating_sub(1 + footer_lines),
        ..inner
    };
    let visible = usize::from(list.height).max(1);
    let offset = cursor.saturating_sub(visible - 1);
    let marked: Vec<String> = rows
        .iter()
        .map(|r| match r.marker.as_deref() {
            Some(m) if !m.trim().is_empty() => format!("{m} {}", r.label),
            _ => r.label.clone(),
        })
        .collect();
    let lines = popup_lines(
        p,
        rows.iter()
            .zip(&marked)
            .enumerate()
            .skip(offset)
            .take(visible)
            .map(|(i, (r, l))| (i, l.as_str(), r.selectable)),
        cursor,
        list.width,
    );
    frame.render_widget(Paragraph::new(lines), list);
    if let Some(footer) = &session.render.footer {
        let foot = Rect {
            y: inner.bottom().saturating_sub(1),
            height: 1,
            ..inner
        };
        frame.render_widget(
            Paragraph::new(footer.clone()).style(Style::default().fg(p.dim).bg(p.popup)),
            foot,
        );
    }
}

fn render_picker(frame: &mut Frame, area: Rect, options: &[String], cursor: usize, p: &Palette) {
    let avail_w = area.width.saturating_sub(4).max(1);
    let w = avail_w.clamp(avail_w.min(20), 100); // clamp min to avail so narrow terminals can't overflow (#tui-safety fix 3)
    let rect = popup_rect(area, w, options.len() as u16 + 2);
    frame.render_widget(Clear, rect);
    fill(frame.buffer_mut(), rect, p.popup);
    let inner = Rect {
        x: rect.x,
        y: rect.y + 1,
        width: rect.width,
        height: rect.height.saturating_sub(2),
    };
    let visible = usize::from(inner.height).max(1);
    let offset = cursor.saturating_sub(visible - 1);
    let lines = popup_lines(
        p,
        options
            .iter()
            .enumerate()
            .skip(offset)
            .take(visible)
            .map(|(i, o)| (i, o.as_str(), true)),
        cursor,
        inner.width,
    );
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_footer(
    frame: &mut Frame,
    area: Rect,
    state: &SettingsState,
    snap: &RuntimeSnapshot,
    p: &Palette,
) {
    let cats = visible_categories(&snap.lifecycle_claims);
    let cat = cats
        .get(state.category_idx)
        .copied()
        .unwrap_or(super::schema::Category::Plugins);
    let on_plugins_right = cat == super::schema::Category::Plugins && state.focus == Focus::Right;
    let on_providers_right =
        cat == super::schema::Category::Providers && state.focus == Focus::Right;
    let in_api_key_editor = matches!(state.edit_mode, Some(ActiveEditor::ApiKey { .. }));
    let hint = if in_api_key_editor {
        "type key  enter save  esc cancel"
    } else if on_plugins_right && state.setting_idx == 0 {
        "\u{2191}\u{2193} navigate  tab switch pane  enter open marketplace  esc close"
    } else if on_plugins_right && state.setting_idx > 0 {
        "\u{2191}\u{2193} navigate  tab switch pane  space toggle  esc close"
    } else if on_providers_right {
        "\u{2191}\u{2193} navigate  tab switch pane  enter set key  d/del clear  p ping  esc close"
    } else {
        "\u{2191}\u{2193} navigate  tab switch pane  enter edit  esc close"
    };
    frame.render_widget(Paragraph::new(hint_line(p, hint, area.width)), area);
}

/// Read a bool config key as "on"/"off" for settings display, defaulting when
/// unset. Accepts the same truthy/falsey spellings the parser does.
fn bool_config_display(key: &str, default: bool) -> String {
    let val = synaps_cli::config::read_config_value(key)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let on = match val.as_deref() {
        Some("true" | "1" | "on" | "yes") => true,
        Some("false" | "0" | "off" | "no") => false,
        _ => default,
    };
    if on { "on".into() } else { "off".into() }
}

/// Read a u64 config key for settings display, defaulting when unset/invalid.
fn u64_config_display(key: &str, default: u64) -> String {
    synaps_cli::config::read_config_value(key)
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default)
        .to_string()
}

pub(crate) fn current_value_for(def: &SettingDef, snap: &RuntimeSnapshot) -> String {
    match def.key {
        "model" => snap.model.clone(),
        "thinking" => snap.thinking.clone(),
        "reasoning_type" => snap.reasoning_type.clone(),
        "context_window" => snap.context_window.clone(),
        "compaction_model" => snap.compaction_model.clone(),
        "api_retries" => snap.api_retries.to_string(),
        "subagent_timeout" => format!("{}s", snap.subagent_timeout),
        "max_tool_output" => snap.max_tool_output.to_string(),
        "bash_timeout" => format!("{}s", snap.bash_timeout),
        "bash_max_timeout" => format!("{}s", snap.bash_max_timeout),
        "theme" => snap.theme_name.clone(),
        "tui_background_opaque" => {
            if snap.background_opaque {
                "opaque".into()
            } else {
                "invisible".into()
            }
        }
        "sidecar_toggle_key" => synaps_cli::config::read_config_value("sidecar_toggle_key")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "F8".to_string()),
        "theme_transition" => synaps_cli::config::read_config_value("theme_transition")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "on".to_string()),
        "startup.quick_start" => bool_config_display("startup.quick_start", true),
        "tui_streaming_glow" => bool_config_display("tui_streaming_glow", true),
        "startup.extensions_ready_timeout_secs" => {
            u64_config_display("startup.extensions_ready_timeout_secs", 30)
        }
        "daemon.idle_exit_secs" => u64_config_display("daemon.idle_exit_secs", 10),
        "daemon.prompt_abandon_secs" => u64_config_display("daemon.prompt_abandon_secs", 3600),
        "daemon.parked_evict_secs" => u64_config_display("daemon.parked_evict_secs", 3600),
        _ => "?".into(),
    }
}

#[cfg(test)]
mod tui_safety_tests {
    // ── Fix 2: mask_key char-boundary safety ────────────────────────────────
    // The old implementation used byte-indexing (`&key[n-4..]`) which panics
    // when the key contains multi-byte UTF-8 characters. These tests verify
    // the new char-based implementation is panic-free and produces the right
    // shape of output for both ASCII and non-ASCII inputs.
    fn mask_key(key: &str) -> String {
        let chars: Vec<char> = key.chars().collect();
        let n = chars.len();
        if n <= 8 {
            return "*".repeat(n);
        }
        let suffix: String = chars[n - 4..].iter().collect();
        format!("***...{}", suffix)
    }

    #[test]
    fn mask_key_short_ascii() {
        // ≤8 chars → all stars, length preserved
        assert_eq!(mask_key("abc"), "***");
        assert_eq!(mask_key("12345678"), "********");
    }

    #[test]
    fn mask_key_long_ascii() {
        // >8 ASCII chars → prefix mask + last 4 chars
        let key = "sk-abcdefghij1234";
        let result = mask_key(key);
        assert!(result.starts_with("***..."), "should start with ***...");
        assert!(
            result.ends_with("1234"),
            "should end with last 4 ASCII chars"
        );
    }

    #[test]
    fn mask_key_multibyte_no_panic() {
        // Multi-byte UTF-8 — old code panicked here, new code must not
        let key = "sk-café-über-日本語-key1"; // contains non-ASCII
        let result = mask_key(key); // must not panic
        assert!(
            result.starts_with("***...") || result.chars().all(|c| c == '*'),
            "unexpected shape: {result}"
        );
    }

    #[test]
    fn mask_key_all_multibyte() {
        // Entirely non-ASCII, >8 chars by char-count
        let key = "日本語テスト用キー入力"; // 11 chars, each 3 bytes
        let result = mask_key(key);
        // Must not panic; must start with ***...
        assert!(result.starts_with("***..."), "got: {result}");
        // Last 4 chars should be the tail
        let expected_tail: String = key
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        assert!(result.ends_with(&expected_tail), "tail mismatch: {result}");
    }

    #[test]
    fn mask_key_exactly_9_chars() {
        // 9-char boundary — should produce ***...XXXX (last 4)
        let key = "abcde6789"; // 9 chars
        let result = mask_key(key);
        assert_eq!(result, "***...6789");
    }

    // ── Fix 3: modal width clamping never exceeds area ──────────────────────
    // These are logic-only tests for the clamping expressions used in
    // render_plugin_custom_editor and render_picker; no ratatui Frame needed.

    fn plugin_editor_w(area_width: u16) -> u16 {
        let avail_w = area_width.saturating_sub(4).max(1);
        avail_w.clamp(avail_w.min(40), 100)
    }

    fn picker_w(area_width: u16) -> u16 {
        let avail_w = area_width.saturating_sub(4).max(1);
        avail_w.clamp(avail_w.min(20), 100)
    }

    fn secret_prompt_w(area_width: u16) -> u16 {
        area_width.min(62)
    }

    #[test]
    fn plugin_editor_width_never_exceeds_area() {
        for aw in 0u16..=200 {
            let w = plugin_editor_w(aw);
            assert!(w <= aw.max(1), "plugin editor width {w} exceeds area {aw}");
        }
    }

    #[test]
    fn picker_width_never_exceeds_area() {
        for aw in 0u16..=200 {
            let w = picker_w(aw);
            assert!(w <= aw.max(1), "picker width {w} exceeds area {aw}");
        }
    }

    #[test]
    fn secret_prompt_width_never_exceeds_area() {
        for aw in 0u16..=200 {
            let w = secret_prompt_w(aw);
            assert!(w <= aw, "secret prompt width {w} exceeds area {aw}");
        }
    }

    #[test]
    fn plugin_editor_width_normal_terminal() {
        // 80-wide terminal → avail = 76, clamp(min(76,40), 100) = 76
        assert_eq!(plugin_editor_w(80), 76);
        // 200-wide → avail = 196, clamp(40, 100) = 100
        assert_eq!(plugin_editor_w(200), 100);
    }

    #[test]
    fn picker_width_normal_terminal() {
        // 80-wide → avail = 76, clamp(20, 100) = 76
        assert_eq!(picker_w(80), 76);
        // 24-wide → avail = 20, clamp(min(20,20), 100) = 20
        assert_eq!(picker_w(24), 20);
    }
}

#[cfg(test)]
mod noodle_tests {
    //! The settings modal is borderless (after Noodle): no box-drawing frame,
    //! a dimmed backdrop, selection as a raised row with a `┃` bar.
    use crate::tui::testing::TestHarness;
    use crate::tui::theme::{ModalKind, THEME};
    use crossterm::event::{KeyCode, KeyModifiers};
    use ratatui::buffer::Buffer;
    use ratatui::style::Color;

    const W: u16 = 110;
    const H: u16 = 32;

    /// The modal rect, as `render` computes it.
    fn modal() -> ratatui::layout::Rect {
        let w = (W * 8 / 10).clamp(60, W);
        let h = (H * 7 / 10).clamp(20, H);
        ratatui::layout::Rect::new((W - w) / 2, (H - h) / 2, w, h)
    }

    fn find(buf: &Buffer, needle: &str) -> Option<(u16, u16)> {
        for y in 0..buf.area().height {
            let row: String = (0..buf.area().width)
                .map(|x| buf[(x, y)].symbol())
                .collect();
            if let Some(i) = row.find(needle) {
                // byte index → column (rows are ASCII up to the needle here)
                let col = row[..i].chars().count() as u16;
                return Some((col, y));
            }
        }
        None
    }

    #[test]
    #[serial_test::serial]
    fn modal_has_no_frame() {
        let mut h = TestHarness::boot_with_size(W, H);
        h.open_settings_modal();
        let buf = h.render().clone();
        let m = modal();
        for y in m.top()..m.bottom() {
            for x in m.left()..m.right() {
                let c = buf[(x, y)].symbol().chars().next().unwrap_or(' ');
                assert!(
                    !"╭╮╰╯─│┌┐└┘├┤┬┴┼═║".contains(c),
                    "frame glyph {c:?} at ({x},{y})"
                );
            }
        }
        assert!(find(&buf, "Settings").is_some(), "title");
        // The whole modal is one flat chrome surface at its edges.
        let bg = Some(THEME.load().bg);
        for (x, y) in [(m.left(), m.top()), (m.right() - 1, m.bottom() - 1)] {
            assert_eq!(buf[(x, y)].style().bg, bg, "({x},{y})");
        }
    }

    #[test]
    #[serial_test::serial]
    fn backdrop_dims_behind_the_modal() {
        let mut h = TestHarness::boot_with_size(W, H);
        let before = h.render().clone();
        h.open_settings_modal();
        let after = h.render().clone();
        let lum = |c: Option<Color>| match c {
            Some(Color::Rgb(r, g, b)) => u32::from(r) + u32::from(g) + u32::from(b),
            _ => 0,
        };
        // A footer cell below the modal: its colours darken.
        let (x, y) = (2, H - 1);
        assert!(
            lum(after[(x, y)].style().fg) < lum(before[(x, y)].style().fg),
            "backdrop text dims"
        );
        assert!(
            lum(after[(x, y)].style().bg) <= lum(before[(x, y)].style().bg),
            "backdrop surface dims"
        );
    }

    #[test]
    #[serial_test::serial]
    fn selection_is_a_bar_in_the_focused_pane() {
        let mut h = TestHarness::boot_with_size(W, H);
        h.open_settings_modal();
        let accent = Some(THEME.load().modal_border(ModalKind::Settings));
        let buf = h.render().clone();
        let (x, y) = find(&buf, "\u{2503} Model").expect("selected category");
        assert_eq!(
            buf[(x, y)].style().fg,
            accent,
            "focused sidebar bar is the accent"
        );
        let selected_bg = buf[(x + 3, y)].style().bg;
        assert_ne!(selected_bg, Some(THEME.load().bg), "selected row is raised");

        // Move into the rows pane: the sidebar bar goes muted, the row gets it.
        h.key(KeyCode::Tab, KeyModifiers::empty());
        h.key(KeyCode::Down, KeyModifiers::empty());
        let buf = h.render().clone();
        assert_ne!(
            buf[(x, y)].style().fg,
            accent,
            "sidebar bar muted when unfocused"
        );
        let (rx, ry) = find(&buf, "\u{2503} Thinking").expect("selected setting");
        assert_eq!(buf[(rx, ry)].style().fg, accent);
        // Its description sits under it.
        let below: String = (rx..W).map(|x| buf[(x, ry + 1)].symbol()).collect();
        assert!(below.contains("Thinking depth"), "{below:?}");
    }
}
