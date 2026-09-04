use anyhow::Context;
use axum::extract::Query;
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use chrono::Local;
use futures::{SinkExt, StreamExt};
use rand::Rng;
use std::collections::HashMap;
use std::sync::Arc;
use synaps_cli::core::config::load_config;
use synaps_cli::core::config::resolve_write_path;
use synaps_cli::engine::commands::{self as engine_commands, CommandResult};
use synaps_cli::engine::reactor::{drain_event_queue, wake_action, WakeAction, AUTO_TURN_CAP};
use synaps_cli::engine::session::ConversationState;
use synaps_cli::engine::setup::{self, BackgroundTasks, EngineOpts};
use synaps_cli::engine::stream::{self, EngineStreamEvent, StreamCompletion, SubagentTracker};
use synaps_cli::protocol::{ClientMessage, HistoryEntry, ServerMessage};
use synaps_cli::{truncate_str, CancellationToken, Runtime};
use tokio::sync::{broadcast, Mutex, RwLock};

/// Shared server state
struct ServerState {
    runtime: Mutex<Runtime>,
    allowed_origins: Vec<String>,
    /// Engine-level conversation state — single source of truth for
    /// session, api_messages, token counters (including cache_read /
    /// cache_creation), cost, abort_context, queued_message,
    /// pending_events. Replaces 5 separate RwLocks that diverged from
    /// engine pricing and silently dropped cache tokens.
    conv: RwLock<ConversationState>,
    display_history: RwLock<Vec<HistoryEntry>>,
    streaming: std::sync::atomic::AtomicBool,
    cancel_token: RwLock<Option<CancellationToken>>,
    /// Broadcast channel — server events go to ALL connected clients
    broadcast_tx: broadcast::Sender<ServerMessage>,
    client_count: RwLock<usize>,
    /// Background tasks from engine boot — kept alive for server lifetime.
    /// Aborts on drop (inbox watcher, per-session socket listener).
    #[allow(dead_code)] // held for RAII; tasks tear down when ServerState drops
    background: BackgroundTasks,
    /// Resolved auth token — None means skip auth (backward compat).
    auth_token: Option<String>,
    /// Maximum inbound WebSocket message size in bytes. None = no cap.
    max_message_size: Option<usize>,
    /// Auto-approve confirm hooks (headless/agent mode).
    auto_approve_confirms: bool,
    /// Auto-turn is on by default (`events.auto_turn` defaults to `true`).
    /// Set `events.auto_turn = false` (or `0` / `no`) to opt out.
    ///
    /// POLICY: broadcasts ALWAYS happen (C3: removed). The auto-turn is gated on this flag.
    events_auto_turn: bool,
    /// Internal channel: event drainer signals the auto-turn listener task
    /// when conditions are met. Unit trigger — the event content is already
    /// in conv.api_messages from the drainer.
    auto_turn_tx: tokio::sync::mpsc::Sender<()>,
    /// Counts consecutive event-triggered model turns without an intervening
    /// real client user message. Reset to 0 on every real user message.
    /// Enforces AUTO_TURN_CAP — see reactor::AUTO_TURN_CAP.
    consecutive_auto_turns: std::sync::atomic::AtomicU32,
}

/// RAII guard that clears the streaming flag on drop.
///
/// Without this, a panic or task cancellation inside the stream loop would
/// leave `streaming = true` forever, bricking the server until process
/// restart. The guard ensures the flag clears on every exit path —
/// happy completion, error, panic, future-drop — without manual cleanup
/// at every return site.
///
/// `cancel_token` is intentionally not part of this guard: a stale token
/// in `cancel_token` is harmless (the stream it referenced is dead, and
/// the next user message overwrites the slot before any new stream
/// starts). The streaming flag is the safety-critical one.
struct StreamingGuard {
    state: Arc<ServerState>,
}

