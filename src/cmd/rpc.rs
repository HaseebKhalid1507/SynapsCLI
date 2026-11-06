//! `synaps rpc` — the line-JSON protocol on stdin/stdout, served by the
//! session daemon.
//!
//! One process = one synaps session. Bridges (Pria guest-agent, the Slack
//! bridge, synaps-bench) spawn one of these per conversation and speak LDJSON
//! frames on its `stdin` / `stdout`.
//!
//! # Protocol
//!
//! See `docs/rpc-protocol.md`. In brief:
//!
//! * Parent → child: [`RpcCommand`] frames (line-JSON on stdin)
//! * Child → parent: [`RpcEvent`] frames (line-JSON on stdout)
//! * First byte on stdout is always `{` — the `Ready` frame.
//! * Max inbound frame: 1 MiB.
//! * **stdout is reserved for protocol frames only.**
//!
//! # Architecture
//!
//! This process runs no engine. It spawns a PRIVATE session daemon in a
//! fresh run dir of its own (`<run>/rpc/<pid>-<rand>`, see [`PrivateDir`]):
//! the daemon inherits this process's env, config, base dir, mount
//! namespace and cgroup, exactly what the in-process engine had, and
//! nothing else ever attaches to it. There it creates an OWNED session
//! (this process's cwd and env, secrets forwarded for the tools),
//! translates each [`RpcCommand`] into a `SessionCommand` / `SessionQuery`,
//! and each daemon event back into the [`RpcEvent`] frames the in-process
//! loop used to write. The session actor does the turn loop, saves,
//! context-head checkpoints, event auto-turns and turn budgets. Owned: when
//! this process goes away (EOF, `shutdown`, SIGTERM, kill), the session
//! ends with it and any turn is cancelled; the daemon then exits.
//!
//! The daemon's events are read eagerly: frames wait for stdout in an
//! in-memory queue ([`Out`]), so a parent that reads stdout slowly never
//! stalls the socket (the daemon cuts an owner that lags, which would end
//! the session and lose usage).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch, Notify};
use tokio::time::Instant;

use agent_engine::daemon::{self, registry, EnsureError, EXIT_DAEMON_LOST, EXIT_REFUSED, EXIT_VERSION};
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::{Attach, Hello};
use agent_engine::session::{
    AttachMode, ClientKind, ClientTransport, EndReason, HistoryMode, SessionCommand, SessionConfig,
    SessionEventWire, SessionQuery, SessionSetting, TransportError, TurnTrigger,
};
use agent_engine::{AgentEvent, SessionEvent, StreamEvent, TurnError};
use synaps_cli::core::rpc_dispatch::{accumulate_usage, build_tools_list_body, map_stream_event, parse_frame, MAX_FRAME_BYTES};
use synaps_cli::core::rpc_protocol::{RpcAttachment, RpcCommand, RpcEvent, TurnUsage, RPC_PROTOCOL_VERSION};
use synaps_cli::runtime::openai::registry::{list_models, list_providers};

/// Bytes of frames that may wait for a slow stdout reader before `emit`
/// waits too (backpressure, which then reaches the daemon socket). Far
/// above any stall seen in practice: a long answer is a few hundred KiB.
const OUT_QUEUE_CAP: usize = 64 * 1024 * 1024;
/// `shutdown`: how long an in-flight turn may run on before the session ends anyway.
const SHUTDOWN_TURN_WAIT: Duration = Duration::from_secs(30);
/// How long to wait for the daemon's `Ended` after `End{ClientQuit}`.
const END_WAIT: Duration = Duration::from_secs(10);
/// SIGTERM: how long a cancelled turn gets to end (`Aborted`), and then the
/// session, before this process writes the terminal frames itself.
const TERM_WAIT: Duration = Duration::from_secs(2);
/// On exit, frames still queued are written as long as the parent can read
/// stdout (its read end open), for at most this long.
const WRITER_DRAIN_MAX: Duration = Duration::from_secs(60);
/// `compact`: the actor answers (`CompactionStarted` or a refusal) at once;
/// the summarisation itself may take minutes.
const COMPACT_START_WAIT: Duration = Duration::from_secs(30);
const COMPACT_RUN_WAIT: Duration = Duration::from_secs(600);
/// A private daemon idles out this soon (at most) after its session ends,
/// e.g. when this process was SIGKILLed.
const PRIVATE_IDLE_EXIT_MAX: Duration = Duration::from_secs(10);
/// Exit code after SIGTERM (128 + 15).
const EXIT_TERM: i32 = 143;

// ─── stderr ──────────────────────────────────────────────────────────────────

/// One diagnostic line on stderr, never blocking (a parent may pipe stderr
/// and never read it): see [`daemon::stderr_lossy`].
fn diag(message: impl std::fmt::Display) {
    daemon::stderr_lossy(format!("synaps rpc: {message}\n").as_bytes());
}

