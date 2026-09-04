//! Headless TUI test harness (P4).
//!
//! Drives the chat UI without a terminal by replicating the tri-loop the real
//! `run()` performs, minus the tokio `select!` scaffolding:
//!
//! 1. **event** — synthetic [`crossterm::event::Event`]s go through the exact
//!    same dispatch surface production uses: [`super::input::handle_event`].
//! 2. **snapshot** — [`super::draw::build_render_model`] materializes the
//!    owned, `Send`-safe [`super::render_model::RenderModel`], applying the
//!    same line-cache maintenance the main loop applies.
//! 3. **render** — [`super::draw::render_frame_into`] draws that snapshot into
//!    a [`ratatui::backend::TestBackend`] buffer — the same frame body the
//!    render thread executes against the real terminal, minus the
//!    crossterm-specific edge scrub.
//!
//! No alternate screen, no raw mode, no render thread, no TTY.
//!
//! # Scope and limitations
//!
//! - **Sync scenarios + the bounded slash drive (P6.3).** [`InputAction`]s
//!   that the real event loop resolves asynchronously (message submission →
//!   streaming, plugin outcomes, …) are *recorded* — see
//!   [`TestHarness::take_actions`] — but not executed. The exception is the
//!   SYNC-SAFE slash-command subset: [`TestHarness::drive_slash_commands`]
//!   runs recorded slash commands through the production
//!   `commands::handle_command` / `commands::execute_command_action` path on
//!   a private current-thread tokio runtime, HARD-BOUNDED by
//!   [`SLASH_DRIVE_TIMEOUT`] — the drive fails loudly, it never hangs.
//!   Streaming/engine command outcomes stay P4-style recorded-not-executed.
//! - The [`Runtime`] inside the harness is [`Runtime::new_headless`]: stub
//!   credentials, no network, no reaper task. UI state that merely *reads*
//!   the runtime (model name, thinking level) renders normally.
//! - `App::new` reads the user config for `agent_name`; the harness overrides
//!   it to a fixed value afterwards so frames are machine-independent. It
//!   never writes to `~/.synaps-cli/`.
//!
//! # Example
//!
//! ```rust,no_run
//! use agent_tui::tui::testing::TestHarness;
//! use crossterm::event::{KeyCode, KeyModifiers};
//!
//! let mut h = TestHarness::boot(); // 80x24
//! h.type_str("hello world")
//!     .key(KeyCode::Left, KeyModifiers::empty());
//!
//! let frame = h.snapshot();
//! assert!(frame.contains("hello world"));
//!
//! // Buffer-level assertions:
//! let buf = h.render();
//! assert_eq!(buf.area().width, 80);
//! ```

use std::sync::Arc;

/// P6.4 — replayable interaction tapes. Lives in a sibling file
/// (`testing/tape.rs`) but is a child module of `testing`, so it reaches the
/// harness's driver surface directly.
#[path = "testing/tape.rs"]
pub mod tape;
/// A3/A5 — the in-memory `ClientTransport` the harness drives commands
/// through (`Set`/`EngineCommand`/`Query` answered from a headless
/// `Runtime`; envelopes fed by tests).
#[path = "testing/scripted.rs"]
pub mod scripted;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::buffer::Buffer;
use ratatui::layout::{Rect, Size};
use ratatui::{Terminal, TerminalOptions, Viewport};

use synaps_cli::skills::keybinds::KeybindRegistry;
use synaps_cli::skills::registry::CommandRegistry;
use synaps_cli::skills::BUILTIN_COMMANDS;
use synaps_cli::{Runtime, Session};

use super::app::{App, ChatMessage};
use super::session_link::{PromptBridge, SessionLink};
use agent_engine::session::SessionEventWire;
use super::draw::{build_render_model, render_frame_into};
use super::input::{self, InputAction};

/// A headless, deterministic driver for the chat UI.
///
/// Owns the [`App`], a stub [`Runtime`], and a [`TestBackend`] terminal.
/// See the [module docs](self) for the loop it replicates and its limits.
pub struct TestHarness {
    app: App,
    /// The session behind a [`scripted::ScriptedTransport`] (+ the published
    /// `RuntimeView` every getter reads).
    link: SessionLink,
    /// Feeds the secret-prompt pane exactly like production (`Prompt` envelopes).
    prompt_bridge: PromptBridge,
    scripted_log: std::sync::Arc<std::sync::Mutex<scripted::ScriptedLog>>,
    secret_prompt_rx: std::sync::Arc<
        std::sync::Mutex<
            tokio::sync::mpsc::UnboundedReceiver<synaps_cli::tools::SecretPromptRequest>,
        >,
    >,
    registry: Arc<CommandRegistry>,
    keybinds: KeybindRegistry,
    terminal: Terminal<TestBackend>,
    size: Size,
    /// Human-readable records of dispatched [`InputAction`]s the harness
    /// cannot execute synchronously (submissions, slash commands, …).
    actions: Vec<String>,
    /// Structured queue of dispatched slash commands awaiting the P6.3
    /// bounded async drive ([`Self::drive_slash_commands`]).
    pending_slash: Vec<(String, String)>,
    quit_requested: bool,
}

