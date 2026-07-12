//! `synaps rpc` — headless line-JSON server on stdin/stdout.
//!
//! One process = one synaps session. The bridge daemon spawns one of these per
//! Slack thread and communicates via LDJSON frames on the child's
//! `stdin` / `stdout` pipes.
//!
//! # Protocol
//!
//! See `docs/rpc-protocol.md` and `synaps-bridge.SPEC.md §4` for the full
//! wire-format specification. In brief:
//!
//! * Parent → child: [`synaps_cli::core::rpc_protocol::RpcCommand`] frames (line-JSON on stdin)
//! * Child → parent: [`synaps_cli::core::rpc_protocol::RpcEvent`] frames (line-JSON on stdout)
//! * First byte on stdout is always `{` — the `Ready` frame.
//! * Max inbound frame: 1 MiB.
//! * **stdout is reserved for protocol frames only.** All `tracing::*` output
//!   goes to the log file / stderr.

use anyhow::Context;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use futures::StreamExt;

use synaps_cli::{
    Runtime, Session, SessionEvent, StreamEvent,
    core::rpc_protocol::{RpcAttachment, RpcCommand, RpcEvent, TurnUsage, RPC_PROTOCOL_VERSION},
    core::rpc_dispatch::{
        accumulate_usage, build_user_content, build_tools_list_body, map_stream_event, parse_frame, MAX_FRAME_BYTES,
    },
    engine::setup::{self, EngineOpts},
    engine::reactor::{drain_event_queue, event_payload_from_drained},
};
use synaps_cli::runtime::openai::registry::{list_models, list_providers};

// ─── Constants ───────────────────────────────────────────────────────────────

/// Capacity of the writer-task channel (frames). Provides backpressure when the
/// parent reads slowly.
const WRITER_CHAN_CAP: usize = 256;

// ─── State ───────────────────────────────────────────────────────────────────

/// Tracks a currently in-flight streaming prompt.
struct InFlight {
    /// Correlation id of the originating Prompt/FollowUp command.
    /// Retained for diagnostics and future Task 3 e2e introspection.
    prompt_id: String,
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

/// Full runtime state for the RPC session.
struct RpcState {
    runtime: Runtime,
    session: Session,
    api_messages: Vec<synaps_cli::SharedMessage>,
    total_input_tokens: u64,
    total_output_tokens: u64,
    session_cost: f64,
    in_flight: Option<InFlight>,
    /// Events buffered while streaming is in flight. Flushed as `Event` frames
    /// when the turn completes (Done path), then injected for the next turn.
    pending_events: Vec<String>,
}

impl RpcState {
    /// Persist the current conversation to the session file. No-op if the
    /// message list is empty.
    async fn save_session(&mut self) {
        if self.api_messages.is_empty() {
            return;
        }
        self.session.api_messages = self.api_messages.clone();
        self.session.total_input_tokens = self.total_input_tokens;
        self.session.total_output_tokens = self.total_output_tokens;
        self.session.session_cost = self.session_cost;
        self.session.model = self.runtime.model().to_string();
        self.session.system_prompt = self.runtime.system_prompt().map(|s| s.to_string());
        self.session.thinking_level = self.runtime.thinking_level().to_string();
        self.session.updated_at = chrono::Utc::now();
        self.session.auto_title();
        if let Err(e) = self.session.save().await {
            tracing::error!(error = %e, "failed to save session");
        }
    }

    /// Returns `true` if a streaming task is currently running.
    fn is_streaming(&self) -> bool {
        self.in_flight.is_some()
    }
}

// ─── Frame serialisation ──────────────────────────────────────────────────────

/// Serialise an [`RpcEvent`] to a JSON string (no trailing newline).
///
/// Serialisation of well-typed enum variants should never fail. If it does, a
/// fallback error frame is returned so the writer task never silently drops data.
fn encode_event(ev: &RpcEvent) -> String {
    serde_json::to_string(ev).unwrap_or_else(|e| {
        tracing::error!(error = %e, "BUG: failed to serialise RpcEvent");
        format!(r#"{{"type":"error","message":"internal serialisation error: {e}"}}"#)
    })
}

/// Generate a UUID v4 string — used for synthesised EventPayload ids when the
/// original DrainedEvent metadata is not available (buffered-event flush path).
fn uuid_v4_simple() -> String {
    uuid::Uuid::new_v4().to_string()
}

// ─── Writer task ──────────────────────────────────────────────────────────────

/// Spawn a dedicated task that owns stdout and serialises frames from a channel.
///
/// All code paths must route frames through the returned sender; nothing else
/// may write to stdout so protocol frames never interleave with diagnostics.
fn spawn_writer(mut rx: mpsc::Receiver<RpcEvent>) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            // println! is correct here — stdout IS the protocol channel.
            println!("{}", encode_event(&ev));
        }
    })
}

