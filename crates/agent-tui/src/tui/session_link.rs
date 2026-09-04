//! The TUI's side of the session protocol (PLAN-phase3 §2.4, §3.2).
//!
//! [`SessionLink`] wraps a `Box<dyn ClientTransport>` + the published
//! [`RuntimeView`] and adds the two things every slash-command/setter site
//! needs: correlated round-trips (`Set{id}` → `SettingChanged{id}`,
//! `EngineCommand{id}` → `QueryResult{id}`, …) and an envelope buffer so
//! anything that arrives while a reply is awaited is replayed to the loop
//! in order — presentation order is exactly today's (the reply line is
//! pushed by the same handler that sent the command).
//!
//! [`PromptBridge`] turns `Prompt(req)` envelopes into the
//! `SecretPromptRequest`s the (untouched) secret-prompt pane consumes, and
//! routes the pane's answer back as `Answer{prompt_id}`.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use agent_engine::session::{
    AttachMode, ClientId, ClientTransport, Envelope, PromptRequest, RuntimeView,
    SessionCommand, SessionEventWire, SessionQuery, SessionSetting, SettingApplied,
    TransportError,
};

/// Bound on any single command round-trip. The in-process actor answers in
/// microseconds unless it is inside `run_stream_with_messages` setup or an
/// inline compaction (B2 spawns it); the socket adds one hop.
pub(crate) const ROUNDTRIP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Reply to `Resume{id}`.
pub(crate) struct ResumedReply {
    pub old_id: String,
    pub new_id: String,
    pub via: Option<String>,
    pub clamp_notice: Option<String>,
}

pub(crate) struct SessionLink {
    transport: Box<dyn ClientTransport>,
    view: Arc<RuntimeView>,
    buffered: VecDeque<Envelope>,
    next_id: u64,
    /// `true` once `next_event` returned `None` (actor gone / socket EOF).
    closed: bool,
}

impl SessionLink {
    pub(crate) fn new(transport: Box<dyn ClientTransport>) -> Self {
        let view = transport.view();
        Self {
            transport,
            view,
            buffered: VecDeque::new(),
            next_id: 1,
            closed: false,
        }
    }

    /// The view every synchronous getter reads (`impl RuntimeRead`).
    pub(crate) fn view(&self) -> &Arc<RuntimeView> {
        &self.view
    }

    #[allow(dead_code)]
    pub(crate) fn transport(&self) -> &dyn ClientTransport {
        &*self.transport
    }

    pub(crate) fn transport_mut(&mut self) -> &mut Box<dyn ClientTransport> {
        &mut self.transport
    }

    pub(crate) fn client_id(&self) -> ClientId {
        self.transport.client_id()
    }

    pub(crate) fn mode(&self) -> AttachMode {
        self.transport.mode()
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed
    }

