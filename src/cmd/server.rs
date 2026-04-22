use synaps_cli::{Runtime, StreamEvent, CancellationToken, Session, truncate_str};
use synaps_cli::protocol::{ClientMessage, ServerMessage, HistoryEntry};
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::State,
    response::IntoResponse,
    routing::get,
    Router,
};
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, RwLock};
use chrono::Local;

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
    if let Some(ref prof) = profile {
        synaps_cli::config::set_profile(Some(prof.clone()));
    }

    let _log_guard = synaps_cli::logging::init_logging();
    let mut runtime = Runtime::new().await?;

    // Load config and apply
    let config = synaps_cli::config::load_config();
    runtime.apply_config(&config);

    // Load system prompt
    let system_prompt = synaps_cli::config::resolve_system_prompt(system.as_deref());
    runtime.set_system_prompt(system_prompt);

    // Session: continue existing or create new
    let (session, initial_api_messages, initial_history, initial_in, initial_out, initial_cost) =
        match continue_session {
            Some(maybe_id) => {
                let session = match maybe_id {
                    Some(id) => synaps_cli::resolve_session(&id)?,
                    None => synaps_cli::latest_session()?,
                };
                runtime.set_model(session.model.clone());
                if let Some(ref sp) = session.system_prompt {
                    runtime.set_system_prompt(sp.clone());
                }
                let api_msgs = session.api_messages.clone();
                let history = rebuild_history(&api_msgs);
                let input_t = session.total_input_tokens;
                let output_t = session.total_output_tokens;
                let cost = session.session_cost;
                (session, api_msgs, history, input_t, output_t, cost)
            }
            None => {
                let session = Session::new(runtime.model(), runtime.thinking_level(), runtime.system_prompt());
                (session, Vec::new(), Vec::new(), 0, 0, 0.0)
            }
        };

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
                message: "cancelled".to_string(),
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
    state.push_history(HistoryEntry::User {
        content: content.clone(),
        time: ts,
    }).await;

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
        rt.run_stream_with_messages(messages, cancel, None).await
    };

    let broadcast = state.broadcast_tx.clone();

    // Process stream events
    while let Some(event) = stream.next().await {
        let ts = ServerState::timestamp();
        match event {
            StreamEvent::Thinking(text) => {
                let _ = broadcast.send(ServerMessage::Thinking { content: text.clone() });
                // Append to last thinking entry or create new
                let mut history = state.display_history.write().await;
                if let Some(HistoryEntry::Thinking { content: ref mut c, .. }) = history.last_mut() {
                    c.push_str(&text);
                } else {
                    history.push(HistoryEntry::Thinking { content: text, time: ts });
                }
            }
            StreamEvent::Text(text) => {
                let _ = broadcast.send(ServerMessage::Text { content: text.clone() });
                let mut history = state.display_history.write().await;
                if let Some(HistoryEntry::Text { content: ref mut c, .. }) = history.last_mut() {
                    c.push_str(&text);
                } else {
                    history.push(HistoryEntry::Text { content: text, time: ts });
                }
            }
            StreamEvent::ToolUseStart(tool_name) => {
                let _ = broadcast.send(ServerMessage::ToolUseStart { tool_name });
            }
            StreamEvent::ToolUseDelta(delta) => {
                let _ = broadcast.send(ServerMessage::ToolUseDelta(delta));
            }
            StreamEvent::ToolUse { tool_name, tool_id, input } => {
                let _ = broadcast.send(ServerMessage::ToolUse {
                    tool_name: tool_name.clone(),
                    tool_id: tool_id.clone(),
                    input: input.clone(),
                });
                state.push_history(HistoryEntry::ToolUse {
                    tool_name,
                    input: serde_json::to_string(&input).unwrap_or_default(),
                    time: ts,
                }).await;
            }
            StreamEvent::ToolResultDelta { tool_id, delta } => {
                let _ = broadcast.send(ServerMessage::ToolResultDelta {
                    tool_id,
                    delta,
                });
            }
            StreamEvent::ToolResult { tool_id: _, result } => {
                let _ = broadcast.send(ServerMessage::ToolResult {
                    tool_id: String::new(),
                    result: result.clone(),
                });
                state.push_history(HistoryEntry::ToolResult {
                    result,
                    time: ts,
                }).await;
            }
            StreamEvent::MessageHistory(history) => {
                *state.api_messages.write().await = history;
                state.save_session().await;
            }
            StreamEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_input_tokens: _,
                cache_creation_input_tokens: _,
                model: _,
            } => {
                state.add_usage(input_tokens, output_tokens, &model).await;
                let _ = broadcast.send(ServerMessage::Usage { input_tokens, output_tokens });
            }
            StreamEvent::Done => {
                let _ = broadcast.send(ServerMessage::Done);
            }
            // Subagent events — not yet wired to server protocol
            StreamEvent::SubagentStart { .. }
            | StreamEvent::SubagentUpdate { .. }
            | StreamEvent::SubagentDone { .. }
            | StreamEvent::SteeringDelivered { .. } => {}
            StreamEvent::Error(err) => {
                let _ = broadcast.send(ServerMessage::Error { message: err.clone() });
                state.push_history(HistoryEntry::Error {
                    content: err,
                    time: ts,
                }).await;

                // Clean up trailing broken messages (same logic as chatui)
                let mut msgs = state.api_messages.write().await;
                if let Some(last) = msgs.last() {
                    let role = last["role"].as_str().unwrap_or("");
                    let is_text_user = role == "user" && last["content"].is_string();
                    let is_assistant = role == "assistant";
                    if is_text_user || is_assistant {
                        msgs.pop();
                    }
                }
            }
        }
    }

    *state.streaming.write().await = false;
    *state.cancel_token.write().await = None;
}