// ─── Streaming task ───────────────────────────────────────────────────────────

/// Spawn a streaming task for a `Prompt` or `FollowUp` command.
///
/// The task takes a snapshot of `api_messages` while briefly holding the mutex,
/// then releases it before the long-running LLM stream begins so that `Abort`
/// and read-only commands (`GetState`, `GetSessionStats`) can still acquire the
/// lock while the stream is in flight.
async fn spawn_prompt(
    prompt_id: String,
    state: Arc<Mutex<RpcState>>,
    writer_tx: mpsc::Sender<RpcEvent>,
) -> InFlight {
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    let cancel_check = cancel.clone();
    let pid = prompt_id.clone();
    let wtx = writer_tx.clone();

    let handle = tokio::spawn(async move {
        // Snapshot message history; release lock before blocking on the stream.
        // Vec<SharedMessage> clone = pointer bumps only.
        let messages: Vec<synaps_cli::SharedMessage> =
            state.lock().await.api_messages.clone();

        // Acquire lock only long enough to start the stream future.
        let mut stream = {
            let st = state.lock().await;
            st.runtime
                .run_stream_with_messages(messages, cancel_clone, None, None, false)
                .await
        };

        let mut usage_acc = TurnUsage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_creation_5m: None,
            cache_creation_1h: None,
            model: None,
        };

        while let Some(ev) = stream.next().await {
            // Peel off MessageHistory first so we can MOVE the payload into
            // state instead of cloning it (the vec can be several MB).
            if let StreamEvent::Session(SessionEvent::MessageHistory(msgs)) = ev {
                let mut st = state.lock().await;
                st.api_messages = msgs;
                st.save_session().await;
                continue;
            }
            match &ev {
                StreamEvent::Session(se @ SessionEvent::Usage {
                    input_tokens,
                    output_tokens,
                    ..
                }) => {
                    accumulate_usage(&mut usage_acc, se);
                    let mut st = state.lock().await;
                    st.total_input_tokens += input_tokens;
                    st.total_output_tokens += output_tokens;
                    continue;
                }
                // ── Turn complete ───────────────────────────────────────────
                StreamEvent::Session(SessionEvent::Done) => {
                    let _ = wtx.send(RpcEvent::AgentEnd { usage: usage_acc.clone() }).await;
                    // Flush events buffered while streaming was active.
                    // Lock briefly to drain pending_events; release before sends.
                    let buffered: Vec<String> = {
                        let mut st = state.lock().await;
                        let to_inject = std::mem::take(&mut st.pending_events);
                        // Inject buffered events into messages for the next turn.
                        for formatted in &to_inject {
                            st.api_messages.push(std::sync::Arc::new(
                                serde_json::json!({"role": "user", "content": formatted})
                            ));
                        }
                        to_inject
                    };
                    // Forward buffered events as Event frames (client can follow up).
                    for formatted in buffered {
                        // Re-build a minimal EventPayload from the formatted string.
                        // (Full DrainedEvent is not available here — just the string.)
                        let ev_frame = RpcEvent::Event {
                            payload: Box::new(synaps_cli::core::rpc_protocol::EventPayload {
                                id: uuid_v4_simple(),
                                source: "buffered".into(),
                                severity: "medium".into(),
                                content_type: "message".into(),
                                text: formatted.clone(),
                                timestamp: chrono::Utc::now().to_rfc3339(),
                                formatted,
                            }),
                        };
                        let _ = wtx.send(ev_frame).await;
                    }
                    let _ = wtx
                        .send(RpcEvent::Response {
                            id: pid.clone(),
                            command: "prompt".to_string(),
                            body: serde_json::json!({ "ok": true }),
                        })
                        .await;
                    state.lock().await.in_flight = None;
                    return;
                }
                // ── Turn error (always returns early) ───────────────────────
                StreamEvent::Session(SessionEvent::Error(msg)) => {
                    // If we requested cancellation, the engine surfaces the
                    // cancel as a downstream `SessionEvent::Error` (typically
                    // "operation canceled" from the OpenAI stream layer). That
                    // is a benign abort, not a failure — report ok: true,
                    // cancelled: true exactly as the stream-exhausted branch
                    // below would, and skip the noisy Error frame.
                    if cancel_check.is_cancelled() {
                        let _ = wtx
                            .send(RpcEvent::AgentEnd { usage: usage_acc.clone() })
                            .await;
                        let _ = wtx
                            .send(RpcEvent::Response {
                                id: pid.clone(),
                                command: "prompt".to_string(),
                                body: serde_json::json!({ "ok": true, "cancelled": true }),
                            })
                            .await;
                        state.lock().await.in_flight = None;
                        return;
                    }
                    let _ = wtx
                        .send(RpcEvent::Error {
                            id: Some(pid.clone()),
                            message: msg.clone(),
                        })
                        .await;
                    let _ = wtx.send(RpcEvent::AgentEnd { usage: usage_acc.clone() }).await;
                    let _ = wtx
                        .send(RpcEvent::Response {
                            id: pid.clone(),
                            command: "prompt".to_string(),
                            body: serde_json::json!({ "ok": false, "error": msg }),
                        })
                        .await;
                    state.lock().await.in_flight = None;
                    return;
                }
                _ => {}
            }

            // Forward mapped LLM / Agent events to the parent.
            if let Some(rpc_ev) = map_stream_event(&ev) {
                if wtx.send(rpc_ev).await.is_err() {
                    tracing::warn!("writer channel closed; aborting stream early");
                    break;
                }
            }
        }

        // Stream ended without Session::Done. If we requested cancellation, that's an
        // orderly abort; otherwise it's a silent failure (provider drop, extension
        // crash, etc.) and the parent must be told.
        let cancelled = cancel_check.is_cancelled();
        let _ = wtx.send(RpcEvent::AgentEnd { usage: usage_acc.clone() }).await;
        let body = if cancelled {
            serde_json::json!({ "ok": true, "cancelled": true })
        } else {
            serde_json::json!({
                "ok": false,
                "error": "stream ended without Done"
            })
        };
        let _ = wtx
            .send(RpcEvent::Response {
                id: pid.clone(),
                command: "prompt".to_string(),
                body,
            })
            .await;
        state.lock().await.in_flight = None;
    });

    InFlight { prompt_id, cancel, handle }
}

