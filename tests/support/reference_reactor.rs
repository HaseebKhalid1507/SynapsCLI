//! FROZEN reference reactor — the oracle for `tests/session_actor_differential.rs`.
//!
//! A verbatim copy of today's engine-half reactor logic driving a `Runtime`
//! directly, exactly as the inline TUI/chat hosts do @ e640cafb:
//!   - `crates/agent-tui/src/tui/dispatch.rs:1231-1288`  Submit
//!   - `crates/agent-tui/src/tui/dispatch.rs:134-192`    Abort
//!   - `crates/agent-tui/src/tui/dispatch.rs:1369-1378`  StreamingInput (steer/queue)
//!   - `crates/agent-tui/src/tui/stream_handler.rs:40-248`  handle_stream_event (engine half)
//!   - `crates/agent-tui/src/tui/stream_handler.rs:256-391` handle_event_queue_arm
//!   - `crates/agent-tui/src/tui/stream_handler.rs:393-545` handle_stream_arm
//!
//! It is an oracle, not shared code. NEVER edit after A5 — the differential
//! test asserts this file's sha256 against a constant.

#![allow(dead_code, clippy::collapsible_match)]

use futures::StreamExt;
use synaps_cli::engine::reactor::{
    claim_auto_turn, drain_event_queue, wake_action, EventDisposition, WakeAction, AUTO_TURN_CAP,
};
use synaps_cli::{AgentEvent, CancellationToken, Runtime, SessionEvent, StreamEvent};

pub type ActiveStream = std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send>>;

pub struct ReferenceReactor {
    pub runtime: Runtime,
    pub api_messages: Vec<synaps_cli::SharedMessage>,
    pub pending_events: Vec<String>,
    pub queued_message: Option<String>,
    pub abort_context: Option<String>,
    pub consecutive_auto_turns: u32,
    pub turn_baseline: usize,
    pub streaming: bool,
    pub stream: Option<ActiveStream>,
    pub cancel: Option<CancellationToken>,
    pub steer_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// `format!("{:?}", StreamEvent)` in arrival order.
    pub seen: Vec<String>,
    /// Cap notices in order (stream_handler.rs:368-378 / :528-533).
    pub cap_notices: u32,
}

impl ReferenceReactor {
    pub fn new(runtime: Runtime) -> Self {
        Self {
            runtime,
            api_messages: Vec::new(),
            pending_events: Vec::new(),
            queued_message: None,
            abort_context: None,
            consecutive_auto_turns: 0,
            turn_baseline: 0,
            streaming: false,
            stream: None,
            cancel: None,
            steer_tx: None,
            seen: Vec::new(),
            cap_notices: 0,
        }
    }

