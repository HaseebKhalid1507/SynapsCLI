use crossterm::{
    execute,
    terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap},
    Terminal,
};
use std::io;
use tachyonfx::{fx, Effect, Interpolation};

use super::text_metrics::{char_width, width as display_width};
use super::theme::background_is_opaque;

/// The six named panes that make up the outer app layout.
///
/// Single source of truth for the header/body/subagent/download/input/footer
/// split — use [`AppAreas::from_heights`] instead of inlining the constraints.
pub(crate) struct AppAreas {
    pub header: ratatui::layout::Rect,
    pub body: ratatui::layout::Rect,
    pub subagent: ratatui::layout::Rect,
    pub download: ratatui::layout::Rect,
    pub input: ratatui::layout::Rect,
    pub footer: ratatui::layout::Rect,
}

impl AppAreas {
    /// Split `area` into the 6 app panes given the runtime-computed heights.
    /// Single source of truth for the outer layout (header/body/subagent/download/input/footer).
    pub(crate) fn from_heights(
        area: ratatui::layout::Rect,
        subagent_height: u16,
        download_height: u16,
        input_height: u16,
    ) -> Self {
        let [header, body, subagent, download, input, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(subagent_height),
            Constraint::Length(download_height),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .areas(area);
        Self {
            header,
            body,
            subagent,
            download,
            input,
            footer,
        }
    }
}

/// Build a single sidecar pill segment for one `SidecarUiState`.
#[allow(dead_code)] // used in tests
fn sidecar_pill_segment(
    sidecar: &super::sidecar::SidecarUiState,
    spinner_frame: usize,
) -> Span<'static> {
    let label = sidecar.display_name.as_deref().unwrap_or("sidecar");
    let text = sidecar_pill_text(label, &sidecar.status, sidecar.armed, spinner_frame);
    let color = match &sidecar.status {
        super::sidecar::SidecarUiStatus::Idle => {
            if sidecar.armed {
                let pulse = ((spinner_frame as f64 / 18.0).sin() * 0.3 + 0.7).max(0.4);
                let base = match THEME.load().status_streaming {
                    Color::Rgb(r, g, b) => (r, g, b),
                    _ => (220, 80, 80),
                };
                Color::Rgb(
                    (base.0 as f64 * pulse) as u8,
                    (base.1 as f64 * pulse) as u8,
                    (base.2 as f64 * pulse) as u8,
                )
            } else {
                // P19.1: resting pill tint — `sidecar.pill` override if set,
                // else `muted` (the color used today).
                THEME.load().sidecar_pill_color()
            }
        }
        super::sidecar::SidecarUiStatus::Active { .. } => {
            let pulse = ((spinner_frame as f64 / 18.0).sin() * 0.3 + 0.7).max(0.4);
            let base = match THEME.load().status_streaming {
                Color::Rgb(r, g, b) => (r, g, b),
                _ => (220, 80, 80),
            };
            Color::Rgb(
                (base.0 as f64 * pulse) as u8,
                (base.1 as f64 * pulse) as u8,
                (base.2 as f64 * pulse) as u8,
            )
        }
        super::sidecar::SidecarUiStatus::Loading => THEME.load().muted,
        super::sidecar::SidecarUiStatus::Error(_) => Color::Red,
    };
    let modifier = Modifier::BOLD;
    Span::styled(text, Style::default().fg(color).add_modifier(modifier))
}

/// Pure helper for [`sidecar_pill_spans`] — given a set of (plugin_id, display_name?)
/// pairs and the registry's lifecycle claims, return the plugin ids in
/// display order: importance desc, then display_name alphabetical, then
/// plugin id. Pulled out so the ordering can be unit-tested without
/// constructing a full `SidecarUiState` (which owns a child process).
pub(crate) fn order_sidecar_pills(
    sidecars: &[(String, Option<String>)],
    claims: &[synaps_cli::skills::registry::LifecycleClaim],
) -> Vec<String> {
    let importance_for = |pid: &str| -> i32 {
        claims
            .iter()
            .find(|c| c.plugin == pid)
            .map(|c| c.importance)
            .unwrap_or(0)
    };
    let mut keys: Vec<&(String, Option<String>)> = sidecars.iter().collect();
    keys.sort_by(|a, b| {
        let imp_a = importance_for(&a.0);
        let imp_b = importance_for(&b.0);
        imp_b
            .cmp(&imp_a)
            .then_with(|| {
                let an = a.1.as_deref().unwrap_or(a.0.as_str());
                let bn = b.1.as_deref().unwrap_or(b.0.as_str());
                an.cmp(bn)
            })
            .then_with(|| a.0.cmp(&b.0))
    });
    keys.into_iter().map(|(p, _)| p.clone()).collect()
}

/// Pure helper backing [`sidecar_pill_span`] — returns the rendered
/// text only. Lives separately so tests can exercise the label logic
/// without spawning a real sidecar process or mounting the full App.
pub(crate) fn sidecar_pill_text(
    label: &str,
    status: &super::sidecar::SidecarUiStatus,
    armed: bool,
    spinner_frame: usize,
) -> String {
    match status {
        super::sidecar::SidecarUiStatus::Loading => format!(" {label}: loading "),
        super::sidecar::SidecarUiStatus::Idle => {
            if armed {
                format!(" \u{25cf} {label} active ")
            } else {
                format!(" \u{25cb} {label} ")
            }
        }
        super::sidecar::SidecarUiStatus::Active { label: state_label } => {
            let spinner_idx = (spinner_frame / 3) % SPINNER_FRAMES.len();
            let frame = SPINNER_FRAMES[spinner_idx];
            format!(" {frame} {label}: {state_label} ")
        }
        super::sidecar::SidecarUiStatus::Error(_) => format!(" \u{26a0} {label} error "),
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)] // test mod precedes helper items in this file
mod sidecar_pill_tests {
    use super::*;

    #[test]
    fn toast_dims_never_panics_on_tiny_terminal() {
        // Regression: a pane resized to 11x2 used to panic in render_toasts_from_snap
        // because the width clamp was clamp(18, 11) — min > max.
        let cases = [
            (50u16, 5usize, 11u16, 2u16), // the reported crash: 11 cols, 2 rows
            (0, 0, 1, 1),                 // 1x1 — absolute minimum
            (200, 50, 0, 0),              // 0x0 — degenerate
            (10, 1, 5, 1),                // narrow + single row
            (100, 20, 200, 100),          // big — normal case
        ];
        for (cw, lc, aw, ah) in cases {
            let (w, h) = toast_dims(cw, lc, aw, ah); // must not panic
            assert!(w >= 1, "width {w} too small for area {aw}x{ah}");
            assert!(w <= aw.clamp(1, 64), "width {w} exceeds area {aw}");
            assert!(h >= 1, "height {h} too small for area {aw}x{ah}");
            assert!(h <= ah.max(1), "height {h} exceeds area {ah}");
        }
    }

    #[test]
    fn pill_uses_display_name_when_set() {
        // Idle, unarmed pill should show the display name.
        let text = sidecar_pill_text(
            "Voice",
            &super::super::sidecar::SidecarUiStatus::Idle,
            false,
            0,
        );
        assert!(
            text.contains("Voice"),
            "expected pill to contain 'Voice', got: {text:?}"
        );
        assert!(
            !text.contains("sidecar"),
            "expected no 'sidecar' fallback, got: {text:?}"
        );
    }

    #[test]
    fn pill_falls_back_to_sidecar_when_no_display_name() {
        let text = sidecar_pill_text(
            "sidecar",
            &super::super::sidecar::SidecarUiStatus::Idle,
            false,
            0,
        );
        assert!(text.contains("sidecar"), "got: {text:?}");
    }

    #[test]
    fn pill_error_state_uses_display_name() {
        let text = sidecar_pill_text(
            "Voice",
            &super::super::sidecar::SidecarUiStatus::Error("oops".into()),
            false,
            0,
        );
        assert!(text.contains("Voice error"), "got: {text:?}");
    }

    #[test]
    fn pill_active_state_shows_plugin_supplied_label() {
        let text = sidecar_pill_text(
            "Plugin",
            &super::super::sidecar::SidecarUiStatus::Active {
                label: "Working".into(),
            },
            true,
            0,
        );
        assert!(text.contains("Plugin"), "got: {text:?}");
        assert!(text.contains("Working"), "got: {text:?}");
    }

    #[test]
    fn pill_loading_state_shows_label() {
        let text = sidecar_pill_text(
            "Voice",
            &super::super::sidecar::SidecarUiStatus::Loading,
            false,
            0,
        );
        assert!(text.contains("Voice"), "got: {text:?}");
        assert!(text.contains("loading"), "got: {text:?}");
    }

    fn claim(
        plugin: &str,
        command: &str,
        importance: i32,
    ) -> synaps_cli::skills::registry::LifecycleClaim {
        synaps_cli::skills::registry::LifecycleClaim {
            plugin: plugin.into(),
            command: command.into(),
            settings_category: None,
            display_name: command.into(),
            importance,
        }
    }

    #[test]
    fn multi_segment_pill_orders_by_importance_desc() {
        // alpha @ 10, beta @ 90 — beta should come first.
        let claims = vec![claim("alpha", "alpha", 10), claim("beta", "beta", 90)];
        let inputs = vec![
            ("alpha".to_string(), Some("Alpha".to_string())),
            ("beta".to_string(), Some("Beta".to_string())),
        ];
        let order = super::order_sidecar_pills(&inputs, &claims);
        assert_eq!(order, vec!["beta".to_string(), "alpha".to_string()]);
    }

    #[test]
    fn multi_segment_pill_tiebreaks_alphabetical_by_display_name() {
        // Both importance 50 — Alpha before Beta by display name.
        let claims = vec![claim("p2", "p2", 50), claim("p1", "p1", 50)];
        let inputs = vec![
            ("p2".to_string(), Some("Alpha".to_string())),
            ("p1".to_string(), Some("Beta".to_string())),
        ];
        let order = super::order_sidecar_pills(&inputs, &claims);
        assert_eq!(order, vec!["p2".to_string(), "p1".to_string()]);
    }

