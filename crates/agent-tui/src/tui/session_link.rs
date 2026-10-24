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
//! routes the pane's answer back as `Answer{prompt_id}`. With
//! [`ConfirmPolicy::ViewOnly`] (`--no-confirm-answers`) it never opens the
//! pane for a confirm prompt and never answers one: it only lists them as
//! waiting.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use agent_engine::session::{
    AttachMode, ClientId, ClientTransport, Envelope, PromptKind, PromptRequest, RuntimeView,
    SessionCommand, SessionEventWire, SessionQuery, SessionSetting, SettingApplied, TransportError,
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
    /// else is buffered for the loop in arrival order. A `Refused` addressed
    /// to this client (B1 ownership: a non-owner's setter) IS the reply —
    /// `Err(reason)` immediately, never the 30 s timeout.
    async fn roundtrip<T>(
        &mut self,
        cmd: SessionCommand,
        mut pick: impl FnMut(&SessionEventWire) -> Option<T>,
    ) -> Result<T, String> {
        let me = self.client_id();
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
            if let SessionEventWire::Refused {
                client,
                command,
                reason,
            } = &env.event
            {
                if *client == me {
                    return Err(format!("{command} refused: {reason}"));
                }
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

    /// Send `Query{id, query}` without waiting: the reply reaches the loop
    /// as a `QueryResult{id}` envelope (the subagent watch poll, which must
    /// never stall input or rendering). Returns the id.
    pub(crate) async fn send_query(&mut self, query: SessionQuery) -> Result<u64, String> {
        let id = self.alloc_id();
        self.send(SessionCommand::Query { id, query })
            .await
            .map_err(|e| e.to_string())?;
        Ok(id)
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

/// Whether this client answers confirm prompts (tool-call and tool-activation
/// approvals). `--no-confirm-answers` sets [`ConfirmPolicy::ViewOnly`]: the
/// TUI then never opens the approval pane and never sends an `Answer` for a
/// confirm, whatever keys reach it, so approvals stay with another client
/// (e.g. an app embedding the TUI in a terminal pane answers them in its own
/// UI). Secret prompts are unchanged: they are still typed in the pane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ConfirmPolicy {
    /// Confirms open the pane and are answered here (the default).
    #[default]
    Answer,
    /// Confirms are only listed as waiting; another client answers them.
    ViewOnly,
}

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
    confirms: ConfirmPolicy,
    /// The kind of every prompt fed to the pane and not yet answered or
    /// resolved: under `ViewOnly` only a `Secret` fed here may be answered.
    kinds: HashMap<u64, PromptKind>,
    /// Confirm prompts shown view-only (`ViewOnly`), in arrival order:
    /// id and title, until a `PromptResolved` clears them.
    waiting: Vec<(u64, String)>,
    /// Whether this client owns input. Only the owner may arm "Allow all"
    /// (the actor refuses anyone else), so a non-owner's pane never offers
    /// the button: the confirm scope is stripped before it reaches the pane.
    owns_input: bool,
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
            confirms: ConfirmPolicy::Answer,
            kinds: HashMap::new(),
            waiting: Vec::new(),
            owns_input: true,
        }
    }

    /// Track input ownership (attach snapshot, `InputOwnerChanged`).
    pub(crate) fn set_owns_input(&mut self, owns: bool) {
        self.owns_input = owns;
    }

    /// Set before the first prompt is fed (attach replays pending prompts).
    pub(crate) fn set_confirm_policy(&mut self, policy: ConfirmPolicy) {
        self.confirms = policy;
    }

    pub(crate) fn on_prompt(&mut self, req: PromptRequest) {
        if self.confirms == ConfirmPolicy::ViewOnly && req.kind == PromptKind::Confirm {
            // Never the pane, never an answer: list it until it resolves.
            if !self.waiting.iter().any(|(id, _)| *id == req.id) {
                let label = waiting_label(&req);
                self.waiting.push((req.id, label));
            }
            return;
        }
        let (response_tx, response_rx) = tokio::sync::oneshot::channel::<Option<String>>();
        self.kinds.insert(req.id, req.kind);
        let _ = self
            .secret_prompt_tx
            .send(synaps_cli::tools::SecretPromptRequest {
                kind: req.kind,
                title: req.title,
                prompt: req.prompt,
                confirm_scope: req.confirm_scope.filter(|_| self.owns_input),
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
    /// `PromptResolved` is not treated as a foreign resolution. Returns
    /// whether the answer may be sent: always under `Answer`; under
    /// `ViewOnly` only for a `Secret` prompt fed to the pane (defence in
    /// depth: a confirm answer is dropped even if one ever reached here).
    pub(crate) fn on_local_answer(&mut self, id: u64) -> bool {
        let kind = self.kinds.remove(&id);
        self.order.retain(|x| *x != id);
        let send = match self.confirms {
            ConfirmPolicy::Answer => true,
            ConfirmPolicy::ViewOnly => kind == Some(PromptKind::Secret),
        };
        if send {
            self.answered.insert(id);
        }
        send
    }

    /// `PromptResolved{id}`: returns `true` when the ACTIVE pane prompt must
    /// be dismissed (resolved by someone else). A view-only confirm is just
    /// removed from the waiting list.
    pub(crate) fn on_resolved(&mut self, id: u64) -> bool {
        self.waiting.retain(|(w, _)| *w != id);
        self.kinds.remove(&id);
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

    /// Confirm prompts waiting for another client (`ViewOnly` only), oldest
    /// first: id and title.
    pub(crate) fn waiting_confirms(&self) -> &[(u64, String)] {
        &self.waiting
    }
}

/// What the `--no-confirm-answers` toast lists for a waiting confirm: its
/// title plus the tool and extension when known, from the scope or the
/// body's `Extension:` / `Tool:` lines (04-tui M7: every entry used to read
/// "Confirm tool call").
fn waiting_label(req: &PromptRequest) -> String {
    let field = |key: &str| {
        req.prompt
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .map(str::trim)
            .filter(|v| !v.is_empty() && *v != "(unknown)")
            .map(str::to_string)
    };
    let scope = req.confirm_scope.as_ref();
    let tool = scope.and_then(|s| s.tool.clone()).or_else(|| field("Tool:"));
    let ext = scope
        .filter(|s| !s.extensions.is_empty())
        .map(|s| s.extensions_label())
        .or_else(|| field("Extension:"));
    match (tool, ext) {
        (Some(t), Some(e)) => format!("{}: {t} ({e})", req.title),
        (Some(t), None) => format!("{}: {t}", req.title),
        _ => req.title.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::testing::scripted::ScriptedTransport;
    use synaps_cli::Runtime;

    fn prompt(id: u64, kind: PromptKind) -> PromptRequest {
        PromptRequest {
            id,
            kind,
            title: match kind {
                PromptKind::Confirm => agent_engine::session::CONFIRM_PROMPT_TITLE.into(),
                PromptKind::Secret => "API key".into(),
            },
            prompt: "details".into(),
            raised_at: chrono::Utc::now(),
            confirm_scope: None,
        }
    }

    fn bridge(
        policy: ConfirmPolicy,
    ) -> (
        PromptBridge,
        tokio::sync::mpsc::UnboundedReceiver<synaps_cli::tools::SecretPromptRequest>,
    ) {
        let (tx, pane) = tokio::sync::mpsc::unbounded_channel();
        let mut b = PromptBridge::new(tx);
        b.set_confirm_policy(policy);
        (b, pane)
    }

    /// With `--no-confirm-answers` a confirm never reaches the pane and
    /// is only listed as waiting (once, even when replayed on attach), and
    /// an answer for it is never sent even if one reached the loop.
    #[tokio::test]
    async fn view_only_confirm_never_reaches_the_pane_or_an_answer() {
        let (mut b, mut pane) = bridge(ConfirmPolicy::ViewOnly);
        b.on_prompt(prompt(3, PromptKind::Confirm));
        b.on_prompt(prompt(3, PromptKind::Confirm));
        assert!(pane.try_recv().is_err(), "no pane request for a confirm");
        assert_eq!(
            b.waiting_confirms(),
            &[(3, agent_engine::session::CONFIRM_PROMPT_TITLE.to_string())]
        );
        assert!(!b.on_local_answer(3), "a confirm answer is never sent");
        assert!(!b.on_local_answer(99), "nor one for a prompt never fed");
        assert!(b.answers_rx.try_recv().is_err(), "nothing queued to send");
    }

    /// 04-tui M7: every waiting entry read "Confirm tool call", so the user
    /// couldn't tell which approval to look for in the other client. It
    /// names the tool (and its extension) from the scope or the body.
    #[tokio::test]
    async fn a_waiting_confirm_names_its_tool() {
        let (mut b, _pane) = bridge(ConfirmPolicy::ViewOnly);
        let mut from_body = prompt(4, PromptKind::Confirm);
        from_body.prompt = "Extension: jev\nTool: bash\n\nrm -rf build\n\nAllow this tool call?".into();
        b.on_prompt(from_body);
        let mut from_scope = prompt(5, PromptKind::Confirm);
        from_scope.confirm_scope = Some(agent_engine::session::ConfirmScope {
            extensions: vec!["guard".into()],
            tool: Some("write".into()),
            rewritten_by: vec![],
            tool_id: None,
        });
        b.on_prompt(from_scope);
        assert_eq!(
            b.waiting_confirms(),
            &[
                (4, "Confirm tool call: bash (jev)".to_string()),
                (5, "Confirm tool call: write (guard)".to_string()),
            ]
        );
    }

    /// prA m3: only the input owner can arm "Allow all", so a non-owner's
    /// pane never offers it (the scope is stripped); the owner's does.
    #[tokio::test]
    async fn non_owner_pane_gets_no_allow_all_scope() {
        let scoped = |id| {
            let mut p = prompt(id, PromptKind::Confirm);
            p.confirm_scope = Some(agent_engine::session::ConfirmScope {
                extensions: vec!["jev".into()],
                tool: None,
                rewritten_by: vec![],
                tool_id: None,
            });
            p
        };
        let (mut b, mut pane) = bridge(ConfirmPolicy::Answer);
        b.on_prompt(scoped(1));
        assert!(pane.try_recv().unwrap().confirm_scope.is_some(), "owner keeps it");
        b.set_owns_input(false);
        b.on_prompt(scoped(2));
        let req = pane.try_recv().expect("non-owner can still answer y/n");
        assert!(req.confirm_scope.is_none(), "no Allow-all for a non-owner");
    }

    /// Secret prompts are unchanged under `--no-confirm-answers`: the
    /// pane gets them and their answer is sent.
    #[tokio::test]
    async fn view_only_secret_still_goes_through() {
        let (mut b, mut pane) = bridge(ConfirmPolicy::ViewOnly);
        b.on_prompt(prompt(5, PromptKind::Secret));
        let req = pane.try_recv().expect("the pane gets the secret prompt");
        assert_eq!(req.kind, PromptKind::Secret);
        assert!(b.waiting_confirms().is_empty());
        req.response_tx.send(Some("k".into())).unwrap();
        let (id, value) =
            tokio::time::timeout(std::time::Duration::from_secs(2), b.answers_rx.recv())
                .await
                .unwrap()
                .unwrap();
        assert_eq!((id, value.as_deref()), (5, Some("k")));
        assert!(b.on_local_answer(5), "a secret answer is sent");
        assert!(!b.on_resolved(5), "its own resolution is not a dismissal");
    }

    /// Another client's `PromptResolved` clears a waiting confirm and
    /// does not dismiss the secret prompt the pane is showing.
    #[tokio::test]
    async fn foreign_resolution_clears_a_waiting_confirm() {
        let (mut b, mut pane) = bridge(ConfirmPolicy::ViewOnly);
        b.on_prompt(prompt(5, PromptKind::Secret));
        b.on_prompt(prompt(6, PromptKind::Confirm));
        b.on_prompt(prompt(7, PromptKind::Confirm));
        assert!(pane.try_recv().is_ok() && pane.try_recv().is_err());
        assert!(!b.on_resolved(6), "the pane's secret stays");
        assert_eq!(b.waiting_confirms().len(), 1);
        assert_eq!(b.waiting_confirms()[0].0, 7);
        assert!(!b.on_resolved(7));
        assert!(b.waiting_confirms().is_empty());
        assert!(b.on_resolved(5), "the secret itself, resolved elsewhere");
    }

    /// The default policy is today's behaviour: a confirm opens the pane and
    /// its answer is sent.
    #[tokio::test]
    async fn default_policy_answers_confirms_here() {
        let (mut b, mut pane) = bridge(ConfirmPolicy::default());
        b.on_prompt(prompt(4, PromptKind::Confirm));
        assert_eq!(pane.try_recv().unwrap().kind, PromptKind::Confirm);
        assert!(b.waiting_confirms().is_empty());
        assert!(b.on_local_answer(4));
        assert!(!b.on_resolved(4));
    }

    fn refusing_link() -> SessionLink {
        let mut t = ScriptedTransport::new(Runtime::new_headless());
        t.set_refuse_input(Some("input owned by client #2".to_string()));
        SessionLink::new(Box::new(t))
    }

    /// M2: a non-owner's setter is answered `Refused{client: me}`; the
    /// round-trip must return that reason at once, not block until
    /// `ROUNDTRIP_TIMEOUT`.
    #[tokio::test]
    async fn roundtrip_returns_refused_reason_immediately() {
        let mut link = refusing_link();
        let started = std::time::Instant::now();
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            link.set(SessionSetting::Model {
                model: "claude-opus-4-1".into(),
            }),
        )
        .await
        .expect("must not hit the round-trip timeout");
        assert_eq!(
            res.unwrap_err(),
            "set refused: input owned by client #2"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        // Same for every correlated verb.
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            link.engine_command("model", "claude-opus-4-1"),
        )
        .await
        .unwrap();
        assert!(r.unwrap_err().ends_with("input owned by client #2"));
        let r = tokio::time::timeout(std::time::Duration::from_secs(2), link.resume("x"))
            .await
            .unwrap();
        match r {
            Err(e) => assert!(e.ends_with("input owned by client #2"), "{e}"),
            Ok(_) => panic!("resume must be refused"),
        }
        // Nothing leaked into the loop's buffer.
        assert!(link.buffered.is_empty());
    }

    /// After `End`, a channel that closes without `Ended` is a clean close
    /// (`false`, not a hang): `run()` exits 0 on it.
    #[tokio::test]
    async fn wait_ended_distinguishes_ended_from_clean_close() {
        let mut t = ScriptedTransport::new(Runtime::new_headless());
        t.push_event(SessionEventWire::Idle);
        t.push_event(SessionEventWire::Ended {
            reason: agent_engine::session::EndReason::ClientQuit,
        });
        let mut link = SessionLink::new(Box::new(t));
        assert!(link.wait_ended().await);

        let t = ScriptedTransport::new(Runtime::new_headless());
        let mut link = SessionLink::new(Box::new(t));
        assert!(!tokio::time::timeout(
            std::time::Duration::from_secs(1),
            link.wait_ended()
        )
        .await
        .expect("closed channel returns at once"));
    }

    /// A `Refused` addressed to ANOTHER client is not our reply: buffered
    /// for the loop, the real reply still picked.
    #[tokio::test]
    async fn roundtrip_ignores_refused_for_other_clients() {
        let mut t = ScriptedTransport::new(Runtime::new_headless());
        t.push_event(SessionEventWire::Refused {
            client: ClientId(7),
            command: "set".into(),
            reason: "not you".into(),
        });
        let mut link = SessionLink::new(Box::new(t));
        let applied = link
            .set(SessionSetting::ContextWindow {
                tokens: Some(1_000_000),
            })
            .await
            .expect("real reply picked");
        assert!(applied.ok, "{applied:?}");
        assert_eq!(link.buffered.len(), 1);
    }
}
