//! Process-based extension runtime — JSON-RPC 2.0 over stdio.
//!
//! Spawns the extension as a child process. Communication uses
//! Content-Length framing (LSP-style) over stdin/stdout.
//!
//! # Wire format
//!
//! Every message is prefixed by a single header line followed by a blank line:
//!
//! ```text
//! Content-Length: <byte-count>\r\n
//! \r\n
//! <json-body>
//! ```
//!
//! This matches the Language Server Protocol framing so extensions can
//! be written with any LSP-aware JSON-RPC library.
//!
//! # Required dependency
//!
//! ```toml
//! # Cargo.toml
//! async-trait = "0.1"
//! ```

use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::extensions::hooks::events::{HookEvent, HookResult};
use super::ExtensionHandler;

// ── Wire types ────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    method: String,
    params: Value,
    id: u64,
}

#[derive(Deserialize)]
struct JsonRpcResponse {
    #[allow(dead_code)]
    jsonrpc: String,
    result: Option<Value>,
    error: Option<JsonRpcError>,
    #[allow(dead_code)]
    id: u64,
}

#[derive(Deserialize)]
struct JsonRpcError {
    #[allow(dead_code)]
    code: i64,
    message: String,
}

// ── ProcessExtension ──────────────────────────────────────────────────────────

/// A running extension process communicating via JSON-RPC 2.0 over stdio.
///
/// Each [`handle`](ExtensionHandler::handle) call serialises the [`HookEvent`]
/// as the `params` of a `hook.handle` request and deserialises the response
/// body back into a [`HookResult`].
///
/// On error the handler fails-open — it logs a warning and returns
/// [`HookResult::Continue`] so a misbehaving extension never blocks the agent.
pub struct ProcessExtension {
    id: String,
    stdin: Arc<Mutex<tokio::process::ChildStdin>>,
    stdout: Arc<Mutex<BufReader<tokio::process::ChildStdout>>>,
    child: Arc<Mutex<Child>>,
    /// Monotonically-increasing JSON-RPC request id.
    next_id: AtomicU64,
}

impl ProcessExtension {
    /// Spawn `command` with `args` and return a ready [`ProcessExtension`].
    ///
    /// Stderr of the child process is discarded (redirect it yourself before
    /// calling this if you need to capture it).
    pub async fn spawn(id: &str, command: &str, args: &[String]) -> Result<Self, String> {
        let mut child = Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("Failed to spawn extension '{}': {}", id, e))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("No stdin for extension '{}'", id))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("No stdout for extension '{}'", id))?;

        Ok(Self {
            id: id.to_string(),
            stdin: Arc::new(Mutex::new(stdin)),
            stdout: Arc::new(Mutex::new(BufReader::new(stdout))),
            child: Arc::new(Mutex::new(child)),
            next_id: AtomicU64::new(1),
        })
    }

    /// Send a JSON-RPC request and return the `result` value from the response.
    ///
    /// Uses Content-Length framing on both ends. Requests and responses are
    /// serialised / deserialised in one lock-scope each to keep the mutex
    /// held for the minimum time.
    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let body = serde_json::to_string(&JsonRpcRequest {
            jsonrpc: "2.0",
            method: method.to_string(),
            params,
            id,
        })
        .map_err(|e| format!("Serialize error: {}", e))?;

        // ── Write request ───────────────────────────────────────────────────
        let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(frame.as_bytes())
                .await
                .map_err(|e| format!("Write error: {}", e))?;
            stdin
                .flush()
                .await
                .map_err(|e| format!("Flush error: {}", e))?;
        } // drop stdin lock before reading

        // ── Read response ───────────────────────────────────────────────────
        let body_buf = {
            let mut stdout = self.stdout.lock().await;

            // Header: "Content-Length: <n>\r\n"
            let mut header_line = String::new();
            stdout
                .read_line(&mut header_line)
                .await
                .map_err(|e| format!("Read header error: {}", e))?;

            let content_length: usize = header_line
                .trim()
                .strip_prefix("Content-Length: ")
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| {
                    format!("Invalid Content-Length header: {:?}", header_line)
                })?;

            // Guard against OOM from malicious/buggy Content-Length
            const MAX_RESPONSE_SIZE: usize = 4 * 1024 * 1024; // 4 MB
            if content_length > MAX_RESPONSE_SIZE {
                return Err(format!(
                    "Extension '{}' response too large: {} bytes (max {})",
                    self.id, content_length, MAX_RESPONSE_SIZE
                ));
            }

            // Blank separator line "\r\n"
            let mut blank = String::new();
            stdout
                .read_line(&mut blank)
                .await
                .map_err(|e| format!("Read separator error: {}", e))?;

            // Body
            let mut buf = vec![0u8; content_length];
            tokio::io::AsyncReadExt::read_exact(&mut *stdout, &mut buf)
                .await
                .map_err(|e| format!("Read body error: {}", e))?;
            buf
        }; // drop stdout lock

        let response: JsonRpcResponse = serde_json::from_slice(&body_buf)
            .map_err(|e| format!("Parse response error: {}", e))?;

        if let Some(err) = response.error {
            return Err(format!("Extension error: {}", err.message));
        }

        Ok(response.result.unwrap_or(Value::Null))
    }
}

// ── ExtensionHandler impl ─────────────────────────────────────────────────────

#[async_trait]
impl ExtensionHandler for ProcessExtension {
    fn id(&self) -> &str {
        &self.id
    }

    /// Dispatch a hook event to the extension process.
    ///
    /// Fails-open: on any transport or extension-reported error the result is
    /// [`HookResult::Continue`] so a broken extension never stalls the agent.
    async fn handle(&self, event: &HookEvent) -> HookResult {
        let params = serde_json::to_value(event).unwrap_or(Value::Null);
        match self.call("hook.handle", params).await {
            Ok(value) => serde_json::from_value(value).unwrap_or(HookResult::Continue),
            Err(e) => {
                tracing::warn!(
                    extension = %self.id,
                    error = %e,
                    "Extension hook handler failed — continuing",
                );
                HookResult::Continue
            }
        }
    }

    /// Send a `shutdown` notification then SIGKILL after 500 ms.
    async fn shutdown(&self) {
        // Best-effort shutdown request — bound the wait in case the
        // extension never replies; the kill below is the real cleanup.
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            self.call("shutdown", Value::Null),
        ).await;

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let mut child = self.child.lock().await;
        let _ = child.kill().await;
    }
}