    #[test]
    fn active_tasks_progress_line_renders_fraction() {
        let mut tasks = synaps_cli::extensions::active_tasks::ActiveTasks::new();
        tasks.apply(synaps_cli::extensions::tasks::TaskEvent::Start {
            id: "dl".into(),
            label: "Download base".into(),
            kind: synaps_cli::extensions::tasks::TaskKind::Download,
        });
        tasks.apply(synaps_cli::extensions::tasks::TaskEvent::Update {
            id: "dl".into(),
            current: Some(50),
            total: Some(100),
            message: Some("half".into()),
        });
        let _ = render_active_tasks_line(&tasks, 80);
    }
}

use super::app::SPINNER_FRAMES;
use super::markdown::format_tokens;
use super::theme::THEME;
use super::view_model::{RenderPatch, ViewInputs};

/// Generate a bash execution trace animation string and its pulsing color.
/// Returns (trace_string, Color) for use in Span styling.
pub(crate) fn bash_trace(spinner_frame: usize) -> (String, Color) {
    const CHARS: [char; 8] = [' ', '░', '▒', '▓', '█', '▓', '▒', '░'];
    const WIDTH: usize = 14;
    let offset = (spinner_frame / 2) % (WIDTH + CHARS.len());
    let trace: String = (0..WIDTH)
        .map(|i| {
            let dist = if offset >= i {
                offset - i
            } else {
                WIDTH + CHARS.len()
            };
            if dist < CHARS.len() {
                CHARS[dist]
            } else {
                ' '
            }
        })
        .collect();
    let pulse = (spinner_frame as f64 / 15.0).sin() * 0.3 + 0.7;
    let Color::Rgb(br, bg, bb) = THEME.load().border_active else {
        return (trace, Color::Reset);
    };
    let color = Color::Rgb(
        (br as f64 * pulse) as u8,
        (bg as f64 * pulse) as u8,
        (bb as f64 * pulse) as u8,
    );
    (trace, color)
}

/// Format a tool name for display. Returns (icon, display_name, optional_server_tag).
/// MCP tools like "ext__byteray__read_pseudocode" become ("⟫", "read_pseudocode", Some("byteray"))
pub(crate) fn format_tool_name(tool_name: &str) -> (&'static str, String, Option<String>) {
    if tool_name.starts_with("ext__") {
        let parts: Vec<&str> = tool_name.splitn(3, "__").collect();
        let server = parts.get(1).unwrap_or(&"mcp").to_string();
        let tool = parts.get(2).unwrap_or(&tool_name).to_string();
        ("\u{27EB}", tool, Some(server)) // ⟫
    } else {
        let icon = match tool_name {
            "bash" => "\u{276F}",     // ❯  the deck / shell
            "read" => "\u{25A4}",     // ▤  data in
            "write" => "\u{270E}",    // ✎  mutation
            "edit" => "\u{2726}",     // ✦  surgical
            "grep" => "\u{2315}",     // ⌕  scan
            "find" => "\u{2756}",     // ❖  locate
            "ls" => "\u{2263}",       // ≣  listing
            "subagent" => "\u{25C8}", // ◈  spawn
            _ => "\u{2022}",          // •
        };
        (icon, tool_name.to_string(), None)
    }
}

/// Per-tool gutter/accent colour, read from the active theme.
/// When a theme field is `Color::Reset` (the default sentinel), we derive a
/// colour from the theme's own semantic palette so every theme looks coherent.
pub(crate) fn tool_accent(tool_name: &str) -> Color {
    let t = THEME.load();

    /// Resolve a raw tool_* field: if Reset, fall back to `derived`.
    #[inline]
    fn resolve(raw: Color, derived: Color) -> Color {
        if raw == Color::Reset {
            derived
        } else {
            raw
        }
    }

    if tool_name.starts_with("ext__") {
        return resolve(t.tool_ext, t.event_icon);
    }

    match tool_name {
        "bash" => resolve(t.tool_bash, t.tool_result_ok),
        "read" => resolve(t.tool_read, t.claude_label),
        "write" => resolve(t.tool_write, t.warning_color),
        "edit" => resolve(t.tool_edit, t.cost_color),
        "grep" => resolve(t.tool_grep, t.table_header_color),
        "find" => resolve(t.tool_find, t.subagent_name),
        "ls" => resolve(t.tool_ls, t.table_cell_color),
        "subagent" => resolve(t.tool_subagent, t.subagent_name),
        _ => resolve(t.tool_generic, t.tool_label),
    }
}

pub(crate) fn boot_effect() -> Effect {
    use tachyonfx::Motion as FxDir;
    let Color::Rgb(r, g, b) = THEME.load().bg else {
        return fx::sleep(0);
    };
    fx::parallel(&[
        // CRT-style scanline reveal, top-to-bottom, clean (no randomness) with a tight gradient trail
        fx::sweep_in(
            FxDir::UpToDown,
            10,
            0,
            Color::Rgb(
                r.saturating_add(10),
                g.saturating_add(15),
                b.saturating_add(20),
            ),
            (750, Interpolation::QuintOut),
        ),
        // long, slow fade from pure black — elegant deceleration
        fx::fade_from_fg(
            Color::Rgb(
                r.saturating_add(2),
                g.saturating_add(3),
                b.saturating_add(5),
            ),
            (750, Interpolation::QuintOut),
        ),
    ])
}

pub(crate) fn quit_effect() -> Effect {
    use tachyonfx::Motion as FxDir;
    let Color::Rgb(r, g, b) = THEME.load().muted else {
        return fx::sleep(0);
    };
    fx::sequence(&[
        fx::hsl_shift_fg([180.0, -40.0, 0.0], (180, Interpolation::QuadOut)),
        fx::parallel(&[
            fx::sweep_out(
                FxDir::DownToUp,
                18,
                12,
                Color::Rgb(r, g, b),
                (650, Interpolation::QuadIn),
            ),
            fx::dissolve((650, Interpolation::QuadIn)),
            fx::fade_to_fg(Color::Black, (650, Interpolation::QuadIn)),
        ]),
    ])
}

/// Render the first generic extension active task as a single sticky progress line.
pub(crate) fn render_active_tasks_line<'a>(
    tasks: &'a synaps_cli::extensions::active_tasks::ActiveTasks,
    width: u16,
) -> Paragraph<'a> {
    let theme = THEME.load();
    let Some(task) = tasks.iter().next() else {
        return Paragraph::new(Line::from(""));
    };
    let pct = task.fraction().map(|f| (f * 100.0).round() as u32);
    let bar_width = ((width as usize).saturating_sub(42)).clamp(8, 28);
    let fill = task
        .fraction()
        .map(|f| (f * bar_width as f32).round() as usize)
        .unwrap_or(0)
        .min(bar_width);
    let bar = format!("{}{}", "█".repeat(fill), "░".repeat(bar_width - fill));
    let status = if let Some(err) = &task.error {
        format!("✘ {}: {}", task.label, err)
    } else if task.done {
        format!("✓ {}", task.label)
    } else {
        let pct_text = pct
            .map(|p| format!("{p}%"))
            .unwrap_or_else(|| "?%".to_string());
        match &task.message {
            Some(msg) if !msg.is_empty() => {
                format!("⟳ {} [{}] {}  {}", task.label, bar, pct_text, msg)
            }
            _ => format!("⟳ {} [{}] {}", task.label, bar, pct_text),
        }
    };
    Paragraph::new(Line::from(Span::styled(
        status,
        Style::default().fg(theme.help_fg),
    )))
}

// ══════════════════════════════════════════════════════════════════════════════
// Render split: build_render_model (main task) + render_frame (render thread)
// ══════════════════════════════════════════════════════════════════════════════

use super::render_model::{
    GhostHint, RenderModel, SecretPromptSnap, SidecarPillSnap, SubagentSnap,
};

