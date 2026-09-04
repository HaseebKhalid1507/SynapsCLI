//! `SessionActor` — command bodies that belong to the TUI port (A3):
//! `engine_command` (structured reply), `submit_prepared`, `plugin_command`,
//! `resume`, `session_name`, `emit_subagent_rows`. Kept apart from
//! `actor.rs` so the turn machine (B) and the command surface (A) never
//! touch the same file.

use std::sync::Arc;

use super::actor::SessionActor;
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
        let (value, view_changed) =
            engine_command_reply(&cmd, &arg, &mut self.runtime, &mut self.conv.session);
        if view_changed {
            self.publish_view().await;
        }
        if let Some(CommandResult::Compact {
            custom_instructions,
        }) = compact_request(&cmd, &arg)
        {
            self.emit(SessionEventWire::SystemNotice("compacting...".into()));
            self.compact(custom_instructions, "manual").await;
        }
        self.emit(SessionEventWire::QueryResult { id, value });
    }

    /// `/saveas <name>` | `/saveas` (clear). Force-saves even with no
    /// messages — persist the name change (commands.rs `saveas` arm).
    async fn session_name(&mut self, arg: &str) -> serde_json::Value {
        let trimmed = arg.trim();
        if trimmed.is_empty() {
            self.conv.session.clear_name();
            let _ = self.conv.session.save().await;
            self.emit_conversation();
            return serde_json::json!({
                "kind": "output", "text": "session name cleared", "name": serde_json::Value::Null,
            });
        }
        match self.conv.session.set_name(trimmed) {
            Ok(()) => {
                let _ = self.conv.session.save().await;
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
    /// (+ optional user text) then a turn. Does NOT fold `abort_context` and
    /// does NOT reset `consecutive_auto_turns`.
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
        let old_id = self.conv.session.id.clone();
        let new_id = session.id.clone();
        let via = if crate::chain::load_chain(&query).is_ok() {
            Some(format!("chain '{}'", query))
        } else if crate::find_session_by_name(&query).is_ok() {
            Some(format!("name '{}'", query))
        } else {
            None
        };
        self.conv = crate::engine::session::ConversationState::from_resumed(session);
        if clamp_notice.is_some() {
            // Keep the session file in sync with the clamped runtime.
            self.conv.session.thinking_level = self.runtime.thinking_level().to_string();
        }
        self.consecutive_auto_turns = 0;
        self.runtime.set_session_id(Some(new_id.clone()));
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

#[cfg(test)]
mod tests {
    //! `/resume`'s reasoning restore is the runtime's `restore_session_reasoning`
    //! + the clamp line above; pin the two behaviours the TUI tests pinned.
    use crate::Runtime;

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
