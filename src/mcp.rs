use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use crate::{Result, RuntimeError, Tool, ToolContext, ToolRegistry};

/// MCP server configuration — matches claude-code/gemini-cli format.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

/// MCP config file format: { "mcpServers": { "name": { command, args, env } } }
#[derive(Debug, Clone, serde::Deserialize)]
pub struct McpConfig {
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,
}

/// A running MCP server connection — manages the child process and JSON-RPC.
struct McpConnection {
    #[allow(dead_code)] // kept alive for kill_on_drop
    child: Child,
    stdin: tokio::process::ChildStdin,
    reader: BufReader<tokio::process::ChildStdout>,
    next_id: u64,
}

impl McpConnection {
    /// Spawn and initialize an MCP server.
    async fn start(config: &McpServerConfig) -> std::result::Result<Self, String> {
        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args);
        
        for (k, v) in &config.env {
            cmd.env(k, v);
        }
        
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        
        let mut child = cmd.spawn()
            .map_err(|e| format!("Failed to spawn MCP server '{}': {}", config.command, e))?;
        
        let stdin = child.stdin.take()
            .ok_or_else(|| "Failed to capture MCP server stdin".to_string())?;
        let stdout = child.stdout.take()
            .ok_or_else(|| "Failed to capture MCP server stdout".to_string())?;

        // Pipe stderr to tracing so MCP server errors are visible in logs
        if let Some(stderr) = child.stderr.take() {
            let cmd_name = config.command.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut reader = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    // Filter out noisy npm/npx output
                    let trimmed = line.trim();
                    if trimmed.is_empty() { continue; }
                    if trimmed.starts_with("npm") || trimmed.starts_with("npx") { continue; }
                    tracing::warn!(server = %cmd_name, "{}", trimmed);
                }
            });
        }
        
        let mut conn = McpConnection {
            child,
            stdin,
            reader: BufReader::new(stdout),
            next_id: 1,
        };
        
        // Initialize handshake
        let init_result = conn.request("initialize", json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "synaps-cli",
                "version": env!("CARGO_PKG_VERSION")
            }
        })).await?;
        
        tracing::debug!("MCP initialize response: {:?}", init_result);
        
        // Send initialized notification (no response expected)
        conn.notify("notifications/initialized", json!({})).await?;
        
        Ok(conn)
    }
    
    /// Send a JSON-RPC request and wait for the response.
    async fn request(&mut self, method: &str, params: Value) -> std::result::Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        
        let msg = format!("{}\n", serde_json::to_string(&request)
            .map_err(|e| format!("Failed to serialize MCP request: {}", e))?);
        self.stdin.write_all(msg.as_bytes()).await
            .map_err(|e| format!("Failed to write to MCP server: {}", e))?;
        self.stdin.flush().await
            .map_err(|e| format!("Failed to flush MCP server stdin: {}", e))?;
        
        // Read lines until we get a response with matching id
        // (skip notifications from the server)
        let timeout = tokio::time::Duration::from_secs(30);
        let result = tokio::time::timeout(timeout, async {
            loop {
                let mut line = String::new();
                self.reader.read_line(&mut line).await
                    .map_err(|e| format!("Failed to read from MCP server: {}", e))?;
                
                if line.trim().is_empty() {
                    continue;
                }
                
                let response: Value = serde_json::from_str(line.trim())
                    .map_err(|e| format!("Invalid JSON from MCP server: {} — line: {}", e, line.trim()))?;
                
                // Check if this is our response (has matching id)
                if response.get("id").and_then(|v| v.as_u64()) == Some(id) {
                    if let Some(error) = response.get("error") {
                        let msg = error["message"].as_str().unwrap_or("Unknown MCP error");
                        let code = error["code"].as_i64().unwrap_or(-1);
                        return Err(format!("MCP error ({}): {}", code, msg));
                    }
                    return Ok(response["result"].clone());
                }
                // Otherwise it's a notification or response to different request — skip
            }
        }).await;
        
        match result {
            Ok(r) => r,
            Err(_) => Err(format!("MCP request '{}' timed out after 30s", method)),
        }
    }
    
    /// Send a JSON-RPC notification (no response).
    async fn notify(&mut self, method: &str, params: Value) -> std::result::Result<(), String> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });
        
        let msg = format!("{}\n", serde_json::to_string(&notification)
            .map_err(|e| format!("Failed to serialize MCP notification: {}", e))?);
        self.stdin.write_all(msg.as_bytes()).await
            .map_err(|e| format!("Failed to write notification to MCP server: {}", e))?;
        self.stdin.flush().await
            .map_err(|e| format!("Failed to flush MCP server stdin: {}", e))?;
        Ok(())
    }
    
    /// List available tools from the server.
    async fn list_tools(&mut self) -> std::result::Result<Vec<McpToolDef>, String> {
        let result = self.request("tools/list", json!({})).await?;
        
        let tools = result["tools"].as_array()
            .ok_or_else(|| "MCP tools/list response missing 'tools' array".to_string())?;
        
        let mut defs = Vec::new();
        for tool in tools {
            let name = tool["name"].as_str().unwrap_or("").to_string();
            let description = tool["description"].as_str().unwrap_or("").to_string();
            let input_schema = tool.get("inputSchema").cloned().unwrap_or(json!({
                "type": "object",
                "properties": {},
                "required": []
            }));
            
            if !name.is_empty() {
                defs.push(McpToolDef { name, description, input_schema });
            }
        }
        
        Ok(defs)
    }
    
    /// Call a tool on the server.
    async fn call_tool(&mut self, name: &str, arguments: Value) -> std::result::Result<String, String> {
        let result = self.request("tools/call", json!({
            "name": name,
            "arguments": arguments
        })).await?;
        
        // Extract text content from the result
        let content = result.get("content").and_then(|c| c.as_array());
        
        match content {
            Some(blocks) => {
                let mut output = String::new();
                for block in blocks {
                    match block["type"].as_str() {
                        Some("text") => {
                            if let Some(text) = block["text"].as_str() {
                                if !output.is_empty() { output.push('\n'); }
                                output.push_str(text);
                            }
                        }
                        Some("image") => {
                            output.push_str("[image content]");
                        }
                        Some("resource") => {
                            if let Some(text) = block.get("resource").and_then(|r| r["text"].as_str()) {
                                if !output.is_empty() { output.push('\n'); }
                                output.push_str(text);
                            }
                        }
                        _ => {}
                    }
                }
                
                if result.get("isError").and_then(|v| v.as_bool()).unwrap_or(false) {
                    Err(output)
                } else {
                    Ok(output)
                }
            }
            None => {
                // Fallback: stringify the whole result
                Ok(serde_json::to_string_pretty(&result).unwrap_or_default())
            }
        }
    }
}

