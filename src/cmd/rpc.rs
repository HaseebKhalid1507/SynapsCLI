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
use futures::StreamExt;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use synaps_cli::core::config::load_config;
use synaps_cli::runtime::openai::registry::{list_models, list_providers};
use synaps_cli::{
    core::rpc_dispatch::{
        accumulate_usage, build_tools_list_body, build_user_content, map_stream_event, parse_frame,
        MAX_FRAME_BYTES,
    },
    core::rpc_protocol::{RpcAttachment, RpcCommand, RpcEvent, TurnUsage, RPC_PROTOCOL_VERSION},
    engine::reactor::{
        claim_auto_turn, drain_event_queue, event_payload_from_drained,
        spawn_prompt_registration_check, wake_action, WakeAction, AUTO_TURN_CAP,
    },
    engine::setup::{self, EngineOpts},
    Runtime, Session, SessionEvent, StreamEvent,
};

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
    /// Number of consecutive auto-triggered turns since the last real user message.
    /// Reset to 0 on every real Prompt / FollowUp. Capped at AUTO_TURN_CAP.
    consecutive_auto_turns: u32,
    /// `true` while an auto-turn has been reserved but its `spawn_prompt` call
    /// has not yet registered `in_flight`.  Counts as busy so concurrent Prompt
    /// commands are rejected during the narrow window between reservation and
    /// actual task start.
    auto_turn_pending: bool,
    /// Mirror of `config.events.auto_turn` — loaded once at boot.
    events_auto_turn: bool,
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

    /// Returns `true` if the session is busy — either a streaming task is
    /// running **or** an auto-turn has been reserved but not yet started.
    /// All Prompt / FollowUp / NewSession commands must reject when busy.
    fn is_busy(&self) -> bool {
        self.in_flight.is_some() || self.auto_turn_pending
    }

    /// Convenience alias kept for the `GetState` body (legacy field name).
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

// ─── Terminal-path helper ─────────────────────────────────────────────────────

/// Atomically close out a terminal path: clear `in_flight`, clear
/// `auto_turn_pending`, and flush any buffered `pending_events` into
/// `api_messages` — all under **one** mutex acquisition.
///
/// # `allow_chain` flag
///
/// Controls whether a post-flush auto-turn may be reserved:
///
/// * **`true`** (Done path): if all conditions are met (`events_auto_turn` enabled,
///   buffered events present, `consecutive_auto_turns < AUTO_TURN_CAP`, last
///   message is `role=user`), atomically claim the cap slot, set
///   `auto_turn_pending = true`, and return `Some(auto_id)`.  The caller
///   **must** forward the id to the scheduler without holding the lock.
///
/// * **`false`** (error / cancel / silent-drop paths): atomically clear
///   `in_flight` and `auto_turn_pending`, flush `pending_events` into
///   `api_messages` (so buffered events are not lost from history), but
///   **never** claim a cap slot or set `auto_turn_pending`.  Returns `None`.
///   The cap counter (`consecutive_auto_turns`) is left unchanged.
///
/// This eliminates the critical bug where error/cancel/drop paths previously
/// allowed `terminal_flush` to increment `consecutive_auto_turns` and set
/// `auto_turn_pending = true` while returning `Some(auto_id)` that was then
/// silently discarded — leaving the session permanently stuck in busy state.
async fn terminal_flush(state: &Mutex<RpcState>, allow_chain: bool) -> Option<String> {
    let mut st = state.lock().await;
    st.in_flight = None;
    st.auto_turn_pending = false;
    let to_inject = std::mem::take(&mut st.pending_events);
    let had_buffered = !to_inject.is_empty();
    for formatted in to_inject {
        st.api_messages.push(std::sync::Arc::new(
            serde_json::json!({"role": "user", "content": formatted}),
        ));
    }

    // Only attempt to reserve a post-flush auto-turn on the Done path.
    if allow_chain
        && had_buffered
        && st.events_auto_turn
        && st.consecutive_auto_turns < AUTO_TURN_CAP
        && st
            .api_messages
            .last()
            .map(|m| m["role"].as_str() == Some("user"))
            .unwrap_or(false)
    {
        // Atomically claim the turn and reserve pending flag.
        if claim_auto_turn(&mut st.consecutive_auto_turns) {
            st.auto_turn_pending = true;
            let auto_id = format!("auto:post-flush-{}", chrono::Utc::now().timestamp_millis());
            return Some(auto_id);
        }
    }
    None
}

