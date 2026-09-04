//! `SessionHandle` — the in-process address of a session (PLAN-phase2 §2.6).
//!
//! `EngineHost::create_session` (A1) hands one out per session; every
//! `ClientTransport` is built on it. `echo_for_test` ships a stand-in actor
//! so package B can build the wire before the real `SessionActor` exists.

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};

use super::transport::TransportError;
use super::types::{Envelope, SessionCommand, SessionId, SessionMeta};
use super::view::RuntimeView;

/// Bounded command queue depth (rpc `WRITER_CHAN_CAP` precedent).
pub const CMD_CHAN_CAP: usize = 256;
/// Default broadcast capacity; `SYNAPS_SESSION_EVENTS_CAP` overrides.
pub const DEFAULT_EVENTS_CAP: usize = 1024;

/// Broadcast capacity from `SYNAPS_SESSION_EVENTS_CAP` (min 16) or the default.
pub fn events_cap() -> usize {
    std::env::var("SYNAPS_SESSION_EVENTS_CAP")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .map(|n| n.max(16))
        .unwrap_or(DEFAULT_EVENTS_CAP)
}

#[derive(Clone)]
pub struct SessionHandle {
    pub id: SessionId,
    cmd_tx: mpsc::Sender<SessionCommand>,
    events: broadcast::Sender<Envelope>,
    meta: Arc<SessionMeta>,
    /// Sync, lock-free read of the last published RuntimeView (§2.7).
    view: Arc<arc_swap::ArcSwap<RuntimeView>>,
}

impl std::fmt::Debug for SessionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionHandle")
            .field("id", &self.id)
            .field("model", &self.meta.model)
            .finish()
    }
}

/// Actor-side ends of a session's channels, minted with the handle.
pub struct SessionEndpoints {
    pub cmd_rx: mpsc::Receiver<SessionCommand>,
    pub events: broadcast::Sender<Envelope>,
    pub view: Arc<arc_swap::ArcSwap<RuntimeView>>,
}

impl SessionHandle {
    /// Mint a handle + the actor's ends. Used by `SessionActor` (A1) and the
    /// test `EchoActor`.
    pub fn new(meta: SessionMeta, view: RuntimeView) -> (Self, SessionEndpoints) {
        let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CHAN_CAP);
        let (events, _) = broadcast::channel(events_cap());
        let view = Arc::new(arc_swap::ArcSwap::from_pointee(view));
        let handle = Self {
            id: meta.id.clone(),
            cmd_tx,
            events: events.clone(),
            meta: Arc::new(meta),
            view: Arc::clone(&view),
        };
        (
            handle,
            SessionEndpoints {
                cmd_rx,
                events,
                view,
            },
        )
    }

    /// Send a command. `Backpressure` when the queue is full (never drop
    /// silently); `Closed` once the actor is gone.
    pub async fn send(&self, cmd: SessionCommand) -> Result<(), TransportError> {
        match self.cmd_tx.try_send(cmd) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(TransportError::Backpressure),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(TransportError::Closed),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Envelope> {
        self.events.subscribe()
    }

    pub fn meta(&self) -> &SessionMeta {
        &self.meta
    }

    pub fn view(&self) -> arc_swap::Guard<Arc<RuntimeView>> {
        self.view.load()
    }

    /// Whether the actor is still alive (its command receiver exists).
    pub fn is_alive(&self) -> bool {
        !self.cmd_tx.is_closed()
    }

    /// EchoActor: `Submit{text}` → `TurnStarted`, `Stream(Text(text))`,
    /// `Stream(Done)`, `Conversation` — for B's socket tests before A1 lands.
    #[cfg(any(test, feature = "testing"))]
    pub fn echo_for_test(id: SessionId) -> (Self, tokio::task::JoinHandle<()>) {
        echo::spawn(id)
    }
}

#[cfg(any(test, feature = "testing"))]
pub mod echo {
    //! Minimal stand-in actor. Honours the envelope/seq contract, `Attach`,
    //! `Detach`, `Submit`, `Steer`, `Cancel`, `Query{View}`, `End`;
    //! everything else is acknowledged with a `SystemNotice`.

    use std::collections::HashMap;

    use agent_core::reasoning::ReasoningLevel;

    use super::*;
    use crate::session::types::*;
    use crate::{LlmEvent, SessionEvent, StreamEvent};