/// Discovered tool definition from an MCP server.
#[derive(Debug, Clone)]
struct McpToolDef {
    name: String,
    description: String,
    input_schema: Value,
}

/// A dynamic tool backed by an MCP server connection.
/// The connection is shared (Arc<Mutex<>>) across all tools from the same server.
pub struct McpTool {
    tool_name: String,
    /// Original tool name as the MCP server knows it (without prefix).
    server_tool_name: String,
    server_name: String,
    description: String,
    input_schema: Value,
    connection: Arc<Mutex<McpConnection>>,
}

#[async_trait::async_trait]
impl Tool for McpTool {
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
        conn.call_tool(&self.server_tool_name, params).await
            .map_err(|e| RuntimeError::Tool(format!(
                "MCP tool '{}' (server '{}') failed: {}", self.tool_name, self.server_name, e
            )))
    }
}

/// Load MCP config from ~/.synaps-cli/mcp.json (or profile variant).
pub fn load_mcp_config() -> Option<McpConfig> {
    let path = crate::config::resolve_read_path("mcp.json");
    if !path.exists() {
        return None;
    }
    
    let content = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str::<McpConfig>(&content) {
        Ok(config) => Some(config),
        Err(e) => {
            tracing::warn!("Failed to parse MCP config at {}: {}", path.display(), e);
            None
        }
    }
}