// ─── Streaming task ───────────────────────────────────────────────────────────

/// Spawn a streaming task for a `Prompt` or `FollowUp` command.
///
/// **Race fix — oneshot start barrier (issue 1):**
/// The spawned task must NOT touch shared state or call `terminal_flush` until
/// `in_flight` has been set, otherwise a fast error path can call
/// `terminal_flush` (clearing `in_flight = None`) before `in_flight` is even
/// written — leaving a zombie `JoinHandle` in the slot.
///
/// Protocol:
/// 1. Create a `oneshot` channel `(start_tx, start_rx)`.
/// 2. Spawn the task — it immediately awaits `start_rx` before any state work.
/// 3. Re-validate reservation under registration lock (see bug-1 fix below).
/// 4. Set `in_flight = Some(InFlight { handle, … })` under the lock.
/// 5. Send `start_tx` to release the task.
///
/// This guarantees: `terminal_flush` cannot run until `in_flight` is set, so
/// every `in_flight = None` from inside the task sees a previously-set handle
/// and leaves state consistent. Abort can always find the handle.
///
/// **Bug 1 fix — Abort-between-snapshot-and-registration:**
/// There is a narrow window between the snapshot-guard lock release and the
/// registration lock acquisition during which `Abort` can run and clear
/// `auto_turn_pending`. Without the re-check, the task would be registered
/// regardless — leaving a ghost `InFlight` that Abort already acknowledged as
/// gone. The fix: re-validate `auto_turn_pending` (and `in_flight`) inside the
/// registration lock. If the reservation was revoked, `start_tx` is dropped
/// (task sees `Err` on `start_rx.await` and exits cleanly) and we return
/// without registering `in_flight`.
///
/// **Bug 2 fix — guard-fail leaves `auto_turn_pending` set:**
/// If the snapshot-guard check fails we now clear `auto_turn_pending` before
/// returning so the session is never left permanently busy by a phantom
/// reservation.
///
/// `auto_turn_tx`: channel to the scheduler task in `run()`. Terminal paths
/// that want to chain an additional auto-turn send the reserved `auto_id` here
/// instead of calling `spawn_prompt` recursively (which would create a
/// non-`Send` recursive async future).
async fn spawn_prompt(
    prompt_id: String,
    state: Arc<Mutex<RpcState>>,
    writer_tx: mpsc::Sender<RpcEvent>,
    auto_turn_tx: mpsc::UnboundedSender<String>,
) {
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    let cancel_check = cancel.clone();
    let pid = prompt_id.clone();
    let wtx = writer_tx.clone();

    // Snapshot messages under the lock.
    // For auto: IDs we also re-validate the reservation atomically — same lock
    // acquisition for both the guard check and the snapshot so there is no
    // window between "reservation still valid" and "messages snapshotted".
    // Normal client prompts skip the guard (they were already validated by
    // handle_prompt's is_busy() check before reaching here).
    let messages: Vec<synaps_cli::SharedMessage> = {
        let mut st = state.lock().await;
        if prompt_id.starts_with("auto:") && (!st.auto_turn_pending || st.in_flight.is_some()) {
            tracing::warn!(
                prompt_id,
                auto_turn_pending = st.auto_turn_pending,
                in_flight_live = st.in_flight.is_some(),
                "rpc: spawn_prompt: auto-turn reservation invalidated at snapshot — aborting"
            );
            // Bug 2 fix: defensively clear auto_turn_pending before returning so
            // the session is never left in a permanently-busy phantom state when
            // the guard check here fails.
            st.auto_turn_pending = false;
            return;
        }
        st.api_messages.clone()
    };

    // Start barrier: task awaits this before touching any state/stream work.
    let (start_tx, start_rx) = oneshot::channel::<()>();

    let state_task = Arc::clone(&state);
    let handle = tokio::spawn(async move {
        let state = state_task;

        // Wait until in_flight has been registered so terminal_flush is safe.
        // If the sender is dropped (caller panicked), just exit cleanly.
        if start_rx.await.is_err() {
            return;
        }

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
                StreamEvent::Session(
                    se @ SessionEvent::Usage {
                        input_tokens,
                        output_tokens,
                        ..
                    },
                ) => {
                    accumulate_usage(&mut usage_acc, se);
                    let mut st = state.lock().await;
                    st.total_input_tokens += input_tokens;
                    st.total_output_tokens += output_tokens;
                    continue;
                }
                // ── Turn complete ───────────────────────────────────────────
                StreamEvent::Session(SessionEvent::Done) => {
                    let _ = wtx
                        .send(RpcEvent::AgentEnd {
                            usage: usage_acc.clone(),
                        })
                        .await;
                    // terminal_flush(allow_chain=true): Done path — eligible to
                    // reserve a post-flush auto-turn if conditions are met.
                    let post_flush_id = terminal_flush(&state, true).await;
                    let resp_command = if pid.starts_with("auto:") {
                        "auto_turn"
                    } else {
                        "prompt"
                    };
                    let _ = wtx
                        .send(RpcEvent::Response {
                            id: pid.clone(),
                            command: resp_command.to_string(),
                            body: serde_json::json!({ "ok": true }),
                        })
                        .await;
                    // Schedule post-flush auto-turn via the scheduler channel.
                    // We must NOT call spawn_prompt(...).await here — that creates
                    // a recursive async Send cycle. The scheduler task in run()
                    // owns the call site.
                    if let Some(auto_id) = post_flush_id {
                        let _ = auto_turn_tx.send(auto_id);
                    }
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
                            .send(RpcEvent::AgentEnd {
                                usage: usage_acc.clone(),
                            })
                            .await;
                        let resp_command = if pid.starts_with("auto:") {
                            "auto_turn"
                        } else {
                            "prompt"
                        };
                        let _ = wtx
                            .send(RpcEvent::Response {
                                id: pid.clone(),
                                command: resp_command.to_string(),
                                body: serde_json::json!({ "ok": true, "cancelled": true }),
                            })
                            .await;
                        // Cancel path: terminal_flush(allow_chain=false) — never reserve auto-turn.
                        let _ = terminal_flush(&state, false).await;
                        return;
                    }
                    let _ = wtx
                        .send(RpcEvent::Error {
                            id: Some(pid.clone()),
                            message: msg.message.clone(),
                        })
                        .await;
                    let _ = wtx
                        .send(RpcEvent::AgentEnd {
                            usage: usage_acc.clone(),
                        })
                        .await;
                    let resp_command = if pid.starts_with("auto:") {
                        "auto_turn"
                    } else {
                        "prompt"
                    };
                    // Typed spec §5.2 outcome: forward the engine's terminal
                    // category + correlation ID verbatim — never re-derived.
                    let _ = wtx
                        .send(RpcEvent::Response {
                            id: pid.clone(),
                            command: resp_command.to_string(),
                            body: serde_json::json!({
                                "ok": false,
                                "error": msg.message,
                                "outcome": msg.outcome,
                            }),
                        })
                        .await;
                    // Error path: terminal_flush(allow_chain=false) — never reserve auto-turn.
                    let _ = terminal_flush(&state, false).await;
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
        let _ = wtx
            .send(RpcEvent::AgentEnd {
                usage: usage_acc.clone(),
            })
            .await;
        let body = if cancelled {
            serde_json::json!({ "ok": true, "cancelled": true })
        } else {
            serde_json::json!({
                "ok": false,
                "error": "stream ended without Done"
            })
        };
        let resp_command = if pid.starts_with("auto:") {
            "auto_turn"
        } else {
            "prompt"
        };
        let _ = wtx
            .send(RpcEvent::Response {
                id: pid.clone(),
                command: resp_command.to_string(),
                body,
            })
            .await;
        // Silent-drop / abort path: terminal_flush(allow_chain=false) — never reserve auto-turn.
        let _ = terminal_flush(&state, false).await;
    });

    // Register in_flight BEFORE releasing the start barrier.
    // Bug 1 fix — Abort-between-snapshot-and-registration:
    // Between the snapshot guard above and this lock, Abort may have run and
    // cleared auto_turn_pending (+ taken in_flight if any). Re-validate here
    // under the same registration lock before writing in_flight. If the
    // reservation was revoked, drop start_tx (the waiting task sees Err on
    // start_rx.await and exits cleanly) and return without leaving a ghost.
    {
        let mut st = state.lock().await;
        let is_auto = prompt_id.starts_with("auto:");
        let in_flight_live = st.in_flight.is_some();
        if !spawn_prompt_registration_check(is_auto, &mut st.auto_turn_pending, in_flight_live) {
            tracing::warn!(
                prompt_id,
                auto_turn_pending = st.auto_turn_pending,
                in_flight_live,
                "rpc: spawn_prompt: Abort cleared reservation between snapshot and registration — dropping task"
            );
            // Defensively clear pending so the session is not stuck busy.
            st.auto_turn_pending = false;
            // Drop start_tx here — the spawned task's start_rx.await returns
            // Err and the task exits without touching any state.
            drop(start_tx);
            // Drop the JoinHandle — task will finish immediately on start_rx Err.
            drop(handle);
            return;
        }
        st.in_flight = Some(InFlight {
            prompt_id,
            cancel,
            handle,
        });
        st.auto_turn_pending = false;
    }

    // Release the task. From this point the task may proceed with stream work.
    // If send fails the task already exited (should never happen in practice).
    let _ = start_tx.send(());
}