    async fn start_stream(&mut self) {
        let ct = CancellationToken::new();
        let (s_tx, s_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        self.streaming = true;
        self.turn_baseline = self.api_messages.len();
        self.stream = Some(
            self.runtime
                .run_stream_with_messages(
                    self.api_messages.clone(),
                    ct.clone(),
                    Some(s_rx),
                    None,
                    false,
                )
                .await,
        );
        self.cancel = Some(ct);
        self.steer_tx = Some(s_tx);
    }

    /// dispatch.rs:1231-1288
    pub async fn submit(&mut self, input: String) {
        self.consecutive_auto_turns = 0;
        let api_content = if let Some(ref ctx) = self.abort_context {
            let combined = format!("{}\n\n{}", ctx, input);
            self.abort_context = None;
            combined
        } else {
            input
        };
        self.api_messages.push(std::sync::Arc::new(
            serde_json::json!({"role": "user", "content": api_content}),
        ));
        self.start_stream().await;
    }

    /// dispatch.rs:1369-1378
    pub fn steer(&mut self, input: String) -> bool {
        let steered = self
            .steer_tx
            .as_ref()
            .map(|tx| tx.send(input.clone()).is_ok())
            .unwrap_or(false);
        self.queued_message = Some(input);
        steered
    }

    /// stream_handler.rs:256-391 (engine half)
    pub async fn on_queue_wake(&mut self) {
        let busy = self.streaming;
        let drained = drain_event_queue(
            self.runtime.event_queue(),
            &mut self.api_messages,
            &mut self.pending_events,
            busy,
            self.steer_tx.as_ref(),
        );
        if drained.is_empty() {
            return;
        }
        let auto_turn_enabled = true;
        let action = wake_action(
            &drained,
            &self.api_messages,
            busy,
            auto_turn_enabled,
            self.consecutive_auto_turns,
        );
        match action {
            WakeAction::RunTurn => {
                if self.stream.is_none() {
                    self.consecutive_auto_turns += 1;
                    self.start_stream().await;
                }
            }
            WakeAction::Forward => {
                let hit_cap = drained
                    .iter()
                    .any(|d| d.disposition == EventDisposition::Injected)
                    && !busy
                    && auto_turn_enabled
                    && self.consecutive_auto_turns >= AUTO_TURN_CAP;
                if hit_cap {
                    self.cap_notices += 1;
                }
            }
            WakeAction::Nothing => {}
        }
    }

    /// Drive the active stream to its terminal event, applying
    /// stream_handler.rs:40-248 + :393-545 (engine halves), including any
    /// auto-continuation turns. Returns when the machine is idle.
    pub async fn drive_to_idle(&mut self) {
        while let Some(stream) = self.stream.as_mut() {
            let Some(event) = stream.next().await else {
                self.stream = None;
                self.streaming = false;
                break;
            };
            self.seen.push(format!("{:?}", event));
            enum Action {
                Continue,
                AutoSendQueued(String),
                AutoTriggerEvents,
            }
            let mut action = Action::Continue;
            match event {
                StreamEvent::Session(SessionEvent::MessageHistory(history)) => {
                    self.api_messages = history;
                }
                StreamEvent::Agent(AgentEvent::SteeringDelivered { message }) => {
                    if self.queued_message.as_ref() == Some(&message) {
                        self.queued_message = None;
                    }
                }
                StreamEvent::Session(SessionEvent::Done) => {
                    self.streaming = false;
                    let had_pending = !self.pending_events.is_empty();
                    for formatted in self.pending_events.drain(..) {
                        self.api_messages
                            .push(std::sync::Arc::new(serde_json::json!({
                                "role": "user",
                                "content": formatted
                            })));
                    }
                    if let Some(queued) = self.queued_message.take() {
                        action = Action::AutoSendQueued(queued);
                    } else if had_pending {
                        action = Action::AutoTriggerEvents;
                    }
                }
                StreamEvent::Session(SessionEvent::Error(_)) => {
                    self.streaming = false;
                    synaps_cli::engine::stream::repair_history_after_failure(
                        &mut self.api_messages,
                        self.turn_baseline,
                    );
                }
                _ => {}
            }
            match action {
                Action::Continue => {
                    if !self.streaming {
                        self.stream = None;
                        self.cancel = None;
                        self.steer_tx = None;
                    }
                }
                Action::AutoSendQueued(queued) => {
                    drop(self.stream.take());
                    drop(self.cancel.take());
                    drop(self.steer_tx.take());
                    self.consecutive_auto_turns = 0;
                    let api_content = if let Some(ref ctx) = self.abort_context {
                        let combined = format!("{}\n\n{}", ctx, queued);
                        self.abort_context = None;
                        combined
                    } else {
                        queued
                    };
                    self.api_messages.push(std::sync::Arc::new(
                        serde_json::json!({"role": "user", "content": api_content}),
                    ));
                    self.start_stream().await;
                }
                Action::AutoTriggerEvents => {
                    drop(self.stream.take());
                    drop(self.cancel.take());
                    drop(self.steer_tx.take());
                    if claim_auto_turn(&mut self.consecutive_auto_turns) {
                        self.start_stream().await;
                    } else {
                        self.cap_notices += 1;
                    }
                }
            }
        }
    }
}
