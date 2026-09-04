//! `ScriptedTransport` — an in-memory `ClientTransport` for the harness and
//! unit tests (PLAN-phase3 §5.1 layer 2). Records every command it is sent,
//! answers the correlated ones (`Set{id}` → `SettingChanged{id}`,
//! `EngineCommand{id}` / `PluginCommand{id}` / `Query{id}` → `QueryResult`)
//! from a headless `Runtime` exactly as the actor would, and lets a test
//! feed arbitrary envelopes (`push_event`) that `next_event` then yields.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use agent_engine::session::{
    AttachMode, AttachSnapshot, ClientId, ClientTransport, Envelope, ReasoningClampWire,
    RuntimeView, SessionCommand, SessionEventWire, SessionId, SessionMeta, SessionQuery,
    SessionSetting, SettingApplied, TransportError,
};
use synaps_cli::{Runtime, Session};

/// Shared state so a test can inspect what was sent while the harness owns
/// the `Box<dyn ClientTransport>`.
#[derive(Default)]
pub struct ScriptedLog {
    /// `Debug` renderings (bodies redacted by the manual impl).
    pub sent: Vec<String>,
}

pub struct ScriptedTransport {
    id: SessionId,
    meta: SessionMeta,
    runtime: Runtime,
    session: Session,
    api_messages: Vec<synaps_cli::SharedMessage>,
    view: Arc<RuntimeView>,
    queue: VecDeque<Envelope>,
    seq: u64,
    pub log: Arc<Mutex<ScriptedLog>>,
    /// Commands sent through `&self`, handled at the next `next_event`.
    pending: Mutex<VecDeque<SessionCommand>>,
    /// When set, every input command is answered `Refused{client: me,
    /// reason}` with no side effect (B1 ownership, non-owner client).
    refuse_input: Option<String>,
}

impl ScriptedTransport {
    pub fn new(runtime: Runtime) -> Self {
        let session = Session::new(runtime.model(), runtime.thinking_level(), None);
        Self::with_session(runtime, session)
    }

    pub fn with_session(runtime: Runtime, session: Session) -> Self {
        let id = SessionId::from(session.id.clone());
        let meta = SessionMeta {
            id: id.clone(),
            name: session.name.clone(),
            model: runtime.model().to_string(),
            cwd: None,
            created_at: session.created_at,
            continued: false,
            continue_info: None,
            host_pid: std::process::id(),
            lifecycle: Default::default(),
            clients: 1,
            input_owner: Some(ClientId(1)),
            awaiting_input: 0,
            journal_id: session.id.clone(),
        };
        let view = Arc::new(RuntimeView::snapshot(&runtime, 0));
        Self {
            id,
            meta,
            runtime,
            session,
            api_messages: Vec::new(),
            view,
            queue: VecDeque::new(),
            seq: 0,
            log: Arc::new(Mutex::new(ScriptedLog::default())),
            pending: Mutex::new(VecDeque::new()),
            refuse_input: None,
        }
    }

    /// Behave as the actor does for a non-owner: input commands are
    /// `Refused` with `reason` (None = owner, normal handling).
    pub fn set_refuse_input(&mut self, reason: Option<String>) {
        self.refuse_input = reason;
    }

    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    pub fn runtime_mut(&mut self) -> &mut Runtime {
        &mut self.runtime
    }

    pub fn set_api_messages(&mut self, msgs: Vec<synaps_cli::SharedMessage>) {
        self.api_messages = msgs;
    }

    /// Queue an envelope for `next_event` (tape `SessionEvent` steps).
    pub fn push_event(&mut self, event: SessionEventWire) {
        let env = Envelope {
            session_id: self.id.clone(),
            seq: self.seq,
            ts: chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap_or_default(),
            event,
        };
        self.seq += 1;
        self.queue.push_back(env);
    }

    pub fn sent(&self) -> Vec<String> {
        self.log.lock().unwrap().sent.clone()
    }

    fn refresh_view(&mut self) {
        self.view = Arc::new(RuntimeView::snapshot(&self.runtime, 0));
    }