/// Hard upper bound on any single bounded async drive (P6.3). A slash
/// command that has not resolved within this budget panics the test —
/// deterministic failure instead of a hung harness.
pub const SLASH_DRIVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// P6.3 bounded executor: run one future to completion on a throwaway
/// current-thread tokio runtime, capped by [`SLASH_DRIVE_TIMEOUT`].
///
/// Invariants:
/// - **Never hangs.** `tokio::time::timeout` is the hard bound; on expiry we
///   panic with `what` so the failure names the offending command.
/// - **Drain-to-idle.** `block_on` returns only when the future is `Ready`;
///   there are no detached tasks — the sync-safe subset spawns nothing.
/// - Must be called from a NON-async test (`#[test]`, not `#[tokio::test]`):
///   nesting runtimes panics immediately (deterministic, not a hang).
fn block_on_bounded<F: std::future::Future>(what: &str, fut: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread tokio runtime construction is infallible");
    rt.block_on(async move {
        match tokio::time::timeout(SLASH_DRIVE_TIMEOUT, fut).await {
            Ok(out) => out,
            Err(_) => panic!(
                "bounded async drive timed out after {SLASH_DRIVE_TIMEOUT:?}: {what} — \
                 this command is not in the sync-safe subset; keep it P4-style recorded"
            ),
        }
    })
}

impl TestHarness {
    /// Boot a headless App at the default 80x24 geometry.
    pub fn boot() -> Self {
        Self::boot_with_size(80, 24)
    }