    /// Re-read the transport's published view (after `Attached`/reconnect).
    pub(crate) fn refresh_view(&mut self) {
        self.view = self.transport.view();
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Fire-and-forget, stamped with this client's id.
    pub(crate) async fn send(&self, cmd: SessionCommand) -> Result<(), TransportError> {
        self.transport.send_from_self(cmd).await
    }

    /// Next envelope for the loop: buffered replays first. Keeps `view`
    /// fresh from `SettingChanged` (carries the view) and `Resumed`/`Attached`.
    pub(crate) async fn next_event(&mut self) -> Option<Envelope> {
        if let Some(env) = self.buffered.pop_front() {
            return Some(env);
        }
        let env = self.transport.next_event().await;
        match env {
            Some(env) => {
                self.note(&env);
                Some(env)
            }
            None => {
                self.closed = true;
                None
            }
        }
    }

    fn note(&mut self, env: &Envelope) {
        match &env.event {
            SessionEventWire::SettingChanged(applied) => {
                self.view = Arc::new(applied.view.clone());
            }
            SessionEventWire::Resumed { .. } | SessionEventWire::Attached { .. } => {
                self.view = self.transport.view();
            }
            _ => {}
        }
    }

    /// Send `cmd` and pull envelopes until `pick` accepts one; everything
    /// else is buffered for the loop in arrival order.
    async fn roundtrip<T>(
        &mut self,
        cmd: SessionCommand,
        mut pick: impl FnMut(&SessionEventWire) -> Option<T>,
    ) -> Result<T, String> {
        self.send(cmd).await.map_err(|e| e.to_string())?;
        let deadline = tokio::time::Instant::now() + ROUNDTRIP_TIMEOUT;
        loop {
            let env = match tokio::time::timeout_at(deadline, self.transport.next_event()).await {
                Ok(Some(env)) => env,
                Ok(None) => {
                    self.closed = true;
                    return Err("session closed".to_string());
                }
                Err(_) => return Err("session did not reply in time".to_string()),
            };
            self.note(&env);
            if let Some(t) = pick(&env.event) {
                return Ok(t);
            }
            self.buffered.push_back(env);
        }
    }

    /// `Set{id, setting}` → the matching `SettingChanged`.
    pub(crate) async fn set(&mut self, setting: SessionSetting) -> Result<SettingApplied, String> {
        let id = self.alloc_id();
        self.roundtrip(SessionCommand::Set { id, setting }, |ev| match ev {
            SessionEventWire::SettingChanged(a) if a.id == id => Some(a.clone()),
            _ => None,
        })
        .await
    }

    /// `Set` that folds `ok=false` into `Err(message)`.
    pub(crate) async fn set_checked(
        &mut self,
        setting: SessionSetting,
    ) -> Result<SettingApplied, String> {
        let applied = self.set(setting).await?;
        if applied.ok {
            Ok(applied)
        } else {
            Err(applied
                .message
                .clone()
                .unwrap_or_else(|| "setting rejected".to_string()))
        }
    }

    pub(crate) async fn query(&mut self, query: SessionQuery) -> Result<serde_json::Value, String> {
        let id = self.alloc_id();
        self.roundtrip(SessionCommand::Query { id, query }, |ev| match ev {
            SessionEventWire::QueryResult { id: rid, value } if *rid == id => Some(value.clone()),
            _ => None,
        })
        .await
    }

    pub(crate) async fn engine_command(
        &mut self,
        name: &str,
        arg: &str,
    ) -> Result<serde_json::Value, String> {
        let id = self.alloc_id();
        let value = self
            .roundtrip(
                SessionCommand::EngineCommand {
                    id,
                    name: name.to_string(),
                    arg: arg.to_string(),
                },
                |ev| match ev {
                    SessionEventWire::QueryResult { id: rid, value } if *rid == id => {
                        Some(value.clone())
                    }
                    _ => None,
                },
            )
            .await?;
        // model/thinking changes republish the actor's view without a
        // `SettingChanged`; re-read it so the footer/getters are current
        // under either transport.
        if value.get("event").is_some() {
            if let Ok(v) = self.query(SessionQuery::View).await {
                if let Ok(view) = serde_json::from_value::<RuntimeView>(v) {
                    self.view = Arc::new(view);
                }
            }
        }
        Ok(value)
    }

    pub(crate) async fn plugin_command(
        &mut self,
        plugin: &str,
        name: &str,
        arg: &str,
    ) -> Result<serde_json::Value, String> {
        let id = self.alloc_id();
        self.roundtrip(
            SessionCommand::PluginCommand {
                id,
                plugin: plugin.to_string(),
                name: name.to_string(),
                arg: arg.to_string(),
            },
            |ev| match ev {
                SessionEventWire::QueryResult { id: rid, value } if *rid == id => {
                    Some(value.clone())
                }
                _ => None,
            },
        )
        .await
    }

    /// `Resume{id, query}` → `Resumed{id, ..}` (or the actor's error reply as
    /// a `QueryResult{id, {kind:"error", text}}`).
    pub(crate) async fn resume(&mut self, query: &str) -> Result<ResumedReply, String> {
        let id = self.alloc_id();
        self.roundtrip(
            SessionCommand::Resume {
                id,
                query: query.to_string(),
            },
            |ev| match ev {
                SessionEventWire::Resumed {
                    id: rid,
                    old_id,
                    new_id,
                    via,
                    clamp_notice,
                } if *rid == id => Some(Ok(ResumedReply {
                    old_id: old_id.clone(),
                    new_id: new_id.clone(),
                    via: via.clone(),
                    clamp_notice: clamp_notice.clone(),
                })),
                SessionEventWire::QueryResult { id: rid, value } if *rid == id => Some(Err(value
                    ["text"]
                    .as_str()
                    .unwrap_or("resume failed")
                    .to_string())),
                _ => None,
            },
        )
        .await?
    }

    /// Wait for `Ended` (teardown). Buffered/late envelopes are dropped —
    /// nothing renders past this point.
    pub(crate) async fn wait_ended(&mut self) -> bool {
        loop {
            match self.transport.next_event().await {
                Some(env) => {
                    if matches!(env.event, SessionEventWire::Ended { .. }) {
                        return true;
                    }
                }
                None => return false,
            }
        }
    }
}

// ── PromptBridge ─────────────────────────────────────────────────────────

/// `Prompt(req)` → local `SecretPromptRequest` on the SAME channel the
/// secret-prompt pane polls; the pane's oneshot answer comes back through
/// `answers_rx` and the loop sends `Answer{prompt_id, value}`.
pub(crate) struct PromptBridge {
    secret_prompt_tx: tokio::sync::mpsc::UnboundedSender<synaps_cli::tools::SecretPromptRequest>,
    answers_tx: tokio::sync::mpsc::UnboundedSender<(u64, Option<String>)>,
    pub(crate) answers_rx: tokio::sync::mpsc::UnboundedReceiver<(u64, Option<String>)>,
    /// Prompt ids fed to the pane, FIFO — the queue activates in this order.
    order: VecDeque<u64>,
    /// Prompt ids this client answered (its own `PromptResolved` is not a
    /// dismissal).
    answered: HashSet<u64>,
}

impl PromptBridge {
    pub(crate) fn new(
        secret_prompt_tx: tokio::sync::mpsc::UnboundedSender<
            synaps_cli::tools::SecretPromptRequest,
        >,
    ) -> Self {
        let (answers_tx, answers_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            secret_prompt_tx,
            answers_tx,
            answers_rx,
            order: VecDeque::new(),
            answered: HashSet::new(),
        }
    }

