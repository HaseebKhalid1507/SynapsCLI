use synaps_cli::{Runtime, Session};
use synaps_cli::transport::{
    BusHandle, ConversationDriver, WebSocketTransport,
    AgentEvent, MetaEvent, SyncState,
    driver::DriverConfig,
};
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::State,
    response::IntoResponse,
    routing::get,
    Router,
};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

struct ServerState {
    bus: BusHandle,
    client_count: AtomicUsize,
}

impl ServerState {
    fn sync_state_stub(&self) -> SyncState {
        // Minimal sync for new connections — driver owns the real state,
        // but we don't have direct access from here. The transport will
        // get events from the bus going forward.
        SyncState {
            agent_name: None,
            model: String::new(),
            thinking_level: String::new(),
            session_id: String::new(),
            is_streaming: false,
            turn_count: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cost_usd: 0.0,
            partial_text: None,
            partial_thinking: None,
            active_tool: None,
            recent_events: Vec::new(),
        }
    }
}

#[derive(Parser)]
#[command(name = "server", about = "SynapsCLI WebSocket server")]
struct Cli {
    #[arg(long, short, default_value = "3145")]
    port: u16,

    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    #[arg(long = "system", short = 's', value_name = "PROMPT_OR_FILE")]
    system: Option<String>,

    #[arg(long = "continue", value_name = "SESSION_ID")]
    continue_session: Option<Option<String>>,

    #[arg(long, global = true)]
    profile: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if let Some(ref prof) = cli.profile {
        synaps_cli::config::set_profile(Some(prof.clone()));
    }

    let _log_guard = synaps_cli::logging::init_logging();
    let mut runtime = Runtime::new().await?;

    let config = synaps_cli::config::load_config();
    runtime.apply_config(&config);

    let system_prompt = synaps_cli::config::resolve_system_prompt(cli.system.as_deref());
    runtime.set_system_prompt(system_prompt);

    // Session: continue or new
    let session = match cli.continue_session {
        Some(maybe_id) => {
            let s = match maybe_id {
                Some(id) => synaps_cli::find_session(&id)?,
                None => synaps_cli::latest_session()?,
            };
            runtime.set_model(s.model.clone());
            if let Some(ref sp) = s.system_prompt {
                runtime.set_system_prompt(sp.clone());
            }
            s
        }
        None => Session::new(runtime.model(), runtime.thinking_level(), runtime.system_prompt()),
    };

    let session_id = session.id.clone();

    let mut driver = ConversationDriver::new(runtime, session, DriverConfig::default());
    let bus_handle = driver.bus().handle();

    // If continuing, inject existing messages into driver
    // (they're already in the session which the driver owns)

    let state = Arc::new(ServerState {
        bus: bus_handle,
        client_count: AtomicUsize::new(0),
    });

    // Run driver in background
    tokio::spawn(async move {
        if let Err(e) = driver.run().await {
            tracing::error!("Driver error: {}", e);
        }
    });

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/health", get(health_handler))
        .with_state(state.clone());

    let addr = format!("{}:{}", cli.host, cli.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    eprintln!("╔══════════════════════════════════════╗");
    eprintln!("║        SynapsCLI Server v0.3         ║");
    eprintln!("╠══════════════════════════════════════╣");
    eprintln!("║  Listening: ws://{}:{:<5}      ║", cli.host, cli.port);
    eprintln!("║  Session:   {:<24}║", &session_id);
    eprintln!("╚══════════════════════════════════════╝");

    axum::serve(listener, app).await?;
    Ok(())
}

async fn health_handler() -> impl IntoResponse { "ok" }

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<ServerState>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_client(socket, state))
}

async fn handle_client(socket: WebSocket, state: Arc<ServerState>) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    let n = state.client_count.fetch_add(1, Ordering::Relaxed) + 1;
    tracing::info!("Client connected ({} total)", n);
    state.bus.broadcast(AgentEvent::Meta(MetaEvent::Steered {
        message: format!("client connected ({} total)", n),
    }));

    // Channels bridging WebSocket ↔ WebSocketTransport
    let (to_transport_tx, to_transport_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (from_transport_tx, mut from_transport_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    let transport = WebSocketTransport::new(to_transport_rx, from_transport_tx);
    let sync = state.sync_state_stub();
    state.bus.connect(transport, sync);

    // Bridge: transport → WebSocket
    let ws_send = tokio::spawn(async move {
        while let Some(json) = from_transport_rx.recv().await {
            if ws_tx.send(Message::Text(json)).await.is_err() {
                break;
            }
        }
    });

    // Bridge: WebSocket → transport
    while let Some(Ok(msg)) = ws_rx.next().await {
        match msg {
            Message::Text(text) => {
                if to_transport_tx.send(text.to_string()).is_err() {
                    break;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    ws_send.abort();
    let n = state.client_count.fetch_sub(1, Ordering::Relaxed) - 1;
    tracing::info!("Client disconnected ({} remaining)", n);
    state.bus.broadcast(AgentEvent::Meta(MetaEvent::Steered {
        message: format!("client disconnected ({} remaining)", n),
    }));
}
