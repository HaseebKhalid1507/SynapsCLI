# Transport Abstraction & Agent Bus — Architecture Spec

**Version:** 0.1.0  
**Date:** 2025-04-14  
**Status:** Proposed  

---

## 0. Thesis

> *"The mark of a primitive system is that every new interface demands a new binary. The mark of an evolved system is that a new interface demands only a new adapter."*

SynapsCLI currently has **six binaries** that each reinvent the same conversation loop with different I/O. The chat binary handles `StreamEvent` with `print!`. The TUI handles it with ratatui widgets. The server handles it with WebSocket broadcast. The agent binary handles it with JSONL logging. Each one independently manages message history, token tracking, session persistence, and cancellation.

This spec defines three abstractions that eliminate all of that duplication:

1. **Transport** — A trait that any I/O frontend implements. Two methods. That's it.
2. **AgentBus** — A broadcast channel that makes any running agent observable and steerable by any number of transports.
3. **Router** — The watcher evolves from process supervisor into message multiplexer. Any transport connects to any agent.

The conversation loop is written **once**. Adding Discord is ~50 lines. Adding Telegram is ~50 lines. Attaching a TUI to a running autonomous agent is a CLI flag.

---

## 1. The Duplication Problem

### What every binary does (identically):

| Concern | chat.rs | chatui | server.rs | agent.rs |
|---|---|---|---|---|
| Create `Runtime` | ✓ | ✓ | ✓ | ✓ |
| Load config / apply | partial | ✓ | ✓ | ✓ |
| Manage `Vec<Value>` message history | ✓ | ✓ | ✓ | ✓ |
| Call `run_stream_with_messages()` | ✓ | ✓ | ✓ | ✓ |
| Match on `StreamEvent` variants | ✓ (40 lines) | ✓ (100 lines) | ✓ (80 lines) | ✓ (40 lines) |
| Handle `MessageHistory` → save state | ✓ | ✓ | ✓ | ✓ |
| Handle `Usage` → accumulate tokens | — | ✓ | ✓ | ✓ |
| Handle `Error` → pop broken messages | — | ✓ | ✓ | — |
| Handle `Done` → end turn | ✓ | ✓ | ✓ | ✓ |
| Cancellation via `CancellationToken` | ✓ | ✓ | ✓ | ✓ |
| Cost estimation from token counts | — | ✓ | ✓ | ✓ |

### What's unique to each:

| Binary | Unique concern |
|---|---|
| **chat.rs** | `print!` with ANSI escape codes |
| **chatui** | ratatui rendering, input handling, scroll, effects, subagent panel |
| **server.rs** | WebSocket accept, broadcast channel, `ServerMessage` serialization |
| **agent.rs** | Limit enforcement, heartbeat, JSONL logging, handoff protocol |
| **client.rs** | WebSocket client consuming `ServerMessage`, formatting to terminal |

The unique parts are the *only* parts that should exist in each binary. Everything else belongs in a shared conversation driver.

---

## 2. Architecture Overview

```
                    ┌────────────────────────────────────────────────┐
                    │              ConversationDriver                 │
                    │  (owns Runtime, message history, session,       │
                    │   token tracking, cancellation, steering)       │
                    │                                                 │
                    │  Consumes StreamEvent internally.               │
                    │  Emits AgentEvent (cleaned, enriched).          │
                    │  Accepts Inbound (user messages, commands).     │
                    └────────────┬───────────────────────────────────┘
                                 │
                          AgentBus (the real abstraction)
                          subscribe() + inbound()
                                 │
              ┌──────────────────┼──────────────────────┐
              │                  │                      │
     Direct bus consumers   Transport trait wrappers    │
     (own their event loop) (bus spawns bridge task)    │
              │                  │                      │
           TUI             StdioTransport         DiscordTransport
     (uses bus channels    WebSocketTransport
      in its own select!)
```

### Two-Tier Consumer Model

The **AgentBus** is the universal abstraction — not the Transport trait. The bus exposes raw channels that any consumer can use directly. The Transport trait is a convenience layer for simple I/O adapters.

**Tier 1: Direct bus consumers** — Complex applications (TUI) that need their own event loop. They call `bus.subscribe()` and `bus.inbound()` to get raw channels, then use them in their own `tokio::select!` alongside terminal events, animation ticks, etc.

**Tier 2: Transport trait impls** — Simple adapters (stdio, WebSocket, Discord) that fit the `recv()`/`send()` model. The bus spawns a bridge task that calls these methods in a loop. Zero boilerplate for the implementor.

Both tiers are first-class citizens. The bus doesn't know or care which tier a consumer is.

### Data Flow

```
                         AgentBus
                    subscribe() / inbound()
                            │
               ┌────────────┼────────────────────┐
               │            │                    │
          TUI (Tier 1)   Stdio (Tier 2)    Discord (Tier 2)
          uses channels   uses Transport    uses Transport
          in own loop     trait via          trait via
                          bus.connect()      bus.connect()
               │            │                    │
               ▼            ▼                    ▼
         AgentEvent    AgentEvent           AgentEvent
         in select!    via send()           via send()
         alongside     (bus calls it)       (bus calls it)
         terminal
         events

Inbound flows back the same way:
  TUI: inbound_tx.send(Inbound::Message { ... })
  Stdio: fn recv() → Some(Inbound::Message { ... })
  Discord: fn recv() → Some(Inbound::Message { ... })
```

---

## 3. Core Types

### 3.1 AgentEvent — What transports receive

`StreamEvent` is an internal implementation detail tied to the API streaming protocol. Transports should never see it. `AgentEvent` is the **public** event type — cleaned, enriched, and stable.

**Design: Grouped enums.** 7 top-level variants instead of 18 flat ones. Tool and subagent lifecycle are grouped into sub-enums. Simple transports match on 4-5 top-level arms. Complex consumers (TUI) drill into sub-enums when they need granularity.

