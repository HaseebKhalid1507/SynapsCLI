//! MCP JSON-RPC connection — child process management and protocol implementation.
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use super::McpToolDef;

/// Hard bound on one JSON-RPC response line from a server.
pub(super) const MAX_LINE_BYTES: u64 = 1024 * 1024;

/// A running MCP server connection — manages the child process and JSON-RPC.
pub(super) struct McpConnection {
    child: Child,
    /// `None` after stdin has been closed for graceful shutdown.
    stdin: Option<tokio::process::ChildStdin>,
    reader: BufReader<tokio::process::ChildStdout>,
    next_id: u64,
}

impl McpConnection {
    /// Spawn and initialize an MCP server.
    pub(super) async fn start(
        config: &super::McpServerConfig,
    ) -> std::result::Result<Self, String> {
        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args);

        // H3: clear inherited env to prevent leaking host secrets
        // (ANTHROPIC_API_KEY, AWS_*, etc.) to third-party MCP servers.
        // Mirror the ProcessExtension pattern (process.rs env_clear).
        cmd.env_clear();
        // Re-inject essential vars for child process operation.
        for var in ["PATH", "HOME", "LANG", "TERM", "XDG_RUNTIME_DIR", "TMPDIR"] {
            if let Ok(val) = std::env::var(var) {
                cmd.env(var, val);
            }
        }
        // Apply user-configured env overrides from mcp.json.
        for (k, v) in &config.env {
            cmd.env(k, v);
        }

        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| {
            // Static process metadata only: command/env values may be
            // sensitive local operator data.
            format!(
                "Failed to spawn MCP server process (io kind {:?}, os code {:?})",
                e.kind(),
                e.raw_os_error()
            )
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Failed to capture MCP server stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Failed to capture MCP server stdout".to_string())?;

        // Drain stderr with bounded memory and WITHOUT logging provider
        // content: only byte-count metadata is recorded. The task ends at
        // child EOF (no unbounded work).
        if let Some(mut stderr) = child.stderr.take() {
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut buf = [0u8; 8192];
                let mut total: u64 = 0;
                while let Ok(n) = stderr.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    total = total.saturating_add(n as u64);
                }
                if total > 0 {
                    // Static metadata only: no command name, no content.
                    tracing::debug!(
                        bytes = total,
                        "MCP server stderr drained (content withheld)"
                    );
                }
            });
        }

        let mut conn = McpConnection {
            child,
            stdin: Some(stdin),
            reader: BufReader::new(stdout),
            next_id: 1,
        };

        // Initialize handshake
        let init_result = conn
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "synaps-cli",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            )
            .await?;

        // Response content is provider-controlled — never logged.
        let _ = init_result;
        tracing::debug!("MCP initialize handshake complete");

        // Send initialized notification (no response expected)
        conn.notify("notifications/initialized", json!({})).await?;

        Ok(conn)
    }

    /// Send a JSON-RPC request and wait for the response.
    pub(super) async fn request(
        &mut self,
        method: &str,
        params: Value,
    ) -> std::result::Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;

        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });

        let msg = format!(
            "{}\n",
            serde_json::to_string(&request)
                .map_err(|e| format!("Failed to serialize MCP request: {}", e))?
        );
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| "MCP connection is shut down".to_string())?;
        stdin
            .write_all(msg.as_bytes())
            .await
            .map_err(|e| format!("Failed to write to MCP server: {}", e))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("Failed to flush MCP server stdin: {}", e))?;

        // Read lines until we get a response with matching id
        // (skip notifications from the server)
        let timeout = tokio::time::Duration::from_secs(30);
        let result = tokio::time::timeout(timeout, async {
            loop {
                // Bounded read: a hostile/buggy server cannot balloon memory
                // with one endless line, and EOF is a typed failure instead
                // of a silent spin until timeout.
                use tokio::io::AsyncReadExt;
                let mut line = String::new();
                let mut limited = (&mut self.reader).take(MAX_LINE_BYTES + 1);
                let n = limited
                    .read_line(&mut line)
                    .await
                    .map_err(|e| format!("Failed to read from MCP server: {}", e))?;
                if n == 0 {
                    return Err("MCP server closed its output stream (EOF)".to_string());
                }
                if line.len() as u64 > MAX_LINE_BYTES {
                    return Err(format!(
                        "MCP response line exceeds the {} byte bound",
                        MAX_LINE_BYTES
                    ));
                }

                if line.trim().is_empty() {
                    continue;
                }

                // Provider content is never echoed into errors — only the
                // parser position metadata and the line length.
                let response: Value = serde_json::from_str(line.trim()).map_err(|e| {
                    format!(
                        "Invalid JSON from MCP server ({} byte line): {}",
                        line.trim().len(),
                        e
                    )
                })?;

                // Check if this is our response (has matching id)
                if response.get("id").and_then(|v| v.as_u64()) == Some(id) {
                    if let Some(error) = response.get("error") {
                        // Provider-authored message text is withheld from
                        // errors/logs; only code + length metadata surface.
                        let code = error["code"].as_i64().unwrap_or(-1);
                        let msg_len = error["message"].as_str().map(str::len).unwrap_or(0);
                        return Err(format!(
                            "MCP error (code {}) from server; {}-byte provider message withheld",
                            code, msg_len
                        ));
                    }
                    return Ok(response["result"].clone());
                }
                // Otherwise it's a notification or response to different request — skip
            }
        })
        .await;

        match result {
            Ok(r) => r,
            Err(_) => Err(format!("MCP request '{}' timed out after 30s", method)),
        }
    }

    /// Send a JSON-RPC notification (no response).
    pub(super) async fn notify(
        &mut self,
        method: &str,
        params: Value,
    ) -> std::result::Result<(), String> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });

        let msg = format!(
            "{}\n",
            serde_json::to_string(&notification)
                .map_err(|e| format!("Failed to serialize MCP notification: {}", e))?
        );
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| "MCP connection is shut down".to_string())?;
        stdin
            .write_all(msg.as_bytes())
            .await
            .map_err(|e| format!("Failed to write notification to MCP server: {}", e))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("Failed to flush MCP server stdin: {}", e))?;
        Ok(())
    }

    /// Ask the OS to kill the child now (idempotent; `kill_on_drop` remains
    /// the backstop when the connection is dropped).
    pub(super) fn start_kill(&mut self) {
        let _ = self.child.start_kill();
    }

    /// Close the child's stdin (EOF) without killing it — the graceful
    /// first step of shutdown. Sync and idempotent.
    pub(super) fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// Bounded completion of a shutdown whose stdin is already closed:
    /// give the child ~250ms to exit on EOF, then `start_kill` and wait a
    /// further bounded second for the reap. Never unbounded.
    pub(super) async fn finish_shutdown(&mut self) {
        const GRACE: std::time::Duration = std::time::Duration::from_millis(250);
        const KILL_WAIT: std::time::Duration = std::time::Duration::from_secs(1);
        if tokio::time::timeout(GRACE, self.child.wait()).await.is_ok() {
            return;
        }
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(KILL_WAIT, self.child.wait()).await;
    }

    /// Full graceful-then-forced shutdown: close stdin, bounded EOF grace,
    /// bounded kill+reap fallback.
    pub(super) async fn shutdown(&mut self) {
        self.close_stdin();
        self.finish_shutdown().await;
    }

    /// List available tools from the server.
    pub(super) async fn list_tools(&mut self) -> std::result::Result<Vec<McpToolDef>, String> {
        let result = self.request("tools/list", json!({})).await?;

        let tools = result["tools"]
            .as_array()
            .ok_or_else(|| "MCP tools/list response missing 'tools' array".to_string())?;

        // Bounded acceptance: count, name, description, and schema size are
        // all capped (same budgets as the local descriptor cache); invalid
        // entries are skipped without echoing provider content.
        use super::descriptors::{
            SERVER_MAX_TOOLS, TOOL_DESCRIPTION_MAX_BYTES, TOOL_NAME_MAX_BYTES,
            TOOL_SCHEMA_MAX_BYTES,
        };
        let mut defs = Vec::new();
        for tool in tools {
            if defs.len() >= SERVER_MAX_TOOLS {
                tracing::warn!(max = SERVER_MAX_TOOLS, "MCP tools/list truncated at bound");
                break;
            }
            let name = tool["name"].as_str().unwrap_or("").to_string();
            if name.is_empty()
                || name.len() > TOOL_NAME_MAX_BYTES
                || name.chars().any(char::is_control)
            {
                tracing::warn!("Skipping MCP listed tool with invalid name (content withheld)");
                continue;
            }
            let description = agent_core::BoundedText::new(
                tool["description"].as_str().unwrap_or(""),
                TOOL_DESCRIPTION_MAX_BYTES,
            )
            .text;
            let input_schema = tool.get("inputSchema").cloned().unwrap_or(json!({
                "type": "object",
                "properties": {},
                "required": []
            }));
            let schema_len = serde_json::to_vec(&input_schema)
                .map(|b| b.len())
                .unwrap_or(usize::MAX);
            if !input_schema.is_object() || schema_len > TOOL_SCHEMA_MAX_BYTES {
                // Listed name is provider-controlled — withheld from logs.
                tracing::warn!(
                    "Skipping MCP listed tool with invalid or oversized schema (name withheld)"
                );
                continue;
            }
            defs.push(McpToolDef {
                name,
                description,
                input_schema,
            });
        }

        Ok(defs)
    }

    /// Call a tool on the server.
    pub(super) async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
    ) -> std::result::Result<String, String> {
        let result = self
            .request(
                "tools/call",
                json!({
                    "name": name,
                    "arguments": arguments
                }),
            )
            .await?;

        // Extract text content from the result
        let content = result.get("content").and_then(|c| c.as_array());

        match content {
            Some(blocks) => {
                let mut output = String::new();
                for block in blocks {
                    match block["type"].as_str() {
                        Some("text") => {
                            if let Some(text) = block["text"].as_str() {
                                if !output.is_empty() {
                                    output.push('\n');
                                }
                                output.push_str(text);
                            }
                        }
                        Some("image") => {
                            output.push_str("[image content]");
                        }
                        Some("resource") => {
                            if let Some(text) =
                                block.get("resource").and_then(|r| r["text"].as_str())
                            {
                                if !output.is_empty() {
                                    output.push('\n');
                                }
                                output.push_str(text);
                            }
                        }
                        _ => {}
                    }
                }

                if result
                    .get("isError")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
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
