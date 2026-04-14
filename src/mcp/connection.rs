use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use super::McpToolDef;

/// A running MCP server connection — manages the child process and JSON-RPC.
pub(super) struct McpConnection {
    #[allow(dead_code)] // kept alive for kill_on_drop
    child: Child,
    stdin: tokio::process::ChildStdin,
    reader: BufReader<tokio::process::ChildStdout>,
    next_id: u64,
}

impl McpConnection {
    /// Spawn and initialize an MCP server.
    pub(super) async fn start(config: &super::McpServerConfig) -> std::result::Result<Self, String> {
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
    pub(super) async fn request(&mut self, method: &str, params: Value) -> std::result::Result<Value, String> {
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
    pub(super) async fn notify(&mut self, method: &str, params: Value) -> std::result::Result<(), String> {
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
    pub(super) async fn list_tools(&mut self) -> std::result::Result<Vec<McpToolDef>, String> {
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
    pub(super) async fn call_tool(&mut self, name: &str, arguments: Value) -> std::result::Result<String, String> {
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
