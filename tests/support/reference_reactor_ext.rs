//! FROZEN reference reactor EXTENSION — the oracle half that
//! `support/reference_reactor.rs` deliberately left out (its header forbids
//! editing it, so this file wraps it instead of patching it).
//!
//! Re-derived (NOT verbatim — same caveat as the base file: same author,
//! same reading; a shared misreading passes) from these inline TUI sites
//! @ f0ee1e62:
//!   - `dispatch.rs:134-192`       Abort (engine half: cancel token,
//!                                 abort_context capture, dequeue, flush
//!                                 pending events, save, streaming=false)
//!   - `app.rs:644-683`            capture_abort_context (format strings)
//!   - `app.rs:460-474`            save_session (the three call sites:
//!                                 stream_handler.rs:77 MessageHistory,
//!                                 dispatch.rs:191 Abort,
//!                                 stream_handler.rs:203 AutoTriggerEvents)
//!   - `app.rs:476-…`              add_usage (stream_handler.rs:142-161)
//!   - `run_setup.rs:188-199`      secret-prompt channel: the 4th argument
//!                                 of `run_stream_with_messages` is
//!                                 `Some(handle)` here (the base file passes
//!                                 `None`; the actor passes `Some`).
//!
//! `start_stream`, `submit` and `drive_to_idle` are copied from the base
//! file and extended at the cited sites; `steer` and `on_queue_wake`
//! delegate to it (the RunTurn branch of `on_queue_wake` therefore still
//! starts a stream WITHOUT the prompt handle — no scenario below prompts
//! from an event-triggered turn).
//!
//! What this extension STILL cannot assert — read before trusting a green
//! run:
//!   1. `abort_context` for a turn whose tool result was only partially
//!      streamed (`ToolResultDelta`): the TUI folds from its transcript
//!      (`app.rs:651`), this oracle folds from a `TurnLog`-shaped list
//!      built from the same events the actor's `TurnLog` sees. Identical
//!      construction on both sides ⇒ a shared misreading is invisible.
//!   2. Steer/queue with a DEAD steering channel (`delivered == false` →
//!      Done → auto-send of `queued_message`): the runtime holds the
//!      receiver for the whole turn, so from the outside `delivered` is
//!      always `true` while streaming; the `false` case is a scheduling
//!      race (stream task finished, terminal event not yet consumed) that
//!      cannot be scripted deterministically on either side. The base
//!      file's `steer()` sets `queued_message`; only the Cancel→Dequeued
//!      path is exercised here.
//!   3. Event injection with disposition `Buffered` — same reason as (2):
//!      while the stream is live, `drain_event_queue` steers. Only the
//!      `Steered` disposition is differential-tested.
//!   4. Compaction (inline in the actor at this commit; `Runtime::clone`
//!      latch semantics) — phase-4 scenario.
//!   5. Save COUNT on the actor side is observed from outside (inode
//!      sampling of `sessions/<id>.json`, see the test file); the oracle's
//!      logical counter is compared against the same sampler on its own
//!      file in every scenario so a sampler miss would show up as an
//!      oracle self-mismatch first.
//!   6. Anything presentational (Layer 2/3 of PLAN-phase3 §5.1).
//!
//! NEVER edit the code below this header — `session_actor_differential.rs`
//! pins this file's sha256.

#![allow(dead_code)]

use futures::StreamExt;
use synaps_cli::engine::reactor::claim_auto_turn;
use synaps_cli::tools::{SecretPromptHandle, SecretPromptRequest};
use synaps_cli::{AgentEvent, CancellationToken, LlmEvent, Session, SessionEvent, StreamEvent};

use super::reference_reactor::ReferenceReactor;

/// The TUI transcript's abort-fold source, reduced to what
/// `capture_abort_context` reads (app.rs:644-683).
pub enum Part {
    Thinking(String),
    Text(String),
    ToolUse { name: String, input: String },
    ToolResult { tool_id: String, content: String },
}

