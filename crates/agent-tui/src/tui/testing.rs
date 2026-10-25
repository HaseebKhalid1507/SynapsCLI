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
    /// The scripted session's engine-side history (`DisplayTail`/`Messages`).
    scripted_history: std::sync::Arc<std::sync::Mutex<Vec<synaps_cli::SharedMessage>>>,
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
    /// Lines typed while streaming, awaiting [`Self::drive_streaming_input`].
    pending_streaming: Vec<String>,
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

/// The fixed "now" the harness pins the /sessions modal to.
pub fn sessions_test_now() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::<chrono::Utc>::from_timestamp(1_800_000_000, 0).unwrap_or_default()
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        // `set_build_id` is per harness, not per thread.
        super::header::set_build_id_for_thread(None);
        // Same for the injected terminal default bg.
        super::theme::set_terminal_default_bg_for_thread(None);
    }
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
        // Determinism: a fresh session's id is a timestamp plus a random
        // suffix, and the header shows it when the session has no name or
        // title — pin it so replays are byte-identical across boots.
        app.session.id = "test-session".to_string();
        // Determinism: the header shows the working folder.
        app.header_folder = "~/Projects/SynapsCLI".to_string();
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
        let scripted_history = transport.history();
        let (secret_prompt_tx, secret_prompt_rx) = tokio::sync::mpsc::unbounded_channel();
        TestHarness {
            app,
            link: SessionLink::new(Box::new(transport)),
            prompt_bridge: PromptBridge::new(secret_prompt_tx),
            scripted_log,
            scripted_history,
            secret_prompt_rx: std::sync::Arc::new(std::sync::Mutex::new(secret_prompt_rx)),
            registry: Arc::new(CommandRegistry::new(BUILTIN_COMMANDS, Vec::new())),
            keybinds: KeybindRegistry::new(),
            terminal,
            size: Size::new(cols, rows),
            actions: Vec::new(),
            pending_slash: Vec::new(),
            pending_streaming: Vec::new(),
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

    /// Pretend the terminal answered OSC 11 with `bg` (or, with `None`, that
    /// it never answered). Per thread, like [`TestHarness::set_build_id`], so
    /// the process-global detected bg is never written by tests.
    pub fn set_terminal_bg(&mut self, bg: Option<(u8, u8, u8)>) -> &mut Self {
        super::theme::set_terminal_default_bg_for_thread(Some(bg));
        self
    }

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

    /// Render one frame with a boot effect in flight, exactly as the render
    /// thread passes it to `render_frame_into`. Regression hook for the
    /// idle-redraw bug: the frame body must never consume the boot effect —
    /// the render thread's done-check owns clearing it (and signals
    /// `boot_done`).
    pub fn render_with_boot_fx(
        &mut self,
        boot_fx: &mut Option<tachyonfx::Effect>,
        elapsed: std::time::Duration,
    ) {
        let (model, patch) = build_render_model(
            &mut super::view_model::ViewInputs::from_app(&mut self.app),
            &**self.link.view(),
            &self.registry,
            self.size,
        )
        .expect("build_render_model returned None — gamba never runs headless");
        patch.apply(&mut self.app);
        let mut exit_fx = None;
        self.terminal
            .draw(|frame| render_frame_into(frame, &model, boot_fx, &mut exit_fx, elapsed))
            .expect("TestBackend draw is infallible");
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

    /// Whether the neon prompt currently wants animation frames — the term it
    /// contributes to the main loop's tick guard.
    pub fn prompt_animating(&self) -> bool {
        self.app
            .prompt_clock
            .animating(self.app.clock.now(), self.app.streaming)
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
    /// Seed a user turn into the transcript.
    pub fn push_user_message(&mut self, text: &str) -> &mut Self {
        self.app.push_msg(ChatMessage::User(text.to_string()));
        self
    }

    /// Seed a thinking block into the transcript.
    pub fn push_thinking_message(&mut self, text: &str) -> &mut Self {
        self.app.push_msg(ChatMessage::Thinking(text.to_string()));
        self
    }

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

    /// Open the `/sessions` modal the way dispatch does. `socket = None` is
    /// the in-process TUI; `Some(_)` puts the Live tab in daemon mode
    /// ("asking the daemon…") — the harness has no runtime, so no poller
    /// runs: feed answers with [`Self::sessions_feed_live`].
    pub fn open_sessions_with(&mut self, socket: Option<std::path::PathBuf>) -> &mut Self {
        let own = super::sessions::own_row(&self.app, &self.link);
        super::sessions::open_with(&mut self.app, own, socket);
        if let Some(st) = self.app.sessions.as_mut() {
            // Determinism: relative times and `~` are drawn against these.
            st.now = sessions_test_now();
            st.home = Some(std::path::PathBuf::from("/home/u"));
        }
        self
    }

    /// The id of this harness's own session (as the Live tab sees it).
    pub fn own_session_id(&self) -> String {
        self.link.transport().session_id().0.clone()
    }

    /// Deliver a daemon list answer through the production arm logic
    /// (`SessionsState::apply`), as the poller would.
    pub fn sessions_feed_live(
        &mut self,
        result: Result<Vec<agent_engine::session::SessionMeta>, String>,
    ) -> &mut Self {
        let own = super::sessions::own_row(&self.app, &self.link);
        if let Some(st) = self.app.sessions.as_mut() {
            let gen = st.gen;
            st.apply(super::sessions::SessionsMsg::Live { gen, result }, || own);
            st.now = sessions_test_now();
        }
        self
    }

    /// Deliver the Past list answer.
    pub fn sessions_feed_past(&mut self, rows: Vec<synaps_cli::SessionInfo>) -> &mut Self {
        if let Some(st) = self.app.sessions.as_mut() {
            let gen = st.gen;
            st.apply(super::sessions::SessionsMsg::Past { gen, result: Ok(rows) }, || unreachable!());
        }
        self
    }

    /// Deliver a pin/close answer for the in-flight op.
    pub fn sessions_feed_op(&mut self, result: Result<String, String>) -> &mut Self {
        if let Some(st) = self.app.sessions.as_mut() {
            let gen = st.gen;
            let op = st.pending.clone().unwrap_or(super::sessions::SessionsOp::Pin { id: String::new(), on: true });
            st.apply(super::sessions::SessionsMsg::OpDone { gen, op, result }, || unreachable!());
        }
        self
    }

    /// Mark a pin/close as in flight (what `spawn_op` does before its task).
    pub fn sessions_mark_pending(&mut self, op_close_id: &str) -> &mut Self {
        if let Some(st) = self.app.sessions.as_mut() {
            let label = st
                .live
                .iter()
                .find(|m| m.id.0 == op_close_id)
                .map(super::sessions::label)
                .unwrap_or_else(|| op_close_id.to_string());
            st.pending = Some(super::sessions::SessionsOp::Close { id: op_close_id.to_string(), label });
        }
        self
    }

    /// Whether the /sessions modal is open.
    pub fn sessions_open(&self) -> bool {
        self.app.sessions.is_some()
    }

    /// Whether the close-confirm popup is up.
    pub fn sessions_confirm_open(&self) -> bool {
        self.app.sessions.as_ref().is_some_and(|s| s.confirm.is_some())
    }

    /// The inline notice line, if any.
    pub fn sessions_notice(&self) -> Option<String> {
        self.app.sessions.as_ref().and_then(|s| s.notice.as_ref().map(|n| n.text.clone()))
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
        let _rx = self.activate_prompt_with_kind(
            synaps_cli::tools::PromptKind::Secret,
            title,
            prompt,
        );
        self
    }

    /// Inject a prompt of an explicit [`PromptKind`] and keep the answer
    /// receiver so tests can assert what the pane sent back (`Some("y")` for
    /// a confirm allow, `None` for deny/cancel). Same production path as
    /// [`Self::activate_secret_prompt`].
    pub fn activate_prompt_with_kind(
        &mut self,
        kind: synaps_cli::tools::PromptKind,
        title: &str,
        prompt: &str,
    ) -> tokio::sync::oneshot::Receiver<Option<String>> {
        self.activate_prompt_scoped(kind, title, prompt, None)
    }

    /// Inject an extension tool-call confirm (`Confirm tool call`) raised by
    /// `extension` for `tool`, exactly as the runtime builds it: the scope
    /// is what makes "Allow all from <ext>" available.
    pub fn activate_tool_call_confirm(
        &mut self,
        extension: &str,
        tool: &str,
        prompt: &str,
    ) -> tokio::sync::oneshot::Receiver<Option<String>> {
        self.activate_prompt_scoped(
            synaps_cli::tools::PromptKind::Confirm,
            agent_engine::session::CONFIRM_PROMPT_TITLE,
            prompt,
            Some(agent_engine::session::ConfirmScope {
                extensions: vec![extension.to_string()],
                tool: Some(tool.to_string()),
                rewritten_by: vec![],
            }),
        )
    }

    fn activate_prompt_scoped(
        &mut self,
        kind: synaps_cli::tools::PromptKind,
        title: &str,
        prompt: &str,
        confirm_scope: Option<agent_engine::session::ConfirmScope>,
    ) -> tokio::sync::oneshot::Receiver<Option<String>> {
        use synaps_cli::tools::SecretPromptRequest;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        tx.send(SecretPromptRequest {
            kind,
            title: title.to_string(),
            prompt: prompt.to_string(),
            confirm_scope,
            response_tx,
        })
        .expect("prompt request send is infallible on a fresh channel");
        let rx = std::sync::Arc::new(std::sync::Mutex::new(rx));
        self.app.secret_prompts.poll_requests(&rx);
        input::reconcile_secret_prompt(&mut self.app);
        // Scenarios answer right away; the 400 ms confirm arming delay is
        // exercised explicitly by `activate_tool_call_confirm_unarmed`.
        self.app.secret_prompts.arm_now_for_tests();
        response_rx
    }

    /// Like [`Self::activate_tool_call_confirm`] but WITHOUT skipping the
    /// confirm arming delay: keys sent right away must be ignored.
    pub fn activate_tool_call_confirm_unarmed(
        &mut self,
        extension: &str,
        tool: &str,
        prompt: &str,
    ) -> tokio::sync::oneshot::Receiver<Option<String>> {
        use synaps_cli::tools::SecretPromptRequest;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        tx.send(SecretPromptRequest {
            kind: synaps_cli::tools::PromptKind::Confirm,
            title: agent_engine::session::CONFIRM_PROMPT_TITLE.to_string(),
            prompt: prompt.to_string(),
            confirm_scope: Some(agent_engine::session::ConfirmScope {
                extensions: vec![extension.to_string()],
                tool: Some(tool.to_string()),
                rewritten_by: vec![],
            }),
            response_tx,
        })
        .expect("prompt request send is infallible on a fresh channel");
        let rx = std::sync::Arc::new(std::sync::Mutex::new(rx));
        self.app.secret_prompts.poll_requests(&rx);
        input::reconcile_secret_prompt(&mut self.app);
        response_rx
    }

    /// Let the active confirm's arming delay elapse (without sleeping).
    pub fn arm_confirm_now(&mut self) {
        self.app.secret_prompts.arm_now_for_tests();
    }

    /// Pretend `ms` milliseconds passed for the active confirm's input guard.
    pub fn age_confirm_guard(&mut self, ms: u64) {
        self.app
            .secret_prompts
            .age_input_guard_for_tests(std::time::Duration::from_millis(ms));
    }

    /// Inject and activate a `Confirm` prompt (the `activate_tools` y/yes
    /// gate shape). Returns the answer receiver.
    pub fn activate_confirm_prompt(
        &mut self,
        title: &str,
        prompt: &str,
    ) -> tokio::sync::oneshot::Receiver<Option<String>> {
        self.activate_prompt_with_kind(synaps_cli::tools::PromptKind::Confirm, title, prompt)
    }

    /// `Confirm` dialog: the focused button (`Deny` when none is active).
    pub fn confirm_focus(&self) -> synaps_cli::tools::ConfirmChoice {
        self.app.secret_prompts.confirm_focus()
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
            PaneId::Sessions => "sessions",
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
    /// The probes are thread-local (#285): they only see highlight work done
    /// on the calling thread, which is where `render()` draws.
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

    /// Run `frames` animation ticks exactly as the run loop's tick arm
    /// does for the message area (spinner advance + tail invalidation).
    /// Pair with [`Self::advance_clock_ms`] to let a running tool's clock
    /// move between frames.
    pub fn tick_animations(&mut self, frames: usize) -> &mut Self {
        for _ in 0..frames {
            self.app.tick_message_animations();
        }
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

    /// Run as `--attach --no-confirm-answers` does: confirm prompts are
    /// never opened or answered, only listed as waiting.
    pub fn no_confirm_answers(&mut self) -> &mut Self {
        self.prompt_bridge
            .set_confirm_policy(super::session_link::ConfirmPolicy::ViewOnly);
        self
    }

    /// Force the app's streaming flag — lets scenarios exercise the
    /// streaming-input command refusal path without a live engine stream.
    pub fn set_streaming(&mut self, streaming: bool) -> &mut Self {
        self.app.streaming = streaming;
        self
    }

    /// Pin the build id the header shows (`""` = no id: `v<version>` alone),
    /// so header layout tests don't depend on the id the binary was built
    /// with (a baked git hash, or none). For this harness only; cleared on
    /// drop.
    pub fn set_build_id(&mut self, id: &str) -> &mut Self {
        super::header::set_build_id_for_thread(Some(id));
        self
    }

    /// Pin the working folder the header shows.
    pub fn set_folder(&mut self, folder: &str) -> &mut Self {
        self.app.header_folder = folder.to_string();
        self
    }

    /// Mark this client as attached read-only (`--observe`).
    pub fn set_observing(&mut self, observing: bool) -> &mut Self {
        self.app.observing = observing;
        self
    }

    /// Name the session (the header's session label).
    pub fn set_session_name(&mut self, name: &str) -> &mut Self {
        self.app.session.name = Some(name.to_string());
        self
    }

    /// Force the transient status line (`connecting…`, `reconnecting… (…)`,
    /// …) the run loop sets around daemon reconnects.
    pub fn set_status_text(&mut self, text: Option<&str>) -> &mut Self {
        self.app.status_text = text.map(str::to_string);
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
        self.app.secret_prompts.arm_now_for_tests();
        self
    }

    /// Fire the subagent watch poll arm once (as `run()` does when its
    /// deadline passes) and return the query id it sent, if any.
    pub fn drive_watch_poll(&mut self) -> Option<u64> {
        let now = std::time::Instant::now();
        block_on_bounded(
            "watch::poll",
            super::watch::poll(&mut self.app, &mut self.link, now),
        );
        super::watch::pending_poll(&self.app).map(|(id, _)| id)
    }

    /// Whether the watch poll arm is armed (a worker view is focused and
    /// still has something to read).
    pub fn watch_poll_due(&self) -> bool {
        super::watch::poll_due_at(&self.app).is_some()
    }

    /// The focused worker view's handle id (`None` on the main thread).
    pub fn watched(&self) -> Option<String> {
        super::watch::focused(&self.app).map(str::to_string)
    }

    /// Plain text of the main thread's transcript.
    pub fn main_transcript_text(&self) -> String {
        self.app
            .transcript
            .messages()
            .iter()
            .map(|m| describe(&m.msg))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Plain text of a worker view's transcript (empty if not open).
    pub fn worker_transcript_text(&self, handle_id: &str) -> String {
        self.app
            .threads
            .worker(handle_id)
            .map(|w| {
                w.store
                    .messages()
                    .iter()
                    .map(|m| describe(&m.msg))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }

    /// Stage the engine-side history the scripted session answers
    /// `Query{DisplayTail|Messages}` from (phase 4 Digest-mode rebuilds).
    pub fn set_history(&mut self, msgs: Vec<synaps_cli::SharedMessage>) -> &mut Self {
        *self.scripted_history.lock().unwrap() = msgs;
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

    /// Resolve every line typed while streaming through the production
    /// `commands::resolve_streaming_input`, then apply the result the way
    /// the dispatch `StreamingInput` arm does. Steers and refusals are
    /// recorded as `streaming-steer:<text>` / `streaming-refused:<text>`.
    pub fn drive_streaming_input(&mut self) -> &mut Self {
        use super::commands::{CommandAction, StreamingSlash};
        for text in std::mem::take(&mut self.pending_streaming) {
            match super::commands::resolve_streaming_input(&text, &mut self.app, &self.registry) {
                StreamingSlash::Steer => self.actions.push(format!("streaming-steer:{text}")),
                StreamingSlash::Refused => {
                    self.actions.push(format!("streaming-refused:{text}"))
                }
                StreamingSlash::Ran(CommandAction::OpenSettings) => {
                    super::settings::open_modal(&mut self.app);
                }
                StreamingSlash::Ran(CommandAction::OpenSessions) => {
                self.open_sessions_with(None);
            }
                StreamingSlash::Ran(CommandAction::Quit) => self.quit_requested = true,
                StreamingSlash::Ran(_) => {}
            }
        }
        #[cfg(debug_assertions)]
        super::focus::debug_assert_stack_sync(&self.app);
        self
    }

    /// Run one `/settings` apply through the production
    /// `helpers::apply_setting` against the scripted session (what the
    /// dispatch `SettingsApply` arm does). Writes the config file under
    /// whatever `SYNAPS_BASE_DIR` the test pinned.
    pub fn apply_setting(&mut self, key: &'static str, value: &str) -> &mut Self {
        let what = format!("apply_setting({key}={value})");
        block_on_bounded(
            &what,
            super::helpers::apply_setting(key, value, &mut self.app, &mut self.link),
        );
        self
    }

    /// Whether the app believes a turn is streaming.
    pub fn is_streaming(&self) -> bool {
        self.app.streaming
    }

    /// The settings modal's row note / error (`(row key, text)`), if any.
    pub fn settings_row_note(&self) -> Option<(String, String)> {
        self.app.settings.as_ref().and_then(|s| s.row_error.clone())
    }

    /// Whether a theme cross-fade is in flight.
    pub fn theme_transition_active(&self) -> bool {
        self.app.theme_transition.is_some()
    }

    /// The model the session view reports.
    pub fn view_model(&self) -> String {
        self.link.view().model.clone()
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
            CommandAction::OpenSettings => super::settings::open_modal(&mut self.app),
            CommandAction::OpenSessions => {
                self.open_sessions_with(None);
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
            InputAction::StreamingInput(text) => {
                self.pending_streaming.push(text.clone());
                format!("streaming-input:{text}")
            }
            InputAction::SubagentSteer { handle_id, text } => {
                // The dispatch arm's App-side half (its send is not run).
                super::watch::steer_sent(&mut self.app, &handle_id, &text);
                format!("subagent-steer:{handle_id}:{text}")
            }
            InputAction::Abort => "abort".to_string(),
            InputAction::SettingsApply(key, value) => format!("settings-apply:{key}={value}"),
            InputAction::ModelsApply(model) => format!("models-apply:{model}"),
            InputAction::GrantWorkerModel(model) => format!("grant-worker-model:{model}"),
            InputAction::EffortApply(apply) => format!(
                "effort-apply:{}:{}:{}",
                apply.model, apply.generation, apply.value
            ),
            InputAction::ModelsExpandProvider(p) => format!("models-expand:{p}"),
            InputAction::SessionsOp(op) => match op {
                super::sessions::SessionsOp::Pin { id, on } => format!("sessions-pin:{id}:{on}"),
                super::sessions::SessionsOp::Close { id, .. } => format!("sessions-close:{id}"),
            },
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

/// `kind: text` for one transcript message (the transcript-text helpers).
fn describe(msg: &super::transcript::ChatMessage) -> String {
    use super::transcript::ChatMessage as M;
    let kind = match msg {
        M::User(_) => "user",
        M::Thinking(_) => "thinking",
        M::Text(_) => "text",
        M::ToolUseStart { .. } => "tool-start",
        M::ToolUse { .. } => "tool",
        M::ToolResult { .. } => "tool-result",
        M::Error(_) => "error",
        M::System(_) => "system",
        M::Event { .. } => "event",
    };
    format!("{kind}: {}", msg.source_text())
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
/// Only available under `feature = "testing"`. The highlight probes are
/// thread-local (#285): reads/resets only see the calling thread's work.
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
