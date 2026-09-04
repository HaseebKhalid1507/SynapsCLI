use synaps_cli::pricing::calculate_cost_optional_split;
use synaps_cli::Session;

/// `SessionHeader` → the `Session` the TUI mirrors (`App.session`). Never
/// carries `api_messages` — the actor owns the journal.
pub(crate) fn session_from_header(h: &agent_engine::session::SessionHeader) -> Session {
    let mut s = Session::new(&h.model, &h.thinking_level, h.system_prompt.as_deref());
    apply_header(&mut s, h);
    s
}

/// `SessionEventWire::CompactionApplied` fields, parked until the
/// successor `Conversation` lands.
pub(crate) struct CompactionApplied {
    pub previous_session_id: String,
    /// The successor id (also carried by the following `Conversation`).
    #[allow(dead_code)]
    pub session_id: String,
    pub chains_advanced: Vec<String>,
    pub queued_restored: Option<String>,
    pub msg_count: usize,
}

/// `SessionEventWire::Resumed` fields, parked until the `Conversation`.
pub(crate) struct ResumePending {
    pub old_id: String,
    pub new_id: String,
    pub via: Option<String>,
    pub clamp_notice: Option<String>,
}

/// Scrollback cap for [`super::transcript::TranscriptStore::set_scrollback`]
/// by transport (PLAN-phase4 §2.3): Socket → 400 msgs / 2 MiB, Local → 0/0
/// (unbounded — today's behaviour, so the R-vs-L differential cannot move).
/// `SYNAPS_TUI_SCROLLBACK` / `SYNAPS_TUI_SCROLLBACK_BYTES` (aliases
/// `SYNAPS_CLIENT_SCROLLBACK_MSGS` / `_BYTES`) override either (0 =
/// unbounded); unparsable values fall back to the mode default.
pub(crate) fn scrollback_from_env(mode: &super::run_setup::TransportMode) -> (usize, usize) {
    let (msgs, bytes) = match mode {
        super::run_setup::TransportMode::Socket => (400, 2 * 1024 * 1024),
        super::run_setup::TransportMode::Local { .. } => (0, 0),
    };
    let env = |keys: [&str; 2], default: usize| {
        keys.iter()
            .find_map(|k| std::env::var(k).ok())
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(default)
    };
    (
        env(["SYNAPS_TUI_SCROLLBACK", "SYNAPS_CLIENT_SCROLLBACK_MSGS"], msgs),
        env(["SYNAPS_TUI_SCROLLBACK_BYTES", "SYNAPS_CLIENT_SCROLLBACK_BYTES"], bytes),
    )
}

pub(crate) fn apply_header(s: &mut Session, h: &agent_engine::session::SessionHeader) {
    s.id = h.id.clone();
    s.title = h.title.clone();
    s.name = h.name.clone();
    s.model = h.model.clone();
    s.thinking_level = h.thinking_level.clone();
    s.system_prompt = h.system_prompt.clone();
    s.created_at = h.created_at;
    s.updated_at = h.updated_at;
    s.parent_session = h.parent_session.clone();
}

// Type re-export shims from slice (a). Kept this release — deleting is churn
// for no gain. One-release grace per design §5(f).
// TODO(P9-followup): drop re-exports; callers should import from transcript directly.
pub(crate) use super::transcript::{ChatMessage, TranscriptStore, THINKING_PLACEHOLDER};
// Test-only re-exports: production code reaches the cache via store methods;
// only ported cache tests name these types directly.
#[allow(unused_imports)]
pub(crate) use super::transcript::TimestampedMsg;
#[allow(unused_imports)]
pub(crate) use super::transcript::{CacheState, LineCache, MsgSlot, RenderCtx};

/// CP-11 fix-3 sibling audit: bounded capacity for the extension widget
/// event lane. Producers are extension-controlled notification watchers;
/// see the `App::widget_rx` field docs for the drop-on-overflow policy.
pub(crate) const WIDGET_EVENT_QUEUE_CAPACITY: usize = 256;

/// Central TUI state.
///
/// T199.2 boundary: `App` is **loop state**, not render input. The render
/// builder ([`super::draw::build_render_model`]) never takes `App`; it takes
/// [`super::view_model::ViewInputs`], which names the exact render-input
/// subset of these fields (input/chrome, session totals, panes, modal
/// projections). Fields NOT in `ViewInputs` — per-turn accounting, history/
/// paste/tab bookkeeping, channel endpoints, async task handles — are
/// invisible to the renderer by construction. Builder-requested mutations
/// come back as a [`super::view_model::RenderPatch`].
pub(crate) struct App {
    pub(crate) transcript: TranscriptStore,
    /// Sole input-buffer state (hybrid plan §3.1, tui-textarea-2). Never
    /// rendered — snapshot time derives flat `(text, cursor)` via the
    /// accessors (`input_text`, `cursor_char_pos`) and feeds the unchanged
    /// soft-wrap render pipeline.
    pub(crate) editor: tui_textarea::TextArea<'static>,
    pub(crate) api_messages: Vec<synaps_cli::SharedMessage>,
    pub(crate) streaming: bool,
    /// `api_messages.len()` at active-turn start. Failure repair may only
    /// remove messages appended at or after this index (spec §5.2).
    pub(crate) turn_baseline: usize,
    pub(crate) input_history: Vec<String>,
    pub(crate) history_index: Option<usize>,
    pub(crate) input_stash: String,
    /// Tab-completion cycle state for slash commands.
    /// `Some((prefix, index, matching_commands))` when the user is cycling
    /// through matches via repeated Tab; cleared on any non-Tab keypress.
    /// See input.rs::handle_tab_complete.
    pub(crate) tab_cycle: Option<(String, usize, Vec<String>)>,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) total_input_tokens: u64,
    pub(crate) total_output_tokens: u64,
    pub(crate) total_cache_read_tokens: u64,
    pub(crate) total_cache_creation_tokens: u64,
    /// Per-TTL-bucket accumulators for cache writes.  Populated by
    /// `add_usage` when the API returns the 5m/1h split in `SessionEvent::Usage`.
    /// Zero when no split has arrived (e.g. cache_ttl=default, no writes yet).
    pub(crate) total_cache_write_5m: u64,
    pub(crate) total_cache_write_1h: u64,
    /// Most recent turn's actual context occupancy (what the API ingested
    /// this request): uncached input + cache read + cache creation. Unlike
    /// `total_*_tokens` which accumulate for cost tracking, this is reassigned
    /// every turn and reflects the current per-request context window use.
    /// Used by the context-usage bar in `draw.rs`.
    pub(crate) last_turn_context: u64,
    /// Context window size (in tokens) of the model that answered the most
    /// recent turn. Updated alongside `last_turn_context` so the bar's
    /// denominator adapts when users switch models mid-session. See
    /// `synaps_cli::models::context_window_for_model`.
    pub(crate) last_turn_context_window: u64,
    api_call_count: u32, // private: accounting, used only within app.rs
    pub(crate) session_cost: f64,
    pub(crate) session: Session,
    pub(crate) agent_name: String,
    pub(crate) needs_redraw: bool,
    /// When set, the next repaint bypasses the streaming redraw throttle.
    /// Set by user input (scroll/typing/cursor) so interaction stays instant
    /// even while the model is streaming; cleared after the paint.
    pub(crate) force_redraw: bool,
    pub(crate) logo_dismiss_t: Option<f64>,
    pub(crate) logo_build_t: Option<f64>,
    /// Active subagent status for the live panel
    pub(crate) subagents: Vec<SubagentState>,
    /// Counter for unique subagent IDs within a session
    /// Saved context from an aborted response — injected into the next user message
    pub(crate) abort_context: Option<String>,
    /// Message queued while streaming — auto-sent when current response finishes
    pub(crate) queued_message: Option<String>,
    /// Tracks paste state: snapshot of input before first paste, and total pasted char count
    pub(crate) input_before_paste: Option<String>,
    pub(crate) pasted_char_count: usize,
    /// Spinner frame counter (incremented on tick)
    pub(crate) spinner_frame: usize,
    /// Transient status text shown in the header bar (auto-cleared when streaming starts)
    pub(crate) status_text: Option<String>,
    /// GamblersDen child process — spawned by /gamba, killed when streaming finishes
    pub(crate) gamba_child: Option<std::process::Child>,
    /// Active settings modal state (Some while /settings is open).
    pub(crate) settings: Option<super::settings::SettingsState>,
    /// Active plugins modal state (Some while /plugins is open).
    pub(crate) plugins: Option<super::plugins::PluginsModalState>,
    /// Active models router modal state (Some while /model or /models is open).
    pub(crate) models: Option<super::models::ModelsModalState>,
    /// Active /help find lightbox state.
    pub(crate) help_find: Option<synaps_cli::help::HelpFindState>,
    /// Active /effort lightbox state (Some while /effort is open).
    pub(crate) effort: Option<super::effort::EffortModalState>,
    /// P7 modal-routing stack (finished, P7.8). The single source of input
    /// routing: `input.rs` dispatches on `modal_stack.top()`, one arm per
    /// `PaneId`. It is an *index over* the `Option<…State>` modal fields above
    /// (+ the `secret_prompts` queue), never a new owner (§6). Every open/close
    /// site pushes/pops in lock-step; membership is cross-checked against the
    /// backing fields by `debug_assert_stack_sync`. Empty ⇒ `top()` is
    /// `PaneId::Chat` ⇒ input goes to the base chat pane.
    pub(crate) modal_stack: super::focus::ModalStack,
    /// P7.8 secret-prompt queue — folded onto App from the `run()` local
    /// (§5). Drained from the mpsc channel via `poll_requests` in the tick
    /// arm; `is_active()` is mirrored by `modal_stack.contains(SecretPrompt)`
    /// via `reconcile_secret_prompt` (asserted by `debug_assert_stack_sync`).
    pub(crate) secret_prompts: synaps_cli::tools::SecretPromptQueue,
    /// Compaction in flight on the actor (`CompactionStarted` … `Applied`/
    /// `Failed`/`Cancelled`). Client-side guard for `/compact` and Submit.
    pub(crate) compacting: bool,
    /// Mirror of the actor's buffered-during-streaming event count
    /// (`ConversationSnapshot.pending_events_len`).
    pub(crate) pending_events_len: usize,
    /// Last `SubagentRows` from the actor — the tick arm reconciles the HUD
    /// from this cache at the same 1 Hz cadence the registry read had.
    pub(crate) subagent_rows: Vec<synaps_cli::tools::SubagentDisplayRow>,
    /// `CompactionApplied` waiting for the `Conversation` that carries the
    /// successor's messages (then the "✓ compacted …" lines are pushed).
    pub(crate) compaction_applied: Option<CompactionApplied>,
    /// `Resumed` reply waiting for the `Conversation` that carries the
    /// resumed session's messages (then the "switched from …" line).
    pub(crate) resume_pending: Option<ResumePending>,
    /// The last Submit text until `TurnStarted`/`Refused` (§6 #9: a refused
    /// Submit gives the editor its text back).
    pub(crate) last_submitted: Option<String>,
    /// Consecutive auto-triggered model turns since the last real user send.
    /// Incremented by the event-reactor wake path; reset on Submit / queued user message.
    /// When this reaches AUTO_TURN_CAP the reactor parks and shows a system message.
    pub(crate) consecutive_auto_turns: u32,
    /// Cached model ping results: "provider/model" -> (status, latency_ms).
    pub(crate) model_health:
        std::collections::HashMap<String, (synaps_cli::runtime::openai::ping::PingStatus, u64)>,
    /// App-level live catalog overrides (provider → (bare id, label) rows).
    /// Updated whenever a live model-list result arrives, so both the /models
    /// modal and the /settings model picker share one catalog cache.
    pub(crate) catalog_overrides:
        std::collections::BTreeMap<String, super::models::ProviderCatalogOverride>,
    /// Print ping results to chat as they arrive (set by /ping command).
    pub(crate) ping_print: bool,
    pub(crate) ping_pending: usize,
    /// Channel for receiving async ping results.
    pub(crate) ping_tx: tokio::sync::mpsc::UnboundedSender<(
        String,
        synaps_cli::runtime::openai::ping::PingStatus,
        u64,
    )>,
    pub(crate) ping_rx: tokio::sync::mpsc::UnboundedReceiver<(
        String,
        synaps_cli::runtime::openai::ping::PingStatus,
        u64,
    )>,
    /// Channel for receiving expanded model-list API results.
    pub(crate) model_list_tx: tokio::sync::mpsc::UnboundedSender<(
        String,
        Result<Vec<super::models::ExpandedModelEntry>, String>,
    )>,
    pub(crate) model_list_rx: tokio::sync::mpsc::UnboundedReceiver<(
        String,
        Result<Vec<super::models::ExpandedModelEntry>, String>,
    )>,
    /// Suppress paste events arriving shortly after a right-click copy/paste.
    /// Terminals that auto-paste on right-click generate a spurious Event::Paste
    /// immediately after MouseDown(Right). We suppress only within a short TTL
    /// window (~150ms) to avoid eating legitimate Ctrl+V pastes.
    pub(crate) suppress_paste_until: Option<std::time::Instant>,
    /// Active sidecar instances keyed by plugin id (manifest name).
    ///
    /// Phase 8 8B: replaces the legacy single `Option<SidecarUiState>` so
    /// multiple plugin-claimed sidecars can be hosted concurrently.
    pub(crate) sidecars: std::collections::HashMap<String, super::sidecar::SidecarUiState>,
    /// Generic extension-provided active tasks rendered in the sticky progress area.
    /// Stored behind `Arc` so the per-frame snapshot is a refcount bump, not a deep clone.
    pub(crate) active_tasks: std::sync::Arc<synaps_cli::extensions::active_tasks::ActiveTasks>,
    /// Overlay toast provider used by core and extension-adjacent features.
    pub(crate) toasts: super::toast::ToastProvider,
    /// Channel for async extension loader progress events.
    pub(crate) extension_loader_rx:
        tokio::sync::mpsc::UnboundedReceiver<synaps_cli::extensions::loader::ExtensionLoaderEvent>,
    pub(crate) extension_loader_tx:
        tokio::sync::mpsc::UnboundedSender<synaps_cli::extensions::loader::ExtensionLoaderEvent>,
    pub(crate) extension_loader_running: bool,
    /// Channel for receiving widget events from background extension notification watchers.
    ///
    /// CP-11 fix-3 sibling audit: BOUNDED ([`WIDGET_EVENT_QUEUE_CAPACITY`]).
    /// Producers are extension-controlled (`widget.*` notification
    /// frames), and this loop's consumer arm stalls during inline awaits
    /// — e.g. the up-to-120 s `command.invoke` window — so an unbounded
    /// channel here let a hostile extension park aggregate widget bytes
    /// in host memory. Watchers `try_send` and DROP-newest on overflow
    /// with a warn: widget upserts are idempotent last-writer-wins UI
    /// state, so the next event after the consumer resumes restores it.
    pub(crate) widget_rx:
        tokio::sync::mpsc::Receiver<synaps_cli::extensions::widgets::ExtensionWidgetEvent>,
    pub(crate) widget_tx:
        tokio::sync::mpsc::Sender<synaps_cli::extensions::widgets::ExtensionWidgetEvent>,
    /// Live keybind registry — held so /settings can hot-swap plugin toggle keys.
    pub(crate) keybinds:
        Option<std::sync::Arc<std::sync::RwLock<synaps_cli::skills::keybinds::KeybindRegistry>>>,
    /// Live MXC palettes from the myx subscriber task (theme::mxc), paired
    /// with the wire's advisory `fade_ms` (None = absent). The main loop's
    /// receiver arm is the ONLY place these are applied (animated through
    /// the same set_theme + invalidate path /theme uses); the task never
    /// mutates theme state. Unbounded is safe: the publisher dedupes and
    /// emits a handful of events per minute, and each message is small (one
    /// Theme value).
    pub(crate) myx_theme_rx:
        tokio::sync::mpsc::UnboundedReceiver<(super::theme::Theme, Option<u64>)>,
    pub(crate) myx_theme_tx: tokio::sync::mpsc::UnboundedSender<(super::theme::Theme, Option<u64>)>,
    /// The running MXC subscriber, present only while the "myx" theme is
    /// active. Aborted on theme switch-away (`sync_myx_live`) and at
    /// shutdown, so no task leaks and nothing writes after teardown.
    /// `Some` here doubles as the receive-side guard's "the active theme is
    /// still myx" bit: `sync_myx_live` is the single writer, on this thread,
    /// on every theme-apply path.
    pub(crate) myx_task: Option<tokio::task::JoinHandle<()>>,
    /// Last-good LIVE palette from the MXC subscriber. Re-applying "myx"
    /// statically (`/theme myx` again, settings Esc-revert, picker browse)
    /// restores this instead of stranding on the static snapshot until the
    /// next track change. Cleared on switch-away so a stale album palette
    /// can never resurrect through it.
    pub(crate) myx_last_live: Option<super::theme::Theme>,
    /// In-flight animated theme cross-fade (theme::transition). `Some` is
    /// the "animation active" signal the tick GUARD in mod.rs keys on; the
    /// tick arm advances it and clears it on landing, so idle cost returns
    /// to zero the moment the fade completes (#131 discipline).
    pub(crate) theme_transition: Option<super::theme::transition::ThemeTransition>,
    /// Injectable clock (P6.2). Real in production, Test in the harness so
    /// time-dependent state (toast expiry, tool timers) stays deterministic.
    pub(crate) clock: super::clock::TuiClock,
}

