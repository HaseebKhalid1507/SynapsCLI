//! RespondTool — reply to an event through its original source channel.
use serde_json::{json, Value};
use crate::Result;
use super::{Tool, ToolContext};

pub struct RespondTool;

#[async_trait::async_trait]
impl Tool for RespondTool {
    fn name(&self) -> &str { "respond" }

    fn description(&self) -> &str {
        "Reply to an event through its original source channel. Sends the response text back via the event's callback URL or logs it if no callback is available."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "event_id": { "type": "string", "description": "ID of the event to respond to" },
                "text":     { "type": "string", "description": "Response message" }
            },
            "required": ["event_id", "text"]
        })
    }

    async fn execute(&self, params: Value, _ctx: ToolContext) -> Result<String> {
        let event_id = params["event_id"].as_str()
            .ok_or_else(|| crate::RuntimeError::Tool("Missing 'event_id' parameter".to_string()))?
            .to_string();
        let text = params["text"].as_str()
            .ok_or_else(|| crate::RuntimeError::Tool("Missing 'text' parameter".to_string()))?
            .to_string();

        tracing::info!(event_id = %event_id, "respond tool invoked: {}", text);

        // NOT YET IMPLEMENTED — return clear failure so the model knows
        Ok(json!({
            "responded": false,
            "error": "respond tool is not yet implemented — callback dispatch not wired",
            "event_id": event_id,
        }).to_string())
    }
}
