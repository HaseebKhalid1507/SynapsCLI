//! The TUI's setup shared by every boot: `App` from the attach snapshot,
//! terminal + render thread, channels, and the reconnect flow. The TUI is
//! always a daemon client (`attach::run_attached`); there is no in-process
//! boot (PR 5, "daemon is the only mode").

use super::*;

use std::time::Instant;
use synaps_cli::Result;

use agent_engine::session::{AttachSnapshot, ClientTransport};
use helpers::apply_display_tail;
use session_link::{PromptBridge, SessionLink};

/// Client-local HTTP client, built on first use (P4-0): the TUI pays for
/// reqwest + rustls + the native root store only when `/models`,
/// `/settings` or a catalog expand actually asks (`SYNAPS_CLIENT_HTTP=eager`
/// builds it at boot — bisect aid).
pub(crate) struct LazyHttp(std::cell::OnceCell<reqwest::Client>);

use agent_core::core::memstat::ladder as ladder_stage;

impl LazyHttp {
    /// Empty; the first `get()` builds via `build_host_http_client`.
    pub(crate) fn new() -> Self {
        Self(std::cell::OnceCell::new())
    }

    /// The client, building it on first use (emits the `http` ladder stage).
    pub(crate) fn get(&self) -> Result<&reqwest::Client> {
        if let Some(c) = self.0.get() {
            return Ok(c);
        }
        let t0 = Instant::now();
        let client = agent_engine::runtime::build_host_http_client()?;
        agent_core::core::memstat::ladder(
            "http",
            &format_args!("build_ms={}", t0.elapsed().as_millis()),
        );
        Ok(self.0.get_or_init(|| client))
    }

    /// Already built?
    #[allow(dead_code)]
    pub(crate) fn is_built(&self) -> bool {
        self.0.get().is_some()
    }
}

/// Everything `run()`'s event loop and teardown consume from the boot
/// prologue. Fields are handed back by value so `run()` owns them exactly as
/// it did when they were locals — the loop's `&mut` borrows are unchanged.
pub(crate) struct RunContext {
    pub app: App,
    /// The session, behind its `SocketTransport`.
    pub link: SessionLink,
    /// Client-local HTTP client for catalog/model-list fetches (the same
    /// builder the host uses; lazy on the socket path).
    pub http: LazyHttp,
    pub prompt_bridge: PromptBridge,
    pub config: synaps_cli::SynapsConfig,
    pub registry: std::sync::Arc<synaps_cli::skills::registry::CommandRegistry>,
    pub keybind_registry:
        std::sync::Arc<std::sync::RwLock<synaps_cli::skills::keybinds::KeybindRegistry>>,
    pub system_prompt_path: std::path::PathBuf,
    pub render_handle: render_thread::RenderHandle,
    pub boot_done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub exit_done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// P16.2: negotiated terminal capabilities — env detection merged with
    /// the DA1-fenced query burst (or env-only if the burst timed out).
    pub term_caps: termcaps::TermCaps,
    pub event_reader: EventStream,
    pub shutdown_signal_rx: tokio::sync::mpsc::UnboundedReceiver<signals::ShutdownSignal>,
    pub shutdown_signal_task: signals::SignalHandle,
    pub secret_prompt_rx: std::sync::Arc<
        std::sync::Mutex<
            tokio::sync::mpsc::UnboundedReceiver<synaps_cli::tools::SecretPromptRequest>,
        >,
    >,
    pub boot_fx_sent: bool,
    pub exit_fx_sent: bool,
    pub last_draw: Instant,
}

/// First-run guidance: no Anthropic credentials and no provider keys means
/// the first message will fail, so say so up front, instead of a
/// token-refresh error on the first message.
pub(crate) fn no_credentials_notice() -> Option<String> {
    // Capability queries via the broker boundary — no credential file or
    // credential env reads here, and answers are booleans only.
    let has_anthropic = synaps_cli::auth::broker::anthropic_credential_available();
    let has_provider_key = !synaps_cli::auth::broker::configured_static_provider_keys().is_empty();
    credentials_notice_for(has_anthropic, has_provider_key)
}

fn credentials_notice_for(has_anthropic: bool, has_provider_key: bool) -> Option<String> {
    (!has_anthropic && !has_provider_key).then(|| {
        "👋 No credentials found. To get started:\n   • `synaps login` — sign in with Claude Pro/Max (OAuth)\n   • or `synaps login --provider <name>` — store a provider API key with the credential broker (groq, openrouter, …), then pick with /model".to_string()
    })
}