// ─── Per-command handlers ─────────────────────────────────────────────────────

/// Handle a `Prompt` or `FollowUp` command (same engine path, no attachments on FollowUp).
async fn handle_prompt(
    id: String,
    message: String,
    attachments: Vec<RpcAttachment>,
    state: Arc<Mutex<RpcState>>,
    writer_tx: mpsc::Sender<RpcEvent>,
    auto_turn_tx: mpsc::UnboundedSender<String>,
) {
    // Reject concurrent prompt.
    {
        let st = state.lock().await;
        if st.is_busy() {
            tracing::warn!(id, "rejected concurrent prompt — session busy");
            let _ = writer_tx
                .send(RpcEvent::Error {
                    id: Some(id),
                    message: "another prompt is in flight; abort first".to_string(),
                })
                .await;
            return;
        }
    }

    // Push user message and reset the auto-turn counter (real user input).
    let content = build_user_content(&message, &attachments);
    {
        let mut st = state.lock().await;
        st.consecutive_auto_turns = 0;
        st.api_messages.push(std::sync::Arc::new(
            serde_json::json!({"role": "user", "content": content}),
        ));
    }

    // spawn_prompt snapshots messages, sets in_flight atomically (issue 1 fix),
    // and spawns the streaming task.  No separate write-back needed here.
    spawn_prompt(id, state.clone(), writer_tx, auto_turn_tx).await;
}

