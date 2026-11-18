use super::{Tool, ToolContext, ToolOrigin};
use crate::runtime::continuation::SharedContinuation;
use agent_core::core::context_policy::WorkPhase;
use serde_json::{json, Value};

/// One instance serves every chat on the host: it is registered once, when
/// the registry is built, and holds nothing. The rollover state it writes
/// to is the calling chat's, taken from the call's own context.
pub struct ContextCheckpointTool {}

/// The constructor two tests in `runtime::continuation` still call. The
/// state it is given is not kept. Remove with those two call sites.
#[cfg(test)]
#[allow(non_snake_case)]
pub(crate) fn ContextCheckpointTool(_state: SharedContinuation) -> ContextCheckpointTool {
    ContextCheckpointTool {}
}

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
        let phase = params["phase"]
            .as_str()
            .and_then(WorkPhase::parse)
            .ok_or_else(|| crate::RuntimeError::Tool("invalid context phase".into()))?;
        let state: SharedContinuation = ctx.capabilities.continuation.ok_or_else(|| {
            crate::RuntimeError::Tool("context management is not available in this context".into())
        })?;
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