pub(crate) const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Clone)]
pub(crate) struct SubagentState {
    pub(crate) id: u64,
    pub(crate) name: String,
    pub(crate) status: String,
    pub(crate) start_time: std::time::Instant,
    pub(crate) done: bool,
    pub(crate) duration_secs: Option<f64>,
    /// Stamped when this entry transitions to done — drives the 5s flash expiry.
    pub(crate) done_at: Option<std::time::Instant>,
}

impl App {
    /// Test-only convenience constructor: production-equivalent App backed by
    /// the real system clock. Production code uses `new_with_clock` directly.
    #[cfg(test)]
    pub(crate) fn new(session: Session) -> Self {
        Self::new_with_clock(session, super::clock::TuiClock::real())
    }

    /// Construct an App with an explicit clock (P6.2). `run()` passes
    /// `TuiClock::real()`; the harness passes `TuiClock::test()`.
    pub(crate) fn new_with_clock(session: Session, clock: super::clock::TuiClock) -> Self {
        let (ping_tx_init, ping_rx_init) = tokio::sync::mpsc::unbounded_channel();
        let (model_list_tx_init, model_list_rx_init) = tokio::sync::mpsc::unbounded_channel();
        let (extension_loader_tx_init, extension_loader_rx_init) =
            tokio::sync::mpsc::unbounded_channel();
        let (widget_tx_init, widget_rx_init) =
            tokio::sync::mpsc::channel(WIDGET_EVENT_QUEUE_CAPACITY);
        let (myx_theme_tx_init, myx_theme_rx_init) = tokio::sync::mpsc::unbounded_channel();
        Self {
            transcript: TranscriptStore::new(clock.clone()),
            editor: tui_textarea::TextArea::default(),
            api_messages: Vec::new(),
            streaming: false,
            turn_baseline: 0,
            input_history: Vec::new(),
            history_index: None,
            input_stash: String::new(),
            tab_cycle: None,
            input_tokens: 0,
            output_tokens: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            total_cache_write_5m: 0,
            total_cache_write_1h: 0,
            last_turn_context: 0,
            last_turn_context_window: synaps_cli::models::context_window_for_model(
                synaps_cli::models::default_model(),
            ),
            api_call_count: 0,
            session_cost: 0.0,
            session,
            agent_name: synaps_cli::config::load_config()
                .agent_name
                .unwrap_or_else(|| "agent".to_string()),
            needs_redraw: true,
            force_redraw: false,
            logo_dismiss_t: None,
            // SYNAPS_NO_BOOT_FX=1: skip both the tachyonfx boot effect and the
            // ASCII logo build animation (full-screen redraws per frame are
            // brutal over high-latency/SSH links).
            logo_build_t: if std::env::var("SYNAPS_NO_BOOT_FX").is_ok_and(|v| v == "1") {
                None
            } else {
                Some(0.0)
            },
            subagents: Vec::new(),
            abort_context: None,
            queued_message: None,
            input_before_paste: None,
            pasted_char_count: 0,
            spinner_frame: 0,
            status_text: None,
            gamba_child: None,
            settings: None,
            plugins: None,
            models: None,
            help_find: None,
            effort: None,
            // P7.3: wired but starts EMPTY — pure no-op until P7.4 migrates a modal.
            modal_stack: super::focus::ModalStack::new(),
            // P7.8: folded off the `run()` local; production wires the mpsc
            // channel separately (mod.rs). Starts empty (no active prompt).
            secret_prompts: synaps_cli::tools::SecretPromptQueue::new(),
            compacting: false,
            pending_events_len: 0,
            subagent_rows: Vec::new(),
            compaction_applied: None,
            resume_pending: None,
            last_submitted: None,
            consecutive_auto_turns: 0,
            model_health: std::collections::HashMap::new(),
            catalog_overrides: std::collections::BTreeMap::new(),
            ping_print: false,
            ping_pending: 0,
            ping_tx: ping_tx_init,
            ping_rx: ping_rx_init,
            model_list_tx: model_list_tx_init,
            model_list_rx: model_list_rx_init,
            suppress_paste_until: None,
            sidecars: std::collections::HashMap::new(),
            active_tasks: std::sync::Arc::new(
                synaps_cli::extensions::active_tasks::ActiveTasks::new(),
            ),
            toasts: super::toast::ToastProvider::new(clock.clone()),
            extension_loader_rx: extension_loader_rx_init,
            extension_loader_tx: extension_loader_tx_init,
            extension_loader_running: false,
            widget_rx: widget_rx_init,
            widget_tx: widget_tx_init,
            myx_theme_rx: myx_theme_rx_init,
            myx_theme_tx: myx_theme_tx_init,
            myx_task: None,
            myx_last_live: None,
            theme_transition: None,
            keybinds: None,
            clock,
        }
    }
    /// Build the text shown in the chat transcript for a submitted user message.
    /// Large pasted payloads are collapsed to a label, while any text typed before
    /// or after the pasted range remains visible.
    pub(crate) fn user_display_text_for_submission(&self, input: &str) -> String {
        if self.pasted_char_count == 0 {
            return input.to_string();
        }

        let before_paste = self.input_before_paste.as_deref().unwrap_or("");
        let before_chars = before_paste.chars().count();
        let total_chars = input.chars().count();
        let paste_chars = self
            .pasted_char_count
            .min(total_chars.saturating_sub(before_chars));
        let after_chars = total_chars.saturating_sub(before_chars + paste_chars);

        let paste_byte_start = input
            .char_indices()
            .nth(before_chars)
            .map(|(i, _)| i)
            .unwrap_or(input.len());
        let paste_byte_end = input
            .char_indices()
            .nth(before_chars + paste_chars)
            .map(|(i, _)| i)
            .unwrap_or(input.len());

        let pasted = &input[paste_byte_start..paste_byte_end];
        let after_paste = if after_chars == 0 {
            ""
        } else {
            &input[paste_byte_end..]
        };

        let line_count = pasted.lines().count();
        let paste_label = if line_count > 1 {
            format!("[Pasted {} lines]", line_count)
        } else {
            format!("[Pasted {} chars]", paste_chars)
        };

        match (before_paste.is_empty(), after_paste.is_empty()) {
            (true, true) => paste_label,
            (false, true) => format!("{} {}", before_paste.trim(), paste_label),
            (true, false) => format!("{} {}", paste_label, after_paste.trim()),
            (false, false) => format!(
                "{} {} {}",
                before_paste.trim(),
                paste_label,
                after_paste.trim()
            ),
        }
    }

