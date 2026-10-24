//! `SessionActor` — command bodies that belong to the TUI port (A3):
//! `engine_command` (structured reply), `submit_prepared`, `plugin_command`,
//! `resume`, `session_name`, `emit_subagent_rows`. Kept apart from
//! `actor.rs` so the turn machine (B) and the command surface (A) never
//! touch the same file.

use std::sync::Arc;

use super::actor::{Live, SessionActor};
use super::types::*;

/// `/compact [instructions]` as the engine parses it (no runtime mutation).
fn compact_request(cmd: &str, arg: &str) -> Option<crate::engine::commands::CommandResult> {
    match crate::engine::commands::evaluate_engine_command(cmd, arg) {
        Some(r @ crate::engine::commands::CommandResult::Compact { .. }) => Some(r),
        _ => None,
    }
}

/// `handle_engine_command` on `runtime` + the `QueryResult` value clients
/// render. Returns `(value, view_changed)`. Shared by the actor and the
/// TUI's `ScriptedTransport` so both answer identically.
pub fn engine_command_reply(
    cmd: &str,
    arg: &str,
    runtime: &mut crate::Runtime,
    session: &mut crate::Session,
) -> (serde_json::Value, bool) {
    use crate::engine::commands::{handle_engine_command, CommandResult};
    match handle_engine_command(cmd, arg, runtime) {
        None => (serde_json::json!({ "kind": "unhandled" }), false),
        Some(CommandResult::Quit) => (serde_json::json!({ "kind": "quit" }), false),
        Some(CommandResult::ModelChanged {
            model,
            reasoning_clamped,
        }) => {
            session.model = runtime.model().to_string();
            let mut text = format!("model → {}", model);
            let mut clamp = serde_json::Value::Null;
            if let Some(c) = reasoning_clamped {
                session.thinking_level = runtime.thinking_level().to_string();
                text.push_str(&format!(
                    "\nthinking → {} (clamped from {}: not supported by {})",
                    c.to.as_str(),
                    c.from.as_str(),
                    runtime.model()
                ));
                clamp = serde_json::json!({ "from": c.from.as_str(), "to": c.to.as_str() });
            }
            (
                serde_json::json!({
                    "kind": "notice",
                    "event": "model_changed",
                    "text": text,
                    "model": runtime.model(),
                    "clamp": clamp,
                }),
                true,
            )
        }
        Some(CommandResult::ThinkingChanged { spec }) => {
            session.thinking_level = spec.config_value();
            (
                serde_json::json!({
                    "kind": "notice",
                    "event": "thinking_changed",
                    "text": format!("thinking → {}", spec.level()),
                    "level": spec.level().to_string(),
                    "config_value": spec.config_value(),
                }),
                true,
            )
        }
        Some(CommandResult::Compact { .. }) => (serde_json::json!({ "kind": "none" }), false),
        Some(CommandResult::Error(e)) => (serde_json::json!({ "kind": "error", "text": e }), false),
        Some(CommandResult::Output(text)) => {
            (serde_json::json!({ "kind": "output", "text": text }), false)
        }
        Some(_) => (serde_json::json!({ "kind": "none" }), false),
    }
}

/// `/auth reload` → `{kind: output|error, text}`. Token values never appear:
/// success text is built by `Runtime::reload_auth`, failures carry the broker
/// error (which is token-free by construction).
pub async fn auth_command_reply(arg: &str, runtime: &crate::Runtime) -> serde_json::Value {
    match arg.trim() {
        "reload" => match runtime.reload_auth().await {
            Ok(text) => serde_json::json!({ "kind": "output", "text": text }),
            Err(e) => serde_json::json!({
                "kind": "error",
                "text": format!("auth reload failed: {e}"),
            }),
        },
        _ => serde_json::json!({ "kind": "error", "text": "usage: /auth reload" }),
    }
}