    /// = `SessionActor::apply_setting` (actor.rs) on the headless runtime.
    fn apply_setting(&mut self, id: u64, setting: SessionSetting) {
        let name = match &setting {
            SessionSetting::Model { .. } => "model",
            SessionSetting::ReasoningLevel { .. } => "reasoning_level",
            SessionSetting::ContextWindow { .. } => "context_window",
            SessionSetting::CompactionModel { .. } => "compaction_model",
            SessionSetting::ApiRetries { .. } => "api_retries",
            SessionSetting::SubagentTimeout { .. } => "subagent_timeout",
            SessionSetting::MaxToolOutput { .. } => "max_tool_output",
            SessionSetting::BashTimeout { .. } => "bash_timeout",
            SessionSetting::BashMaxTimeout { .. } => "bash_max_timeout",
            SessionSetting::SystemPrompt { .. } => "system_prompt",
            SessionSetting::ReloadPrompt => "reload_prompt",
            SessionSetting::GrantWorkerModel { .. } => "grant_worker_model",
        };
        let rt = &mut self.runtime;
        let mut clamp_wire = None;
        let result: Result<Option<String>, String> = match setting {
            SessionSetting::Model { model } => rt.try_set_model(model).map(|clamp| {
                self.session.model = rt.model().to_string();
                clamp.map(|c| {
                    self.session.thinking_level = rt.thinking_level().to_string();
                    clamp_wire = Some(ReasoningClampWire {
                        from: c.from.as_str().to_string(),
                        to: c.to.as_str().to_string(),
                    });
                    format!(
                        "thinking → {} (clamped from {}: not supported by {})",
                        c.to.as_str(),
                        c.from.as_str(),
                        rt.model()
                    )
                })
            }),
            SessionSetting::ReasoningLevel { level } => {
                rt.set_reasoning_level_checked(level).map(|_| {
                    self.session.thinking_level = rt.thinking_level().to_string();
                    None
                })
            }
            SessionSetting::ContextWindow { tokens } => {
                rt.set_context_window(tokens);
                Ok(None)
            }
            SessionSetting::CompactionModel { model } => {
                rt.set_compaction_model(model);
                Ok(None)
            }
            SessionSetting::ApiRetries { n } => {
                rt.set_api_retries(n);
                Ok(None)
            }
            SessionSetting::SubagentTimeout { secs } => {
                rt.set_subagent_timeout(secs);
                Ok(None)
            }
            SessionSetting::MaxToolOutput { bytes } => {
                rt.set_max_tool_output(bytes);
                Ok(None)
            }
            SessionSetting::BashTimeout { secs } => {
                rt.set_bash_timeout(secs);
                Ok(None)
            }
            SessionSetting::BashMaxTimeout { secs } => {
                rt.set_bash_max_timeout(secs);
                Ok(None)
            }
            SessionSetting::SystemPrompt { text } => {
                rt.set_system_prompt(text);
                Ok(None)
            }
            SessionSetting::ReloadPrompt => rt
                .reload_prompt()
                .map(|generation| Some(format!("prompt reloaded (generation {generation})")))
                .map_err(|e| e.to_string()),
            SessionSetting::GrantWorkerModel { model } => {
                rt.grant_worker_model(&model).map(|_| None)
            }
        };
        self.refresh_view();
        let (ok, message) = match result {
            Ok(m) => (true, m),
            Err(e) => (false, Some(e)),
        };
        self.push_event(SessionEventWire::SettingChanged(SettingApplied {
            id,
            setting: name.to_string(),
            ok,
            message,
            view: (*self.view).clone(),
            clamp: clamp_wire,
        }));
    }

