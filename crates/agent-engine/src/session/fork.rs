//! `fork` (side chats): a new live session that starts from this
//! one's state as sent. The parent copies its history up to its last
//! completed turn, and what its next request is built from, into a
//! [`ForkSeed`]; `EngineHost::create_session` builds the fork from it
//! (`SessionActor::create`) off the parent's loop, and the parent then
//! tells the asking client `forked`, or `refused` with `command: "fork"`.
//!
//! The fork gets no turn until its first message. It copies none of the
//! parent's running work: no turn in flight, no workers, no prompts, no
//! queued message, no keep-warm pin, no spend.

use std::sync::Arc;

use super::actor::SessionActor;
use super::types::*;

/// Most bytes a fork's label may have.
pub const FORK_LABEL_MAX_BYTES: usize = 200;

/// What a fork copies from its parent (`SessionConfig::fork`; host-only,
/// never on the wire).
pub struct ForkSeed {
    /// The parent's journal id (it names the parent's sandbox policy file).
    pub(crate) parent: String,
    /// The parent's history up to its last completed turn.
    pub(crate) messages: Vec<crate::SharedMessage>,
    /// The client tool stamps of the tool results in `messages` (history
    /// metadata; the parent's client tool registration is never copied).
    pub(crate) client_tool_stamps: super::types::ClientToolStamps,
    pub(crate) forked_from: agent_core::session::ForkedFrom,
    /// The parent's non-persisted settings (`settings_replay`).
    pub(crate) settings_replay: Vec<SessionSetting>,
    /// The parent's `on_session_start` injection once that hook has run
    /// (`Some(None)`: it ran and injected nothing). `None`: it has not run
    /// yet, so the parent has sent none, and the fork runs its own hook.
    pub(crate) injection: Option<Option<String>>,
    pub(crate) runtime: crate::runtime::fork::RuntimeForkSeed,
}

impl std::fmt::Debug for ForkSeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForkSeed")
            .field("parent", &self.parent)
            .field("messages", &self.messages.len())
            .field("client_tool_stamps", &self.client_tool_stamps.len())
            .field("forked_from", &self.forked_from)
            .finish_non_exhaustive()
    }
}

/// The actor's fork bookkeeping.
#[derive(Default)]
pub(crate) struct ForkState {
    /// The history's length when the session was last idle.
    idle_len: usize,
    /// The running turn's history before its own messages (taken when the
    /// turn starts): what a fork made during the turn copies.
    turn_point: Option<Vec<crate::SharedMessage>>,
    /// A parked session's `forked_from` (its conversation is not in memory).
    pub(crate) parked_from: Option<agent_core::session::ForkedFrom>,
}

/// Why a fork labelled `label` is refused, if it is.
pub(crate) fn label_refusal(label: &str) -> Option<String> {
    if label.trim().is_empty() {
        Some("fork label is empty".to_string())
    } else if label.len() > FORK_LABEL_MAX_BYTES {
        Some(format!("fork label is longer than {FORK_LABEL_MAX_BYTES} bytes"))
    } else {
        None
    }
}

/// Build the fork `SessionActor::create` is making from `seed`: the
/// parent's settings, then its request state (over what the replay set),
/// its history and `forked_from` on the new session, then the parent's
/// sandbox policy file carried to the fork's id, before the fork can run
/// anything; then the first save (it writes `forked_from`). Dropping the
/// returned policy without `keep()` removes the file this carry created.
pub(crate) async fn seed_child(
    runtime: &mut crate::Runtime,
    sb: &mut crate::engine::setup::SessionBootResult,
    seed: &ForkSeed,
    persist: bool,
) -> crate::Result<crate::sandbox::CarriedPolicy> {
    for setting in &seed.settings_replay {
        super::actor::replay_setting(runtime, setting);
    }
    runtime
        .apply_fork_seed(&seed.runtime, &sb.session.id, &seed.messages)
        .await;
    sb.api_messages = seed.messages.clone();
    sb.session.api_messages = seed.messages.clone();
    sb.session.client_tool_stamps = seed.client_tool_stamps.clone();
    sb.session.forked_from = Some(seed.forked_from.clone());
    sb.session.model = runtime.model().to_string();
    sb.session.thinking_level = runtime.thinking_level().to_string();
    let carried = crate::sandbox::carry_to_successor(&seed.parent, &sb.session.id).map_err(|reason| {
        crate::RuntimeError::Session(format!(
            "fork: the sandbox policy could not be carried to the new session: {reason}"
        ))
    })?;
    if persist && !seed.messages.is_empty() {
        sb.session
            .save()
            .await
            .map_err(|e| crate::RuntimeError::Session(format!("fork: saving the new session failed: {e}")))?;
    }
    Ok(carried)
}

