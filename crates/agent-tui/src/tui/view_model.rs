//! T199.2 — the narrow render-input seam between `App` and the render builder.
//!
//! [`super::draw::build_render_model`] used to take `&mut App`, which made
//! every `App` field look like render input. This module draws the boundary:
//!
//! - [`ViewInputs`] is the **complete** set of `App` state the render builder
//!   is allowed to read. If a field is not here, the renderer cannot see it.
//!   Loop bookkeeping, channel endpoints, and accounting state stay on `App`
//!   (see the T199.1 spec, `docs/plans/2026-07-09-t199-viewmodel-boundary.md`).
//! - [`RenderPatch`] is the **complete** set of `App` mutations the builder
//!   may request. The builder itself is non-mutating; the caller applies the
//!   patch to authoritative `App` state after the snapshot is taken. This
//!   replaces the old in-builder `help_find` visible-height write-back
//!   (spec §4, resolution A).
//!
//! P7 note: the modal `Option<State>` fields cross this seam read-only. Their
//! ownership migrates under P7's `ModalStack`; once the stack carries the
//! modal states themselves, the four `Option` borrows here collapse into a
//! top-of-stack projection (spec §3, Wave 5).

use super::app::{App, SubagentState};

/// Borrowed, narrow render-input view over `App`.
///
/// Assembled per frame by [`ViewInputs::from_app`] immediately before
/// [`super::draw::build_render_model`]; lives only for that call.
/// `transcript` is `&mut` because `TranscriptStore::visible_window` performs
/// cache sync + scroll-clamp bookkeeping (the P9 seam) — that mutation is
/// owned by the transcript store, not by the builder.
pub(crate) struct ViewInputs<'a> {
    /// Full-screen suppression gate: gamba owns the terminal, skip the frame.
    /// Carries only the flag — the `Child` handle stays on `App` (P7 decides
    /// its ModalStack membership; it is never rendered).
    pub(crate) gamba_active: bool,

    // ── transcript (P9 store) ──
    pub(crate) transcript: &'a mut super::transcript::TranscriptStore,

    // ── input / edit chrome ──
    /// Flat input text, materialized ONCE per frame from the editor
    /// (`App::input_text`) in [`ViewInputs::from_app`]. Owned because the
    /// editor stores lines, not a flat string — this is the single per-frame
    /// flattening point.
    pub(crate) input: String,
    pub(crate) cursor_pos: usize,

    // ── status / spinner / identity ──
    pub(crate) streaming: bool,
    pub(crate) spinner_frame: usize,
    pub(crate) agent_name: &'a str,
    pub(crate) status_text: &'a Option<String>,

    // ── boot/exit logo animation clocks ──
    pub(crate) logo_build_t: Option<f64>,
    pub(crate) logo_dismiss_t: Option<f64>,

    // ── panes / pills / toasts ──
    pub(crate) subagents: &'a [SubagentState],
    pub(crate) sidecars: &'a std::collections::HashMap<String, super::sidecar::SidecarUiState>,
    pub(crate) active_tasks: &'a std::sync::Arc<synaps_cli::extensions::active_tasks::ActiveTasks>,
    pub(crate) toasts: &'a super::toast::ToastProvider,

    // ── footer accounting (session totals; per-turn counters stay on App) ──
    pub(crate) session_cost: f64,
    pub(crate) total_input_tokens: u64,
    pub(crate) total_output_tokens: u64,
    pub(crate) total_cache_read_tokens: u64,
    pub(crate) total_cache_creation_tokens: u64,
    pub(crate) total_cache_write_1h: u64,
    pub(crate) last_turn_context: u64,
    pub(crate) last_turn_context_window: u64,

    // ── modal projections (read-only here; ownership is P7's — spec §3) ──
    pub(crate) settings: &'a Option<super::settings::SettingsState>,
    pub(crate) plugins: &'a Option<super::plugins::PluginsModalState>,
    pub(crate) models: &'a Option<super::models::ModelsModalState>,
    pub(crate) help_find: &'a Option<synaps_cli::help::HelpFindState>,
    pub(crate) effort: &'a Option<super::effort::EffortModalState>,
    /// Settings-modal-scoped health snapshot input (spec §3.1).
    pub(crate) model_health:
        &'a std::collections::HashMap<String, (synaps_cli::runtime::openai::ping::PingStatus, u64)>,
    pub(crate) secret_prompts: &'a synaps_cli::tools::SecretPromptQueue,
    pub(crate) modal_stack: &'a super::focus::ModalStack,
}

