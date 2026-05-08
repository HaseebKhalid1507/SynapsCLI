use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::State,
    response::IntoResponse,
    routing::get,
    Router,
};
use chrono::Local;
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use synaps_cli::engine::commands::{self as engine_commands, CommandResult};
use synaps_cli::engine::setup::{self, BackgroundTasks, EngineOpts};
use synaps_cli::engine::stream::{self, EngineStreamEvent, StreamCompletion, SubagentTracker};
use synaps_cli::protocol::{ClientMessage, HistoryEntry, ServerMessage};
use synaps_cli::{truncate_str, CancellationToken, Runtime, Session};
use tokio::sync::{broadcast, Mutex, RwLock};

/// Shared server state
struct ServerState {
    runtime: Mutex<Runtime>,
    session: RwLock<Session>,
    api_messages: RwLock<Vec<serde_json::Value>>,
    display_history: RwLock<Vec<HistoryEntry>>,
    total_input_tokens: RwLock<u64>,
    total_output_tokens: RwLock<u64>,
    session_cost: RwLock<f64>,
    streaming: RwLock<bool>,
    cancel_token: RwLock<Option<CancellationToken>>,
    /// Broadcast channel — server events go to ALL connected clients
    broadcast_tx: broadcast::Sender<ServerMessage>,
    client_count: RwLock<usize>,
    /// Background tasks from engine boot — kept alive for server lifetime.
    /// Aborts on drop (inbox watcher, per-session socket listener).
    #[allow(dead_code)] // held for RAII; tasks tear down when ServerState drops
    background: BackgroundTasks,
}

impl ServerState {
    fn timestamp() -> String {
        Local::now().format("%H:%M").to_string()
    }

    async fn add_usage(&self, input_tokens: u64, output_tokens: u64, model: &str) {
        *self.total_input_tokens.write().await += input_tokens;
        *self.total_output_tokens.write().await += output_tokens;

        let (input_price, output_price) = match model {
            m if m.contains("opus") => (15.0, 75.0),
            m if m.contains("sonnet") => (3.0, 15.0),
            m if m.contains("haiku") => (0.80, 4.0),
            _ => (3.0, 15.0),
        };
        let cost = (input_tokens as f64 / 1_000_000.0) * input_price
            + (output_tokens as f64 / 1_000_000.0) * output_price;
        *self.session_cost.write().await += cost;
    }

