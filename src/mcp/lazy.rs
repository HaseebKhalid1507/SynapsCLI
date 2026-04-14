use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use crate::Tool;

use super::McpServerConfig;
use super::connection::McpConnection;
use super::tool::McpTool;

/// A tool that lazily connects to MCP servers on demand.
/// Instead of spawning all servers at startup (burning tokens on 65 tool schemas),
/// this registers ONE tool that the model calls to activate a specific server.
///
/// MCP connect gateway — discovers tools from an MCP server and registers them dynamically.
/// Uses the ToolContext.tool_register_tx channel instead of holding a direct Arc to the registry,
/// breaking the circular reference that previously existed.
pub struct McpConnectTool {
    configs: HashMap<String, McpServerConfig>,
    connected: Arc<Mutex<std::collections::HashSet<String>>>,
}

impl McpConnectTool {
    pub fn new(
        configs: HashMap<String, McpServerConfig>,
    ) -> Self {
        Self {
            configs,
            connected: Arc::new(Mutex::new(std::collections::HashSet::new())),
        }
    }
}

#[async_trait::async_trait]
impl Tool for McpConnectTool {
    fn name(&self) -> &str { "mcp_connect" }

    fn description(&self) -> &str {
        "Connect to an MCP server and load its tools. Call this before using tools from an external MCP server. Available servers are listed in the description below. Once connected, the server's tools become available for the rest of the session."
    }

    fn parameters(&self) -> Value {
        let server_names: Vec<&str> = self.configs.keys().map(|s| s.as_str()).collect();
        let server_list = server_names.join(", ");
        json!({
            "type": "object",
            "properties": {
                "server": {
                    "type": "string",
                    "description": format!("Name of the MCP server to connect to. Available: {}", server_list)
                }
            },
            "required": ["server"]
        })
    }

    async fn execute(&self, params: Value, ctx: crate::ToolContext) -> crate::Result<String> {
        let server_name = params["server"].as_str()
            .ok_or_else(|| crate::RuntimeError::Tool("Missing 'server' parameter".to_string()))?;

        // Atomically check-and-mark to prevent double-connect from parallel calls
        {
            let mut connected = self.connected.lock().await;
            if connected.contains(server_name) {
                return Ok(format!("Server '{}' is already connected.", server_name));
            }
            // Mark now — if connection fails, we'll unmark
            connected.insert(server_name.to_string());
        }

        let config = self.configs.get(server_name)
            .ok_or_else(|| {
                let available: Vec<&str> = self.configs.keys().map(|s| s.as_str()).collect();
                crate::RuntimeError::Tool(format!(
                    "Unknown MCP server '{}'. Available: {}", server_name, available.join(", ")
                ))
            })?;

        tracing::info!(server = %server_name, "Lazy-connecting to MCP server");

        let mut conn = match McpConnection::start(config).await {
            Ok(c) => c,
            Err(e) => {
                // Unmark on failure so retry is possible
                self.connected.lock().await.remove(server_name);
                return Err(crate::RuntimeError::Tool(format!(
                    "Failed to connect to MCP server '{}': {}", server_name, e
                )));
            }
        };

        let tools = match conn.list_tools().await {
            Ok(t) => t,
            Err(e) => {
                self.connected.lock().await.remove(server_name);
                return Err(crate::RuntimeError::Tool(format!(
                    "Failed to list tools from '{}': {}", server_name, e
                )));
            }
        };

        let tool_count = tools.len();
        let connection = Arc::new(Mutex::new(conn));
        let mut tool_names = Vec::new();
        let mut new_tools: Vec<Arc<dyn crate::Tool>> = Vec::new();

        for tool_def in tools {
            let prefixed_name = format!("mcp__{}__{}", server_name, tool_def.name);
            tool_names.push(format!("{} — {}", tool_def.name,
                tool_def.description.chars().take(60).collect::<String>()));

            let mcp_tool = McpTool {
                tool_name: prefixed_name,
                server_tool_name: tool_def.name.clone(),
                server_name: server_name.to_string(),
                description: format!("[MCP:{}] {}", server_name, tool_def.description),
                input_schema: tool_def.input_schema,
                connection: Arc::clone(&connection),
            };

            new_tools.push(Arc::new(mcp_tool));
        }

        // Send new tools to the runtime for registration (via channel, no circular Arc)
        if let Some(ref tx) = ctx.tool_register_tx {
            let _ = tx.send(new_tools);
        }

        tracing::info!(server = %server_name, tools = tool_count, "MCP server connected (lazy)");

        let tool_list = tool_names.join("\n  • ");
        Ok(format!(
            "Connected to '{}' — {} tools now available:\n  • {}",
            server_name, tool_count, tool_list
        ))
    }
}