```rust
// src/transport/events.rs

/// Events emitted by the conversation driver to all connected transports.
/// This is the public API surface — stable across versions.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Streamed text output from the model
    Text(String),

    /// Extended thinking (streamed)
    Thinking(String),

    /// Tool lifecycle (grouped — drill in for details)
    Tool(ToolEvent),

    /// Subagent lifecycle (grouped — drill in for details)
    Subagent(SubagentEvent),

    /// Session metadata: usage, stats, steering, shutdown
    Meta(MetaEvent),

    /// Current turn is done (model stopped generating)
    TurnComplete,

    /// Something went wrong (non-fatal — session continues)
    Error(String),
}

/// Tool invocation lifecycle events.
#[derive(Debug, Clone)]
pub enum ToolEvent {
    /// Tool invocation started (name known, args streaming)
    Start { tool_name: String, tool_id: String },

    /// Streaming JSON argument data for the current tool
    ArgsDelta(String),

    /// Tool fully invoked (args complete, execution starting)
    Invoke { tool_name: String, tool_id: String, input: serde_json::Value },

    /// Streaming output from tool execution
    OutputDelta { tool_id: String, delta: String },

    /// Tool execution finished
    Complete { tool_id: String, result: String, elapsed_ms: Option<u64> },
}

/// Subagent lifecycle events.
#[derive(Debug, Clone)]
pub enum SubagentEvent {
    Start { id: u64, agent_name: String, task_preview: String },
    Update { id: u64, agent_name: String, status: String },
    Done { id: u64, agent_name: String, result_preview: String, duration_secs: f64 },
}

/// Session metadata events.
#[derive(Debug, Clone)]
pub enum MetaEvent {
    /// Token usage for this API turn
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
        model: String,
        cost_usd: f64,
    },

    /// Running totals (emitted after each turn)
    SessionStats {
        total_input_tokens: u64,
        total_output_tokens: u64,
        total_cost_usd: f64,
        turn_count: u64,
        tool_call_count: u64,
    },

    /// A steering message was injected mid-stream
    Steered { message: String },

    /// Agent is shutting down
    Shutdown { reason: String },
}
```

**Rationale:** `AgentEvent` differs from `StreamEvent` in several important ways:
- Grouped enums: 7 top-level variants instead of 18. Simple transports ignore `Tool(_)`, `Subagent(_)`, `Meta(_)` with one `_ => {}` arm each.
- `Usage` includes pre-calculated `cost_usd` — transports don't need pricing tables
- `SessionStats` provides running totals — no accumulation logic in transports
- `ToolEvent::Complete` includes `elapsed_ms` — timed by the driver, not each transport
- `MessageHistory` is **gone** — transports don't manage API message arrays
- `Done` becomes `TurnComplete` (clearer semantics)
- `Shutdown` is explicit under `MetaEvent` (vs just stopping the stream)

### 3.2 Inbound — What transports send

```rust
// src/transport/inbound.rs

/// Messages sent from a transport into the conversation driver.
#[derive(Debug, Clone)]
pub enum Inbound {
    /// A user message to add to the conversation
    Message { content: String },

    /// Steering message injected while the model is working
    Steer { content: String },

    /// Cancel the current streaming response
    Cancel,

    /// Execute a runtime command (model change, thinking level, etc.)
    Command { name: String, args: String },

    /// Request current session state (for late-joining transports)
    SyncRequest,
}
```

### 3.3 SyncState — Snapshot for late joiners

```rust
// src/transport/sync.rs

/// Full state snapshot sent to a transport that connects mid-session.
/// Allows late joiners (e.g. `synaps attach scout`) to reconstruct context.
///
/// Includes partial state for mid-stream joins — if the agent is currently
/// thinking or generating text, the in-progress content is included so the
/// late joiner can display it immediately before live events take over.
#[derive(Debug, Clone)]
pub struct SyncState {
    pub agent_name: Option<String>,
    pub model: String,
    pub thinking_level: String,
    pub session_id: String,
    pub is_streaming: bool,
    pub turn_count: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost_usd: f64,
    /// Partial content from the current turn (if mid-stream).
    pub partial_text: Option<String>,
    pub partial_thinking: Option<String>,
    pub active_tool: Option<String>,
    /// Recent conversation events (last N completed turns) for context.
    /// NOT the full API message history — that's an internal concern.
    pub recent_events: Vec<AgentEvent>,
}
```

**Mid-stream join protocol:** When a transport connects while the agent is mid-turn:
1. Bus subscribes the transport to the broadcast channel (events start buffering)
2. Driver builds `SyncState` including partial text/thinking from the current turn
3. Transport receives `SyncState`, reconstructs display
4. Buffered events (anything emitted between steps 1 and 3) are delivered
5. Live events continue from this point — no gaps, no missed words

---

## 4. The Transport Trait

The Transport trait is a **convenience layer** for simple I/O adapters. It is NOT the only way to consume the bus. Complex applications (TUI) use bus channels directly (see §2 Two-Tier Consumer Model).

**Use the Transport trait when:** your frontend fits a simple recv/send model (stdio, WebSocket, Discord, Slack, Telegram).

**Use the bus directly when:** your frontend needs its own event loop with multiple concurrent concerns (TUI with animations, keyboard polling, rendering).