/// Build a [`RenderModel`] snapshot from the narrow render-input view.
///
/// This is the **main-side extraction step**.  It reads [`ViewInputs`] — the
/// T199.2 seam that names exactly which `App` state the render may consume —
/// computes every derived value the renderer needs, and returns a fully-owned
/// snapshot plus a [`RenderPatch`] of the mutations the caller must apply to
/// authoritative `App` state (the builder itself mutates nothing on `App`;
/// the only `&mut` it holds is the transcript store's cache-sync seam).
///
/// Returns `None` when the frame should be skipped (gamba casino owns the
/// terminal, same semantics as the old early-return in `draw()`).
pub(crate) fn build_render_model(
    inputs: &mut ViewInputs<'_>,
    runtime: &impl agent_engine::session::RuntimeRead,
    registry: &std::sync::Arc<synaps_cli::skills::registry::CommandRegistry>,
    term_size: ratatui::layout::Size,
) -> Option<(std::sync::Arc<RenderModel>, RenderPatch)> {
    // ── 1. gamba gate ─────────────────────────────────────────────────────────
    if inputs.gamba_active {
        return None;
    }

    // ── 2. Layout math (mirrors draw.rs pre-closure block) ────────────────────
    let has_subagents = !inputs.subagents.is_empty();
    let subagent_height: u16 = if has_subagents {
        (inputs.subagents.len() as u16 + 2).min(8)
    } else {
        0
    };
    let input_inner_width = term_size
        .width
        .saturating_sub(2 * super::neon_prompt::INSET_X);
    let (input_lines, _, _) =
        super::view_model::input_wrap_info(&inputs.input, inputs.cursor_pos, input_inner_width);
    let max_input_lines: u16 = 10;
    let input_height = input_lines.min(max_input_lines) + 2;
    let download_height: u16 = if !inputs.active_tasks.is_empty() {
        1
    } else {
        0
    };
    let protected_bottom_rows = subagent_height
        .saturating_add(download_height)
        .saturating_add(input_height)
        .saturating_add(1); // footer

    // Use AppAreas to derive msg_area — single source of truth for the outer layout.
    // body sits at y=1 (after the 1-line header) with Min(1) height matching the
    // old saturating_sub chain + .max(1), so the resulting Rect is identical.
    let area = ratatui::layout::Rect {
        x: 0,
        y: 0,
        width: term_size.width,
        height: term_size.height,
    };
    let msg_area =
        AppAreas::from_heights(area, subagent_height, download_height, input_height).body;

    // ── 3. Transcript visible window ──────────────────────────────────────────
    // One call folds the old §3–§6 block: cache sync → scroll growth/clamp
    // bookkeeping → viewport geometry + visible range recording → O(viewport)
    // slice clone + selection snapshot (design §3.5). Ephemeral App state the
    // renderer needs crosses the seam via RenderCtx.
    let vw = {
        let ctx = super::transcript::RenderCtx {
            spinner_frame: inputs.spinner_frame,
            streaming: inputs.streaming,
            agent_name: inputs.agent_name,
        };
        inputs.transcript.visible_window(msg_area, &ctx)
    };

    // ── 7. Subagent snapshots ─────────────────────────────────────────────────
    let subagents: Vec<SubagentSnap> = inputs
        .subagents
        .iter()
        .map(|sa| SubagentSnap {
            name: sa.name.clone(),
            status: sa.status.clone(),
            elapsed_secs: sa
                .duration_secs
                .unwrap_or_else(|| sa.start_time.elapsed().as_secs_f64()),
            done: sa.done,
        })
        .collect();

    // ── 8. Sidecar pills ──────────────────────────────────────────────────────
    let sidecar_pills: Vec<SidecarPillSnap> = {
        if inputs.sidecars.is_empty() {
            Vec::new()
        } else {
            let claims = registry.lifecycle_claims();
            let pill_inputs: Vec<(String, Option<String>)> = inputs
                .sidecars
                .iter()
                .map(|(pid, st)| (pid.clone(), st.display_name.clone()))
                .collect();
            let order = order_sidecar_pills(&pill_inputs, &claims);
            order
                .into_iter()
                .filter_map(|pid| {
                    let st = inputs.sidecars.get(&pid)?;
                    Some(SidecarPillSnap {
                        plugin_id: pid,
                        display_name: st.display_name.clone(),
                        status: st.status.clone(),
                        armed: st.armed,
                    })
                })
                .collect()
        }
    };

    // ── 9. Ghost hint ─────────────────────────────────────────────────────────
    let ghost_hint: Option<GhostHint> = {
        if inputs.input.starts_with('/')
            && inputs.input.len() > 1
            && !inputs.input[1..].contains(' ')
        {
            let partial = &inputs.input[1..];
            let commands = super::commands::all_commands_with_skills(registry);
            let prefix_matches: Vec<&String> =
                commands.iter().filter(|c| c.starts_with(partial)).collect();
            if prefix_matches.len() == 1 {
                let cmd = prefix_matches[0];
                if cmd.as_str() != partial {
                    let ghost_text = if let Some(rest) = cmd.strip_prefix(partial) {
                        rest.to_string()
                    } else {
                        format!(" → /{}", cmd)
                    };
                    Some(GhostHint {
                        ghost_text,
                        match_badge: None,
                    })
                } else {
                    None
                }
            } else if prefix_matches.len() > 1 {
                Some(GhostHint {
                    ghost_text: String::new(),
                    match_badge: Some(format!("{} matches", prefix_matches.len())),
                })
            } else {
                None
            }
        } else {
            None
        }
    };

    // ── 10. Toasts ────────────────────────────────────────────────────────────
    let toasts: Vec<super::toast::Toast> = inputs.toasts.visible().cloned().collect();

    // ── 11. Modals ────────────────────────────────────────────────────────────
    let settings = inputs.settings.clone().map(|s| {
        let snap = super::settings::RuntimeSnapshot::from_runtime_with_health(
            runtime,
            registry,
            inputs.model_health.clone(),
        );
        (s, snap)
    });
    let plugins = inputs.plugins.clone();
    let models = inputs.models.clone();
    // ── help_find visible_height (returned as a RenderPatch, spec §4-A) ──────
    // `help_find::render` computes visible_height from the terminal geometry
    // and calls `set_visible_height` on its &mut state.  Because the render
    // runs on a separate thread it was previously mutating a throwaway clone,
    // so the modal's scroll window was wrong on first open at a non-default
    // size.  The geometry is mirrored here on the main side; the value is
    // applied to the snapshot clone (so this frame matches the old in-builder
    // write-back byte-for-byte) and returned in the RenderPatch for the
    // caller to apply to the authoritative App state.  The builder itself no
    // longer mutates App.
    let mut patch = RenderPatch::default();
    let mut help_find = inputs.help_find.clone();
    if let Some(ref mut hf) = help_find {
        let area_w = term_size.width;
        let area_h = term_size.height;
        let _modal_w = ((area_w as u32 * 8 / 10) as u16).max(50).min(area_w);
        let modal_h = ((area_h as u32 * 8 / 10) as u16).max(14).min(area_h);
        // block.inner subtracts 1px border on each side → -2 height
        // padded_rect(inner, 2, 1) subtracts 1px vertical pad each side → -2 height
        let inner_h = modal_h.saturating_sub(2).saturating_sub(2);
        // Layout [Length(2), Min(1), Length(1)]: chunk[1] = inner_h - 3
        let visible_h = (inner_h.saturating_sub(3) as usize).max(1);
        hf.set_visible_height(visible_h);
        patch.help_find_visible_height = Some(visible_h);
    }

    // ── 12. Secret prompt ─────────────────────────────────────────────────────
    let secret_prompt = inputs.secret_prompts.active().map(|p| SecretPromptSnap {
        kind: p.kind,
        title: p.title.clone(),
        prompt: p.prompt.clone(),
        masked_buffer_chars: p.buffer.chars().count(),
    });

    // ── 13. Runtime strings ───────────────────────────────────────────────────
    let runtime_model = runtime.model().to_string();
    let runtime_thinking = runtime.thinking_level().to_string();

    // ── 14. Assemble ──────────────────────────────────────────────────────────
    let model = std::sync::Arc::new(RenderModel {
        status_text: inputs.status_text.clone(),
        streaming: inputs.streaming,
        spinner_frame: inputs.spinner_frame,
        sidecar_pills,
        runtime_model,
        runtime_thinking,
        lines: vw.lines,
        lines_width: vw.lines_width,
        scroll_back: vw.scroll_back,
        selection: vw.selection,
        messages_empty: vw.is_empty,
        logo_build_t: inputs.logo_build_t,
        logo_dismiss_t: inputs.logo_dismiss_t,
        subagents,
        active_tasks: std::sync::Arc::clone(inputs.active_tasks),
        input: inputs.input.clone(),
        cursor_pos: inputs.cursor_pos,
        ghost_hint,
        prompt_fx: inputs.prompt_fx,
        show_full_output: inputs.transcript.show_full_output(),
        session_cost: inputs.session_cost,
        total_input_tokens: inputs.total_input_tokens,
        total_output_tokens: inputs.total_output_tokens,
        total_cache_read_tokens: inputs.total_cache_read_tokens,
        total_cache_creation_tokens: inputs.total_cache_creation_tokens,
        total_cache_write_1h: inputs.total_cache_write_1h,
        last_turn_context: inputs.last_turn_context,
        last_turn_context_window: inputs.last_turn_context_window,
        toasts,
        settings,
        plugins,
        models,
        help_find,
        effort: inputs.effort.clone(),
        secret_prompt,
        // P7.8: stack-order snapshot driving the modal draw loop below.
        modal_order: inputs.modal_stack.iter_bottom_up().collect(),
        protected_bottom_rows,
    });
    Some((model, patch))
}

/// The footer context bar's colour for this much of the window used:
/// `border_active` under 50%, `status_streaming` under 75%, `error_color`
/// above. With no turn yet (the bar is hidden) it is the under-50% colour.
/// Shared with the prompt's streaming glow.
fn context_bar_color(theme: &super::theme::Theme, context: u64, window: u64) -> Color {
    let ratio = (context as f64 / window.max(1) as f64).min(1.0);
    if ratio < 0.5 {
        theme.border_active
    } else if ratio < 0.75 {
        theme.status_streaming
    } else {
        theme.error_color
    }
}

/// Render one frame from a [`RenderModel`] snapshot.
///
/// Runs on the dedicated render `std::thread`, which maintains its own
/// monotonic clock for tachyonfx effect timing so main-loop pressure never
/// compresses animation time.
///
/// **Invariant**: this function takes NO `&App` and accesses NO `App` field.
/// All data comes from `model`.  If it compiles without `App`, snapshot
/// completeness is proven.
pub(crate) fn render_frame(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    model: &RenderModel,
    caps: Option<&super::termcaps::TermCaps>,
    boot_fx: &mut Option<Effect>,
    exit_fx: &mut Option<Effect>,
    last_frame: &mut std::time::Instant,
) -> io::Result<()> {
    // Render-thread-local clock: elapsed since the last call to render_frame.
    // This is the correct place to measure effect timing — independent of main
    // loop pressure.  If the main task is busy, the render thread still ticks
    // effects at its own cadence.
    let elapsed = last_frame.elapsed();
    *last_frame = std::time::Instant::now();

    // DEC 2026 synchronized output: bracket the edge-scrub AND the ratatui
    // diff flush together so terminals that support it (kitty, wezterm, foot,
    // iTerm2 ≥3.5, VTE ≥0.70) don't render partial frames. Terminals that
    // don't support mode 2026 ignore the private-mode sequences harmlessly.
    //
    // P16.3 gate: emit the bracket unless the DA1-fenced negotiation
    // AFFIRMATIVELY reported mode 2026 unsupported. Unknown caps / DA1 timeout
    // (`sync_output_enabled(..) == true`) keep today's UNCONDITIONAL bracket —
    // harmless log-honesty, since non-2026 terminals ignore it anyway. Begin
    // and End are gated by the SAME `sync` bool so we never leave an unmatched
    // Begin (which would freeze the display).
    let sync = super::termcaps::sync_output_enabled(caps);
    if sync {
        execute!(io::stdout(), BeginSynchronizedUpdate)?;
    }

    let frame_result = (|| -> io::Result<()> {
        // P16.3 gate: the scrub itself is gated on tmux provenance inside
        // `scrub_crossterm_terminal_edges` (short-circuits before any size
        // query when caps affirmatively say no-tmux).
        //
        // Fix A (tmux-canvas-edge-scrub): the scrub area is entirely inside
        // the transcript body (edge_scrub_area skips 2 top rows for the header
        // + msg top border, and `protected_bottom_rows` for the subagent,
        // download, input, and footer chrome). In opaque mode the physical
        // blanks must land with the transcript CANVAS color
        // (`message_background()`), rather than inheriting stale SGR state.
        // In invisible mode no canvas color may be emitted at all: it would
        // form an opaque strip at the physical first/last terminal columns.
        // The scrub physically paints its blanks with `message_background()`.
        // In invisible mode that would reintroduce an opaque strip in the first
        // and last columns after the canvas was deliberately cleared, so only
        // run it when the conversation canvas itself is opaque.
        if background_is_opaque() {
            super::viewport::scrub_crossterm_terminal_edges(
                terminal,
                caps,
                model.protected_bottom_rows,
                Style::default().bg(THEME.load().message_background()),
            )?;
        }

        terminal.draw(|frame| render_frame_into(frame, model, boot_fx, exit_fx, elapsed))?;
        Ok(())
    })();

    // Always end the synchronized update if we opened one, even if rendering
    // failed. Leaving mode 2026 open would freeze the terminal display.
    if sync {
        execute!(io::stdout(), EndSynchronizedUpdate).ok();
    }

    frame_result
}

