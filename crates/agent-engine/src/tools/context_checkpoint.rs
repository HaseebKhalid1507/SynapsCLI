use super::{Tool, ToolContext, ToolOrigin};
use agent_core::core::context_policy::WorkPhase;
use serde_json::{json, Value};

/// Stateless: a call records into the calling session's own continuation
/// state, carried by its [`ToolContext`] (`capabilities.continuation`). One
/// instance may sit in a registry shared by every session of a host, so it
/// must never hold session state itself.
pub struct ContextCheckpointTool;
#[async_trait::async_trait]
impl Tool for ContextCheckpointTool {
    fn name(&self) -> &str {
        "context_checkpoint"
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    fn description(&self) -> &str {
        "Report a substantial task boundary (plan, execute, wrap_up, new_task) to automatic context management. Call alone at such a boundary or when requested by the host, not for routine progress logging. Note optional; no summary required. Host checks capacity automatically. Does not grant permissions or forget history."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","additionalProperties":false,"properties":{
        "phase":{"type":"string","enum":["plan","execute","wrap_up","new_task"]},
        "note":{"type":"string","maxLength":8192,"description":"Optional few lines of unfinished state, next action and essential source references. Do not repeat the transcript, settled requirements or completed work; no secrets."}
    },"required":["phase"]})
    }
    async fn execute(&self, params: Value, ctx: ToolContext) -> crate::Result<String> {
        let state = ctx.capabilities.continuation.ok_or_else(|| {
            crate::RuntimeError::Tool("context management is not available here".into())
        })?;
        let phase = params["phase"]
            .as_str()
            .and_then(WorkPhase::parse)
            .ok_or_else(|| crate::RuntimeError::Tool("invalid context phase".into()))?;
        let mut s = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        s.checkpoint(phase, params["note"].as_str())?;
        Ok(format!(
            "phase={} recorded; continue authorized work. The host checks capacity automatically.",
            phase.as_str()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::continuation::{ContinuationState, SharedContinuation};
    use std::sync::{Arc, Mutex};

    fn auto_state() -> SharedContinuation {
        let mut state = ContinuationState::default();
        state.config.mode = agent_core::config::ContextManagementMode::Auto;
        Arc::new(Mutex::new(state))
    }

    fn context(state: Option<SharedContinuation>) -> ToolContext {
        let mut ctx = crate::tools::test_helpers::create_tool_context();
        ctx.capabilities.continuation = state;
        ctx
    }

    #[tokio::test]
    async fn without_a_session_state_the_call_fails_typed_and_records_nothing() {
        // A state that exists elsewhere in the process must stay untouched:
        // there is no fallback to any other session.
        let elsewhere = auto_state();
        let err = ContextCheckpointTool
            .execute(json!({"phase":"plan","note":"STRAY"}), context(None))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, crate::RuntimeError::Tool(m) if m == "context management is not available here"),
            "{err:?}"
        );
        let s = elsewhere.lock().unwrap();
        assert!(s.note.is_empty());
        assert_eq!(s.policy.phase(), WorkPhase::Unknown);
    }

    #[tokio::test]
    async fn a_call_records_into_the_state_its_context_carries() {
        let mine = auto_state();
        let other = auto_state();
        let out = ContextCheckpointTool
            .execute(
                json!({"phase":"wrap_up","note":"MINE"}),
                context(Some(Arc::clone(&mine))),
            )
            .await
            .unwrap();
        assert!(out.starts_with("phase=wrap_up recorded"), "{out}");
        assert_eq!(mine.lock().unwrap().note, "MINE");
        assert_eq!(mine.lock().unwrap().policy.phase(), WorkPhase::WrapUp);
        assert!(other.lock().unwrap().note.is_empty());
        assert_eq!(other.lock().unwrap().policy.phase(), WorkPhase::Unknown);
    }
}