/// Build `App` from the attach snapshot — the same lines and texts the
/// `EngineBoot`-based construction produced (`continued` branch included).
pub(crate) fn app_from_snapshot(snapshot: &AttachSnapshot) -> App {
    let conv = &snapshot.conversation;
    let session = app::session_from_header(&conv.header);
    // F2: apply conversation + display unconditionally when there is history.
    // `continued` only gates the "resumed session …" notice — a fresh session
    // re-attached after parking has history but `continued == false`.
    let has_history = conv.messages_len > 0
        || !conv.api_messages.is_empty()
        || snapshot
            .display_tail
            .as_ref()
            .is_some_and(|t| !t.items.is_empty());
    let mut app = App::new_with_clock(session, clock::TuiClock::real());
    if has_history {
        app.apply_conversation(conv);
        rebuild_display_from_snapshot(&mut app, snapshot);
    }
    if snapshot.meta.continued {
        app.push_msg(ChatMessage::System(format!(
            "resumed session {}",
            conv.header.id
        )));
        if let Some(ref info) = snapshot.meta.continue_info {
            if let Some(ref via) = info.resolved_via {
                app.push_msg(ChatMessage::System(format!(
                    "  ↳ resolved via {} '{}'",
                    via, info.query
                )));
            }
            if let Some(ref notice) = info.compaction_notice {
                app.push_msg(ChatMessage::System(format!("  ↳ {}", notice)));
            }
        }
    }
    app.last_turn_context_window = snapshot.view.context_window;
    app
}

/// Terminal + render thread + channels for the socket attach
/// (`run_attached`).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn finish_setup(
    mut app: App,
    transport: Box<dyn ClientTransport>,
    http: LazyHttp,
    config: synaps_cli::SynapsConfig,
    registry: std::sync::Arc<synaps_cli::skills::registry::CommandRegistry>,
    keybind_registry: std::sync::Arc<
        std::sync::RwLock<synaps_cli::skills::keybinds::KeybindRegistry>,
    >,
    system_prompt_path: std::path::PathBuf,
) -> Result<RunContext> {
    let link = SessionLink::new(transport);
    app.keybinds = Some(keybind_registry.clone());
    // ── Terminal setup + render thread ──
    //
    // The Terminal is moved into the render thread immediately after creation.
    // The main task never touches it again.  All terminal I/O (draw, clear,
    // teardown) goes through `render_handle`.
    //
    // Terminal size for build_render_model: we call crossterm::terminal::size()
    // directly — it reads the TTY fd without needing the Terminal object.
    // See render_thread.rs module comment for the design rationale.
    // P16.3: `setup_terminal` runs BEFORE the DA1 burst below (raw mode must be
    // enabled first), so no negotiated facts exist at the kitty-push site yet.
    // Pass `None` ⇒ blind best-effort push, byte-identical with today. See the
    // `setup_terminal` doc for why the push can't be fact-gated at this site.
    let terminal = setup_terminal(None)?;
    ladder_stage("terminal", &{
        let (c, r) = crossterm::terminal::size().unwrap_or((0, 0));
        format!("cols={c} rows={r}")
    });

    // ── P16.2: DA1-fenced terminal capability query burst ──
    //
    // SINGLE-CONSUMER ORDERING — LOAD-BEARING (crossterm #963/#993, see
    // termcaps.rs module docs + the substrate memo). This emits the batched
    // query burst and reads the replies DIRECTLY from fd 0 under a hard
    // deadline. That is safe if and only if, by construction:
    //   1. raw mode is already enabled — `setup_terminal()` ABOVE — so
    //      replies arrive unechoed and unbuffered by the line discipline;
    //   2. NO other stdin consumer exists yet — `EventStream::new()` is
    //      created BELOW, and nothing in this crate touches
    //      crossterm::event::{poll,read} before it, so crossterm's internal
    //      reader has not been spawned. The bounded read completes (DA1
    //      fence or timeout) and releases fd 0 before the EventStream is
    //      constructed. It even runs before `spawn_render_thread` so no
    //      other thread writes to the terminal during the reply window.
    // Timeout / partial replies ⇒ env-detected caps unchanged (= today's
    // behavior) ⇒ boot proceeds normally. NEVER move this below
    // `EventStream::new()`; NEVER add a second stdin reader for it.
    let t_caps = Instant::now();
    let term_caps =
        termcaps::negotiate(termcaps::TermCaps::detect(), termcaps::BURST_TIMEOUT).await;
    ladder_stage("termcaps", &format_args!("burst_ms={}", t_caps.elapsed().as_millis()));
    // OSC 11: publish the terminal's default bg (None = unknown ⇒ unchanged)
    // before the render thread starts, so the opaque guard sees it from the
    // first frame.
    super::theme::set_terminal_default_bg(term_caps.terminal_bg);

    // P16.3: hand the negotiated caps to the render thread so `render_frame`
    // can gate edge-scrub (tmux provenance) and synchronized-output (mode 2026)
    // on facts. Cloned because `term_caps` is also returned in `RunContext`.
    let (render_handle, boot_done, exit_done) = spawn_render_thread(terminal, term_caps.clone());
    ladder_stage("render_thread", &"");
    // Boot effect is sent via the command channel so the render thread owns it.
    // SYNAPS_NO_BOOT_FX=1 skips it (slow/high-latency links, screen readers).
    if std::env::var("SYNAPS_NO_BOOT_FX").map_or(true, |v| v != "1") {
        render_handle.send_boot_fx(boot_effect());
    }

    let event_reader = EventStream::new();
    let (shutdown_signal_tx, shutdown_signal_rx) = tokio::sync::mpsc::unbounded_channel();
    let shutdown_signal_task = signals::spawn_shutdown_signal_task_with(
        shutdown_signal_tx,
        signals::SignalBackend::for_socket(true),
    );
    ladder_stage("event_stream", &"");
    let (secret_prompt_tx, secret_prompt_rx) = tokio::sync::mpsc::unbounded_channel();
    let prompt_bridge = PromptBridge::new(secret_prompt_tx);
    let secret_prompt_rx = std::sync::Arc::new(std::sync::Mutex::new(secret_prompt_rx));
    // P7.8: the secret-prompt queue now lives on `app.secret_prompts` (§5).
    // The mpsc channel wiring above is unchanged; only the queue moved onto
    // App so the pane handler / harness share production state.
    // ── Event loop ──
    // Track whether the render thread currently has an active boot or exit
    // effect.  The render thread owns the actual Effect values; we track
    // "has been sent and not yet done" on the main side for the tick throttle.
    let boot_fx_sent = std::env::var("SYNAPS_NO_BOOT_FX").map_or(true, |v| v != "1");
    let exit_fx_sent = false;
    let last_draw = Instant::now() - std::time::Duration::from_secs(1);

    Ok(RunContext {
        app,
        link,
        http,
        prompt_bridge,
        config,
        registry,
        keybind_registry,
        system_prompt_path,
        render_handle,
        boot_done,
        exit_done,
        term_caps,
        event_reader,
        shutdown_signal_rx,
        shutdown_signal_task,
        secret_prompt_rx,
        boot_fx_sent,
        exit_fx_sent,
        last_draw,
    })
}