pub struct ReferenceReactorExt {
    pub r: ReferenceReactor,
    pub session: Session,
    pub prompt_handle: SecretPromptHandle,
    pub prompt_rx: tokio::sync::mpsc::UnboundedReceiver<SecretPromptRequest>,
    /// Logical save count — incremented at the three TUI call sites.
    pub saves: usize,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_cache_creation_tokens: u64,
    pub session_cost: f64,
    pub parts: Vec<Part>,
}

impl ReferenceReactorExt {
    pub fn new(r: ReferenceReactor, session: Session) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            r,
            session,
            prompt_handle: SecretPromptHandle::new(tx),
            prompt_rx: rx,
            saves: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            session_cost: 0.0,
            parts: Vec::new(),
        }
    }

    /// app.rs:460-474
    pub async fn save_session(&mut self) {
        if self.r.api_messages.is_empty() {
            return;
        }
        self.saves += 1;
        self.session.api_messages = self.r.api_messages.clone();
        self.session.total_input_tokens = self.total_input_tokens;
        self.session.total_output_tokens = self.total_output_tokens;
        self.session.session_cost = self.session_cost;
        self.session.abort_context = self.r.abort_context.clone();
        self.session.updated_at = chrono::Utc::now();
        self.session.auto_title();
        if let Err(e) = self.session.save().await {
            eprintln!("[ERROR] Failed to save session: {}", e);
        }
    }

    /// app.rs add_usage (stream_handler.rs:142-161 caller).
    #[allow(clippy::too_many_arguments)]
    fn add_usage(
        &mut self,
        input_tokens: u64,
        output_tokens: u64,
        cache_read: u64,
        cache_creation: u64,
        cache_creation_5m: Option<u64>,
        cache_creation_1h: Option<u64>,
        model: &str,
    ) {
        self.total_input_tokens += input_tokens;
        self.total_output_tokens += output_tokens;
        self.total_cache_read_tokens += cache_read;
        self.total_cache_creation_tokens += cache_creation;
        self.session_cost += synaps_cli::pricing::calculate_cost_optional_split(
            model,
            input_tokens,
            output_tokens,
            cache_read,
            cache_creation,
            cache_creation_5m,
            cache_creation_1h,
        );
    }

    /// Base `start_stream` with `Some(prompt_handle)` as the 4th argument
    /// (run_setup.rs:188-199 + dispatch.rs:1262-1270).
    async fn start_stream(&mut self) {
        let ct = CancellationToken::new();
        let (s_tx, s_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        self.r.streaming = true;
        self.r.turn_baseline = self.r.api_messages.len();
        self.parts.clear();
        self.r.stream = Some(
            self.r
                .runtime
                .run_stream_with_messages(
                    self.r.api_messages.clone(),
                    ct.clone(),
                    Some(s_rx),
                    Some(self.prompt_handle.clone()),
                    false,
                )
                .await,
        );
        self.r.cancel = Some(ct);
        self.r.steer_tx = Some(s_tx);
    }

    /// dispatch.rs:1231-1288
    pub async fn submit(&mut self, input: String) {
        self.r.consecutive_auto_turns = 0;
        let api_content = if let Some(ref ctx) = self.r.abort_context {
            let combined = format!("{}\n\n{}", ctx, input);
            self.r.abort_context = None;
            combined
        } else {
            input
        };
        self.r.api_messages.push(std::sync::Arc::new(
            serde_json::json!({"role": "user", "content": api_content}),
        ));
        self.start_stream().await;
    }

    /// dispatch.rs:1369-1378 (delegated).
    pub fn steer(&mut self, input: String) -> bool {
        self.r.steer(input)
    }

    /// stream_handler.rs:256-391 (delegated; see header for the RunTurn
    /// caveat).
    pub async fn on_queue_wake(&mut self) {
        self.r.on_queue_wake().await
    }

    /// app.rs:644-683 `capture_abort_context`.
    fn capture_abort_context(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        for p in &self.parts {
            match p {
                Part::Thinking(t) if !t.is_empty() => {
                    let preview: String = t.chars().take(500).collect();
                    parts.push(format!("[thinking]: {}", preview));
                }
                Part::Text(t) if !t.is_empty() => {
                    parts.push(format!("[response]: {}", t));
                }
                Part::ToolUse { name, input } => {
                    let input_preview: String = input.chars().take(200).collect();
                    parts.push(format!("[tool_use]: {} — {}", name, input_preview));
                }
                Part::ToolResult { content, .. } if !content.is_empty() => {
                    let preview: String = content.chars().take(300).collect();
                    parts.push(format!("[tool_result]: {}", preview));
                }
                _ => {}
            }
        }
        if parts.is_empty() {
            return None;
        }
        Some(format!(
            "[ABORT CONTEXT — your previous response was interrupted. Here's what you completed before the abort:]\n\n{}\n\n[END ABORT CONTEXT — continue from where you left off or adjust based on the user's new message]",
            parts.join("\n")
        ))
    }

    /// dispatch.rs:134-192 Abort, engine half. Returns the dequeued
    /// message, if any (dispatch.rs:147-150 presentation input).
    pub async fn abort(&mut self) -> Option<String> {
        if !self.r.streaming {
            return None;
        }
        if let Some(ref ct) = self.r.cancel {
            ct.cancel();
        }
        self.r.abort_context = self.capture_abort_context();
        let dequeued = self.r.queued_message.take();
        for formatted in self.r.pending_events.drain(..) {
            self.r
                .api_messages
                .push(std::sync::Arc::new(serde_json::json!({
                    "role": "user",
                    "content": formatted
                })));
        }
        self.r.stream = None;
        self.r.cancel = None;
        self.r.steer_tx = None;
        self.r.streaming = false;
        // subagent cancellation (dispatch.rs:164-178) has no observable
        // effect without subagents; omitted.
        self.save_session().await; // dispatch.rs:191
        dequeued
    }

    /// Base `drive_to_idle` + `parts` (transcript fold source), usage,
    /// and the two in-stream save sites. A secret prompt raised by a tool
    /// mid-turn is answered with `prompt_answer` (the input pane's submit,
    /// engine half: `response_tx.send`).
    pub async fn drive_to_idle(&mut self, prompt_answer: Option<String>) {
        self.drive_until(prompt_answer, |_| false).await;
    }

    /// `drive_to_idle` that returns early once `stop(&event)` matches
    /// (after applying that event) — the oracle-side "pump until the first
    /// Text delta, then steer/abort" for the mid-stream scenarios.
    pub async fn drive_until(
        &mut self,
        prompt_answer: Option<String>,
        stop: impl Fn(&StreamEvent) -> bool,
    ) {
        while self.r.stream.is_some() {
            let event = loop {
                let stream = self.r.stream.as_mut().unwrap();
                tokio::select! {
                    ev = stream.next() => break ev,
                    Some(req) = self.prompt_rx.recv() => {
                        let _ = req.response_tx.send(prompt_answer.clone());
                    }
                }
            };
            let Some(event) = event else {
                self.r.stream = None;
                self.r.streaming = false;
                break;
            };
            self.r.seen.push(format!("{:?}", event));
            let stop_here = stop(&event);
            enum Action {
                Continue,
                AutoSendQueued(String),
                AutoTriggerEvents,
            }
            let mut action = Action::Continue;
            match event {
                StreamEvent::Llm(LlmEvent::Thinking(t)) => {
                    if let Some(Part::Thinking(s)) = self.parts.last_mut() {
                        s.push_str(&t);
                    } else {
                        self.parts.push(Part::Thinking(t));
                    }
                }
                StreamEvent::Llm(LlmEvent::Text(t)) => {
                    if let Some(Part::Text(s)) = self.parts.last_mut() {
                        s.push_str(&t);
                    } else {
                        self.parts.push(Part::Text(t));
                    }
                }
                StreamEvent::Llm(LlmEvent::ToolUse {
                    tool_name, input, ..
                }) => self.parts.push(Part::ToolUse {
                    name: tool_name,
                    input: serde_json::to_string(&input).unwrap_or_default(),
                }),
                StreamEvent::Llm(LlmEvent::ToolResultDelta { tool_id, delta }) => {
                    match self.parts.iter_mut().rev().find_map(|p| match p {
                        Part::ToolResult { tool_id: id, content } if *id == tool_id => {
                            Some(content)
                        }
                        _ => None,
                    }) {
                        Some(c) => c.push_str(&delta),
                        None => self.parts.push(Part::ToolResult {
                            tool_id,
                            content: delta,
                        }),
                    }
                }
                StreamEvent::Llm(LlmEvent::ToolResult { tool_id, result }) => {
                    match self.parts.iter_mut().rev().find_map(|p| match p {
                        Part::ToolResult { tool_id: id, content } if *id == tool_id => {
                            Some(content)
                        }
                        _ => None,
                    }) {
                        Some(c) => *c = result,
                        None => self.parts.push(Part::ToolResult {
                            tool_id,
                            content: result,
                        }),
                    }
                }
                StreamEvent::Llm(_) => {}
                StreamEvent::Session(SessionEvent::MessageHistory(history)) => {
                    self.r.api_messages = history;
                    self.save_session().await; // stream_handler.rs:77
                }
                StreamEvent::Session(SessionEvent::Usage {
                    input_tokens,
                    output_tokens,
                    cache_read_input_tokens,
                    cache_creation_input_tokens,
                    cache_creation_5m,
                    cache_creation_1h,
                    model,
                }) => {
                    let model_for_pricing = model
                        .as_deref()
                        .unwrap_or(self.r.runtime.model())
                        .to_string();
                    self.add_usage(
                        input_tokens,
                        output_tokens,
                        cache_read_input_tokens,
                        cache_creation_input_tokens,
                        cache_creation_5m,
                        cache_creation_1h,
                        &model_for_pricing,
                    );
                }
                StreamEvent::Agent(AgentEvent::SteeringDelivered { message }) => {
                    if self.r.queued_message.as_ref() == Some(&message) {
                        self.r.queued_message = None;
                    }
                }
                StreamEvent::Agent(_) => {}
                StreamEvent::Session(SessionEvent::Notice(_)) => {}
                StreamEvent::Session(SessionEvent::Done) => {
                    self.r.streaming = false;
                    let had_pending = !self.r.pending_events.is_empty();
                    for formatted in self.r.pending_events.drain(..) {
                        self.r
                            .api_messages
                            .push(std::sync::Arc::new(serde_json::json!({
                                "role": "user",
                                "content": formatted
                            })));
                    }
                    if let Some(queued) = self.r.queued_message.take() {
                        action = Action::AutoSendQueued(queued);
                    } else if had_pending {
                        self.save_session().await; // stream_handler.rs:203
                        action = Action::AutoTriggerEvents;
                    }
                }
                StreamEvent::Session(SessionEvent::Error(_)) => {
                    self.r.streaming = false;
                    synaps_cli::engine::stream::repair_history_after_failure(
                        &mut self.r.api_messages,
                        self.r.turn_baseline,
                    );
                }
            }
            match action {
                Action::Continue => {
                    if !self.r.streaming {
                        self.r.stream = None;
                        self.r.cancel = None;
                        self.r.steer_tx = None;
                    }
                }
                Action::AutoSendQueued(queued) => {
                    drop(self.r.stream.take());
                    drop(self.r.cancel.take());
                    drop(self.r.steer_tx.take());
                    self.r.consecutive_auto_turns = 0;
                    let api_content = if let Some(ref ctx) = self.r.abort_context {
                        let combined = format!("{}\n\n{}", ctx, queued);
                        self.r.abort_context = None;
                        combined
                    } else {
                        queued
                    };
                    self.r.api_messages.push(std::sync::Arc::new(
                        serde_json::json!({"role": "user", "content": api_content}),
                    ));
                    self.start_stream().await;
                }
                Action::AutoTriggerEvents => {
                    drop(self.r.stream.take());
                    drop(self.r.cancel.take());
                    drop(self.r.steer_tx.take());
                    if claim_auto_turn(&mut self.r.consecutive_auto_turns) {
                        self.start_stream().await;
                    } else {
                        self.r.cap_notices += 1;
                    }
                }
            }
            if stop_here {
                return;
            }
        }
    }
}