impl<'a> ViewInputs<'a> {
    /// Project the render-input subset out of `App`.
    ///
    /// This is the ONLY place the render path touches `App`. Everything the
    /// builder consumes is named here; everything it may mutate comes back
    /// as a [`RenderPatch`].
    pub(crate) fn from_app(app: &'a mut App) -> Self {
        // Flatten editor state before the disjoint field borrows below.
        let input = app.input_text();
        let cursor_pos = app.cursor_char_pos();
        Self {
            gamba_active: app.gamba_child.is_some(),
            transcript: &mut app.transcript,
            input,
            cursor_pos,
            streaming: app.streaming,
            spinner_frame: app.spinner_frame,
            agent_name: &app.agent_name,
            status_text: &app.status_text,
            logo_build_t: app.logo_build_t,
            logo_dismiss_t: app.logo_dismiss_t,
            subagents: &app.subagents,
            sidecars: &app.sidecars,
            active_tasks: &app.active_tasks,
            toasts: &app.toasts,
            session_cost: app.session_cost,
            total_input_tokens: app.total_input_tokens,
            total_output_tokens: app.total_output_tokens,
            total_cache_read_tokens: app.total_cache_read_tokens,
            total_cache_creation_tokens: app.total_cache_creation_tokens,
            total_cache_write_1h: app.total_cache_write_1h,
            last_turn_context: app.last_turn_context,
            last_turn_context_window: app.last_turn_context_window,
            settings: &app.settings,
            plugins: &app.plugins,
            models: &app.models,
            help_find: &app.help_find,
            effort: &app.effort,
            model_health: &app.model_health,
            secret_prompts: &app.secret_prompts,
            modal_stack: &app.modal_stack,
        }
    }
}

/// The complete set of `App` mutations the render builder may request.
///
/// Resolution (A) from the T199.1 spec §4: instead of the builder writing
/// `help_find` visible-height back through `&mut App`, it computes the value,
/// applies it to the *snapshot clone* (so the current frame is byte-identical
/// to the old in-builder write-back), and returns it here for the caller to
/// apply to the authoritative modal state. Resolution (B) — moving the
/// geometry mirror into the P7 modal owner — is the follow-up home.
#[must_use = "the patch must be applied to App or the authoritative modal state desyncs"]
#[derive(Debug, Default)]
pub(crate) struct RenderPatch {
    /// `help_find` scroll-window height derived from terminal geometry.
    pub(crate) help_find_visible_height: Option<usize>,
}

impl RenderPatch {
    /// Apply the requested mutations to authoritative `App` state.
    ///
    /// Called on the main task immediately after `build_render_model`
    /// returns, before any input handling can observe the modal state.
    pub(crate) fn apply(self, app: &mut App) {
        if let Some(h) = self.help_find_visible_height {
            if let Some(ref mut hf) = app.help_find {
                hf.set_visible_height(h);
            }
        }
    }
}

/// Visual wrap metrics for the input editor: `(total_lines, cursor_row,
/// cursor_col)` given an inner width. Free function so both the layout math
/// in `build_render_model` (via [`ViewInputs`]) and `App::input_wrap_info`
/// share one implementation.
pub(crate) fn input_wrap_info(input: &str, cursor_pos: usize, inner_width: u16) -> (u16, u16, u16) {
    use super::text_metrics::char_width;
    let w = inner_width.max(1) as usize;
    // prefix "❯ " is 2 display columns (only on first line)
    let prefix_width: usize = 2;

    let mut row: u16 = 0;
    let mut col: usize = prefix_width;
    let mut cursor_row: u16 = 0;
    let mut cursor_col: u16 = prefix_width as u16;

    for (i, ch) in input.chars().enumerate() {
        if i == cursor_pos {
            cursor_row = row;
            cursor_col = col as u16;
        }
        if ch == '\n' {
            row += 1;
            col = prefix_width; // continuation lines also have 2-char indent
            continue;
        }
        let cw = char_width(ch);
        if col + cw > w {
            row += 1;
            col = 0;
        }
        col += cw;
    }
    // If cursor is at the end
    if cursor_pos == input.chars().count() {
        cursor_row = row;
        cursor_col = col as u16;
        // If cursor is exactly at the wrap boundary
        if col >= w {
            cursor_row += 1;
            cursor_col = 0;
        }
    }

    let total_lines = row + 1;
    (total_lines, cursor_row, cursor_col)
}