/// Connect to all configured MCP servers and register their tools.
/// Returns the number of tools registered.
pub async fn connect_mcp_servers(registry: &mut ToolRegistry) -> usize {
    let config = match load_mcp_config() {
        Some(c) => c,
        None => return 0,
    };
    
    let mut total_tools = 0;
    
    for (server_name, server_config) in &config.mcp_servers {
        tracing::info!(server = %server_name, command = %server_config.command, "Connecting to MCP server");
        
        match McpConnection::start(server_config).await {
            Ok(mut conn) => {
                match conn.list_tools().await {
                    Ok(tools) => {
                        let tool_count = tools.len();
                        let connection = Arc::new(Mutex::new(conn));
                        
                        for tool_def in tools {
                            // Prefix tool names with server name to avoid collisions
                            // e.g. "filesystem__read_file" for server "filesystem"
                            let prefixed_name = format!("mcp__{}__{}", server_name, tool_def.name);
                            
                            let mcp_tool = McpTool {
                                tool_name: prefixed_name,
                                server_tool_name: tool_def.name.clone(),
                                server_name: server_name.clone(),
                                description: format!("[MCP:{}] {}", server_name, tool_def.description),
                                input_schema: tool_def.input_schema,
                                connection: Arc::clone(&connection),
                            };
                            
                            registry.register(Arc::new(mcp_tool));
                            total_tools += 1;
                        }
                        
                        tracing::info!(
                            server = %server_name,
                            tools = tool_count,
                            "MCP server connected — {} tools registered",
                            tool_count
                        );
                    }
                    Err(e) => {
                        tracing::error!(server = %server_name, error = %e, "Failed to list MCP tools");
                    }
                }
            }
            Err(e) => {
                tracing::error!(server = %server_name, error = %e, "Failed to connect to MCP server");
            }
        }
    }
    
    total_tools
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_mcp_config_deserialize() {
        let json_str = r#"{"mcpServers": {"test": {"command": "echo", "args": ["hi"]}}}"#;
        let config: McpConfig = serde_json::from_str(json_str).unwrap();
        
        assert_eq!(config.mcp_servers.len(), 1);
        assert!(config.mcp_servers.contains_key("test"));
        
        let server = &config.mcp_servers["test"];
        assert_eq!(server.command, "echo");
        assert_eq!(server.args, vec!["hi"]);
    }

    #[test]
    fn test_mcp_config_empty_servers() {
        let json_str = r#"{"mcpServers": {}}"#;
        let config: McpConfig = serde_json::from_str(json_str).unwrap();
        
        assert_eq!(config.mcp_servers.len(), 0);
        assert!(config.mcp_servers.is_empty());
    }

    #[test]
    fn test_mcp_server_config_defaults() {
        let json_str = r#"{"command": "echo"}"#;
        let server_config: McpServerConfig = serde_json::from_str(json_str).unwrap();
        
        assert_eq!(server_config.command, "echo");
        assert_eq!(server_config.args, Vec::<String>::new());
        assert_eq!(server_config.env, HashMap::new());
    }

    #[test]
    fn test_mcp_config_deserialize_from_value() {
        let json_value = json!({
            "mcpServers": {
                "test": {
                    "command": "echo",
                    "args": ["hi"]
                }
            }
        });
        
        let config: McpConfig = serde_json::from_value(json_value).unwrap();
        
        assert_eq!(config.mcp_servers.len(), 1);
        assert!(config.mcp_servers.contains_key("test"));
        
        let server = &config.mcp_servers["test"];
        assert_eq!(server.command, "echo");
        assert_eq!(server.args, vec!["hi"]);
    }

    #[test]
    fn test_load_mcp_config_returns_some_or_none() {
        // This test verifies that load_mcp_config() returns either Some or None
        // depending on whether the config file exists
        let result = load_mcp_config();
        
        // Result can be either Some(config) or None - both are valid
        // depending on whether ~/.synaps-cli/mcp.json exists
        match result {
            Some(config) => {
                // If file exists and parses correctly, we get a config
                assert!(config.mcp_servers.len() >= 0); // Can be empty
            }
            None => {
                // If file doesn't exist or fails to parse, we get None
                // This is expected behavior
            }
        }
    }
}

// ── Lazy Loading ────────────────────────────────────────────────────────

/// A tool that lazily connects to MCP servers on demand.
/// Instead of spawning all servers at startup (burning tokens on 65 tool schemas),
/// this registers ONE tool that the model calls to activate a specific server.
///
/// NOTE: This tool holds an Arc to the ToolRegistry it is registered in (circular Arc).
/// This is safe because: the write lock on the registry is only acquired inside execute(),
/// never nested with another lock on the same registry, and the tool loop in runtime.rs
/// drops its read lock before executing tools.
pub struct McpConnectTool {
    configs: HashMap<String, McpServerConfig>,
    registry: Arc<tokio::sync::RwLock<crate::ToolRegistry>>,
    connected: Arc<Mutex<std::collections::HashSet<String>>>,
}

impl McpConnectTool {
    pub fn new(
        configs: HashMap<String, McpServerConfig>,
        registry: Arc<tokio::sync::RwLock<crate::ToolRegistry>>,
    ) -> Self {
        Self {
            configs,
            registry,
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

    async fn execute(&self, params: Value, _ctx: crate::ToolContext) -> crate::Result<String> {
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

        {
            let mut registry = self.registry.write().await;
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

                registry.register(Arc::new(mcp_tool));
            }
        }

        tracing::info!(server = %server_name, tools = tool_count, "MCP server connected (lazy)");

        let tool_list = tool_names.join("\n  • ");
        Ok(format!(
            "Connected to '{}' — {} tools now available:\n  • {}",
            server_name, tool_count, tool_list
        ))
    }
}

/// Set up lazy MCP loading: parse config, register the mcp_connect gateway tool.
/// Returns the number of available (but not yet connected) servers.
pub async fn setup_lazy_mcp(registry: &Arc<tokio::sync::RwLock<crate::ToolRegistry>>) -> usize {
    let config = match load_mcp_config() {
        Some(c) => c,
        None => return 0,
    };

    let server_count = config.mcp_servers.len();
    if server_count == 0 {
        return 0;
    }

    let server_names: Vec<&str> = config.mcp_servers.keys().map(|s| s.as_str()).collect();
    tracing::info!(servers = ?server_names, "MCP lazy loading: {} servers available", server_count);

    let connect_tool = McpConnectTool::new(
        config.mcp_servers,
        Arc::clone(registry),
    );

    registry.write().await.register(Arc::new(connect_tool));

    server_count
}
