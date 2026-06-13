use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap},
    Terminal,
};
use std::io;
use tachyonfx::{fx, Effect, Interpolation, Shader};

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
                THEME.load().muted
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
mod sidecar_pill_tests {
    use super::*;

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

use super::app::{App, SPINNER_FRAMES};
use super::markdown::format_tokens;
use super::theme::THEME;

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
/// MCP tools like "ext__byteray__read_pseudocode" become ("⚡", "read_pseudocode", Some("byteray"))
pub(crate) fn format_tool_name(tool_name: &str) -> (&'static str, String, Option<String>) {
    if tool_name.starts_with("ext__") {
        let parts: Vec<&str> = tool_name.splitn(3, "__").collect();
        let server = parts.get(1).unwrap_or(&"mcp").to_string();
        let tool = parts.get(2).unwrap_or(&tool_name).to_string();
        ("\u{00bb}", tool, Some(server)) // »
    } else {
        let icon = match tool_name {
            "bash" => "$",
            "read" => ">",
            "write" => "<",
            "edit" => "~",
            "grep" => "/",
            "find" => "?",
            "ls" => "=",
            "subagent" => "*",
            _ => "-",
        };
        (icon, tool_name.to_string(), None)
    }
}

pub(crate) fn boot_effect() -> Effect {
    use tachyonfx::fx::Direction as FxDir;
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
    use tachyonfx::fx::Direction as FxDir;
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

/// Build a [`RenderModel`] snapshot from `app` and associated services.
///
/// This is the **main-side extraction step**.  It reads `App` + computes
/// every derived value the renderer needs, applies the five App mutations
/// that `draw()` used to do, and returns a fully-owned snapshot.
///
/// Returns `None` when the frame should be skipped (gamba casino owns the
/// terminal, same semantics as the old early-return in `draw()`).
pub(crate) fn build_render_model(
    app: &mut App,
    runtime: &synaps_cli::Runtime,
    registry: &std::sync::Arc<synaps_cli::skills::registry::CommandRegistry>,
    secret_prompts: &synaps_cli::tools::SecretPromptQueue,
    term_size: ratatui::layout::Size,
) -> Option<std::sync::Arc<RenderModel>> {
    // ── 1. gamba gate ─────────────────────────────────────────────────────────
    if app.gamba_child.is_some() {
        return None;
    }

    // ── 2. Layout math (mirrors draw.rs pre-closure block) ────────────────────
    let has_subagents = !app.subagents.is_empty();
    let subagent_height: u16 = if has_subagents {
        (app.subagents.len() as u16 + 2).min(8)
    } else {
        0
    };
    let input_inner_width = term_size.width.saturating_sub(2);
    let (input_lines, _, _) = app.input_wrap_info(input_inner_width);
    let max_input_lines: u16 = 10;
    let input_height = input_lines.min(max_input_lines) + 2;
    let download_height: u16 = if !app.active_tasks.is_empty() { 1 } else { 0 };
    let protected_bottom_rows = subagent_height
        .saturating_add(download_height)
        .saturating_add(input_height)
        .saturating_add(1); // footer

    // Replicate the outer layout to find msg_area deterministically.
    // header(1) + messages(min 1) + subagents + download + input + footer(1).
    let msg_area_height = term_size
        .height
        .saturating_sub(1) // header
        .saturating_sub(subagent_height)
        .saturating_sub(download_height)
        .saturating_sub(input_height)
        .saturating_sub(1) // footer
        .max(1);
    let msg_area = ratatui::layout::Rect {
        x: 0,
        y: 1,
        width: term_size.width,
        height: msg_area_height,
    };
    let content_height = msg_area.height.saturating_sub(2) as usize;
    let content_width = msg_area.width.saturating_sub(2) as usize;

    // ── 3. Line-cache maintenance ──────────────────────────────────────────────
    let needs_rebuild = app
        .line_cache
        .as_ref()
        .map_or(true, |(w, _)| *w != content_width);
    if needs_rebuild {
        let lines = app.render_lines(content_width);
        app.line_cache = Some((content_width, lines));
    }
    // SAFETY: the branch above guarantees line_cache is Some; the fallback
    // rebuild handles any hypothetical race where it was taken between here
    // and there (not possible today, but avoids a panic on the main task).
    if app.line_cache.is_none() {
        let lines = app.render_lines(content_width);
        app.line_cache = Some((content_width, lines));
    }
    let all_lines_vec: &[ratatui::text::Line<'static>] =
        app.line_cache.as_ref().map_or(&[], |(_, v)| v.as_slice());
    let total = all_lines_vec.len();
    // Wrap in Arc for zero-copy clone into the model.
    let lines: std::sync::Arc<[ratatui::text::Line<'static>]> = all_lines_vec.to_vec().into();

    // ── 4. Scroll bookkeeping (App mutations lifted from draw()) ──────────────
    if app.scroll_pinned {
        app.scroll_back = 0;
    } else {
        let prev = app.last_line_count;
        if total > prev && prev > 0 {
            let growth = (total - prev) as u16;
            app.scroll_back = app.scroll_back.saturating_add(growth);
        }
        let max_back = total.saturating_sub(content_height).min(u16::MAX as usize) as u16;
        if app.scroll_back > max_back {
            app.scroll_back = max_back;
        }
    }
    app.last_line_count = total;
    let scroll_back = app.scroll_back;

    // ── 5. Visible range + msg_area_rect write-back ───────────────────────────
    let end = total.saturating_sub(scroll_back as usize);
    let start = end.saturating_sub(content_height);
    // Inner rect: TOP+BOTTOM borders (-2 height, +1 y), horizontal padding
    // (-2 width, +1 x).  Matches `msg_block.inner(msg_area)` exactly.
    let msg_inner = ratatui::layout::Rect {
        x: msg_area.x + 1,
        y: msg_area.y + 1,
        width: msg_area.width.saturating_sub(2),
        height: msg_area.height.saturating_sub(2),
    };
    app.msg_area_rect = Some(msg_inner);
    app.visible_line_range = Some((start, end));

    // ── 6. Selection range ────────────────────────────────────────────────────
    let selection = app.selection_range();

    // ── 7. Subagent snapshots ─────────────────────────────────────────────────
    let subagents: Vec<SubagentSnap> = app
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
        if app.sidecars.is_empty() {
            Vec::new()
        } else {
            let claims = registry.lifecycle_claims();
            let inputs: Vec<(String, Option<String>)> = app
                .sidecars
                .iter()
                .map(|(pid, st)| (pid.clone(), st.display_name.clone()))
                .collect();
            let order = order_sidecar_pills(&inputs, &claims);
            order
                .into_iter()
                .filter_map(|pid| {
                    let st = app.sidecars.get(&pid)?;
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
        if app.input.starts_with('/') && app.input.len() > 1 && !app.input[1..].contains(' ') {
            let partial = &app.input[1..];
            let commands = super::commands::all_commands_with_skills(registry);
            let prefix_matches: Vec<&String> =
                commands.iter().filter(|c| c.starts_with(partial)).collect();
            if prefix_matches.len() == 1 {
                let cmd = prefix_matches[0];
                if cmd.as_str() != partial {
                    let ghost_text = if cmd.starts_with(partial) {
                        cmd[partial.len()..].to_string()
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
                    match_badge: Some(format!("  {} matches · Tab search", prefix_matches.len())),
                })
            } else {
                None
            }
        } else {
            None
        }
    };

    // ── 10. Toasts ────────────────────────────────────────────────────────────
    let toasts: Vec<super::toast::Toast> = app.toasts.visible().cloned().collect();

    // ── 11. Modals ────────────────────────────────────────────────────────────
    let settings = app.settings.clone().map(|s| {
        let snap = super::settings::RuntimeSnapshot::from_runtime_with_health(
            runtime,
            registry,
            app.model_health.clone(),
        );
        (s, snap)
    });
    let plugins = app.plugins.clone();
    let models = app.models.clone();
    // ── help_find visible_height write-back ───────────────────────────────────
    // `help_find::render` computes visible_height from the terminal geometry
    // and calls `set_visible_height` on its &mut state.  Because the render
    // runs on a separate thread it was previously mutating a throwaway clone,
    // so the modal's scroll window was wrong on first open at a non-default size.
    // Fix: mirror the geometry computation here on the main side and call
    // `set_visible_height` on the authoritative App state before snapshotting.
    if let Some(ref mut hf) = app.help_find {
        let area_w = term_size.width;
        let area_h = term_size.height;
        let _modal_w = ((area_w as u32 * 8 / 10) as u16).max(50).min(area_w);
        let modal_h = ((area_h as u32 * 8 / 10) as u16).max(14).min(area_h);
        // block.inner subtracts 1px border on each side → -2 height
        // padded_rect(inner, 2, 1) subtracts 1px vertical pad each side → -2 height
        let inner_h = modal_h.saturating_sub(2).saturating_sub(2);
        // Layout [Length(2), Min(1), Length(1)]: chunk[1] = inner_h - 3
        let visible_h = inner_h.saturating_sub(3) as usize;
        hf.set_visible_height(visible_h.max(1));
    }
    let help_find = app.help_find.clone();

    // ── 12. Secret prompt ─────────────────────────────────────────────────────
    let secret_prompt = secret_prompts.active().map(|p| SecretPromptSnap {
        title: p.title.clone(),
        prompt: p.prompt.clone(),
        masked_buffer_chars: p.buffer.chars().count(),
    });

    // ── 13. Runtime strings ───────────────────────────────────────────────────
    let runtime_model = runtime.model().to_string();
    let runtime_thinking = runtime.thinking_level().to_string();

    // ── 14. Assemble ──────────────────────────────────────────────────────────
    Some(std::sync::Arc::new(RenderModel {
        status_text: app.status_text.clone(),
        streaming: app.streaming,
        spinner_frame: app.spinner_frame,
        sidecar_pills,
        runtime_model,
        runtime_thinking,
        lines,
        lines_width: content_width,
        scroll_back,
        visible_range: (start, end),
        selection,
        messages_empty: app.messages.is_empty(),
        logo_build_t: app.logo_build_t,
        logo_dismiss_t: app.logo_dismiss_t,
        subagents,
        active_tasks: std::sync::Arc::clone(&app.active_tasks),
        input: app.input.clone(),
        cursor_pos: app.cursor_pos,
        ghost_hint,
        show_full_output: app.show_full_output,
        session_cost: app.session_cost,
        total_input_tokens: app.total_input_tokens,
        total_output_tokens: app.total_output_tokens,
        total_cache_read_tokens: app.total_cache_read_tokens,
        total_cache_creation_tokens: app.total_cache_creation_tokens,
        total_cache_write_1h: app.total_cache_write_1h,
        last_turn_context: app.last_turn_context,
        last_turn_context_window: app.last_turn_context_window,
        toasts,
        settings,
        plugins,
        models,
        help_find,
        secret_prompt,
        protected_bottom_rows,
    }))
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

    super::viewport::scrub_crossterm_terminal_edges(
        terminal,
        model.protected_bottom_rows,
        Style::default().bg(THEME.load().bg),
    )?;

    terminal.draw(|frame| {
        // ── Layout ────────────────────────────────────────────────────────────
        let has_subagents = !model.subagents.is_empty();
        let subagent_height: u16 = if has_subagents {
            (model.subagents.len() as u16 + 2).min(8)
        } else {
            0
        };
        let input_inner_width = frame.area().width.saturating_sub(2);
        let max_input_lines: u16 = 10;

        // Recompute input_lines for layout using the snapshot input + cursor_pos.
        let (input_lines, _, _) = {
            use unicode_width::UnicodeWidthChar;
            let w = input_inner_width.max(1) as usize;
            let prefix_width: usize = 2;
            let mut total_lines: u16 = 1;
            let mut col: usize = prefix_width;
            for ch in model.input.chars() {
                if ch == '\n' {
                    total_lines += 1;
                    col = prefix_width;
                    continue;
                }
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                if col + cw > w {
                    total_lines += 1;
                    col = 0;
                }
                col += cw;
            }
            // cursor_row / cursor_col not needed for layout — use dummy values.
            (total_lines, 0u16, 0u16)
        };
        let input_height = input_lines.min(max_input_lines) + 2;
        let download_height: u16 = if !model.active_tasks.is_empty() { 1 } else { 0 };

        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(subagent_height),
                Constraint::Length(download_height),
                Constraint::Length(input_height),
                Constraint::Length(1),
            ])
            .split(frame.area());

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
            concat!("v", env!("CARGO_PKG_VERSION"), " "),
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
                    }
                    super::sidecar::SidecarUiStatus::Error(_) => Color::Red,
                };
                spans.push(Span::styled(
                    text,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ));
            }
            let used: usize = spans.iter().map(|s| s.content.len()).sum();
            let total_w = outer[0].width as usize;
            if total_w > used + version_span.content.len() {
                let pad = total_w - used - version_span.content.len();
                spans.push(Span::raw(" ".repeat(pad)));
            }
            spans.push(version_span);
            spans
        }))
        .style(Style::default().bg(THEME.load().bg));
        frame.render_widget(header, outer[0]);