/// Handle the `Compact` command.
///
/// The lock is held only for brief snapshot and write-back phases; the slow
/// LLM round-trip in `compact_conversation` runs with **no lock held** so
/// that `Abort`, `GetState`, and `GetSessionStats` remain responsive. The
/// transition itself goes through the ONE engine operation (T30, spec §9.2)
/// with the in-place policy.
async fn handle_compact(
    id: String,
    state: Arc<Mutex<RpcState>>,
    writer_tx: mpsc::Sender<RpcEvent>,
) {
    use synaps_cli::runtime::compaction::{
        apply_compaction, CompactionPolicy, CompactionTransition,
    };

    // 1. Brief lock: snapshot what the transition needs, then drop guard.
    let (msgs, runtime, session) = {
        let st = state.lock().await;
        (
            st.api_messages.clone(),
            st.runtime.clone(),
            st.session.clone(),
        )
    };

    // Spec §9.4 / CP-12 M4: the disclosure is CLIENT-VISIBLE before any
    // summarization dispatch — a dedicated Response frame (command
    // "compact.disclosure", same correlation id) precedes the LLM call, and
    // the disclosure also rides the final compact response.
    let disclosure =
        synaps_cli::runtime::compaction::preview_compaction_disclosure(&runtime, &msgs);
    tracing::info!(disclosure = %disclosure.render_line(), "compaction disclosure");
    let _ = writer_tx
        .send(RpcEvent::Response {
            id: id.clone(),
            command: "compact.disclosure".to_string(),
            body: serde_json::to_value(&disclosure).unwrap_or_default(),
        })
        .await;

    // 2. Long-running LLM call + engine transition — no lock held.
    let applied =
        match synaps_cli::runtime::compaction::compact_conversation(&msgs, &runtime, None).await {
            Ok(outcome) => apply_compaction(
                &runtime,
                &session,
                &msgs,
                &outcome,
                CompactionTransition {
                    policy: CompactionPolicy::InPlace,
                    pending_events: Vec::new(),
                    queued_message: None,
                    hook_source: "manual".to_string(),
                },
            )
            .await
            .map(|applied| (applied, outcome.summary_text)),
            Err(e) => Err(e),
        };

    match applied {
        Ok((applied, summary)) => {
            {
                let mut st = state.lock().await;
                st.session = applied.session;
                st.api_messages = applied.api_messages;
                st.save_session().await;
            }
            let _ = writer_tx
                .send(RpcEvent::Response {
                    id,
                    command: "compact".to_string(),
                    body: serde_json::json!({ "summary": summary, "disclosure": disclosure }),
                })
                .await;
        }
        Err(e) => {
            tracing::error!(error = %e, "compaction failed");
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
    // Reject if session is busy (streaming or auto-turn pending).
    {
        let st = state.lock().await;
        if st.is_busy() {
            tracing::warn!(id, "rejected new_session — session busy");
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
        let new_sess = Session::new(
            st.runtime.model(),
            st.runtime.thinking_level(),
            st.runtime.system_prompt(),
        );
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
    let providers = list_providers();

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
async fn handle_abort(id: String, state: Arc<Mutex<RpcState>>, writer_tx: mpsc::Sender<RpcEvent>) {
    let handle_opt = {
        let mut st = state.lock().await;
        // Clear auto_turn_pending so a reserved-but-not-started auto-turn is cancelled.
        st.auto_turn_pending = false;
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
        prompt_manifest: None,
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
    synaps_cli::extensions::loader::spawn_discover_and_load(Arc::clone(&ext_manager), loader_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while let Some(ev) = loader_rx.recv().await {
            if matches!(
                ev,
                synaps_cli::extensions::loader::ExtensionLoaderEvent::Finished { .. }
            ) {
                break;
            }
        }
    })
    .await;
    // Any straggler events after this point are simply dropped when the
    // receiver is dropped; the loader task will exit when its sender drops.

    // Capture session_id + model for the Ready frame before state is consumed.
    let ready_session_id = session.id.clone();
    let ready_model = runtime.model().to_string();

    // Load config for events_auto_turn (default: true per SynapsConfig defaults).
    let events_auto_turn = load_config().events.auto_turn;

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
        consecutive_auto_turns: 0,
        auto_turn_pending: false,
        events_auto_turn,
    }));

    // 5. Spawn the writer task that owns stdout.
    let (writer_tx, writer_rx) = mpsc::channel::<RpcEvent>(WRITER_CHAN_CAP);
    let writer_handle = spawn_writer(writer_rx);

    // 6a. Auto-turn scheduler channel.
    //
    // Terminal paths (Done branch in spawned task) and the drainer send a
    // reserved `auto_id` here instead of calling `spawn_prompt` directly.
    // Calling `spawn_prompt(...).await` inside `tokio::spawn` creates a
    // recursive async future that is not `Send`; routing through an unbounded
    // mpsc channel eliminates the cycle entirely.  The scheduler task below
    // is the single call site for `spawn_prompt` on auto-turn IDs.
    let (auto_turn_tx, mut auto_turn_rx) = mpsc::unbounded_channel::<String>();

    // 6. Spawn the exactly-one event-drainer task.
    //
    // Policy (per C2 spec):
    //   * Always build Event frames from drained events.
    //   * Idle + events_auto_turn + wake_action=RunTurn + claim_auto_turn:
    //     atomically reserve auto_turn_pending=true, then schedule auto-turn
    //     with synthetic id `auto:<first-event-id>` after releasing lock.
    //   * Busy: drain → buffer in pending_events (flushed at Done via terminal_flush).
    //   * One auto-turn per drained batch (coalesced).
    //   * Opt-out: events_auto_turn=false → Event frames only, no turn.
    {
        let state_d = Arc::clone(&state);
        let writer_d = writer_tx.clone();
        let auto_turn_tx_d = auto_turn_tx.clone();
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
                // Lock briefly: snapshot busy flag + drain + mutate messages/pending
                // + atomically reserve auto-turn if conditions met.
                let (frames, auto_turn_id): (Vec<RpcEvent>, Option<String>) = {
                    let mut st = state_d.lock().await;
                    let busy = st.is_busy();
                    let events_auto_turn = st.events_auto_turn;
                    let consecutive = st.consecutive_auto_turns;
                    // Drain: split borrows explicitly to satisfy the borrow checker
                    // for the drain call, then drop the split borrow before
                    // accessing st.consecutive_auto_turns / st.auto_turn_pending.
                    let drained = {
                        let RpcState {
                            ref mut api_messages,
                            ref mut pending_events,
                            ..
                        } = *st;
                        drain_event_queue(
                            &eq,
                            api_messages,
                            pending_events,
                            busy,
                            None, // RPC has no steer channel
                        )
                    };

                    let frames: Vec<RpcEvent> = drained
                        .iter()
                        .map(|d| RpcEvent::Event {
                            payload: Box::new(event_payload_from_drained(d)),
                        })
                        .collect();

                    // Decide auto-turn: only when idle + enabled + wake says RunTurn.
                    let auto_id = if !busy && events_auto_turn {
                        let action =
                            wake_action(&drained, &st.api_messages, false, true, consecutive);
                        if action == WakeAction::RunTurn {
                            // Atomically claim and reserve — one turn per batch.
                            if claim_auto_turn(&mut st.consecutive_auto_turns) {
                                st.auto_turn_pending = true;
                                let first_id = drained
                                    .first()
                                    .map(|d| d.event.id.clone())
                                    .unwrap_or_else(|| "unknown".to_string());
                                Some(format!("auto:{first_id}"))
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    (frames, auto_id)
                }; // mutex released here

                // Forward ALL Event frames through the writer channel.
                for frame in frames {
                    if writer_d.send(frame).await.is_err() {
                        tracing::warn!(
                            "rpc: event drainer: writer channel closed — exiting drainer"
                        );
                        return;
                    }
                }

                // Schedule auto-turn if reserved.  Send auto_id to the scheduler
                // task — do NOT call spawn_prompt directly here as that would
                // require awaiting it inside tokio::spawn which creates a
                // non-Send recursive future cycle.
                if let Some(auto_id) = auto_turn_id {
                    tracing::debug!(auto_id, "rpc: scheduling auto-turn for runtime events");
                    let _ = auto_turn_tx_d.send(auto_id);
                }
            }
        });
    }

    // 7. Spawn the exactly-one auto-turn scheduler task.
    //
    // This is the ONLY place `spawn_prompt` is called for auto-generated turns.
    // Both the drainer and the terminal Done-path send reserved `auto_id` strings
    // here via `auto_turn_tx` (unbounded, so send never blocks).
    // The task simply awaits each id in order — no lock is held across the await.
    {
        let state_s = Arc::clone(&state);
        let writer_s = writer_tx.clone();
        let auto_turn_tx_s = auto_turn_tx.clone();
        tokio::spawn(async move {
            while let Some(auto_id) = auto_turn_rx.recv().await {
                // ── Stale-reservation guard ───────────────────────────────────
                // An Abort or new Prompt can clear `auto_turn_pending` between
                // the drainer/Done-path reserving the id and the scheduler
                // receiving it.  Validate under lock before proceeding so we
                // never overwrite a live `in_flight` with a ghost auto-turn.
                {
                    let mut st = state_s.lock().await;
                    if !st.auto_turn_pending || st.in_flight.is_some() {
                        tracing::warn!(
                            auto_id,
                            auto_turn_pending = st.auto_turn_pending,
                            in_flight_live = st.in_flight.is_some(),
                            "rpc: scheduler: stale auto-turn reservation — dropping"
                        );
                        // Clear the pending flag in case it's still set but
                        // in_flight raced in from a concurrent real prompt.
                        st.auto_turn_pending = false;
                        continue;
                    }
                }
                tracing::debug!(auto_id, "rpc: auto-turn scheduler: calling spawn_prompt");
                spawn_prompt(
                    auto_id,
                    Arc::clone(&state_s),
                    writer_s.clone(),
                    auto_turn_tx_s.clone(),
                )
                .await;
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
                    RpcCommand::Prompt {
                        id,
                        message,
                        attachments,
                    } => {
                        handle_prompt(
                            id,
                            message,
                            attachments,
                            state.clone(),
                            writer_tx.clone(),
                            auto_turn_tx.clone(),
                        )
                        .await;
                    }
                    RpcCommand::FollowUp { id, message } => {
                        // Same engine path as Prompt — no attachments.
                        handle_prompt(
                            id,
                            message,
                            Vec::new(),
                            state.clone(),
                            writer_tx.clone(),
                            auto_turn_tx.clone(),
                        )
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
                        // Bounded by 30 s to prevent a zombie stream from hanging shutdown.
                        let shutdown_poll = async {
                            loop {
                                let done = state.lock().await.in_flight.is_none();
                                if done {
                                    break;
                                }
                                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                            }
                        };
                        match tokio::time::timeout(
                            tokio::time::Duration::from_secs(30),
                            shutdown_poll,
                        )
                        .await
                        {
                            Ok(()) => {}
                            Err(_) => {
                                tracing::warn!(
                                    "Shutdown: in-flight stream did not finish within 30 s; \
                                     proceeding with shutdown anyway"
                                );
                            }
                        }

                        state.lock().await.save_session().await;

                        // Bounded observability flush (Task 11) — before
                        // `process::exit`, which bypasses all destructors.
                        // In-flight streams already drained above, so the
                        // shared writer is safe to close. `off` → `None`
                        // no-op; "flushed" = OS file buffers, no fsync; a
                        // timeout logs metadata-only stats and shutdown
                        // proceeds regardless.
                        if let Some(outcome) = state
                            .lock()
                            .await
                            .runtime
                            .shutdown_observability_async(
                                synaps_cli::runtime::telemetry::DEFAULT_SHUTDOWN_FLUSH_TIMEOUT,
                            )
                            .await
                        {
                            if !outcome.is_flushed() {
                                tracing::warn!(
                                    stats = ?outcome.stats(),
                                    "observability flush timed out — detached worker keeps draining"
                                );
                            }
                        }

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

    // Reached via `break` (stdin EOF or read error) — the only graceful
    // return path that does not `process::exit`. Same bounded observability
    // flush as the Shutdown frame: `off` → `None` no-op; idempotent, so a
    // future refactor routing Shutdown through here double-flushes safely.
    if let Some(outcome) = state
        .lock()
        .await
        .runtime
        .shutdown_observability_async(
            synaps_cli::runtime::telemetry::DEFAULT_SHUTDOWN_FLUSH_TIMEOUT,
        )
        .await
    {
        if !outcome.is_flushed() {
            tracing::warn!(
                stats = ?outcome.stats(),
                "observability flush timed out — detached worker keeps draining"
            );
        }
    }

    Ok(())
}
