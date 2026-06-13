//! Owned, `Send`-safe snapshot of everything `draw()` reads.
//!
//! Built on the main task each frame in [`super::draw::build_render_model`].
//! In Step 1 it is consumed synchronously by [`super::draw::render_frame`] on
//! the same task — no channel, no threading.  Step 2 will ship it over a
//! latest-wins slot to a dedicated render `std::thread`.
//!
//! **Invariant**: zero borrows back into `App`.  If `render_frame` compiles
//! without an `&App` parameter, the snapshot is proven complete.

use std::sync::Arc;

use ratatui::layout::Rect;

use super::models::ModelsModalState;
use super::plugins::PluginsModalState;
use super::settings::{RuntimeSnapshot, SettingsState};
use super::sidecar::SidecarUiStatus;

/// The per-frame snapshot shipped to the render side.
pub(crate) struct RenderModel {
    // ── Header / status ──────────────────────────────────────────────────────
    pub(crate) status_text: Option<String>,
    pub(crate) streaming: bool,
    pub(crate) spinner_frame: usize,

    /// Sidecar pills projected out of the live `HashMap<String, SidecarUiState>`.
    /// Ordered by importance desc then display_name asc, as computed on the
    /// main side.  Strips the `Child` process entirely.
    pub(crate) sidecar_pills: Vec<SidecarPillSnap>,

    /// `runtime.model()` string — cheap clone, avoids shipping the Runtime.
    pub(crate) runtime_model: String,
    /// `runtime.thinking_level()` string.
    pub(crate) runtime_thinking: String,

    // ── Messages pane ────────────────────────────────────────────────────────
    /// Pre-rendered, width-keyed line cache.  `Arc` so snapshot clone is a
    /// refcount bump, not a deep copy.
    pub(crate) lines: Arc<[ratatui::text::Line<'static>]>,
    /// Content width the cache was built at (for diagnostic / assert use in Step 2).
    #[allow(dead_code)]
    pub(crate) lines_width: usize,
    /// Final scroll offset, already clamped on the main side.
    pub(crate) scroll_back: u16,
    /// `(start, end)` index pair into `lines` for the visible viewport.
    pub(crate) visible_range: (usize, usize),
    /// Text-selection overlay coordinates (cell-level), pre-computed.
    pub(crate) selection: Option<(u16, u16, u16, u16)>,
    /// `true` when there are no messages — drives logo visibility.
    pub(crate) messages_empty: bool,
    /// Inner content rect saved for mouse-coordinate mapping on next event.
    /// Computed deterministically on the main side; consumed by `input.rs`.
    #[allow(dead_code)]
    pub(crate) msg_inner_rect: Rect,

    // ── Logo / boot animation ─────────────────────────────────────────────────
    pub(crate) logo_build_t: Option<f64>,
    pub(crate) logo_dismiss_t: Option<f64>,

    // ── Subagent panel ────────────────────────────────────────────────────────
    pub(crate) subagents: Vec<SubagentSnap>,

    // ── Active-task progress bar ──────────────────────────────────────────────
    /// Arc snapshot of `app.active_tasks` — refcount bump, not a deep clone.
    pub(crate) active_tasks: std::sync::Arc<synaps_cli::extensions::active_tasks::ActiveTasks>,

    // ── Input box ────────────────────────────────────────────────────────────
    pub(crate) input: String,
    pub(crate) cursor_pos: usize,
    /// Pre-computed ghost-hint for slash-command completion.
    pub(crate) ghost_hint: Option<GhostHint>,

    // ── Footer ───────────────────────────────────────────────────────────────
    pub(crate) show_full_output: bool,
    pub(crate) session_cost: f64,
    pub(crate) total_input_tokens: u64,
    pub(crate) total_output_tokens: u64,
    pub(crate) total_cache_read_tokens: u64,
    pub(crate) total_cache_creation_tokens: u64,
    pub(crate) total_cache_write_1h: u64,
    pub(crate) last_turn_context: u64,
    pub(crate) last_turn_context_window: u64,

    // ── Toasts ───────────────────────────────────────────────────────────────
    pub(crate) toasts: Vec<super::toast::Toast>,

    // ── Modals ───────────────────────────────────────────────────────────────
    /// `Some((state, runtime_snap))` when the settings modal is open.
    pub(crate) settings: Option<(SettingsState, RuntimeSnapshot)>,
    pub(crate) plugins: Option<PluginsModalState>,
    pub(crate) models: Option<ModelsModalState>,
    /// Cloned snapshot of `HelpFindState`.  `help_find::render` takes
    /// `&mut HelpFindState`; in Step 1 we clone per-frame and let render
    /// mutate its local copy.  The authoritative state stays on `App`.
    pub(crate) help_find: Option<synaps_cli::help::HelpFindState>,

    // ── Secret prompt modal ───────────────────────────────────────────────────
    pub(crate) secret_prompt: Option<SecretPromptSnap>,

    // ── Edge-scrub geometry ───────────────────────────────────────────────────
    /// Pre-computed `protected_bottom_rows` for `scrub_crossterm_terminal_edges`.
    pub(crate) protected_bottom_rows: u16,

}

// ── Projection types ─────────────────────────────────────────────────────────

/// Render-safe projection of `SidecarUiState` — strips the `Child` process.
#[derive(Clone)]
pub(crate) struct SidecarPillSnap {
    /// Plugin id — retained for Step 2 ordering / debug; not read by renderer.
    #[allow(dead_code)]
    pub(crate) plugin_id: String,
    pub(crate) display_name: Option<String>,
    pub(crate) status: SidecarUiStatus,
    pub(crate) armed: bool,
}

/// Render-safe projection of `SubagentState`.
#[derive(Clone)]
pub(crate) struct SubagentSnap {
    pub(crate) name: String,
    pub(crate) status: String,
    /// `duration_secs.unwrap_or_else(|| start_time.elapsed().as_secs_f64())`
    /// pre-computed so the render thread doesn't need `Instant`.
    pub(crate) elapsed_secs: f64,
    pub(crate) done: bool,
}

/// Pre-computed ghost-hint for slash-command completion in the input box.
#[derive(Clone)]
pub(crate) struct GhostHint {
    /// Either the suffix after the user's partial (prefix match) or
    /// `" → /command"` form.
    pub(crate) ghost_text: String,
    /// Multi-match badge: `"N matches · Tab search"`.
    pub(crate) match_badge: Option<String>,
}

/// Render-safe projection of `PendingSecretPrompt`.
#[derive(Clone)]
pub(crate) struct SecretPromptSnap {
    pub(crate) title: String,
    pub(crate) prompt: String,
    pub(crate) masked_buffer_chars: usize,
}