        // ── Messages ──────────────────────────────────────────────────────────
        let msg_area = outer[1];
        let (start, end) = model.visible_range;
        let visible: Vec<ratatui::text::Line> = model.lines[start..end].to_vec();
        let visible_is_empty = visible.is_empty();

        let msg_block = Block::default()
            .borders(Borders::TOP | Borders::BOTTOM)
            .border_type(BorderType::Plain)
            .border_style(Style::default().fg(THEME.load().border))
            .padding(Padding::horizontal(1));
        let msg_inner = msg_block.inner(msg_area);
        let messages_widget = Paragraph::new(visible).block(msg_block.clone());
        frame.render_widget(Clear, msg_area);
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
            use unicode_width::UnicodeWidthStr;
            let art_display_widths: Vec<usize> = ascii_art
                .iter()
                .map(|l| UnicodeWidthStr::width(*l))
                .collect();
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
                    let art_x =
                        msg_area.x + (avail_w as u16).saturating_sub(max_art_width as u16) / 2;
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
                                                build_chars
                                                    [(lp * build_chars.len() as f64) as usize],
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
                            frame.render_widget(
                                Paragraph::new(Span::styled(sub_text, sub_style)),
                                area,
                            );
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
            frame.render_widget(Paragraph::new(agent_lines).block(agent_block), outer[2]);
        }

        // ── Input ─────────────────────────────────────────────────────────────
        let input_border_color = if model.streaming {
            THEME.load().border
        } else {
            THEME.load().border_active
        };
        let input_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(input_border_color))
            .style(Style::default().bg(THEME.load().bg));
        let w = input_inner_width.max(1) as usize;
        let prefix_width: usize = 2;
        let prompt_style = Style::default().fg(THEME.load().prompt_fg);
        let input_style = Style::default().fg(THEME.load().input_fg);
        let input_lines_vec: Vec<ratatui::text::Line> = {
            use unicode_width::UnicodeWidthChar;
            let mut rows: Vec<Vec<Span>> = Vec::new();
            let mut current_row: Vec<Span> = vec![Span::styled("\u{276f} ", prompt_style)];
            let mut col: usize = prefix_width;
            for ch in model.input.chars() {
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
                let mut s = String::new();
                s.push(ch);
                current_row.push(Span::styled(s, input_style));
                col += cw;
            }
            rows.push(current_row);

            // Apply ghost hint from model
            if let Some(ref hint) = model.ghost_hint {
                let ghost_style = Style::default()
                    .fg(THEME.load().border)
                    .add_modifier(Modifier::DIM);
                if let Some(last_row) = rows.last_mut() {
                    if let Some(ref badge) = hint.match_badge {
                        last_row.push(Span::styled(badge.clone(), ghost_style));
                    } else if !hint.ghost_text.is_empty() {
                        last_row.push(Span::styled(hint.ghost_text.clone(), ghost_style));
                    }
                }
            }
            rows.into_iter().map(ratatui::text::Line::from).collect()
        };

        // Cursor position for scroll offset
        let (_, cursor_row, cursor_col) = {
            use unicode_width::UnicodeWidthChar;
            let w2 = input_inner_width.max(1) as usize;
            let mut total_rows: u16 = 1;
            let mut cur_row: u16 = 0;
            let mut cur_col: u16 = 0;
            let mut col: usize = prefix_width;
            for (i, ch) in model.input.chars().enumerate() {
                if i == model.cursor_pos {
                    cur_row = total_rows - 1;
                    cur_col = col as u16;
                }
                if ch == '\n' {
                    total_rows += 1;
                    col = prefix_width;
                    continue;
                }
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                if col + cw > w2 {
                    total_rows += 1;
                    col = 0;
                }
                col += cw;
            }
            if model.cursor_pos >= model.input.chars().count() {
                cur_row = total_rows - 1;
                cur_col = col as u16;
            }
            (total_rows, cur_row, cur_col)
        };

        let visible_lines = max_input_lines;
        let input_scroll: u16 = if cursor_row >= visible_lines {
            cursor_row - visible_lines + 1
        } else {
            0
        };
        let input_widget = Paragraph::new(input_lines_vec)
            .scroll((input_scroll, 0))
            .block(input_block);
        frame.render_widget(input_widget, outer[4]);

        // Software cursor
        let cursor_x = outer[4].x + 1 + cursor_col;
        let cursor_y = outer[4].y + 1 + cursor_row - input_scroll;
        if cursor_x < outer[4].x.saturating_add(outer[4].width)
            && cursor_y < outer[4].y.saturating_add(outer[4].height)
        {
            if let Some(cell) = frame.buffer_mut().cell_mut((cursor_x, cursor_y)) {
                let symbol = cell.symbol().to_string();
                let cursor_symbol = if symbol.trim().is_empty() {
                    " "
                } else {
                    symbol.as_str()
                };
                cell.set_symbol(cursor_symbol)
                    .set_fg(THEME.load().bg)
                    .set_bg(THEME.load().input_fg);
            }
        }

        // ── Active task bar ───────────────────────────────────────────────────
        if !model.active_tasks.is_empty() {
            let bar = render_active_tasks_line(&model.active_tasks, outer[3].width);
            frame.render_widget(bar, outer[3]);
        }

        // ── Footer ────────────────────────────────────────────────────────────
        let footer_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(model.runtime_model.len() as u16 + 75),
            ])
            .split(outer[5]);

        let key_style = Style::default().fg(THEME.load().muted);
        let label_style = Style::default().fg(THEME.load().help_fg);
        let dot_style = Style::default().fg(THEME.load().help_fg);
        let keybinds = Paragraph::new(ratatui::text::Line::from(vec![
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
                if model.show_full_output {
                    "full"
                } else {
                    "compact"
                },
                label_style,
            ),
            Span::styled(" \u{00b7} ", dot_style),
            Span::styled("enter ", key_style),
            Span::styled("send", label_style),
        ]))
        .style(Style::default().bg(THEME.load().bg));
        frame.render_widget(keybinds, footer_chunks[0]);

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
                let rate =
                    (model.total_cache_read_tokens as f64 / total_input as f64 * 100.0) as u32;
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
        let info = Paragraph::new(ratatui::text::Line::from(vec![
            Span::styled(&cost_str, Style::default().fg(THEME.load().cost_color)),
            Span::styled(&token_str, Style::default().fg(THEME.load().muted)),
            {
                let turn_context = model.last_turn_context;
                let context_window = model.last_turn_context_window.max(1);
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
            Span::styled("\u{03b8}:", Style::default().fg(THEME.load().muted)),
            Span::styled(
                model.runtime_thinking.clone(),
                Style::default().fg(THEME.load().help_fg),
            ),
            Span::styled(" \u{2502} ", Style::default().fg(THEME.load().border)),
            Span::styled(
                model.runtime_model.clone(),
                Style::default().fg(THEME.load().header_fg),
            ),
            Span::styled(" ", Style::default()),
        ]))
        .alignment(Alignment::Right)
        .style(Style::default().bg(THEME.load().bg));
        frame.render_widget(info, footer_chunks[1]);

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

        // ── Secret prompt modal ───────────────────────────────────────────────
        if let Some(ref prompt) = model.secret_prompt {
            let area = frame.area();
            let width = area.width.min(62).max(30);
            let height = 7u16;
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
            let masked = "\u{2022}".repeat(prompt.masked_buffer_chars);
            let text = vec![
                ratatui::text::Line::from(Span::styled(
                    prompt.prompt.clone(),
                    Style::default().fg(THEME.load().help_fg),
                )),
                ratatui::text::Line::from(""),
                ratatui::text::Line::from(vec![
                    Span::styled("password: ", Style::default().fg(THEME.load().muted)),
                    Span::styled(masked, Style::default().fg(THEME.load().input_fg)),
                ]),
                ratatui::text::Line::from(Span::styled(
                    "Enter submit · Esc cancel",
                    Style::default().fg(THEME.load().muted),
                )),
            ];
            frame.render_widget(
                Paragraph::new(text).block(block).alignment(Alignment::Left),
                modal_area,
            );
        }

        // ── Toasts ────────────────────────────────────────────────────────────
        render_toasts_from_snap(frame, &model.toasts);

        // ── Modals ────────────────────────────────────────────────────────────
        if let Some((ref state, ref snap)) = model.settings {
            super::settings::render(frame, frame.area(), state, snap);
        }
        if let Some(ref state) = model.models {
            super::models::render(frame, frame.area(), state, &model.runtime_model);
        }
        if let Some(ref state) = model.plugins {
            super::plugins::render(frame, frame.area(), state);
        }
        if let Some(mut state) = model.help_find.clone() {
            super::help_find::render(frame, frame.area(), &mut state);
        }
    })?;
    Ok(())
}

/// Render toasts from a pre-cloned snapshot vec (used by `render_frame`).
fn render_toasts_from_snap(frame: &mut ratatui::Frame<'_>, toasts: &[super::toast::Toast]) {
    let area = frame.area();
    for toast in toasts {
        let lines = super::toast::toast_lines(toast);
        let content_width = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| unicode_width::UnicodeWidthStr::width(span.content.as_ref()))
            .max()
            .unwrap_or(1) as u16;
        let width = content_width
            .saturating_add(4)
            .clamp(18, area.width.min(64));
        let height = (lines.len() as u16)
            .saturating_add(2)
            .clamp(3, area.height.max(1));
        let rect = super::toast::toast_rect(area, width, height, toast.position);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(THEME.load().border_active))
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