```rust
// src/transport/mod.rs

/// A Transport is a simple I/O adapter that receives user input and sends agent events.
///
/// Implementing this trait is ALL you need to create a new frontend for simple cases.
/// The bus spawns a bridging task that calls recv() and send() in a select! loop.
/// The conversation driver handles everything else: message history, API calls,
/// tool execution, token tracking, session persistence, cancellation.
///
/// For complex frontends (TUI) that need their own event loop, use
/// bus.subscribe() and bus.inbound() directly instead of this trait.
///
/// # Contract
/// - `recv()` is called in a loop. Return `None` to disconnect this transport.
/// - `send()` is called for every AgentEvent. Implementations filter/format as needed.
/// - Transports MUST be `Send + 'static` (they run in spawned tasks).
/// - Transports SHOULD be cheap to construct — one per connection.
///
/// # Example: Minimal stdin/stdout transport (~30 lines)
/// ```rust
/// struct StdioTransport { rx: tokio::sync::mpsc::UnboundedReceiver<String> }
///
/// #[async_trait]
/// impl Transport for StdioTransport {
///     async fn recv(&mut self) -> Option<Inbound> {
///         let line = self.rx.recv().await?;
///         Some(Inbound::Message { content: line })
///     }
///     async fn send(&mut self, event: AgentEvent) -> bool {
///         match event {
///             AgentEvent::Text(t) => print!("{}", t),
///             AgentEvent::TurnComplete => println!(),
///             AgentEvent::Error(e) => eprintln!("Error: {}", e),
///             _ => {}
///         }
///         true
///     }
/// }
/// ```
#[async_trait::async_trait]
pub trait Transport: Send + 'static {
    /// Receive the next inbound message from this transport.
    /// Returns `None` when the transport disconnects (EOF, WebSocket close, etc.)
    async fn recv(&mut self) -> Option<Inbound>;

    /// Send an agent event to this transport. Returns `false` if the transport
    /// has disconnected and should be removed.
    async fn send(&mut self, event: AgentEvent) -> bool;

    /// Called once when the transport first connects. Receives a snapshot of
    /// current session state for context reconstruction.
    /// Default implementation does nothing.
    async fn on_sync(&mut self, _state: SyncState) {}

    /// Human-readable name for logging (e.g. "discord", "ws:192.168.1.5", "stdio")
    fn name(&self) -> &str { "unknown" }
}
```

**Why two required methods?** Real transports are bidirectional and concurrent. The bus's bridging task calls both in a `tokio::select!` — recv for input, send for output, simultaneously. A single `poll()` method would force serial processing.

**Why `on_sync` is separate from `send`:** Late-joining transports need bulk state reconstruction (recent history, token counts, model info). Mixing this into the `AgentEvent` stream would require a special "init" event variant that only fires once and clutters the enum. Separate method, clean separation.

**Why the TUI doesn't use this trait:** The TUI needs to `select!` over terminal events, animation ticks, AND agent events in a single loop with shared mutable state. The two-method interface assumes the bus controls the polling cadence. The TUI needs to control its own cadence. So it uses `bus.subscribe()` + `bus.inbound()` directly — same data, full control. See §7.2.

---

## 5. The ConversationDriver

The driver is the **single owner** of conversation state. No transport touches message history, token counts, or session persistence. The driver:

1. Owns the `Runtime`
2. Owns `Vec<Value>` message history
3. Owns `Session` and persists it
4. Owns token/cost accumulators
5. Owns the `CancellationToken`
6. Owns the steering channel
7. Processes `StreamEvent` internally, emits `AgentEvent` publicly

```rust
// src/transport/driver.rs

pub struct ConversationDriver {
    runtime: Runtime,
    messages: Vec<Value>,
    session: Session,
    cancel: Option<CancellationToken>,
    steer_tx: Option<mpsc::UnboundedSender<String>>,

    // Accumulators
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cost_usd: f64,
    turn_count: u64,
    tool_call_count: u64,
    tool_start_time: Option<Instant>,

    // Event distribution
    bus: AgentBus,

    // Configuration
    auto_save: bool,           // persist session on each turn (interactive modes)
    event_buffer_size: usize,  // how many recent events to keep for SyncState
    recent_events: VecDeque<AgentEvent>,
}

impl ConversationDriver {
    /// Create a new driver with a fresh session.
    pub async fn new(config: DriverConfig) -> Result<Self>;

    /// Create a driver that resumes an existing session.
    pub async fn resume(session: Session, config: DriverConfig) -> Result<Self>;

    /// Get a handle to the agent bus (for connecting transports).
    pub fn bus(&self) -> &AgentBus;

    /// Run the driver loop. Processes inbound messages from all connected
    /// transports and emits AgentEvents on the bus. Returns when all
    /// transports disconnect or Shutdown is triggered.
    pub async fn run(&mut self) -> Result<()>;

    /// Inject a message programmatically (for agent mode boot messages).
    pub async fn inject_user_message(&mut self, content: String);

    /// Trigger graceful shutdown.
    pub fn shutdown(&self, reason: String);

    /// Get a SyncState snapshot for late-joining transports.
    pub fn sync_state(&self) -> SyncState;
}
```

### Driver Config

```rust
pub struct DriverConfig {
    pub system_prompt: String,
    pub model: String,
    pub thinking_budget: u32,
    pub tools: ToolRegistry,
    pub agent_name: Option<String>,
    pub auto_save: bool,
    pub watcher_exit_path: Option<PathBuf>,
    pub synaps_config: SynapsConfig,
}
```

### Internal Driver Loop (pseudocode)

```rust
async fn run(&mut self) -> Result<()> {
    loop {
        tokio::select! {
            // Wait for any transport to send us something
            inbound = self.bus.recv_inbound() => {
                match inbound {
                    None => break, // All transports disconnected
                    Some(Inbound::Message { content }) => {
                        self.handle_user_message(content).await?;
                    }
                    Some(Inbound::Steer { content }) => {
                        if let Some(ref tx) = self.steer_tx {
                            let _ = tx.send(content.clone());
                            self.bus.broadcast(AgentEvent::Steered { message: content });
                        }
                    }
                    Some(Inbound::Cancel) => {
                        if let Some(ref ct) = self.cancel {
                            ct.cancel();
                        }
                    }
                    Some(Inbound::Command { name, args }) => {
                        self.handle_command(&name, &args).await;
                    }
                    Some(Inbound::SyncRequest) => {
                        // Handled by AgentBus — on_sync called automatically
                    }
                }
            }
        }
    }
    Ok(())
}