    pub fn meta_for(id: &SessionId) -> SessionMeta {
        SessionMeta {
            id: id.clone(),
            name: None,
            model: "echo".into(),
            cwd: None,
            created_at: chrono::Utc::now(),
            continued: false,
            continue_info: None,
            host_pid: std::process::id(),
        }
    }

    pub fn view_for() -> RuntimeView {
        RuntimeView {
            model: "echo".into(),
            thinking_level: "off".into(),
            reasoning_level: ReasoningLevel::Off,
            is_reasoning_explicit: false,
            thinking_budget: 0,
            context_window: 200_000,
            system_prompt: None,
            compaction_model: "echo".into(),
            api_retries: 0,
            subagent_timeout: 0,
            max_tool_output: 4096,
            bash_timeout: 30,
            bash_max_timeout: 300,
            prompt_generation: 0,
            hook_handler_count: 0,
            prompt_inspection: None,
        }
    }

    pub fn spawn(id: SessionId) -> (SessionHandle, tokio::task::JoinHandle<()>) {
        let (handle, ep) = SessionHandle::new(meta_for(&id), view_for());
        let task = tokio::spawn(run(id, ep));
        (handle, task)
    }

    struct Echo {
        id: SessionId,
        events: broadcast::Sender<Envelope>,
        view: Arc<arc_swap::ArcSwap<RuntimeView>>,
        seq: u64,
        next_client: u64,
        clients: HashMap<ClientId, ClientMeta>,
        conv: ConversationSnapshot,
    }

    impl Echo {
        fn emit(&mut self, event: SessionEventWire) {
            let env = Envelope {
                session_id: self.id.clone(),
                seq: self.seq,
                ts: chrono::Utc::now(),
                event,
            };
            self.seq += 1;
            // No receivers is not an error: streams are not tied to clients.
            let _ = self.events.send(env);
        }

        fn snapshot(&self) -> AttachSnapshot {
            AttachSnapshot {
                meta: meta_for(&self.id),
                view: (**self.view.load()).clone(),
                conversation: self.conv.clone(),
                streaming: false,
                replay: Vec::new(),
                pending_prompts: Vec::new(),
                clients: self.clients.iter().map(|(c, m)| (*c, m.kind)).collect(),
            }
        }

