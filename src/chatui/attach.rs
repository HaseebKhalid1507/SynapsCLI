//! Attach mode — connect TUI to a running watcher agent's event stream.
//!
//! Instead of creating a local ConversationDriver, the TUI connects to the
//! watcher supervisor's Unix socket and subscribes to an agent's bus via the
//! NDJSON attach protocol. The TUI gets the same events (Text, Tool, Subagent,
//! etc.) and can send messages and cancel — full interactive control over a
//! remote agent with the rich TUI renderer.

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc};
use synaps_cli::{
    AttachEvent, AttachInbound, SyncState,
    WatcherCommand, WatcherResponse,
    transport::{AgentEvent, Inbound},
};

/// Everything the TUI needs to talk to a remote agent.
pub(crate) struct AttachSession {
    pub event_rx: broadcast::Receiver<AgentEvent>,
    pub inbound_tx: mpsc::UnboundedSender<Inbound>,
    pub sync_state: SyncState,
    // Keep tasks alive for the session's lifetime — pub for forget in main.rs
    pub _reader_task: tokio::task::JoinHandle<()>,
    pub _writer_task: tokio::task::JoinHandle<()>,
}

impl AttachSession {
    /// Connect to a running watcher agent by name.
    ///
    /// Returns an AttachSession with channels that are type-compatible with
    /// the local driver's bus_handle.subscribe() / bus_handle.inbound(),
    /// so the TUI event loop works unchanged.
    pub async fn connect(name: &str, readonly: bool) -> Result<Self, String> {
        let socket_path = synaps_cli::config::base_dir().join("watcher").join("watcher.sock");
        if !socket_path.exists() {
            return Err("Watcher not running. Start with: watcher run".into());
        }

        let stream = UnixStream::connect(&socket_path).await
            .map_err(|e| format!("Failed to connect to watcher: {}", e))?;

        let (reader, mut writer) = stream.into_split();
        let mut buf_reader = BufReader::new(reader);

        // Send attach command
        let mode = if readonly { "ro" } else { "rw" };
        let cmd = serde_json::to_string(&WatcherCommand::Attach {
            name: name.to_string(),
            mode: mode.to_string(),
        }).map_err(|e| format!("Failed to serialize command: {}", e))?;

        writer.write_all(cmd.as_bytes()).await.map_err(|e| e.to_string())?;
        writer.write_all(b"\n").await.map_err(|e| e.to_string())?;
        writer.flush().await.map_err(|e| e.to_string())?;

        // Read response
        let mut line = String::new();
        buf_reader.read_line(&mut line).await.map_err(|e| e.to_string())?;
        let resp: WatcherResponse = serde_json::from_str(line.trim())
            .map_err(|e| format!("Failed to parse watcher response: {}", e))?;

        let sync_json = match resp {
            WatcherResponse::AttachOk { sync_state } => sync_state,
            WatcherResponse::Error { message } => return Err(message),
            _ => return Err("Unexpected response from watcher".into()),
        };

        let sync_state: SyncState = serde_json::from_str(&sync_json)
            .unwrap_or(SyncState {
                agent_name: Some(name.to_string()),
                model: "unknown".into(),
                thinking_level: "medium".into(),
                session_id: "attached".into(),
                is_streaming: false,
                turn_count: 0,
                total_input_tokens: 0,
                total_output_tokens: 0,
                total_cost_usd: 0.0,
                partial_text: None,
                partial_thinking: None,
                active_tool: None,
                recent_events: Vec::new(),
            });

        // Create channels that match the bus interface types
        let (event_tx, event_rx) = broadcast::channel::<AgentEvent>(256);
        let (inbound_tx, mut inbound_rx) = mpsc::unbounded_channel::<Inbound>();

        // Reader task: socket NDJSON → broadcast channel
        let reader_task = tokio::spawn(async move {
            let mut line = String::new();
            loop {
                line.clear();
                match buf_reader.read_line(&mut line).await {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() { continue; }
                        if let Ok(attach_event) = serde_json::from_str::<AttachEvent>(trimmed) {
                            if let AttachEvent::Event { event } = attach_event {
                                let _ = event_tx.send(event);
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // Writer task: mpsc channel → socket NDJSON
        let writer_task = tokio::spawn(async move {
            while let Some(inbound) = inbound_rx.recv().await {
                let attach_msg = match inbound {
                    Inbound::Message { content } => Some(AttachInbound::Message { content }),
                    Inbound::Cancel => Some(AttachInbound::Cancel),
                    // Steer sent as Message — driver will process after current stream
                    Inbound::Steer { content } => Some(AttachInbound::Message { content }),
                    // Commands are local (model, theme, etc.) — not forwarded
                    _ => None,
                };
                if let Some(msg) = attach_msg {
                    let Ok(json) = serde_json::to_string(&msg) else { continue };
                    if writer.write_all(json.as_bytes()).await.is_err() { break; }
                    if writer.write_all(b"\n").await.is_err() { break; }
                    let _ = writer.flush().await;
                }
            }
            // TUI exiting — send detach
            let detach = serde_json::to_string(&AttachInbound::Detach).unwrap_or_default();
            let _ = writer.write_all(detach.as_bytes()).await;
            let _ = writer.write_all(b"\n").await;
            let _ = writer.flush().await;
        });

        Ok(Self {
            event_rx,
            inbound_tx,
            sync_state,
            _reader_task: reader_task,
            _writer_task: writer_task,
        })
    }
}