/// `tracing` for this process: warnings and errors on stderr, lossy like
/// [`diag`] (stdout is protocol frames only). The engine logs in the daemon.
fn init_diagnostics() {
    struct Lossy;
    impl std::io::Write for Lossy {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            daemon::stderr_lossy(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_writer(|| Lossy)
        .with_ansi(false)
        .try_init();
}

// ─── stdout ──────────────────────────────────────────────────────────────────

/// Serialise an [`RpcEvent`] (no trailing newline). Never fails silently.
fn encode_event(ev: &RpcEvent) -> String {
    serde_json::to_string(ev).unwrap_or_else(|e| {
        tracing::error!(error = %e, "BUG: failed to serialise RpcEvent");
        format!(r#"{{"type":"error","message":"internal serialisation error: {e}"}}"#)
    })
}

/// Frames bound for stdout, in order. The queue is unbounded up to
/// [`OUT_QUEUE_CAP`] bytes, so translating daemon events never waits on the
/// parent reading stdout.
#[derive(Clone)]
struct Out {
    tx: mpsc::UnboundedSender<String>,
    queued: Arc<AtomicUsize>,
    drained: Arc<Notify>,
    /// SIGTERM: never park at the cap any more (the exit path must run).
    term: watch::Receiver<bool>,
}

impl Out {
    async fn send(&self, ev: &RpcEvent) {
        let line = encode_event(ev);
        let n = line.len() + 1;
        loop {
            let drained = self.drained.notified();
            if self.queued.load(Ordering::Acquire) < OUT_QUEUE_CAP || self.tx.is_closed() || *self.term.borrow() {
                break;
            }
            let mut term = self.term.clone();
            tokio::select! {
                _ = drained => {}
                _ = term.wait_for(|t| *t) => {}
            }
        }
        self.queued.fetch_add(n, Ordering::AcqRel);
        if self.tx.send(line).is_err() {
            self.queued.fetch_sub(n, Ordering::AcqRel);
        }
    }
}

/// The one thread that owns stdout (blocking writes never hold up the
/// runtime). A closed stdout ends it (the parent is gone). `done` fires
/// when every frame is written or stdout closed.
fn spawn_writer(term: watch::Receiver<bool>) -> (Out, oneshot::Receiver<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let (queued, drained) = (Arc::new(AtomicUsize::new(0)), Arc::new(Notify::new()));
    let out = Out { tx, queued: Arc::clone(&queued), drained: Arc::clone(&drained), term };
    let (done_tx, done) = oneshot::channel();
    // A thread, not the (current-thread) runtime: a blocking stdout write
    // must never hold up reading the daemon socket.
    std::thread::spawn(move || {
        use std::io::Write;
        let stdout = std::io::stdout();
        let mut stdout = stdout.lock();
        while let Some(line) = rx.blocking_recv() {
            let ok = writeln!(stdout, "{line}").and_then(|_| stdout.flush()).is_ok();
            queued.fetch_sub(line.len() + 1, Ordering::AcqRel);
            drained.notify_waiters();
            if !ok {
                break;
            }
        }
        rx.close();
        drained.notify_waiters();
        let _ = done_tx.send(());
    });
    (out, done)
}

/// Let the writer finish once every sender is gone: as long as the parent
/// can still read stdout (a slow reader gets the cut turn's `agent_end`),
/// at most [`WRITER_DRAIN_MAX`].
async fn drain_writer(out: Out, mut done: oneshot::Receiver<()>) {
    let queued = Arc::clone(&out.queued);
    drop(out);
    let t0 = Instant::now();
    loop {
        if tokio::time::timeout(Duration::from_millis(100), &mut done).await.is_ok() {
            return;
        }
        let left = queued.load(Ordering::Acquire);
        if daemon::stdout_peer_closed() {
            tracing::warn!(queued_bytes = left, "stdout closed by the parent; exiting with frames unwritten");
            return;
        }
        if t0.elapsed() >= WRITER_DRAIN_MAX {
            tracing::warn!(queued_bytes = left, "stdout not drained within 60 s; exiting with frames unwritten");
            return;
        }
    }
}

/// Before `Ready`: say why there is no session (stderr, plus one error frame
/// so a parent reading only stdout sees it) and exit `code`.
fn refuse(code: i32, message: String) -> ! {
    diag(&message);
    println!("{}", encode_event(&RpcEvent::Error { id: None, message }));
    std::process::exit(code);
}

/// [`refuse`] once the private daemon may exist: stop it and remove its dir
/// first.
async fn refuse_private(private: PrivateDir, paths: &registry::DaemonPaths, code: i32, message: String) -> ! {
    private.remove(paths).await;
    refuse(code, message)
}

// ─── attachments ─────────────────────────────────────────────────────────────

/// Validate RPC attachment paths: must be absolute, no `..` components.
fn rpc_attachment_paths(attachments: &[RpcAttachment]) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::with_capacity(attachments.len());
    for a in attachments {
        let path = PathBuf::from(&a.path);
        if !path.is_absolute() {
            return Err(format!("attachment path must be absolute: {}", a.path));
        }
        if path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
            return Err(format!("attachment path must not contain ..: {}", a.path));
        }
        paths.push(path);
    }
    Ok(paths)
}

/// Load RPC user content: validate paths, load files, build content blocks,
/// validate the proposed history against the session model. Returns the
/// user message content and the attachment count.
async fn load_rpc_user_content(
    message: &str,
    attachments: &[RpcAttachment],
    model: &str,
    existing_messages: &[synaps_cli::SharedMessage],
) -> Result<(serde_json::Value, usize), String> {
    let paths = rpc_attachment_paths(attachments)?;
    if paths.is_empty() {
        return Ok((serde_json::Value::String(message.to_string()), 0));
    }
    let content = agent_engine::attachments::build_user_content(message, &paths).await?;
    let candidate = std::sync::Arc::new(serde_json::json!({"role": "user", "content": content}));
    let mut proposed = existing_messages.to_vec();
    proposed.push(candidate);
    agent_engine::runtime::attachments::validate_messages(model, &proposed)?;
    Ok((content, paths.len()))
}

/// The attachment blocks of a built user content, for `Submit`: the actor
/// rebuilds `[text block (non-empty text only), ...attachments]`, which is
/// exactly what `build_user_content` produced.
fn submit_blocks(message: &str, content: serde_json::Value) -> Vec<serde_json::Value> {
    match content {
        serde_json::Value::Array(mut blocks) => {
            if !message.is_empty() && !blocks.is_empty() {
                blocks.remove(0);
            }
            blocks
        }
        _ => Vec::new(),
    }
}

/// `get_messages` carries the messages as the provider sees them: the
/// engine's own bookkeeping keys on a message (`_synaps_at`, …) are not
/// part of the protocol.
fn strip_internal_keys(mut messages: serde_json::Value) -> serde_json::Value {
    if let Some(list) = messages.as_array_mut() {
        for m in list.iter_mut().filter_map(|m| m.as_object_mut()) {
            m.retain(|k, _| !k.starts_with("_synaps_"));
        }
    }
    messages
}

// ─── the bridge ──────────────────────────────────────────────────────────────

fn empty_usage() -> TurnUsage {
    TurnUsage {
        input_tokens: 0,
        output_tokens: 0,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
        cache_creation_5m: None,
        cache_creation_1h: None,
        model: None,
    }
}

/// The turn in flight, as the parent knows it.
struct Turn {
    /// The `Prompt`/`FollowUp` id, or `auto:<first-event-id>` /
    /// `auto:post-flush-<ms>` for an event-started turn.
    id: String,
    usage: TurnUsage,
    /// `abort` ids, answered after the turn's own terminal frames.
    aborts: Vec<String>,
    cancel_sent: bool,
    /// The prompt text (a pending prompt is matched by it).
    text: String,
    /// Pending only: the actor took the text as a steer of another turn...
    steered: bool,
    /// ...and that turn's engine took it into the history.
    consumed: bool,
}

impl Turn {
    fn new(id: String) -> Self {
        Self { id, usage: empty_usage(), aborts: Vec::new(), cancel_sent: false, text: String::new(), steered: false, consumed: false }
    }

    fn command(&self) -> &'static str {
        if self.id.starts_with("auto:") {
            "auto_turn"
        } else {
            "prompt"
        }
    }
}