    async fn save_session(&self) {
        let api_msgs = self.api_messages.read().await;
        if api_msgs.is_empty() {
            return;
        }
        let mut session = self.session.write().await;
        session.api_messages = api_msgs.clone();
        session.total_input_tokens = *self.total_input_tokens.read().await;
        session.total_output_tokens = *self.total_output_tokens.read().await;
        session.session_cost = *self.session_cost.read().await;
        session.updated_at = chrono::Utc::now();
        session.auto_title();
        if let Err(e) = session.save().await {
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
) -> anyhow::Result<()> {
    // ── Boot via engine ──
    // Replaces ~50 lines of inlined Runtime::new + system prompt + session
    // resolution. Also gains: skills registry, MCP, inbox watcher,
    // per-session socket, on_session_start hook, extension manager,
    // session-start index record.
    let boot = setup::boot(EngineOpts {
        continue_session,
        system,
        profile,
        no_extensions: false,
    })
    .await
    .map_err(|e| anyhow::anyhow!("engine boot failed: {e}"))?;

    let runtime = boot.runtime;
    let session = boot.session;
    let initial_api_messages = boot.api_messages;
    let initial_history = rebuild_history(&initial_api_messages);
    let initial_in = boot.total_input_tokens;
    let initial_out = boot.total_output_tokens;
    let initial_cost = boot.session_cost;

    let session_id = session.id.clone();
    let (broadcast_tx, _) = broadcast::channel::<ServerMessage>(256);

    let state = Arc::new(ServerState {
        runtime: Mutex::new(runtime),
        session: RwLock::new(session),
        api_messages: RwLock::new(initial_api_messages),
        display_history: RwLock::new(initial_history),
        total_input_tokens: RwLock::new(initial_in),
        total_output_tokens: RwLock::new(initial_out),
        session_cost: RwLock::new(initial_cost),
        streaming: RwLock::new(false),
        cancel_token: RwLock::new(None),
        broadcast_tx,
        client_count: RwLock::new(0),
        background: boot.background,
    });

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
    eprintln!("╚══════════════════════════════════════╝");

    axum::serve(listener, app).await?;
    Ok(())
}

async fn health_handler() -> impl IntoResponse {
    "ok"
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<ServerState>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_client(socket, state))
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
    let tx_handle = tokio::spawn(async move {
        while let Ok(msg) = broadcast_rx.recv().await {
            if let Ok(json) = serde_json::to_string(&msg) {
                if ws_tx.send(Message::Text(json)).await.is_err() {
                    break;
                }
            }
        }
    });

    // Main loop: receive messages from this client
    while let Some(Ok(msg)) = ws_rx.next().await {
        match msg {
            Message::Text(text) => {
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
            let runtime = state.runtime.lock().await;
            let session = state.session.read().await;
            let _ = state.broadcast_tx.send(ServerMessage::StatusResponse {
                model: runtime.model().to_string(),
                thinking: runtime.thinking_level().to_string(),
                streaming: *state.streaming.read().await,
                session_id: session.id.clone(),
                total_input_tokens: *state.total_input_tokens.read().await,
                total_output_tokens: *state.total_output_tokens.read().await,
                session_cost: *state.session_cost.read().await,
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
    // Don't allow concurrent streaming
    {
        let is_streaming = *state.streaming.read().await;
        if is_streaming {
            let _ = state.broadcast_tx.send(ServerMessage::Error {
                message: "already streaming — cancel first or wait".to_string(),
            });
            return;
        }
        *state.streaming.write().await = true;
    }

    // Add to history
    let ts = ServerState::timestamp();
    state
        .push_history(HistoryEntry::User {
            content: content.clone(),
            time: ts,
        })
        .await;

    // Add to API messages
    {
        let mut msgs = state.api_messages.write().await;
        msgs.push(serde_json::json!({"role": "user", "content": content}));
    }

    // Start streaming
    let cancel = CancellationToken::new();
    *state.cancel_token.write().await = Some(cancel.clone());

    let messages = state.api_messages.read().await.clone();
    let model = {
        let rt = state.runtime.lock().await;
        rt.model().to_string()
    };

    let mut stream = {
        let rt = state.runtime.lock().await;
        rt.run_stream_with_messages(messages, cancel, None, None)
            .await
    };

    let broadcast = state.broadcast_tx.clone();

    // Engine-level per-stream state. Server doesn't currently expose
    // queued_message or pending_events through the protocol, so we keep
    // them local — process_stream_event still drains them on completion.
    let mut subagents: Vec<SubagentTracker> = Vec::new();
    let mut queued_message: Option<String> = None;
    let mut pending_events: Vec<String> = Vec::new();

    // Process stream events through the engine
    while let Some(event) = stream.next().await {
        let ts = ServerState::timestamp();

        // process_stream_event mutates api_messages in place — hold the
        // write lock only for the call itself, then release before any
        // broadcast / display_history work.
        let (engine_event, completion) = {
            let mut api_msgs = state.api_messages.write().await;
            stream::process_stream_event(
                event,
                &mut api_msgs,
                &mut subagents,
                &mut queued_message,
                &mut pending_events,
            )
        };

        // Side effects that depend on the event kind: display_history,
        // usage accounting, session save on MessageHistory boundaries.
        apply_engine_event_side_effects(&engine_event, state, &model, &ts).await;

        // Translate to wire format and broadcast (if there's anything to send).
        if let Some(msg) = engine_event_to_server_message(engine_event) {
            let _ = broadcast.send(msg);
        }

        // Stream-completion handling. Server doesn't currently support
        // auto-send-queued or auto-trigger-events flows; treat them as Done.
        match completion {
            StreamCompletion::Continue => {}
            StreamCompletion::Done
            | StreamCompletion::AutoSendQueued(_)
            | StreamCompletion::AutoTriggerEvents => {
                state.save_session().await;
            }
            StreamCompletion::Error(_) => {
                // process_stream_event already trimmed dangling messages.
                state.save_session().await;
            }
        }
    }

    *state.streaming.write().await = false;
    *state.cancel_token.write().await = None;
}

/// Apply event-specific side effects that the wire-message translator can't:
///   - Append/extend display_history (replay buffer for late-connecting clients)
///   - Bump usage counters on Usage events
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
            state
                .push_history(HistoryEntry::ToolUse {
                    tool_name: tool_name.clone(),
                    input: input.clone(),
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
            ..
        } => {
            state.add_usage(*input_tokens, *output_tokens, model).await;
        }
        EngineStreamEvent::Error(err) => {
            state
                .push_history(HistoryEntry::Error {
                    content: err.clone(),
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
            // Engine serialised input to JSON string; reparse for the wire.
            let input_value =
                serde_json::from_str(&input).unwrap_or(serde_json::Value::String(input));
            Some(ServerMessage::ToolUse {
                tool_name,
                tool_id,
                input: input_value,
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
            ..
        } => Some(ServerMessage::Usage {
            input_tokens,
            output_tokens,
        }),
        EngineStreamEvent::Done => Some(ServerMessage::Done),
        EngineStreamEvent::Error(message) => Some(ServerMessage::Error { message }),
        // Server protocol doesn't expose these (yet).
        EngineStreamEvent::SubagentStart { .. }
        | EngineStreamEvent::SubagentUpdate { .. }
        | EngineStreamEvent::SubagentDone { .. }
        | EngineStreamEvent::SteeringDelivered { .. }
        | EngineStreamEvent::Noop => None,
    }
}

async fn handle_command(name: &str, args: &str, state: &Arc<ServerState>) {
    let broadcast = &state.broadcast_tx;

    // Server-specific overrides — handled BEFORE engine to preserve existing
    // wire behaviour for cases the engine doesn't know about.
    if name == "thinking" && args == "adaptive" {
        let mut rt = state.runtime.lock().await;
        rt.set_thinking_budget(0);
        let _ = broadcast.send(ServerMessage::System {
            message: format!("thinking set to: {}", rt.thinking_level()),
        });
        return;
    }
    // Engine doesn't display current values when args are empty for these,
    // but the existing server contract does. Intercept first.
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
            CommandResult::ModelChanged { model } => {
                let _ = broadcast.send(ServerMessage::System {
                    message: format!("model set to: {model}"),
                });
            }
            CommandResult::ThinkingChanged { level, .. } => {
                let _ = broadcast.send(ServerMessage::System {
                    message: format!("thinking set to: {level}"),
                });
            }
            CommandResult::Quit => {
                let _ = broadcast.send(ServerMessage::System {
                    message: "/quit ignored — server is long-lived; close the WebSocket instead"
                        .to_string(),
                });
            }
            CommandResult::Compact => {
                let _ = broadcast.send(ServerMessage::System {
                    message: "/compact not yet wired in server mode".to_string(),
                });
            }
            CommandResult::Error(msg) => {
                let _ = broadcast.send(ServerMessage::Error { message: msg });
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
            state.save_session().await;
            state.api_messages.write().await.clear();
            state.display_history.write().await.clear();
            *state.total_input_tokens.write().await = 0;
            *state.total_output_tokens.write().await = 0;
            *state.session_cost.write().await = 0.0;
            {
                let rt = state.runtime.lock().await;
                *state.session.write().await =
                    Session::new(rt.model(), rt.thinking_level(), rt.system_prompt());
            }
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

/// Rebuild display history from API messages (for --continue)
fn rebuild_history(api_messages: &[serde_json::Value]) -> Vec<HistoryEntry> {
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