async fn handle_command(name: &str, args: &str, state: &Arc<ServerState>) {
    let broadcast = &state.broadcast_tx;

    match name {
        "model" => {
            if args.is_empty() {
                let rt = state.runtime.lock().await;
                let _ = broadcast.send(ServerMessage::System {
                    message: format!("current model: {}", rt.model()),
                });
            } else {
                let mut rt = state.runtime.lock().await;
                rt.set_model(args.to_string());
                let _ = broadcast.send(ServerMessage::System {
                    message: format!("model set to: {}", args),
                });
            }
        }
        "thinking" => {
            let mut rt = state.runtime.lock().await;
            match args {
                "low" => { rt.set_thinking_budget(2048); }
                "medium" | "med" => { rt.set_thinking_budget(4096); }
                "high" => { rt.set_thinking_budget(16384); }
                "xhigh" => { rt.set_thinking_budget(32768); }
                "adaptive" => { rt.set_thinking_budget(0); }
                "" => {
                    let _ = broadcast.send(ServerMessage::System {
                        message: format!("thinking: {} ({})", rt.thinking_level(), rt.thinking_budget()),
                    });
                    return;
                }
                _ => {
                    let _ = broadcast.send(ServerMessage::Error {
                        message: "usage: /thinking low|medium|high|xhigh".to_string(),
                    });
                    return;
                }
            }
            let _ = broadcast.send(ServerMessage::System {
                message: format!("thinking set to: {}", rt.thinking_level()),
            });
        }
        "clear" => {
            state.save_session().await;
            state.api_messages.write().await.clear();
            state.display_history.write().await.clear();
            *state.total_input_tokens.write().await = 0;
            *state.total_output_tokens.write().await = 0;
            *state.session_cost.write().await = 0.0;
            {
                let rt = state.runtime.lock().await;
                *state.session.write().await = Session::new(
                    rt.model(), rt.thinking_level(), rt.system_prompt()
                );
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
                message: format!("unknown command: {}", name),
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
                                let input = serde_json::to_string(&block["input"]).unwrap_or_default();
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