async fn handle_user_message(&mut self, content: String) -> Result<()> {
    self.messages.push(json!({"role": "user", "content": content}));

    let cancel = CancellationToken::new();
    let (steer_tx, steer_rx) = mpsc::unbounded_channel();
    self.cancel = Some(cancel.clone());
    self.steer_tx = Some(steer_tx);

    let mut stream = self.runtime
        .run_stream_with_messages(self.messages.clone(), cancel, Some(steer_rx))
        .await;

    while let Some(event) = stream.next().await {
        match event {
            // ── Internal bookkeeping (not forwarded directly) ──
            StreamEvent::MessageHistory(history) => {
                self.messages = history;
                if self.auto_save { self.save_session().await; }
            }
            StreamEvent::Usage { input_tokens, output_tokens, cache_read_input_tokens,
                                 cache_creation_input_tokens, model } => {
                let m = model.as_deref().unwrap_or(self.runtime.model());
                let cost = estimate_cost(input_tokens, output_tokens, m);
                self.total_input_tokens += input_tokens;
                self.total_output_tokens += output_tokens;
                self.total_cost_usd += cost;
                self.bus.broadcast(AgentEvent::Usage {
                    input_tokens, output_tokens,
                    cache_read_tokens: cache_read_input_tokens,
                    cache_creation_tokens: cache_creation_input_tokens,
                    model: m.to_string(), cost_usd: cost,
                });
            }

            // ── Direct forwarding (1:1 mapping) ──
            StreamEvent::Thinking(t) => self.bus.broadcast(AgentEvent::Thinking(t)),
            StreamEvent::Text(t) => self.bus.broadcast(AgentEvent::Text(t)),
            StreamEvent::ToolUseStart(name) => {
                self.tool_call_count += 1;
                self.tool_start_time = Some(Instant::now());
                self.bus.broadcast(AgentEvent::ToolStart {
                    tool_name: name, tool_id: String::new(),
                });
            }
            StreamEvent::ToolUseDelta(d) => self.bus.broadcast(AgentEvent::ToolArgsDelta(d)),
            StreamEvent::ToolUse { tool_name, tool_id, input } => {
                self.tool_start_time = Some(Instant::now());
                self.bus.broadcast(AgentEvent::ToolInvoke { tool_name, tool_id, input });
            }
            StreamEvent::ToolResultDelta { tool_id, delta } => {
                self.bus.broadcast(AgentEvent::ToolOutputDelta { tool_id, delta });
            }
            StreamEvent::ToolResult { tool_id, result } => {
                let elapsed = self.tool_start_time.take()
                    .map(|t| t.elapsed().as_millis() as u64);
                self.bus.broadcast(AgentEvent::ToolComplete { tool_id, result, elapsed_ms: elapsed });
            }
            StreamEvent::SubagentStart { subagent_id, agent_name, task_preview } => {
                self.bus.broadcast(AgentEvent::SubagentStart { subagent_id, agent_name, task_preview });
            }
            StreamEvent::SubagentUpdate { subagent_id, agent_name, status } => {
                self.bus.broadcast(AgentEvent::SubagentUpdate { subagent_id, agent_name, status });
            }
            StreamEvent::SubagentDone { subagent_id, agent_name, result_preview, duration_secs } => {
                self.bus.broadcast(AgentEvent::SubagentDone { subagent_id, agent_name, result_preview, duration_secs });
            }
            StreamEvent::SteeringDelivered { message } => {
                self.bus.broadcast(AgentEvent::Steered { message });
            }

            // ── Terminal events ──
            StreamEvent::Done => {
                self.turn_count += 1;
                self.bus.broadcast(AgentEvent::SessionStats {
                    total_input_tokens: self.total_input_tokens,
                    total_output_tokens: self.total_output_tokens,
                    total_cost_usd: self.total_cost_usd,
                    turn_count: self.turn_count,
                    tool_call_count: self.tool_call_count,
                });
                self.bus.broadcast(AgentEvent::TurnComplete);
            }
            StreamEvent::Error(e) => {
                // Clean up broken trailing messages
                self.cleanup_trailing_messages();
                self.bus.broadcast(AgentEvent::Error(e));
                self.bus.broadcast(AgentEvent::TurnComplete);
            }
        }
    }

    self.cancel = None;
    self.steer_tx = None;
    Ok(())
}
```

---

## 6. The AgentBus

The bus is the **core abstraction** — the universal interface between one ConversationDriver and N consumers. It wraps `tokio::broadcast` for events and `mpsc` for inbound messages.

The bus exposes two levels of API:
1. **Raw channels** — `subscribe()` + `inbound()` for direct consumers (TUI, custom apps)
2. **Transport convenience** — `connect()` for simple adapters (spawns a bridge task)

```rust
// src/transport/bus.rs

pub struct AgentBus {
    /// Broadcast channel for AgentEvents (driver → consumers)
    event_tx: broadcast::Sender<AgentEvent>,

    /// Aggregated inbound channel (consumers → driver)
    inbound_tx: mpsc::UnboundedSender<Inbound>,
    inbound_rx: mpsc::UnboundedReceiver<Inbound>,  // owned by driver
}

impl AgentBus {
    pub fn new() -> Self;

    // ── Tier 1: Raw channel access (for complex consumers like TUI) ──

    /// Subscribe to the agent event stream. Returns a broadcast receiver.
    /// Each subscriber gets every event independently. Late joiners should
    /// call sync_state() on the driver first.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent>;

    /// Get a sender for inbound messages. Multiple senders can coexist.
    /// All inbound messages are merged into one stream for the driver.
    pub fn inbound(&self) -> mpsc::UnboundedSender<Inbound>;

    // ── Tier 2: Transport convenience (for simple adapters) ──

    /// Connect a Transport impl to this bus. Spawns a bidirectional bridge task
    /// that calls recv()/send() in a select! loop. Returns immediately.
    pub fn connect<T: Transport>(&self, transport: T, sync: SyncState);