// ─── Per-command handlers ─────────────────────────────────────────────────────

/// Handle a `Prompt` or `FollowUp` command (same engine path, no attachments on FollowUp).
async fn handle_prompt(
    id: String,
    message: String,
    attachments: Vec<RpcAttachment>,
    state: Arc<Mutex<RpcState>>,
    writer_tx: mpsc::Sender<RpcEvent>,
) {
    // Reject concurrent prompt.
    {
        let st = state.lock().await;
        if st.is_streaming() {
            tracing::warn!(id, "rejected concurrent prompt — stream already in flight");
            let _ = writer_tx
                .send(RpcEvent::Error {
                    id: Some(id),
                    message: "another prompt is in flight; abort first".to_string(),
                })
                .await;
            return;
        }
    }

    // Push user message.
    let content = build_user_content(&message, &attachments);
    {
        let mut st = state.lock().await;
        st.api_messages
            .push(std::sync::Arc::new(serde_json::json!({"role": "user", "content": content})));
    }

    let in_flight = spawn_prompt(id, state.clone(), writer_tx).await;
    state.lock().await.in_flight = Some(in_flight);
}

/// Handle the `Compact` command.
///
/// The lock is held only for brief snapshot and write-back phases; the slow
/// LLM round-trip in `compact_conversation` runs with **no lock held** so
/// that `Abort`, `GetState`, and `GetSessionStats` remain responsive.
async fn handle_compact(
    id: String,
    state: Arc<Mutex<RpcState>>,
    writer_tx: mpsc::Sender<RpcEvent>,
) {
    // 1. Brief lock: snapshot what compact_conversation needs, then drop guard.
    let (msgs, runtime) = {
        let st = state.lock().await;
        (st.api_messages.clone(), st.runtime.clone())
    };

    // 2. Long-running LLM call — no lock held.
    let summary_result =
        synaps_cli::runtime::compaction::compact_conversation(&msgs, &runtime, None).await;

    match summary_result {
        Ok(summary) => {
            {
                let mut st = state.lock().await;
                st.api_messages = vec![std::sync::Arc::new(
                    serde_json::json!({"role": "user", "content": summary.clone()}),
                )];
                st.save_session().await;
            }
            let _ = writer_tx
                .send(RpcEvent::Response {
                    id,
                    command: "compact".to_string(),
                    body: serde_json::json!({ "summary": summary }),
                })
                .await;
        }
        Err(e) => {
            tracing::error!(error = %e, "compact_conversation failed");
            let _ = writer_tx
                .send(RpcEvent::Error {
                    id: Some(id),
                    message: e.to_string(),
                })
                .await;
        }
    }
}