/// The session envelope stream ended (A4): reconnect with backoff and
/// re-mirror; `false` = give up.
///
/// F9: when the daemon died (not a reload), show "daemon connection lost"
/// immediately, clear streaming state, and update the line on each retry.
/// On success show the reconnect notice. On failure set `app.daemon_lost`
/// for the caller to exit non-zero.
pub(crate) async fn try_reconnect(
    app: &mut App,
    link: &mut SessionLink,
    render_handle: &super::render_thread::RenderHandle,
    registry: &std::sync::Arc<synaps_cli::skills::registry::CommandRegistry>,
) -> bool {
    use agent_engine::session::socket_transport::SocketTransport;
    use std::time::Duration;

    let attach_mode = link.mode();
    // Snapshot transport state before entering the loop (avoids
    // holding a `&mut` on link across force_publish / refresh_view).
    let is_reload = link.transport_mut().is_reload_pending();
    let daemon_pid = link.transport_mut().daemon_pid();
    let session_id = link.transport_mut().session_id().clone();
    let budget = SocketTransport::reconnect_budget();
    let budget_secs = budget.as_secs();

    // F9: clear streaming state — a turn cannot be streaming without a daemon.
    if app.streaming {
        app.header_turn_ended(None);
        app.streaming = false;
    }
    app.compacting = false;

    // Show the initial message (reload = existing text, crash = F9 notice).
    if !is_reload {
        app.push_msg(ChatMessage::System(format!(
            "daemon connection lost — reconnecting ({budget_secs}s left)…"
        )));
        app.request_redraw();
        force_publish(app, link, registry, render_handle);
    }

    let deadline = tokio::time::Instant::now() + budget;
    let mut backoff = Duration::from_millis(100);
    let mut attempts = 0u32;

    loop {
        match link.transport_mut().reconnect_once(attach_mode).await {
            Ok(snapshot) => {
                link.refresh_view();
                let new_pid = link.transport_mut().daemon_pid();
                let lifecycle = &snapshot.meta.lifecycle;
                let status = format!("{lifecycle:?}");
                remirror(app, &snapshot);
                // The new attach has its own roster (every client
                // re-attached under a new id, or hasn't yet).
                app.attached_clients = link.transport().roster().clients().len();
                super::watch::on_reconnect(app, std::time::Instant::now());
                app.push_msg(ChatMessage::System(format!(
                    "reconnected to daemon (pid {new_pid}) — session {} {status}",
                    session_id
                )));
                // `daemon reload` re-execs only the daemon: after an
                // upgrade this client is still the old binary.
                // Once per daemon build, not on every reconnect.
                let skew = link.transport_mut().daemon_binary().and_then(|d| {
                    super::attach::skew_notice_once(app, d, snapshot.meta.id.as_str())
                });
                if let Some(skew) = skew {
                    app.push_msg(ChatMessage::System(format!("⚠ {skew}")));
                }
                app.toasts.upsert(
                    toast::Toast::new("reload", "reconnected").titled("Daemon"),
                );
                app.request_redraw();
                return true;
            }
            Err(agent_engine::session::TransportError::Refused(_))
            | Err(agent_engine::session::TransportError::Version { .. }) => {
                // Terminal errors — stop retrying.
                break;
            }
            Err(_e) => {
                attempts += 1;
            }
        }

        if tokio::time::Instant::now() + backoff > deadline {
            break;
        }

        // Update the countdown message (at least on each retry).
        if !is_reload {
            let left = deadline.duration_since(tokio::time::Instant::now());
            let secs_left = left.as_secs();
            app.status_text = Some(format!(
                "reconnecting… ({secs_left}s left, attempt {attempts})"
            ));
            app.request_redraw();
            force_publish(app, link, registry, render_handle);
        }

        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }

    // Reconnect failed.
    app.status_text = None;
    if !is_reload {
        app.daemon_lost = Some(super::app::DaemonLostInfo {
            pid: daemon_pid,
            session_id: session_id.to_string(),
            budget_secs,
        });
    }
    app.push_msg(ChatMessage::Error(format!(
        "lost the daemon (pid {daemon_pid}) — could not reconnect within {budget_secs}s"
    )));
    app.request_redraw();
    false
}

