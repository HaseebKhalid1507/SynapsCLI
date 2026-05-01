//! Process-based extension runtime — JSON-RPC 2.0 over stdio.
//!
//! Spawns the extension as a child process. Communication uses
//! Content-Length framing (LSP-style) over stdin/stdout.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::collections::HashSet;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use super::{ExtensionHandler, ExtensionHealth};
use crate::extensions::hooks::events::{HookEvent, HookResult};
use crate::extensions::manifest::CURRENT_EXTENSION_PROTOCOL_VERSION;

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


#[derive(Serialize)]
struct InitializeParams {
    synaps_version: &'static str,
    extension_protocol_version: u32,
    plugin_id: String,
    plugin_root: Option<String>,
    config: Value,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RegisteredExtensionToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RegisteredProviderSpec {
    pub id: String,
    pub display_name: String,
    pub description: String,
    #[serde(default)]
    pub models: Vec<RegisteredProviderModelSpec>,
    #[serde(default)]
    pub config_schema: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RegisteredProviderModelSpec {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub capabilities: Value,
    #[serde(default)]
    pub context_window: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderCompleteParams {
    pub provider_id: String,
    pub model_id: String,
    pub model: String,
    pub messages: Vec<Value>,
    pub system_prompt: Option<String>,
    pub tools: Vec<Value>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub thinking_budget: u32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProviderCompleteResult {
    pub content: Vec<Value>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub usage: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderToolUse {
    pub id: String,
    pub name: String,
    pub input: Value,
}

pub fn extract_provider_tool_uses(content: &[Value]) -> Result<Vec<ProviderToolUse>, String> {
    let mut tool_uses = Vec::new();
    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let id = block
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| "provider tool_use missing id".to_string())?;
        let name = block
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| "provider tool_use missing name".to_string())?;
        if id.trim().is_empty() {
            return Err("provider tool_use id is empty".to_string());
        }
        if name.trim().is_empty() {
            return Err("provider tool_use name is empty".to_string());
        }
        let input = block
            .get("input")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        if !input.is_object() {
            return Err(format!(
                "provider tool_use '{}' input must be a JSON object",
                id
            ));
        }
        tool_uses.push(ProviderToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input,
        });
    }
    Ok(tool_uses)
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct InitializeCapabilitiesResult {
    pub tools: Vec<RegisteredExtensionToolSpec>,
    pub providers: Vec<RegisteredProviderSpec>,
}

#[derive(Deserialize)]
struct InitializeResult {
    protocol_version: u32,
    #[serde(default)]
    capabilities: InitializeCapabilities,
}

#[derive(Default, Deserialize)]
struct InitializeCapabilities {
    #[serde(default)]
    tools: Vec<RegisteredExtensionToolSpec>,
    #[serde(default)]
    providers: Vec<RegisteredProviderSpec>,
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

    pub fn restart_count(&self) -> usize {
        self.restart_count.load(Ordering::Relaxed)
    }

    pub async fn initialize(&self, plugin_root: Option<PathBuf>, config: Value) -> Result<InitializeCapabilitiesResult, String> {
        let params = InitializeParams {
            synaps_version: env!("CARGO_PKG_VERSION"),
            extension_protocol_version: CURRENT_EXTENSION_PROTOCOL_VERSION,
            plugin_id: self.id.clone(),
            plugin_root: plugin_root
                .or_else(|| self.cwd.clone())
                .map(|path| path.to_string_lossy().to_string()),
            config,
        };
        let value = self.call_no_restart("initialize", serde_json::to_value(params).map_err(|e| e.to_string())?).await?;
        Self::parse_initialize_result(&self.id, value)
    }

    fn parse_initialize_result(id: &str, value: Value) -> Result<InitializeCapabilitiesResult, String> {
        let result: InitializeResult = serde_json::from_value(value)
            .map_err(|e| format!("Invalid initialize response from extension '{}': {}", id, e))?;
        if result.protocol_version != CURRENT_EXTENSION_PROTOCOL_VERSION {
            return Err(format!(
                "Extension '{}' initialize returned unsupported protocol_version {} (supported: {})",
                id, result.protocol_version, CURRENT_EXTENSION_PROTOCOL_VERSION,
            ));
        }
        Self::validate_registered_tool_specs(id, &result.capabilities.tools)?;
        Self::validate_registered_provider_specs(id, &result.capabilities.providers)?;
        Ok(InitializeCapabilitiesResult {
            tools: result.capabilities.tools,
            providers: result.capabilities.providers,
        })
    }

    fn validate_registered_tool_specs(id: &str, tools: &[RegisteredExtensionToolSpec]) -> Result<(), String> {
        let mut names = HashSet::new();
        for tool in tools {
            let name = tool.name.trim();
            if name.is_empty() {
                return Err(format!("Extension '{}' registered a tool with an empty tool name", id));
            }
            if !names.insert(name.to_string()) {
                return Err(format!("Extension '{}' registered duplicate tool name '{}'", id, name));
            }
            if tool.description.trim().is_empty() {
                return Err(format!(
                    "Extension '{}' registered tool '{}' with an empty description",
                    id, name,
                ));
            }
            if !tool.input_schema.is_object() {
                return Err(format!(
                    "Extension '{}' registered tool '{}' with invalid input_schema: input_schema must be a JSON object",
                    id, name,
                ));
            }
        }
        Ok(())
    }

    fn validate_registered_provider_specs(id: &str, providers: &[RegisteredProviderSpec]) -> Result<(), String> {
        for provider in providers {
            let provider_id = provider.id.trim();
            if provider_id.is_empty() {
                return Err(format!("Extension '{}' registered provider with empty provider id", id));
            }
            if !Self::is_safe_provider_id(provider_id) {
                return Err(format!(
                    "Extension '{}' registered provider '{}' with invalid provider id",
                    id, provider_id,
                ));
            }
            if provider.display_name.trim().is_empty() {
                return Err(format!(
                    "Extension '{}' registered provider '{}' with empty display_name",
                    id, provider_id,
                ));
            }
            if provider.description.trim().is_empty() {
                return Err(format!(
                    "Extension '{}' registered provider '{}' with empty description",
                    id, provider_id,
                ));
            }
            if provider.models.is_empty() {
                return Err(format!(
                    "Extension '{}' registered provider '{}' must declare at least one model",
                    id, provider_id,
                ));
            }
            let mut model_ids = HashSet::new();
            for model in &provider.models {
                let model_id = model.id.trim();
                if model_id.is_empty() {
                    return Err(format!(
                        "Extension '{}' registered provider '{}' with empty model id",
                        id, provider_id,
                    ));
                }
                if model_id.contains(':') {
                    return Err(format!(
                        "Extension '{}' registered provider '{}' with invalid model id '{}': ':' is reserved",
                        id, provider_id, model_id,
                    ));
                }
                if !model_ids.insert(model_id.to_string()) {
                    return Err(format!(
                        "Extension '{}' registered provider '{}' with duplicate model id '{}'",
                        id, provider_id, model_id,
                    ));
                }
            }
            if let Some(config_schema) = &provider.config_schema {
                if !config_schema.is_object() {
                    return Err(format!(
                        "Extension '{}' registered provider '{}' with invalid config_schema: config_schema must be a JSON object",
                        id, provider_id,
                    ));
                }
            }
        }
        Ok(())
    }

    fn is_safe_provider_id(id: &str) -> bool {
        !id.is_empty()
            && !id.contains(':')
            && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    }

    #[doc(hidden)]
    pub async fn initialize_for_test(&self, plugin_root: Option<PathBuf>) -> Result<(), String> {
        self.initialize(plugin_root, Value::Object(Default::default())).await.map(|_| ())
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
        self.initialize_locked(state).await?;
        Ok(())
    }


    async fn initialize_locked(&self, state: &mut Option<ProcessState>) -> Result<(), String> {
        let params = InitializeParams {
            synaps_version: env!("CARGO_PKG_VERSION"),
            extension_protocol_version: CURRENT_EXTENSION_PROTOCOL_VERSION,
            plugin_id: self.id.clone(),
            plugin_root: self.cwd
                .clone()
                .map(|path| path.to_string_lossy().to_string()),
            config: Value::Object(Default::default()),
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let value = self.call_once_locked(
            state.as_mut().expect("state should exist for initialize"),
            "initialize",
            serde_json::to_value(params).map_err(|e| e.to_string())?,
            id,
        ).await?;
        Self::parse_initialize_result(&self.id, value).map(|_| ())
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

    async fn call_no_restart(&self, method: &str, params: Value) -> Result<Value, String> {
        let _call_guard = self.call_lock.lock().await;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut state_guard = self.state.lock().await;
        if state_guard.is_none() {
            *state_guard = Some(Self::spawn_state(
                &self.id,
                &self.command,
                &self.args,
                self.cwd.as_ref(),
            ).await?);
        }
        self.call_once_locked(
            state_guard.as_mut().expect("state should exist"),
            method,
            params,
            id,
        ).await
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

    async fn call_tool(&self, name: &str, input: Value) -> Result<Value, String> {
        self.call("tool.call", serde_json::json!({
            "name": name,
            "input": input,
        })).await
    }

    async fn provider_complete(&self, params: ProviderCompleteParams) -> Result<ProviderCompleteResult, String> {
        let value = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            self.call("provider.complete", serde_json::to_value(params).map_err(|e| e.to_string())?),
        )
        .await
        .map_err(|_| format!("Extension '{}' provider.complete timed out", self.id))??;
        let result: ProviderCompleteResult = serde_json::from_value(value)
            .map_err(|e| format!("Invalid provider.complete response from extension '{}': {}", self.id, e))?;
        if result.content.is_empty() {
            return Err(format!("Extension '{}' provider.complete returned empty content", self.id));
        }
        Ok(result)
    }

    async fn provider_stream(&self, _params: ProviderCompleteParams) -> Result<(), String> {
        Err("provider.stream is reserved but not implemented in this Synaps version".to_string())
    }

    async fn handle(&self, event: &HookEvent) -> HookResult {
        let params = serde_json::to_value(event).unwrap_or(Value::Null);
        match tokio::time::timeout(std::time::Duration::from_secs(5), self.call("hook.handle", params)).await {
            Ok(Ok(value)) => match serde_json::from_value(value.clone()) {
                Ok(result) => result,
                Err(error) => {
                    tracing::warn!(
                        extension = %self.id,
                        error = %error,
                        response = %value,
                        "Extension hook handler returned invalid result",
                    );
                    if value.get("action").and_then(Value::as_str) == Some("modify") {
                        HookResult::Block {
                            reason: "Extension returned malformed modify result".to_string(),
                        }
                    } else {
                        HookResult::Continue
                    }
                }
            },
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

    async fn restart_count(&self) -> usize {
        self.restart_count()
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