    /// Boot a headless App at an explicit geometry.
    pub fn boot_with_size(cols: u16, rows: u16) -> Self {
        let session = Session::new(synaps_cli::models::default_model(), "medium", None);
        let mut app = App::new_with_clock(session, super::clock::TuiClock::test());
        // Determinism: `App::new` resolves agent_name from the user config —
        // pin it so snapshots don't vary per machine.
        app.agent_name = "agent".to_string();
        // Skip the boot animation; a fixed frame beats a time-parametric one.
        app.logo_build_t = None;

        let backend = TestBackend::new(cols, rows);
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fullscreen,
            },
        )
        .expect("TestBackend terminal construction is infallible");

        let transport = scripted::ScriptedTransport::new(Runtime::new_headless());
        let scripted_log = std::sync::Arc::clone(&transport.log);
        let (secret_prompt_tx, secret_prompt_rx) = tokio::sync::mpsc::unbounded_channel();
        TestHarness {
            app,
            link: SessionLink::new(Box::new(transport)),
            prompt_bridge: PromptBridge::new(secret_prompt_tx),
            scripted_log,
            secret_prompt_rx: std::sync::Arc::new(std::sync::Mutex::new(secret_prompt_rx)),
            registry: Arc::new(CommandRegistry::new(BUILTIN_COMMANDS, Vec::new())),
            keybinds: KeybindRegistry::new(),
            terminal,
            size: Size::new(cols, rows),
            actions: Vec::new(),
            pending_slash: Vec::new(),
            quit_requested: false,
        }
    }

    // ── Event injection ──────────────────────────────────────────────────────

    /// Send a synthetic key press through the same dispatch surface `run()` uses.
    pub fn key(&mut self, code: KeyCode, mods: KeyModifiers) -> &mut Self {
        self.event(Event::Key(KeyEvent::new(code, mods)))
    }

    /// Type a string as a sequence of plain character key presses.
    pub fn type_str(&mut self, text: &str) -> &mut Self {
        for ch in text.chars() {
            self.key(KeyCode::Char(ch), KeyModifiers::empty());
        }
        self
    }

    /// Send a bracketed-paste event.
    pub fn paste(&mut self, text: &str) -> &mut Self {
        self.event(Event::Paste(text.to_string()))
    }

    /// Send a synthetic mouse event.
    pub fn mouse(&mut self, event: MouseEvent) -> &mut Self {
        self.event(Event::Mouse(event))
    }

    /// Resize the virtual terminal and notify the app, mirroring what the
    /// real loop sees when the terminal window changes size.
    pub fn resize(&mut self, cols: u16, rows: u16) -> &mut Self {
        self.size = Size::new(cols, rows);
        self.terminal.backend_mut().resize(cols, rows);
        self.event(Event::Resize(cols, rows))
    }

    /// Escape hatch: send a raw [`Event`] through the dispatch surface.
    pub fn event(&mut self, event: Event) -> &mut Self {
        let streaming = self.app.streaming;
        let action = input::handle_event(
            event,
            &mut self.app,
            &**self.link.view(),
            streaming,
            &self.registry,
            &self.keybinds,
            // Production reads config.scroll_lines.unwrap_or(3); the harness
            // uses the same default. Scroll-step-sensitive tests can drive the
            // mouse path (hardcoded 3 lines) directly instead.
            3,
        );
        self.record(action);
        self
    }

    // ── Render / assertion surface ───────────────────────────────────────────

    /// Advance to steady state and render one frame into the [`TestBackend`].
    ///
    /// Sync equivalent of the main loop's publish step: materialize the
    /// [`super::render_model::RenderModel`] snapshot (including line-cache
    /// maintenance), then draw it with the production frame body. Returns
    /// the rendered [`Buffer`] for cell-level assertions.
    pub fn render(&mut self) -> &Buffer {
        let (model, patch) = build_render_model(
            &mut super::view_model::ViewInputs::from_app(&mut self.app),
            &**self.link.view(),
            &self.registry,
            self.size,
        )
        .expect("build_render_model returned None — gamba never runs headless");
        // Mirror the main loop: apply the builder's patch to authoritative
        // App state so modal geometry persists across harness frames.
        patch.apply(&mut self.app);

        // Effects are render-thread state; the harness renders effect-free,
        // deterministic frames. Duration::ZERO keeps any effect math inert.
        let (mut boot_fx, mut exit_fx) = (None, None);
        self.terminal
            .draw(|frame| {
                render_frame_into(
                    frame,
                    &model,
                    &mut boot_fx,
                    &mut exit_fx,
                    std::time::Duration::ZERO,
                )
            })
            .expect("TestBackend draw is infallible");

        self.terminal.backend().buffer()
    }

    /// Render one frame through `CrosstermBackend<Vec<u8>>` and return the
    /// raw ANSI byte stream (P5 spike).
    ///
    /// Unlike [`render`](Self::render) — which yields a ratatui cell grid
    /// that never touches the escape layer — this drives the *production
    /// backend* with an in-memory `Write` sink, so the returned bytes are
    /// the real escape sequences crossterm would send to the terminal.
    /// Feed them to a `vt100::Parser` and assert on the parsed screen.
    ///
    /// Scope: **frame content only.** The render thread's edge scrub and
    /// the lifecycle enter/leave sequences do not pass through here — see
    /// `tests/vt100_spike.rs` for the full scoping note.
    pub fn render_ansi(&mut self) -> Vec<u8> {
        let (model, patch) = build_render_model(
            &mut super::view_model::ViewInputs::from_app(&mut self.app),
            &**self.link.view(),
            &self.registry,
            self.size,
        )
        .expect("build_render_model returned None — gamba never runs headless");
        // Mirror the main loop: apply the builder's patch to authoritative
        // App state so modal geometry persists across harness frames.
        patch.apply(&mut self.app);

        // Shared in-memory sink: `CrosstermBackend::writer_mut` is unstable
        // in ratatui 0.30 (`backend-writer` feature), so instead of taking
        // the Vec back out of the backend we hand it a cloneable handle and
        // read the bytes through our copy after the draw.
        let sink = SharedSink::default();

        // Fixed viewport: `CrosstermBackend::size()` queries the actual TTY
        // (crossterm::terminal::size()), which fails headless. A fixed
        // viewport of the harness geometry sidesteps the query entirely.
        let area = Rect::new(0, 0, self.size.width, self.size.height);
        let mut terminal = Terminal::with_options(
            CrosstermBackend::new(sink.clone()),
            TerminalOptions {
                viewport: Viewport::Fixed(area),
            },
        )
        .expect("in-memory CrosstermBackend terminal construction is infallible");

        let (mut boot_fx, mut exit_fx) = (None, None);
        terminal
            .draw(|frame| {
                render_frame_into(
                    frame,
                    &model,
                    &mut boot_fx,
                    &mut exit_fx,
                    std::time::Duration::ZERO,
                )
            })
            .expect("draw to an in-memory sink is infallible");

        sink.take()
    }

    /// Render and return the visible frame as a plain string — one line per
    /// terminal row, trailing whitespace trimmed. Suited to insta-style
    /// snapshot assertions and failure diffs.
    pub fn snapshot(&mut self) -> String {
        let buf = self.render();
        let area = *buf.area();
        let mut out = String::with_capacity((area.width as usize + 1) * area.height as usize);
        for y in area.top()..area.bottom() {
            let mut line = String::with_capacity(area.width as usize);
            for x in area.left()..area.right() {
                line.push_str(buf[(x, y)].symbol());
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    // ── State inspection ─────────────────────────────────────────────────────

    /// Current contents of the input box (flattened from the editor).
    pub fn input_contents(&self) -> String {
        self.app.input_text()
    }

    /// Whether a `Quit` action was dispatched (the real loop would start the
    /// exit animation and tear down).
    pub fn quit_requested(&self) -> bool {
        self.quit_requested
    }

    /// Drain the human-readable records of dispatched actions the harness
    /// could not execute synchronously (e.g. `submit`, `slash:/help`).
    /// Lets tests assert *dispatch* without an engine attached.
    pub fn take_actions(&mut self) -> Vec<String> {
        std::mem::take(&mut self.actions)
    }

    /// Seed a system line into the transcript — useful for exercising
    /// scroll, selection and line-cache paths without an engine.
    pub fn push_system_message(&mut self, text: &str) -> &mut Self {
        self.app.push_msg(ChatMessage::System(text.to_string()));
        self
    }

    /// Seed a raw-markdown assistant `Text` message. Used by the P10
    /// copy-fidelity pins, which need known markdown source in the transcript.
    pub fn push_text_message(&mut self, text: &str) -> &mut Self {
        self.app.push_msg(ChatMessage::Text(text.to_string()));
        self
    }

    /// The clipboard-bound text for the current selection — the seam the P10
    /// copy pins assert against (design §5 (pre): `selected_text()` is
    /// directly callable, no clipboard mock needed).
    ///
    /// T241 Slice 5: `&mut self` because `selected_text` now promotes
    /// estimated off-screen slots on demand (promote-on-touch §4.5).
    pub fn selected_text(&mut self) -> Option<String> {
        self.app.transcript.selected_text()
    }

    /// Open the settings modal directly (bypasses the async command dispatch).
    pub fn open_settings_modal(&mut self) -> &mut Self {
        self.app.settings = Some(super::settings::SettingsState::new());
        // P7.7 HARNESS HELPER RULE: mirror production — opening a migrated modal
        // pushes onto the ModalStack, else `debug_assert_stack_sync` (run after
        // every harness `event()`) trips on the missing Settings push.
        self.app.modal_stack.push(super::focus::PaneId::Settings);
        self
    }

    /// Open the models modal directly.
    pub fn open_models_modal(&mut self) -> &mut Self {
        self.app.models = Some(super::models::ModelsModalState::new());
        self.app.modal_stack.push(super::focus::PaneId::Models);
        self
    }

    /// Open the plugins modal directly with default (empty) plugin state.
    pub fn open_plugins_modal(&mut self) -> &mut Self {
        self.app.plugins = Some(super::plugins::PluginsModalState::new(
            synaps_cli::skills::state::PluginsState::default(),
        ));
        // P7.6 HARNESS HELPER RULE: mirror production — every open of a migrated
        // modal pushes onto the ModalStack, else `debug_assert_stack_sync`
        // (run after every harness `event()`) trips on the missing Plugins push.
        self.app.modal_stack.push(super::focus::PaneId::Plugins);
        self
    }

    /// P7.8: inject and activate an async secret prompt — the harness
    /// equivalent of a tool calling `SecretPromptHandle::prompt`. Sends a
    /// request through a throwaway channel, drains it into `app.secret_prompts`
    /// via the same `poll_requests` the tick arm uses, then reconciles the modal
    /// stack exactly as production does. The response receiver is dropped: the
    /// harness asserts on UI state (buffer / stack / frame), not the tool reply.
    pub fn activate_secret_prompt(&mut self, title: &str, prompt: &str) -> &mut Self {
        use synaps_cli::tools::SecretPromptRequest;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
        tx.send(SecretPromptRequest {
            title: title.to_string(),
            prompt: prompt.to_string(),
            response_tx,
        })
        .expect("secret prompt request send is infallible on a fresh channel");
        let rx = std::sync::Arc::new(std::sync::Mutex::new(rx));
        self.app.secret_prompts.poll_requests(&rx);
        input::reconcile_secret_prompt(&mut self.app);
        self
    }

    /// Whether a secret prompt is currently active (mirrors the SecretPrompt
    /// stack membership by the P7.8 sync invariant).
    pub fn secret_prompt_active(&self) -> bool {
        self.app.secret_prompts.is_active()
    }

    /// Current modal-stack depth (0 = base Chat pane, no modals open).
    pub fn modal_stack_depth(&self) -> usize {
        self.app.modal_stack.depth()
    }

    /// Stable, machine-independent name of the pane that currently receives
    /// input (`modal_stack.top()`). `"chat"` is the base pane (empty stack).
    /// P7.9: lets integration tests assert *which* pane is top-of-stack without
    /// naming the crate-private `PaneId` type.
    pub fn top_pane_name(&self) -> &'static str {
        use super::focus::PaneId;
        match self.app.modal_stack.top() {
            PaneId::Chat => "chat",
            PaneId::HelpFind => "help-find",
            PaneId::Effort => "effort",
            PaneId::Models => "models",
            PaneId::Plugins => "plugins",
            PaneId::Settings => "settings",
            PaneId::PluginEditor => "plugin-editor",
            PaneId::SecretPrompt => "secret-prompt",
        }
    }

    /// Left/Right focus side of the open settings modal as the draw layer reads
    /// it (`settings.focus`, the synced projection of the FocusManager ring).
    /// `None` when settings is closed. P7.9 focus-traversal witness.
    pub fn settings_focus_side(&self) -> Option<&'static str> {
        self.app.settings.as_ref().map(|st| match st.focus {
            super::settings::Focus::Left => "left",
            super::settings::Focus::Right => "right",
        })
    }

    /// Left/Right focus side of the open plugins modal as the draw layer reads
    /// it (`plugins.focus`, the synced projection of the FocusManager ring).
    /// `None` when plugins is closed. P7.9 focus-traversal witness.
    pub fn plugins_focus_side(&self) -> Option<&'static str> {
        self.app.plugins.as_ref().map(|st| match st.focus {
            super::plugins::state::Focus::Left => "left",
            super::plugins::state::Focus::Right => "right",
        })
    }

    /// Open the nested plugin-custom editor ON TOP of an already-open settings
    /// modal — the harness equivalent of `InputAction::PluginEditorOpen`
    /// resolving. Sets `edit_mode = PluginCustom(..)` and pushes
    /// `PaneId::PluginEditor`, mirroring production (`mod.rs`) so
    /// `debug_assert_stack_sync` stays satisfied. No-op if settings is closed.
    pub fn open_plugin_editor(&mut self) -> &mut Self {
        use super::settings::plugin_editor::PluginEditorSession;
        use synaps_cli::extensions::settings_editor::SettingsEditorRenderParams;
        if let Some(st) = self.app.settings.as_mut() {
            let render = SettingsEditorRenderParams {
                rows: Vec::new(),
                cursor: None,
                footer: None,
            };
            st.edit_mode = Some(super::settings::ActiveEditor::PluginCustom {
                plugin_id: "demo".to_string(),
                category: "general".to_string(),
                field: "token".to_string(),
                render: PluginEditorSession {
                    plugin_id: "demo".to_string(),
                    category: "general".to_string(),
                    field: "token".to_string(),
                    render,
                },
            });
            self.app
                .modal_stack
                .push(super::focus::PaneId::PluginEditor);
        }
        self
    }

    /// Whether the nested PluginCustom editor is currently active — mirrors the
    /// `PaneId::PluginEditor` stack membership by the P7.7 sync invariant.
    pub fn plugin_editor_active(&self) -> bool {
        matches!(
            self.app.settings.as_ref().map(|st| &st.edit_mode),
            Some(Some(super::settings::ActiveEditor::PluginCustom { .. }))
        )
    }

    /// Current transcript scrollback offset (0 = pinned to bottom).
    pub fn scroll_back(&self) -> u16 {
        self.app.transcript.scroll_back_pos()
    }

    /// Whether the transcript is pinned to the latest message.
    pub fn scroll_pinned(&self) -> bool {
        self.app.transcript.is_pinned()
    }

    // ── P11 perf-pin surface (design §5.2 / lock L4) ─────────────────────────

    /// Message renders since the last [`Self::reset_perf_probe`] — one count
    /// per `render_message_lines` call (measure == render under P11).
    pub fn render_count(&self) -> usize {
        self.app.transcript.probe_render_count()
    }

    /// Cumulative-offset entries written since the last
    /// [`Self::reset_perf_probe`]. Zero on a Clean frame is the lock-L4
    /// "cum-height lookup is cached, no O(n) re-sum per frame" invariant.
    pub fn cum_height_writes(&self) -> usize {
        self.app.transcript.probe_cum_write_count()
    }

    /// Zero the perf counters — call after warm-up, before the measured frame.
    pub fn reset_perf_probe(&self) {
        self.app.transcript.probe_reset()
    }

    /// Zero the highlight counters (`HIGHLIGHT_CALLS` + `SYNTAX_SET_TOUCHED`).
    /// Call before the frame under measurement to get a clean read.
    pub fn reset_highlight_probe(&self) {
        super::highlight::highlight_reset_counters();
    }

    /// Read `HIGHLIGHT_CALLS` — syntect highlight sessions triggered since last
    /// `reset_highlight_probe`.
    pub fn highlight_call_count(&self) -> usize {
        super::highlight::highlight_call_count()
    }

    /// Read `SYNTAX_SET_TOUCHED` — whether the `SYNTAX_SET` LazyLock has been
    /// initialized (forced-touch) since the last `reset_highlight_probe`.
    pub fn syntax_set_was_touched(&self) -> bool {
        super::highlight::syntax_set_was_touched()
    }

    /// Begin a streaming tool call — drives the store's real
    /// `on_tool_use_start` routing, exactly as the stream handler does.
    pub fn tool_use_start(&mut self, tool_id: &str, tool_name: &str) -> &mut Self {
        self.app
            .on_tool_use_start(tool_id.to_string(), tool_name.to_string());
        self
    }

    /// Stream a tool-input delta into the matching `ToolUseStart` block —
    /// the P11 perf pins measure the render cost of exactly this path.
    pub fn tool_use_delta(&mut self, tool_id: &str, delta: &str) -> &mut Self {
        self.app.on_tool_use_delta(tool_id, delta);
        self
    }

    // ── Deterministic clock / toast control (P6.2) ──────────────────────────

    /// Advance the injectable [`TuiClock`] by `ms` milliseconds. Under the
    /// harness the clock is frozen at boot, so time-dependent state (toast
    /// expiry, tool timers) only moves when the test calls this.
    pub fn advance_clock_ms(&mut self, ms: u64) -> &mut Self {
        self.app.clock.advance(std::time::Duration::from_millis(ms));
        self
    }

    /// Publish a toast with an explicit TTL (seconds) through the same
    /// provider the app uses. Expiry is governed by the frozen clock.
    pub fn push_toast_with_ttl_secs(&mut self, id: &str, text: &str, ttl_secs: u64) -> &mut Self {
        let toast =
            super::toast::Toast::new(id, text).ttl(Some(std::time::Duration::from_secs(ttl_secs)));
        self.app.toasts.upsert(toast);
        self
    }

    /// Publish a toast anchored dead-CENTER (overlapping the secret-prompt box's
    /// centered draw rect). Used by the P7.9 toast-vs-prompt z-order pin: the
    /// prompt is drawn AFTER toasts and issues a `Clear`, so a CENTER toast must
    /// end up painted *under* the prompt. Long TTL under the frozen boot clock.
    pub fn push_center_toast(&mut self, id: &str, text: &str) -> &mut Self {
        let toast = super::toast::Toast::new(id, text)
            .at(super::toast::ToastPosition::CENTER)
            .ttl(Some(std::time::Duration::from_secs(3600)));
        self.app.toasts.upsert(toast);
        self
    }

    /// Run one toast expiry sweep against the current clock time. Returns
    /// `true` if any toast was reaped.
    pub fn tick_toasts(&mut self) -> bool {
        self.app.toasts.tick()
    }

    /// Number of currently-live toasts.
    pub fn toast_count(&self) -> usize {
        self.app.toasts.visible().count()
    }

    /// Force the app's streaming flag — lets scenarios exercise the
    /// streaming-input command refusal path without a live engine stream.
    pub fn set_streaming(&mut self, streaming: bool) -> &mut Self {
        self.app.streaming = streaming;
        self
    }

    // ── Session envelopes (PLAN-phase3 §5.1 layer 2) ─────────────────────────

    /// Feed session events through the PRODUCTION presentation arm
    /// (`stream_handler::handle_session_event_arm`) — the same code path the
    /// `run()` loop executes for every envelope under either transport.
    /// Rendering is skipped (no render thread): `render()`/`snapshot()`
    /// show the resulting steady state, as the tmux differential does.
    pub fn feed_events(&mut self, events: &[SessionEventWire]) -> &mut Self {
        for ev in events {
            self.feed_event(ev.clone());
        }
        self
    }

    /// One envelope through the production arm (bounded like the slash
    /// drive). After it, the tick arm's secret-prompt poll + stack reconcile
    /// run so a `Prompt` activates the pane exactly as in production.
    pub fn feed_event(&mut self, event: SessionEventWire) -> &mut Self {
        let env = agent_engine::session::Envelope {
            session_id: self.link.transport().session_id().clone(),
            seq: 0,
            ts: chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap_or_default(),
            event,
        };
        let render_handle = super::render_thread::RenderHandle::headless();
        let what = "handle_session_event_arm";
        let flow = block_on_bounded(
            what,
            super::stream_handler::handle_session_event_arm(
                env,
                &mut self.app,
                &mut self.link,
                &self.registry,
                &render_handle,
                &mut self.prompt_bridge,
                None,
            ),
        );
        if let super::stream_handler::ArmFlow::Ended = flow {
            self.actions.push("session-ended".to_string());
        }
        self.app.secret_prompts.poll_requests(&self.secret_prompt_rx);
        input::reconcile_secret_prompt(&mut self.app);
        self
    }

    /// Commands the harness sent to its scripted session so far
    /// (`Debug` renderings; bodies redacted).
    pub fn sent_commands(&self) -> Vec<String> {
        self.scripted_log.lock().unwrap().sent.clone()
    }

    // ── Bounded async slash drive (P6.3) ─────────────────────────────────────

    /// Execute every slash command recorded since the last drive through the
    /// REAL production resolution path — `commands::handle_command` and, for
    /// plugin commands, `commands::execute_command_action` — against the
    /// headless [`Runtime`]. Each command is bounded by [`SLASH_DRIVE_TIMEOUT`]
    /// on a private current-thread runtime: the drive fails, it never hangs.
    ///
    /// Scope: the SYNC-SAFE, App-mutating subset. `CommandAction` arms that
    /// production resolves against the live terminal or engine (gamba launch,
    /// skill-load streaming, plugin reload, …) are recorded as
    /// `command-action-unexecuted:*` in [`Self::take_actions`], never run.
    pub fn drive_slash_commands(&mut self) -> &mut Self {
        for (cmd, arg) in std::mem::take(&mut self.pending_slash) {
            self.run_slash_command(&cmd, &arg);
        }
        self
    }

    /// Run one slash command (already prefix-resolved, no leading `/`)
    /// through the bounded async path directly. Building block of
    /// [`Self::drive_slash_commands`]; useful when a test wants the command
    /// executed without typing it.
    pub fn run_slash_command(&mut self, cmd: &str, arg: &str) -> &mut Self {
        // Production threads the boot-resolved system-prompt path; the
        // harness pins a temp-dir path so `/system save` never touches
        // a real config. Nothing reads it unless the test invokes it.
        let system_prompt_path = std::env::temp_dir().join("synaps-harness-system-prompt.md");
        let what = format!("handle_command(/{cmd} {arg})");
        let action = block_on_bounded(
            &what,
            super::commands::handle_command(
                cmd,
                arg,
                &mut self.app,
                &mut self.link,
                &system_prompt_path,
                &self.registry,
                &self.keybinds,
            ),
        );
        self.apply_command_action(action);
        self
    }

    /// Apply a resolved [`super::commands::CommandAction`], mirroring the
    /// production `SlashCommand` arm in `mod.rs` for the sync-safe subset —
    /// including the ModalStack pushes, so `debug_assert_stack_sync` holds
    /// after every drive exactly as it does after every real event.
    fn apply_command_action(&mut self, action: super::commands::CommandAction) {
        use super::commands::CommandAction;
        match action {
            CommandAction::None | CommandAction::StartStream => {}
            CommandAction::Quit => {
                // Production sends the exit effect; headless we record intent.
                self.quit_requested = true;
            }
            CommandAction::OpenHelpFind { query } => {
                // Mirrors mod.rs `CommandAction::OpenHelpFind` (P7.4 arm).
                let help_registry = synaps_cli::help::HelpRegistry::new(
                    synaps_cli::help::builtin_entries(),
                    self.registry.plugin_help_entries(),
                );
                self.app.help_find = Some(synaps_cli::help::HelpFindState::new(
                    help_registry.entries().to_vec(),
                    &query,
                ));
                self.app.modal_stack.push(super::focus::PaneId::HelpFind);
            }
            CommandAction::OpenEffort => {
                // Mirrors the dispatch OpenEffort arm (idle path — the
                // harness has no live stream; streaming refusal is exercised
                // via the streaming-input route).
                if !self.app.streaming {
                    self.app.effort = Some(super::effort::EffortModalState::new(
                        &self.link.view().model,
                        &self.link.view().thinking_level,
                    ));
                    self.app.modal_stack.push(super::focus::PaneId::Effort);
                }
            }
            CommandAction::OpenModels => {
                self.app.models = Some(super::models::ModelsModalState::new());
                self.app.modal_stack.push(super::focus::PaneId::Models);
            }
            CommandAction::OpenSettings => {
                self.app.settings = Some(super::settings::SettingsState::new());
                self.app.modal_stack.push(super::focus::PaneId::Settings);
            }
            CommandAction::OpenPlugins => {
                // Production loads plugins.json from disk; the harness opens
                // the deterministic default state instead of touching $HOME.
                self.app.plugins = Some(super::plugins::PluginsModalState::new(
                    synaps_cli::skills::state::PluginsState::default(),
                ));
                self.app.modal_stack.push(super::focus::PaneId::Plugins);
            }
            CommandAction::PluginCommand { command, arg } => {
                // The commands.rs:114 executor — bounded like everything else.
                // The default harness registry has no plugin commands, so this
                // arm only fires for tests that register their own.
                block_on_bounded(
                    "execute_command_action(PluginCommand)",
                    super::commands::execute_command_action(
                        CommandAction::PluginCommand { command, arg },
                        &mut self.app,
                        &mut self.link,
                    ),
                );
            }
            // NOT sync-safe: terminal-mode swaps, engine streaming, config
            // reloads. These stay P4-style recorded-not-executed.
            CommandAction::LaunchGamba => self.record_unexecuted("launch-gamba"),
            CommandAction::ReloadPlugins => self.record_unexecuted("reload-plugins"),
            CommandAction::LoadSkill { .. } => self.record_unexecuted("load-skill"),
            _ => self.record_unexecuted("other"),
        }
        #[cfg(debug_assertions)]
        super::focus::debug_assert_stack_sync(&self.app);
    }

    fn record_unexecuted(&mut self, name: &str) {
        self.actions
            .push(format!("command-action-unexecuted:{name}"));
    }

    // ── Internals ────────────────────────────────────────────────────────────

    fn record(&mut self, action: InputAction) {
        let desc = match action {
            InputAction::None | InputAction::HelpFindOutcome => return,
            InputAction::Quit => {
                self.quit_requested = true;
                "quit".to_string()
            }
            InputAction::Submit(text) => format!("submit:{text}"),
            InputAction::SlashCommand(cmd, arg) => {
                let desc = format!("slash:{cmd}:{arg}");
                // P6.3: also queue structurally for the bounded async drive.
                self.pending_slash.push((cmd, arg));
                desc
            }
            InputAction::StreamingInput(text) => format!("streaming-input:{text}"),
            InputAction::Abort => "abort".to_string(),
            InputAction::SettingsApply(key, value) => format!("settings-apply:{key}={value}"),
            InputAction::ModelsApply(model) => format!("models-apply:{model}"),
            InputAction::GrantWorkerModel(model) => format!("grant-worker-model:{model}"),
            InputAction::EffortApply(apply) => format!(
                "effort-apply:{}:{}:{}",
                apply.model, apply.generation, apply.value
            ),
            InputAction::ModelsExpandProvider(p) => format!("models-expand:{p}"),
            InputAction::PluginsOutcome(_) => "plugins-outcome".to_string(),
            InputAction::OpenPluginsMarketplace => "open-plugins-marketplace".to_string(),
            InputAction::PingModels => "ping-models".to_string(),
            InputAction::PluginEditorOpen {
                plugin_id,
                category,
                field,
            } => {
                format!("plugin-editor-open:{plugin_id}:{category}:{field}")
            }
            InputAction::PluginEditorKey { plugin_id, .. } => {
                format!("plugin-editor-key:{plugin_id}")
            }
        };
        self.actions.push(desc);
    }
}