/// Handle the `NewSession` command.
async fn handle_new_session(
    id: String,
    state: Arc<Mutex<RpcState>>,
    writer_tx: mpsc::Sender<RpcEvent>,
) {
    // Reject if a streaming prompt is in flight.
    {
        let st = state.lock().await;
        if st.is_streaming() {
            tracing::warn!(id, "rejected new_session — stream in flight");
            let _ = writer_tx
                .send(RpcEvent::Error {
                    id: Some(id),
                    message: "another prompt is in flight; abort first".to_string(),
                })
                .await;
            return;
        }
    }

    let new_session_id = {
        let mut st = state.lock().await;
        st.save_session().await;
        let new_sess =
            Session::new(st.runtime.model(), st.runtime.thinking_level(), st.runtime.system_prompt());
        let sid = new_sess.id.clone();
        st.session = new_sess;
        st.api_messages.clear();
        st.total_input_tokens = 0;
        st.total_output_tokens = 0;
        st.session_cost = 0.0;
        sid
    };

    let _ = writer_tx
        .send(RpcEvent::Response {
            id,
            command: "new_session".to_string(),
            body: serde_json::json!({ "session_id": new_session_id }),
        })
        .await;
}

/// Handle the `GetMessages` command.
async fn handle_get_messages(
    id: String,
    state: Arc<Mutex<RpcState>>,
    writer_tx: mpsc::Sender<RpcEvent>,
) {
    let messages = state.lock().await.api_messages.clone();
    let _ = writer_tx
        .send(RpcEvent::Response {
            id,
            command: "get_messages".to_string(),
            body: serde_json::json!({ "messages": messages }),
        })
        .await;
}

/// Handle the `SetModel` command.
async fn handle_set_model(
    id: String,
    model: String,
    state: Arc<Mutex<RpcState>>,
    writer_tx: mpsc::Sender<RpcEvent>,
) {
    state.lock().await.runtime.set_model(model.clone());
    let _ = writer_tx
        .send(RpcEvent::Response {
            id,
            command: "set_model".to_string(),
            body: serde_json::json!({ "model": model }),
        })
        .await;
}

/// Handle the `GetAvailableModels` command.
async fn handle_get_available_models(id: String, writer_tx: mpsc::Sender<RpcEvent>) {
    let overrides: BTreeMap<String, String> = BTreeMap::new();
    let providers = list_providers(&overrides);

    let mut models_list: Vec<serde_json::Value> = Vec::new();
    for (provider_key, _provider_name, _has_key, _count) in &providers {
        if let Some(models) = list_models(provider_key) {
            for (model_id, model_name, _default_flag) in models {
                models_list.push(serde_json::json!({
                    "provider": provider_key,
                    "model_id": model_id,
                    "model_name": model_name,
                }));
            }
        }
    }

    let _ = writer_tx
        .send(RpcEvent::Response {
            id,
            command: "get_available_models".to_string(),
            body: serde_json::json!({ "models": models_list }),
        })
        .await;
}