impl Drop for StreamingGuard {
    fn drop(&mut self) {
        self.state
            .streaming
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

impl ServerState {
    fn timestamp() -> String {
        Local::now().format("%H:%M").to_string()
    }

    /// Add usage from a stream's Usage event. Delegates to ConversationState
    /// which uses engine::pricing::calculate_cost (handles cache tokens
    /// correctly and tracks every model the engine knows about — opus,
    /// sonnet, haiku, plus future ones added to engine pricing).
    #[allow(clippy::too_many_arguments)]
    async fn add_usage(
        &self,
        input_tokens: u64,
        output_tokens: u64,
        cache_read: u64,
        cache_creation: u64,
        cache_creation_5m: Option<u64>,
        cache_creation_1h: Option<u64>,
        model: &str,
    ) {
        let mut conv = self.conv.write().await;
        conv.add_usage(
            input_tokens,
            output_tokens,
            cache_read,
            cache_creation,
            cache_creation_5m,
            cache_creation_1h,
            model,
        );
    }

    /// Save the conversation to disk.
    ///
    /// Reproduces ConversationState::save inline so we can release the
    /// `conv` write-lock BEFORE the slow `Session::save().await`
    /// (atomic file rename). Holding the conv lock across that I/O
    /// would block every other state read — particularly the stream
    /// loop's `process_stream_event` write — for the duration of the
    /// disk write.
    async fn save_session(&self) {
        let session_to_save = {
            let mut conv = self.conv.write().await;
            if conv.api_messages.is_empty() {
                return;
            }
            // Mirror ConversationState::save body — sync conv state into
            // the embedded session struct.
            conv.session.api_messages = conv.api_messages.clone();
            conv.session.total_input_tokens = conv.total_input_tokens;
            conv.session.total_output_tokens = conv.total_output_tokens;
            conv.session.session_cost = conv.session_cost;
            conv.session.abort_context = conv.abort_context.clone();
            conv.session.updated_at = chrono::Utc::now();
            conv.session.auto_title();
            // Clone the session out so we can save it without holding the lock.
            conv.session.clone()
        }; // conv write-lock released here
        if let Err(e) = session_to_save.save().await {
            tracing::error!("Failed to save session: {}", e);
        }
    }

    async fn push_history(&self, entry: HistoryEntry) {
        self.display_history.write().await.push(entry);
    }
}

pub async fn run(
    port: u16,
    host: String,
    system: Option<String>,
    continue_session: Option<Option<String>>,
    profile: Option<String>,
    token_override: Option<String>,
    auto_approve_flag: bool,
    allowed_origins_override: Option<String>,
) -> anyhow::Result<()> {
    // ── Boot via engine ──
    // Replaces ~50 lines of inlined Runtime::new + system prompt + session
    // resolution. Boot gives us:
    //   - config + system prompt loaded
    //   - skills registry built (server doesn't consume the registry handle,
    //     but `load_skill` tool registration is a side effect we keep)
    //   - lazy MCP setup
    //   - session resolved (continue or new)
    //   - inbox watcher + per-session Unix socket + session registry entry
    //   - extension manager constructed (still needs explicit loader spawn below)
    //   - on_session_start hook emitted (no subscribers until extensions loaded)
    //   - session-start index record appended
    let boot = setup::boot(EngineOpts {
        continue_session,
        system,
        prompt_manifest: None,
        profile,
        no_extensions: false,
    })
    .await
    .context("engine boot failed")?;

    // ── Discover and load extensions ──
    // setup::boot constructs an empty ExtensionManager. We have to actually
    // discover plugins from disk and spawn their processes here, mirroring
    // cmd/chat.rs. Without this, on_session_start fires onto an empty bus
    // and no before_tool_call / before_message / on_session_end hooks ever
    // run in server mode — defeating the purpose of the engine refactor.
    let session_id_for_hook = boot.session.id.clone();
    let (loader_tx, mut loader_rx) = tokio::sync::mpsc::unbounded_channel();
    synaps_cli::extensions::loader::spawn_discover_and_load(
        Arc::clone(&boot.ext_manager),
        loader_tx,
        Some(session_id_for_hook.clone()),
    );
    // Drain loader events in the background — prevents SendError in the loader
    // and lets us log when discovery completes.
    tokio::spawn(async move {
        use synaps_cli::extensions::loader::ExtensionLoaderEvent;
        while let Some(ev) = loader_rx.recv().await {
            if let ExtensionLoaderEvent::Finished { loaded, failed } = ev {
                tracing::info!(
                    server_extensions_loaded = loaded.len(),
                    server_extensions_failed = failed.len(),
                    "server: extensions ready"
                );
            }
        }
    });

    let runtime = boot.runtime;
    let initial_history = rebuild_history(&boot.api_messages);
    let conv = if boot.continued {
        ConversationState::from_resumed(boot.session)
    } else {
        ConversationState::new(boot.session)
    };

    let session_id = conv.session.id.clone();
    let (broadcast_tx, _) = broadcast::channel::<ServerMessage>(256);

    // ── Resolve auth token + server config ──
    let config = load_config();
    // CLI overrides take precedence over config file values.
    let allowed_origins = if let Some(ref origins) = allowed_origins_override {
        origins
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        config.server.allowed_origins.clone()
    };
    let max_message_size = config.server.max_message_size;
    let auto_approve_confirms = auto_approve_flag || config.server.auto_approve_confirms;
    let auth_token: Option<String> = match &token_override {
        // --token "" disables auth entirely
        Some(t) if t.is_empty() => None,
        // --token <value> uses that value
        Some(t) => Some(t.clone()),
        // No CLI override — use config or auto-generate
        None => {
            let token = if let Some(t) = config.server.token {
                t
            } else {
                // Auto-generate a 32-byte random hex token.
                let bytes: [u8; 32] = rand::rng().random();
                bytes.iter().map(|b| format!("{:02x}", b)).collect()
            };
            // Atomic write: temp file → rename.
            let token_path = resolve_write_path("server-token");
            let tmp_path = token_path.with_extension("tmp");
            if let Err(e) = std::fs::write(&tmp_path, &token) {
                eprintln!("Warning: could not write token file: {e}");
            } else {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ =
                        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600));
                }
                let _ = std::fs::rename(&tmp_path, &token_path);
            }
            Some(token)
        }
    };

    let (auto_turn_tx, auto_turn_rx) = tokio::sync::mpsc::channel::<()>(32);

    let events_auto_turn = config.events.auto_turn;

    let state = Arc::new(ServerState {
        runtime: Mutex::new(runtime),
        conv: RwLock::new(conv),
        display_history: RwLock::new(initial_history),
        streaming: std::sync::atomic::AtomicBool::new(false),
        cancel_token: RwLock::new(None),
        broadcast_tx,
        client_count: RwLock::new(0),
        background: boot.background,
        auth_token: auth_token.clone(),
        allowed_origins,
        max_message_size,
        auto_approve_confirms,
        events_auto_turn,
        auto_turn_tx,
        consecutive_auto_turns: std::sync::atomic::AtomicU32::new(0),
    });

    // ── Event drainer task (exactly one per Runtime) ──────────────────────
    //
    // Policy:
    //   * Runtime events are internalized into the single shared conversation.
    //   * No raw event frame is broadcast; clients see the ordinary model stream.
    //   * Auto-turn follows `events.auto_turn` (default true, explicit opt-out).
    //   * Do NOT touch the `streaming` guard from this task — use the
    //     auto_turn_tx channel to hand off to the listener task below.
    //   * conv.pending_events tracks buffered strings while streaming; they
    //     are flushed by the turn loop's AutoTriggerEvents path.
    {
        let state_d = Arc::clone(&state);
        tokio::spawn(async move {
            let eq = {
                let rt = state_d.runtime.lock().await;
                rt.event_queue().clone()
            };
            loop {
                eq.notified().await;

                // Drain + mutate conv fields — brief lock, released before sends.
                let action = {
                    let mut conv = state_d.conv.write().await;
                    let busy = state_d.streaming.load(std::sync::atomic::Ordering::Acquire);
                    // Reborrow through &mut *conv so NLL field-split borrow analysis
                    // can see two disjoint fields (api_messages, pending_events).
                    let conv_ref: &mut ConversationState = &mut *conv;
                    let drained = drain_event_queue(
                        &eq,
                        &mut conv_ref.api_messages,
                        &mut conv_ref.pending_events,
                        busy,
                        None,
                    );
                    let consecutive = state_d
                        .consecutive_auto_turns
                        .load(std::sync::atomic::Ordering::Acquire);
                    wake_action(
                        &drained,
                        &conv_ref.api_messages,
                        busy,
                        state_d.events_auto_turn,
                        consecutive,
                    )
                }; // conv write-lock released

                // Events are internalized into conv.api_messages by drain_event_queue.
                // No raw event broadcast to clients — model turn is the delivery mechanism.

                // If auto_turn enabled + conditions met, signal the listener.
                // Unit signal — event content already in conv.api_messages.
                if action == WakeAction::RunTurn {
                    let _ = state_d.auto_turn_tx.send(()).await;
                }
            }
        });
    }

    // ── Auto-turn listener task ───────────────────────────────────────────
    //
    // B2 FIX: calls `run_injected_event_turn` instead of `handle_user_message`.
    // This avoids injecting a bogus sentinel user message and spurious
    // "already-streaming" broadcasts. The event content is already in
    // conv.api_messages from the drainer; we only need to acquire the streaming
    // guard and run the turn loop.
    {
        let state_l = Arc::clone(&state);
        let mut auto_turn_rx = auto_turn_rx;
        tokio::spawn(async move {
            while auto_turn_rx.recv().await.is_some() {
                tracing::debug!("server: event auto-turn triggered");
                run_injected_event_turn(&state_l).await;
            }
        });
    }

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/health", get(health_handler))
        .with_state(state.clone());

    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    eprintln!("╔══════════════════════════════════════╗");
    eprintln!("║        SynapsCLI Server v0.2         ║");
    eprintln!("╠══════════════════════════════════════╣");
    eprintln!("║  Listening: ws://{}:{:<5}      ║", host, port);
    eprintln!("║  Session:   {:<24}║", &session_id);
    if let Some(ref tok) = auth_token {
        eprintln!("║  Token:     {:<24}║", &tok[..tok.len().min(24)]);
    }
    eprintln!("╚══════════════════════════════════════╝");

    // Serve with graceful shutdown on SIGINT/SIGTERM.
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // ── Graceful teardown ──
    // Mirrors tui/mod.rs teardown with the same two-budget pattern:
    //   STEP 1: save_session first (own timeout) — data safety before hooks.
    //   STEP 2: on_session_end hook emit (own timeout, concurrent) — after save.
    //
    // Without timeouts here a hung extension handler would block `systemctl stop`
    // until systemd's 90 s SIGKILL fires.
    //
    // Timing constants mirror signals.rs (SAVE_TIMEOUT=2s, HOOKS_TIMEOUT=5s).
    const SAVE_TIMEOUT_SECS: u64 = 2;
    const HOOKS_TIMEOUT_SECS: u64 = 5;

    eprintln!("\n↓ graceful shutdown — saving session, firing hooks, unregistering.");

    // STEP 1: Save session — bounded, highest priority.
    match tokio::time::timeout(
        std::time::Duration::from_secs(SAVE_TIMEOUT_SECS),
        state.save_session(),
    )
    .await
    {
        Ok(()) => eprintln!("  ✓ session saved"),
        Err(_) => eprintln!("  ⚠ session save timed out after {}s", SAVE_TIMEOUT_SECS),
    }

    // STEP 2: Fire on_session_end hook — bounded, after save, concurrent dispatch.
    {
        let runtime = state.runtime.lock().await;
        let hook = synaps_cli::extensions::hooks::events::HookEvent::on_session_end(
            &session_id,
            None, // server doesn't preserve a transcript blob — extensions can read api_messages from disk
        );
        match tokio::time::timeout(
            std::time::Duration::from_secs(HOOKS_TIMEOUT_SECS),
            runtime.hook_bus().emit_concurrent(&hook),
        )
        .await
        {
            Ok(_) => eprintln!("  ✓ on_session_end hooks fired"),
            Err(_) => eprintln!(
                "  ⚠ on_session_end hooks timed out after {}s — extensions may not have flushed",
                HOOKS_TIMEOUT_SECS
            ),
        }
    }

    // STEP 3: Bounded observability flush — after axum's graceful drain
    // (no request handler is still producing records) and after hooks.
    // Task 11: telemetry `off` → no writer → `None`, a true no-op.
    // "Flushed" means appended into OS file buffers (no fsync). A timeout
    // prints metadata-only stats and teardown continues — trace loss never
    // fails a clean shutdown.
    {
        let runtime = state.runtime.lock().await;
        match runtime
            .shutdown_observability_async(
                synaps_cli::runtime::telemetry::DEFAULT_SHUTDOWN_FLUSH_TIMEOUT,
            )
            .await
        {
            Some(outcome) if outcome.is_flushed() => eprintln!("  ✓ observability flushed"),
            Some(outcome) => eprintln!(
                "  ⚠ observability flush timed out — detached worker keeps draining ({:?})",
                outcome.stats()
            ),
            None => {} // telemetry off — nothing to flush
        }
    }

    state.background.shutdown();

    Ok(())
}