    // ── Driver-side API ──

    /// Broadcast an event to all subscribers (both tiers).
    pub fn broadcast(&self, event: AgentEvent);

    /// Receive the next inbound message from any consumer.
    pub async fn recv_inbound(&mut self) -> Option<Inbound>;

    /// Number of currently connected subscribers.
    pub fn subscriber_count(&self) -> usize;
}
```

### Bus-Transport Bridge (internal, for Tier 2)

When `connect()` is called, the bus spawns a task that bridges the Transport trait to channels:

```rust
fn connect<T: Transport>(&self, mut transport: T, sync: SyncState) {
    let mut event_rx = self.event_tx.subscribe();
    let inbound_tx = self.inbound_tx.clone();
    let name = transport.name().to_string();

    tokio::spawn(async move {
        // Deliver sync state
        transport.on_sync(sync).await;

        loop {
            tokio::select! {
                // Forward events from bus → transport
                event = event_rx.recv() => {
                    match event {
                        Ok(e) => {
                            if !transport.send(e).await {
                                break; // Transport disconnected
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("[{}] lagged by {} events", name, n);
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }

                // Forward input from transport → bus
                inbound = transport.recv() => {
                    match inbound {
                        Some(msg) => { let _ = inbound_tx.send(msg); }
                        None => break, // Transport disconnected
                    }
                }
            }
        }

        tracing::info!("[{}] transport disconnected", name);
    });
}
```

### TUI Direct Bus Usage (Tier 1 example)

```rust
// The TUI uses raw channels in its own event loop:
let event_rx = bus.subscribe();
let inbound_tx = bus.inbound();

loop {
    draw(&mut terminal, &app);

    tokio::select! {
        // TUI's own concerns
        _ = sleep(16ms), if animating => { /* tick */ }
        key = crossterm.next() => {
            // user submits → inbound_tx.send(Inbound::Message { ... })
            // user cancels → inbound_tx.send(Inbound::Cancel)
        }

        // Agent events from bus
        event = event_rx.recv() => {
            match event {
                Ok(AgentEvent::Text(t)) => app.append_text(t),
                Ok(AgentEvent::TurnComplete) => app.streaming = false,
                // ...
            }
        }
    }
}
```

Both tiers see the same events. Both can send inbound messages. The bus doesn't distinguish between them.

---

        tracing::info!("[{}] transport disconnected", name);
    });
}
```

### TUI Direct Bus Usage (Tier 1 example)

```rust
// The TUI uses raw channels in its own event loop:
let event_rx = bus.subscribe();
let inbound_tx = bus.inbound();

loop {
    draw(&mut terminal, &app);

    tokio::select! {
        // TUI's own concerns
        _ = sleep(16ms), if animating => { /* tick */ }
        key = crossterm.next() => {
            // user submits → inbound_tx.send(Inbound::Message { ... })
            // user cancels → inbound_tx.send(Inbound::Cancel)
        }

        // Agent events from bus
        event = event_rx.recv() => {
            match event {
                Ok(AgentEvent::Text(t)) => app.append_text(t),
                Ok(AgentEvent::TurnComplete) => app.streaming = false,
                // ...
            }
        }
    }
}
```

Both tiers see the same events. Both can send inbound messages. The bus doesn't distinguish between them.

---

## 7. Transport Implementations

### 7.1 StdioTransport (replaces `chat.rs` + `cli.rs`)

```
~25 lines of recv (read stdin lines, parse commands)
~40 lines of send (match on AgentEvent, print with ANSI codes)
```

**recv:** Spawn a blocking task that reads stdin line by line. Forward through an mpsc channel. Parse `/command` prefixes into `Inbound::Command`, everything else into `Inbound::Message`.

**send:** Match on `AgentEvent` variants. Print text directly. Use ANSI escapes for thinking (dim), tools (colored), errors (red). Ignore events that don't matter for terminal output (e.g. `SessionStats`).

### 7.2 TUI (replaces `chatui/main.rs`) — Direct Bus Consumer

The TUI is a **Tier 1 bus consumer**, NOT a Transport impl. It uses `bus.subscribe()` and `bus.inbound()` directly in its own event loop.

**Why not a Transport?** The TUI runs its own `tokio::select!` loop over terminal events (keyboard, mouse, paste), 60fps animation ticks, and agent events — all with shared mutable `App` state. The Transport trait assumes the bus controls polling cadence. The TUI needs to control its own.

**recv equivalent:** Crossterm `EventStream` → parse input → `inbound_tx.send(Inbound::Message { ... })`.
**send equivalent:** `event_rx.recv()` in the TUI's own select loop → match on `AgentEvent` → update `App` display state.

**Key change from current architecture:** `App` no longer owns `api_messages`, `total_input_tokens`, `total_output_tokens`, or `session_cost`. Those live in the ConversationDriver. The TUI receives `Meta(SessionStats { ... })` events and displays them. Session saving happens in the driver, not the TUI.

**The TUI's event loop barely changes** — it just stops owning the Runtime and receives `AgentEvent` instead of `StreamEvent`. Minimal surgery to the existing architecture.

### 7.3 WebSocketTransport (replaces `server.rs`)

**recv:** Read `ClientMessage` JSON from WebSocket. Map to `Inbound` variants:
- `ClientMessage::Message` → `Inbound::Message`
- `ClientMessage::Cancel` → `Inbound::Cancel`
- `ClientMessage::Command` → `Inbound::Command`
- `ClientMessage::Status` → `Inbound::SyncRequest`

**send:** Map `AgentEvent` to `ServerMessage` JSON. Send over WebSocket. The existing `ServerMessage` enum stays as the wire protocol — the transport does the mapping.

**Multi-client:** The WebSocket *server* itself is NOT a transport. The server accepts connections, and each connection creates a new `WebSocketTransport` instance connected to the same bus. This replaces the manual `broadcast::channel` in current `server.rs` — the bus IS the broadcast.

### 7.4 AgentTransport (replaces `agent.rs` loop)

The autonomous agent transport has NO user input — it's output-only with limit enforcement.

**recv:** Returns `None` immediately (or after detecting limit breach). The agent's boot message is injected via `driver.inject_user_message()` before the transport connects. The "prompt for handoff" logic is also injected programmatically.

**send:** Writes JSONL log entries. Updates heartbeat. Checks limits (tokens, cost, duration, tool calls). When a limit is hit, sends `Inbound::Cancel` back through the bus and returns `false`.

Actually — the agent transport has a subtle inversion: it needs to *inject* messages (the handoff prompt) and *enforce* limits. This makes it a **DriverPlugin** rather than a pure transport. See §7.4.1.

#### 7.4.1 AgentHarness — A transport + lifecycle manager

```rust
pub struct AgentHarness {
    config: AgentConfig,
    agent_dir: PathBuf,
    session_log: PathBuf,
    heartbeat_handle: JoinHandle<()>,
    session_start: Instant,
    total_tokens: u64,
    total_cost: f64,
    total_tool_calls: u64,
    watcher_exit_called: bool,
}
```

The harness wraps a `ConversationDriver` with agent-specific lifecycle:

```
1. Load config, soul, handoff
2. Create ConversationDriver
3. Inject boot message
4. Connect AgentTransport (logger + limit checker)
5. Run driver
6. On shutdown: request handoff if needed
7. Write stats, clean up heartbeat
```

This keeps the driver generic while the harness adds the autonomous agent protocol on top.

### 7.5 DiscordTransport (future — illustrative)

```rust
struct DiscordTransport {
    channel_id: ChannelId,
    http: Arc<Http>,
    rx: mpsc::UnboundedReceiver<DiscordMessage>,
    current_message: Option<MessageId>,  // for editing streamed responses
}

#[async_trait]
impl Transport for DiscordTransport {
    async fn recv(&mut self) -> Option<Inbound> {
        let msg = self.rx.recv().await?;
        Some(Inbound::Message { content: msg.content })
    }

    async fn send(&mut self, event: AgentEvent) -> bool {
        match event {
            AgentEvent::Text(t) => {
                // Accumulate text, edit Discord message every 500ms
                self.buffer.push_str(&t);
                if self.last_edit.elapsed() > Duration::from_millis(500) {
                    self.flush_to_discord().await;
                }
            }
            AgentEvent::TurnComplete => {
                self.flush_to_discord().await;
                self.current_message = None;
            }
            AgentEvent::ToolInvoke { tool_name, .. } => {
                // Short inline notification
                self.send_ephemeral(&format!("🔧 {}", tool_name)).await;
            }
            _ => {} // Discord doesn't need thinking blocks, deltas, etc.
        }
        true
    }

    fn name(&self) -> &str { "discord" }
}
```

**~50 lines.** The transport only handles Discord-specific formatting. All conversation logic lives in the driver.

### 7.6 SlackTransport, TelegramTransport (same pattern)

Each is ~50 lines implementing `Transport`. The hard part (API client, webhook setup, auth) is external crate territory. The *SynapsCLI integration* is trivially small.

---

## 8. The Router — Watcher as Multiplexer

### Current State

The watcher spawns `synaps-agent` as a child process and communicates via:
- Heartbeat files (agent → watcher)
- IPC socket with `WatcherCommand`/`WatcherResponse` (CLI → watcher)
- Handoff JSON files (agent → next agent session)
- Exit codes (agent → watcher)

### Target State

The watcher becomes a **router** — it hosts `ConversationDriver` instances in-process (no child processes) and multiplexes transport connections.

```
┌─────────────────────────────────────────────────────────────────┐
│  ROUTER (watcher process)                                        │
│                                                                   │
│  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐  │
│  │ ConversationDriver│  │ ConversationDriver│  │ ConversationDriver│  │
│  │ "patrol"          │  │ "scout"           │  │ "dexter"          │  │
│  │ AgentBus ──┐      │  │ AgentBus ──┐      │  │ AgentBus ──┐      │  │
│  └────────────│──────┘  └────────────│──────┘  └────────────│──────┘  │
│               │                      │                      │         │
│  ┌────────────┴──────────────────────┴──────────────────────┴──────┐  │
│  │                        Transport Router                         │  │
│  │  Routes incoming transport connections to the correct AgentBus  │  │
│  └──────┬──────────┬──────────┬──────────┬─────────────────────────┘  │
│         │          │          │          │                             │
└─────────│──────────│──────────│──────────│─────────────────────────────┘
          │          │          │          │
     TUI attach   WS client  Discord   Slack
     (patrol)     (scout)    (all)     (dexter)
```

### Router Commands (extends WatcherCommand)

```rust
pub enum WatcherCommand {
    // Existing
    Deploy { name: String },
    Stop { name: String },
    Status,
    AgentStatus { name: String },

    // New — transport routing
    Attach {
        agent_name: String,
        transport_type: String,  // "tui", "ws", etc.
    },
    Detach {
        agent_name: String,
        transport_id: String,
    },
    ListTransports {
        agent_name: Option<String>,  // None = all agents
    },
}
```

### `synaps attach <agent>` CLI command

```bash
# Attach a TUI to a running agent
$ synaps attach patrol

# Attach a simple stdout stream
$ synaps attach scout --mode stream

# Attach read-only (observe but can't send messages)
$ synaps attach dexter --readonly
```

### Attach Wire Protocol

The watcher's existing Unix socket supports request-response IPC (`WatcherCommand` → `WatcherResponse`). Attach reuses the same socket but switches to **streaming mode** after the handshake. Wire format: newline-delimited JSON (NDJSON).

```
Phase 1 — Handshake:
  Client → {"type": "attach", "agent": "patrol", "mode": "rw"}
  Server → {"type": "sync", "state": { ...SyncState... }}

Phase 2 — Bidirectional streaming (both directions, concurrent):
  Server → {"type": "event", "event": {"Text": "disk is at 92%"}}
  Server → {"type": "event", "event": {"Tool": {"Start": {"tool_name": "bash", ...}}}}
  Client → {"type": "message", "content": "check /var/log too"}
  Client → {"type": "cancel"}
  Server → {"type": "event", "event": "TurnComplete"}
  ...

Phase 3 — Detach:
  Client → {"type": "detach"}
  // connection closes
  // OR: client drops connection, watcher cleans up subscriber
```

**Mode switch:** The socket starts in command mode. If the first message is an `attach` command, the connection switches to streaming mode for its lifetime. Non-attach commands (`Deploy`, `Status`, etc.) work as before — request-response, then close.

**For TUI attach:** The CLI launches the TUI locally. Instead of creating its own `Runtime`, the TUI uses `bus.subscribe()` + `bus.inbound()` bridged over the Unix socket. The TUI runs locally with full rendering; only event data travels over the socket (kernel-level IPC, effectively zero latency).

**For read-only:** Watcher accepts the connection but ignores any `Inbound` messages from it. The subscriber only receives events.

### Multi-Transport Observing

Multiple transports can connect to the same agent simultaneously. The bus broadcasts to all:

```
Agent "patrol" bus:
  ├─ AgentTransport (JSONL logger — always connected)
  ├─ TUI (admin watching via `synaps attach patrol`)
  └─ DiscordTransport (channel #patrol-feed)
```

All three see the same `AgentEvent` stream. Each formats it differently. Each attach connection is independent — if one disconnects, others are unaffected.

---

## 9. Phased Migration Path

### Phase 0 — Define types (no behavioral changes)

**Files:**
- `src/transport/mod.rs` — module root, Transport trait
- `src/transport/events.rs` — `AgentEvent` enum
- `src/transport/inbound.rs` — `Inbound` enum
- `src/transport/sync.rs` — `SyncState` struct
- `src/transport/bus.rs` — `AgentBus`
- `src/transport/driver.rs` — `ConversationDriver` (stub)

**No binary changes.** Just define the types and make them compile. Export from `lib.rs`.

**Estimated effort:** Small. ~300 lines of type definitions.

### Phase 1 — ConversationDriver (the big one)

Extract the common loop from all binaries into `ConversationDriver`:
- Message history management
- StreamEvent → AgentEvent mapping
- Token/cost accumulation
- Session persistence
- Error recovery (pop broken trailing messages)
- Cancellation + steering forwarding
- `estimate_cost()` centralized

**Test:** Write a `NullTransport` that discards all events. Run the driver with it and verify message history, token tracking, and session saving work correctly.

**Estimated effort:** Medium. ~400 lines. This is mostly moving existing code.

### Phase 2 — StdioTransport + migrate `chat.rs`

Implement `StdioTransport`. Rewrite `chat.rs` to:
1. Create `ConversationDriver`
2. Connect `StdioTransport`
3. Call `driver.run()`

**Validation:** The new `chat.rs` should produce identical output to the current one.

**Estimated effort:** Small. ~80 lines for transport, `chat.rs` shrinks from 135 → ~30.

### Phase 3 — WebSocketTransport + migrate `server.rs`

Implement `WebSocketTransport`. Rewrite `server.rs` to:
1. Create `ConversationDriver`
2. On WebSocket connect, create `WebSocketTransport`, connect to bus
3. Remove all manual `ServerState` bookkeeping (the driver owns it now)

**Validation:** `client.rs` should work unchanged — the wire protocol (`ServerMessage`) is stable.

**Estimated effort:** Medium. ~120 lines for transport. `server.rs` shrinks from 560 → ~100.

### Phase 4 — TuiTransport + migrate `chatui`

The biggest migration. The TUI's `App` state needs to be split:
- **Conversation state** (api_messages, tokens, cost, session) → driver
- **Display state** (messages, scroll, cursor, effects, cache) → remains in App

The TUI transport wraps the existing event loop but delegates conversation concerns to the driver.

**Estimated effort:** Large. The TUI has ~800 lines of event handling that needs refactoring. The transport itself is ~60 lines, but untangling `App` from conversation state is the work.

### Phase 5 — AgentHarness + migrate `agent.rs`

Replace the agent binary's manual loop with:
1. `AgentHarness` creates `ConversationDriver`
2. Connects `AgentTransport` (logger + limit enforcer)
3. Injects boot message
4. Runs driver
5. Handles handoff on shutdown

**Estimated effort:** Medium. The limit enforcement and handoff logic move into the harness. `agent.rs` shrinks from 535 → ~50 (just arg parsing + harness setup).

### Phase 6 — Router integration (watcher)

Evolve the watcher from process supervisor to in-process router:
1. Instead of spawning `synaps-agent`, create `AgentHarness` in-process
2. Each agent gets its own `ConversationDriver` and `AgentBus`
3. Add `Attach`/`Detach` commands to IPC
4. Implement `synaps attach <agent>` CLI

**This is optional and can be deferred.** The current process-based supervisor works. The in-process router is an optimization that enables the transport attach pattern.

**Estimated effort:** Large. This is a significant rearchitecture of the watcher.

### Phase 7 — External transports (Discord, Slack, Telegram)

Once Phase 6 is done, adding external transports is trivially small. Each is a separate crate or feature-gated module that implements `Transport`.

---

## 10. File Structure

```
src/
├── transport/
│   ├── mod.rs              # Transport trait, re-exports
│   ├── events.rs           # AgentEvent enum
│   ├── inbound.rs          # Inbound enum
│   ├── sync.rs             # SyncState struct
│   ├── bus.rs              # AgentBus
│   ├── driver.rs           # ConversationDriver
│   ├── stdio.rs            # StdioTransport
│   ├── websocket.rs        # WebSocketTransport
│   ├── agent.rs            # AgentTransport + AgentHarness
│   └── null.rs             # NullTransport (for testing)
├── bin/
│   ├── chat.rs             # ~30 lines: create driver + StdioTransport
│   ├── server.rs           # ~100 lines: axum server + WebSocketTransport
│   ├── agent.rs            # ~50 lines: parse args + AgentHarness
│   └── ...
├── chatui/
│   ├── main.rs             # ~80 lines: create driver + TuiTransport
│   ├── transport.rs        # TuiTransport implementation
│   ├── app.rs              # Display state only (no conversation state)
│   └── ...
└── ...
```

---

## 11. Invariants & Contracts

1. **Single writer for conversation state.** Only `ConversationDriver` mutates message history, token counts, and session. Transports are read-only observers of conversation state.

2. **Transport independence.** A transport MUST NOT assume it's the only transport connected. Events may be consumed by others simultaneously.

3. **Late-join safety.** A transport connecting mid-conversation MUST receive `on_sync()` before any events. It MUST handle the case where it missed prior events gracefully.

4. **Backpressure tolerance.** If a transport is slow (e.g., Discord rate limits), the bus drops events for that transport rather than blocking the driver. The `broadcast::RecvError::Lagged` case is handled, not panicked.

5. **Clean shutdown.** `AgentEvent::Shutdown` is always the last event. Transports that receive it should clean up and return `false` from `send()`.

6. **Wire protocol stability.** `ServerMessage` (WebSocket protocol) is NOT changed. The `WebSocketTransport` maps between `AgentEvent` and `ServerMessage`. Existing `client.rs` continues to work.

7. **No allocation in the hot path.** `AgentEvent` variants use `String` (not `&str`) because they cross task boundaries via broadcast channels. This is the correct tradeoff — broadcast requires `Clone`, and the data is already heap-allocated from the API response parser.

---

## 12. Resolved Design Decisions

### D1: Should agents run in-process or as child processes?

**Decision:** Support both. Config flag per agent: `isolation = "process" | "task"`.
- `process`: current behavior, fork/exec `synaps-agent`. Crash isolation via OS.
- `task`: in-process tokio task. Lower latency, transport attach is trivial.
Phase 5 keeps child processes. Phase 6 adds in-process as an option.

### D2: How does the TUI fit?

**Decision:** The TUI is a **Tier 1 direct bus consumer**, NOT a Transport impl. It calls `bus.subscribe()` + `bus.inbound()` and uses the raw channels in its own `tokio::select!` loop alongside terminal events and animation ticks. The Transport trait is a convenience layer for simple adapters only. See §2 (Two-Tier Consumer Model) and §7.2.

### D3: What about read-only transports?

**Decision:** The attach handshake includes a `mode` field (`"rw"` or `"ro"`). Read-only connections can subscribe to events but the watcher ignores any inbound messages from them. For Transport impls, `recv()` returns `std::future::pending().await` (never resolves). For direct bus consumers, simply don't call `bus.inbound()`.

### D4: Event ordering guarantees?

**Decision:** Events from a single ConversationDriver are ordered (tokio broadcast preserves send order). Events from different drivers (different agents) have no ordering guarantee — they're independent streams.

### D5: How does the attach wire protocol work?

**Decision:** Same Unix socket, mode upgrade. The watcher's IPC socket supports both request-response (existing commands) and streaming (attach). Wire format: newline-delimited JSON. The first message determines the mode. See §8 (Attach Wire Protocol).

### D6: Driver error strategy

**Decision:** The driver never crashes. It emits errors and continues.
- **API errors:** Already retried internally by `api.rs` (configurable `api_retries`). On final failure, driver receives `StreamEvent::Error`, cleans up broken trailing messages, emits `AgentEvent::Error`, emits `AgentEvent::TurnComplete`. Session continues.
- **Tool panics:** Tool returns `Err`, gets stringified as tool result. Not a driver concern — handled at the tool execution layer.
- **Driver internal errors:** (serialization failure, session save failure) — emit `AgentEvent::Error`, continue if possible, emit `Meta(Shutdown { reason })` if fatal.
- **Transports decide presentation.** The driver emits structured error events. Each transport formats them for its medium (red text in TUI, error embed in Discord, JSON error in WebSocket).

### D7: AgentEvent design

**Decision:** Grouped enums with 7 top-level variants. Tool lifecycle (5 phases) and subagent lifecycle (3 phases) are sub-enums. Session metadata (usage, stats, steering, shutdown) is grouped under `Meta`. See §3.1.

---

## 13. What This Eliminates

| Current duplication | After |
|---|---|
| 4 copies of StreamEvent match arms | 1 (in ConversationDriver) |
| 4 copies of message history management | 1 (in ConversationDriver) |
| 3 copies of token accumulation | 1 (in ConversationDriver) |
| 3 copies of cost estimation | 1 (in ConversationDriver) |
| 3 copies of error recovery (pop broken messages) | 1 (in ConversationDriver) |
| 3 copies of session persistence | 1 (in ConversationDriver) |
| Manual broadcast channel in server.rs | AgentBus |
| Per-binary Runtime setup boilerplate | DriverConfig |

**Net reduction:** ~800 lines of duplicated logic eliminated. Each binary shrinks to its essence — the unique I/O handling that justifies its existence.

---

## 14. Summary

The architecture has three layers:

1. **Transport** (trait) — How events enter and leave the system. Two methods. Trivial to implement.
2. **ConversationDriver** (struct) — The conversation loop, written once. Owns all state. Emits enriched events.
3. **AgentBus** (struct) — The multiplexer. N transports observe one driver. Late-join safe. Backpressure tolerant.

The watcher optionally evolves into a **Router** — hosting multiple drivers and routing transport connections between them.

The migration is **incremental** — each phase produces a working system. No big bang rewrite. The existing binaries continue to work until their transport replacement is validated.

> *"Every system eventually reveals its true architecture. The question is whether you discovered it, or whether it discovered you."*
>
> The transports were always there. They were just wearing disguises.