    // ── Editor accessors (hybrid plan §3.1) ────────────────────────────────
    // The editor is the sole input-buffer state; these accessors are the only
    // way in/out. The render path materializes one flat `(text, cursor)` pair
    // per frame in `ViewInputs::from_app`.

    /// Flat input text — editor lines joined by `\n`.
    pub(crate) fn input_text(&self) -> String {
        self.editor.lines().join("\n")
    }

    /// First line of the buffer, borrowed — for cheap hot-path guards that
    /// only care about the line a slash command lives on (`/`-detection).
    pub(crate) fn input_first_line(&self) -> &str {
        self.editor.lines().first().map_or("", |s| s.as_str())
    }

    /// True when the input buffer contains no text at all.
    pub(crate) fn input_is_empty(&self) -> bool {
        let lines = self.editor.lines();
        lines.len() <= 1 && lines.first().map_or(true, |l| l.is_empty())
    }

    /// Flat **char** index of the editor cursor within `input_text()`.
    pub(crate) fn cursor_char_pos(&self) -> usize {
        flat_cursor_pos(self.editor.lines(), self.editor.cursor())
    }

    /// Replace the whole buffer, cursor moved to the end of the text.
    pub(crate) fn set_input_text(&mut self, s: &str) {
        self.editor = tui_textarea::TextArea::from(s.split('\n'));
        self.editor.move_cursor(tui_textarea::CursorMove::Bottom);
        self.editor.move_cursor(tui_textarea::CursorMove::End);
    }

    /// Clear the whole buffer (today's Ctrl-U semantics — plan §3.2 note).
    pub(crate) fn clear_input(&mut self) {
        self.editor = tui_textarea::TextArea::default();
    }

    /// Insert text at the cursor (paste, sidecar transcription, tab-complete).
    pub(crate) fn insert_at_cursor(&mut self, s: &str) {
        self.editor.insert_str(s);
    }

    /// Calculate the number of visual lines the input needs, given an inner width.
    /// Returns (total_lines, cursor_row, cursor_col) for layout and cursor placement.
    ///
    /// Replace the conversation mirrors from a `Conversation(_)` envelope
    /// (PLAN-phase3 §2.4: replace, never merge). Per-turn `input_tokens`/
    /// `output_tokens`, the TTL-split cache counters and `api_call_count`
    /// are client-side per-turn state and stay as `Stream(Usage)` left them.
    pub(crate) fn apply_conversation(&mut self, conv: &agent_engine::session::ConversationSnapshot) {
        self.api_messages = conv.api_messages.clone();
        self.total_input_tokens = conv.tokens.input;
        self.total_output_tokens = conv.tokens.output;
        self.total_cache_read_tokens = conv.tokens.cache_read;
        self.total_cache_creation_tokens = conv.tokens.cache_creation;
        self.session_cost = conv.cost;
        self.abort_context = conv.abort_context.clone();
        self.queued_message = conv.queued_message.clone();
        self.pending_events_len = conv.pending_events_len;
        self.consecutive_auto_turns = conv.consecutive_auto_turns;
        apply_header(&mut self.session, &conv.header);
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_usage(
        &mut self,
        input_tokens: u64,
        output_tokens: u64,
        cache_read: u64,
        cache_creation: u64,
        cache_creation_5m: Option<u64>,
        cache_creation_1h: Option<u64>,
        model: &str,
        context_window_override: Option<u64>,
    ) {
        self.input_tokens = input_tokens;
        self.output_tokens = output_tokens;
        self.total_input_tokens += input_tokens;
        self.total_output_tokens += output_tokens;
        self.total_cache_read_tokens += cache_read;
        self.total_cache_creation_tokens += cache_creation;
        // Accumulate per-TTL-bucket write totals for /stats and footer display.
        if let Some(c5) = cache_creation_5m {
            self.total_cache_write_5m += c5;
        }
        if let Some(c1) = cache_creation_1h {
            self.total_cache_write_1h += c1;
        }
        // Per-turn context occupancy (bar numerator): what the API actually
        // ingested this request. Output tokens are generated, not ingested,
        // so they don't count toward current-window use. Reassigned, not accumulated.
        self.last_turn_context = input_tokens + cache_read + cache_creation;
        // Per-turn bar denominator — the context window of the model that
        // answered this turn. Tracked alongside so mid-session model swaps
        // (e.g. main thread Opus → subagent Sonnet) recalibrate the bar.
        // If the user configured an explicit context_window, honour it.
        self.last_turn_context_window = context_window_override
            .unwrap_or_else(|| synaps_cli::models::context_window_for_model(model));
        self.api_call_count += 1;
        // Delegate cost calculation to the single source of truth in `pricing`.
        // Split-aware: 1h cache writes bill at 2.0x when the TTL split arrived.
        self.session_cost += calculate_cost_optional_split(
            model,
            input_tokens,
            output_tokens,
            cache_read,
            cache_creation,
            cache_creation_5m,
            cache_creation_1h,
        );
    }

    // ── Slice (b′) delegating wrappers ──────────────────────────────────
    //
    // Content mutations, tool routing, and the invalidate family moved to
    // TranscriptStore (transcript.rs). These wrappers keep identical
    // signatures so call sites don't churn (locked decision #3); redraw
    // signaling (`needs_redraw`) stays here on App.

    pub(crate) fn push_msg(&mut self, msg: ChatMessage) {
        self.transcript.push_msg(msg);
        self.needs_redraw = true;
    }

    /// Delegates to [`TranscriptStore::cap_resumed_display`]. Note: does not
    /// signal a redraw — verbatim-preserves the pre-move behavior (the method
    /// never invalidated; it runs at resume before any cache exists).
    pub(crate) fn cap_resumed_display(&mut self, cap: usize) {
        self.transcript.cap_resumed_display(cap);
    }

    /// Mark the cached message lines stale — they'll be rebuilt on the next draw.
    /// Call this after any mutation that changes how `messages` renders.
    /// Use for structural changes (theme, width, message list reshuffle). For
    /// streaming deltas prefer `invalidate_last()` which is O(1).
    pub(crate) fn invalidate(&mut self) {
        self.transcript.invalidate();
        self.needs_redraw = true;
    }

    /// Mark only the tail message dirty. O(1) during streaming.
    pub(crate) fn invalidate_last(&mut self) {
        self.transcript.invalidate_last();
        self.needs_redraw = true;
    }

    /// Request a redraw without invalidating the line cache (e.g. for
    /// panel-only changes like spinner/timer updates, scroll, cursor blink).
    pub(crate) fn request_redraw(&mut self) {
        self.needs_redraw = true;
    }

    /// Request an immediate repaint that bypasses the streaming redraw throttle.
    /// Use for user-driven changes (scroll, typing, cursor, paste, resize) that
    /// must feel instant even while the model is streaming. Streaming text
    /// deltas use the throttled `request_redraw` / `invalidate` paths.
    pub(crate) fn request_immediate_redraw(&mut self) {
        self.needs_redraw = true;
        self.force_redraw = true;
    }

    /// Advance spinner/animation state.
    ///
    /// Returns true only when text generated from `render_lines` may have changed
    /// and the cached message lines must be rebuilt. Full-screen overlays and
    /// header/panel spinners are redrawn from current state without touching the
    /// message cache, avoiding unnecessary whole-terminal clears on every frame.
    pub(crate) fn advance_animations(&mut self) -> bool {
        self.spinner_frame = self.spinner_frame.wrapping_add(1);
        if self.spinner_frame % 3 != 0 {
            return false;
        }

        self.transcript.uses_spinner()
    }

    /// True when a full terminal clear is needed before an animated redraw.
    ///
    /// The message pane is already cleared locally in `draw()` before rendering
    /// the transcript, and rendered lines are clamped to terminal display width.
    /// Calling `terminal.clear()` on streaming animation frames repaints the
    /// whole alternate screen and causes visible flicker, so animation ticks
    /// should not request a full-screen clear.
    pub(crate) fn needs_clear_for_animation_redraw(&self) -> bool {
        false
    }

    // ── Tool-event routing ──────────────────────────────────────────────
    //
    // Routing logic lives in TranscriptStore (transcript.rs) since slice (b′);
    // these are signature-identical delegates.

    /// Delegates to [`TranscriptStore::on_tool_use_start`].
    pub(crate) fn on_tool_use_start(&mut self, tool_id: String, tool_name: String) {
        self.transcript.on_tool_use_start(tool_id, tool_name);
        self.needs_redraw = true;
    }

    /// Delegates to [`TranscriptStore::on_tool_use_delta`].
    pub(crate) fn on_tool_use_delta(&mut self, tool_id: &str, delta: &str) {
        self.transcript.on_tool_use_delta(tool_id, delta);
        self.needs_redraw = true;
    }

    /// Delegates to [`TranscriptStore::on_tool_use_finalized`].
    pub(crate) fn on_tool_use_finalized(
        &mut self,
        tool_id: String,
        tool_name: String,
        input_str: String,
    ) {
        self.transcript
            .on_tool_use_finalized(tool_id, tool_name, input_str);
        self.needs_redraw = true;
    }

    /// Delegates to [`TranscriptStore::on_tool_result_delta`].
    pub(crate) fn on_tool_result_delta(&mut self, tool_id: String, delta: String) {
        self.transcript.on_tool_result_delta(tool_id, delta);
        self.needs_redraw = true;
    }

    /// Delegates to [`TranscriptStore::on_tool_result`].
    pub(crate) fn on_tool_result(&mut self, tool_id: String, result: String) {
        self.transcript.on_tool_result(tool_id, result);
        self.needs_redraw = true;
    }

    pub(crate) fn history_up(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        match self.history_index {
            None => {
                self.input_stash = self.input_text();
                self.history_index = Some(self.input_history.len() - 1);
            }
            Some(i) if i > 0 => {
                self.history_index = Some(i - 1);
            }
            _ => return,
        }
        if let Some(idx) = self.history_index {
            let text = self.input_history[idx].clone();
            // set_input_text rebuilds the editor with the cursor at the end.
            self.set_input_text(&text);
        }
    }

    pub(crate) fn history_down(&mut self) {
        if let Some(i) = self.history_index {
            if i + 1 < self.input_history.len() {
                self.history_index = Some(i + 1);
                let text = self.input_history[i + 1].clone();
                self.set_input_text(&text);
            } else {
                self.history_index = None;
                let stash = std::mem::take(&mut self.input_stash);
                self.set_input_text(&stash);
            }
        }
    }

    pub(crate) fn append_or_update_text(&mut self, text: &str) {
        self.transcript.append_or_update_text(text);
        self.needs_redraw = true;
    }

    pub(crate) fn append_or_update_thinking(&mut self, text: &str) {
        self.transcript.append_or_update_thinking(text);
        self.needs_redraw = true;
    }

    /// Delegates to [`TranscriptStore::drop_empty_thinking`].
    pub(crate) fn drop_empty_thinking(&mut self) {
        self.transcript.drop_empty_thinking();
        self.needs_redraw = true;
    }

    pub(crate) fn handle_theme_command(&mut self, arg: &str) {
        let descriptions: &[(&str, &str)] = &[
            ("default", "cool teal on dark blue-gray"),
            ("night-city", "premium neon-noir — cyberpunk/blade runner"),
            ("neon-rain", "cyberpunk hot pink + cyan"),
            ("amber", "warm CRT retro terminal"),
            ("phosphor", "green monochrome CRT"),
            ("solarized-dark", "Ethan Schoonover's classic"),
            ("blood", "dark red, Doom/horror"),
            ("ocean", "deep sea bioluminescence"),
            ("rose-pine", "elegant muted purples/pinks"),
            ("nord", "arctic frost blues"),
            ("dracula", "purple/pink/cyan vibrant"),
            ("monokai", "classic orange/pink/green"),
            ("myx", "album-reactive colors via Myx (MXC)"),
            ("gruvbox", "warm earthy tones"),
            ("catppuccin", "soft pastels, cozy dark"),
            ("tokyo-night", "dark blue-purple, soft accents"),
            ("sunset", "warm oranges/pinks dusk"),
            ("ice", "frozen arctic pale blues"),
            ("forest", "deep greens and browns"),
            ("lavender", "rich purple/violet"),
        ];

        if arg.is_empty() {
            self.push_msg(ChatMessage::System("Available themes:".to_string()));
            for (name, desc) in descriptions {
                // Soft detection annotation for the live theme: "myx" always
                // works (static fallback), but say whether Myx is around.
                if *name == "myx" {
                    let status = if super::theme::mxc::myx_detected() {
                        "live"
                    } else {
                        "static — myx not detected"
                    };
                    self.push_msg(ChatMessage::System(format!(
                        "  {:<15} — {} [{}]",
                        name, desc, status
                    )));
                    continue;
                }
                self.push_msg(ChatMessage::System(format!("  {:<15} — {}", name, desc)));
            }
            let themes_dir = synaps_cli::config::base_dir().join("themes");
            if let Ok(entries) = std::fs::read_dir(&themes_dir) {
                let mut custom: Vec<String> = entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| !descriptions.iter().any(|(d, _)| *d == n.as_str()))
                    .collect();
                custom.sort();
                for name in &custom {
                    self.push_msg(ChatMessage::System(format!("  {:<15} — custom", name)));
                }
            }
            self.push_msg(ChatMessage::System(String::new()));
            self.push_msg(ChatMessage::System(
                "Usage: /theme <name> to set. Restart to apply.".to_string(),
            ));
        } else {
            let name = arg.trim();
            let is_valid = descriptions.iter().any(|(n, _)| *n == name)
                || synaps_cli::config::base_dir()
                    .join("themes")
                    .join(name)
                    .exists();

            if is_valid {
                match synaps_cli::config::write_config_value("theme", name) {
                    Ok(_) => {
                        // Prefer the cached live palette when re-applying
                        // "myx" over a healthy subscriber — the static
                        // snapshot would strand until the next track change.
                        let new_theme = self.resolve_apply_theme(name).unwrap_or_default();
                        // Animated cross-fade (350ms default; theme_transition
                        // knob can retune or disable it). The tick arm applies
                        // frames through the same set_theme path as before.
                        self.apply_theme_animated(new_theme, None);
                        // Reconcile the live-MXC layer: spawn the subscriber
                        // when switching TO myx, abort it when switching away.
                        self.sync_myx_live(name);
                        self.push_msg(ChatMessage::System(format!("Theme applied: {}", name)));
                        self.invalidate();
                    }
                    Err(e) => {
                        self.push_msg(ChatMessage::Error(format!("failed to write config: {}", e)));
                    }
                }
            } else {
                self.push_msg(ChatMessage::Error(format!(
                    "unknown theme: '{}'. Use /theme to list available themes.",
                    name
                )));
            }
        }
    }

