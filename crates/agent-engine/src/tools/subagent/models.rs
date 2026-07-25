//! Read-only session worker catalog operation.
use super::super::{Tool, ToolContext};
use crate::{Result, RuntimeError};
use serde_json::{json, Value};

pub struct SubagentModelsTool;

#[async_trait::async_trait]
impl Tool for SubagentModelsTool {
    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Builtin
    }

    fn name(&self) -> &str {
        "subagent_models"
    }
    fn description(&self) -> &str {
        "List this session's exact trusted worker model choices. Omit model on delegation to inherit the foreground identity."
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    async fn execute(&self, _params: Value, ctx: ToolContext) -> Result<String> {
        let policy = ctx
            .capabilities
            .orchestration
            .as_ref()
            .ok_or_else(|| RuntimeError::Tool("delegation policy unavailable".into()))?;
        Ok(json!({
            "foreground_model": policy.foreground_model(),
            "models": policy.effective_choices(),
            "model_omission": "inherit_foreground"
        })
        .to_string())
    }
}
