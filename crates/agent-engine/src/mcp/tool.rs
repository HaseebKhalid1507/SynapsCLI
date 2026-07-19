//! McpTool — bridges an MCP server tool into the native Tool trait.
use crate::{Result, RuntimeError, Tool, ToolContext};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::connection::McpConnection;

/// A dynamic tool backed by an MCP server connection.
/// The connection is shared (Arc<Mutex<>>) across all tools from the same server.
pub struct McpTool {
    pub(super) tool_name: String,
    /// Original tool name as the MCP server knows it (without prefix).
    pub(super) server_tool_name: String,
    pub(super) server_name: String,
    pub(super) description: String,
    pub(super) input_schema: Value,
    pub(super) connection: Arc<Mutex<McpConnection>>,
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Mcp {
            server_id: self.server_name.clone(),
            server_tool_name: self.server_tool_name.clone(),
        }
    }

    fn name(&self) -> &str {
        &self.tool_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.input_schema.clone()
    }

    async fn execute(&self, params: Value, _ctx: ToolContext) -> Result<String> {
        let mut conn = self.connection.lock().await;
        conn.call_tool(&self.server_tool_name, params)
            .await
            .map_err(|e| {
                RuntimeError::Tool(format!(
                    "MCP tool '{}' (server '{}') failed: {}",
                    self.tool_name, self.server_name, e
                ))
            })
    }
}