/// Backend-agnostic frame body — everything `render_frame` draws, minus the
/// crossterm-specific edge scrub and terminal ownership.
///
/// This is the seam the headless test harness renders through: production
/// calls it from `render_frame` inside `Terminal::<CrosstermBackend>::draw`;
/// tests call it inside `Terminal::<TestBackend>::draw`. Same pixels, no TTY.
///
/// **Invariant**: takes NO `&App` — all data comes from the [`RenderModel`]
/// snapshot, same completeness proof as `render_frame`.
pub(crate) fn render_frame_into(
    frame: &mut ratatui::Frame<'_>,
    model: &RenderModel,
    boot_fx: &mut Option<Effect>,
    exit_fx: &mut Option<Effect>,
    elapsed: std::time::Duration,
) {
    if background_is_opaque() {
        frame.render_widget(
            Block::default().style(Style::default().bg(THEME.load().bg)),
            frame.area(),
        );
    }

    // ── Layout ────────────────────────────────────────────────────────────
    let has_subagents = !model.subagents.is_empty();
    let subagent_height: u16 = if has_subagents {
        (model.subagents.len() as u16 + 2).min(8)
    } else {
        0
    };
    let input_inner_width = frame
        .area()
        .width
        .saturating_sub(2 * super::neon_prompt::INSET_X);
    let max_input_lines: u16 = 10;
    let (input_lines, cursor_row, cursor_col) =
        super::view_model::input_wrap_info(&model.input, model.cursor_pos, input_inner_width);
    let input_height = input_lines.min(max_input_lines) + 2;
    let download_height: u16 = if !model.active_tasks.is_empty() { 1 } else { 0 };

    let AppAreas {
        header: header_area,
        body,
        subagent: subagent_area,
        download: download_area,
        input: input_area,
        footer: footer_area,
    } = AppAreas::from_heights(frame.area(), subagent_height, download_height, input_height);

    // ── Header ────────────────────────────────────────────────────────────
    let spinner_idx = (model.spinner_frame / 3) % SPINNER_FRAMES.len();
    let status_span = if let Some(ref status) = model.status_text {
        Span::styled(
            format!(" {} {} ", SPINNER_FRAMES[spinner_idx], status),
            Style::default().fg(THEME.load().status_streaming),
        )
    } else if has_subagents {
        let active = model.subagents.iter().filter(|s| !s.done).count();
        let done = model.subagents.iter().filter(|s| s.done).count();
        let spinner = if active > 0 {
            SPINNER_FRAMES[spinner_idx]
        } else {
            "\u{2714}"
        };
        Span::styled(
            format!(
                " {} {} agent{} ({} done) ",
                spinner,
                active,
                if active != 1 { "s" } else { "" },
                done
            ),
            Style::default().fg(THEME.load().subagent_name),
        )
    } else if model.streaming {
        let pulse = ((model.spinner_frame as f64 / 20.0).sin() * 0.3 + 0.7).max(0.4);
        let (sr, sg, sb) = match THEME.load().status_streaming {
            Color::Rgb(r, g, b) => (r, g, b),
            _ => (128, 128, 128),
        };
        Span::styled(
            " \u{25cf} streaming ",
            Style::default().fg(Color::Rgb(
                (sr as f64 * pulse) as u8,
                (sg as f64 * pulse) as u8,
                (sb as f64 * pulse) as u8,
            )),
        )
    } else {
        Span::styled(
            " \u{25cb} ready ",
            Style::default().fg(THEME.load().status_ready),
        )
    };
    let version_span = Span::styled(
        concat!("v", env!("CARGO_PKG_VERSION"), " · ", env!("GIT_HASH"), " "),
        Style::default().fg(THEME.load().muted),
    );
    let header = Paragraph::new(Line::from({
        let mut spans = vec![
            Span::styled(
                "  Synaps",
                Style::default()
                    .fg(THEME.load().header_fg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("CLI ", Style::default().fg(THEME.load().muted)),
            Span::styled("\u{2502}", Style::default().fg(THEME.load().border)),
            status_span,
        ];
        // Sidecar pills from snapshot
        for pill in &model.sidecar_pills {
            spans.push(Span::styled(
                "\u{2502}",
                Style::default().fg(THEME.load().border),
            ));
            let label = pill.display_name.as_deref().unwrap_or("sidecar");
            let text = sidecar_pill_text(label, &pill.status, pill.armed, model.spinner_frame);
            let color = match &pill.status {
                super::sidecar::SidecarUiStatus::Idle => {
                    if pill.armed {
                        let pulse =
                            ((model.spinner_frame as f64 / 18.0).sin() * 0.3 + 0.7).max(0.4);
                        let base = match THEME.load().status_streaming {
                            Color::Rgb(r, g, b) => (r, g, b),
                            _ => (220, 80, 80),
                        };
                        Color::Rgb(
                            (base.0 as f64 * pulse) as u8,
                            (base.1 as f64 * pulse) as u8,
                            (base.2 as f64 * pulse) as u8,
                        )
                    } else {
                        THEME.load().muted
                    }
                }
                super::sidecar::SidecarUiStatus::Active { .. } => {
                    let pulse = ((model.spinner_frame as f64 / 18.0).sin() * 0.3 + 0.7).max(0.4);
                    let base = match THEME.load().status_streaming {
                        Color::Rgb(r, g, b) => (r, g, b),
                        _ => (220, 80, 80),
                    };
                    Color::Rgb(
                        (base.0 as f64 * pulse) as u8,
                        (base.1 as f64 * pulse) as u8,
                        (base.2 as f64 * pulse) as u8,
                    )
                }
                super::sidecar::SidecarUiStatus::Loading => THEME.load().muted,
                super::sidecar::SidecarUiStatus::Error(_) => Color::Red,
            };
            spans.push(Span::styled(
                text,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ));
        }
        let used: usize = spans.iter().map(|s| s.content.len()).sum();
        let total_w = header_area.width as usize;
        if total_w > used + version_span.content.len() {
            let pad = total_w - used - version_span.content.len();
            spans.push(Span::raw(" ".repeat(pad)));
        }
        spans.push(version_span);
        spans
    }))
    .style(Style::default().bg(THEME.load().bg));
    frame.render_widget(header, header_area);

    // ── Messages ──────────────────────────────────────────────────────────
    let msg_area = body;
    // model.lines IS the visible window (sliced on the main side) — render the whole arc.
    let visible: Vec<ratatui::text::Line> = model.lines.to_vec();
    let visible_is_empty = visible.is_empty();

    let msg_block = Block::default()
        .borders(Borders::TOP)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(THEME.load().border))
        .padding(Padding::horizontal(1));
    let msg_inner = msg_block.inner(msg_area);
    let messages_widget = Paragraph::new(visible).block(msg_block.clone());
    // The conversation canvas is the ONLY surface the background toggle owns.
    // Opaque paints the themed canvas; invisible falls back to 0.7.0's exact
    // behaviour (`Clear`) so the terminal background shows through while the
    // header, input, and footer chrome stay painted, unchanged from 0.7.0.
    if background_is_opaque() {
        frame.render_widget(
            Block::default().style(Style::default().bg(THEME.load().message_background())),
            msg_area,
        );
    } else {
        frame.render_widget(Clear, msg_area);
    }
    if model.secret_prompt.is_some() {
        let blank = Paragraph::new(Vec::<ratatui::text::Line>::new()).block(msg_block);
        frame.render_widget(blank, msg_area);
    } else {
        frame.render_widget(messages_widget, msg_area);
    }

    // Text selection overlay
    if let Some((sc, sr, ec, er)) = model.selection {
        let content_x = msg_inner.x;
        let content_y = msg_inner.y;
        let content_h = msg_inner.height;
        let content_w = msg_inner.width;
        for y in sr..=er {
            if y < content_y || y >= content_y + content_h {
                continue;
            }
            let row_start = if y == sr {
                sc.max(content_x)
            } else {
                content_x
            };
            let row_end = if y == er {
                ec.min(content_x + content_w)
            } else {
                content_x + content_w
            };
            for x in row_start..row_end {
                if x >= content_x && x < content_x + content_w {
                    if let Some(cell) = frame.buffer_mut().cell_mut((x, y)) {
                        let fg = cell.fg;
                        let bg = cell.bg;
                        cell.set_fg(bg);
                        cell.set_bg(match fg {
                            Color::Reset => THEME.load().border_active,
                            other => other,
                        });
                    }
                }
            }
        }
    }

    // Logo
    let show_logo = model.messages_empty || model.logo_dismiss_t.is_some();
    if show_logo && visible_is_empty {
        let ascii_art: Vec<&str> = vec![
            r" ███████ ██    ██ ███   ██  █████  ██████  ███████",
            r" ██       ██  ██  ████  ██ ██   ██ ██   ██ ██    ",
            r" ███████   ████   ██ ██ ██ ███████ ██████  ███████",
            r"      ██    ██    ██  ████ ██   ██ ██           ██",
            r" ███████    ██    ██   ███ ██   ██ ██      ███████",
        ];

        let art_display_widths: Vec<usize> = ascii_art.iter().map(|l| display_width(l)).collect();
        let max_art_width = art_display_widths.iter().copied().max().unwrap_or(0);
        let avail_w = msg_area.width as usize;
        let avail_h = msg_area.height as usize;
        let art_height = ascii_art.len();
        let sub_text = "neural interface ready";
        let sub_width = sub_text.chars().count();
        let total_block = art_height + 3;

        if avail_h >= total_block && avail_w >= max_art_width + 2 {
            let center_y = msg_area.y + msg_area.height / 2;
            let dismiss_t = model.logo_dismiss_t.unwrap_or(0.0);
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();

            if dismiss_t < 0.001 {
                let phase1 = ((t % 4000) as f64 / 4000.0 * std::f64::consts::PI * 2.0).sin();
                let phase2 = ((t % 6500) as f64 / 6500.0 * std::f64::consts::PI * 2.0).sin();
                let breathe = phase1 * 0.7 + phase2 * 0.3;
                let (ar, ag, ab) = match THEME.load().border_active {
                    Color::Rgb(r, g, b) => (r, g, b),
                    _ => (128, 128, 128),
                };
                let breathe_scale = 0.7 + 0.3 * breathe;
                let art_style = Style::default()
                    .fg(Color::Rgb(
                        (ar as f64 * breathe_scale) as u8,
                        (ag as f64 * breathe_scale) as u8,
                        (ab as f64 * breathe_scale) as u8,
                    ))
                    .add_modifier(Modifier::BOLD);
                let (mr, mg, mb) = match THEME.load().muted {
                    Color::Rgb(r, g, b) => (r, g, b),
                    _ => (64, 64, 64),
                };
                let sub_style = Style::default().fg(Color::Rgb(
                    (mr as f64 * breathe_scale) as u8,
                    (mg as f64 * breathe_scale) as u8,
                    (mb as f64 * breathe_scale) as u8,
                ));
                let build_t = model.logo_build_t.unwrap_or(1.0);
                let start_y = center_y.saturating_sub((total_block as u16) / 2);
                let art_x = msg_area.x + (avail_w as u16).saturating_sub(max_art_width as u16) / 2;
                for (j, line) in ascii_art.iter().enumerate() {
                    let x = art_x;
                    let y = start_y + j as u16;
                    if y >= msg_area.y && y < msg_area.y + msg_area.height {
                        let clamped_w = max_art_width.min(avail_w);
                        if build_t >= 1.0 {
                            let text: String = line.chars().take(clamped_w).collect();
                            let area = ratatui::layout::Rect {
                                x,
                                y,
                                width: clamped_w as u16,
                                height: 1,
                            };
                            frame.render_widget(
                                Paragraph::new(Line::from(Span::styled(text, art_style))),
                                area,
                            );
                        } else {
                            let mut built = String::with_capacity(clamped_w);
                            let build_chars: &[char] = &['░', '▒', '▓'];
                            for (ci, ch) in line.chars().take(clamped_w).enumerate() {
                                let inv_row = (art_height - 1 - j) as f64;
                                let inv_col = (max_art_width.saturating_sub(ci + 1)) as f64;
                                let diag = (inv_row + inv_col)
                                    / (art_height as f64 + max_art_width as f64);
                                if build_t >= diag {
                                    let lp = ((build_t - diag) / 0.15).min(1.0);
                                    if lp < 1.0 && ch != ' ' {
                                        built.push(
                                            build_chars[(lp * build_chars.len() as f64) as usize],
                                        );
                                    } else {
                                        built.push(ch);
                                    }
                                } else {
                                    built.push(' ');
                                }
                            }
                            let area = ratatui::layout::Rect {
                                x,
                                y,
                                width: clamped_w as u16,
                                height: 1,
                            };
                            frame.render_widget(
                                Paragraph::new(Span::styled(built, art_style)),
                                area,
                            );
                        }
                    }
                }
                if build_t >= 1.0 {
                    let sub_y = start_y + art_height as u16 + 1;
                    if sub_y >= msg_area.y
                        && sub_y < msg_area.y + msg_area.height
                        && avail_w >= sub_width
                    {
                        let sub_x =
                            msg_area.x + (avail_w as u16).saturating_sub(sub_width as u16) / 2;
                        let area = ratatui::layout::Rect {
                            x: sub_x,
                            y: sub_y,
                            width: sub_width as u16,
                            height: 1,
                        };
                        frame
                            .render_widget(Paragraph::new(Span::styled(sub_text, sub_style)), area);
                    }
                }
            } else {
                let art_style = Style::default()
                    .fg(THEME.load().muted)
                    .add_modifier(Modifier::BOLD);
                let start_y = center_y.saturating_sub((total_block as u16) / 2);
                for (j, line) in ascii_art.iter().enumerate() {
                    let char_w = art_display_widths[j];
                    let x = msg_area.x + (avail_w as u16).saturating_sub(char_w as u16) / 2;
                    let y = start_y + j as u16;
                    if y >= msg_area.y && y < msg_area.y + msg_area.height {
                        let clamped_w = char_w.min(avail_w);
                        let mut dis = String::with_capacity(clamped_w);
                        let dis_chars: &[char] = &['▓', '▒', '░'];
                        for (ci, ch) in line.chars().take(clamped_w).enumerate() {
                            let row = j as f64;
                            let col = ci as f64;
                            let diag = (row + col) / (art_height as f64 + max_art_width as f64);
                            let threshold = diag;
                            if dismiss_t < (1.0 - threshold) {
                                let rem = (1.0 - threshold) - dismiss_t;
                                if rem < 0.15 && ch != ' ' {
                                    let idx =
                                        ((1.0 - rem / 0.15) * dis_chars.len() as f64) as usize;
                                    dis.push(dis_chars[idx.min(dis_chars.len() - 1)]);
                                } else {
                                    dis.push(ch);
                                }
                            } else {
                                dis.push(' ');
                            }
                        }
                        let area = ratatui::layout::Rect {
                            x,
                            y,
                            width: clamped_w as u16,
                            height: 1,
                        };
                        frame.render_widget(Paragraph::new(Span::styled(dis, art_style)), area);
                    }
                }
            }
        }
    }

    // Scroll indicator
    if model.scroll_back > 0 {
        let indicator = format!(" \u{2191}{} ", model.scroll_back);
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

    // ── Subagent Panel ────────────────────────────────────────────────────
    if has_subagents {
        let spinner_idx2 = (model.spinner_frame / 3) % SPINNER_FRAMES.len();
        let mut agent_lines: Vec<ratatui::text::Line> = Vec::new();
        for sa in &model.subagents {
            let elapsed_s = sa.elapsed_secs;
            let time_str = if elapsed_s < 60.0 {
                format!("{:.1}s", elapsed_s)
            } else {
                format!("{}m{:.0}s", (elapsed_s / 60.0) as u32, elapsed_s % 60.0)
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
                let icon = if is_timeout {
                    "  \u{26a0} "
                } else if is_error {
                    "  \u{2718} "
                } else {
                    "  \u{2714} "
                };
                agent_lines.push(ratatui::text::Line::from(vec![
                    Span::styled(icon, Style::default().fg(done_color)),
                    Span::styled(
                        format!("{} ", sa.name),
                        Style::default()
                            .fg(THEME.load().subagent_name)
                            .add_modifier(Modifier::BOLD),
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
                let spinner = SPINNER_FRAMES[spinner_idx2];
                agent_lines.push(ratatui::text::Line::from(vec![
                    Span::styled(
                        format!("  {} ", spinner),
                        Style::default().fg(THEME.load().subagent_name),
                    ),
                    Span::styled(
                        format!("{} ", sa.name),
                        Style::default()
                            .fg(THEME.load().subagent_name)
                            .add_modifier(Modifier::BOLD),
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
        let active = model.subagents.iter().filter(|s| !s.done).count();
        let done = model.subagents.iter().filter(|s| s.done).count();
        let title = if done > 0 && active > 0 {
            format!(" \u{25c8} {} running, {} done ", active, done)
        } else if active > 0 {
            format!(
                " \u{25c8} {} agent{} ",
                active,
                if active != 1 { "s" } else { "" }
            )
        } else {
            format!(" \u{2714} {} done ", done)
        };
        let agent_block = Block::default()
            .title(Span::styled(
                title,
                Style::default()
                    .fg(THEME.load().subagent_name)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(THEME.load().subagent_border))
            .style(Style::default().bg(THEME.load().bg));
        frame.render_widget(
            Paragraph::new(agent_lines).block(agent_block),
            subagent_area,
        );
    }

    // ── Input ─────────────────────────────────────────────────────────────
    // Neon prompt (neon_prompt.rs): a soft-edged slab of light with no drawn
    // border. The text sits in a fixed column `INSET_X` in from the edge, with
    // a hanging indent on every row after the first, so wrapped and
    // multi-line input stays aligned. Wrap math: view_model::input_wrap_info.
    {
        use super::neon_prompt::{self as neon, CursorAt};
        use super::view_model::INPUT_PREFIX_WIDTH;

        let inset = neon::INSET_X;
        let theme = THEME.load();
        // Optional streaming glow (Settings → Streaming glow): it wears the
        // context bar's colour, so it shifts with the bar as the window fills.
        let glow = (model.streaming && neon::streaming_glow_enabled()).then(|| {
            context_bar_color(
                &theme,
                model.last_turn_context,
                model.last_turn_context_window,
            )
        });
        let slab = neon::Slab::new(&theme, model.prompt_fx, input_area.width, glow);
        let text_area = ratatui::layout::Rect {
            x: input_area.x.saturating_add(inset),
            y: input_area.y.saturating_add(1),
            width: input_area.width.saturating_sub(2 * inset),
            height: input_area.height.saturating_sub(2),
        };
        let w = (input_inner_width as usize).max(INPUT_PREFIX_WIDTH + 1);
        let visible_lines = text_area.height.max(1);
        let input_scroll: u16 = if cursor_row >= visible_lines {
            cursor_row - visible_lines + 1
        } else {
            0
        };
        let cursor_at = (text_area.height > 0).then(|| CursorAt {
            x: text_area.x + cursor_col,
            y: text_area.y + cursor_row - input_scroll,
        });
        neon::paint_slab(frame.buffer_mut(), input_area, &slab, cursor_at);

        let prompt_span = if model.streaming {
            Span::styled(
                format!("{} ", SPINNER_FRAMES[spinner_idx]),
                Style::default()
                    .fg(slab.spinner_fg())
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(
                "\u{276f} ",
                Style::default()
                    .fg(slab.prompt_fg())
                    .add_modifier(Modifier::BOLD),
            )
        };
        let text_style = Style::default().fg(slab.text_fg());
        let indent = || Span::raw(" ".repeat(INPUT_PREFIX_WIDTH));
        let mut rows: Vec<Vec<Span>> = Vec::new();
        let mut current_row: Vec<Span> = vec![prompt_span];
        let mut col: usize = INPUT_PREFIX_WIDTH;
        for ch in model.input.chars() {
            if ch == '\n' {
                rows.push(std::mem::replace(&mut current_row, vec![indent()]));
                col = INPUT_PREFIX_WIDTH;
                continue;
            }
            let cw = char_width(ch);
            if col + cw > w && col > INPUT_PREFIX_WIDTH {
                rows.push(std::mem::replace(&mut current_row, vec![indent()]));
                col = INPUT_PREFIX_WIDTH;
            }
            current_row.push(Span::styled(ch.to_string(), text_style));
            col += cw;
        }
        rows.push(current_row);

        if let Some(last_row) = rows.last_mut() {
            if let Some(ref hint) = model.ghost_hint {
                let ghost_style = Style::default()
                    .fg(slab.ghost_fg())
                    .add_modifier(Modifier::ITALIC);
                // Several matches: the count and "tab search" hang in the
                // status tab instead (see below).
                if hint.match_badge.is_none() && !hint.ghost_text.is_empty() {
                    last_row.push(Span::styled(hint.ghost_text.clone(), ghost_style));
                }
            } else if model.input.is_empty() {
                // Placeholder: the prompt in dim italic, then hints that
                // fit whole (Noodle: key bright, word dim), dropped from the
                // right when narrow.
                let room = w.saturating_sub(INPUT_PREFIX_WIDTH + 1);
                let lead = if model.streaming {
                    "agent is working \u{2014} type to steer or queue a follow-up"
                } else {
                    "Ask anything"
                };
                let lead: String = lead.chars().take(room).collect();
                let mut used = display_width(&lead);
                last_row.push(Span::styled(
                    lead,
                    Style::default()
                        .fg(slab.dim_fg())
                        .add_modifier(Modifier::ITALIC),
                ));
                if !model.streaming {
                    let key = Style::default().fg(slab.tone_fg(neon::Tone::SoftKey));
                    let word = Style::default().fg(slab.dim_fg());
                    for (k, wd) in [("/", " commands"), ("alt+enter", " newline")] {
                        let seg = 3 + display_width(k) + display_width(wd);
                        if used + seg > room {
                            break;
                        }
                        last_row.push(Span::raw("   "));
                        last_row.push(Span::styled(k, key));
                        last_row.push(Span::styled(wd, word));
                        used += seg;
                    }
                }
            }
        }
        let lines: Vec<ratatui::text::Line> =
            rows.into_iter().map(ratatui::text::Line::from).collect();
        // No block and no base style: the text patches only its foreground, so
        // the slab painted underneath shows through.
        frame.render_widget(Paragraph::new(lines).scroll((input_scroll, 0)), text_area);

        // Scroll arrows in the prompt column (hanging-indent rows only).
        if text_area.height > 0 {
            let buf = frame.buffer_mut();
            if input_scroll > 0 {
                if let Some(cell) = buf.cell_mut((text_area.x, text_area.y)) {
                    cell.set_symbol("\u{2191}").set_fg(slab.dim_fg());
                }
            }
            let below = input_lines > input_scroll + text_area.height;
            if below && (text_area.height > 1 || input_scroll > 0) {
                if let Some(cell) = buf.cell_mut((text_area.x, text_area.bottom() - 1)) {
                    cell.set_symbol("\u{2193}").set_fg(slab.dim_fg());
                }
            }
        }

        if let Some(at) = cursor_at {
            neon::paint_cursor(frame.buffer_mut(), input_area, &slab, at);
        }

        // Status tab hanging off the bottom edge (key bright, word dim).
        use neon::Tone::{Key, Live, SoftKey, Word};
        let tab: Option<Vec<(String, neon::Tone)>> = if model.streaming {
            Some(vec![
                (format!("{} working ", SPINNER_FRAMES[spinner_idx]), Live),
                (format!("{:.1}s", model.prompt_fx.stream_secs), Word),
                ("   ".into(), Word),
                ("esc".into(), Key),
                (" abort".into(), Word),
            ])
        } else if model
            .ghost_hint
            .as_ref()
            .is_some_and(|h| !h.ghost_text.is_empty())
        {
            Some(vec![("tab".into(), Key), (" complete".into(), Word)])
        } else if let Some(count) = model
            .ghost_hint
            .as_ref()
            .and_then(|h| h.match_badge.clone())
        {
            Some(vec![
                (count, Word),
                ("   ".into(), Word),
                ("tab".into(), Key),
                (" search".into(), Word),
            ])
        } else if input_lines > 1 {
            Some(vec![
                (format!("{input_lines} lines"), Word),
                ("   ".into(), Word),
                ("alt+enter".into(), SoftKey),
                (" newline".into(), Word),
            ])
        } else {
            None
        };
        if let Some(tab) = tab {
            neon::paint_tab(frame.buffer_mut(), input_area, &slab, &tab);
        }
    }

    // ── Active task bar ───────────────────────────────────────────────────
    if !model.active_tasks.is_empty() {
        let bar = render_active_tasks_line(&model.active_tasks, download_area.width);
        frame.render_widget(bar, download_area);
    }

    // ── Footer ────────────────────────────────────────────────────────────
    // Noodle's status bar: keys bright, words at a legible dim, segments set
    // apart by space instead of dots or pipes. The info block takes only its
    // real width, and hints that don't fit are dropped whole instead of
    // clipped mid-word.
    let theme = THEME.load();
    let dim = theme.chrome_dim();
    let cost_str = if model.session_cost > 0.0 {
        format!("${:.4} ", model.session_cost)
    } else {
        String::new()
    };
    let cache_rate = {
        let total_input = model.total_input_tokens
            + model.total_cache_read_tokens
            + model.total_cache_creation_tokens;
        if total_input > 0 && model.total_cache_read_tokens > 0 {
            let rate = (model.total_cache_read_tokens as f64 / total_input as f64 * 100.0) as u32;
            let ttl_hint = if model.total_cache_write_1h > 0 {
                "·1h"
            } else {
                ""
            };
            format!(" {}%↺{}", rate, ttl_hint)
        } else {
            String::new()
        }
    };
    let token_str = if model.total_input_tokens > 0 || model.total_output_tokens > 0 {
        format!(
            "{}\u{2191} {}\u{2193}{}  ",
            format_tokens(model.total_input_tokens),
            format_tokens(model.total_output_tokens),
            cache_rate,
        )
    } else {
        String::new()
    };
    let info_line = ratatui::text::Line::from(vec![
        Span::styled(&cost_str, Style::default().fg(theme.cost_color)),
        Span::styled(&token_str, Style::default().fg(dim)),
        {
            let turn_context = model.last_turn_context;
            let context_window = model.last_turn_context_window.max(1);
            if turn_context > 0 {
                let usage_ratio = (turn_context as f64 / context_window as f64).min(1.0);
                let bar_width: usize = 14;
                let filled = (usage_ratio * bar_width as f64).round() as usize;
                let empty = bar_width.saturating_sub(filled);
                let bar_color = context_bar_color(&theme, turn_context, context_window);
                let pct = (usage_ratio * 100.0) as u32;
                Span::styled(
                    format!(
                        "{}{} {}% ",
                        "\u{2593}".repeat(filled),
                        "\u{2591}".repeat(empty),
                        pct
                    ),
                    Style::default().fg(bar_color),
                )
            } else {
                Span::raw("")
            }
        },
        Span::styled("\u{03b8} ", Style::default().fg(dim)),
        Span::styled(
            model.runtime_thinking.clone(),
            Style::default().fg(theme.claude_text),
        ),
        Span::raw("   "),
        Span::styled(
            model.runtime_model.clone(),
            Style::default().fg(theme.header_fg),
        ),
        Span::styled(" ", Style::default()),
    ]);
    let info_width = (info_line.width() as u16).min(footer_area.width);
    let [keybinds_area, info_area] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(info_width)]).areas(footer_area);

    let key_style = Style::default().fg(theme.claude_text);
    let label_style = Style::default().fg(dim);
    let hints: [(&str, &str); 5] = [
        ("ctrl+c", "quit"),
        ("esc", "abort"),
        ("shift+\u{2191}\u{2193}", "scroll"),
        (
            "ctrl+o",
            if model.show_full_output {
                "full"
            } else {
                "compact"
            },
        ),
        ("enter", "send"),
    ];
    // One cell of air before the info block.
    let room = usize::from(keybinds_area.width).saturating_sub(1);
    let mut spans = vec![Span::raw(" ")];
    let mut used = 1;
    for (i, (key, label)) in hints.iter().enumerate() {
        let sep = if i > 0 { 3 } else { 0 };
        let seg = sep + display_width(key) + 1 + display_width(label);
        if used + seg > room {
            break;
        }
        if i > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled(format!("{key} "), key_style));
        spans.push(Span::styled(*label, label_style));
        used += seg;
    }
    frame.render_widget(
        Paragraph::new(ratatui::text::Line::from(spans)).style(Style::default().bg(theme.bg)),
        keybinds_area,
    );
    frame.render_widget(
        Paragraph::new(info_line)
            .alignment(Alignment::Right)
            .style(Style::default().bg(theme.bg)),
        info_area,
    );

    // ── Effects ───────────────────────────────────────────────────────────
    if let Some(ref mut fx) = boot_fx {
        let area = frame.area();
        fx.process(elapsed.into(), frame.buffer_mut(), area);
        if fx.done() {
            *boot_fx = None;
        }
    }
    if let Some(ref mut fx) = exit_fx {
        let area = frame.area();
        fx.process(elapsed.into(), frame.buffer_mut(), area);
    }

    // ── Toasts ────────────────────────────────────────────────────────────
    render_toasts_from_snap(frame, &model.toasts);

    // ── Modals (stack order: bottom → top; topmost paints last) ───────────
    // P7.8: driven by the `ModalStack` snapshot (`model.modal_order`) instead
    // of a hardcoded order, dispatching to the same render fns. For every
    // reachable state the stack order equals the old hardcoded order EXCEPT
    // the secret-prompt-over-modal case: SecretPrompt now paints LAST
    // (topmost) when it coexists with a modal (§5.5 z-order fix) — the one
    // deliberate divergence, blessed at Gate 2. No existing harness scenario
    // opens a modal mid-prompt, so snapshots stay byte-identical.
    for pane in &model.modal_order {
        match pane {
            super::focus::PaneId::Settings => {
                if let Some((ref state, ref snap)) = model.settings {
                    super::settings::render(frame, frame.area(), state, snap);
                }
            }
            super::focus::PaneId::Models => {
                if let Some(ref state) = model.models {
                    super::models::render(frame, frame.area(), state, &model.runtime_model);
                }
            }
            super::focus::PaneId::Plugins => {
                if let Some(ref state) = model.plugins {
                    super::plugins::render(frame, frame.area(), state);
                }
            }
            super::focus::PaneId::HelpFind => {
                if let Some(mut state) = model.help_find.clone() {
                    super::help_find::render(frame, frame.area(), &mut state);
                }
            }
            super::focus::PaneId::Effort => {
                if let Some(ref state) = model.effort {
                    super::effort::render(frame, frame.area(), state);
                }
            }
            super::focus::PaneId::SecretPrompt => {
                if let Some(ref prompt) = model.secret_prompt {
                    render_secret_prompt(frame, prompt);
                }
            }
            // PluginEditor is drawn as the Settings `edit_mode` overlay by
            // `settings::render` above (no standalone pass); Chat is the base
            // pane and is never on the stack.
            super::focus::PaneId::PluginEditor | super::focus::PaneId::Chat => {}
        }
    }
}

/// Number of terminal rows `text` occupies when word-wrapped at `width`
/// columns (ratatui `Wrap { trim: false }` semantics, approximated by
/// character count per `\n`-separated line). Empty lines still count as one.
fn wrapped_line_count(text: &str, width: u16) -> usize {
    let w = width.max(1) as usize;
    text.lines()
        .map(|l| l.chars().count().div_ceil(w).max(1))
        .sum::<usize>()
        .max(1)
}

/// Render the async prompt modal — dispatched on [`SecretPromptSnap::kind`].
///
/// * `Secret`  → the masked `password:` field (unchanged behaviour, now with
///   a word-wrapped multi-line body so a long sudo reason is not cut off).
/// * `Confirm` → a y/n dialog: the FULL body (e.g. the exact tool-id list from
///   `activate_tools`) is visible and unmasked; there is no input field.
///   `y` allows, `n`/Esc/Enter deny (fail-closed, see `route_secret_prompt`).
///
/// P7.8: extracted from the former inline block so it can be dispatched from
/// the stack-order modal loop in [`render_frame_into`].
fn render_secret_prompt(frame: &mut ratatui::Frame<'_>, prompt: &SecretPromptSnap) {
    use synaps_cli::tools::PromptKind;
    let area = frame.area();
    let is_confirm = prompt.kind == PromptKind::Confirm;
    // Cap to available width; prefer 30-62 (72 for confirm — tool ids are
    // long) but never overflow (#tui-safety fix 3).
    let width = area.width.min(if is_confirm { 72 } else { 62 });
    let inner_w = width.saturating_sub(2);
    let body_lines = wrapped_line_count(&prompt.prompt, inner_w);
    // body + blank + (field | nothing) + footer, plus 2 border rows. Floor
    // at the legacy fixed 7 so single-line secret prompts render exactly as
    // before (reference-binary differential); grow only for longer bodies.
    let content_rows = body_lines + 1 + if is_confirm { 0 } else { 1 } + 1;
    let height = (content_rows as u16 + 2).max(7).min(area.height).max(3);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let modal_area = ratatui::layout::Rect {
        x,
        y,
        width,
        height,
    };
    frame.render_widget(Clear, modal_area);
    let block = Block::default()
        .title(Span::styled(
            format!(" {} ", prompt.title),
            Style::default()
                .fg(THEME.load().warning_color)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(THEME.load().warning_color))
        .style(Style::default().bg(THEME.load().bg));
    let mut text: Vec<ratatui::text::Line<'_>> = prompt
        .prompt
        .lines()
        .map(|l| {
            ratatui::text::Line::from(Span::styled(
                l.to_string(),
                Style::default().fg(THEME.load().help_fg),
            ))
        })
        .collect();
    if text.is_empty() {
        text.push(ratatui::text::Line::from(""));
    }
    text.push(ratatui::text::Line::from(""));
    if is_confirm {
        text.push(ratatui::text::Line::from(Span::styled(
            "y allow · n/esc deny · server.auto_approve_confirms=true skips this",
            Style::default().fg(THEME.load().muted),
        )));
    } else {
        let masked = "\u{2022}".repeat(prompt.masked_buffer_chars);
        text.push(ratatui::text::Line::from(vec![
            Span::styled("password: ", Style::default().fg(THEME.load().muted)),
            Span::styled(masked, Style::default().fg(THEME.load().input_fg)),
        ]));
        text.push(ratatui::text::Line::from(Span::styled(
            "Enter submit · Esc cancel",
            Style::default().fg(THEME.load().muted),
        )));
    }
    frame.render_widget(
        Paragraph::new(text)
            .block(block)
            .alignment(Alignment::Left)
            .wrap(ratatui::widgets::Wrap { trim: false }),
        modal_area,
    );
}

/// Toast box dimensions, clamped so they ALWAYS fit a terminal of any size.
///
/// Returns `(width, height)`. The minimums (18 wide, 3 tall) are capped by the
/// available space so `clamp`'s `min` can never exceed its `max` — which used
/// to panic (`min > max`) when a tmux pane was resized smaller than the minimum.
/// On a cramped terminal the toast simply renders smaller instead of crashing.
fn toast_dims(content_width: u16, line_count: usize, area_w: u16, area_h: u16) -> (u16, u16) {
    let max_w = area_w.clamp(1, 64);
    let width = content_width
        .saturating_add(4)
        .clamp(18u16.min(max_w), max_w);
    let max_h = area_h.max(1);
    let height = (line_count as u16)
        .saturating_add(2)
        .clamp(3u16.min(max_h), max_h);
    (width, height)
}

/// Render toasts from a pre-cloned snapshot vec (used by `render_frame`).
fn render_toasts_from_snap(frame: &mut ratatui::Frame<'_>, toasts: &[super::toast::Toast]) {
    let area = frame.area();
    for toast in toasts {
        let lines = super::toast::toast_lines(toast);
        let content_width = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| display_width(span.content.as_ref()))
            .max()
            .unwrap_or(1) as u16;
        // Dimensions are clamped to always fit a terminal of ANY size — see
        // toast_dims (a tiny tmux resize used to panic here with min > max).
        let (width, height) = toast_dims(content_width, lines.len(), area.width, area.height);
        let rect = super::toast::toast_rect(area, width, height, toast.position);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            // P19.2: extension widgets may carry an accent resolved from
            // `ext.<id>.accent`; None => border_active, identical to before.
            .border_style(Style::default().fg(toast.accent.unwrap_or(THEME.load().border_active)))
            .style(Style::default().bg(THEME.load().bg));
        frame.render_widget(Clear, rect);
        let paragraph = if toast.has_rich_lines() {
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false })
                .alignment(ratatui::layout::Alignment::Center)
                .style(Style::default().bg(THEME.load().bg))
        } else {
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: true })
                .style(Style::default().fg(THEME.load().help_fg))
        };
        frame.render_widget(paragraph, rect);
    }
}

#[cfg(test)]
mod background_toggle_tests {
    use super::super::testing::TestHarness;
    use super::super::theme::{background_is_opaque, set_background_opaque};
    use ratatui::style::Color;
    use serial_test::serial;

    /// Background colour of every cell in one row.
    fn row_bgs(h: &mut TestHarness, y: u16) -> Vec<Option<Color>> {
        let buf = h.render();
        let area = *buf.area();
        (area.x..area.x + area.width)
            .map(|x| buf[(x, y)].style().bg)
            .collect()
    }

    /// Regression (S278): `invisible` must change ONLY the conversation
    /// canvas. An earlier fix routed header/input/footer through the toggle
    /// too, which made the whole frame lose its chrome. 0.7.0 painted all
    /// chrome unconditionally and used `Clear` for the message area — that
    /// is exactly what `invisible` must still look like.
    #[test]
    #[serial]
    fn invisible_leaves_header_and_footer_chrome_identical_to_opaque() {
        let prior = background_is_opaque();
        let mut h = TestHarness::boot();
        let bottom = 23; // 80x24 default geometry

        set_background_opaque(true);
        let header_opaque = row_bgs(&mut h, 0);
        let footer_opaque = row_bgs(&mut h, bottom);

        set_background_opaque(false);
        let header_invisible = row_bgs(&mut h, 0);
        let footer_invisible = row_bgs(&mut h, bottom);

        assert_eq!(
            header_opaque, header_invisible,
            "header chrome must be identical in both modes — invisible owns \
             only the conversation canvas"
        );
        assert_eq!(
            footer_opaque, footer_invisible,
            "footer/input chrome must be identical in both modes"
        );

        set_background_opaque(prior);
    }

    /// The other half of the contract: the canvas itself really does stop
    /// painting, otherwise the toggle is a no-op (the original bug).
    #[test]
    #[serial]
    fn invisible_stops_painting_the_conversation_canvas() {
        let prior = background_is_opaque();
        let mut h = TestHarness::boot();

        set_background_opaque(true);
        let opaque: Vec<Vec<Option<Color>>> = (0..24).map(|y| row_bgs(&mut h, y)).collect();

        set_background_opaque(false);
        let invisible: Vec<Vec<Option<Color>>> = (0..24).map(|y| row_bgs(&mut h, y)).collect();

        let changed: Vec<usize> = (0..24).filter(|&y| opaque[y] != invisible[y]).collect();

        assert!(
            !changed.is_empty(),
            "toggling to invisible changed nothing — the canvas is still painted"
        );
        assert!(
            !changed.contains(&0),
            "row 0 (header) must not change between modes, but did"
        );

        set_background_opaque(prior);
    }
}

#[cfg(test)]
mod neon_prompt_tests {
    //! The input is the neon slab (neon_prompt.rs): half-block shape, no
    //! box-drawing border, text in a fixed column with a hanging indent.
    use super::super::app::SPINNER_FRAMES;
    use super::super::testing::TestHarness;
    use super::super::theme::{background_is_opaque, set_background_opaque, THEME};
    use ratatui::buffer::Buffer;
    use serial_test::serial;

    const W: u16 = 80;
    const H: u16 = 16;

    fn sym(buf: &Buffer, x: u16, y: u16) -> &str {
        buf[(x, y)].symbol()
    }

    fn row(buf: &Buffer, y: u16) -> String {
        (0..buf.area().width).map(|x| sym(buf, x, y)).collect()
    }

    /// (top rim, bottom rim) rows of the slab: the rows whose column 1 holds
    /// the top-left / bottom-left quadrant.
    fn rims(buf: &Buffer) -> (u16, u16) {
        let h = buf.area().height;
        let bottom = (0..h)
            .rev()
            .find(|&y| sym(buf, 1, y) == "\u{259D}")
            .expect("bottom rim ▝");
        let top = (0..bottom)
            .rev()
            .find(|&y| sym(buf, 1, y) == "\u{2597}")
            .expect("top rim ▗");
        (top, bottom)
    }

    #[test]
    fn prompt_is_a_half_block_slab_without_box_drawing() {
        let mut h = TestHarness::boot_with_size(W, H);
        h.type_str("hello");
        let buf = h.render().clone();
        let (top, bottom) = rims(&buf);
        assert_eq!(bottom, top + 2, "one text row between the rims");
        let text = top + 1;
        for (x, y, want) in [
            (1, top, "\u{2597}"),
            (W - 2, top, "\u{2596}"),
            (1, text, "\u{2590}"),
            (W - 2, text, "\u{258C}"),
            (1, bottom, "\u{259D}"),
            (W - 2, bottom, "\u{2598}"),
            (3, text, "\u{276f}"),
            (5, text, "h"),
        ] {
            assert_eq!(sym(&buf, x, y), want, "({x},{y})");
        }
        for y in top..=bottom {
            for x in 0..W {
                let c = sym(&buf, x, y).chars().next().unwrap_or(' ');
                assert!(
                    !('\u{2500}'..='\u{257F}').contains(&c),
                    "box-drawing {c:?} at ({x},{y})"
                );
            }
        }
    }

    #[test]
    fn wrapped_input_keeps_a_hanging_indent() {
        let mut h = TestHarness::boot_with_size(W, H);
        // 80 cols → text column 5..=76 (72 cells) on every row.
        h.type_str(&"a".repeat(72));
        h.type_str("bcd");
        let buf = h.render().clone();
        let (top, bottom) = rims(&buf);
        assert_eq!(bottom, top + 3, "two text rows");
        assert_eq!(
            sym(&buf, 76, top + 1),
            "a",
            "first row fills the text column"
        );
        assert_eq!(sym(&buf, 77, top + 1), " ", "right padding stays clear");
        assert_eq!(
            sym(&buf, 3, top + 2),
            " ",
            "no prompt glyph on continuation rows"
        );
        assert_eq!(
            sym(&buf, 5, top + 2),
            "b",
            "continuation starts in the text column"
        );
    }

    #[test]
    fn streaming_swaps_the_prompt_for_a_spinner_and_hangs_a_status_tab() {
        let mut h = TestHarness::boot_with_size(W, H);
        h.set_streaming(true);
        let buf = h.render().clone();
        let (top, bottom) = rims(&buf);
        assert!(
            SPINNER_FRAMES.contains(&sym(&buf, 3, top + 1)),
            "spinner in the prompt column, got {:?}",
            sym(&buf, 3, top + 1)
        );
        let rim = row(&buf, bottom);
        assert!(
            rim.contains("working") && rim.contains("esc abort"),
            "tab: {rim:?}"
        );
        assert!(
            row(&buf, top + 1).contains("steer or queue"),
            "streaming placeholder"
        );
    }

    #[test]
    fn empty_prompt_shows_the_placeholder() {
        let mut h = TestHarness::boot_with_size(W, H);
        let buf = h.render().clone();
        let (top, _) = rims(&buf);
        assert!(row(&buf, top + 1).contains("Ask anything"));
    }

    #[test]
    fn ghost_completion_and_multiline_hang_their_hint_tabs() {
        let mut h = TestHarness::boot_with_size(W, H);
        h.type_str("/them");
        let buf = h.render().clone();
        let (top, bottom) = rims(&buf);
        assert!(
            row(&buf, top + 1).contains("/theme"),
            "ghost completes inline"
        );
        assert!(row(&buf, bottom).contains("tab complete"));

        let mut h = TestHarness::boot_with_size(W, H);
        h.paste("one\ntwo\nthree");
        let buf = h.render().clone();
        let (_, bottom) = rims(&buf);
        assert!(
            row(&buf, bottom).contains("3 lines"),
            "{:?}",
            row(&buf, bottom)
        );
    }

    #[test]
    fn several_matches_hang_a_search_tab() {
        let mut h = TestHarness::boot_with_size(W, H);
        h.type_str("/s"); // several commands start with s
        let buf = h.render().clone();
        let (top, bottom) = rims(&buf);
        let rim = row(&buf, bottom);
        assert!(
            rim.contains(" matches") && rim.contains("tab search"),
            "{rim:?}"
        );
        assert!(
            !row(&buf, top + 1).contains("matches"),
            "not inline any more"
        );
    }

    #[test]
    fn cursor_is_a_lit_block_after_the_text() {
        let mut h = TestHarness::boot_with_size(W, H);
        h.type_str("hi");
        let buf = h.render().clone();
        let (top, _) = rims(&buf);
        let (cursor, body) = (&buf[(7, top + 1)], &buf[(20, top + 1)]);
        assert_ne!(cursor.style().bg, body.style().bg, "cursor cell is lit");
    }

    /// No motion while idle: the prompt asks for frames only while a
    /// keystroke's trail fades, or while a turn streams.
    #[test]
    fn prompt_is_still_when_idle() {
        let mut h = TestHarness::boot_with_size(W, H);
        assert!(!h.prompt_animating(), "still at boot");
        h.type_str("x");
        assert!(h.prompt_animating(), "a keystroke lights the trail");
        h.advance_clock_ms(1_000);
        assert!(!h.prompt_animating(), "still again once it fades");
        h.set_streaming(true);
        assert!(h.prompt_animating(), "animates while streaming");
    }

    /// The streaming glow is off unless the setting turns it on.
    #[test]
    #[serial]
    fn streaming_glow_follows_the_setting() {
        use super::super::neon_prompt::{set_streaming_glow, streaming_glow_enabled};
        let prior = streaming_glow_enabled();
        let mut h = TestHarness::boot_with_size(W, H);
        h.set_streaming(true);
        h.render(); // the stream clock starts on the first streaming frame
        h.advance_clock_ms(950); // half a sweep: its centre is mid-slab
        let body_varies = |h: &mut TestHarness| {
            let buf = h.render().clone();
            let (top, _) = rims(&buf);
            // The top rim's body half, across the slab (no cursor there).
            let fills: Vec<_> = (3..W - 3).map(|x| buf[(x, top)].style().fg).collect();
            fills.iter().any(|f| *f != fills[0])
        };
        set_streaming_glow(false);
        assert!(!body_varies(&mut h), "no sweep with the glow off");
        set_streaming_glow(true);
        assert!(
            body_varies(&mut h),
            "a sweep across the slab with the glow on"
        );
        set_streaming_glow(prior);
    }

    #[test]
    #[serial]
    fn backdrop_is_full_width_chrome_like_the_footer() {
        let prior = background_is_opaque();
        let mut h = TestHarness::boot_with_size(W, H);
        for opaque in [true, false] {
            set_background_opaque(opaque);
            let chrome = Some(THEME.load().bg);
            let buf = h.render().clone();
            let (top, bottom) = rims(&buf);
            assert_eq!(buf[(0, H - 1)].style().bg, chrome, "footer row is chrome");
            for y in top..=bottom {
                for x in [0, W - 1] {
                    assert_eq!(buf[(x, y)].style().bg, chrome, "({x},{y}) opaque={opaque}");
                }
            }
        }
        set_background_opaque(prior);
    }
}

#[cfg(test)]
mod footer_tests {
    use super::super::testing::TestHarness;
    use super::super::theme::THEME;

    const HINTS: &[&str] = &[
        "ctrl+c quit",
        "esc abort",
        "shift+\u{2191}\u{2193} scroll",
        "ctrl+o compact",
        "enter send",
    ];

    fn footer(h: &mut TestHarness, w: u16) -> String {
        let buf = h.render();
        (0..w).map(|x| buf[(x, 23)].symbol()).collect()
    }

    /// Hints are dropped whole when narrow — never clipped mid-word (the
    /// old footer showed "ctrl+c qui" in the README GIF and nothing at all at
    /// 80 columns).
    #[test]
    fn hints_fit_whole_or_not_at_all() {
        for w in [40u16, 60, 80, 100, 120, 200] {
            let mut h = TestHarness::boot_with_size(w, 24);
            let row = footer(&mut h, w);
            let shown = HINTS.iter().filter(|hint| row.contains(*hint)).count();
            for hint in HINTS {
                let key = hint.split(' ').next().unwrap();
                if row.contains(&format!("{key} ")) {
                    assert!(row.contains(hint), "w={w}: clipped {hint:?} in {row:?}");
                }
            }
            assert!(
                !row.contains('\u{00b7}') && !row.contains('\u{2502}'),
                "no dots or pipes: {row:?}"
            );
            if w >= 80 {
                assert!(shown >= 2, "w={w}: hints visible at common widths: {row:?}");
            }
            if w >= 120 {
                assert_eq!(shown, HINTS.len(), "w={w}: all hints fit: {row:?}");
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn keys_are_bright_and_words_legible() {
        let mut h = TestHarness::boot_with_size(120, 24);
        let theme = THEME.load();
        let buf = h.render();
        let row: String = (0..120).map(|x| buf[(x, 23)].symbol()).collect();
        let key_x = row.find("ctrl+c").expect("hint shown") as u16;
        let word_x = row.find("quit").expect("hint shown") as u16;
        assert_eq!(buf[(key_x, 23)].style().fg, Some(theme.claude_text));
        assert_eq!(buf[(word_x, 23)].style().fg, Some(theme.chrome_dim()));
    }
}

#[cfg(test)]
mod context_bar_color_tests {
    use super::super::theme::Theme;
    use super::context_bar_color;

    #[test]
    fn follows_the_usage_thresholds() {
        let t = Theme::default();
        assert_eq!(context_bar_color(&t, 0, 0), t.border_active, "no turn yet");
        assert_eq!(context_bar_color(&t, 49, 100), t.border_active);
        assert_eq!(context_bar_color(&t, 50, 100), t.status_streaming);
        assert_eq!(context_bar_color(&t, 74, 100), t.status_streaming);
        assert_eq!(context_bar_color(&t, 75, 100), t.error_color);
        assert_eq!(context_bar_color(&t, 500, 100), t.error_color, "clamped");
    }
}