/// Listen for SIGINT (Ctrl-C) or SIGTERM (`systemctl stop`) and resolve
/// when either arrives. axum's `with_graceful_shutdown` takes a future
/// that, when ready, signals the server to stop accepting new connections,
/// drain in-flight requests, and return.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::warn!("failed to install SIGTERM handler: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received SIGINT, shutting down"),
        _ = terminate => tracing::info!("received SIGTERM, shutting down"),
    }
}

async fn health_handler() -> impl IntoResponse {
    "ok"
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<ServerState>>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // Origin validation — reject non-matching origins when allowlist is configured.
    if !state.allowed_origins.is_empty() {
        let origin = headers
            .get(axum::http::header::ORIGIN)
            .and_then(|v| v.to_str().ok());
        match origin {
            Some(o) if state.allowed_origins.iter().any(|a| a == o) => {}
            _ => {
                tracing::warn!(
                    origin = ?headers.get(axum::http::header::ORIGIN).map(|v| v.to_str().unwrap_or("<invalid>")),
                    "WebSocket upgrade rejected: origin not in allowlist"
                );
                return (StatusCode::FORBIDDEN, "Forbidden: origin not allowed").into_response();
            }
        }
    }

    // Token auth — validate ?token=X or Authorization: Bearer X.
    if let Some(ref expected) = state.auth_token {
        let provided = params.get("token").map(|s| s.as_str()).or_else(|| {
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
        });

        let valid = match provided {
            Some(tok) => {
                // Constant-time comparison to prevent timing attacks.
                let a = tok.as_bytes();
                let b = expected.as_bytes();
                a.len() == b.len()
                    && a.iter()
                        .zip(b.iter())
                        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
                        == 0
            }
            None => false,
        };

        if !valid {
            tracing::warn!("WebSocket upgrade rejected: invalid or missing auth token");
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    }

    ws.on_upgrade(|socket| handle_client(socket, state))
        .into_response()
}

async fn handle_client(socket: WebSocket, state: Arc<ServerState>) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Register client
    {
        let mut count = state.client_count.write().await;
        *count += 1;
        let n = *count;
        tracing::info!("Client connected ({} total)", n);

        // Notify all clients
        let _ = state.broadcast_tx.send(ServerMessage::System {
            message: format!("client connected ({} total)", n),
        });
    }

    // Subscribe to broadcast
    let mut broadcast_rx = state.broadcast_tx.subscribe();

    // Task: forward broadcast messages → this client's WebSocket
    //
    // `while let Ok(msg) = ...` would silently exit on RecvError::Lagged
    // (slow client falls behind the 256-buffer ring), leaving the WS
    // reader loop alive but the writer dead — a zombie connection that
    // looks healthy from the outside. Match explicitly so we keep the
    // forward pipe alive and only break on a real Closed.
    let tx_handle = tokio::spawn(async move {
        loop {
            match broadcast_rx.recv().await {
                Ok(msg) => {
                    if let Ok(json) = serde_json::to_string(&msg) {
                        if ws_tx.send(Message::Text(json)).await.is_err() {
                            break;
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        dropped = n,
                        "client lagged on broadcast channel — keeping forward pipe alive"
                    );
                    // Best-effort: tell the client they missed messages.
                    let warn = ServerMessage::System {
                        message: format!("[client lagged — {} message(s) dropped]", n),
                    };
                    if let Ok(json) = serde_json::to_string(&warn) {
                        let _ = ws_tx.send(Message::Text(json)).await;
                    }
                    // Continue the loop — receiver remains usable after Lagged.
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    });

    // Main loop: receive messages from this client
    while let Some(Ok(msg)) = ws_rx.next().await {
        match msg {
            Message::Text(text) => {
                // Message size cap — reject oversized payloads before deserialization.
                if let Some(max) = state.max_message_size {
                    let len = text.len();
                    if len > max {
                        tracing::warn!(len, max, "inbound message too large — dropping");
                        let _ = state.broadcast_tx.send(ServerMessage::Error {
                            message: format!(
                                "Message too large: {} bytes exceeds limit of {} bytes",
                                len, max
                            ),
                        });
                        continue;
                    }
                }
                if let Ok(client_msg) = serde_json::from_str::<ClientMessage>(&text) {
                    handle_message(client_msg, &state).await;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    // Client disconnected
    tx_handle.abort();
    {
        let mut count = state.client_count.write().await;
        *count = count.saturating_sub(1);
        let n = *count;
        tracing::info!("Client disconnected ({} remaining)", n);
        let _ = state.broadcast_tx.send(ServerMessage::System {
            message: format!("client disconnected ({} remaining)", n),
        });
    }
}

async fn handle_message(msg: ClientMessage, state: &Arc<ServerState>) {
    match msg {
        ClientMessage::Message { content } => {
            handle_user_message(content, state).await;
        }
        ClientMessage::Command { name, args } => {
            handle_command(&name, &args, state).await;
        }
        ClientMessage::Cancel => {
            let token = state.cancel_token.read().await;
            if let Some(ref ct) = *token {
                ct.cancel();
            }
            let _ = state.broadcast_tx.send(ServerMessage::System {
                message: "canceled".to_string(),
            });
        }
        ClientMessage::Status => {
            // Snapshot all status fields under a single conv read-lock to avoid
            // tearing across multiple awaits and to release the runtime mutex
            // before we do anything expensive (broadcast send).
            let runtime = state.runtime.lock().await;
            let model = runtime.model().to_string();
            let thinking = runtime.thinking_level().to_string();
            drop(runtime);
            let conv = state.conv.read().await;
            let _ = state.broadcast_tx.send(ServerMessage::StatusResponse {
                model,
                thinking,
                streaming: state.streaming.load(std::sync::atomic::Ordering::Acquire),
                session_id: conv.session.id.clone(),
                total_input_tokens: conv.total_input_tokens,
                total_output_tokens: conv.total_output_tokens,
                session_cost: conv.session_cost,
                connected_clients: *state.client_count.read().await,
            });
        }
        ClientMessage::History => {
            let history = state.display_history.read().await;
            let _ = state.broadcast_tx.send(ServerMessage::HistoryResponse {
                messages: history.clone(),
            });
        }
    }
}

async fn handle_user_message(content: String, state: &Arc<ServerState>) {
    // Reset consecutive auto-turn counter — real user message breaks the chain.
    state
        .consecutive_auto_turns
        .store(0, std::sync::atomic::Ordering::Release);

    // Atomic check-then-set: if `streaming` was already true, reject.
    // AcqRel gives us happens-before ordering on the flag toggle without the
    // cross-thread sync overhead of SeqCst, which we don't need for a single
    // boolean flag. Replaces a previous read+write split that allowed two
    // concurrent clients to slip through.
    if state
        .streaming
        .swap(true, std::sync::atomic::Ordering::AcqRel)
    {
        let _ = state.broadcast_tx.send(ServerMessage::Error {
            message: "already streaming — cancel first or wait".to_string(),
        });
        return;
    }
    // RAII: clears `streaming` on every return path, including panic.
    let _streaming_guard = StreamingGuard {
        state: Arc::clone(state),
    };

    // Add to history
    let ts = ServerState::timestamp();
    state
        .push_history(HistoryEntry::User {
            content: content.clone(),
            time: ts,
        })
        .await;

    // Push initial user message into conv.api_messages (single source of truth).
    {
        let mut conv = state.conv.write().await;
        conv.api_messages.push(std::sync::Arc::new(
            serde_json::json!({"role": "user", "content": content}),
        ));
    }

    // Server-local subagent tracker — chat.rs has the same. queued_message
    // and pending_events are NOT local; they live in ConversationState
    // because process_stream_event needs to mutate them across multiple
    // stream calls in a single user-message handling cycle (AutoSendQueued
    // and AutoTriggerEvents both produce follow-up turns).
    let mut subagents: Vec<SubagentTracker> = Vec::new();

    let model = {
        let rt = state.runtime.lock().await;
        rt.model().to_string()
    };
    let broadcast = state.broadcast_tx.clone();

    // Outer turn loop — runs until StreamCompletion::Done or Error.
    // AutoSendQueued and AutoTriggerEvents both push another user-style
    // entry into conv.api_messages and continue the loop, mirroring how
    // cmd/chat.rs handles the same completions. Without this loop, the
    // queued message and pending events would sit in api_messages with
    // no follow-up turn — the next real user message would ship malformed
    // history to the API.
    'turn: loop {
        // Snapshot messages and set up a fresh cancel token for this turn.
        // Vec<SharedMessage> clone = pointer bumps only.
        let messages: Vec<synaps_cli::SharedMessage> = state.conv.read().await.api_messages.clone();
        // Failure repair may only remove messages appended by this turn.
        let turn_baseline = messages.len();
        let cancel = CancellationToken::new();
        *state.cancel_token.write().await = Some(cancel.clone());

        let mut stream = {
            let rt = state.runtime.lock().await;
            rt.run_stream_with_messages(messages, cancel, None, None, state.auto_approve_confirms)
                .await
        };

        // Inner loop — process events from this turn's stream.
        while let Some(event) = stream.next().await {
            let ts = ServerState::timestamp();

            // process_stream_event mutates conv fields in place. Hold the
            // write lock only for the call itself, then release before
            // broadcast / display_history work to keep latency low.
            let (engine_event, completion) = {
                let mut conv = state.conv.write().await;
                let conv = &mut *conv;
                stream::process_stream_event(
                    event,
                    &mut conv.api_messages,
                    &mut subagents,
                    &mut conv.queued_message,
                    &mut conv.pending_events,
                    turn_baseline,
                )
            };

            apply_engine_event_side_effects(&engine_event, state, &model, &ts).await;

            if let Some(msg) = engine_event_to_server_message(engine_event) {
                let _ = broadcast.send(msg);
            }

            match completion {
                StreamCompletion::Continue => {}
                StreamCompletion::Done => {
                    state.save_session().await;
                    break 'turn;
                }
                StreamCompletion::Error(ref err) => {
                    // process_stream_event has already repaired the history
                    // (turn-appended invalid messages only) and emitted
                    // EngineStreamEvent::Error which we translated to a
                    // ServerMessage::Error above. Log the typed terminal
                    // category + correlation ID for traceability.
                    tracing::debug!(
                        error = %err.message,
                        outcome = %err.category_label(),
                        "stream completed with error"
                    );
                    state.save_session().await;
                    break 'turn;
                }
                StreamCompletion::AutoSendQueued(queued) => {
                    // Take the queued user message out of conv (process_stream_event
                    // already cleared the option in conv) and push it as the
                    // next user turn. Then save and continue the outer loop.
                    {
                        let mut conv = state.conv.write().await;
                        conv.api_messages.push(std::sync::Arc::new(
                            serde_json::json!({"role": "user", "content": queued}),
                        ));
                    }
                    state.save_session().await;
                    continue 'turn;
                }
                StreamCompletion::AutoTriggerEvents => {
                    // Issue 3 fix: gate on events_auto_turn config; enforce counter+cap.
                    // handle_user_message already reset the counter to 0 at entry, but
                    // subsequent auto-trigger iterations within this same user message
                    // handling must still be bounded.
                    if !state.events_auto_turn {
                        state.save_session().await;
                        break 'turn;
                    }
                    let cur = state
                        .consecutive_auto_turns
                        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                    if cur >= AUTO_TURN_CAP {
                        tracing::info!(
                            cap = AUTO_TURN_CAP,
                            "server: auto-turn cap reached (handle_user_message AutoTriggerEvents) — parking"
                        );
                        state.save_session().await;
                        break 'turn;
                    }
                    // pending_events were already drained into conv.api_messages
                    // by process_stream_event. Save and trigger a follow-up turn.
                    state.save_session().await;
                    continue 'turn;
                }
            }
        }

        // Stream ended without an explicit completion (network drop or
        // similar). Save and exit — don't loop forever waiting for events
        // that won't come.
        state.save_session().await;
        break 'turn;
    }

    // _streaming_guard's Drop clears `streaming` — no manual store needed.
    *state.cancel_token.write().await = None;
}

/// Apply event-specific side effects: append to `display_history`
/// (the replay buffer for late-connecting WS clients) and bump the
/// engine-managed usage counters when a `Usage` event arrives.
///
/// This is a *separate* function from `engine_event_to_server_message`
/// for an ownership reason, not a logical-split reason: that function
/// consumes the `EngineStreamEvent` by value to build a `ServerMessage`,
/// so any work that needs `&EngineStreamEvent` must run first while
/// the event is still borrowable. The split is dictated by Rust's
/// borrow checker; if `EngineStreamEvent` becomes `Clone` cheaply,
/// these can collapse.
async fn apply_engine_event_side_effects(
    event: &EngineStreamEvent,
    state: &Arc<ServerState>,
    model: &str,
    ts: &str,
) {
    match event {
        EngineStreamEvent::Thinking(text) => {
            let mut history = state.display_history.write().await;
            if let Some(HistoryEntry::Thinking { content: c, .. }) = history.last_mut() {
                c.push_str(text);
            } else {
                history.push(HistoryEntry::Thinking {
                    content: text.clone(),
                    time: ts.to_string(),
                });
            }
        }
        EngineStreamEvent::Text(text) => {
            let mut history = state.display_history.write().await;
            if let Some(HistoryEntry::Text { content: c, .. }) = history.last_mut() {
                c.push_str(text);
            } else {
                history.push(HistoryEntry::Text {
                    content: text.clone(),
                    time: ts.to_string(),
                });
            }
        }
        EngineStreamEvent::ToolFinalized {
            tool_name, input, ..
        } => {
            // HistoryEntry::ToolUse.input is `String` for display purposes;
            // engine emits `Value` now, so serialise here.
            let input_str = serde_json::to_string(input).unwrap_or_default();
            state
                .push_history(HistoryEntry::ToolUse {
                    tool_name: tool_name.clone(),
                    input: input_str,
                    time: ts.to_string(),
                })
                .await;
        }
        EngineStreamEvent::ToolResult { result, .. } => {
            state
                .push_history(HistoryEntry::ToolResult {
                    result: result.clone(),
                    time: ts.to_string(),
                })
                .await;
        }
        EngineStreamEvent::Usage {
            input_tokens,
            output_tokens,
            cache_read,
            cache_creation,
            cache_creation_5m,
            cache_creation_1h,
            model: _event_model,
        } => {
            state
                .add_usage(
                    *input_tokens,
                    *output_tokens,
                    *cache_read,
                    *cache_creation,
                    *cache_creation_5m,
                    *cache_creation_1h,
                    model,
                )
                .await;
        }
        EngineStreamEvent::Error(err) => {
            state
                .push_history(HistoryEntry::Error {
                    content: err.clone(),
                    time: ts.to_string(),
                })
                .await;
        }
        // Notices are displayable — persist them so reconnecting clients see
        // e.g. the cache-TTL downgrade warning in history.
        EngineStreamEvent::Notice(text) => {
            state
                .push_history(HistoryEntry::System {
                    content: text.clone(),
                    time: ts.to_string(),
                })
                .await;
        }
        // Variants without server-side side effects.
        EngineStreamEvent::ToolStart { .. }
        | EngineStreamEvent::ToolDelta { .. }
        | EngineStreamEvent::ToolResultDelta { .. }
        | EngineStreamEvent::SubagentStart { .. }
        | EngineStreamEvent::SubagentUpdate { .. }
        | EngineStreamEvent::SubagentDone { .. }
        | EngineStreamEvent::SteeringDelivered { .. }
        | EngineStreamEvent::Done
        | EngineStreamEvent::Noop => {}
    }
}

/// Translate an engine-level event to the wire-format ServerMessage.
/// Returns None for events that have no client-facing representation
/// (subagent / steering / noop — TODO: wire subagent variant in v2).
fn engine_event_to_server_message(event: EngineStreamEvent) -> Option<ServerMessage> {
    match event {
        EngineStreamEvent::Thinking(content) => Some(ServerMessage::Thinking { content }),
        EngineStreamEvent::Text(content) => Some(ServerMessage::Text { content }),
        EngineStreamEvent::ToolStart { tool_name, .. } => {
            Some(ServerMessage::ToolUseStart { tool_name })
        }
        EngineStreamEvent::ToolDelta { delta, .. } => Some(ServerMessage::ToolUseDelta(delta)),
        EngineStreamEvent::ToolFinalized {
            tool_id,
            tool_name,
            input,
        } => {
            // Engine emits Value directly now — pass through without
            // the previous Value→String→Value round-trip that could
            // silently corrupt input on serialisation failure.
            Some(ServerMessage::ToolUse {
                tool_name,
                tool_id,
                input,
            })
        }
        EngineStreamEvent::ToolResultDelta { tool_id, delta } => {
            Some(ServerMessage::ToolResultDelta { tool_id, delta })
        }
        EngineStreamEvent::ToolResult { tool_id, result } => {
            Some(ServerMessage::ToolResult { tool_id, result })
        }
        EngineStreamEvent::Usage {
            input_tokens,
            output_tokens,
            cache_read: _cache_read,
            cache_creation: _cache_creation,
            cache_creation_5m,
            cache_creation_1h,
            model: _model,
        } => Some(ServerMessage::Usage {
            input_tokens,
            output_tokens,
            cache_creation_5m,
            cache_creation_1h,
        }),
        EngineStreamEvent::Done => Some(ServerMessage::Done),
        EngineStreamEvent::Error(message) => Some(ServerMessage::Error { message }),
        // Was silently dropped — the cache-TTL downgrade warning (and any
        // future advisory) never reached server-mode clients.
        EngineStreamEvent::Notice(text) => Some(ServerMessage::Notice { text }),
        // Server protocol doesn't expose these (yet).
        EngineStreamEvent::SubagentStart { .. }
        | EngineStreamEvent::SubagentUpdate { .. }
        | EngineStreamEvent::SubagentDone { .. }
        | EngineStreamEvent::SteeringDelivered { .. }
        | EngineStreamEvent::Noop => None,
    }
}

/// Run a model turn driven by an injected runtime event (auto-turn path).
///
/// B2 FIX: Unlike `handle_user_message`, this function does NOT push a sentinel
/// user message into history or conv.api_messages — the event content is already
/// there from the drainer. It atomically acquires the streaming guard (silently
/// dropping the trigger if a real stream is already active), increments the
/// consecutive auto-turn counter, runs the turn loop, then resets on cap.
///
/// The streaming guard check here is the safety-critical piece: if a real client
/// message arrived between the drainer signalling and this function running, the
/// streaming flag will already be true and we drop the auto-turn silently.
/// The event content remains in api_messages and will be processed with the
/// client-driven turn.
async fn run_injected_event_turn(state: &Arc<ServerState>) {
    // Guard: if already streaming, event stays in api_messages for the active turn.
    if state
        .streaming
        .swap(true, std::sync::atomic::Ordering::AcqRel)
    {
        tracing::debug!(
            "server: auto-turn skipped — stream already active; event stays in history"
        );
        return;
    }
    // RAII: clears `streaming` on every return path.
    let _streaming_guard = StreamingGuard {
        state: Arc::clone(state),
    };

    // Guard: last api_messages entry must be role=user (injected event content).
    // Stale signals (e.g. fired after a user turn already consumed the message)
    // are silently parked — no turn started.
    {
        let conv = state.conv.read().await;
        let last_role = conv.api_messages.last().and_then(|m| m["role"].as_str());
        if last_role != Some("user") {
            tracing::debug!("server: auto-turn parked — last message is not role=user");
            return;
        }
    }

    // Increment counter; bail (saturate, do NOT reset) if cap reached.
    // Only handle_user_message resets the counter so the cap stays latched
    // until real user input arrives — preventing a looping event source from
    // immediately re-saturating after a reset.
    let prev = state
        .consecutive_auto_turns
        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    if prev >= AUTO_TURN_CAP {
        // Saturate at cap — leave counter elevated so further signals are
        // also discarded. handle_user_message is the only reset path.
        tracing::info!(
            cap = AUTO_TURN_CAP,
            "server: auto-turn cap reached — parking until real user input"
        );
        return;
    }

    let mut subagents: Vec<SubagentTracker> = Vec::new();
    let model = {
        let rt = state.runtime.lock().await;
        rt.model().to_string()
    };
    let broadcast = state.broadcast_tx.clone();

    // Turn loop — mirrors handle_user_message's loop but no history entry
    // and no initial message push (event is already in api_messages).
    'turn: loop {
        let messages: Vec<synaps_cli::SharedMessage> = state.conv.read().await.api_messages.clone();
        // Failure repair may only remove messages appended by this turn.
        let turn_baseline = messages.len();
        let cancel = CancellationToken::new();
        *state.cancel_token.write().await = Some(cancel.clone());

        let mut stream = {
            let rt = state.runtime.lock().await;
            rt.run_stream_with_messages(messages, cancel, None, None, state.auto_approve_confirms)
                .await
        };

        while let Some(event) = stream.next().await {
            let ts = ServerState::timestamp();

            let (engine_event, completion) = {
                let mut conv = state.conv.write().await;
                let conv = &mut *conv;
                stream::process_stream_event(
                    event,
                    &mut conv.api_messages,
                    &mut subagents,
                    &mut conv.queued_message,
                    &mut conv.pending_events,
                    turn_baseline,
                )
            };

            apply_engine_event_side_effects(&engine_event, state, &model, &ts).await;

            if let Some(msg) = engine_event_to_server_message(engine_event) {
                let _ = broadcast.send(msg);
            }

            match completion {
                StreamCompletion::Continue => {}
                StreamCompletion::Done => {
                    state.save_session().await;
                    break 'turn;
                }
                StreamCompletion::Error(ref err) => {
                    tracing::debug!(
                        error = %err.message,
                        outcome = %err.category_label(),
                        "auto-turn stream completed with error"
                    );
                    state.save_session().await;
                    break 'turn;
                }
                StreamCompletion::AutoSendQueued(queued) => {
                    {
                        let mut conv = state.conv.write().await;
                        conv.api_messages.push(std::sync::Arc::new(
                            serde_json::json!({"role": "user", "content": queued}),
                        ));
                    }
                    state.save_session().await;
                    continue 'turn;
                }
                StreamCompletion::AutoTriggerEvents => {
                    // Issue 3 fix: gate on events_auto_turn and enforce cap.
                    // Counter was already incremented at function entry. Check again
                    // before spinning another turn — if cap is reached, park.
                    if !state.events_auto_turn {
                        state.save_session().await;
                        break 'turn;
                    }
                    let cur = state
                        .consecutive_auto_turns
                        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                    if cur >= AUTO_TURN_CAP {
                        // Saturate — leave counter elevated until real user input.
                        tracing::info!(
                            cap = AUTO_TURN_CAP,
                            "server: auto-turn cap reached (AutoTriggerEvents) — parking"
                        );
                        state.save_session().await;
                        break 'turn;
                    }
                    state.save_session().await;
                    continue 'turn;
                }
            }
        }

        state.save_session().await;
        break 'turn;
    }

    *state.cancel_token.write().await = None;
}

async fn handle_command(name: &str, args: &str, state: &Arc<ServerState>) {
    let broadcast = &state.broadcast_tx;

    // Server-specific overrides — handled BEFORE engine to preserve
    // existing wire behaviour for empty-arg display queries that the
    // engine treats as no-ops.
    //
    // `/thinking adaptive` previously needed an override here too, but
    // engine::commands now knows the `adaptive` level natively (matches
    // the runtime's own label for budget=0), so it routes through the
    // engine path below. One less special case to drift.
    if name == "model" && args.is_empty() {
        let rt = state.runtime.lock().await;
        let _ = broadcast.send(ServerMessage::System {
            message: format!("current model: {}", rt.model()),
        });
        return;
    }
    if name == "thinking" && args.is_empty() {
        let rt = state.runtime.lock().await;
        let _ = broadcast.send(ServerMessage::System {
            message: format!(
                "thinking: {} ({})",
                rt.thinking_level(),
                rt.thinking_budget()
            ),
        });
        return;
    }

    // Try engine-level command (model with args, thinking with engine-known
    // levels, quit, compact).
    let engine_result = {
        let mut rt = state.runtime.lock().await;
        engine_commands::handle_engine_command(name, args, &mut rt)
    };

    if let Some(result) = engine_result {
        match result {
            CommandResult::ModelChanged {
                model,
                reasoning_clamped,
            } => {
                let mut message = format!("model set to: {model}");
                {
                    let mut conv = state.conv.write().await;
                    conv.session.model = model.clone();
                    if let Some(clamp) = reasoning_clamped {
                        let rt = state.runtime.lock().await;
                        conv.session.thinking_level = rt.thinking_level().to_string();
                        message.push_str(&format!(
                            "; thinking → {} (clamped from {}: not supported by {})",
                            clamp.to.as_str(),
                            clamp.from.as_str(),
                            rt.model()
                        ));
                    }
                }
                let _ = broadcast.send(ServerMessage::System { message });
            }
            CommandResult::ThinkingChanged { spec } => {
                state.conv.write().await.session.thinking_level = spec.config_value();
                let _ = broadcast.send(ServerMessage::System {
                    message: format!("thinking set to: {}", spec.level()),
                });
            }
            CommandResult::Quit => {
                let _ = broadcast.send(ServerMessage::System {
                    message: "/quit ignored — server is long-lived; close the WebSocket instead"
                        .to_string(),
                });
            }
            CommandResult::Compact {
                custom_instructions,
            } => {
                // T30 (spec §9.2): server compaction routes through the ONE
                // engine transition with the in-place policy. Snapshot under
                // brief locks; the LLM round-trip runs with no lock held.
                let (msgs, session, rt) = {
                    let conv = state.conv.read().await;
                    (conv.api_messages.clone(), conv.session.clone(), {
                        let rt = state.runtime.lock().await;
                        rt.clone()
                    })
                };
                if msgs.len() < 4 {
                    let _ = broadcast.send(ServerMessage::System {
                        message: "nothing to compact (need at least 2 turns)".to_string(),
                    });
                    return;
                }
                let _ = broadcast.send(ServerMessage::System {
                    message: "compacting conversation...".to_string(),
                });
                // Spec §9.4: surface provider/model and approximate
                // disclosure BEFORE the summarization dispatch.
                let _ = broadcast.send(ServerMessage::System {
                    message: synaps_cli::runtime::compaction::preview_compaction_disclosure(
                        &rt, &msgs,
                    )
                    .render_line(),
                });
                let applied = match synaps_cli::runtime::compaction::compact_conversation(
                    &msgs,
                    &rt,
                    custom_instructions.as_deref(),
                )
                .await
                {
                    Ok(outcome) => {
                        synaps_cli::runtime::compaction::apply_compaction(
                            &rt,
                            &session,
                            &msgs,
                            &outcome,
                            synaps_cli::runtime::compaction::CompactionTransition {
                                policy: synaps_cli::runtime::compaction::CompactionPolicy::InPlace,
                                pending_events: Vec::new(),
                                queued_message: None,
                                hook_source: "manual".to_string(),
                            },
                        )
                        .await
                    }
                    Err(e) => Err(e),
                };
                match applied {
                    Ok(applied) => {
                        let compacted_from = msgs.len();
                        {
                            let mut conv = state.conv.write().await;
                            conv.session = applied.session;
                            conv.api_messages = applied.api_messages;
                        }
                        let _ = broadcast.send(ServerMessage::System {
                            message: format!(
                                "✓ compacted {} messages into a summary context",
                                compacted_from
                            ),
                        });
                    }
                    Err(e) => {
                        let _ = broadcast.send(ServerMessage::Error {
                            message: format!("compaction failed: {e}"),
                        });
                    }
                }
            }
            CommandResult::Error(msg) => {
                let _ = broadcast.send(ServerMessage::Error { message: msg });
            }
            CommandResult::Output(text) => {
                let _ = broadcast.send(ServerMessage::System { message: text });
            }
            other => {
                tracing::debug!(?other, "engine command result not handled by server");
            }
        }
        return;
    }

    // Server-specific commands the engine doesn't cover.
    match name {
        "clear" => {
            // Delegate to ConversationState::clear, which saves the current
            // session and replaces it with a fresh one. Mirrors how
            // cmd/chat.rs handles /clear.
            {
                let rt = state.runtime.lock().await;
                let mut conv = state.conv.write().await;
                conv.clear(&rt).await;
            }
            state.display_history.write().await.clear();
            let _ = broadcast.send(ServerMessage::System {
                message: "session cleared".to_string(),
            });
        }
        "system" => {
            if args.is_empty() || args == "show" {
                let rt = state.runtime.lock().await;
                let prompt = rt.system_prompt().unwrap_or("(none)");
                let _ = broadcast.send(ServerMessage::System {
                    message: format!("system prompt: {}", truncate_str(prompt, 200)),
                });
            } else {
                let mut rt = state.runtime.lock().await;
                rt.set_system_prompt(args.to_string());
                let _ = broadcast.send(ServerMessage::System {
                    message: "system prompt updated".to_string(),
                });
            }
        }
        _ => {
            let _ = broadcast.send(ServerMessage::Error {
                message: format!("unknown command: {name}"),
            });
        }
    }
}

/// Rebuild the WS replay buffer (`display_history`) from raw API messages
/// after a `--continue` boot.
///
/// Why this stays — the engine has no public API for "give me a
/// display-friendly history view" of api_messages. `display_history` is
/// a server-renderer concern (replay buffer for late-joining WS clients),
/// distinct from the LLM-facing api_messages list. Until the engine
/// exposes an equivalent helper, server has to do the JSON-block
/// decoding here.
///
/// Known limitation: this is a manual `block["type"].as_str()` walk.
/// Adding a new block type to the protocol means updating both
/// `engine::stream::process_stream_event` and this function or losing
/// fidelity in the replay buffer for resumed sessions. Tracked as
/// follow-up — engine should expose a `Session::display_history()`
/// helper.
fn rebuild_history(api_messages: &[synaps_cli::SharedMessage]) -> Vec<HistoryEntry> {
    let mut history = Vec::new();
    for msg in api_messages {
        match msg["role"].as_str() {
            Some("user") => {
                if let Some(content) = msg["content"].as_str() {
                    history.push(HistoryEntry::User {
                        content: content.to_string(),
                        time: String::new(),
                    });
                }
            }
            Some("assistant") => {
                if let Some(content) = msg["content"].as_array() {
                    for block in content {
                        match block["type"].as_str() {
                            Some("thinking") => {
                                if let Some(text) = block["thinking"].as_str() {
                                    history.push(HistoryEntry::Thinking {
                                        content: text.to_string(),
                                        time: String::new(),
                                    });
                                }
                            }
                            Some("text") => {
                                if let Some(text) = block["text"].as_str() {
                                    history.push(HistoryEntry::Text {
                                        content: text.to_string(),
                                        time: String::new(),
                                    });
                                }
                            }
                            Some("tool_use") => {
                                let name = block["name"].as_str().unwrap_or("").to_string();
                                let input =
                                    serde_json::to_string(&block["input"]).unwrap_or_default();
                                history.push(HistoryEntry::ToolUse {
                                    tool_name: name,
                                    input,
                                    time: String::new(),
                                });
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
    history
}
