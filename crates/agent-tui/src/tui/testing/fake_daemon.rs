//! A scripted daemon for `/sessions` switch scenarios: holds Live sessions
//! (id, name, journal, history, streaming) and saved journals, and answers
//! the modal's switch [`Connector`] with fresh [`ScriptedTransport`]s — the
//! same `Attach::Existing` / `Continue` decisions the socket connector makes,
//! including "that journal is already live → attach to it".

use super::scripted::{ScriptedLog, ScriptedTransport};
use crate::tui::sessions::switch::{Attached, Connector};
use crate::tui::sessions::SwitchTarget;
use agent_engine::session::{AttachMode, AttachSnapshot, ClientId, ClientTransport};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One session the fake daemon holds (or a saved journal it can continue).
#[derive(Clone, Debug)]
pub struct FakeSession {
    pub id: String,
    pub name: Option<String>,
    pub journal_id: String,
    /// `api_messages` (`{"role":…, "content":…}` values).
    pub history: Vec<serde_json::Value>,
    pub streaming: bool,
    /// The folder the daemon created it in (`Continue`'s resolved cwd).
    pub cwd: Option<std::path::PathBuf>,
    /// Mid-turn partial text the attach replays (implies `streaming`).
    pub replay_text: Option<String>,
}

impl FakeSession {
    /// A session with one user/assistant exchange.
    pub fn new(id: &str, name: Option<&str>, user: &str, assistant: &str) -> Self {
        Self {
            id: id.to_string(),
            name: name.map(str::to_string),
            journal_id: id.to_string(),
            history: vec![
                serde_json::json!({"role": "user", "content": user}),
                serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": assistant}]}),
            ],
            streaming: false,
            cwd: None,
            replay_text: None,
        }
    }
}

#[derive(Default)]
struct Inner {
    live: Vec<FakeSession>,
    past: Vec<FakeSession>,
    /// Command logs of every transport handed out, by session id (latest).
    logs: HashMap<String, Arc<Mutex<ScriptedLog>>>,
    /// Every connect, in order (`attach:<id>` / `continue:<journal>`).
    connects: Vec<String>,
}

/// Shared handle (the harness keeps one, the connector another).
#[derive(Clone, Default)]
pub struct FakeDaemon {
    inner: Arc<Mutex<Inner>>,
}

impl FakeDaemon {
    pub fn add_live(&self, s: FakeSession) {
        self.inner.lock().unwrap().live.push(s);
    }

    pub fn add_past(&self, s: FakeSession) {
        self.inner.lock().unwrap().past.push(s);
    }

    pub fn live_ids(&self) -> Vec<String> {
        self.inner.lock().unwrap().live.iter().map(|s| s.id.clone()).collect()
    }

    pub fn connects(&self) -> Vec<String> {
        self.inner.lock().unwrap().connects.clone()
    }

    /// Commands the latest transport for `id` was sent.
    pub fn sent_to(&self, id: &str) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .logs
            .get(id)
            .map(|l| l.lock().unwrap().sent.clone())
            .unwrap_or_default()
    }

    pub(crate) fn log_of(&self, id: &str) -> Option<Arc<Mutex<ScriptedLog>>> {
        self.inner.lock().unwrap().logs.get(id).cloned()
    }

    fn build(inner: &mut Inner, s: &FakeSession, reused_live: bool) -> Attached {
        let runtime = synaps_cli::Runtime::new_headless();
        let mut session = synaps_cli::Session::new(runtime.model(), runtime.thinking_level(), None);
        session.id = s.id.clone();
        session.name = s.name.clone();
        let history: Vec<synaps_cli::SharedMessage> =
            s.history.iter().cloned().map(Arc::new).collect();
        let mut t = ScriptedTransport::with_session(runtime, session.clone()).with_daemon_pid(4242);
        t.set_api_messages(history.clone());
        inner.logs.insert(s.id.clone(), Arc::clone(&t.log));
        let mut meta = t.meta().clone();
        meta.name = s.name.clone();
        meta.journal_id = s.journal_id.clone();
        meta.busy = s.streaming;
        meta.cwd = s.cwd.clone();
        let snapshot = AttachSnapshot {
            meta,
            view: (*t.view()).clone(),
            conversation: agent_engine::session::ConversationSnapshot {
                header: agent_engine::session::SessionHeader::from(&session),
                messages_len: history.len(),
                api_messages: history,
                ..Default::default()
            },
            streaming: s.streaming,
            roster: Default::default(),
            client_input: agent_engine::session::ClientInput::wire_default(),
            replay: s
                .replay_text
                .iter()
                .flat_map(|t| {
                    let env = |event| agent_engine::session::Envelope {
                        session_id: agent_engine::session::SessionId(s.id.clone()),
                        seq: 0,
                        ts: chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap_or_default(),
                        event,
                    };
                    [
                        env(agent_engine::session::SessionEventWire::TurnStarted {
                            turn_baseline: s.history.len(),
                            trigger: agent_engine::session::TurnTrigger::User,
                            user_text: None,
                        }),
                        env(agent_engine::session::SessionEventWire::Stream(
                            agent_engine::StreamEvent::Llm(agent_engine::LlmEvent::Text(t.clone())),
                        )),
                    ]
                })
                .collect(),
            pending_prompts: Vec::new(),
            clients: Vec::new(),
            input_owner: Some(ClientId(1)),
            display_tail: None,
        };
        Attached { transport: Box::new(t), snapshot, reused_live }
    }
}

#[async_trait::async_trait]
impl Connector for FakeDaemon {
    async fn connect(&self, target: &SwitchTarget, _mode: AttachMode) -> Result<Attached, String> {
        let mut inner = self.inner.lock().unwrap();
        match target {
            SwitchTarget::Attach { id } => {
                inner.connects.push(format!("attach:{id}"));
                let s = inner
                    .live
                    .iter()
                    .find(|s| s.id == *id)
                    .cloned()
                    .ok_or_else(|| format!("unknown session: {id}"))?;
                Ok(Self::build(&mut inner, &s, false))
            }
            SwitchTarget::Continue { journal_id, cwd } => {
                let at = cwd.as_ref().map(|c| format!("@{}", c.display())).unwrap_or_default();
                inner.connects.push(format!("continue:{journal_id}{at}"));
                if let Some(s) = inner
                    .live
                    .iter()
                    .find(|s| s.journal_id == *journal_id || s.id == *journal_id)
                    .cloned()
                {
                    return Ok(Self::build(&mut inner, &s, true));
                }
                let past = inner
                    .past
                    .iter()
                    .find(|s| s.journal_id == *journal_id)
                    .cloned()
                    .ok_or_else(|| format!("no saved session {journal_id}"))?;
                // The daemon continues it as a new live session on that journal.
                let s = FakeSession { id: format!("cont-{journal_id}"), cwd: cwd.clone(), ..past };
                inner.live.push(s.clone());
                Ok(Self::build(&mut inner, &s, false))
            }
        }
    }
}