    async fn handle(&mut self, cmd: SessionCommand) {
        if let Some(reason) = self.refuse_input.clone() {
            let command = match &cmd {
                SessionCommand::Submit { .. } => Some("submit"),
                SessionCommand::Steer { .. } => Some("steer"),
                SessionCommand::Set { .. } => Some("set"),
                SessionCommand::EngineCommand { .. } => Some("engine_command"),
                SessionCommand::PluginCommand { .. } => Some("plugin_command"),
                SessionCommand::Resume { .. } => Some("resume"),
                SessionCommand::Compact { .. } => Some("compact"),
                SessionCommand::NewSession => Some("new_session"),
                _ => None,
            };
            if let Some(command) = command {
                self.push_event(SessionEventWire::Refused {
                    client: ClientId(1),
                    command: command.to_string(),
                    reason,
                });
                return;
            }
        }
        match cmd {
            SessionCommand::Set { id, setting } => self.apply_setting(id, setting),
            SessionCommand::EngineCommand { id, name, arg } => {
                let (value, view_changed) = agent_engine::session::actor_cmds::engine_command_reply(
                    &name,
                    &arg,
                    &mut self.runtime,
                    &mut self.session,
                );
                if view_changed {
                    self.refresh_view();
                }
                self.push_event(SessionEventWire::QueryResult { id, value });
            }
            SessionCommand::PluginCommand {
                id,
                plugin,
                name,
                arg,
            } => {
                // Test seam: the command is looked up in the scripted
                // registry the test installed via `plugin_commands`.
                let value = match self.plugin_command(&plugin, &name) {
                    Some(command) => {
                        match synaps_cli::skills::commands::execute_plugin_command_with_tools(
                            &command,
                            &arg,
                            self.runtime.tools_shared(),
                        )
                        .await
                        {
                            Ok(output) => serde_json::json!({
                                "kind": "plugin_output",
                                "status": output.status,
                                "stdout": output.stdout,
                                "stderr": output.stderr,
                            }),
                            Err(e) => serde_json::json!({ "kind": "error", "text": e.to_string() }),
                        }
                    }
                    None => serde_json::json!({
                        "kind": "error",
                        "text": format!("unknown plugin command /{plugin}:{name}"),
                    }),
                };
                self.push_event(SessionEventWire::QueryResult { id, value });
            }
            SessionCommand::Query { id, query } => {
                let value = match query {
                    SessionQuery::View => serde_json::to_value(&*self.view).unwrap_or_default(),
                    SessionQuery::ContextReport => {
                        use synaps_cli::engine::commands::{context_command, CommandResult};
                        match context_command(&self.runtime, Some(&self.api_messages)) {
                            CommandResult::Output(text) => serde_json::json!({ "text": text }),
                            CommandResult::Error(e) => serde_json::json!({ "error": e }),
                            other => serde_json::json!({ "unsupported": format!("{other:?}") }),
                        }
                    }
                    SessionQuery::Messages => {
                        serde_json::to_value(&self.api_messages).unwrap_or_default()
                    }
                    other => serde_json::json!({ "unsupported": format!("{other:?}") }),
                };
                self.push_event(SessionEventWire::QueryResult { id, value });
            }
            SessionCommand::Resume { id, .. } => self.push_event(SessionEventWire::QueryResult {
                id,
                value: serde_json::json!({ "kind": "error", "text": "scripted: no sessions on disk" }),
            }),
            SessionCommand::NewSession => {
                self.session = Session::new(
                    self.runtime.model(),
                    self.runtime.thinking_level(),
                    self.runtime.system_prompt(),
                );
                self.api_messages.clear();
                self.push_event(SessionEventWire::Cleared {
                    session_id: self.session.id.clone(),
                });
                self.push_event(SessionEventWire::Conversation(
                    agent_engine::session::ConversationSnapshot {
                        header: agent_engine::session::SessionHeader::from(&self.session),
                        ..Default::default()
                    },
                ));
            }
            // Turn-machine commands: recorded only (the tape feeds the
            // envelopes a real actor would answer with).
            _ => {}
        }
    }

    fn plugin_command(
        &self,
        plugin: &str,
        name: &str,
    ) -> Option<Arc<synaps_cli::skills::registry::RegisteredPluginCommand>> {
        let reg = PLUGIN_COMMANDS.lock().unwrap();
        reg.iter()
            .find(|c| c.plugin == plugin && c.name == name)
            .cloned()
    }
}

/// Plugin commands the scripted actor can run (tests register theirs).
static PLUGIN_COMMANDS: Mutex<Vec<Arc<synaps_cli::skills::registry::RegisteredPluginCommand>>> =
    Mutex::new(Vec::new());

/// Register a plugin command for `ScriptedTransport::plugin_command`.
pub fn register_plugin_command(cmd: Arc<synaps_cli::skills::registry::RegisteredPluginCommand>) {
    PLUGIN_COMMANDS.lock().unwrap().push(cmd);
}

#[async_trait::async_trait]
impl ClientTransport for ScriptedTransport {
    fn session_id(&self) -> &SessionId {
        &self.id
    }
    fn meta(&self) -> &SessionMeta {
        &self.meta
    }
    async fn send(&self, cmd: SessionCommand) -> Result<(), TransportError> {
        // `&self`: handling happens in `next_event`'s drain.
        self.log.lock().unwrap().sent.push(format!("{cmd:?}"));
        self.pending.lock().unwrap().push_back(cmd);
        Ok(())
    }
    async fn send_from_self(&self, cmd: SessionCommand) -> Result<(), TransportError> {
        self.send(cmd).await
    }
    async fn next_event(&mut self) -> Option<Envelope> {
        // Handle everything sent since the last poll (in order), then yield.
        let pending: Vec<SessionCommand> = self.pending.lock().unwrap().drain(..).collect();
        for cmd in pending {
            self.handle(cmd).await;
        }
        self.queue.pop_front()
    }
    fn view(&self) -> Arc<RuntimeView> {
        Arc::clone(&self.view)
    }
    fn client_id(&self) -> ClientId {
        ClientId(1)
    }
    fn mode(&self) -> AttachMode {
        AttachMode::Mirror
    }
    fn input_owner(&self) -> Option<ClientId> {
        Some(ClientId(1))
    }
    async fn reconnect(&mut self, _mode: AttachMode) -> Result<AttachSnapshot, TransportError> {
        Err(TransportError::Unsupported("reconnect: scripted transport"))
    }
}
