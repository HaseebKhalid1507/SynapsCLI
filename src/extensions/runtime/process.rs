//! Process-based extension runtime — JSON-RPC 2.0 over stdio.
//!
//! Spawns the extension as a child process. Communication uses
//! Content-Length framing (LSP-style) over stdin/stdout.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use super::{ExtensionHandler, ExtensionHealth};
use crate::extensions::hooks::events::{HookEvent, HookResult};

const MAX_RESTARTS: usize = 3;

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
    id: u64,
}

#[derive(Deserialize)]
struct JsonRpcError {
    #[allow(dead_code)]
    code: i64,
    message: String,
}

struct ProcessState {
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
}

/// A running extension process communicating via JSON-RPC 2.0 over stdio.
pub struct ProcessExtension {
    id: String,
    command: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    state: Arc<Mutex<Option<ProcessState>>>,
    /// Serializes a full request/response exchange and restart attempts.
    call_lock: Arc<Mutex<()>>,
    next_id: AtomicU64,
    restart_count: AtomicUsize,
}

impl ProcessExtension {
    pub async fn spawn(id: &str, command: &str, args: &[String]) -> Result<Self, String> {
        Self::spawn_with_cwd(id, command, args, None).await
    }

    /// Spawn `command` with `args` and optional working directory.
    ///
    /// Child stderr is captured and forwarded to debug tracing with the extension
    /// id so extension authors can inspect diagnostics without corrupting stdout.
    pub async fn spawn_with_cwd(
        id: &str,
        command: &str,
        args: &[String],
        cwd: Option<PathBuf>,
    ) -> Result<Self, String> {
        let state = Self::spawn_state(id, command, args, cwd.as_ref()).await?;
        Ok(Self {
            id: id.to_string(),
            command: command.to_string(),
            args: args.to_vec(),
            cwd,
            state: Arc::new(Mutex::new(Some(state))),
            call_lock: Arc::new(Mutex::new(())),
            next_id: AtomicU64::new(1),
            restart_count: AtomicUsize::new(0),
        })
    }

