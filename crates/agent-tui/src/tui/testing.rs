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
//! - **Sync scenarios only.** [`InputAction`]s that the real event loop
//!   resolves asynchronously (message submission → streaming, slash commands,
//!   plugin outcomes, …) are *recorded* — see [`TestHarness::take_actions`] —
//!   but not executed. Deterministic clocks and async wait-until-idle are the
//!   P6 follow-up.
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

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::buffer::Buffer;
use ratatui::layout::{Rect, Size};
use ratatui::{Terminal, TerminalOptions, Viewport};

use synaps_cli::skills::keybinds::KeybindRegistry;
use synaps_cli::skills::registry::CommandRegistry;
use synaps_cli::skills::BUILTIN_COMMANDS;
use synaps_cli::tools::SecretPromptQueue;
use synaps_cli::{Runtime, Session};

use super::app::{App, ChatMessage};
use super::draw::{build_render_model, render_frame_into};
use super::input::{self, InputAction};

/// A headless, deterministic driver for the chat UI.
///
/// Owns the [`App`], a stub [`Runtime`], and a [`TestBackend`] terminal.
/// See the [module docs](self) for the loop it replicates and its limits.
pub struct TestHarness {
    app: App,
    runtime: Runtime,
    registry: Arc<CommandRegistry>,
    keybinds: KeybindRegistry,
    secret_prompts: SecretPromptQueue,
    terminal: Terminal<TestBackend>,
    size: Size,
    /// Human-readable records of dispatched [`InputAction`]s the harness
    /// cannot execute synchronously (submissions, slash commands, …).
    actions: Vec<String>,
    quit_requested: bool,
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

        TestHarness {
            app,
            runtime: Runtime::new_headless(),
            registry: Arc::new(CommandRegistry::new(BUILTIN_COMMANDS, Vec::new())),
            keybinds: KeybindRegistry::new(),
            secret_prompts: SecretPromptQueue::new(),
            terminal,
            size: Size::new(cols, rows),
            actions: Vec::new(),
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
            &self.runtime,
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
        let model = build_render_model(
            &mut self.app,
            &self.runtime,
            &self.registry,
            &self.secret_prompts,
            self.size,
        )
        .expect("build_render_model returned None — gamba never runs headless");

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
        let model = build_render_model(
            &mut self.app,
            &self.runtime,
            &self.registry,
            &self.secret_prompts,
            self.size,
        )
        .expect("build_render_model returned None — gamba never runs headless");

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

    /// Current contents of the input box.
    pub fn input_contents(&self) -> &str {
        &self.app.input
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
    pub fn selected_text(&self) -> Option<String> {
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

    /// Begin a streaming tool call — drives the store's real
    /// `on_tool_use_start` routing, exactly as the stream handler does.
    pub fn tool_use_start(&mut self, tool_id: &str, tool_name: &str) -> &mut Self {
        self.app.on_tool_use_start(tool_id.to_string(), tool_name.to_string());
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
        let toast = super::toast::Toast::new(id, text)
            .ttl(Some(std::time::Duration::from_secs(ttl_secs)));
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

    // ── Internals ────────────────────────────────────────────────────────────

    fn record(&mut self, action: InputAction) {
        let desc = match action {
            InputAction::None | InputAction::HelpFindOutcome => return,
            InputAction::Quit => {
                self.quit_requested = true;
                "quit".to_string()
            }
            InputAction::Submit(text) => format!("submit:{text}"),
            InputAction::SlashCommand(cmd, arg) => format!("slash:{cmd}:{arg}"),
            InputAction::StreamingInput(text) => format!("streaming-input:{text}"),
            InputAction::Abort => "abort".to_string(),
            InputAction::SettingsApply(key, value) => format!("settings-apply:{key}={value}"),
            InputAction::ModelsApply(model) => format!("models-apply:{model}"),
            InputAction::ModelsExpandProvider(p) => format!("models-expand:{p}"),
            InputAction::PluginsOutcome(_) => "plugins-outcome".to_string(),
            InputAction::OpenPluginsMarketplace => "open-plugins-marketplace".to_string(),
            InputAction::PingModels => "ping-models".to_string(),
            InputAction::PluginEditorOpen { plugin_id, category, field } => {
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