impl SessionActor {
    /// `Fork{label}` from `from`: refuse it, or copy this session's state
    /// and create the fork off the loop; [`Self::fork_done`] answers.
    pub(crate) async fn fork(&mut self, label: String, from: Option<ClientId>) {
        let refusal = if self.conv.session.forked_from.is_some() {
            Some("a forked session can't fork".to_string())
        } else {
            label_refusal(&label)
        };
        if let Some(reason) = refusal {
            self.fork_refused(from, reason);
            return;
        }
        let messages = self.fork_messages();
        let message_count = messages.len();
        let client_tool_stamps = self.conv.session.client_tool_stamps_of(&messages);
        let parent = self.conv.session.id.clone();
        let injection = if self.session_start.is_done() {
            let key = self.runtime.session_id().unwrap_or(parent.as_str()).to_string();
            Some(self.hook_bus.session_injection_for(&key).await)
        } else {
            None
        };
        let seed = ForkSeed {
            forked_from: agent_core::session::ForkedFrom {
                session: parent.clone(),
                message_count,
                at_ms: crate::epoch_millis(),
            },
            parent,
            messages,
            client_tool_stamps,
            settings_replay: self.settings_replay.clone(),
            injection,
            runtime: self.runtime.fork_seed(),
        };
        let mut cfg = self.config.clone();
        cfg.continue_session = None;
        cfg.ephemeral_layer_wait_ms = None;
        cfg.continue_lookup = ContinueLookup::Query;
        cfg.name = None;
        cfg.keep_warm = false;
        cfg.restore_parked = false;
        cfg.restore_parked_since = None;
        cfg.memory_mode = self.effective_memory_mode();
        cfg.model_override = Some(seed.runtime.model().to_string());
        cfg.fork = Some(Arc::new(seed));
        let host = Arc::clone(&self.host);
        let parent_id = self.id.clone();
        tokio::spawn(async move {
            let result = host
                .create_session(cfg)
                .await
                .map(|handle| handle.id.to_string())
                .map_err(|e| e.to_string());
            let Some(parent) = host.session_handles().into_iter().find(|h| h.id == parent_id) else {
                tracing::info!(session = %parent_id, ok = result.is_ok(), "fork done after its parent ended");
                return;
            };
            let _ = parent
                .send(SessionCommand::ForkDone {
                    client: from,
                    label,
                    message_count,
                    result,
                })
                .await;
        });
    }

    /// The fork's create finished: `forked` to the client that asked, or
    /// its refusal.
    pub(crate) fn fork_done(
        &mut self,
        client: Option<ClientId>,
        label: String,
        message_count: usize,
        result: Result<String, String>,
    ) {
        match result {
            Ok(session_id) => self.emit(SessionEventWire::Forked {
                client,
                session_id,
                label,
                message_count,
            }),
            Err(e) => self.fork_refused(client, format!("fork failed: {e}")),
        }
    }

    fn fork_refused(&mut self, client: Option<ClientId>, reason: String) {
        match client {
            Some(client) => self.emit(SessionEventWire::Refused {
                client,
                command: "fork".into(),
                reason,
            }),
            None => self.emit(SessionEventWire::SystemNotice(reason)),
        }
    }