/// How a turn ended.
enum TurnEnd {
    Done,
    Aborted,
    /// Cancelled by the daemon, not by an `abort` (cost cap, close, reload).
    Interrupted(String),
    Failed(TurnError),
    /// `Idle` with no `Done`/`Error`: the stream ended early.
    NoDone,
}

/// A query in flight, by the parent's id.
enum PendingQuery {
    Messages(String),
    State(String),
    Stats(String),
    Tools(Option<String>),
}

impl PendingQuery {
    fn into_id(self) -> Option<String> {
        match self {
            Self::Messages(i) | Self::State(i) | Self::Stats(i) => Some(i),
            Self::Tools(i) => i,
        }
    }
}

struct PendingCompact {
    id: String,
    /// `CompactionStarted` seen: the actor runs it (a timeout cancels it).
    started: bool,
    disclosure: Option<serde_json::Value>,
    /// No answer by then: error, and stdin is read again.
    deadline: Instant,
}

/// Why the main loop stops.
enum Exit {
    /// `shutdown` (after the turn, ≤ 30 s): exit 0.
    Shutdown,
    /// stdin closed: the turn is cancelled, what is in flight answered, exit 0.
    Eof,
    /// SIGTERM: as EOF, bounded by [`TERM_WAIT`], exit 143.
    Term,
    /// The daemon connection or the event stream failed: error frame, exit 4.
    Lost(String),
    /// The session ended under us (daemon shutdown, eviction).
    Ended(EndReason),
}

struct Bridge {
    t: SocketTransport,
    out: Out,
    turn: Option<Turn>,
    /// A prompt sent to the actor whose turn has not started. Normally the
    /// next `TurnStarted{User}`; when it crossed an event auto-turn starting
    /// (the actor then takes it as a steer), it waits for that turn: queued
    /// → it starts as `QueuedAuto`; consumed → answered at `Idle`; dropped
    /// (`Dequeued`) → an error.
    pending: Option<Turn>,
    queries: HashMap<u64, PendingQuery>,
    /// `Set` id → (parent id, requested model).
    settings: HashMap<u64, (String, String)>,
    new_sessions: VecDeque<String>,
    compact: Option<PendingCompact>,
    /// A `compact` timed out: its outcome may still arrive (logged only).
    compact_late: bool,
    next_id: u64,
    /// First event that arrived while idle since the last turn: names the
    /// auto-turn it starts (`auto:<id>`).
    idle_event: Option<String>,
    /// Why the daemon may cancel the turn on its own (`CostCapReached`).
    interrupt_note: Option<String>,
}

impl Bridge {
    async fn emit(&self, ev: RpcEvent) {
        self.out.send(&ev).await;
    }

    async fn error(&self, id: Option<String>, message: impl Into<String>) {
        self.emit(RpcEvent::Error { id, message: message.into() }).await;
    }

    async fn respond(&self, id: String, command: &str, body: serde_json::Value) {
        self.emit(RpcEvent::Response { id, command: command.to_string(), body }).await;
    }

    fn busy(&self) -> bool {
        self.turn.is_some() || self.pending.is_some()
    }