impl SessionActor {
    /// `engine::commands::handle_engine_command` on THE runtime; reply as a
    /// `QueryResult` the client renders. `kind`/`text` are what chat.rs
    /// reads (`notice`/`error`/`output`/`quit`/`unhandled`); the TUI reads
    /// the structured extras (`event`, `model`, `clamp`, `level`,
    /// `config_value`) to render its own lines byte-for-byte.
    pub(crate) async fn engine_command(&mut self, id: u64, cmd: String, arg: String) {
        use crate::engine::commands::CommandResult;
        // TUI-only commands that need the actor's session (it owns the
        // journal): `saveas` names/unnames + force-saves.
        if cmd == "saveas" {
            let value = self.session_name(&arg).await;
            self.emit(SessionEventWire::QueryResult { id, value });
            return;
        }
        // `/keepwarm [status|off|auto|<N>h]`: actor-owned prompt-cache
        // keep-warm state (task #444).
        if cmd == "keepwarm" {
            let value = self.cache_keepwarm_command(&arg).await;
            self.emit(SessionEventWire::QueryResult { id, value });
            return;
        }
        // `/auth reload` is async (broker round-trip), so it cannot live in
        // the sync `handle_engine_command`. Never echoes a token.
        if cmd == "auth" {
            let value = auth_command_reply(&arg, &self.runtime).await;
            self.emit(SessionEventWire::QueryResult { id, value });
            return;
        }
        crate::engine::commands::prepare_engine_command(&cmd, &arg, &self.runtime).await;
        // `/workers cancel`: which workers this command stops (a cancel
        // already in flight is not this command's).
        let cancelling = cmd == "workers" && arg.trim_start().starts_with("cancel");
        let uncancelled = if cancelling { self.uncancelled_workers() } else { Vec::new() };
        let (value, view_changed) =
            engine_command_reply(&cmd, &arg, &mut self.runtime, &mut self.conv.session);
        if cancelling {
            let after = self
                .runtime
                .subagent_registry()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .display_rows();
            for (id, agent) in &cancelled_by_command(uncancelled, &after) {
                self.note_in_main_conversation(super::actor::subagent_user_cancel_event(id, agent));
            }
        }
        if cmd == "memory" {
            self.note_memory_choice(&arg);
            self.observe_memory();
        }
        if view_changed {
            self.publish_view().await;
        }
        if let Some(CommandResult::Compact {
            custom_instructions,
        }) = compact_request(&cmd, &arg)
        {
            self.compact(custom_instructions, "manual").await;
        }
        self.emit(SessionEventWire::QueryResult { id, value });
    }

    /// The memory choice a rebuilt runtime must honour: the recorded one,
    /// or `Off` once memory is effectively off while a recorded on-mode or
    /// the operator's durable default would grant it again. Consent can be
    /// revoked without `/memory` — the model's `memory_context` tool
    /// (`action: "disable"`), a lease expiry, a host-side revoke — and
    /// recording `Off` is always the safe direction. On-modes are recorded
    /// only from `/memory` ([`Self::note_memory_choice`]): the tool cannot
    /// enable (that needs the command's ExplicitCommand proof), so no other
    /// path may make one sticky.
    ///
    /// Off counts as a revocation only once memory was seen active in this
    /// runtime (`memory_seen_active`): a grant that never took effect — an
    /// extension-backed provider not registered yet after a reload or
    /// unpark — leaves the choice alone, so the next rebuild retries it.
    pub(crate) fn effective_memory_mode(&self) -> Option<super::SessionMemoryMode> {
        use crate::runtime::memory_context::DurableStatus;
        let recorded = self.config.memory_mode;
        if !self.runtime.is_live() {
            return recorded;
        }
        let off = matches!(self.runtime.memory_context_status().durable, DurableStatus::Off);
        let would_grant = match recorded {
            Some(mode) => mode.lease_mode().is_some(),
            None => self.memory_default_is_durable(),
        };
        if off && would_grant && self.memory_seen_active {
            Some(super::SessionMemoryMode::Off)
        } else {
            recorded
        }
    }

    /// The operator's confirmed `memory.default_mode` grants a durable
    /// session lease (`recall_once` is a one-shot, not a mode).
    fn memory_default_is_durable(&self) -> bool {
        let memory = &self.host.config().memory;
        memory.default_mode_confirmed
            && matches!(
                memory.default_mode.as_str(),
                "recall_each_prompt" | "capture_only" | "capture_and_recall"
            )
    }

    /// Latch `memory_seen_active` if this runtime's memory is active now.
    /// Called where a grant can take effect: the runtime's build, `/memory`,
    /// each turn's start and end.
    pub(crate) fn observe_memory(&mut self) {
        use crate::runtime::memory_context::DurableStatus;
        if self.runtime.is_live()
            && matches!(
                self.runtime.memory_context_status().durable,
                DurableStatus::Active { .. }
            )
        {
            self.memory_seen_active = true;
        }
    }

