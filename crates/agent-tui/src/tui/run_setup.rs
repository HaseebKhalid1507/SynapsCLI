//! P12.1: boot/setup prologue extracted from `run()` — PURE CODE MOTION.
//!
//! The body of [`run_setup`] below is byte-identical to `run()`'s former
//! prologue (previously `mod.rs` lines 68-207): engine boot unpack, `App`
//! construction (incl. the resumed-session branch), channel/handle setup,
//! render-thread spawn, and the tick-throttle state init. `run()` now calls
//! this, destructures the returned [`RunContext`], and enters the (unchanged)
//! `tokio::select!` loop. No logic changed.

use super::*;

use std::time::Instant;
use synaps_cli::{CancellationToken, Result, StreamEvent};

/// Everything `run()`'s event loop and teardown consume from the boot
/// prologue. Fields are handed back by value so `run()` owns them exactly as
/// it did when they were locals — the loop's `&mut` borrows are unchanged.
pub(crate) struct RunContext {
    pub app: App,
    pub runtime: synaps_cli::Runtime,
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
    pub stream: Option<std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send>>>,
    pub secret_prompt_handle: synaps_cli::tools::SecretPromptHandle,
    pub secret_prompt_rx: std::sync::Arc<
        std::sync::Mutex<
            tokio::sync::mpsc::UnboundedReceiver<synaps_cli::tools::SecretPromptRequest>,
        >,
    >,
    pub cancel_token: Option<CancellationToken>,
    pub steer_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    pub background: synaps_cli::engine::setup::BackgroundTasks,
    pub ext_mgr_shared:
        std::sync::Arc<tokio::sync::RwLock<synaps_cli::extensions::manager::ExtensionManager>>,
    pub boot_fx_sent: bool,
    pub exit_fx_sent: bool,
    pub last_draw: Instant,
}

