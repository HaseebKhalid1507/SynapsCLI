//! `SessionActor` — command bodies that belong to the TUI port (A3):
//! `engine_command` (moved verbatim from `actor.rs`), and — as A3 lands —
//! `submit_prepared`, `plugin_command`, `resume`, `emit_subagent_rows`.
//! Kept apart from `actor.rs` so the turn machine (B) and the command
//! surface (A) never touch the same file.

use super::actor::SessionActor;
use super::types::*;

impl SessionActor {
    /// `engine::commands::handle_engine_command` on THE runtime; reply as a
    /// `QueryResult` the client renders (chat.rs slash-command branch).
    pub(crate) async fn engine_command(&mut self, id: u64, cmd: String, arg: String) {
        use crate::engine::commands::{handle_engine_command, CommandResult};
        let value = match handle_engine_command(&cmd, &arg, &mut self.runtime) {
            None => serde_json::json!({ "kind": "unhandled" }),
            Some(CommandResult::Quit) => serde_json::json!({ "kind": "quit" }),
            Some(CommandResult::ModelChanged {
                model,
                reasoning_clamped,
            }) => {
                self.conv.session.model = self.runtime.model().to_string();
                let mut text = format!("model → {}", model);
                if let Some(clamp) = reasoning_clamped {
                    self.conv.session.thinking_level = self.runtime.thinking_level().to_string();
                    text.push_str(&format!(
                        "\nthinking → {} (clamped from {}: not supported by {})",
                        clamp.to.as_str(),
                        clamp.from.as_str(),
                        self.runtime.model()
                    ));
                }
                self.publish_view().await;
                serde_json::json!({ "kind": "notice", "text": text })
            }
            Some(CommandResult::ThinkingChanged { spec }) => {
                self.conv.session.thinking_level = spec.config_value();
                self.publish_view().await;
                serde_json::json!({ "kind": "notice", "text": format!("thinking → {}", spec.level()) })
            }
            Some(CommandResult::Compact {
                custom_instructions,
            }) => {
                self.compact(custom_instructions, "manual").await;
                serde_json::json!({ "kind": "none" })
            }
            Some(CommandResult::Error(e)) => serde_json::json!({ "kind": "error", "text": e }),
            Some(CommandResult::Output(text)) => {
                serde_json::json!({ "kind": "output", "text": text })
            }
            Some(_) => serde_json::json!({ "kind": "none" }),
        };
        self.emit(SessionEventWire::QueryResult { id, value });
    }
}
