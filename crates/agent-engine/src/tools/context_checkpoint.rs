use super::{Tool, ToolContext, ToolOrigin};
use crate::runtime::continuation::SharedContinuation;
use agent_core::core::context_policy::WorkPhase;
use serde_json::{json, Value};

pub struct ContextCheckpointTool(pub SharedContinuation);
#[async_trait::async_trait]
impl Tool for ContextCheckpointTool {
    fn name(&self) -> &str {
        "context_checkpoint"
    }
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }
    fn description(&self) -> &str {
        "Report task phase and a bounded working note for automatic context management. Call alone before executing a completed plan or starting a large new task. Host may roll over before the next request. Does not grant permissions or forget history."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","additionalProperties":false,"properties":{
        "phase":{"type":"string","enum":["plan","execute","wrap_up","new_task"]},
        "note":{"type":"string","maxLength":8192,"description":"Requirements, decisions, failed approaches, next actions and source references; no secrets."}
    },"required":["phase"]})
    }
    async fn execute(&self, params: Value, _ctx: ToolContext) -> crate::Result<String> {
        let phase = params["phase"]
            .as_str()
            .and_then(WorkPhase::parse)
            .ok_or_else(|| crate::RuntimeError::Tool("invalid context phase".into()))?;
        let mut s = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        s.checkpoint(phase, params["note"].as_str())?;
        Ok(format!(
            "phase={}; context policy will assess before next request",
            phase.as_str()
        ))
    }
}