    /// Record [`Self::effective_memory_mode`] (config + the handle's slot).
    /// A revocation recorded here (not by `/memory`) is told to the user.
    pub(crate) fn sync_memory_choice(&mut self) {
        self.observe_memory();
        let mode = self.effective_memory_mode();
        if mode != self.config.memory_mode {
            self.config.memory_mode = mode;
            *self
                .memory_mode_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = mode;
            if mode == Some(super::SessionMemoryMode::Off) {
                self.emit(SessionEventWire::SystemNotice(
                    "memory stays off for this session: its consent was revoked, and a park or daemon reload keeps it off (/memory on turns it back on)"
                        .into(),
                ));
            }
        }
    }

    /// `/close [force]`: end this session in the daemon. It is saved and
    /// ended exactly like the in-process quit (`End{ClientQuit}`), leaves the
    /// daemon's session list, and every attached client is told (`Ended`);
    /// `synaps --attach --continue <id>` brings it back from its journal.
    /// Over a socket `/quit` only detaches, which leaves the session loaded.
    ///
    /// Refused while closing would destroy work in flight: a running turn or
    /// compaction, a running worker (a session's workers die with its
    /// runtime), an armed `/auto` run. `force` closes anyway: it cancels those
    /// workers first and notes each one in the conversation, for whoever
    /// continues it. A parked session has none of these and closes without
    /// being rebuilt.
    ///
    /// Owner-only, like every engine command. Replies on the `QueryResult`
    /// (`output` / `error`) before the `Ended`. `Some(reason)` = end now.
    pub(crate) fn close_command(&mut self, id: u64, arg: &str) -> Option<EndReason> {
        let reply = |text: String, kind: &str| SessionEventWire::QueryResult {
            id,
            value: serde_json::json!({ "kind": kind, "text": text }),
        };
        let force = match arg.trim() {
            "" => false,
            "force" => true,
            _ => {
                self.emit(reply("usage: /close [force]".into(), "error"));
                return None;
            }
        };
        let live = self.runtime.is_live();
        let workers: Vec<(String, String)> = if live {
            let mut running: Vec<(String, String)> = self
                .runtime
                .subagent_registry()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .display_rows()
                .into_iter()
                .filter(|r| {
                    r.status == crate::runtime::subagent::SubagentStatus::Running
                        && !r.cancel_requested
                })
                .map(|r| (r.handle_id, r.agent_name))
                .collect();
            running.sort();
            running
        } else {
            Vec::new()
        };
        let mut blockers = Vec::new();
        if self.streaming {
            blockers.push("a turn is running".to_string());
        }
        if self.compact.is_some() {
            blockers.push("a compaction is running".to_string());
        }
        if !workers.is_empty() {
            let names: Vec<String> = workers.iter().map(|(id, agent)| format!("{id} {agent}")).collect();
            blockers.push(format!(
                "{} worker{} running ({})",
                workers.len(),
                if workers.len() == 1 { " is" } else { "s are" },
                names.join(", ")
            ));
        }
        if self.driver.is_some() {
            blockers.push("an /auto run is armed".to_string());
        }
        if !blockers.is_empty() && !force {
            self.emit(reply(
                format!(
                    "not closed: {}. Closing the session would stop {}. Wait, or use /close force.",
                    blockers.join("; "),
                    if blockers.len() == 1 { "it" } else { "them" },
                ),
                "error",
            ));
            return None;
        }
        if !workers.is_empty() {
            let cancelled = self
                .runtime
                .subagent_registry()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .cancel_running(crate::runtime::subagent::WorkerCancelScope::All);
            for (handle_id, agent) in workers.iter().filter(|(h, _)| cancelled.contains(h)) {
                self.note_in_main_conversation(super::actor::subagent_close_cancel_event(handle_id, agent));
            }
        }
        let session = (**self.journal_id.load()).clone();
        // No history, no journal: there is nothing to continue (a parked
        // session always has one).
        let text = if self.conv.is_live() && self.conv.api_messages.is_empty() {
            format!("closing session {session} (it never ran a turn, so there is nothing to resume)")
        } else {
            format!("closing session {session}; resume it with: synaps --attach --continue {session}")
        };
        self.emit(reply(text, "output"));
        Some(EndReason::ClientQuit)
    }

