//! P12.1: boot/setup prologue extracted from `run()` — PURE CODE MOTION.
//!
//! The body of [`run_setup`] below is byte-identical to `run()`'s former
//! prologue (previously `mod.rs` lines 68-207): engine boot unpack, `App`
//! construction (incl. the resumed-session branch), channel/handle setup,
//! render-thread spawn, and the tick-throttle state init. `run()` now calls
//! this, destructures the returned [`RunContext`], and enters the (unchanged)
//! `tokio::select!` loop. No logic changed.

use super::*;

use std::sync::Arc;
use std::time::Instant;
use synaps_cli::Result;

use agent_engine::host::{EngineHost, HostOpts};
use agent_engine::session::{
    AttachMode, AttachSnapshot, ClientKind, ClientMeta, ClientTransport, CompactionPolicyWire,
    LocalTransport, SessionConfig,
};
use session_link::{PromptBridge, SessionLink};

/// How this TUI reaches its session (PLAN-phase3 §3.2).
pub(crate) enum TransportMode {
    /// In-process: the `EngineHost` + `SessionActor` live in this process
    /// (the host is kept alive for the loop's lifetime).
    Local {
        #[allow(dead_code)]
        host: Arc<EngineHost>,
    },
    /// Over the daemon socket (A4): no host, no runtime, no extension host.
    Socket,
}

/// Client-local HTTP client, built on first use (P4-0). The in-process TUI
/// seeds it with the host's client (`LazyHttp::from`), so nothing changes
/// there; the socket client (`--attach`) pays for reqwest + rustls + the
/// native root store only when `/models`, `/settings` or a catalog expand
/// actually asks (`SYNAPS_CLIENT_HTTP=eager` builds it at boot — bisect aid).
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

impl From<reqwest::Client> for LazyHttp {
    fn from(client: reqwest::Client) -> Self {
        Self(std::cell::OnceCell::from(client))
    }
}

/// Everything `run()`'s event loop and teardown consume from the boot
/// prologue. Fields are handed back by value so `run()` owns them exactly as
/// it did when they were locals — the loop's `&mut` borrows are unchanged.
pub(crate) struct RunContext {
    pub app: App,
    /// The session, behind a transport (in-process `LocalTransport` today;
    /// `SocketTransport` for `--attach`).
    pub link: SessionLink,
    /// Client-local HTTP client for catalog/model-list fetches (the same
    /// builder the host uses; lazy on the socket path).
    pub http: LazyHttp,
    pub prompt_bridge: PromptBridge,
    pub mode: TransportMode,
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
    /// In-process only (`Some` under `TransportMode::Local`): the host's
    /// extension manager, for `/extensions` and interactive plugin commands.
    pub ext_mgr_shared: Option<
        std::sync::Arc<tokio::sync::RwLock<synaps_cli::extensions::manager::ExtensionManager>>,
    >,
    pub boot_fx_sent: bool,
    pub exit_fx_sent: bool,
    pub last_draw: Instant,
}

/// Boot prologue for [`super::run`]. Same inputs / error type as `run()`.
pub(crate) async fn run_setup(
    continue_session: Option<Option<String>>,
    system: Option<String>,
    prompt_manifest: Option<std::path::PathBuf>,
    profile: Option<String>,
    no_extensions: bool,
) -> Result<RunContext> {
    // ── Engine boot: host + one session on the actor, attached in-process ──
    let host = EngineHost::boot_and_install(HostOpts {
        profile,
        no_extensions,
    })
    .await?;
    let handle = host
        .create_session(SessionConfig {
            continue_session,
            system,
            prompt_manifest,
            cwd: None,
            auto_approve_confirms: false,
            model_override: None,
            persist: true,
            auto_compact: false,
            compaction_policy: CompactionPolicyWire::LinkedSuccessor,
            await_extensions: false,
            keep_warm: false,
        })
        .await?;
    let (transport, snapshot) =
        LocalTransport::attach_with(handle, ClientMeta::new(ClientKind::Tui), AttachMode::Mirror)
            .await
            .map_err(|e| synaps_cli::RuntimeError::Config(format!("attach: {e}")))?;

    let config: synaps_cli::SynapsConfig = (**host.config()).clone();
    let registry = Arc::clone(host.command_registry());
    let keybind_registry = Arc::clone(host.keybind_registry());
    let mcp_server_count = host.mcp_server_count();
    let system_prompt_path = synaps_cli::config::resolve_read_path("system.md");
    let http = LazyHttp::from(host.parts().client.clone());
    let ext_mgr_shared = Arc::clone(host.ext_manager());

    let mut app = app_from_snapshot(&snapshot);
    app.keybinds = Some(keybind_registry.clone());

    // Surface config parse warnings once at startup (unknown keys, bad values).
    for w in &config.warnings {
        app.push_msg(ChatMessage::System(format!("⚠ config: {}", w)));
    }

    // First-run guidance: no Anthropic credentials and no provider keys means
    // the first message will fail — tell the user up front instead.
    {
        // Capability queries via the broker boundary — no credential file or
        // credential env reads here, and answers are booleans only.
        let has_anthropic = synaps_cli::auth::broker::anthropic_credential_available();
        let has_provider_key =
            !synaps_cli::auth::broker::configured_static_provider_keys().is_empty();
        if !has_anthropic && !has_provider_key {
            app.push_msg(ChatMessage::System(
                "👋 No credentials found. To get started:\n   • `synaps login` — sign in with Claude Pro/Max (OAuth)\n   • or `synaps login --provider <name>` — store a provider API key with the credential broker (groq, openrouter, …), then pick with /model".to_string(),
            ));
        }
    }

    if mcp_server_count > 0 {
        tracing::info!(
            "{} MCP servers available (use connect_mcp_server to activate)",
            mcp_server_count
        );
    }

    let mut ctx = finish_setup(
        app,
        Box::new(transport),
        http,
        TransportMode::Local { host },
        config,
        registry,
        keybind_registry,
        system_prompt_path,
        Some(ext_mgr_shared),
    )
    .await?;

    // Legacy sidecar key migration
    loop_arms::migrate_sidecar_toggle_key_to_claimed_plugins(&ctx.registry.lifecycle_claims());

    if !no_extensions {
        let session_id_for_hook = ctx.app.session.id.clone();
        ctx.app.extension_loader_running = true;
        ctx.app.toasts.upsert(
            toast::Toast::new("extension-loader", "Discovering extensions…")
                .titled("Extensions")
                .at(toast::ToastPosition::TOP_CENTER)
                .ttl(None),
        );
        // on_session_start is emitted by the extension loader once
        // subscribers exist — same moment, same bus as before the port.
        // (When the actor's `await_extensions=false` waiter lands (B1) this
        // becomes `None` — the actor emits it.)
        synaps_cli::extensions::loader::spawn_discover_and_load(
            std::sync::Arc::clone(ctx.ext_mgr_shared.as_ref().expect("local mode")),
            ctx.app.extension_loader_tx.clone(),
            Some(session_id_for_hook),
        );
    }
    Ok(ctx)
}