    async fn spawn_state(
        id: &str,
        command: &str,
        args: &[String],
        cwd: Option<&PathBuf>,
    ) -> Result<ProcessState, String> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }

        let mut child = cmd
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
        if let Some(stderr) = child.stderr.take() {
            let extension_id = id.to_string();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => {
                            tracing::debug!(extension = %extension_id, stderr = %line);
                        }
                        Ok(None) => break,
                        Err(error) => {
                            tracing::debug!(
                                extension = %extension_id,
                                error = %error,
                                "Failed to read extension stderr",
                            );
                            break;
                        }
                    }
                }
            });
        }

        Ok(ProcessState {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    async fn restart_locked(&self, state: &mut Option<ProcessState>) -> Result<(), String> {
        let attempted = self.restart_count.fetch_add(1, Ordering::Relaxed) + 1;
        if attempted > MAX_RESTARTS {
            *state = None;
            return Err(format!(
                "Extension '{}' exceeded restart limit ({})",
                self.id, MAX_RESTARTS,
            ));
        }

        if let Some(mut old) = state.take() {
            let _ = old.child.kill().await;
        }

        tracing::warn!(
            extension = %self.id,
            attempt = attempted,
            max_attempts = MAX_RESTARTS,
            "Restarting extension process after transport failure",
        );
        *state = Some(Self::spawn_state(
            &self.id,
            &self.command,
            &self.args,
            self.cwd.as_ref(),
        ).await?);
        Ok(())
    }

    async fn call_once_locked(
        &self,
        state: &mut ProcessState,
        method: &str,
        params: Value,
        id: u64,
    ) -> Result<Value, String> {
        let body = serde_json::to_string(&JsonRpcRequest {
            jsonrpc: "2.0",
            method: method.to_string(),
            params,
            id,
        })
        .map_err(|e| format!("Serialize error: {}", e))?;

        let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        state
            .stdin
            .write_all(frame.as_bytes())
            .await
            .map_err(|e| format!("Write error: {}", e))?;
        state
            .stdin
            .flush()
            .await
            .map_err(|e| format!("Flush error: {}", e))?;

        let mut content_length: Option<usize> = None;
        loop {
            let mut header_line = String::new();
            state
                .stdout
                .read_line(&mut header_line)
                .await
                .map_err(|e| format!("Read header error: {}", e))?;

            if header_line.is_empty() {
                return Err("Unexpected EOF while reading response headers".into());
            }
            if header_line.len() > 1024 {
                return Err(format!(
                    "Extension '{}' header line too long ({} bytes)",
                    self.id, header_line.len()
                ));
            }

            let trimmed = header_line.trim();
            if trimmed.is_empty() {
                break;
            }
            if let Some((name, value)) = trimmed.split_once(':') {
                if name.trim().eq_ignore_ascii_case("Content-Length") {
                    content_length = Some(
                        value.trim().parse().map_err(|_| {
                            format!("Invalid Content-Length value: {:?}", value.trim())
                        })?
                    );
                }
            }
        }

        let content_length = content_length.ok_or_else(|| {
            format!("Extension '{}' response missing Content-Length header", self.id)
        })?;
        const MAX_RESPONSE_SIZE: usize = 4 * 1024 * 1024;
        if content_length > MAX_RESPONSE_SIZE {
            return Err(format!(
                "Extension '{}' response too large: {} bytes (max {})",
                self.id, content_length, MAX_RESPONSE_SIZE
            ));
        }

        let mut buf = vec![0u8; content_length];
        tokio::io::AsyncReadExt::read_exact(&mut state.stdout, &mut buf)
            .await
            .map_err(|e| format!("Read body error: {}", e))?;

        let response: JsonRpcResponse = serde_json::from_slice(&buf)
            .map_err(|e| format!("Parse response error: {}", e))?;
        if response.id != id {
            return Err(format!(
                "Extension '{}' response ID mismatch: expected {}, got {} (protocol desync)",
                self.id, id, response.id
            ));
        }
        if let Some(err) = response.error {
            return Err(format!("Extension error: {}", err.message));
        }
        Ok(response.result.unwrap_or(Value::Null))
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let _call_guard = self.call_lock.lock().await;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut state_guard = self.state.lock().await;
        if state_guard.is_none() {
            self.restart_locked(&mut state_guard).await?;
        }

        let result = self
            .call_once_locked(
                state_guard.as_mut().expect("state should exist after restart"),
                method,
                params.clone(),
                id,
            )
            .await;

        match result {
            Ok(value) => Ok(value),
            Err(first_error) => {
                self.restart_locked(&mut state_guard).await?;
                let retry_id = self.next_id.fetch_add(1, Ordering::Relaxed);
                self.call_once_locked(
                    state_guard.as_mut().expect("state should exist after restart"),
                    method,
                    params,
                    retry_id,
                )
                .await
                .map_err(|retry_error| {
                    format!("{}; retry after restart failed: {}", first_error, retry_error)
                })
            }
        }
    }
}

#[async_trait]
impl ExtensionHandler for ProcessExtension {
    fn id(&self) -> &str {
        &self.id
    }

    async fn handle(&self, event: &HookEvent) -> HookResult {
        let params = serde_json::to_value(event).unwrap_or(Value::Null);
        match tokio::time::timeout(std::time::Duration::from_secs(5), self.call("hook.handle", params)).await {
            Ok(Ok(value)) => serde_json::from_value(value).unwrap_or(HookResult::Continue),
            Ok(Err(e)) => {
                tracing::warn!(
                    extension = %self.id,
                    error = %e,
                    "Extension hook handler failed — continuing",
                );
                HookResult::Continue
            }
            Err(_) => {
                tracing::warn!(
                    extension = %self.id,
                    timeout_secs = 5,
                    "Extension hook handler timed out — continuing",
                );
                HookResult::Continue
            }
        }
    }

    async fn shutdown(&self) {
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            self.call("shutdown", Value::Null),
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let mut state_guard = self.state.lock().await;
        if let Some(mut state) = state_guard.take() {
            let _ = state.child.kill().await;
        }
    }

    async fn health(&self) -> ExtensionHealth {
        if self.restart_count.load(Ordering::Relaxed) > MAX_RESTARTS {
            ExtensionHealth::Failed
        } else if self.restart_count.load(Ordering::Relaxed) > 0 {
            ExtensionHealth::Restarting
        } else {
            ExtensionHealth::Healthy
        }
    }
}