    /// Start (or retarget) an animated theme change toward `target`.
    /// `requested` is a per-change advisory duration (MXC wire `fade_ms`,
    /// clamped upstream); `None` = the configured default. When transitions
    /// are off (or the duration resolves to zero) this snaps instantly —
    /// byte-identical to the old `set_theme` behavior.
    pub(crate) fn apply_theme_animated(
        &mut self,
        target: super::theme::Theme,
        requested: Option<std::time::Duration>,
    ) {
        super::theme::transition::apply_animated(
            &mut self.theme_transition,
            target,
            requested,
            std::time::Instant::now(),
        );
        self.invalidate();
    }

    /// Reconcile the background MXC subscriber with the active theme name.
    ///
    /// `"myx"` active and no live task → spawn `theme::mxc::run_subscriber`
    /// with a clone of our sender. Any other theme → abort the task if one is
    /// running. Idempotent, so callers can invoke it on every theme change
    /// and at boot. Must run on the main loop's runtime (it spawns).
    pub(crate) fn sync_myx_live(&mut self, theme_name: &str) {
        let want = theme_name == "myx";
        let running = self.myx_task.as_ref().is_some_and(|h| !h.is_finished());
        if want && !running {
            let tx = self.myx_theme_tx.clone();
            self.myx_task = Some(tokio::spawn(super::theme::mxc::run_subscriber(tx)));
        } else if !want {
            if let Some(h) = self.myx_task.take() {
                h.abort();
            }
            // A queued stale palette must not resurrect through the
            // last-good cache after switch-away (the receive-side guard in
            // `handle_myx_theme_arm` drops the message itself).
            self.myx_last_live = None;
        }
    }

    /// Resolve the theme to apply for `name`: prefers the cached last-good
    /// LIVE palette when (re-)applying "myx" over a running subscriber, so
    /// `/theme myx`, settings picker browse, and Esc-revert land back on the
    /// palette that is actually current — not the static snapshot.
    pub(crate) fn resolve_apply_theme(&self, name: &str) -> Option<super::theme::Theme> {
        if name == "myx" && self.myx_task.is_some() {
            if let Some(live) = &self.myx_last_live {
                return Some(live.clone());
            }
        }
        super::theme::load_theme_by_name(name)
    }

    /// Settings-modal theme commit: the same persisted + animated apply as
    /// `/theme <name>`, INCLUDING the live-MXC subscriber reconcile. Every
    /// theme-apply path that changes the persisted theme must call
    /// `sync_myx_live`, or switching away from myx leaks the subscriber
    /// (album colors re-stomp the chosen theme on every track change) and
    /// switching to myx never goes live.
    pub(crate) fn apply_theme_from_settings(&mut self, name: &str) {
        let target = self.resolve_apply_theme(name).unwrap_or_default();
        self.apply_theme_animated(target, None);
        self.sync_myx_live(name);
    }
}

/// Test-only render delegates. Production rendering goes through
/// `TranscriptStore::sync_cache` (draw.rs); tests keep calling through App so
/// the incremental-cache regression suite ports with the move (slice d).
#[cfg(test)]
impl App {
    pub(crate) fn render_ctx(&self) -> RenderCtx<'_> {
        RenderCtx {
            spinner_frame: self.spinner_frame,
            streaming: self.streaming,
            agent_name: &self.agent_name,
        }
    }

    pub(crate) fn render_message_lines(&self, idx: usize, width: usize) -> MsgSlot {
        self.transcript
            .render_message_lines(idx, width, &self.render_ctx())
    }

    pub(crate) fn render_lines(&self, width: usize) -> Vec<ratatui::text::Line<'static>> {
        self.transcript.render_lines(width, &self.render_ctx())
    }
}