    /// `(id, agent)` of every worker with no cancel requested, sorted.
    fn uncancelled_workers(&self) -> Vec<(String, String)> {
        let mut workers: Vec<(String, String)> = self
            .runtime
            .subagent_registry()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .display_rows()
            .into_iter()
            .filter(|r| !r.cancel_requested)
            .map(|r| (r.handle_id, r.agent_name))
            .collect();
        workers.sort();
        workers
    }

    /// `/memory on|recall|capture|off` that took effect becomes the
    /// session's own choice (`SessionConfig::memory_mode`): a rebuilt
    /// runtime restores it rather than the operator default. A failed grant
    /// leaves the previous choice in place.
    fn note_memory_choice(&mut self, arg: &str) {
        use crate::runtime::memory_context::DurableStatus;
        let Some(choice) = super::SessionMemoryMode::from_command_arg(arg) else {
            return;
        };
        let took = match (self.runtime.memory_context_status().durable, choice.lease_mode()) {
            (DurableStatus::Off, None) => true,
            (DurableStatus::Active { mode, .. }, Some(want)) => mode == want,
            _ => false,
        };
        if took {
            self.config.memory_mode = Some(choice);
            *self
                .memory_mode_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(choice);
        }
    }

    /// `/saveas <name>` | `/saveas` (clear). Force-saves even with no
    /// messages — persist the name change (commands.rs `saveas` arm).
    async fn session_name(&mut self, arg: &str) -> serde_json::Value {
        let trimmed = arg.trim();
        // This saves `conv.session` directly (even with no messages): a
        // queued save of it, without the new name, must not land after.
        self.persister.flush().await;
        if trimmed.is_empty() {
            self.conv.session.clear_name();
            let _ = self.conv.session.save().await;
            self.sync_name();
            self.emit_conversation();
            return serde_json::json!({
                "kind": "output", "text": "session name cleared", "name": serde_json::Value::Null,
            });
        }
        match self.conv.session.set_name(trimmed) {
            Ok(()) => {
                let _ = self.conv.session.save().await;
                self.sync_name();
                self.emit_conversation();
                serde_json::json!({
                    "kind": "output",
                    "text": format!("session named '{}'", trimmed),
                    "name": trimmed,
                })
            }
            Err(e) => serde_json::json!({ "kind": "error", "text": e.to_string() }),
        }
    }

    /// dispatch.rs LoadSkill (:330-360): pre-built tool_use/tool_result pair
    /// (+ optional user text) then a turn. Does NOT reset
    /// `consecutive_auto_turns`.
    pub(crate) async fn submit_prepared(
        &mut self,
        messages: Vec<crate::SharedMessage>,
        user_text: Option<String>,
    ) {
        if self.streaming {
            self.emit(SessionEventWire::SystemNotice(
                "cannot load a skill while streaming".into(),
            ));
            return;
        }
        self.conv.api_messages.extend(messages);
        if let Some(text) = user_text {
            self.conv
                .api_messages
                .push(Arc::new(serde_json::json!({"role": "user", "content": text})));
        }
        self.start_turn(TurnTrigger::PluginCommand, None).await;
    }

    /// commands.rs:123-158: tools-backed (non-interactive) plugin command on
    /// THE runtime's tool set. Reply `{kind:"plugin_output", status, stdout,
    /// stderr}` or `{kind:"error", text}`.
    pub(crate) async fn plugin_command(&mut self, id: u64, plugin: String, name: String, arg: String) {
        let value = match Self::find_plugin_command(&plugin, &name) {
            None => serde_json::json!({
                "kind": "error",
                "text": format!("unknown plugin command /{plugin}:{name}"),
            }),
            Some(cmd) => match crate::skills::commands::execute_plugin_command_with_tools(
                &cmd,
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
            },
        };
        self.emit(SessionEventWire::QueryResult { id, value });
    }

    fn find_plugin_command(
        plugin: &str,
        name: &str,
    ) -> Option<Arc<crate::skills::registry::RegisteredPluginCommand>> {
        let host = crate::host::EngineHost::current()?;
        match host.command_registry().resolve(&format!("{plugin}:{name}")) {
            crate::skills::registry::Resolution::PluginCommand(c) => Some(c),
            _ => None,
        }
    }