/// Push one render frame from inside `try_reconnect` (the main event loop is
/// blocked, so we replicate the build+publish path directly).
fn force_publish(
    app: &mut App,
    link: &mut SessionLink,
    registry: &std::sync::Arc<synaps_cli::skills::registry::CommandRegistry>,
    render_handle: &super::render_thread::RenderHandle,
) {
    if let Ok((w, h)) = crossterm::terminal::size() {
        if w > 0 && h > 0 {
            let size = ratatui::layout::Size { width: w, height: h };
            let built = super::draw::build_render_model(
                &mut super::view_model::ViewInputs::from_app(app),
                &**link.view(),
                registry,
                size,
            );
            if let Some((model, patch)) = built {
                patch.apply(app);
                render_handle.publish(model);
            }
        }
    }
}

/// Digest attaches carry a daemon-projected `display_tail`; Full attaches
/// carry the history and project it locally — same filter either way.
fn rebuild_display_from_snapshot(app: &mut App, snapshot: &AttachSnapshot) {
    match &snapshot.display_tail {
        Some(tail) => apply_display_tail(tail, app),
        None => rebuild_display_messages(&snapshot.conversation.api_messages, app),
    }
}

/// Rebuild the transcript from an `AttachSnapshot` (reconnect / re-attach):
/// the cancelled turn's partial text is gone by design (§2.8 step 7).
pub(crate) fn remirror(app: &mut App, snapshot: &AttachSnapshot) {
    app.transcript.clear();
    app.invalidate();
    app.apply_conversation(&snapshot.conversation);
    rebuild_display_from_snapshot(app, snapshot);
    app.streaming = snapshot.streaming;
    app.last_turn_context_window = snapshot.view.context_window;
}

#[cfg(test)]
mod tests {
    use super::credentials_notice_for;

    #[test]
    fn credentials_notice_only_when_nothing_can_answer() {
        let n = credentials_notice_for(false, false).expect("no credentials at all");
        assert!(n.contains("No credentials found") && n.contains("synaps login"), "{n}");
        assert!(credentials_notice_for(true, false).is_none());
        assert!(credentials_notice_for(false, true).is_none());
    }
}
