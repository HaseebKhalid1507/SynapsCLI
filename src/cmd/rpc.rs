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
//! This process runs no engine. It connects to the profile's session daemon
//! (auto-spawning it exactly as `synaps attach` does), creates an OWNED
//! session there (this process's cwd and env, secrets forwarded for the
//! tools), translates each [`RpcCommand`] into a `SessionCommand` /
//! `SessionQuery`, and each daemon event back into the [`RpcEvent`] frames
//! the in-process loop used to write. The session actor does the turn loop,
//! saves, context-head checkpoints, event auto-turns and turn budgets.
//! Owned: when this process goes away (EOF, `shutdown`, kill), the session
//! ends with it and any turn is cancelled.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use agent_engine::daemon::{self, registry, EnsureError, EXIT_DAEMON_LOST, EXIT_REFUSED, EXIT_VERSION};
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::{Attach, Hello};
use agent_engine::session::{
    AttachMode, ClientKind, ClientTransport, EndReason, HistoryMode, SessionCommand, SessionConfig,
    SessionEventWire, SessionQuery, SessionSetting, TransportError, TurnTrigger,
};
use agent_engine::{SessionEvent, StreamEvent, TurnError};
use synaps_cli::core::rpc_dispatch::{accumulate_usage, build_tools_list_body, map_stream_event, parse_frame, MAX_FRAME_BYTES};
use synaps_cli::core::rpc_protocol::{RpcAttachment, RpcCommand, RpcEvent, TurnUsage, RPC_PROTOCOL_VERSION};
use synaps_cli::runtime::openai::registry::{list_models, list_providers};

/// Capacity of the writer-task channel (frames): backpressure when the
/// parent reads slowly.
const WRITER_CHAN_CAP: usize = 256;
/// `shutdown`: how long an in-flight turn may run on before the session ends anyway.
const SHUTDOWN_TURN_WAIT: Duration = Duration::from_secs(30);
/// How long to wait for the daemon's `Ended` after `End{ClientQuit}`.
const END_WAIT: Duration = Duration::from_secs(10);
/// How long the writer may take to drain on exit.
const WRITER_DRAIN: Duration = Duration::from_secs(1);

// ─── stdout ──────────────────────────────────────────────────────────────────