    /// commands.rs:647-693 `/resume`: save current, load `query`, restore
    /// model/reasoning/system prompt, swap conversation. Reply `Resumed{id}`
    /// then `Conversation`; error → `QueryResult{id, {kind:"error", text}}`.
    pub(crate) async fn resume(&mut self, id: u64, query: String) {
        if self.streaming {
            self.emit(SessionEventWire::QueryResult {
                id,
                value: serde_json::json!({ "kind": "error", "text": "cannot resume while streaming" }),
            });
            return;
        }
        // Queued saves first: resuming the CURRENT session reloads it from
        // disk, which must hold its latest state.
        self.persister.flush().await;
        let session = match crate::resolve_session(&query) {
            Ok(s) => s,
            Err(e) => {
                self.emit(SessionEventWire::QueryResult {
                    id,
                    value: serde_json::json!({ "kind": "error", "text": e.to_string() }),
                });
                return;
            }
        };
        let old_id = self.conv.session.id.clone();
        let new_id = session.id.clone();
        // F10: the journal lock follows the conversation. Take the new lock
        // FIRST, before anything changes: a session locked by another
        // process (or another session of this daemon) is live there right
        // now, and resuming it here would fork its history. The old lock is
        // only released once the new one is held.
        let new_lock = if new_id == old_id {
            None // resuming the current session: keep its lock
        } else {
            match Self::try_lock_session(&new_id) {
                Ok(lock) => Some(lock),
                Err(e @ agent_core::session_lock::SessionLockError::Held { .. })
                | Err(e @ agent_core::session_lock::SessionLockError::CompactedInto { .. }) => {
                    self.emit(SessionEventWire::QueryResult {
                        id,
                        value: serde_json::json!({
                            "kind": "error",
                            "text": format!("cannot resume: {e}"),
                        }),
                    });
                    return;
                }
                Err(e) => {
                    // I/O trouble (e.g. a read-only sessions dir): proceed
                    // unlocked, as create/unpark do.
                    tracing::warn!(session = %new_id, "resume: session lock: {e}");
                    None
                }
            }
        };
        // Committed to the swap: end keep-warm now, booking any ping in
        // flight to the conversation being left (saved just below).
        self.cache_keepwarm_conversation_replaced();
        self.runtime.set_model(session.model.clone());
        // A resumed session owns its saved choice. Preserve that explicit
        // provenance across later model switches.
        let clamp_notice = self
            .runtime
            .restore_session_reasoning(&session.thinking_level)
            .map(|clamp| {
                format!(
                    "thinking → {} (clamped from {}: not supported by {})",
                    clamp.to.as_str(),
                    clamp.from.as_str(),
                    self.runtime.model()
                )
            });
        if let Some(ref sp) = session.system_prompt {
            self.runtime.set_system_prompt(sp.clone());
        }
        self.save().await;
        let via = if crate::chain::load_chain(&query).is_ok() {
            Some(format!("chain '{}'", query))
        } else if crate::find_session_by_name(&query).is_ok() {
            Some(format!("name '{}'", query))
        } else {
            None
        };
        if new_id != old_id {
            // Drops (releases) the old lock; `None` = proceeding unlocked.
            self.session_lock = new_lock;
        }
        self.conv = Live::new(crate::engine::session::ConversationState::from_resumed(session));
        // Crash recovery is the lock holder's alone (`recover_turn_draft`).
        if self.config.persist && self.session_lock.is_some() {
            crate::engine::setup::recover_turn_draft(&mut self.conv).await;
        }
        if clamp_notice.is_some() {
            // Keep the session file in sync with the clamped runtime.
            self.conv.session.thinking_level = self.runtime.thinking_level().to_string();
        }
        self.consecutive_auto_turns = 0;
        self.runtime.begin_conversation(Some(new_id.clone()));
        self.journal_id.store(Arc::new(new_id.clone()));
        self.publish_view().await;
        self.emit(SessionEventWire::Resumed {
            id,
            old_id,
            new_id,
            via,
            clamp_notice,
        });
        self.emit_conversation();
    }

    /// `runtime.subagent_registry().display_rows()` → `SubagentRows`
    /// (called at Done/Error and from the 1 Hz arm B adds).
    #[allow(dead_code)]
    pub(crate) fn emit_subagent_rows(&mut self) {
        let rows = self
            .runtime
            .subagent_registry()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .display_rows();
        self.emit(SessionEventWire::SubagentRows(rows));
    }