    pub(crate) fn on_prompt(&mut self, req: PromptRequest) {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel::<Option<String>>();
        let _ = self
            .secret_prompt_tx
            .send(synaps_cli::tools::SecretPromptRequest {
                title: req.title,
                prompt: req.prompt,
                response_tx,
            });
        self.order.push_back(req.id);
        let answers = self.answers_tx.clone();
        let id = req.id;
        tokio::spawn(async move {
            // Err = dismissed (oneshot dropped without an answer): another
            // client answered; nothing to send.
            if let Ok(value) = response_rx.await {
                let _ = answers.send((id, value));
            }
        });
    }

    /// The pane answered `id` locally: remember it so the echoed
    /// `PromptResolved` is not treated as a foreign resolution.
    pub(crate) fn on_local_answer(&mut self, id: u64) {
        self.answered.insert(id);
        self.order.retain(|x| *x != id);
    }

    /// `PromptResolved{id}`: returns `true` when the ACTIVE pane prompt must
    /// be dismissed (resolved by someone else).
    pub(crate) fn on_resolved(&mut self, id: u64) -> bool {
        if self.answered.remove(&id) {
            return false;
        }
        if self.order.front() == Some(&id) {
            self.order.pop_front();
            return true;
        }
        self.order.retain(|x| *x != id);
        false
    }
}
