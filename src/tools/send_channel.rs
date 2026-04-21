//! SendChannelTool — proactively send a message to a specific channel.
use serde_json::{json, Value};
use crate::Result;
use super::{Tool, ToolContext};

pub struct SendChannelTool;

#[async_trait::async_trait]
impl Tool for SendChannelTool {
    fn name(&self) -> &str { "send_channel" }

    fn description(&self) -> &str {
        "Send a message to a specific channel (discord, slack, system, etc). Use this to proactively notify through a specific channel rather than replying to an event."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "channel_type": {
                    "type": "string",
                    "description": "Channel type: discord, slack, telegram, system, desktop"
                },
                "channel_id": {
                    "type": "string",
                    "description": "Channel identifier (e.g. Discord channel ID, Slack channel)"
                },
                "text": { "type": "string", "description": "Message to send" }
            },
            "required": ["channel_type", "channel_id", "text"]
        })
    }

    async fn execute(&self, params: Value, _ctx: ToolContext) -> Result<String> {
        let channel_type = params["channel_type"].as_str().unwrap_or("").to_string();
        let channel_id = params["channel_id"].as_str().unwrap_or("").to_string();
        let text = params["text"].as_str().unwrap_or("").to_string();

        tracing::info!(
            channel_type = %channel_type,
            channel_id = %channel_id,
            "send_channel tool invoked: {}",
            text
        );

        Ok(json!({
            "sent": false,
            "error": "send_channel is not yet implemented — no channel dispatch wired",
            "channel_type": channel_type,
            "channel_id": channel_id,
        }).to_string())
    }
}