/// Cloneable in-memory `Write` sink for [`TestHarness::render_ansi`].
///
/// Exists because `CrosstermBackend` in ratatui 0.30 only exposes its writer
/// behind an unstable feature — so the harness keeps a handle to the shared
/// buffer instead of retrieving it from the backend afterwards.
#[derive(Clone, Default)]
struct SharedSink(Arc<parking_lot::Mutex<Vec<u8>>>);

impl SharedSink {
    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.0.lock())
    }
}

impl std::io::Write for SharedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// P16.2 test facade — re-exports the termcaps query-burst surface so
/// integration tests (`tests/vt100_spike.rs`, and P16.4's negotiation matrix)
/// can assert on the exact burst bytes and drive the pure reply parser with
/// synthetic DA1-fenced streams. The `tui::termcaps` module itself stays
/// private; this facade only exists under the `testing` feature. The async
/// fd-0 `negotiate()` path is deliberately NOT exported — tests must never
/// touch real stdin (single-consumer rule).
pub mod termcaps {
    pub use super::super::termcaps::{
        parse_burst_replies, write_query_burst, BurstReplies, TermCaps, BURST_TIMEOUT, QUERY_BURST,
    };
}

/// Slice 0 / T241 measurement probe facade — re-exports the highlight counter
/// surfaces so `tests/mem_transcript.rs` and future Slice-N harnesses can
/// read highlight-call counts without reaching into `pub(crate)` internals.
///
/// Store probe access (render_count, cum_height_writes, reset_perf_probe) is
/// available directly on [`TestHarness`].
///
/// Only available under `feature = "testing"`.
pub mod probe {
    /// Reset both highlight counters (`HIGHLIGHT_CALLS`, `SYNTAX_SET_TOUCHED`).
    /// Call before the frame under measurement.
    pub fn highlight_reset() {
        super::super::highlight::highlight_reset_counters();
    }

    /// Read `HIGHLIGHT_CALLS` — syntect highlight sessions since last reset.
    pub fn highlight_call_count() -> usize {
        super::super::highlight::highlight_call_count()
    }

    /// Read `SYNTAX_SET_TOUCHED` — whether `SYNTAX_SET` LazyLock has fired
    /// since the last reset.
    pub fn syntax_set_was_touched() -> bool {
        super::super::highlight::syntax_set_was_touched()
    }
}