    /// Any non-terminal subagent row (drives the 1 Hz arm).
    #[allow(dead_code)]
    pub(crate) fn has_live_subagents(&self) -> bool {
        self.runtime
            .subagent_registry()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .display_rows()
            .iter()
            .any(|r| matches!(r.status, crate::runtime::subagent::SubagentStatus::Running))
    }
}

/// Of the workers that had no cancel requested before a `/workers cancel`,
/// those that have one now. A worker that left the registry meanwhile
/// (reaped at a turn end that raced the command) was not cancelled by it.
fn cancelled_by_command(
    before: Vec<(String, String)>,
    after: &[crate::runtime::subagent::SubagentDisplayRow],
) -> Vec<(String, String)> {
    before
        .into_iter()
        .filter(|(id, _)| after.iter().any(|r| &r.handle_id == id && r.cancel_requested))
        .collect()
}

#[cfg(test)]
mod tests {
    //! `/resume`'s reasoning restore is the runtime's `restore_session_reasoning`
    //! + the clamp line above; pin the two behaviours the TUI tests pinned.
    use crate::Runtime;

    fn row(id: &str, cancel_requested: bool) -> crate::runtime::subagent::SubagentDisplayRow {
        serde_json::from_value(serde_json::json!({
            "subagent_id": 0, "agent_name": "a", "status": "running",
            "cancel_requested": cancel_requested, "elapsed_secs": 0.0,
            "finished_elapsed": null, "handle_id": id,
        }))
        .unwrap()
    }

    /// `/workers cancel` tells the main agent only about workers the command
    /// flagged: one reaped from the registry while it ran (it completed; its
    /// wake is the truth) is not "cancelled by the user".
    #[test]
    fn the_cancel_note_names_only_workers_the_command_flagged() {
        let before: Vec<(String, String)> = ["sa_1", "sa_2", "sa_3"]
            .iter()
            .map(|id| (id.to_string(), "a".to_string()))
            .collect();
        // sa_1 flagged; sa_2 reaped meanwhile; sa_3 untouched.
        let after = [row("sa_1", true), row("sa_3", false)];
        let named: Vec<String> =
            super::cancelled_by_command(before, &after).into_iter().map(|(id, _)| id).collect();
        assert_eq!(named, ["sa_1"]);
    }

    #[tokio::test]
    async fn restore_session_reasoning_keeps_explicit_provenance() {
        let mut runtime = Runtime::new().await.unwrap();
        runtime.set_model("openai-codex/gpt-5.6-sol".to_string());
        let clamp = runtime.restore_session_reasoning("ultra");
        assert!(clamp.is_none());
        assert_eq!(
            runtime.reasoning_level(),
            agent_core::reasoning::ReasoningLevel::Ultra
        );
        assert!(runtime.is_reasoning_explicit());
        runtime.set_model("openai-codex/gpt-5.6-terra".to_string());
        assert_eq!(
            runtime.reasoning_level(),
            agent_core::reasoning::ReasoningLevel::Ultra
        );
    }

    #[tokio::test]
    async fn restore_session_reasoning_clamps_unsupported_level() {
        let mut runtime = Runtime::new().await.unwrap();
        runtime.set_model("xai-auth/grok-4.6".to_string());
        let clamp = runtime.restore_session_reasoning("xhigh");
        assert!(clamp.is_some());
        assert_eq!(
            runtime.reasoning_level(),
            agent_core::reasoning::ReasoningLevel::High
        );
        assert!(runtime.is_reasoning_explicit());
    }
}

#[cfg(test)]
mod auth_command_tests {
    use super::auth_command_reply;

    #[tokio::test]
    async fn auth_command_usage_and_token_free_error() {
        let rt = crate::Runtime::new_headless(); // api_key "test-token"
        let v = auth_command_reply("bogus", &rt).await;
        assert_eq!(v["kind"], "error");
        assert!(v["text"].as_str().unwrap().contains("usage: /auth reload"));
        let v = auth_command_reply("  reload ", &rt).await;
        let t = v["text"].as_str().unwrap();
        assert_eq!(v["kind"], "error");
        assert!(t.contains("nothing to reload"), "{t}");
        assert!(!t.contains("test-token"), "{t}");
    }

    #[test]
    fn auth_is_a_builtin_command() {
        assert!(crate::skills::BUILTIN_COMMANDS.contains(&"auth"));
    }
}