    /// The history a fork copies: all of it while idle; during a turn, the
    /// history before that turn's own messages.
    fn fork_messages(&self) -> Vec<crate::SharedMessage> {
        if !self.streaming {
            return self.conv.api_messages.clone();
        }
        match &self.fork.turn_point {
            Some(point) => point.clone(),
            None => {
                let n = self.fork.idle_len.min(self.conv.api_messages.len());
                self.conv.api_messages[..n].to_vec()
            }
        }
    }

    /// Note the history's length while the session is idle (the actor
    /// loop, after each step; create; unpark).
    pub(crate) fn fork_note_idle(&mut self) {
        if !self.streaming && self.conv.is_live() {
            self.fork.idle_len = self.conv.api_messages.len();
        }
    }

    /// A turn starts: the messages its trigger pushed follow the history
    /// as it was when the session was last idle.
    pub(crate) fn fork_turn_started(&mut self) {
        let n = self.fork.idle_len.min(self.conv.api_messages.len());
        self.fork.turn_point = Some(self.conv.api_messages[..n].to_vec());
    }

    /// A turn ended: its history is complete.
    pub(crate) fn fork_turn_ended(&mut self) {
        self.fork.turn_point = None;
        if self.conv.is_live() {
            self.fork.idle_len = self.conv.api_messages.len();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::wire::WireSessionEvent;

    #[test]
    fn a_label_must_be_nonblank_and_at_most_200_bytes() {
        assert_eq!(label_refusal("Side: Fix the tray"), None);
        assert_eq!(label_refusal(&"a".repeat(200)), None);
        assert!(label_refusal("").is_some());
        assert!(label_refusal("  \t").is_some());
        assert!(label_refusal(&"a".repeat(201)).is_some());
        // Bytes, not characters: 100 two-byte characters fit, 101 do not.
        assert_eq!(label_refusal(&"é".repeat(100)), None);
        assert!(label_refusal(&"é".repeat(101)).is_some());
    }

    #[test]
    fn fork_is_exactly_the_contract_command() {
        let line = r#"{"cmd":"fork","label":"Side: Fix the tray"}"#;
        let cmd: SessionCommand = serde_json::from_str(line).unwrap();
        match &cmd {
            SessionCommand::Fork { label } => assert_eq!(label, "Side: Fix the tray"),
            other => panic!("decoded as {other:?}"),
        }
        assert_eq!(serde_json::to_string(&cmd).unwrap(), line);
        assert!(cmd.refused_while_reloading(), "a fork waits out a reload");
        assert_eq!(format!("{cmd:?}"), "Fork { label: <redacted> }");
    }

    #[test]
    fn forked_is_exactly_the_contract_event_and_names_no_client() {
        let event = SessionEventWire::Forked {
            client: Some(ClientId(3)),
            session_id: "20261021-013000-ab12".into(),
            label: "Side: Fix the tray".into(),
            message_count: 412,
        };
        let wire = WireSessionEvent::for_client(event).expect("forked has a client form");
        let line = serde_json::to_string(&wire).unwrap();
        assert_eq!(
            line,
            r#"{"ev":"forked","session_id":"20261021-013000-ab12","label":"Side: Fix the tray","message_count":412}"#
        );
        let back: WireSessionEvent = serde_json::from_str(&line).unwrap();
        match SessionEventWire::from(back) {
            SessionEventWire::Forked {
                client,
                session_id,
                label,
                message_count,
            } => {
                assert_eq!(client, None, "the wire form carries no client");
                assert_eq!(session_id, "20261021-013000-ab12");
                assert_eq!(label, "Side: Fix the tray");
                assert_eq!(message_count, 412);
            }
            other => panic!("decoded as {other:?}"),
        }
    }

    #[test]
    fn a_refused_fork_names_the_fork_command() {
        let wire = WireSessionEvent::for_client(SessionEventWire::Refused {
            client: ClientId(3),
            command: "fork".into(),
            reason: "a forked session can't fork".into(),
        })
        .unwrap();
        assert_eq!(
            serde_json::to_string(&wire).unwrap(),
            r#"{"ev":"refused","client":3,"command":"fork","reason":"a forked session can't fork"}"#
        );
    }
}