/// Map the editor's `(row, col)` char-wise cursor to a flat char index into
/// `lines.join("\n")` — the only new math in the hybrid design (plan §3.1):
/// `sum(chars(lines[..row]) + 1) + col`, the `+ 1` counting each `\n`.
/// `col` is clamped to the row's char count defensively.
pub(crate) fn flat_cursor_pos(lines: &[String], (row, col): (usize, usize)) -> usize {
    let mut flat = 0;
    for line in lines.iter().take(row) {
        flat += line.chars().count() + 1; // +1 for the joining '\n'
    }
    let row_chars = lines.get(row).map_or(0, |l| l.chars().count());
    flat + col.min(row_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        App::new(Session::new("test-model", "low", None))
    }

    #[test]
    fn empty_thinking_placeholder_is_dropped_when_agent_moves_on() {
        // Spinner is driven by a Thinking(THINKING_PLACEHOLDER) block. If thinking stays
        // empty, producing text/tools must remove it so the spinner stops.
        let mut app = test_app();

        // Case 1: text follows empty thinking.
        app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
        assert!(
            app.transcript.uses_spinner(),
            "placeholder should animate while present"
        );
        app.append_or_update_text("here is the answer");
        assert!(
            !matches!(
                app.transcript.messages().last().map(|m| &m.msg),
                Some(ChatMessage::Thinking(_))
            ),
            "empty thinking must be gone once text arrives"
        );
        assert!(
            !app.transcript.uses_spinner(),
            "spinner must stop after agent moves on"
        );

        // Case 2: a tool follows empty thinking.
        let mut app = test_app();
        app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
        app.on_tool_use_start("t1".to_string(), "bash".to_string());
        let thinking_count = app
            .transcript
            .messages()
            .iter()
            .filter(|m| matches!(m.msg, ChatMessage::Thinking(_)))
            .count();
        assert_eq!(
            thinking_count, 0,
            "empty thinking must be gone once a tool runs"
        );

        // Case 3: real thinking content is preserved.
        let mut app = test_app();

        app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
        app.append_or_update_thinking("actually reasoning");
        app.append_or_update_text("done");
        assert!(
            app.transcript
                .messages()
                .iter()
                .any(|m| matches!(&m.msg, ChatMessage::Thinking(t) if t == "actually reasoning")),
            "non-empty thinking must survive and not keep the … prefix"
        );
    }

    #[test]
    fn parallel_tool_results_pair_with_their_inputs() {
        let mut app = test_app();
        // Model fans out four tool calls — all inputs arrive first.
        for id in ["t1", "t2", "t3", "t4"] {
            app.on_tool_use_finalized(id.to_string(), "bash".to_string(), "{}".to_string());
        }
        // Results return out of completion order; each must slot under its input.
        for id in ["t2", "t1", "t4", "t3"] {
            app.on_tool_result(id.to_string(), format!("out-{id}"));
        }
        let seq: Vec<(String, bool)> = app
            .transcript
            .messages()
            .iter()
            .filter_map(|m| match &m.msg {
                ChatMessage::ToolUse { tool_id, .. } => Some((tool_id.clone(), false)),
                ChatMessage::ToolResult { tool_id, .. } => Some((tool_id.clone(), true)),
                _ => None,
            })
            .collect();
        let expected = vec![
            ("t1".to_string(), false),
            ("t1".to_string(), true),
            ("t2".to_string(), false),
            ("t2".to_string(), true),
            ("t3".to_string(), false),
            ("t3".to_string(), true),
            ("t4".to_string(), false),
            ("t4".to_string(), true),
        ];
        assert_eq!(
            seq, expected,
            "parallel tool calls must render as input→output pairs"
        );
    }

    #[test]
    fn tool_block_has_gutter_and_background() {
        let mut app = test_app();
        app.on_tool_use_finalized("t1".to_string(), "bash".to_string(), "{}".to_string());
        app.on_tool_result("t1".to_string(), "hello\nworld".to_string());
        let lines = app.render_lines(80);
        // No borders / shade glyphs.
        for l in &lines {
            let s: String = l.spans.iter().map(|sp| sp.content.as_ref()).collect();
            assert!(
                !s.chars().any(|c| matches!(
                    c,
                    '\u{256D}'
                        | '\u{256E}'
                        | '\u{2570}'
                        | '\u{256F}'
                        | '\u{2502}'
                        | '\u{2591}'
                        | '\u{2592}'
                        | '\u{2593}'
                )),
                "no borders or shade glyphs: {s:?}"
            );
        }
        // Panel lines carry a gutter bar ▎ and a unified background.
        let panel_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains('\u{258E}')))
            .expect("a panel line with a gutter");
        assert!(
            panel_line.spans.iter().any(|s| s.style.bg.is_some()),
            "panel cells (incl. text) must share a background"
        );
        // The inset left margin stays transparent.
        if let Some(first) = panel_line.spans.first() {
            if !first.content.is_empty() && first.content.chars().all(|c| c == ' ') {
                assert!(
                    first.style.bg.is_none(),
                    "left margin must stay transparent"
                );
            }
        }
    }

    #[test]
    fn active_tool_result_is_only_latest_incomplete_result() {
        let mut app = test_app();
        app.push_msg(ChatMessage::ToolUse {
            tool_id: "call_1".to_string(),
            tool_name: "bash".to_string(),
            input: "{}".to_string(),
        });
        app.push_msg(ChatMessage::ToolResult {
            tool_id: "call_1".to_string(),
            content: "first output".to_string(),
            elapsed_ms: None,
        });
        app.push_msg(ChatMessage::ToolUse {
            tool_id: "call_2".to_string(),
            tool_name: "bash".to_string(),
            input: "{}".to_string(),
        });
        app.push_msg(ChatMessage::ToolResult {
            tool_id: "call_2".to_string(),
            content: "second output".to_string(),
            elapsed_ms: None,
        });
        // Only call_2 is still in tool_start_times (call_1 is done)
        app.transcript
            .test_set_tool_start_time(Some(app.clock.now()));
        app.transcript
            .test_insert_tool_start_time("call_2".to_string(), app.clock.now());

        assert!(
            !app.transcript.is_active_tool_result(1),
            "completed historical result (call_1, not in tool_start_times) must render done"
        );
        assert!(
            app.transcript.is_active_tool_result(3),
            "latest in-flight result (call_2, in tool_start_times) must be active"
        );

        // Bonus: with BOTH in-flight (parallel), BOTH must be active
        let mut app2 = test_app();
        app2.push_msg(ChatMessage::ToolUse {
            tool_id: "p1".to_string(),
            tool_name: "bash".to_string(),
            input: "{}".to_string(),
        });
        app2.push_msg(ChatMessage::ToolResult {
            tool_id: "p1".to_string(),
            content: "".to_string(),
            elapsed_ms: None,
        });
        app2.push_msg(ChatMessage::ToolUse {
            tool_id: "p2".to_string(),
            tool_name: "bash".to_string(),
            input: "{}".to_string(),
        });
        app2.push_msg(ChatMessage::ToolResult {
            tool_id: "p2".to_string(),
            content: "".to_string(),
            elapsed_ms: None,
        });
        app2.transcript
            .test_set_tool_start_time(Some(app2.clock.now()));
        app2.transcript
            .test_insert_tool_start_time("p1".to_string(), app2.clock.now());
        app2.transcript
            .test_insert_tool_start_time("p2".to_string(), app2.clock.now());
        assert!(
            app2.transcript.is_active_tool_result(1),
            "parallel in-flight p1 mid-vec must be active"
        );
        assert!(
            app2.transcript.is_active_tool_result(3),
            "parallel in-flight p2 last must be active"
        );
    }

    #[test]
    fn completed_latest_tool_result_is_not_active() {
        let mut app = test_app();
        app.push_msg(ChatMessage::ToolUse {
            tool_id: "call_1".to_string(),
            tool_name: "bash".to_string(),
            input: "{}".to_string(),
        });
        app.push_msg(ChatMessage::ToolResult {
            tool_id: "call_1".to_string(),
            content: "done".to_string(),
            elapsed_ms: Some(25),
        });
        app.transcript
            .test_set_tool_start_time(Some(app.clock.now()));

        assert!(!app.transcript.is_active_tool_result(1));
    }

    #[test]
    fn animation_tick_for_subagent_panel_does_not_invalidate_message_cache() {
        let mut app = test_app();
        app.push_msg(ChatMessage::System("stable transcript".to_string()));
        {
            let w = 80;
            let per_msg: Vec<MsgSlot> = (0..app.transcript.message_count())
                .map(|i| app.render_message_lines(i, w))
                .collect();
            app.transcript
                .test_set_cache_clean(LineCache::new(w, per_msg));
        }
        app.subagents.push(SubagentState {
            id: 1,
            name: "tester".to_string(),
            status: "running".to_string(),
            start_time: app.clock.now(),
            done: false,
            duration_secs: None,
            done_at: None,
        });
        app.spinner_frame = 2;

        let invalidate_messages = app.advance_animations();

        assert!(
            !invalidate_messages,
            "subagent panel spinner redraw must not rebuild message cache"
        );
        assert!(
            !app.needs_clear_for_animation_redraw(),
            "subagent-only animation must not force terminal.clear flicker"
        );
        assert!(
            app.transcript.line_cache().is_some(),
            "message cache should remain valid for panel-only animation"
        );
    }

    #[test]
    fn animation_tick_for_active_bash_result_invalidates_message_cache() {
        let mut app = test_app();
        app.push_msg(ChatMessage::ToolUse {
            tool_id: "call_1".to_string(),
            tool_name: "bash".to_string(),
            input: "{}".to_string(),
        });
        app.push_msg(ChatMessage::ToolResult {
            tool_id: "call_1".to_string(),
            content: String::new(),
            elapsed_ms: None,
        });
        app.transcript
            .test_set_tool_start_time(Some(app.clock.now()));
        app.transcript
            .test_insert_tool_start_time("call_1".to_string(), app.clock.now());
        {
            let w = 80;
            let per_msg: Vec<MsgSlot> = (0..app.transcript.message_count())
                .map(|i| app.render_message_lines(i, w))
                .collect();
            app.transcript
                .test_set_cache_clean(LineCache::new(w, per_msg));
        }
        app.spinner_frame = 2;

        let invalidate_messages = app.advance_animations();

        assert!(
            invalidate_messages,
            "active message-area bash animation must rebuild message cache"
        );
        assert!(
            !app.needs_clear_for_animation_redraw(),
            "streaming animation must not force whole-terminal clear flicker"
        );
    }

    #[test]
    fn spinner_tick_with_thinking_placeholder_marks_only_tail_dirty() {
        // After advancing the spinner while THINKING_PLACEHOLDER is present,
        // only the last message slot should be marked dirty (dirty_from == last index).
        // Earlier per_msg slots must be untouched on the next rebuild.
        let mut app = test_app();
        let w = 80;
        app.push_msg(ChatMessage::User("question".to_string()));
        app.push_msg(ChatMessage::Text("partial response".to_string()));
        app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));

        // Build full cache first (Clean = old Some + dirty_from None)
        {
            let per_msg: Vec<MsgSlot> = (0..app.transcript.message_count())
                .map(|i| app.render_message_lines(i, w))
                .collect();
            app.transcript
                .test_set_cache_clean(LineCache::new(w, per_msg));
        }
        let last = app.transcript.message_count() - 1;

        // Snapshot per_msg[0..last]
        let snapshot: Vec<Vec<String>> = app.transcript.line_cache().unwrap().per_msg[..last]
            .iter()
            .map(|slot| {
                slot.lines()
                    .iter()
                    .map(|l| {
                        l.spans
                            .iter()
                            .map(|s| s.content.as_ref())
                            .collect::<String>()
                    })
                    .collect()
            })
            .collect();

        // Advance spinner (spinner_frame % 3 == 0 triggers uses_spinner)
        app.spinner_frame = 2; // next wrapping_add gives 3, which % 3 == 0
        let needs_redraw = app.advance_animations();
        assert!(
            needs_redraw,
            "spinner must signal redraw while THINKING_PLACEHOLDER present"
        );

        // The animation tick itself doesn't call invalidate — the caller in mod.rs does.
        // We simulate that caller calling invalidate_last() (the new behaviour for slice 4).
        // But first assert dirty_from is still None (advance_animations doesn't set it).
        // Then we call invalidate_last() as the updated caller will.
        app.invalidate_last();
        assert_eq!(
            app.transcript.cache_dirty_from(),
            Some(last),
            "dirty watermark must point to tail message only"
        );

        // Simulate draw.rs incremental rebuild
        {
            let fresh: Vec<MsgSlot> = (last..app.transcript.message_count())
                .map(|i| app.render_message_lines(i, w))
                .collect();
            let mut cs = app.transcript.test_take_cache();
            if let CacheState::Dirty(ref mut cache, _) = cs {
                for (offset, rendered) in fresh.into_iter().enumerate() {
                    cache.per_msg[last + offset] = rendered;
                }
            }
            // mark clean
            if let CacheState::Dirty(cache, _) = cs {
                app.transcript.test_set_cache_clean(cache);
            }
        }

        // per_msg[0..last] must be unchanged
        let after: Vec<Vec<String>> = app.transcript.line_cache().unwrap().per_msg[..last]
            .iter()
            .map(|slot| {
                slot.lines()
                    .iter()
                    .map(|l| {
                        l.spans
                            .iter()
                            .map(|s| s.content.as_ref())
                            .collect::<String>()
                    })
                    .collect()
            })
            .collect();
        assert_eq!(
            snapshot, after,
            "earlier per_msg slots must not change on spinner tick"
        );
    }

    #[test]
    fn grouped_system_output_does_not_insert_rules_between_indented_lines() {
        let mut app = test_app();
        app.push_msg(ChatMessage::System("Extensions (1):".to_string()));
        app.push_msg(ChatMessage::System("  capture — ok".to_string()));
        app.push_msg(ChatMessage::System("    tools: speak".to_string()));

        let lines = app.render_lines(80);
        let header_idx = lines
            .iter()
            .position(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content.contains("Extensions (1):"))
            })
            .expect("header system message should render");
        let child_idx = lines
            .iter()
            .position(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content.contains("capture — ok"))
            })
            .expect("child system message should render");
        let grandchild_idx = lines
            .iter()
            .position(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content.contains("tools: speak"))
            })
            .expect("grandchild system message should render");

        // Related system messages should not have extra blank-line separators between them
        // (they flow as a continuous block).
        let has_separator = |slice: &[ratatui::text::Line]| {
            // Two consecutive blank lines indicate a separator was inserted
            slice.windows(2).any(|w| {
                let blank = |l: &ratatui::text::Line| {
                    l.spans.is_empty() || l.spans.iter().all(|s| s.content.is_empty())
                };
                blank(&w[0]) && blank(&w[1])
            })
        };
        assert!(!has_separator(&lines[header_idx + 1..child_idx]));
        assert!(!has_separator(&lines[child_idx + 1..grandchild_idx]));
    }

    #[test]
    fn unrelated_consecutive_system_messages_get_blank_line_separator() {
        let mut app = test_app();
        app.push_msg(ChatMessage::System("first".to_string()));
        app.push_msg(ChatMessage::System("second".to_string()));

        let lines = app.render_lines(80);
        let first_idx = lines
            .iter()
            .position(|line| line.spans.iter().any(|span| span.content.contains("first")))
            .expect("first system message should render");
        let second_idx = lines
            .iter()
            .position(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content.contains("second"))
            })
            .expect("second system message should render");

        let between = &lines[first_idx + 1..second_idx];
        let is_blank = |line: &ratatui::text::Line| {
            line.spans.is_empty() || line.spans.iter().all(|span| span.content.is_empty())
        };
        assert!(
            between.iter().any(is_blank),
            "expected blank line between consecutive system messages"
        );
    }

    #[test]
    fn pasted_message_display_preserves_text_typed_after_paste() {
        let mut app = test_app();
        app.input_before_paste = Some("before".to_string());
        app.pasted_char_count = "PASTED".chars().count();

        let display = app.user_display_text_for_submission("beforePASTED after");

        assert_eq!(display, "before [Pasted 6 chars] after");
    }

    // ── Parallel tool-event routing (Bug 2 regression coverage) ─────────

    fn last_tool_use(app: &App, tool_id: &str) -> Option<(String, String)> {
        app.transcript.messages().iter().find_map(|m| match &m.msg {
            ChatMessage::ToolUse {
                tool_id: tid,
                tool_name,
                input,
            } if tid == tool_id => Some((tool_name.clone(), input.clone())),
            _ => None,
        })
    }

    fn tool_use_start_partial(app: &App, tool_id: &str) -> Option<String> {
        app.transcript.messages().iter().find_map(|m| match &m.msg {
            ChatMessage::ToolUseStart {
                tool_id: tid,
                partial_input,
                ..
            } if tid == tool_id => Some(partial_input.clone()),
            _ => None,
        })
    }

    fn tool_result_content(app: &App, tool_id: &str) -> Option<String> {
        app.transcript.messages().iter().find_map(|m| match &m.msg {
            ChatMessage::ToolResult {
                tool_id: tid,
                content,
                ..
            } if tid == tool_id => Some(content.clone()),
            _ => None,
        })
    }

    #[test]
    fn parallel_tool_use_deltas_are_routed_by_tool_id() {
        // Regression: deltas from two interleaved parallel tool calls
        // must each land on their own ToolUseStart block. The pre-fix
        // behavior always appended to the most recent ToolUseStart,
        // corrupting the second tool's input with the first's deltas.
        let mut app = test_app();
        app.on_tool_use_start("call_a".to_string(), "bash".to_string());
        app.on_tool_use_start("call_b".to_string(), "read".to_string());

        // Codex sends deltas in interleaved order — exercise that.
        app.on_tool_use_delta("call_b", "{\"path\":");
        app.on_tool_use_delta("call_a", "{\"command\":");
        app.on_tool_use_delta("call_a", "\"ls\"}");
        app.on_tool_use_delta("call_b", "\"a\"}");

        assert_eq!(
            tool_use_start_partial(&app, "call_a").as_deref(),
            Some(r#"{"command":"ls"}"#),
            "call_a partial input must accumulate only call_a's deltas"
        );
        assert_eq!(
            tool_use_start_partial(&app, "call_b").as_deref(),
            Some(r#"{"path":"a"}"#),
            "call_b partial input must accumulate only call_b's deltas"
        );
    }

    #[test]
    fn parallel_tool_use_finalize_collapses_matching_start() {
        // Regression: finalize event must replace the matching
        // ToolUseStart in place, not push a new ToolUse at the end —
        // so on-screen order matches the order tools were called.
        let mut app = test_app();
        app.on_tool_use_start("call_a".to_string(), "bash".to_string());
        app.on_tool_use_start("call_b".to_string(), "read".to_string());

        app.on_tool_use_finalized(
            "call_a".to_string(),
            "bash".to_string(),
            r#"{"command":"ls"}"#.to_string(),
        );
        app.on_tool_use_finalized(
            "call_b".to_string(),
            "read".to_string(),
            r#"{"path":"a"}"#.to_string(),
        );

        // Both are now ToolUse, no lingering ToolUseStart.
        let lingering_starts = app
            .transcript
            .messages()
            .iter()
            .filter(|m| matches!(&m.msg, ChatMessage::ToolUseStart { .. }))
            .count();
        assert_eq!(
            lingering_starts, 0,
            "every ToolUseStart must collapse on finalize — leftover starts cause perpetual bash-trace animations"
        );
        assert_eq!(
            last_tool_use(&app, "call_a"),
            Some(("bash".to_string(), r#"{"command":"ls"}"#.to_string()))
        );
        assert_eq!(
            last_tool_use(&app, "call_b"),
            Some(("read".to_string(), r#"{"path":"a"}"#.to_string()))
        );

        // On-screen order matches call order (call_a appears before call_b).
        let positions: Vec<&str> = app
            .transcript
            .messages()
            .iter()
            .filter_map(|m| match &m.msg {
                ChatMessage::ToolUse { tool_id, .. } => Some(tool_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(positions, vec!["call_a", "call_b"]);
    }

    #[test]
    fn parallel_tool_results_do_not_overwrite_each_other() {
        // Regression: two ToolResult events arriving back-to-back must
        // each land on their own block by tool_id. The pre-fix path
        // pushed the first as a new message and then *overwrote* it
        // with the second — losing the first tool's output entirely.
        let mut app = test_app();
        app.on_tool_use_start("call_a".to_string(), "bash".to_string());
        app.on_tool_use_start("call_b".to_string(), "bash".to_string());
        app.on_tool_use_finalized("call_a".to_string(), "bash".to_string(), "{}".to_string());
        app.on_tool_use_finalized("call_b".to_string(), "bash".to_string(), "{}".to_string());

        app.on_tool_result("call_a".to_string(), "first output".to_string());
        app.on_tool_result("call_b".to_string(), "second output".to_string());

        assert_eq!(
            tool_result_content(&app, "call_a").as_deref(),
            Some("first output"),
            "call_a's result must survive — was overwritten by call_b in the buggy implementation"
        );
        assert_eq!(
            tool_result_content(&app, "call_b").as_deref(),
            Some("second output")
        );
    }

    #[test]
    fn tool_result_delta_streams_into_matching_block() {
        let mut app = test_app();
        app.on_tool_use_start("call_a".to_string(), "bash".to_string());
        app.on_tool_use_start("call_b".to_string(), "bash".to_string());
        app.on_tool_use_finalized("call_a".to_string(), "bash".to_string(), "{}".to_string());
        app.on_tool_use_finalized("call_b".to_string(), "bash".to_string(), "{}".to_string());

        // Interleaved deltas — must accumulate into the right block.
        app.on_tool_result_delta("call_a".to_string(), "alpha-".to_string());
        app.on_tool_result_delta("call_b".to_string(), "beta-".to_string());
        app.on_tool_result_delta("call_a".to_string(), "one".to_string());
        app.on_tool_result_delta("call_b".to_string(), "two".to_string());

        // Then finalize results.
        app.on_tool_result("call_a".to_string(), "alpha-one".to_string());
        app.on_tool_result("call_b".to_string(), "beta-two".to_string());

        assert_eq!(
            tool_result_content(&app, "call_a").as_deref(),
            Some("alpha-one")
        );
        assert_eq!(
            tool_result_content(&app, "call_b").as_deref(),
            Some("beta-two")
        );
    }

    #[test]
    fn parallel_tool_results_record_per_tool_elapsed_time() {
        let mut app = test_app();
        app.on_tool_use_start("call_a".to_string(), "bash".to_string());
        app.on_tool_use_start("call_b".to_string(), "bash".to_string());

        // Sleep deliberately tiny so Instant::elapsed > 0 is guaranteed.
        std::thread::sleep(std::time::Duration::from_millis(2));
        app.on_tool_result("call_a".to_string(), "a".to_string());
        std::thread::sleep(std::time::Duration::from_millis(2));
        app.on_tool_result("call_b".to_string(), "b".to_string());

        let a_elapsed = app.transcript.messages().iter().find_map(|m| match &m.msg {
            ChatMessage::ToolResult {
                tool_id,
                elapsed_ms,
                ..
            } if tool_id == "call_a" => *elapsed_ms,
            _ => None,
        });
        let b_elapsed = app.transcript.messages().iter().find_map(|m| match &m.msg {
            ChatMessage::ToolResult {
                tool_id,
                elapsed_ms,
                ..
            } if tool_id == "call_b" => *elapsed_ms,
            _ => None,
        });
        assert!(
            a_elapsed.is_some(),
            "call_a must record elapsed_ms from its own start_time"
        );
        assert!(
            b_elapsed.is_some(),
            "call_b must record elapsed_ms from its own start_time"
        );
    }

    // ── FIX 9 regression tests ───────────────────────────────────────────────

    /// Test A: a REAL "…" (plain ellipsis) in model output must survive — only
    /// the sentinel THINKING_PLACEHOLDER (ellipsis + ZWSP) triggers the drop.
    #[test]
    fn real_ellipsis_thinking_content_is_not_dropped() {
        let mut app = test_app();
        // Push the sentinel placeholder (what the stream start inserts)
        app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
        // First delta is a plain "…" (model literally output ellipsis)
        app.append_or_update_thinking("…");
        // More text follows
        app.append_or_update_text("answer");
        // The Thinking block must survive with "…" content, not be dropped
        assert!(
            app.transcript
                .messages()
                .iter()
                .any(|m| matches!(&m.msg, ChatMessage::Thinking(t) if t == "…")),
            "real ellipsis thinking content must survive — sentinel and real output are distinct"
        );
    }

    /// Test B: THINKING_PLACEHOLDER stranded by a System/Notice message on top
    /// must be cleaned up by drop_empty_thinking.
    #[test]
    fn placeholder_stranded_by_system_message_is_dropped() {
        let mut app = test_app();
        app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
        // A Notice arrives on top (as happens in stream_handler::SessionEvent::Notice)
        app.push_msg(ChatMessage::System("retrying…".to_string()));
        // Now trigger the drop (e.g. text arrives / turn ends)
        app.drop_empty_thinking();
        let has_placeholder = app.transcript.messages().iter().any(|m| {
            matches!(
                &m.msg, ChatMessage::Thinking(t) if t == THINKING_PLACEHOLDER
            )
        });
        assert!(
            !has_placeholder,
            "placeholder stranded under a System message must be removed by drop_empty_thinking"
        );
    }

    /// Test C: aborting a stream must not leave a frozen placeholder spinner.
    #[test]
    fn abort_path_clears_thinking_placeholder() {
        let mut app = test_app();
        app.push_msg(ChatMessage::Thinking(THINKING_PLACEHOLDER.to_string()));
        // Simulate what the abort handler does: drop + push error
        app.drop_empty_thinking();
        app.push_msg(ChatMessage::Error("aborted".to_string()));
        let has_placeholder = app.transcript.messages().iter().any(|m| {
            matches!(
                &m.msg, ChatMessage::Thinking(t) if t == THINKING_PLACEHOLDER
            )
        });
        assert!(
            !has_placeholder,
            "abort must remove thinking placeholder so spinner doesn't freeze"
        );
    }

    #[test]
    fn render_lines_equals_concat_of_render_message_lines() {
        let mut app = test_app();
        app.push_msg(ChatMessage::User("hello world".to_string()));
        app.push_msg(ChatMessage::Thinking("some reasoning".to_string()));
        app.push_msg(ChatMessage::Text("here is the answer".to_string()));
        app.push_msg(ChatMessage::ToolUse {
            tool_id: "call_1".to_string(),
            tool_name: "bash".to_string(),
            input: r#"{"command":"ls"}"#.to_string(),
        });
        app.push_msg(ChatMessage::ToolResult {
            tool_id: "call_1".to_string(),
            content: "file1.txt\nfile2.txt".to_string(),
            elapsed_ms: Some(42),
        });

        let w = 80;
        let flat = app.render_lines(w);
        let concat: Vec<ratatui::text::Line<'static>> = (0..app.transcript.message_count())
            .flat_map(|i| {
                app.render_message_lines(i, w)
                    .lines
                    .expect("freshly rendered slot has lines")
            })
            .collect();

        // Compare by rendered string content (Line doesn't impl PartialEq for spans reliably)
        let to_str = |lines: &[ratatui::text::Line<'static>]| -> Vec<String> {
            lines
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                })
                .collect()
        };
        assert_eq!(
            to_str(&flat),
            to_str(&concat),
            "render_lines must equal concat of render_message_lines for each index"
        );
    }

    #[test]
    fn line_cache_build_produces_heights_equal_to_render_lines_prefix_sums() {
        // P11 flat-kill port of the flat oracle: the LineCache build path
        // must produce cum_heights equal to the prefix sums of the reference
        // render (render_lines, the surviving §4 oracle), and the per_msg
        // concatenation must equal the reference render line-for-line.
        // PERMANENT test (red-team angle 5): this is the standing height
        // oracle now that the flat buffer is gone.
        let mut app = test_app();
        app.push_msg(ChatMessage::User("hi".to_string()));
        app.push_msg(ChatMessage::Text("hello back".to_string()));
        app.push_msg(ChatMessage::ToolUse {
            tool_id: "t1".to_string(),
            tool_name: "bash".to_string(),
            input: "{}".to_string(),
        });
        app.push_msg(ChatMessage::ToolResult {
            tool_id: "t1".to_string(),
            content: "output".to_string(),
            elapsed_ms: Some(10),
        });

        let w = 80;
        let expected_flat = app.render_lines(w);

        // Build a LineCache manually using the new struct
        let per_msg: Vec<MsgSlot> = (0..app.transcript.message_count())
            .map(|i| app.render_message_lines(i, w))
            .collect();
        let cache = LineCache::new(w, per_msg);

        let to_str = |lines: &[ratatui::text::Line<'static>]| -> Vec<String> {
            lines
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                })
                .collect()
        };
        // Height oracle: cum_heights ≡ prefix sums of the reference render.
        assert_eq!(
            cache.total_height(),
            expected_flat.len(),
            "total_height must equal the reference render's line count"
        );
        let mut acc = 0usize;
        for (i, slot) in cache.per_msg.iter().enumerate() {
            assert_eq!(
                cache.cum_heights[i], acc,
                "cum_heights[{i}] must be the prefix sum"
            );
            acc += slot.height();
        }
        // Content oracle: per_msg concatenation ≡ the reference render.
        let concat: Vec<ratatui::text::Line<'static>> = cache
            .per_msg
            .iter()
            .flat_map(|e| e.lines().iter().cloned())
            .collect();
        assert_eq!(
            to_str(&expected_flat),
            to_str(&concat),
            "per_msg concatenation must equal render_lines output"
        );
        assert!(cache.width == w);
    }

    /// Helper: builds a LineCache for an app at a given width, simulating what draw.rs does.
    fn build_cache(app: &App, width: usize) -> LineCache {
        let per_msg: Vec<MsgSlot> = (0..app.transcript.message_count())
            .map(|i| app.render_message_lines(i, width))
            .collect();
        LineCache::new(width, per_msg)
    }

    /// Helper: simulate the incremental rebuild from sync_cache (slice 3 logic,
    /// ported to CacheState in slice d).
    fn rebuild_incremental(app: &mut App, width: usize) {
        match app.transcript.test_take_cache() {
            CacheState::Dirty(mut cache, k) if cache.width == width => {
                // If per_msg length is out of sync with messages (insert/remove), re-render all from k.
                if cache.per_msg.len() != app.transcript.message_count() {
                    cache.per_msg.truncate(k);
                    for i in k..app.transcript.message_count() {
                        cache.per_msg.push(app.render_message_lines(i, width));
                    }
                } else {
                    // In-place partial re-render: only messages[k..]
                    for i in k..app.transcript.message_count() {
                        cache.per_msg[i] = app.render_message_lines(i, width);
                    }
                }
                // cum_heights is recomputed by test_set_cache_clean below.
                app.transcript.test_set_cache_clean(cache);
            }
            CacheState::Clean(cache) if cache.width == width => {
                // Clean at the right width — nothing to do (old dirty_from == None path).
                app.transcript.test_set_cache_clean(cache);
            }
            _ => {
                let c = build_cache(app, width);
                app.transcript.test_set_cache_clean(c);
            }
        }
    }

    #[test]
    fn incremental_cache_does_not_re_render_unchanged_messages() {
        let mut app = test_app();
        let w = 80;
        // Build fixture: User + Thinking + Text (streaming last)
        app.push_msg(ChatMessage::User("hello".to_string()));
        app.push_msg(ChatMessage::Thinking("reasoning".to_string()));
        app.push_msg(ChatMessage::Text("partial answer".to_string()));

        // Full build
        app.transcript.test_set_cache_clean(build_cache(&app, w));

        let last = app.transcript.message_count() - 1; // index of Text message

        // Snapshot per_msg[0..last] content strings (before update)
        let snapshot: Vec<Vec<String>> = app.transcript.line_cache().unwrap().per_msg[..last]
            .iter()
            .map(|slot| {
                slot.lines()
                    .iter()
                    .map(|l| {
                        l.spans
                            .iter()
                            .map(|s| s.content.as_ref())
                            .collect::<String>()
                    })
                    .collect()
            })
            .collect();

        // Simulate append_or_update_text delta (modifies last message, marks tail dirty)
        if let Some(crate::tui::app::TimestampedMsg {
            msg: ChatMessage::Text(ref mut t),
            ..
        }) = app.transcript.test_last_msg_mut()
        {
            t.push_str(" — more content appended");
        }
        app.transcript.invalidate_from(last); // invalidate_last equivalent (direct store call)

        // Incremental rebuild
        rebuild_incremental(&mut app, w);

        let cache = app.transcript.line_cache().unwrap();

        // per_msg[0..last] must be unchanged (content-equal to snapshot)
        let after: Vec<Vec<String>> = cache.per_msg[..last]
            .iter()
            .map(|slot| {
                slot.lines()
                    .iter()
                    .map(|l| {
                        l.spans
                            .iter()
                            .map(|s| s.content.as_ref())
                            .collect::<String>()
                    })
                    .collect()
            })
            .collect();
        assert_eq!(
            snapshot, after,
            "per_msg[0..last] must not change on tail-only invalidation"
        );

        // per_msg[last] must have changed (the text grew)
        let last_strs: Vec<String> = cache.per_msg[last]
            .lines()
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let contains_new = last_strs
            .iter()
            .any(|s| s.contains("more content appended"));
        assert!(contains_new, "per_msg[last] must reflect the updated text");

        // Concatenated slots + cum_heights must equal the reference render
        // (the surviving §4 oracle — flat is gone).
        let expected_flat = app.render_lines(w);
        let to_str = |lines: &[ratatui::text::Line<'static>]| -> Vec<String> {
            lines
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                })
                .collect()
        };
        let concat: Vec<ratatui::text::Line<'static>> = cache
            .per_msg
            .iter()
            .flat_map(|e| e.lines().iter().cloned())
            .collect();
        assert_eq!(
            to_str(&expected_flat),
            to_str(&concat),
            "per_msg concatenation must equal full render_lines after incremental rebuild"
        );
        assert_eq!(
            cache.total_height(),
            expected_flat.len(),
            "cum_heights total must track the reference render after incremental rebuild"
        );
    }

    #[test]
    fn incremental_cache_handles_tool_result_insert() {
        let mut app = test_app();
        let w = 80;
        app.push_msg(ChatMessage::User("run something".to_string()));
        app.push_msg(ChatMessage::ToolUse {
            tool_id: "t1".to_string(),
            tool_name: "bash".to_string(),
            input: r#"{"command":"ls"}"#.to_string(),
        });

        app.transcript.test_set_cache_clean(build_cache(&app, w));

        // Insert a ToolResult after ToolUse (as push_tool_result does)
        let at = 2;
        app.transcript.test_insert_at(
            at,
            crate::tui::app::TimestampedMsg {
                msg: ChatMessage::ToolResult {
                    tool_id: "t1".to_string(),
                    content: "file.txt".to_string(),
                    elapsed_ms: Some(5),
                },
                time: "00:00".to_string(),
            },
        );
        app.transcript.invalidate_from(at);

        // Rebuild: since per_msg.len() (2) != messages.len() (3), should do full-from-k rebuild
        rebuild_incremental(&mut app, w);

        let cache = app.transcript.line_cache().unwrap();
        assert_eq!(
            cache.per_msg.len(),
            app.transcript.message_count(),
            "per_msg must track messages after insert"
        );

        let expected_flat = app.render_lines(w);
        let to_str = |lines: &[ratatui::text::Line<'static>]| -> Vec<String> {
            lines
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                })
                .collect()
        };
        let concat: Vec<ratatui::text::Line<'static>> = cache
            .per_msg
            .iter()
            .flat_map(|e| e.lines().iter().cloned())
            .collect();
        assert_eq!(
            to_str(&expected_flat),
            to_str(&concat),
            "per_msg concatenation must equal render_lines after insert + incremental rebuild"
        );
        assert_eq!(
            cache.total_height(),
            expected_flat.len(),
            "cum_heights total must track the reference render after insert + incremental rebuild"
        );
    }

    // ── flat_cursor_pos (hybrid plan §3.1 helper) ──────────────────────────

    fn lines_of(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flat_cursor_empty_buffer() {
        assert_eq!(flat_cursor_pos(&lines_of(&[""]), (0, 0)), 0);
        // Truly empty slice must not panic either.
        assert_eq!(flat_cursor_pos(&[], (0, 0)), 0);
    }

    #[test]
    fn flat_cursor_single_line() {
        let lines = lines_of(&["hello"]);
        assert_eq!(flat_cursor_pos(&lines, (0, 0)), 0);
        assert_eq!(flat_cursor_pos(&lines, (0, 3)), 3);
        assert_eq!(flat_cursor_pos(&lines, (0, 5)), 5); // at end
    }

    #[test]
    fn flat_cursor_multi_line() {
        // "ab\ncde\nf" — flat chars: a=0 b=1 \n=2 c=3 d=4 e=5 \n=6 f=7
        let lines = lines_of(&["ab", "cde", "f"]);
        assert_eq!(flat_cursor_pos(&lines, (0, 2)), 2); // end of line 0
        assert_eq!(flat_cursor_pos(&lines, (1, 0)), 3); // just after first \n
        assert_eq!(flat_cursor_pos(&lines, (1, 2)), 5);
        assert_eq!(flat_cursor_pos(&lines, (2, 0)), 7); // after second \n
        assert_eq!(flat_cursor_pos(&lines, (2, 1)), 8); // end of buffer
    }

    #[test]
    fn flat_cursor_wide_chars() {
        // Char-wise, not width-wise: CJK and emoji count 1 each.
        let lines = lines_of(&["日本語", "a👍b"]);
        assert_eq!(flat_cursor_pos(&lines, (0, 2)), 2);
        assert_eq!(flat_cursor_pos(&lines, (0, 3)), 3); // end of CJK line
        assert_eq!(flat_cursor_pos(&lines, (1, 0)), 4);
        assert_eq!(flat_cursor_pos(&lines, (1, 2)), 6); // after the emoji
        assert_eq!(flat_cursor_pos(&lines, (1, 3)), 7); // end
    }

    #[test]
    fn flat_cursor_clamps_out_of_range_col() {
        let lines = lines_of(&["ab", "cd"]);
        assert_eq!(flat_cursor_pos(&lines, (0, 99)), 2);
        assert_eq!(flat_cursor_pos(&lines, (1, 99)), 5);
    }

    #[test]
    fn accessors_round_trip() {
        let mut app = test_app();
        assert!(app.input_is_empty());
        assert_eq!(app.cursor_char_pos(), 0);

        app.set_input_text("hello\nworld");
        assert_eq!(app.input_text(), "hello\nworld");
        assert!(!app.input_is_empty());
        // set_input_text puts the cursor at the very end.
        assert_eq!(app.cursor_char_pos(), 11);
        assert_eq!(app.input_first_line(), "hello");

        app.insert_at_cursor("!");
        assert_eq!(app.input_text(), "hello\nworld!");
        assert_eq!(app.cursor_char_pos(), 12);

        app.clear_input();
        assert!(app.input_is_empty());
        assert_eq!(app.input_text(), "");
        assert_eq!(app.cursor_char_pos(), 0);
        assert_eq!(app.input_first_line(), "");
    }

    #[test]
    fn editor_cursor_matches_flat_pos_after_typing() {
        let mut app = test_app();
        app.set_input_text("日本\nab");
        // cursor at end: row 1 col 2 → flat 3 (line0) + 2
        assert_eq!(app.editor.cursor(), (1, 2));
        assert_eq!(app.cursor_char_pos(), 5);
    }

    // ---- live-MXC lifecycle (receive-side guard, last-good cache) ----

    /// A never-completing stand-in for the subscriber: `myx_task.is_some()`
    /// is the receive-side guard bit; the task body is irrelevant.
    fn dummy_subscriber() -> tokio::task::JoinHandle<()> {
        tokio::spawn(std::future::pending::<()>())
    }

    fn live_sentinel() -> super::super::theme::Theme {
        super::super::theme::Theme {
            bg: ratatui::style::Color::Rgb(1, 2, 3),
            ..Default::default()
        }
    }

    /// Receive-side guard (shady F1): `UnboundedSender::send` is synchronous,
    /// so a palette queued before `/theme nord` + `abort()` still arrives.
    /// With `myx_task` gone the arm must drop it — the cache staying empty
    /// is the deterministic proxy (the animated-apply duration depends on
    /// the user's `theme_transition` knob, so we don't assert on the slot).
    #[test]
    fn stale_myx_palette_after_switch_away_is_dropped() {
        let mut app = test_app();
        assert!(app.myx_task.is_none());
        super::super::loop_arms::handle_myx_theme_arm(&mut app, (live_sentinel(), Some(600)));
        assert!(
            app.myx_last_live.is_none(),
            "guarded arm must drop the stale palette before caching or applying"
        );
    }

    /// Preview shield (shady F4): while a settings theme preview is browsing,
    /// a live palette is CACHED but not applied — the preview owns the screen.
    #[tokio::test]
    async fn live_palette_is_cached_but_not_applied_during_settings_preview() {
        let mut app = test_app();
        app.myx_task = Some(dummy_subscriber());
        let mut st = super::super::settings::SettingsState::new();
        st.original_theme_name = Some("myx".to_string());
        app.settings = Some(st);

        let live = live_sentinel();
        super::super::loop_arms::handle_myx_theme_arm(&mut app, (live.clone(), None));

        assert_eq!(
            app.myx_last_live.as_ref(),
            Some(&live),
            "must cache as last-good"
        );
        assert!(
            app.theme_transition.is_none(),
            "preview shield must skip the apply entirely"
        );
        app.myx_task.take().unwrap().abort();
    }

    /// Last-good cache (shady F3): re-applying "myx" over a RUNNING
    /// subscriber resolves to the cached live palette; without a subscriber
    /// (or for any other theme) it resolves exactly like the static loader.
    #[tokio::test]
    async fn resolve_apply_theme_prefers_live_cache_only_while_subscriber_runs() {
        let mut app = test_app();
        let live = live_sentinel();
        app.myx_last_live = Some(live.clone());

        // No running subscriber → static resolution, cache ignored.
        assert_eq!(
            app.resolve_apply_theme("myx"),
            super::super::theme::load_theme_by_name("myx")
        );

        app.myx_task = Some(dummy_subscriber());
        assert_eq!(app.resolve_apply_theme("myx"), Some(live));
        // Other themes never see the cache.
        assert_eq!(
            app.resolve_apply_theme("nord"),
            super::super::theme::load_theme_by_name("nord")
        );
        app.myx_task.take().unwrap().abort();
    }

    /// Settings-path lifecycle (shady F2 / okarin F1): committing a theme
    /// through the settings modal must reconcile the subscriber exactly like
    /// `/theme` does — spawn on "myx", abort + clear cache on switch-away.
    #[tokio::test]
    async fn settings_theme_commit_reconciles_the_subscriber() {
        let mut app = test_app();
        app.apply_theme_from_settings("myx");
        assert!(
            app.myx_task.is_some(),
            "settings apply of myx must start the live layer"
        );

        app.myx_last_live = Some(live_sentinel());
        app.apply_theme_from_settings("nord");
        assert!(
            app.myx_task.is_none(),
            "settings apply away from myx must stop the subscriber"
        );
        assert!(
            app.myx_last_live.is_none(),
            "the last-good cache must not survive switch-away"
        );
    }
}