/// Boot prologue for [`super::run`]. Same inputs / error type as `run()`.
pub(crate) async fn run_setup(
    continue_session: Option<Option<String>>,
    system: Option<String>,
    profile: Option<String>,
    no_extensions: bool,
) -> Result<RunContext> {
    // ── Engine boot ──
    let boot = synaps_cli::engine::setup::boot(synaps_cli::engine::setup::EngineOpts {
        continue_session: continue_session.clone(),
        system,
        profile,
        no_extensions,
    })
    .await?;

    let runtime = boot.runtime;
    let config = boot.config;
    let registry = boot.registry;
    let keybind_registry = boot.keybind_registry;
    let mcp_server_count = boot.mcp_server_count;
    let system_prompt_path = boot.system_prompt_path;

    // Build App from engine boot results
    let mut app = if boot.continued {
        let mut app = App::new_with_clock(boot.session.clone(), clock::TuiClock::real());
        app.api_messages = boot.api_messages;
        app.total_input_tokens = boot.total_input_tokens;
        app.total_output_tokens = boot.total_output_tokens;
        app.session_cost = boot.session_cost;
        app.abort_context = boot.abort_context;
        // mem::take avoids deep-cloning the full history just to satisfy
        // the borrow checker (P5 in REVIEW.md).
        let msgs = std::mem::take(&mut app.api_messages);
        rebuild_display_messages(&msgs, &mut app);
        app.api_messages = msgs;
        app.push_msg(ChatMessage::System(format!(
            "resumed session {}",
            boot.session.id
        )));
        if let Some(ref info) = boot.continue_info {
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
        App::new_with_clock(boot.session, clock::TuiClock::real())
    };
    app.keybinds = Some(keybind_registry.clone());
    app.last_turn_context_window = runtime.context_window();

    // Surface config parse warnings once at startup (unknown keys, bad values).
    for w in &config.warnings {
        app.push_msg(ChatMessage::System(format!("⚠ config: {}", w)));
    }

    // First-run guidance: no Anthropic credentials and no provider keys means
    // the first message will fail — tell the user up front instead.
    {
        let has_anthropic = synaps_cli::auth::load_auth()
            .ok()
            .flatten()
            .map(|a| a.anthropic.auth_type == "oauth" && !a.anthropic.access.is_empty())
            .unwrap_or(false)
            || std::env::var("ANTHROPIC_API_KEY").is_ok();
        if !has_anthropic && config.provider_keys.is_empty() {
            app.push_msg(ChatMessage::System(
                "👋 No credentials found. To get started:\n   • `synaps login` — sign in with Claude Pro/Max (OAuth)\n   • or set ANTHROPIC_API_KEY in your environment\n   • or add `provider.<name> = <key>` to ~/.synaps-cli/config (groq, openrouter, …) and pick with /model".to_string(),
            ));
        }
    }

    if mcp_server_count > 0 {
        tracing::info!(
            "{} MCP servers available (use connect_mcp_server to activate)",
            mcp_server_count
        );
    }

    // ── Terminal setup + render thread ──
    //
    // The Terminal is moved into the render thread immediately after creation.
    // The main task never touches it again.  All terminal I/O (draw, clear,
    // teardown) goes through `render_handle`.
    //
    // Terminal size for build_render_model: we call crossterm::terminal::size()
    // directly — it reads the TTY fd without needing the Terminal object.
    // See render_thread.rs module comment for the design rationale.
    let terminal = setup_terminal()?;

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
    let term_caps =
        termcaps::negotiate(termcaps::TermCaps::detect(), termcaps::BURST_TIMEOUT).await;

    let (render_handle, boot_done, exit_done) = spawn_render_thread(terminal);
    // Boot effect is sent via the command channel so the render thread owns it.
    render_handle.send_boot_fx(boot_effect());

    let event_reader = EventStream::new();
    let (shutdown_signal_tx, shutdown_signal_rx) = tokio::sync::mpsc::unbounded_channel();
    let shutdown_signal_task = signals::spawn_shutdown_signal_task(shutdown_signal_tx);
    let stream: Option<std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send>>> =
        None;
    let (secret_prompt_tx, secret_prompt_rx) = tokio::sync::mpsc::unbounded_channel();
    let secret_prompt_handle = synaps_cli::tools::SecretPromptHandle::new(secret_prompt_tx);
    let secret_prompt_rx = std::sync::Arc::new(std::sync::Mutex::new(secret_prompt_rx));
    // P7.8: the secret-prompt queue now lives on `app.secret_prompts` (§5).
    // The mpsc channel wiring above is unchanged; only the queue moved onto
    // App so the pane handler / harness share production state.
    let cancel_token: Option<CancellationToken> = None;
    let steer_tx: Option<tokio::sync::mpsc::UnboundedSender<String>> = None;

    // ── Engine-managed background tasks (inbox watcher, socket, extensions) ──
    let background = boot.background;
    let ext_mgr_shared = boot.ext_manager;

    // Legacy sidecar key migration
    loop_arms::migrate_sidecar_toggle_key_to_claimed_plugins(&registry.lifecycle_claims());

    if !boot.no_extensions {
        app.extension_loader_running = true;
        app.toasts.upsert(
            toast::Toast::new("extension-loader", "Discovering extensions…")
                .titled("Extensions")
                .at(toast::ToastPosition::TOP_CENTER)
                .ttl(None),
        );
        synaps_cli::extensions::loader::spawn_discover_and_load(
            std::sync::Arc::clone(&ext_mgr_shared),
            app.extension_loader_tx.clone(),
        );
    }

    // on_session_start hook already fired by engine::setup::boot()

    // ── Event loop ──
    // Track whether the render thread currently has an active boot or exit
    // effect.  The render thread owns the actual Effect values; we track
    // "has been sent and not yet done" on the main side for the tick throttle.
    let boot_fx_sent  = true;  // boot_effect() is sent at startup above
    let exit_fx_sent  = false;
    let last_draw = Instant::now() - std::time::Duration::from_secs(1);

    Ok(RunContext {
        app,
        runtime,
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
        stream,
        secret_prompt_handle,
        secret_prompt_rx,
        cancel_token,
        steer_tx,
        background,
        ext_mgr_shared,
        boot_fx_sent,
        exit_fx_sent,
        last_draw,
    })
}