        fn handle(&mut self, cmd: SessionCommand) -> bool {
            match cmd {
                SessionCommand::Attach { client, mode } => {
                    let cid = ClientId(self.next_client);
                    self.next_client += 1;
                    let kind = client.kind;
                    self.clients.insert(cid, client);
                    if mode != AttachMode::Mirror {
                        self.emit(SessionEventWire::SystemNotice(format!(
                            "attach mode {mode:?} not supported yet; using mirror"
                        )));
                    }
                    let snapshot = self.snapshot();
                    self.emit(SessionEventWire::Attached {
                        client: cid,
                        snapshot,
                    });
                    self.emit(SessionEventWire::ClientJoined { client: cid, kind });
                }
                SessionCommand::Detach { client } => {
                    if self.clients.remove(&client).is_some() {
                        self.emit(SessionEventWire::ClientLeft { client });
                    }
                }
                SessionCommand::Submit { text, .. } => {
                    let baseline = self.conv.api_messages.len();
                    self.conv.api_messages.push(Arc::new(serde_json::json!({
                        "role": "user", "content": text
                    })));
                    self.emit(SessionEventWire::TurnStarted {
                        turn_baseline: baseline,
                        trigger: TurnTrigger::User,
                    });
                    self.emit(SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::Text(
                        text.clone(),
                    ))));
                    self.conv.api_messages.push(Arc::new(serde_json::json!({
                        "role": "assistant", "content": text
                    })));
                    self.emit(SessionEventWire::Stream(StreamEvent::Session(
                        SessionEvent::MessageHistory(self.conv.api_messages.clone()),
                    )));
                    self.emit(SessionEventWire::Stream(StreamEvent::Session(
                        SessionEvent::Done,
                    )));
                    let snap = self.conv.clone();
                    self.emit(SessionEventWire::Conversation(snap));
                }
                SessionCommand::Steer { text } => {
                    self.conv.queued_message = Some(text.clone());
                    self.emit(SessionEventWire::Steered {
                        text,
                        delivered: false,
                    });
                }
                SessionCommand::Cancel => {
                    let snap = self.conv.clone();
                    self.emit(SessionEventWire::Conversation(snap));
                }
                SessionCommand::Query { id, query } => {
                    let value = match query {
                        SessionQuery::View => {
                            serde_json::to_value(&**self.view.load()).unwrap_or_default()
                        }
                        SessionQuery::Messages => {
                            serde_json::to_value(&self.conv.api_messages).unwrap_or_default()
                        }
                        other => serde_json::json!({ "unsupported": format!("{other:?}") }),
                    };
                    self.emit(SessionEventWire::QueryResult { id, value });
                }
                SessionCommand::Set(setting) => {
                    let mut view = (**self.view.load()).clone();
                    if let SessionSetting::Model { model } = &setting {
                        view.model = model.clone();
                    }
                    self.view.store(Arc::new(view.clone()));
                    self.emit(SessionEventWire::SettingChanged(SettingApplied {
                        setting: "echo".into(),
                        ok: true,
                        message: None,
                        view,
                    }));
                }
                SessionCommand::End { reason } => {
                    self.emit(SessionEventWire::Ended { reason });
                    return false;
                }
                other => {
                    self.emit(SessionEventWire::SystemNotice(format!("echo: {other:?}")));
                }
            }
            true
        }
    }

    async fn run(id: SessionId, mut ep: SessionEndpoints) {
        let mut echo = Echo {
            id,
            events: ep.events,
            view: ep.view,
            seq: 0,
            next_client: 1,
            clients: HashMap::new(),
            conv: ConversationSnapshot::default(),
        };
        while let Some(cmd) = ep.cmd_rx.recv().await {
            if !echo.handle(cmd) {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::types::*;
    use crate::{LlmEvent, SessionEvent, StreamEvent};

    async fn next(rx: &mut broadcast::Receiver<Envelope>) -> Envelope {
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timely")
            .expect("open")
    }

    #[tokio::test]
    async fn echo_actor_submit_emits_turn_in_order() {
        let (handle, task) = SessionHandle::echo_for_test(SessionId::from("echo-1"));
        let mut rx = handle.subscribe();
        handle
            .send(SessionCommand::Submit {
                text: "hello".into(),
                attachments: vec![],
            })
            .await
            .unwrap();

        let e0 = next(&mut rx).await;
        assert!(matches!(
            e0.event,
            SessionEventWire::TurnStarted {
                turn_baseline: 0,
                trigger: TurnTrigger::User
            }
        ));
        let e1 = next(&mut rx).await;
        match e1.event {
            SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::Text(t))) => assert_eq!(t, "hello"),
            other => panic!("unexpected {other:?}"),
        }
        let e2 = next(&mut rx).await;
        assert!(matches!(
            e2.event,
            SessionEventWire::Stream(StreamEvent::Session(SessionEvent::MessageHistory(_)))
        ));
        let e3 = next(&mut rx).await;
        assert!(matches!(
            e3.event,
            SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Done))
        ));
        let e4 = next(&mut rx).await;
        match e4.event {
            SessionEventWire::Conversation(c) => assert_eq!(c.api_messages.len(), 2),
            other => panic!("unexpected {other:?}"),
        }
        // gapless seq, same session id on every envelope
        assert_eq!(
            [e0.seq, e1.seq, e2.seq, e3.seq, e4.seq],
            [0, 1, 2, 3, 4]
        );
        assert!(e4.session_id == SessionId::from("echo-1"));

        handle
            .send(SessionCommand::End {
                reason: EndReason::ClientQuit,
            })
            .await
            .unwrap();
        let e5 = next(&mut rx).await;
        assert!(matches!(
            e5.event,
            SessionEventWire::Ended {
                reason: EndReason::ClientQuit
            }
        ));
        task.await.unwrap();
        assert!(!handle.is_alive());
        assert!(matches!(
            handle.send(SessionCommand::Save).await,
            Err(TransportError::Closed)
        ));
    }

    #[tokio::test]
    async fn handle_view_swaps_on_set() {
        let (handle, task) = SessionHandle::echo_for_test(SessionId::from("echo-2"));
        let mut rx = handle.subscribe();
        assert_eq!(handle.view().model, "echo");
        handle
            .send(SessionCommand::Set(SessionSetting::Model {
                model: "other".into(),
            }))
            .await
            .unwrap();
        let e = next(&mut rx).await;
        assert!(matches!(e.event, SessionEventWire::SettingChanged(_)));
        assert_eq!(handle.view().model, "other");
        drop(handle);
        task.await.unwrap();
    }
}