/// Build `App` from the attach snapshot — the same lines and texts the
/// `EngineBoot`-based construction produced (`continued` branch included).
pub(crate) fn app_from_snapshot(snapshot: &AttachSnapshot) -> App {
    let conv = &snapshot.conversation;
    let session = app::session_from_header(&conv.header);
    let mut app = if snapshot.meta.continued {
        let mut app = App::new_with_clock(session, clock::TuiClock::real());
        app.apply_conversation(conv);
        // mem::take avoids deep-cloning the full history just to satisfy
        // the borrow checker (P5 in REVIEW.md).
        let msgs = std::mem::take(&mut app.api_messages);
        rebuild_display_messages(&msgs, &mut app);
        app.api_messages = msgs;
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
        }
        if app.abort_context.is_some() {
            app.push_msg(ChatMessage::System(
                "⚠ abort context from previous session will be injected into next message"
                    .to_string(),
            ));
        }
        app
    } else {
        App::new_with_clock(session, clock::TuiClock::real())
    };
    app.last_turn_context_window = snapshot.view.context_window;
    app
}

/// Terminal + render thread + channels — shared by the in-process boot and
/// the socket attach (`run_attached`).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn finish_setup(
    mut app: App,
    transport: Box<dyn ClientTransport>,
    http: LazyHttp,
    mode: TransportMode,
    config: synaps_cli::SynapsConfig,
    registry: std::sync::Arc<synaps_cli::skills::registry::CommandRegistry>,
    keybind_registry: std::sync::Arc<
        std::sync::RwLock<synaps_cli::skills::keybinds::KeybindRegistry>,
    >,
    system_prompt_path: std::path::PathBuf,
    ext_mgr_shared: Option<
        std::sync::Arc<tokio::sync::RwLock<synaps_cli::extensions::manager::ExtensionManager>>,
    >,
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
    ladder_stage("terminal", &{ let (c, r) = crossterm::terminal::size().unwrap_or((0, 0)); format!("cols={c} rows={r}") });

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
        signals::SignalBackend::for_socket(matches!(mode, TransportMode::Socket)),
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
        mode,
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
        ext_mgr_shared,
        boot_fx_sent,
        exit_fx_sent,
        last_draw,
    })
}

/// The session envelope stream ended. In-process: the actor is gone — leave.
/// Socket (A4): reconnect with backoff and re-mirror; `false` = give up.
pub(crate) async fn try_reconnect(
    app: &mut App,
    link: &mut SessionLink,
    mode: &TransportMode,
) -> bool {
    match mode {
        TransportMode::Local { .. } => false,
        TransportMode::Socket => {
            let attach_mode = link.mode();
            match link.transport_mut().reconnect(attach_mode).await {
                Ok(snapshot) => {
                    link.refresh_view();
                    remirror(app, &snapshot);
                    app.toasts.upsert(
                        toast::Toast::new("reload", "reconnected").titled("Daemon"),
                    );
                    app.request_redraw();
                    true
                }
                Err(e) => {
                    app.push_msg(ChatMessage::Error(format!("connection lost: {e}")));
                    app.request_redraw();
                    false
                }
            }
        }
    }
}

/// Rebuild the transcript from an `AttachSnapshot` (reconnect / re-attach):
/// the cancelled turn's partial text is gone by design (§2.8 step 7).
pub(crate) fn remirror(app: &mut App, snapshot: &AttachSnapshot) {
    app.transcript.clear();
    app.invalidate();
    app.apply_conversation(&snapshot.conversation);
    let msgs = std::mem::take(&mut app.api_messages);
    rebuild_display_messages(&msgs, &mut *app);
    app.api_messages = msgs;
    app.streaming = snapshot.streaming;
    app.last_turn_context_window = snapshot.view.context_window;
}