    fn next(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    async fn send(&self, cmd: SessionCommand) -> Result<(), String> {
        self.t.send(cmd).await.map_err(|e| format!("session daemon unavailable: {e}"))
    }

    async fn query(&mut self, query: SessionQuery, pending: PendingQuery) {
        let id = self.next();
        match self.send(SessionCommand::Query { id, query }).await {
            Ok(()) => {
                self.queries.insert(id, pending);
            }
            Err(e) => self.error(pending.into_id(), e).await,
        }
    }

    // ── commands ──

    /// `Some(())` = `shutdown`.
    async fn on_command(&mut self, cmd: RpcCommand) -> Option<()> {
        match cmd {
            RpcCommand::Prompt { id, message, attachments } => self.prompt(id, message, attachments).await,
            RpcCommand::FollowUp { id, message } => self.prompt(id, message, Vec::new()).await,
            RpcCommand::Compact { id } => self.compact(id).await,
            RpcCommand::NewSession { id } => {
                if self.busy() {
                    self.error(Some(id), "another prompt is in flight; abort first").await;
                } else if let Err(e) = self.send(SessionCommand::NewSession).await {
                    self.error(Some(id), e).await;
                } else {
                    self.new_sessions.push_back(id);
                }
            }
            RpcCommand::GetMessages { id } => self.query(SessionQuery::Messages, PendingQuery::Messages(id)).await,
            RpcCommand::SetModel { id, model } => {
                let set = self.next();
                match self.send(SessionCommand::Set { id: set, setting: SessionSetting::Model { model: model.clone() } }).await {
                    Ok(()) => {
                        self.settings.insert(set, (id, model));
                    }
                    Err(e) => self.error(Some(id), e).await,
                }
            }
            RpcCommand::GetAvailableModels { id } => self.available_models(id).await,
            RpcCommand::Abort { id } => {
                let cancel = match self.turn.as_mut().or(self.pending.as_mut()) {
                    Some(turn) => {
                        turn.aborts.push(id);
                        !std::mem::replace(&mut turn.cancel_sent, true)
                    }
                    None => {
                        self.respond(id, "abort", serde_json::json!({ "ok": true })).await;
                        false
                    }
                };
                if cancel {
                    if let Err(e) = self.send(SessionCommand::Cancel).await {
                        tracing::warn!(error = %e, "rpc: cancel not delivered");
                    }
                }
            }
            RpcCommand::GetSessionStats { id } => self.query(SessionQuery::Status, PendingQuery::Stats(id)).await,
            RpcCommand::GetState { id } => self.query(SessionQuery::Status, PendingQuery::State(id)).await,
            RpcCommand::ToolsList { id } => self.query(SessionQuery::ToolsSchema, PendingQuery::Tools(id)).await,
            RpcCommand::Shutdown => return Some(()),
        }
        None
    }

    async fn prompt(&mut self, id: String, message: String, attachments: Vec<RpcAttachment>) {
        if self.busy() {
            tracing::warn!(id, "rejected concurrent prompt — session busy");
            self.error(Some(id), "another prompt is in flight; abort first").await;
            return;
        }
        let blocks = if attachments.is_empty() {
            Vec::new()
        } else {
            let model = self.t.view().model.clone();
            match load_rpc_user_content(&message, &attachments, &model, self.t.messages()).await {
                Ok((content, count)) => {
                    self.respond(
                        id.clone(),
                        "attachments.disclosure",
                        serde_json::json!({
                            "count": count,
                            "message": format!("{count} attachment(s) loaded from local filesystem"),
                        }),
                    )
                    .await;
                    submit_blocks(&message, content)
                }
                Err(e) => {
                    self.error(Some(id), e).await;
                    return;
                }
            }
        };
        let cmd = SessionCommand::Submit { text: message.clone(), attachments: blocks };
        // The daemon closes a connection that sends an oversize frame (and
        // with it this owned session): refuse it here instead.
        let bytes = serde_json::to_vec(&cmd).map(|v| v.len()).unwrap_or(usize::MAX);
        if bytes > agent_engine::session::wire::MAX_FRAME_BYTES - 4096 {
            self.error(Some(id), format!("prompt too large for the session daemon ({bytes} bytes)")).await;
            return;
        }
        match self.send(cmd).await {
            Ok(()) => {
                let mut turn = Turn::new(id);
                turn.text = message;
                self.pending = Some(turn);
            }
            Err(e) => self.error(Some(id), e).await,
        }
    }

    async fn compact(&mut self, id: String) {
        if self.busy() {
            self.error(Some(id), "cannot compact while a turn is running").await;
        } else if self.compact.is_some() {
            self.error(Some(id), "compaction already in progress").await;
        } else if let Err(e) = self.send(SessionCommand::Compact { instructions: None }).await {
            self.error(Some(id), e).await;
        } else {
            self.compact = Some(PendingCompact { id, started: false, disclosure: None, deadline: Instant::now() + COMPACT_START_WAIT });
        }
    }

    async fn available_models(&self, id: String) {
        let mut models: Vec<serde_json::Value> = Vec::new();
        for (provider_key, _name, _has_key, _count) in &list_providers() {
            if let Some(list) = list_models(provider_key) {
                for (model_id, model_name, _default) in list {
                    models.push(serde_json::json!({
                        "provider": provider_key,
                        "model_id": model_id,
                        "model_name": model_name,
                    }));
                }
            }
        }
        self.respond(id, "get_available_models", serde_json::json!({ "models": models })).await;
    }

    // ── events ──

    async fn on_event(&mut self, ev: SessionEventWire) -> Result<(), Exit> {
        match ev {
            SessionEventWire::Stream(se) => self.on_stream(se).await,
            SessionEventWire::TurnStarted { trigger, user_text, .. } => {
                let idle_event = self.idle_event.take();
                let ours = match trigger {
                    TurnTrigger::User => self.pending.is_some(),
                    TurnTrigger::QueuedAuto => {
                        self.pending.as_ref().is_some_and(|p| user_text.as_deref() == Some(p.text.as_str()))
                    }
                    _ => false,
                };
                if ours {
                    self.turn = self.pending.take();
                } else if self.turn.is_none() {
                    let id = match (trigger, idle_event) {
                        (TurnTrigger::EventAuto, Some(event)) => format!("auto:{event}"),
                        (TurnTrigger::EventAuto, None) => {
                            format!("auto:post-flush-{}", chrono::Utc::now().timestamp_millis())
                        }
                        (other, _) => format!("auto:{other:?}-{}", chrono::Utc::now().timestamp_millis()).to_lowercase(),
                    };
                    self.turn = Some(Turn::new(id));
                }
            }
            // Our prompt crossed another turn starting: the actor steers it
            // into that turn (or queues it behind).
            SessionEventWire::Steered { text, .. } => {
                if let Some(p) = self.pending.as_mut().filter(|p| p.text == text) {
                    p.steered = true;
                }
            }
            // ...and that turn was cancelled before taking it.
            SessionEventWire::Dequeued { text } => {
                if let Some(p) = self.pending.take_if(|p| p.steered && p.text == text) {
                    self.error(Some(p.id), "prompt not delivered: the turn it was queued behind was cancelled").await;
                    for a in p.aborts {
                        self.respond(a, "abort", serde_json::json!({ "ok": true })).await;
                    }
                }
            }
            SessionEventWire::External(event) => {
                if self.turn.is_none() && self.idle_event.is_none() {
                    self.idle_event = Some(event.id.clone());
                }
                let formatted = agent_engine::events::format_event_for_agent(&event);
                let drained = agent_engine::engine::reactor::DrainedEvent {
                    event,
                    formatted,
                    disposition: agent_engine::engine::reactor::EventDisposition::Injected,
                };
                let payload = agent_engine::engine::reactor::event_payload_from_drained(&drained);
                self.emit(RpcEvent::Event { payload: Box::new(payload) }).await;
            }
            SessionEventWire::Aborted { .. } => {
                let ours = self.turn.as_ref().is_none_or(|t| t.cancel_sent);
                let end = if ours {
                    TurnEnd::Aborted
                } else {
                    let why = self.interrupt_note.take().unwrap_or_else(|| "turn cancelled by the session daemon".into());
                    TurnEnd::Interrupted(why)
                };
                self.finish(end).await;
            }
            SessionEventWire::CostCapReached { scope, cost, cap } => {
                self.interrupt_note = Some(format!("{scope} cost cap reached (${cost:.4} ≥ ${cap:.4})"));
            }
            // A started turn ending without Done/Error. An `Idle` racing
            // ahead of our prompt's `TurnStarted` belongs to what came before.
            SessionEventWire::Idle => {
                if self.turn.is_some() {
                    self.finish(TurnEnd::NoDone).await;
                }
                if let Some(p) = self.pending.take_if(|p| p.steered && p.consumed) {
                    // Steered into the turn that just ended, which answered
                    // it (its frames went out under that turn's id).
                    self.turn = Some(p);
                    self.finish(TurnEnd::Done).await;
                } else if let Some(p) = self.pending.take_if(|p| p.steered) {
                    // That turn ended (failed) without taking it.
                    self.error(Some(p.id), "prompt not delivered: the turn it was steered into ended first").await;
                    for a in p.aborts {
                        self.respond(a, "abort", serde_json::json!({ "ok": true })).await;
                    }
                }
            }
            SessionEventWire::QueryResult { id, value } => {
                if let Some(pending) = self.queries.remove(&id) {
                    self.on_query_result(pending, value).await;
                }
            }
            SessionEventWire::SettingChanged(applied) => {
                if let Some((id, model)) = self.settings.remove(&applied.id) {
                    if applied.ok {
                        self.respond(id, "set_model", serde_json::json!({ "model": applied.view.model })).await;
                    } else {
                        let why = applied.message.unwrap_or_else(|| format!("model {model} not applied"));
                        self.error(Some(id), why).await;
                    }
                }
            }
            SessionEventWire::Cleared { session_id } => {
                if let Some(id) = self.new_sessions.pop_front() {
                    self.respond(id, "new_session", serde_json::json!({ "session_id": session_id })).await;
                }
            }
            SessionEventWire::CompactionStarted { detail, .. } => {
                if let Some(c) = self.compact.as_mut() {
                    c.started = true;
                    c.deadline = Instant::now() + COMPACT_RUN_WAIT;
                    c.disclosure = detail;
                    let (id, body) = (c.id.clone(), c.disclosure.clone().unwrap_or_default());
                    self.respond(id, "compact.disclosure", body).await;
                }
            }
            SessionEventWire::CompactionApplied { summary, .. } => {
                if self.compact.is_none() && std::mem::take(&mut self.compact_late) {
                    tracing::warn!("a compaction finished after its request had timed out");
                }
                if let Some(c) = self.compact.take() {
                    self.respond(
                        c.id,
                        "compact",
                        serde_json::json!({ "summary": summary.unwrap_or_default(), "disclosure": c.disclosure }),
                    )
                    .await;
                }
            }
            SessionEventWire::CompactionFailed { message, .. } => {
                if let Some(c) = self.compact.take() {
                    self.error(Some(c.id), message).await;
                }
            }
            SessionEventWire::CompactionCancelled => {
                if let Some(c) = self.compact.take() {
                    self.error(Some(c.id), "compaction cancelled").await;
                }
            }
            // Lost events would mean lost usage (billing): never go on.
            SessionEventWire::SystemNotice(text) if text.starts_with("event stream lagged") => {
                return Err(Exit::Lost(format!("lost session events ({text})")));
            }
            SessionEventWire::Refused { client, command, reason } if client == self.t.client_id() => {
                if command == "submit" {
                    if let Some(turn) = self.pending.take() {
                        self.error(Some(turn.id), reason).await;
                        for a in turn.aborts {
                            self.respond(a, "abort", serde_json::json!({ "ok": true })).await;
                        }
                        return Ok(());
                    }
                }
                if command == "compact" {
                    if let Some(c) = self.compact.take() {
                        self.error(Some(c.id), reason).await;
                        return Ok(());
                    }
                }
                // The actor answers in order: the oldest request of that kind.
                let id = match command.as_str() {
                    "set" => self.settings.keys().min().copied().and_then(|k| self.settings.remove(&k)).map(|(id, _)| Some(id)),
                    "new_session" => self.new_sessions.pop_front().map(Some),
                    "query" => self.queries.keys().min().copied().and_then(|k| self.queries.remove(&k)).map(PendingQuery::into_id),
                    _ => None,
                };
                match id {
                    Some(id) => self.error(id, reason).await,
                    None => self.error(None, format!("{command} refused: {reason}")).await,
                }
            }
            // Headless: nobody can answer a confirm/secret prompt — deny it,
            // fail-closed, so the turn never waits on it.
            SessionEventWire::Prompt(p) => {
                let _ = self.t.send(SessionCommand::Answer { prompt_id: p.id, value: None }).await;
            }
            SessionEventWire::Ended { reason } => return Err(Exit::Ended(reason)),
            _ => {}
        }
        Ok(())
    }

    async fn on_stream(&mut self, se: StreamEvent) {
        // Our steered prompt went into the running turn's history.
        if let StreamEvent::Agent(AgentEvent::SteeringDelivered { message }) = &se {
            if let Some(p) = self.pending.as_mut().filter(|p| p.steered && &p.text == message) {
                p.consumed = true;
            }
        }
        let Some(turn) = self.turn.as_mut() else { return };
        match se {
            StreamEvent::Session(SessionEvent::Usage { .. }) => {
                if let StreamEvent::Session(ref usage) = se {
                    accumulate_usage(&mut turn.usage, usage);
                }
            }
            StreamEvent::Session(SessionEvent::Done) => self.finish(TurnEnd::Done).await,
            StreamEvent::Session(SessionEvent::Error(e)) => self.finish(TurnEnd::Failed(e)).await,
            other => {
                if let Some(frame) = map_stream_event(&other) {
                    self.emit(frame).await;
                }
            }
        }
    }

    /// The turn's terminal frames, exactly as the in-process loop wrote
    /// them: [error], agent_end, the prompt/auto_turn response, then any
    /// abort responses.
    async fn finish(&mut self, end: TurnEnd) {
        let Some(turn) = self.turn.take() else { return };
        let cancelled = !turn.aborts.is_empty() || matches!(end, TurnEnd::Aborted);
        let command = turn.command();
        let body = match end {
            _ if cancelled => serde_json::json!({ "ok": true, "cancelled": true }),
            TurnEnd::Failed(e) => {
                self.error(Some(turn.id.clone()), e.message.clone()).await;
                serde_json::json!({ "ok": false, "error": e.message, "outcome": e.outcome })
            }
            TurnEnd::Interrupted(why) => {
                self.error(Some(turn.id.clone()), why.clone()).await;
                serde_json::json!({ "ok": false, "error": why })
            }
            TurnEnd::NoDone => serde_json::json!({ "ok": false, "error": "stream ended without Done" }),
            TurnEnd::Done | TurnEnd::Aborted => serde_json::json!({ "ok": true }),
        };
        self.emit(RpcEvent::AgentEnd { usage: turn.usage.clone() }).await;
        self.respond(turn.id.clone(), command, body).await;
        for a in turn.aborts {
            self.respond(a, "abort", serde_json::json!({ "ok": true })).await;
        }
    }

    async fn on_query_result(&self, pending: PendingQuery, value: serde_json::Value) {
        if let Some(err) = value.get("error").and_then(|e| e.as_str()) {
            self.error(pending.into_id(), err.to_string()).await;
            return;
        }
        let messages = || value.get("messages").cloned().unwrap_or_else(|| self.t.messages().len().into());
        match pending {
            PendingQuery::Messages(id) => {
                let messages = strip_internal_keys(value);
                self.respond(id, "get_messages", serde_json::json!({ "messages": messages })).await;
            }
            PendingQuery::State(id) => {
                let body = serde_json::json!({
                    "streaming":     value["streaming"],
                    "model":         value["model"],
                    "session_id":    value["session"],
                    "message_count": messages(),
                });
                self.respond(id, "get_state", body).await;
            }
            PendingQuery::Stats(id) => {
                let body = serde_json::json!({
                    "input_tokens":  value["tokens"]["input"],
                    "output_tokens": value["tokens"]["output"],
                    "cost":          value["cost"],
                    "message_count": messages(),
                    "model":         value["model"],
                    "session_id":    value["session"],
                });
                self.respond(id, "get_session_stats", body).await;
            }
            PendingQuery::Tools(id) => {
                let schema = value.as_array().cloned().unwrap_or_default();
                self.respond(id.unwrap_or_default(), "tools_list", build_tools_list_body(&schema)).await;
            }
        }
    }

    /// The connection was cut (a daemon reload, or the daemon lost), or the
    /// session is over: whatever is still in flight is gone. Close each
    /// request out so the parent never waits on it; a running turn still
    /// gets its `agent_end` with the usage seen so far (what Pria meters).
    /// Returns whether anything was answered.
    async fn cut(&mut self, why: &str) -> bool {
        let mut answered = false;
        // A prompt cut off before it started never ran.
        if let Some(p) = self.pending.take() {
            answered = true;
            self.error(Some(p.id), why).await;
            for a in p.aborts {
                self.respond(a, "abort", serde_json::json!({ "ok": true })).await;
            }
        }
        if let Some(mut turn) = self.turn.take() {
            answered = true;
            let aborts = std::mem::take(&mut turn.aborts);
            self.error(Some(turn.id.clone()), why).await;
            self.emit(RpcEvent::AgentEnd { usage: turn.usage.clone() }).await;
            self.respond(turn.id.clone(), turn.command(), serde_json::json!({ "ok": false, "error": why })).await;
            for a in aborts {
                self.respond(a, "abort", serde_json::json!({ "ok": true })).await;
            }
        }
        let queries: Vec<PendingQuery> = self.queries.drain().map(|(_, q)| q).collect();
        for q in queries {
            answered = true;
            self.error(q.into_id(), why).await;
        }
        let settings: Vec<String> = self.settings.drain().map(|(_, (id, _))| id).collect();
        let news: Vec<String> = self.new_sessions.drain(..).collect();
        for id in settings.into_iter().chain(news) {
            answered = true;
            self.error(Some(id), why).await;
        }
        if let Some(c) = self.compact.take() {
            answered = true;
            self.error(Some(c.id), why).await;
        }
        answered
    }

    /// The parent is going (EOF, SIGTERM, `shutdown` after its wait): what
    /// is running counts as cancelled by it, like an `abort`.
    fn cancelled_by_parent(&mut self) {
        for t in self.turn.iter_mut().chain(self.pending.iter_mut()) {
            t.cancel_sent = true;
        }
    }

    /// SIGTERM: cancel the turn and give it `wait` to end (`Aborted`), its
    /// frames written as they come.
    async fn cancel_turn(&mut self, wait: Duration) {
        if self.turn.is_none() && self.pending.is_none() {
            return;
        }
        self.cancelled_by_parent();
        if self.t.send(SessionCommand::Cancel).await.is_err() {
            return;
        }
        let _ = tokio::time::timeout(wait, async {
            while self.busy() {
                match self.t.next_event().await {
                    Some(env) => {
                        if self.on_event(env.event).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        })
        .await;
    }

    /// End the owned session: `End{ClientQuit}`, then, for up to `wait`,
    /// keep translating what the actor still answers (it answers in order,
    /// before `Ended`) so an immediate `--continue` does not race the dying
    /// session. A turn still running ends as cancelled, with its usage.
    async fn end_session(&mut self, wait: Duration) {
        self.cancelled_by_parent();
        if self.t.send(SessionCommand::End { reason: EndReason::ClientQuit }).await.is_ok() {
            let _ = tokio::time::timeout(wait, async {
                while let Some(env) = self.t.next_event().await {
                    if self.on_event(env.event).await.is_err() {
                        break;
                    }
                }
            })
            .await;
        }
        if self.turn.is_some() {
            self.finish(TurnEnd::Aborted).await;
        }
    }
}

/// The private daemon's run dir, `<run>/rpc/<pid>-<rand>` (0700). This
/// process holds `rpc.lock` in it for its whole life; a dir whose lock and
/// daemon locks are all free belongs to nobody and is swept by the next
/// `synaps rpc`.
struct PrivateDir {
    dir: PathBuf,
    _lock: std::fs::File,
}

const PRIVATE_LOCK: &str = "rpc.lock";
/// A dir touched more recently than this may be mid-creation (its lock not
/// taken yet): never swept.
const PRIVATE_SWEEP_GRACE: Duration = Duration::from_secs(60);

impl PrivateDir {
    fn create() -> std::io::Result<Self> {
        use fs4::fs_std::FileExt;
        let root = synaps_cli::events::registry::registry_dir().join("rpc");
        private_mkdir(&root, true)?;
        sweep_private_dirs(&root);
        let dir = root.join(format!("{}-{:08x}", std::process::id(), rand::random::<u32>()));
        private_mkdir(&dir, false)?;
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true).write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let lock = opts.open(dir.join(PRIVATE_LOCK))?;
        if !FileExt::try_lock_exclusive(&lock)? {
            return Err(std::io::Error::other("private run dir lock is held"));
        }
        Ok(Self { dir, _lock: lock })
    }

    /// Stop the daemon (its session is over) and remove the dir; what does
    /// not go within a second is left to the next sweep.
    async fn remove(self, paths: &registry::DaemonPaths) {
        if registry::is_alive(paths) {
            let _ = tokio::time::timeout(Duration::from_secs(1), SocketTransport::shutdown(&paths.sock, false)).await;
            let t0 = Instant::now();
            while registry::is_alive(paths) && t0.elapsed() < Duration::from_secs(1) {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            if registry::is_alive(paths) {
                return;
            }
        }
        daemon::flush_private_stderr(Duration::from_millis(300));
        if let Err(e) = std::fs::remove_dir_all(&self.dir) {
            tracing::debug!(dir = %self.dir.display(), error = %e, "rpc: private run dir not removed");
        }
    }
}

fn private_mkdir(dir: &Path, existing_ok: bool) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(existing_ok);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(dir)
}

/// Whether someone holds the flock on `path` (a missing file: nobody).
fn lock_held(path: &Path) -> bool {
    use fs4::fs_std::FileExt;
    match std::fs::OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => !matches!(FileExt::try_lock_exclusive(&f), Ok(true)),
        Err(_) => false,
    }
}

/// Remove private run dirs left by rpc processes that are gone (SIGKILL)
/// once their daemon has idled out too.
fn sweep_private_dirs(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
        if !meta.is_dir() {
            continue;
        }
        if meta.modified().ok().and_then(|m| m.elapsed().ok()).is_none_or(|age| age < PRIVATE_SWEEP_GRACE) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&path) else { continue };
        let busy = files.flatten().any(|f| f.file_name().to_string_lossy().ends_with(".lock") && lock_held(&f.path()));
        if !busy {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

// ─── entry point ─────────────────────────────────────────────────────────────

/// Run `synaps rpc`: connect, create the owned session, write `Ready`, then
/// translate until stdin EOF, `shutdown`, or the session ends.
pub async fn run(
    continue_id: Option<String>,
    system: Option<String>,
    model: Option<String>,
    profile: Option<String>,
) -> anyhow::Result<()> {
    init_diagnostics();
    // 1. Profile: the daemon socket is chosen by it.
    if profile.is_some() {
        synaps_cli::config::set_profile(profile.clone());
    }
    let active = synaps_cli::config::get_profile();
    if let Some(want) = profile.as_deref().filter(|w| active.as_deref() != Some(*w)) {
        refuse(
            EXIT_REFUSED,
            format!("--profile {want} could not be applied (profile `{}` already in effect)", active.as_deref().unwrap_or("default")),
        );
    }

    // 2. A private daemon, spawned for this process in a run dir of its
    //    own: never the profile's shared daemon, never adopted by anything
    //    else. There is no in-process fallback.
    if !daemon::enabled() {
        refuse(EXIT_REFUSED, format!("{} — synaps rpc runs on a session daemon", daemon::DISABLED_NOTICE));
    }
    if !daemon::autospawn_enabled() {
        refuse(
            EXIT_REFUSED,
            "daemon autospawn is disabled (SYNAPS_DAEMON_AUTOSPAWN=0) — synaps rpc runs on a private session daemon it spawns".into(),
        );
    }
    let private = match PrivateDir::create() {
        Ok(p) => p,
        Err(e) => refuse(EXIT_REFUSED, format!("daemon unavailable: private run dir: {e}")),
    };
    let idle_exit = daemon::autospawn_idle_exit().map_or(PRIVATE_IDLE_EXIT_MAX, |d| d.min(PRIVATE_IDLE_EXIT_MAX));
    let opts = daemon::DaemonOpts {
        profile: active.clone(),
        runtime_dir: Some(private.dir.clone()),
        private: true,
        idle_exit: Some(idle_exit),
        ..Default::default()
    };
    let paths = opts.paths();
    let spawned = match tokio::task::spawn_blocking(move || daemon::ensure_running(&opts)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(EnsureError::AutospawnDisabled)) => Err("daemon autospawn is disabled".to_string()),
        Ok(Err(EnsureError::Spawn(reason))) => Err(format!("daemon unavailable: {reason}")),
        Err(e) => Err(format!("daemon unavailable: {e}")),
    };
    if let Err(message) = spawned {
        refuse_private(private, &paths, EXIT_REFUSED, message).await;
    }

    // 3. Session config: this process's cwd (and, via Hello, its env).
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let mut config = SessionConfig {
        continue_session: continue_id.map(Some),
        system,
        cwd: Some(cwd.clone()),
        model_override: model,
        ..Default::default()
    };
    if let Err(e) = agent_engine::session::client_args::resolve_client_session_args(&mut config, &cwd) {
        diag(e);
        private.remove(&paths).await;
        std::process::exit(1);
    }

    // 4. Connect and create the OWNED session (secrets forwarded for tools).
    let hello = Hello::new(ClientKind::Rpc).with_history(HistoryMode::Full);
    let conn = match SocketTransport::connect(&paths.sock, hello).await {
        Ok(c) => c,
        Err(TransportError::Version { client, daemon }) => {
            let why = format!("protocol version mismatch (client {client}, daemon {daemon}); restart the daemon with this binary");
            refuse_private(private, &paths, EXIT_VERSION, why).await
        }
        Err(e) => refuse_private(private, &paths, EXIT_REFUSED, format!("daemon unavailable: {e}")).await,
    };
    if let Some(msg) = daemon::profile_mismatch(conn.welcome.profile.as_deref(), active.as_deref()) {
        refuse_private(private, &paths, EXIT_REFUSED, msg).await;
    }
    let (t, snap) = match SocketTransport::attach(conn, Attach::create_owned(config, AttachMode::Mirror, true)).await {
        Ok(v) => v,
        Err(e @ TransportError::DaemonTooOld { .. }) => refuse_private(private, &paths, EXIT_REFUSED, e.to_string()).await,
        // `--continue` of a session another process has live, or one that
        // does not resolve: refused as before (exit 1, no Ready).
        Err(TransportError::Refused(m)) => {
            diag(format!("cannot continue this session: {m}"));
            private.remove(&paths).await;
            std::process::exit(1);
        }
        Err(e) => refuse_private(private, &paths, EXIT_REFUSED, format!("session create failed: {e}")).await,
    };

    // 5. Writer, then Ready — the first byte on stdout.
    let mut term = term_watch();
    let (out, writer) = spawn_writer(term.clone());
    out.send(&RpcEvent::Ready {
        session_id: snap.conversation.header.id.clone(),
        model: snap.view.model.clone(),
        protocol_version: RPC_PROTOCOL_VERSION,
    })
    .await;
    tracing::info!(session = %snap.meta.id, "synaps rpc ready");

    let mut b = Bridge {
        t,
        out: out.clone(),
        turn: None,
        pending: None,
        queries: HashMap::new(),
        settings: HashMap::new(),
        new_sessions: VecDeque::new(),
        compact: None,
        compact_late: false,
        next_id: 0,
        idle_event: None,
        interrupt_note: None,
    };
    drop(snap);

    // 6. Translate. stdin is not read while reconnecting, compacting or
    //    shutting down: the pipe buffers it. The daemon socket is always
    //    read: frames queue for stdout (see `Out`).
    let mut term_rebind = term.clone();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut shutdown: Option<Instant> = None;
    let far = || Instant::now() + Duration::from_secs(3600);
    let exit = loop {
        if shutdown.is_some() && !b.busy() {
            break Exit::Shutdown;
        }
        let deadline = shutdown.unwrap_or_else(far);
        let compact_deadline = b.compact.as_ref().map_or_else(far, |c| c.deadline);
        tokio::select! {
            ev = b.t.next_event() => match ev {
                Some(env) => {
                    if let Err(exit) = b.on_event(env.event).await {
                        break exit;
                    }
                }
                None => {
                    // A lagging owner is cut by the daemon: its events are
                    // gone (billing), so never reconnect (the transport
                    // reports this over a pending reload).
                    if let Some(TransportError::OwnerLagged { dropped }) = b.t.last_error() {
                        break Exit::Lost(format!(
                            "lost {dropped} session events: the daemon disconnected this client for lagging"
                        ));
                    }
                    if matches!(b.t.last_error(), Some(TransportError::Reloading { .. })) {
                        b.cut("interrupted by a session daemon reload").await;
                        // Terminal on any error, including the daemon
                        // refusing a re-bind to a session that ended.
                        let rebound = tokio::select! {
                            r = b.t.reconnect(AttachMode::Mirror) => r,
                            _ = term_rebind.wait_for(|t| *t) => break Exit::Term,
                        };
                        if let Err(e) = rebound {
                            break Exit::Lost(format!("could not reconnect after a daemon reload: {e}"));
                        }
                        continue;
                    }
                    let why = b.t.last_error().map(|e| e.to_string()).unwrap_or_else(|| "connection closed".into());
                    break Exit::Lost(format!("lost the session daemon ({why})"));
                }
            },
            // Not while our `compact` runs: the old loop ran it inline, so
            // what the parent sent meanwhile waited for it (and a prompt
            // sent mid-compaction would be queued, then handed back unsent).
            line = lines.next_line(), if shutdown.is_none() && b.compact.is_none() => match line {
                Ok(Some(line)) => {
                    let line = line.trim_end_matches('\r');
                    if line.trim().is_empty() {
                        continue;
                    }
                    match parse_frame(line, MAX_FRAME_BYTES) {
                        Ok(cmd) => {
                            tracing::debug!(?cmd, "received RpcCommand");
                            if b.on_command(cmd).await.is_some() {
                                tracing::info!("shutdown received; ending the session after the turn");
                                shutdown = Some(Instant::now() + SHUTDOWN_TURN_WAIT);
                            }
                        }
                        Err(err_ev) => b.emit(err_ev).await,
                    }
                }
                // EOF (or a broken stdin): the parent is done.
                Ok(None) | Err(_) => break Exit::Eof,
            },
            _ = tokio::time::sleep_until(compact_deadline), if b.compact.is_some() => {
                if let Some(c) = b.compact.take() {
                    // Not started: nothing to cancel (a Cancel would hit
                    // whatever runs next).
                    if c.started {
                        tracing::warn!("compaction not finished in time; cancelling it");
                        let _ = b.t.send(SessionCommand::Cancel).await;
                    } else {
                        tracing::warn!("compaction not answered in time");
                    }
                    b.compact_late = true;
                    b.error(Some(c.id), "compaction timed out").await;
                }
            }
            _ = term.wait_for(|t| *t) => break Exit::Term,
            _ = tokio::time::sleep_until(deadline), if shutdown.is_some() => {
                tracing::warn!("shutdown: in-flight turn did not finish within 30 s; ending anyway");
                break Exit::Shutdown;
            }
        }
    };

    let code = match exit {
        Exit::Shutdown => {
            b.end_session(END_WAIT).await;
            b.cut("session ended").await;
            0
        }
        Exit::Eof => {
            tracing::info!("stdin closed; ending the session");
            b.end_session(END_WAIT).await;
            b.cut("session ended: stdin closed").await;
            0
        }
        Exit::Term => {
            tracing::info!("SIGTERM; cancelling and ending the session");
            b.cancel_turn(TERM_WAIT).await;
            b.end_session(TERM_WAIT).await;
            b.cut("session ended: terminated").await;
            EXIT_TERM
        }
        Exit::Lost(message) => {
            diag(&message);
            if !b.cut(&message).await {
                b.error(None, message).await;
            }
            EXIT_DAEMON_LOST
        }
        Exit::Ended(reason) => {
            let message = format!("session ended by the daemon ({reason:?})");
            diag(&message);
            if !b.cut(&message).await {
                b.error(None, message).await;
            }
            EXIT_DAEMON_LOST
        }
    };
    drop(b);
    drain_writer(out, writer).await;
    private.remove(&paths).await;
    std::process::exit(code);
}

/// SIGTERM as a stream (never fires where there is no such signal).
struct TermSignal(#[cfg(unix)] Option<tokio::signal::unix::Signal>);

impl TermSignal {
    async fn recv(&mut self) {
        #[cfg(unix)]
        if let Some(s) = self.0.as_mut() {
            s.recv().await;
            return;
        }
        std::future::pending::<()>().await
    }
}

/// SIGTERM as a latch every waiter can see (the main loop, a reconnect, a
/// send parked at the stdout cap).
fn term_watch() -> watch::Receiver<bool> {
    let (tx, rx) = watch::channel(false);
    let mut sig = term_signal();
    tokio::spawn(async move {
        sig.recv().await;
        let _ = tx.send(true);
        // Keep the sender: a closed channel would read as "never".
        std::future::pending::<()>().await
    });
    rx
}

fn term_signal() -> TermSignal {
    #[cfg(unix)]
    {
        TermSignal(tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok())
    }
    #[cfg(not(unix))]
    {
        TermSignal()
    }
}

#[cfg(test)]
mod rpc_attachment_tests {
    use super::*;

    #[test]
    fn rpc_attachment_paths_validates_absolute_and_no_parent() {
        let good = vec![RpcAttachment { path: "/tmp/image.png".into(), name: None, mime: None }];
        assert!(rpc_attachment_paths(&good).is_ok());
        let relative = vec![RpcAttachment { path: "relative/image.png".into(), name: None, mime: None }];
        assert!(rpc_attachment_paths(&relative).is_err());
        let dotdot = vec![RpcAttachment { path: "/tmp/../etc/passwd".into(), name: None, mime: None }];
        assert!(rpc_attachment_paths(&dotdot).is_err());
        assert!(rpc_attachment_paths(&[]).is_ok());
    }

    #[tokio::test]
    async fn load_rpc_user_content_text_only() {
        let (content, count) = load_rpc_user_content("hello", &[], "claude-sonnet-4-20250514", &[]).await.unwrap();
        assert_eq!(content, "hello");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn load_rpc_user_content_missing_file_fails() {
        let attachments =
            vec![RpcAttachment { path: "/tmp/nonexistent_rpc_test_file.png".into(), name: None, mime: None }];
        let result = load_rpc_user_content("hello", &attachments, "claude-sonnet-4-20250514", &[]).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn load_rpc_user_content_real_text_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "file body").unwrap();
        let attachments = vec![RpcAttachment { path: path.to_str().unwrap().into(), name: None, mime: None }];
        let (content, count) =
            load_rpc_user_content("explain", &attachments, "claude-sonnet-4-20250514", &[]).await.unwrap();
        assert_eq!(count, 1);
        let arr = content.as_array().unwrap();
        assert_eq!(arr[0]["text"], "explain");
        assert_eq!(arr[1]["type"], "document");
        // Submit carries the attachment blocks only; the actor re-adds the text.
        let blocks = submit_blocks("explain", content.clone());
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "document");
        assert_eq!(submit_blocks("", serde_json::json!([{"type": "document"}])).len(), 1);
    }
}