/// Serialise an [`RpcEvent`] (no trailing newline). Never fails silently.
fn encode_event(ev: &RpcEvent) -> String {
    serde_json::to_string(ev).unwrap_or_else(|e| {
        tracing::error!(error = %e, "BUG: failed to serialise RpcEvent");
        format!(r#"{{"type":"error","message":"internal serialisation error: {e}"}}"#)
    })
}

/// The one task that owns stdout. A closed stdout ends it (the parent is gone).
fn spawn_writer(mut rx: mpsc::Receiver<RpcEvent>) -> JoinHandle<()> {
    tokio::spawn(async move {
        use std::io::Write;
        let mut stdout = std::io::stdout();
        while let Some(ev) = rx.recv().await {
            if writeln!(stdout, "{}", encode_event(&ev)).and_then(|_| stdout.flush()).is_err() {
                break;
            }
        }
    })
}

/// Before `Ready`: say why there is no session (stderr, plus one error frame
/// so a parent reading only stdout sees it) and exit `code`.
fn refuse(code: i32, message: String) -> ! {
    eprintln!("synaps rpc: {message}");
    println!("{}", encode_event(&RpcEvent::Error { id: None, message }));
    std::process::exit(code);
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
}

impl Turn {
    fn new(id: String) -> Self {
        Self { id, usage: empty_usage(), aborts: Vec::new(), cancel_sent: false }
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

struct PendingCompact {
    id: String,
    disclosure: Option<serde_json::Value>,
    started: bool,
}

/// Why the main loop stops.
enum Exit {
    /// The daemon connection or the event stream failed: error frame, exit 4.
    Lost(String),
    /// The session ended under us (daemon shutdown, eviction).
    Ended(EndReason),
}

struct Bridge {
    t: SocketTransport,
    out: mpsc::Sender<RpcEvent>,
    turn: Option<Turn>,
    queries: HashMap<u64, PendingQuery>,
    /// `Set` id → (parent id, requested model).
    settings: HashMap<u64, (String, String)>,
    new_sessions: VecDeque<String>,
    compact: Option<PendingCompact>,
    next_id: u64,
    /// First event that arrived while idle since the last turn: names the
    /// auto-turn it starts (`auto:<id>`).
    idle_event: Option<String>,
}

impl Bridge {
    async fn emit(&self, ev: RpcEvent) {
        let _ = self.out.send(ev).await;
    }

    async fn error(&self, id: Option<String>, message: impl Into<String>) {
        self.emit(RpcEvent::Error { id, message: message.into() }).await;
    }

    async fn respond(&self, id: String, command: &str, body: serde_json::Value) {
        self.emit(RpcEvent::Response { id, command: command.to_string(), body }).await;
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
            Err(e) => {
                let rid = match pending {
                    PendingQuery::Messages(i) | PendingQuery::State(i) | PendingQuery::Stats(i) => Some(i),
                    PendingQuery::Tools(i) => i,
                };
                self.error(rid, e).await;
            }
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
                if self.turn.is_some() {
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
                let cancel = match self.turn.as_mut() {
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
        if self.turn.is_some() {
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
        let cmd = SessionCommand::Submit { text: message, attachments: blocks };
        // The daemon closes a connection that sends an oversize frame (and
        // with it this owned session): refuse it here instead.
        let bytes = serde_json::to_vec(&cmd).map(|v| v.len()).unwrap_or(usize::MAX);
        if bytes > agent_engine::session::wire::MAX_FRAME_BYTES - 4096 {
            self.error(Some(id), format!("prompt too large for the session daemon ({bytes} bytes)")).await;
            return;
        }
        match self.send(cmd).await {
            Ok(()) => self.turn = Some(Turn::new(id)),
            Err(e) => self.error(Some(id), e).await,
        }
    }

    async fn compact(&mut self, id: String) {
        if self.turn.is_some() {
            self.error(Some(id), "cannot compact while a turn is running").await;
        } else if self.compact.is_some() {
            self.error(Some(id), "compaction already in progress").await;
        } else if let Err(e) = self.send(SessionCommand::Compact { instructions: None }).await {
            self.error(Some(id), e).await;
        } else {
            self.compact = Some(PendingCompact { id, disclosure: None, started: false });
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
            SessionEventWire::TurnStarted { trigger, .. } => {
                let idle_event = self.idle_event.take();
                if self.turn.is_none() {
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
            SessionEventWire::Aborted { .. } => self.finish(TurnEnd::Aborted).await,
            SessionEventWire::Idle => self.finish(TurnEnd::NoDone).await,
            SessionEventWire::QueryResult { id, value } => {
                if let Some(pending) = self.queries.remove(&id) {
                    self.on_query_result(pending, value).await;
                }
            }
            SessionEventWire::SettingChanged(applied) => {
                if let Some((id, model)) = self.settings.remove(&applied.id) {
                    if !applied.ok {
                        tracing::warn!(model, "rpc: set_model not applied");
                    }
                    self.respond(id, "set_model", serde_json::json!({ "model": model })).await;
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
                    c.disclosure = detail;
                    let (id, body) = (c.id.clone(), c.disclosure.clone().unwrap_or_default());
                    self.respond(id, "compact.disclosure", body).await;
                }
            }
            SessionEventWire::CompactionApplied { summary, .. } => {
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
            SessionEventWire::SystemNotice(text) => {
                // Lost events would mean lost usage (billing): never go on.
                if text.starts_with("event stream lagged") {
                    return Err(Exit::Lost(format!("lost session events ({text})")));
                }
                // The actor refused the compaction before starting it.
                if self.compact.as_ref().is_some_and(|c| !c.started) {
                    if let Some(c) = self.compact.take() {
                        self.error(Some(c.id), text).await;
                    }
                }
            }
            SessionEventWire::Refused { client, command, reason } if client == self.t.client_id() => {
                if command == "submit" {
                    if let Some(turn) = self.turn.take() {
                        self.error(Some(turn.id), reason).await;
                        for a in turn.aborts {
                            self.respond(a, "abort", serde_json::json!({ "ok": true })).await;
                        }
                        return Ok(());
                    }
                }
                self.error(None, format!("{command} refused: {reason}")).await;
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
            let id = match pending {
                PendingQuery::Messages(i) | PendingQuery::State(i) | PendingQuery::Stats(i) => Some(i),
                PendingQuery::Tools(i) => i,
            };
            self.error(id, err.to_string()).await;
            return;
        }
        let messages = || value.get("messages").cloned().unwrap_or_else(|| self.t.messages().len().into());
        match pending {
            PendingQuery::Messages(id) => {
                self.respond(id, "get_messages", serde_json::json!({ "messages": value })).await;
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

    /// A daemon reload cut the connection: whatever was in flight is gone
    /// (the checkpoint cancelled the turn). Close each request out so the
    /// parent never waits on it.
    async fn cut_by_reload(&mut self) {
        const CUT: &str = "interrupted by a session daemon reload";
        if let Some(mut turn) = self.turn.take() {
            let aborts = std::mem::take(&mut turn.aborts);
            self.error(Some(turn.id.clone()), CUT).await;
            self.emit(RpcEvent::AgentEnd { usage: turn.usage.clone() }).await;
            self.respond(turn.id.clone(), turn.command(), serde_json::json!({ "ok": false, "error": CUT })).await;
            for a in aborts {
                self.respond(a, "abort", serde_json::json!({ "ok": true })).await;
            }
        }
        let queries: Vec<PendingQuery> = self.queries.drain().map(|(_, q)| q).collect();
        for q in queries {
            let id = match q {
                PendingQuery::Messages(i) | PendingQuery::State(i) | PendingQuery::Stats(i) => Some(i),
                PendingQuery::Tools(i) => i,
            };
            self.error(id, CUT).await;
        }
        let settings: Vec<String> = self.settings.drain().map(|(_, (id, _))| id).collect();
        let news: Vec<String> = self.new_sessions.drain(..).collect();
        for id in settings.into_iter().chain(news) {
            self.error(Some(id), CUT).await;
        }
        if let Some(c) = self.compact.take() {
            self.error(Some(c.id), CUT).await;
        }
    }

    /// End the owned session cleanly: `End{ClientQuit}`, then wait for
    /// `Ended` so an immediate `--continue` of it does not race the dying
    /// session. `forward` = still write frames for what arrives meanwhile.
    async fn end_session(&mut self, forward: bool) {
        if self.t.send(SessionCommand::End { reason: EndReason::ClientQuit }).await.is_err() {
            return;
        }
        let _ = tokio::time::timeout(END_WAIT, async {
            while let Some(env) = self.t.next_event().await {
                if matches!(env.event, SessionEventWire::Ended { .. }) {
                    break;
                }
                if forward && self.on_event(env.event).await.is_err() {
                    break;
                }
            }
        })
        .await;
    }
}

/// Drop the frame sender and give the writer a bounded moment to drain.
async fn drain_writer(out: mpsc::Sender<RpcEvent>, writer: JoinHandle<()>) {
    drop(out);
    if tokio::time::timeout(WRITER_DRAIN, writer).await.is_err() {
        tracing::warn!("rpc: writer did not drain within 1 s; exiting anyway");
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

    // 2. The daemon: running, or auto-spawned exactly as `synaps attach`
    //    does. There is no in-process fallback.
    if !daemon::enabled() {
        refuse(EXIT_REFUSED, format!("{} — synaps rpc runs on the session daemon", daemon::DISABLED_NOTICE));
    }
    let opts = daemon::DaemonOpts { profile: active.clone(), ..Default::default() };
    match tokio::task::spawn_blocking(move || daemon::ensure_running(&opts)).await {
        Ok(Ok(_)) => {}
        Ok(Err(EnsureError::AutospawnDisabled)) => refuse(EXIT_REFUSED, super::attach::no_daemon_message(active.as_deref())),
        Ok(Err(EnsureError::Spawn(reason))) => refuse(EXIT_REFUSED, format!("daemon unavailable: {reason}")),
        Err(e) => refuse(EXIT_REFUSED, format!("daemon unavailable: {e}")),
    }
    let paths = registry::daemon_paths(active.as_deref());

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
        eprintln!("synaps rpc: {e}");
        std::process::exit(1);
    }

    // 4. Connect and create the OWNED session (secrets forwarded for tools).
    let hello = Hello::new(ClientKind::Rpc).with_history(HistoryMode::Full);
    let conn = match SocketTransport::connect(&paths.sock, hello).await {
        Ok(c) => c,
        Err(TransportError::Version { client, daemon }) => refuse(
            EXIT_VERSION,
            format!("protocol version mismatch (client {client}, daemon {daemon}); restart the daemon with this binary"),
        ),
        Err(e) => refuse(EXIT_REFUSED, format!("daemon unavailable: {e}")),
    };
    if let Some(msg) = daemon::profile_mismatch(conn.welcome.profile.as_deref(), active.as_deref()) {
        refuse(EXIT_REFUSED, msg);
    }
    let (t, snap) = match SocketTransport::attach(conn, Attach::create_owned(config, AttachMode::Mirror, true)).await {
        Ok(v) => v,
        Err(e @ TransportError::DaemonTooOld { .. }) => refuse(EXIT_REFUSED, e.to_string()),
        // `--continue` of a session another client has live, or one that
        // does not resolve: refused as before (exit 1, no Ready).
        Err(TransportError::Refused(m)) => {
            eprintln!("synaps rpc: cannot continue this session: {m}");
            std::process::exit(1);
        }
        Err(e) => refuse(EXIT_REFUSED, format!("session create failed: {e}")),
    };

    // 5. Writer, then Ready — the first byte on stdout.
    let (out, writer_rx) = mpsc::channel::<RpcEvent>(WRITER_CHAN_CAP);
    let writer = spawn_writer(writer_rx);
    let _ = out
        .send(RpcEvent::Ready {
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
        queries: HashMap::new(),
        settings: HashMap::new(),
        new_sessions: VecDeque::new(),
        compact: None,
        next_id: 0,
        idle_event: None,
    };
    drop(snap);

    // 6. Translate. stdin is not read while reconnecting or shutting down:
    //    the pipe buffers it.
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut shutdown: Option<tokio::time::Instant> = None;
    let exit = loop {
        if shutdown.is_some() && b.turn.is_none() {
            break None;
        }
        let deadline = shutdown.unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(3600));
        tokio::select! {
            ev = b.t.next_event() => match ev {
                Some(env) => {
                    if let Err(exit) = b.on_event(env.event).await {
                        break Some(exit);
                    }
                }
                None => {
                    if matches!(b.t.last_error(), Some(TransportError::Reloading { .. })) {
                        b.cut_by_reload().await;
                        if let Err(e) = b.t.reconnect(AttachMode::Mirror).await {
                            break Some(Exit::Lost(format!("could not reconnect after a daemon reload: {e}")));
                        }
                        continue;
                    }
                    let why = b.t.last_error().map(|e| e.to_string()).unwrap_or_else(|| "connection closed".into());
                    break Some(Exit::Lost(format!("lost the session daemon ({why})")));
                }
            },
            line = lines.next_line(), if shutdown.is_none() => match line {
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
                                shutdown = Some(tokio::time::Instant::now() + SHUTDOWN_TURN_WAIT);
                            }
                        }
                        Err(err_ev) => b.emit(err_ev).await,
                    }
                }
                Ok(None) | Err(_) => {
                    // EOF (or a broken stdin): the parent is done. End the
                    // session now — an in-flight turn is cancelled.
                    tracing::info!("stdin closed; ending the session");
                    b.end_session(false).await;
                    drain_writer(out, writer).await;
                    return Ok(());
                }
            },
            _ = tokio::time::sleep_until(deadline), if shutdown.is_some() => {
                tracing::warn!("shutdown: in-flight turn did not finish within 30 s; ending anyway");
                break None;
            }
        }
    };

    match exit {
        // `shutdown`: the turn finished (or timed out); end and exit 0.
        None => {
            b.end_session(true).await;
            drain_writer(out, writer).await;
            std::process::exit(0);
        }
        Some(Exit::Lost(message)) => {
            let id = b.turn.as_ref().map(|t| t.id.clone());
            eprintln!("synaps rpc: {message}");
            b.error(id, message).await;
            drop(b);
            drain_writer(out, writer).await;
            std::process::exit(EXIT_DAEMON_LOST);
        }
        Some(Exit::Ended(reason)) => {
            let message = format!("session ended by the daemon ({reason:?})");
            let id = b.turn.as_ref().map(|t| t.id.clone());
            eprintln!("synaps rpc: {message}");
            b.error(id, message).await;
            drop(b);
            drain_writer(out, writer).await;
            std::process::exit(EXIT_DAEMON_LOST);
        }
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