/// Handle the `Abort` command.
///
/// Cancels the in-flight stream (if any) via its `CancellationToken`, awaits
/// the task so it can clean up `in_flight`, then always replies `{ ok: true }`.
async fn handle_abort(
    id: String,
    state: Arc<Mutex<RpcState>>,
    writer_tx: mpsc::Sender<RpcEvent>,
) {
    let handle_opt = {
        let mut st = state.lock().await;
        if let Some(inf) = st.in_flight.take() {
            tracing::info!(prompt_id = %inf.prompt_id, abort_id = %id, "aborted in-flight stream");
            inf.cancel.cancel();
            Some(inf.handle)
        } else {
            None
        }
    };

    if let Some(handle) = handle_opt {
        if let Err(e) = handle.await {
            tracing::warn!(error = ?e, "streaming task panicked during abort");
        }
    }

    let _ = writer_tx
        .send(RpcEvent::Response {
            id,
            command: "abort".to_string(),
            body: serde_json::json!({ "ok": true }),
        })
        .await;
}

/// Handle the `GetSessionStats` command.
async fn handle_get_session_stats(
    id: String,
    state: Arc<Mutex<RpcState>>,
    writer_tx: mpsc::Sender<RpcEvent>,
) {
    let body = {
        let st = state.lock().await;
        serde_json::json!({
            "input_tokens":  st.total_input_tokens,
            "output_tokens": st.total_output_tokens,
            "cost":          st.session_cost,
            "message_count": st.api_messages.len(),
            "model":         st.runtime.model(),
            "session_id":    st.session.id,
        })
    };
    let _ = writer_tx
        .send(RpcEvent::Response {
            id,
            command: "get_session_stats".to_string(),
            body,
        })
        .await;
}

/// Handle the `GetState` command.
async fn handle_get_state(
    id: String,
    state: Arc<Mutex<RpcState>>,
    writer_tx: mpsc::Sender<RpcEvent>,
) {
    let body = {
        let st = state.lock().await;
        serde_json::json!({
            "streaming":     st.is_streaming(),
            "model":         st.runtime.model(),
            "session_id":    st.session.id,
            "message_count": st.api_messages.len(),
        })
    };
    let _ = writer_tx
        .send(RpcEvent::Response {
            id,
            command: "get_state".to_string(),
            body,
        })
        .await;
}

/// Handle the `ToolsList` command.
///
/// Reads the current tool schema from the runtime's shared `ToolRegistry`
/// (built-ins + any MCP / extension tools loaded at boot time) and returns
/// `{ ok: true, tools: [{name, description, input_schema}, ...] }`.
///
/// The response uses the existing [`RpcEvent::Response`] frame with
/// `command: "tools_list"`, which the bridge Phase 8
/// `SynapsRpcSessionRouter.listTools()` validates as:
///   `response.ok === true && Array.isArray(response.tools)`.
async fn handle_tools_list(
    id: Option<String>,
    state: Arc<Mutex<RpcState>>,
    writer_tx: mpsc::Sender<RpcEvent>,
) {
    let schema = {
        let st = state.lock().await;
        let registry = st.runtime.tools_shared();
        let guard = registry.read().await;
        guard.tools_schema().as_ref().clone()
    };

    let body = build_tools_list_body(&schema);
    let response_id = id.unwrap_or_default();
    let _ = writer_tx
        .send(RpcEvent::Response {
            id: response_id,
            command: "tools_list".to_string(),
            body,
        })
        .await;
}

// ─── Entry point ──────────────────────────────────────────────────────────────

/// Run the `synaps rpc` headless server.
///
/// Reads [`RpcCommand`] frames from stdin (line-delimited JSON) and writes
/// [`RpcEvent`] frames to stdout. The very first frame emitted is always a
/// [`RpcEvent::Ready`] event advertising the session id, active model, and
/// protocol version.
///
/// All flags are optional. With no `--continue` a fresh [`Session`] is created.
pub async fn run(
    continue_id: Option<String>,
    system: Option<String>,
    model: Option<String>,
    profile: Option<String>,
) -> anyhow::Result<()> {
    // 1. Boot the engine via the shared setup path. This handles profile,
    //    logging, Runtime + config, system prompt, skills/MCP registration,
    //    session resolution, inbox watcher, and per-session socket. Mirrors
    //    `cmd/chat.rs::run` so RPC mode picks up the same provider extensions
    //    (Groq, OpenAI-compat, etc.) as the TUI and chat modes.
    let boot = setup::boot(EngineOpts {
        continue_session: continue_id.map(Some),
        system,
        profile,
        no_extensions: false,
    })
    .await
    .context("engine boot failed")?;

    let mut runtime = boot.runtime;
    let session = boot.session;
    let initial_messages = boot.api_messages;
    let initial_in = boot.total_input_tokens;
    let initial_out = boot.total_output_tokens;
    let initial_cost = boot.session_cost;
    let ext_manager = boot.ext_manager;
    let background = boot.background;

    // 2. CLI --model overrides whatever was persisted in the resumed session.
    if let Some(ref m) = model {
        runtime.set_model(m.clone());
    }

    // 3. Discover and load planted process extensions (provider plugins live
    //    here). We block Ready on the loader's `Finished` event so the bridge
    //    cannot send a Prompt before extension-backed providers have
    //    registered. Bounded by a 2 s grace period — extension loading is
    //    best-effort, not a hard fail.
    let (loader_tx, mut loader_rx) = mpsc::unbounded_channel();
    synaps_cli::extensions::loader::spawn_discover_and_load(
        Arc::clone(&ext_manager),
        loader_tx,
    );
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        async {
            while let Some(ev) = loader_rx.recv().await {
                if matches!(ev, synaps_cli::extensions::loader::ExtensionLoaderEvent::Finished { .. }) {
                    break;
                }
            }
        },
    )
    .await;
    // Any straggler events after this point are simply dropped when the
    // receiver is dropped; the loader task will exit when its sender drops.

    // Capture session_id + model for the Ready frame before state is consumed.
    let ready_session_id = session.id.clone();
    let ready_model = runtime.model().to_string();

    // 4. Build shared state.
    let state = Arc::new(Mutex::new(RpcState {
        runtime,
        session,
        api_messages: initial_messages,
        total_input_tokens: initial_in,
        total_output_tokens: initial_out,
        session_cost: initial_cost,
        in_flight: None,
        pending_events: Vec::new(),
    }));

    // 5. Spawn the writer task that owns stdout.
    let (writer_tx, writer_rx) = mpsc::channel::<RpcEvent>(WRITER_CHAN_CAP);
    let writer_handle = spawn_writer(writer_rx);

    // 6. Spawn the exactly-one event-drainer task.
    //
    // Policy (per RECON §RPC):
    //   * Never auto-turn. Forward Event frames always. Client decides.
    //   * Idle: drain → inject into api_messages + forward Event frames.
    //   * Busy: drain → buffer in pending_events (flushed at Done).
    //   * Backpressure: writer channel cap 256 already limits burst.
    //   * No async mutex held across awaits — we lock, snapshot/mutate,
    //     release, then send frames.
    {
        let state_d = Arc::clone(&state);
        let writer_d = writer_tx.clone();
        tokio::spawn(async move {
            // Snapshot the event queue handle ONCE (Arc clone, cheap).
            let eq = {
                let st = state_d.lock().await;
                st.runtime.event_queue().clone()
            };
            loop {
                // Wait for at least one event (notify_one pattern).
                eq.notified().await;

                // Drain without holding the mutex across the await above.
                // Lock briefly: snapshot busy flag + drain + mutate messages/pending.
                let frames: Vec<RpcEvent> = {
                    let mut st = state_d.lock().await;
                    let busy = st.is_streaming();
                    // Split borrows explicitly to satisfy the borrow checker.
                    let RpcState { ref mut api_messages, ref mut pending_events, .. } = *st;
                    let drained = drain_event_queue(
                        &eq,
                        api_messages,
                        pending_events,
                        busy,
                        None, // RPC has no steer channel
                    );
                    drained
                        .iter()
                        .map(|d| RpcEvent::Event { payload: Box::new(event_payload_from_drained(d)) })
                        .collect()
                }; // mutex released here

                // Forward ALL Event frames through the writer channel.
                for frame in frames {
                    if writer_d.send(frame).await.is_err() {
                        tracing::warn!("rpc: event drainer: writer channel closed — exiting drainer");
                        return;
                    }
                }
            }
        });
    }

    // 9. Emit Ready — guaranteed to be the first byte on stdout.
    writer_tx
        .send(RpcEvent::Ready {
            session_id: ready_session_id,
            model: ready_model,
            protocol_version: RPC_PROTOCOL_VERSION,
        })
        .await
        .context("writer channel closed before Ready frame could be sent")?;

    tracing::info!("synaps rpc ready");

    // 10. Reader loop: one line = one RpcCommand frame.
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();

    loop {
        match lines.next_line().await {
            Err(e) => {
                tracing::error!(error = %e, "stdin read error; exiting");
                break;
            }
            Ok(None) => {
                // EOF — parent closed stdin; treat the same as Shutdown.
                tracing::info!("stdin EOF; saving session and exiting");
                state.lock().await.save_session().await;
                background.shutdown();
                break;
            }
            Ok(Some(line)) => {
                let line = line.trim_end_matches('\r'); // tolerate CRLF
                if line.trim().is_empty() {
                    continue;
                }

                let cmd = match parse_frame(line, MAX_FRAME_BYTES) {
                    Ok(c) => c,
                    Err(err_ev) => {
                        tracing::warn!("frame parse error");
                        let _ = writer_tx.send(err_ev).await;
                        continue;
                    }
                };

                tracing::debug!(?cmd, "received RpcCommand");

                match cmd {
                    RpcCommand::Prompt { id, message, attachments } => {
                        handle_prompt(id, message, attachments, state.clone(), writer_tx.clone())
                            .await;
                    }
                    RpcCommand::FollowUp { id, message } => {
                        // Same engine path as Prompt — no attachments.
                        handle_prompt(id, message, Vec::new(), state.clone(), writer_tx.clone())
                            .await;
                    }
                    RpcCommand::Compact { id } => {
                        handle_compact(id, state.clone(), writer_tx.clone()).await;
                    }
                    RpcCommand::NewSession { id } => {
                        handle_new_session(id, state.clone(), writer_tx.clone()).await;
                    }
                    RpcCommand::GetMessages { id } => {
                        handle_get_messages(id, state.clone(), writer_tx.clone()).await;
                    }
                    RpcCommand::SetModel { id, model: m } => {
                        handle_set_model(id, m, state.clone(), writer_tx.clone()).await;
                    }
                    RpcCommand::GetAvailableModels { id } => {
                        handle_get_available_models(id, writer_tx.clone()).await;
                    }
                    RpcCommand::Abort { id } => {
                        handle_abort(id, state.clone(), writer_tx.clone()).await;
                    }
                    RpcCommand::GetSessionStats { id } => {
                        handle_get_session_stats(id, state.clone(), writer_tx.clone()).await;
                    }
                    RpcCommand::GetState { id } => {
                        handle_get_state(id, state.clone(), writer_tx.clone()).await;
                    }
                    RpcCommand::ToolsList { id } => {
                        handle_tools_list(id, state.clone(), writer_tx.clone()).await;
                    }
                    RpcCommand::Shutdown => {
                        tracing::info!("Shutdown received; draining and exiting");

                        // Spec: let an in-flight stream finish naturally (no cancel).
                        // Poll in_flight until the streaming task clears it.
                        loop {
                            let done = state.lock().await.in_flight.is_none();
                            if done {
                                break;
                            }
                            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                        }

                        state.lock().await.save_session().await;

                        // Signal background tasks (inbox watcher, session
                        // socket listener) to stop. The `BackgroundTasks`
                        // value is dropped at function exit, which aborts
                        // the join handles as a hard backstop.
                        background.shutdown();

                        // Drop the sender so the writer task drains its queue and exits.
                        drop(writer_tx);

                        // Bounded wait: 1 s is plenty for 256 buffered frames over a stdio pipe.
                        // Replaces the former unconditional sleep(20 ms) which could drop frames.
                        match tokio::time::timeout(
                            tokio::time::Duration::from_secs(1),
                            writer_handle,
                        )
                        .await
                        {
                            Ok(Ok(())) => {} // writer finished cleanly
                            Ok(Err(e)) => {
                                tracing::warn!(error = ?e, "writer task panicked during shutdown")
                            }
                            Err(_) => tracing::warn!(
                                "writer task did not drain within 1s; exiting anyway"
                            ),
                        }

                        std::process::exit(0);
                    }
                }
            }
        }
    }

    Ok(())
}
