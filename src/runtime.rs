use reqwest::Client;
use serde_json::{json, Value};
use serde::{Deserialize};

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use crate::{Result, RuntimeError, ToolRegistry};
use tokio::sync::{mpsc, RwLock};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::sync::CancellationToken;
use futures::stream::Stream;
use std::pin::Pin;
use futures::StreamExt;

#[derive(Debug, Deserialize)]
struct PiAuth {
    anthropic: AnthropicAuth,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct AnthropicAuth {
    #[serde(rename = "type")]
    auth_type: String,
    refresh: Option<String>,
    access: Option<String>,
    expires: Option<u64>,
    key: Option<String>,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Thinking(String),
    Text(String),
    ToolUseStart(String),
    ToolUseDelta(String),
    ToolUse {
        tool_name: String,
        tool_id: String,
        input: Value,
    },
    ToolResult {
        tool_id: String,
        result: String,
    },
    ToolResultDelta {
        tool_id: String,
        delta: String,
    },
    /// Subagent lifecycle events — rendered as a live status panel in the TUI
    SubagentStart {
        subagent_id: u64,
        agent_name: String,
        task_preview: String,
    },
    SubagentUpdate {
        subagent_id: u64,
        agent_name: String,
        status: String,
    },
    SubagentDone {
        subagent_id: u64,
        agent_name: String,
        result_preview: String,
        duration_secs: f64,
    },
    /// A steering message was delivered mid-stream (between tool rounds)
    SteeringDelivered {
        message: String,
    },
    /// Full message history after the tool loop completes, for multi-turn context
    MessageHistory(Vec<Value>),
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cache_read_input_tokens: u64,
        cache_creation_input_tokens: u64,
        /// Model that generated this usage. None = use parent runtime's model.
        model: Option<String>,
    },
    Done,
    Error(String),
}

/// Shared mutable auth state. Lives behind `Arc<RwLock<_>>` so the spawned
/// streaming task and the parent Runtime always see the same (freshest) token.
#[derive(Debug, Clone)]
struct AuthState {
    auth_token: String,
    auth_type: String,
    refresh_token: Option<String>,
    token_expires: Option<u64>,
}

pub struct Runtime {
    client: Client,
    auth: Arc<RwLock<AuthState>>,
    model: String,
    tools: Arc<RwLock<ToolRegistry>>,
    system_prompt: Option<String>,
    thinking_budget: u32,
    /// Path for watcher_exit tool to write handoff state (agent mode only)
    pub watcher_exit_path: Option<PathBuf>,
}

impl Runtime {
    pub async fn new() -> Result<Self> {
        let (auth_token, auth_type, refresh_token, token_expires) = Self::get_auth_token()?;

        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| RuntimeError::Config(format!("Failed to build HTTP client: {}", e)))?;

        Ok(Runtime {
            client,
            auth: Arc::new(RwLock::new(AuthState {
                auth_token,
                auth_type,
                refresh_token,
                token_expires,
            })),
            model: "claude-opus-4-6".to_string(),
            tools: Arc::new(RwLock::new(ToolRegistry::new())),
            system_prompt: None,
            thinking_budget: 4096,
            watcher_exit_path: None,
        })
    }

    pub fn set_system_prompt(&mut self, prompt: String) {
        self.system_prompt = Some(prompt);
    }

    pub fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }

    pub fn set_model(&mut self, model: String) {
        self.model = model;
    }

    pub fn set_tools(&mut self, tools: ToolRegistry) {
        self.tools = Arc::new(RwLock::new(tools));
    }

    /// Get a shared reference to the tool registry (for MCP lazy loading).
    pub fn tools_shared(&self) -> Arc<RwLock<ToolRegistry>> {
        Arc::clone(&self.tools)
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Returns the max output tokens for a given model.
    /// Opus-class models support 128K, Sonnet/Haiku cap at 64K.
    fn max_tokens_for_model(model: &str) -> u64 {
        if model.contains("opus") {
            128000
        } else {
            64000
        }
    }

    pub fn set_thinking_budget(&mut self, budget: u32) {
        self.thinking_budget = budget;
    }

    pub fn thinking_budget(&self) -> u32 {
        self.thinking_budget
    }

    pub fn thinking_level(&self) -> &str {
        match self.thinking_budget {
            0..=2048 => "low",
            2049..=4096 => "medium",
            4097..=16384 => "high",
            _ => "xhigh",
        }
    }

    /// Check if the OAuth token is expired and refresh it if needed.
    /// Uses Pi-style file locking for cross-process safety:
    /// - Acquires exclusive lock on auth.json
    /// - Re-reads inside the lock (another instance may have refreshed)
    /// - Refreshes via API only if still expired
    /// - Writes back atomically and releases lock
    ///
    /// Multiple SynapsCLI instances (or Avante/Jade) can safely call this
    /// simultaneously — they'll serialize on the lock and only one will
    /// actually hit the token endpoint.
    pub async fn refresh_if_needed(&self) -> Result<()> {
        // Fast path: read lock to check expiry without blocking writers
        {
            let auth = self.auth.read().await;
            if auth.auth_type != "oauth" {
                return Ok(());
            }

            let in_memory_expired = match auth.token_expires {
                Some(exp) => {
                    let now = crate::epoch_millis();
                    now >= exp
                }
                None => false,
            };

            if !in_memory_expired {
                return Ok(());
            }
        }
        // Read lock dropped here

        tracing::info!("Token needs refresh, checking...");

        // Slow path: delegate to auth.rs which handles locking, re-read,
        // conditional refresh, and persistence.
        tracing::info!("Refreshing auth token");
        let creds = crate::auth::ensure_fresh_token(&self.client)
            .await
            .map_err(|e| RuntimeError::Auth(format!(
                "Token refresh failed: {}. Run `login` to re-authenticate.", e
            )))?;

        // Update shared auth state so all clones (including spawned stream tasks)
        // immediately see the fresh token.
        {
            let mut auth = self.auth.write().await;
            auth.auth_token = creds.access;
            auth.refresh_token = Some(creds.refresh);
            auth.token_expires = Some(creds.expires);
        }

        Ok(())
    }
    
    fn get_auth_token() -> Result<(String, String, Option<String>, Option<u64>)> {
        // Try auth.json via the auth module
        if let Ok(Some(auth_file)) = crate::auth::load_auth() {
            let creds = &auth_file.anthropic;
            if creds.auth_type == "oauth" && !creds.access.is_empty() {
                return Ok((
                    creds.access.clone(),
                    "oauth".to_string(),
                    Some(creds.refresh.clone()),
                    Some(creds.expires),
                ));
            }
        }

        // Legacy: try the old PiAuth struct format (in case auth.json has optional fields)
        let auth_path = crate::config::resolve_read_path("auth.json");

        if auth_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&auth_path) {
                if let Ok(auth) = serde_json::from_str::<PiAuth>(&content) {
                    let creds = &auth.anthropic;
                    if let (true, Some(access)) = (creds.auth_type == "oauth", creds.access.as_ref()) {
                        return Ok((
                            access.clone(),
                            "oauth".to_string(),
                            creds.refresh.clone(),
                            creds.expires,
                        ));
                    }
                }
            }
        }

        // Fall back to env var
        if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
            return Ok((api_key, "api_key".to_string(), None, None));
        }
        
        Err(RuntimeError::Auth("No Anthropic credentials found. Run `login` to authenticate.".to_string()))
    }

    pub async fn run_single(&self, prompt: &str) -> Result<String> {
        // Refresh OAuth token if expired
        self.refresh_if_needed().await?;

        let mut messages = vec![json!({"role": "user", "content": prompt})];
        
        loop {
            let response = self.call_api(&messages).await?;
            
            // Check if Claude wants to use tools
            if let Some(content) = response["content"].as_array() {
                let mut response_text = String::new();
                let mut tool_uses = Vec::new();
                
                // Process response content
                for item in content {
                    match item["type"].as_str() {
                        Some("text") => {
                            if let Some(text) = item["text"].as_str() {
                                response_text.push_str(text);
                            }
                        }
                        Some("tool_use") => {
                            tool_uses.push(item.clone());
                        }
                        _ => {}
                    }
                }
                
                // If no tool uses, return the text response
                if tool_uses.is_empty() {
                    return Ok(response_text);
                }
                
                // Add assistant's response to conversation (only content, role)
                messages.push(json!({
                    "role": "assistant",
                    "content": content
                }));
                
                // Execute tools — parallel when multiple are requested
                let mut tool_results = Vec::new();
                
                if tool_uses.len() == 1 {
                    // Single tool — run inline, no spawn overhead
                    let tool_use = &tool_uses[0];
                    if let (Some(tool_name), Some(tool_id)) = (
                        tool_use["name"].as_str(),
                        tool_use["id"].as_str()
                    ) {
                        let input = &tool_use["input"];
                        let result = match self.tools.read().await.get(tool_name).cloned() {
                            Some(tool) => {
                                let ctx = crate::ToolContext { tx_delta: None, tx_events: None, watcher_exit_path: self.watcher_exit_path.clone() };
                                match tool.execute(input.clone(), ctx).await {
                                    Ok(output) => output,
                                    Err(e) => format!("Tool execution failed: {}", e),
                                }
                            }
                            None => format!("Unknown tool: {}", tool_name),
                        };
                        tool_results.push(json!({
                            "type": "tool_result",
                            "tool_use_id": tool_id,
                            "content": Self::truncate_tool_result(&result)
                        }));
                    }
                } else {
                    // Multiple tools — run in parallel with JoinSet
                    let mut join_set = tokio::task::JoinSet::new();
                    
                    for tool_use in &tool_uses {
                        if let (Some(tool_name), Some(tool_id)) = (
                            tool_use["name"].as_str().map(|s| s.to_string()),
                            tool_use["id"].as_str().map(|s| s.to_string()),
                        ) {
                            let input = tool_use["input"].clone();
                            let tool = self.tools.read().await.get(&tool_name).cloned();
                            let exit_path = self.watcher_exit_path.clone();
                            
                            join_set.spawn(async move {
                                let result = match tool {
                                    Some(t) => {
                                        let ctx = crate::ToolContext { tx_delta: None, tx_events: None, watcher_exit_path: exit_path };
                                        match t.execute(input, ctx).await {
                                            Ok(output) => output,
                                            Err(e) => format!("Tool execution failed: {}", e),
                                        }
                                    }
                                    None => format!("Unknown tool: {}", tool_name),
                                };
                                (tool_id, result)
                            });
                        }
                    }
                    
                    // Collect results, preserving order by tool_id
                    let mut results_map = std::collections::HashMap::new();
                    while let Some(res) = join_set.join_next().await {
                        match res {
                            Ok((tool_id, result)) => {
                                results_map.insert(tool_id, result);
                            }
                            Err(e) => {
                                // Task panicked — log it but don't crash
                                tracing::error!("Parallel tool task panicked: {}", e);
                            }
                        }
                    }
                    
                    // Build tool_results in original order — every tool_use MUST have a result
                    for tool_use in &tool_uses {
                        if let Some(tool_id) = tool_use["id"].as_str() {
                            let result = results_map.remove(tool_id)
                                .unwrap_or_else(|| "Tool execution failed: task panicked".to_string());
                            tool_results.push(json!({
                                "type": "tool_result",
                                "tool_use_id": tool_id,
                                "content": Self::truncate_tool_result(&result)
                            }));
                        }
                    }
                }
                
                // Add tool results to conversation
                messages.push(json!({
                    "role": "user",
                    "content": tool_results
                }));
                
                // Continue the loop to get Claude's response with tool results
            } else {
                return Err(RuntimeError::Tool("Invalid response format".to_string()));
            }
        }
    }

    pub async fn run_stream(&self, prompt: String, cancel: CancellationToken) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send>> {
        self.run_stream_with_messages(vec![json!({"role": "user", "content": prompt})], cancel, None).await
    }

    pub async fn run_stream_with_messages(
        &self,
        messages: Vec<Value>,
        cancel: CancellationToken,
        steering_rx: Option<mpsc::UnboundedReceiver<String>>,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send>> {
        let (tx, rx) = mpsc::unbounded_channel();

        // Refresh OAuth token if expired before starting the stream.
        if let Err(e) = self.refresh_if_needed().await {
            let _ = tx.send(StreamEvent::Error(e.to_string()));
            let _ = tx.send(StreamEvent::Done);
            return Box::pin(UnboundedReceiverStream::new(rx));
        }

        // Clone the Arc, not the whole Runtime — the spawned task shares the
        // same AuthState so mid-loop token refreshes are visible immediately.
        let auth = Arc::clone(&self.auth);
        let client = self.client.clone();
        let model = self.model.clone();
        let tools = self.tools.clone();
        let system_prompt = self.system_prompt.clone();
        let thinking_budget = self.thinking_budget;
        let watcher_exit_path = self.watcher_exit_path.clone();

        tokio::spawn(async move {
            if let Err(e) = Self::run_stream_internal(
                auth, client, model, tools, system_prompt, thinking_budget,
                messages, tx.clone(), cancel, steering_rx, watcher_exit_path,
            ).await {
                let _ = tx.send(StreamEvent::Error(e.to_string()));
            }
            let _ = tx.send(StreamEvent::Done);
        });

        Box::pin(UnboundedReceiverStream::new(rx))
    }

    async fn run_stream_internal(
        auth: Arc<RwLock<AuthState>>,
        client: Client,
        model: String,
        tools: Arc<RwLock<ToolRegistry>>,
        system_prompt: Option<String>,
        thinking_budget: u32,
        initial_messages: Vec<Value>,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
        mut steering_rx: Option<mpsc::UnboundedReceiver<String>>,
        watcher_exit_path: Option<PathBuf>,
    ) -> Result<()> {
        let mut messages = initial_messages;

        loop {
            // Check for cancellation before each API call
            if cancel.is_cancelled() {
                let _ = tx.send(StreamEvent::MessageHistory(messages));
                return Ok(());
            }

            // Refresh token before each API call in the tool loop — this is
            // the fix for stale tokens in long-running agentic sessions.
            {
                let auth_state = auth.read().await;
                if auth_state.auth_type == "oauth" {
                    let expired = match auth_state.token_expires {
                        Some(exp) => {
                            let now = crate::epoch_millis();
                            now >= exp
                        }
                        None => false,
                    };
                    if expired {
                        // Drop read lock before acquiring write
                        drop(auth_state);

                        tracing::info!("Refreshing token mid-stream");
                        let creds = crate::auth::ensure_fresh_token(&client)
                            .await
                            .map_err(|e| RuntimeError::Auth(format!(
                                "Token refresh failed mid-stream: {}. Run `login` to re-authenticate.", e
                            )))?;

                        let mut auth_w = auth.write().await;
                        auth_w.auth_token = creds.access;
                        auth_w.refresh_token = Some(creds.refresh);
                        auth_w.token_expires = Some(creds.expires);
                    }
                }
            }

            let tools_snapshot = tools.read().await.clone();
            let response = match Self::call_api_stream_inner(
                &auth, &client, &model, &tools_snapshot, &system_prompt, thinking_budget,
                &messages, tx.clone(), &cancel,
            ).await {
                Ok(r) => r,
                Err(e) => {
                    // Send whatever history we have so far, so context isn't lost
                    let _ = tx.send(StreamEvent::MessageHistory(messages));
                    return Err(e);
                }
            };

            // Check if Claude wants to use tools
            if let Some(content) = response["content"].as_array() {
                let mut tool_uses = Vec::new();

                // Process response content
                for item in content {
                    if item["type"].as_str() == Some("tool_use") {
                        tool_uses.push(item.clone());
                    }
                }

                // Add assistant's response to conversation
                messages.push(json!({
                    "role": "assistant",
                    "content": content
                }));

                // If no tool uses, check for steering messages before finishing.
                // Steering can redirect the model even when it has no more tool calls.
                if tool_uses.is_empty() {
                    let steered = Self::drain_steering(&mut steering_rx, &mut messages, &tx);
                    if !steered {
                        // No steering, truly done
                        let _ = tx.send(StreamEvent::MessageHistory(messages));
                        return Ok(());
                    }
                    // Steering message injected — continue the loop for another LLM call
                    continue;
                }

                // Execute tools and add results. We must always produce a tool_result for
                // every tool_use we just pushed onto the assistant message — otherwise the
                // next API call will fail with "tool_use ids were found without tool_result
                // blocks". On cancellation we synthesize a "Cancelled by user" result for any
                // remaining tools so message history stays valid.
                let mut tool_results = Vec::new();
                let mut cancelled = false;

                if cancel.is_cancelled() {
                    // Already cancelled before tool execution — fill all with cancel results
                    for tool_use in &tool_uses {
                        let tool_id = tool_use["id"].as_str().unwrap_or("").to_string();
                        if !tool_id.is_empty() {
                            tool_results.push(json!({
                                "type": "tool_result",
                                "tool_use_id": tool_id,
                                "content": "Cancelled by user"
                            }));
                        }
                    }
                    cancelled = true;
                } else if tool_uses.len() == 1 {
                    // Single tool — run inline with delta streaming + cancellation
                    let tool_use = &tool_uses[0];
                    let tool_id = tool_use["id"].as_str().unwrap_or("").to_string();
                    let tool_name = tool_use["name"].as_str().unwrap_or("").to_string();
                    let input = tool_use["input"].clone();

                    if !tool_id.is_empty() && !tool_name.is_empty() {
                        let result = match tools.read().await.get(&tool_name).cloned() {
                            Some(tool) => {
                                let (tx_d, mut rx_d) = tokio::sync::mpsc::unbounded_channel::<String>();
                                let tx_k = tx.clone();
                                let t_id = tool_id.clone();
                                tokio::spawn(async move {
                                    while let Some(msg) = rx_d.recv().await {
                                        let _ = tx_k.send(StreamEvent::ToolResultDelta {
                                            tool_id: t_id.clone(),
                                            delta: msg,
                                        });
                                    }
                                });

                                tokio::select! {
                                    res = tool.execute(input, crate::ToolContext { tx_delta: Some(tx_d), tx_events: Some(tx.clone()), watcher_exit_path: watcher_exit_path.clone() }) => {
                                        match res {
                                            Ok(output) => output,
                                            Err(e) => format!("Tool execution failed: {}", e),
                                        }
                                    }
                                    _ = cancel.cancelled() => {
                                        cancelled = true;
                                        "Cancelled by user".to_string()
                                    }
                                }
                            }
                            None => format!("Unknown tool: {}", tool_name),
                        };

                        let _ = tx.send(StreamEvent::ToolResult {
                            tool_id: tool_id.clone(),
                            result: result.clone(),
                        });

                        tool_results.push(json!({
                            "type": "tool_result",
                            "tool_use_id": tool_id,
                            "content": Self::truncate_tool_result(&result)
                        }));
                    }
                } else {
                    // Multiple tools — run in parallel with JoinSet
                    // Delta streaming is per-tool so each gets its own channel
                    let mut join_set = tokio::task::JoinSet::new();

                    for tool_use in &tool_uses {
                        let tool_id = tool_use["id"].as_str().unwrap_or("").to_string();
                        let tool_name = tool_use["name"].as_str().unwrap_or("").to_string();
                        let input = tool_use["input"].clone();

                        if tool_id.is_empty() || tool_name.is_empty() {
                            continue;
                        }

                        let tool = tools.read().await.get(&tool_name).cloned();
                        let tx_stream = tx.clone();
                        let cancel_token = cancel.clone();
                        let exit_path = watcher_exit_path.clone();

                        join_set.spawn(async move {
                            let result = match tool {
                                Some(t) => {
                                    let (tx_d, mut rx_d) = tokio::sync::mpsc::unbounded_channel::<String>();
                                    let tx_k = tx_stream.clone();
                                    let t_id = tool_id.clone();
                                    tokio::spawn(async move {
                                        while let Some(msg) = rx_d.recv().await {
                                            let _ = tx_k.send(StreamEvent::ToolResultDelta {
                                                tool_id: t_id.clone(),
                                                delta: msg,
                                            });
                                        }
                                    });

                                    tokio::select! {
                                        res = t.execute(input, crate::ToolContext { tx_delta: Some(tx_d), tx_events: Some(tx_stream.clone()), watcher_exit_path: exit_path.clone() }) => {
                                            match res {
                                                Ok(output) => (false, output),
                                                Err(e) => (false, format!("Tool execution failed: {}", e)),
                                            }
                                        }
                                        _ = cancel_token.cancelled() => {
                                            (true, "Cancelled by user".to_string())
                                        }
                                    }
                                }
                                None => (false, format!("Unknown tool: {}", tool_name)),
                            };

                            let _ = tx_stream.send(StreamEvent::ToolResult {
                                tool_id: tool_id.clone(),
                                result: result.1.clone(),
                            });

                            (tool_id, result.0, result.1)
                        });
                    }

                    // Collect results
                    let mut results_map = std::collections::HashMap::new();
                    while let Some(res) = join_set.join_next().await {
                        match res {
                            Ok((tool_id, was_cancelled, result)) => {
                                if was_cancelled { cancelled = true; }
                                results_map.insert(tool_id, result);
                            }
                            Err(e) => {
                                tracing::error!("Parallel tool task panicked: {}", e);
                            }
                        }
                    }

                    // Build tool_results in original order
                    for tool_use in &tool_uses {
                        if let Some(tool_id) = tool_use["id"].as_str() {
                            let result = results_map.remove(tool_id)
                                .unwrap_or_else(|| "Cancelled by user".to_string());
                            tool_results.push(json!({
                                "type": "tool_result",
                                "tool_use_id": tool_id,
                                "content": Self::truncate_tool_result(&result)
                            }));
                        }
                    }
                }

                // Add tool results to conversation — always, so the assistant's tool_use
                // blocks have matching tool_result blocks even on cancellation.
                messages.push(json!({
                    "role": "user",
                    "content": tool_results
                }));

                if cancelled {
                    // Send final history on cancellation so session can be saved
                    let _ = tx.send(StreamEvent::MessageHistory(messages));
                    return Ok(());
                }

                // Check for steering messages between tool rounds.
                // These get injected as user messages before the next LLM call,
                // allowing the user to redirect the agent mid-work.
                Self::drain_steering(&mut steering_rx, &mut messages, &tx);

                // Continue the loop to get Claude's response with tool results
            } else {
                let _ = tx.send(StreamEvent::MessageHistory(messages));
                return Err(RuntimeError::Tool("Invalid response format".to_string()));
            }
        }
    }

    #[allow(dead_code)]
    pub async fn call_api_stream(&self, messages: &[Value], tx: mpsc::UnboundedSender<StreamEvent>) -> Result<Value> {
        let tools_snapshot = self.tools.read().await.clone();
        Self::call_api_stream_inner(&self.auth, &self.client, &self.model, &tools_snapshot, &self.system_prompt, self.thinking_budget, messages, tx, &CancellationToken::new()).await
    }

    /// Static inner version — used by both `call_api_stream` (instance) and
    /// `run_stream_internal` (spawned task) so there's one implementation.
    async fn call_api_stream_inner(
        auth: &Arc<RwLock<AuthState>>,
        client: &Client,
        model: &str,
        tools: &ToolRegistry,
        system_prompt: &Option<String>,
        thinking_budget: u32,
        messages: &[Value],
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        // Read auth state for this API call
        let (auth_token, auth_type) = {
            let a = auth.read().await;
            (a.auth_token.clone(), a.auth_type.clone())
        };

        let auth_header = if auth_type == "oauth" {
            ("authorization", format!("Bearer {}", auth_token))
        } else {
            ("x-api-key", auth_token.clone())
        };
        
        tracing::info!(model = %model, "Starting API request");
        let mut request = client
            .post("https://api.anthropic.com/v1/messages")
            .header(auth_header.0, auth_header.1)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json");
        
        // Add standard beta headers based on auth type
        if auth_type == "oauth" {
            request = request.header("anthropic-beta", "claude-code-20250219,oauth-2025-04-20");
        }
        
        // We no longer strip_old_thinking or microcompact here! 
        // Modifying historical messages breaks Anthropic's exact-prefix prompt cache.
        // Instead, we leave historical messages fully intact to achieve a ~95% cache hit rate.
        let mut cleaned_messages = messages.to_vec();
        Self::annotate_cache_breakpoint(&mut cleaned_messages);

        let mut body = json!({
            "model": model,
            "max_tokens": Self::max_tokens_for_model(model),
            "messages": cleaned_messages,
            "tools": &*tools.tools_schema(),
            "stream": true,
            "thinking": {
                "type": "enabled",
                "budget_tokens": thinking_budget,
                "display": "summarized"
            }
        });

        // Prompt caching: mark the last tool so all tool schemas are cached
        if let Some(tool_list) = body["tools"].as_array_mut() {
            if let Some(last_tool) = tool_list.last_mut() {
                last_tool["cache_control"] = json!({"type": "ephemeral"});
            }
        }
        
        if auth_type == "oauth" {
            let mut system_blocks = vec![
                json!({"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."}),
                json!({"type": "text", "text": "You are a helpful AI assistant with access to tools. Use them when needed."}),
            ];
            if let Some(ref prompt) = system_prompt {
                system_blocks.push(json!({"type": "text", "text": prompt}));
            }
            // Prompt caching: mark the last system block so entire system prompt is cached
            if let Some(last) = system_blocks.last_mut() {
                last["cache_control"] = json!({"type": "ephemeral"});
            }
            body["system"] = json!(system_blocks);
        } else if let Some(ref prompt) = system_prompt {
            body["system"] = json!([
                {"type": "text", "text": prompt, "cache_control": {"type": "ephemeral"}}
            ]);
        }

        tracing::trace!("Outgoing API Request Payload:\n{}", serde_json::to_string_pretty(&body).unwrap_or_default());
        let response = request.json(&body).send().await?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(RuntimeError::Tool(format!("API Error: {}", error_text)));
        }
        
        let mut stream = response.bytes_stream();
        tracing::debug!("Stream opened");
        let mut accumulated_content: Vec<Value> = Vec::new();
        let mut current_text = String::new();

        // Tool use accumulation state
        let mut current_tool_name = String::new();
        let mut current_tool_id = String::new();
        let mut current_tool_input_json = String::new();
        let mut in_tool_use = false;

        // Thinking accumulation state
        let mut current_thinking = String::new();
        let mut current_thinking_signature = String::new();
        let mut in_thinking = false;

        // SSE can split across chunk boundaries, so buffer partial lines
        let mut line_buffer = String::new();

        while let Some(chunk) = stream.next().await {
            if cancel.is_cancelled() {
                break;
            }
            let chunk = chunk?;
            let chunk_str = String::from_utf8_lossy(&chunk);
            line_buffer.push_str(&chunk_str);

            // Process complete lines from the buffer
            while let Some(newline_pos) = line_buffer.find('\n') {
                let line = line_buffer[..newline_pos].trim_end().to_string();
                line_buffer.drain(..newline_pos + 1);

                if !line.starts_with("data: ") {
                    continue;
                }

                let data_part = &line[6..];
                if data_part.trim() == "[DONE]" {
                    continue;
                }

                let event = match serde_json::from_str::<Value>(data_part) {
                    Ok(e) => e,
                    Err(_) => continue,
                };

                match event["type"].as_str() {
                    Some("content_block_start") => {
                        if let Some(content_block) = event.get("content_block") {
                            match content_block["type"].as_str() {
                                Some("thinking") => {
                                    current_thinking.clear();
                                    current_thinking_signature.clear();
                                    in_thinking = true;
                                }
                                Some("tool_use") => {
                                    // Start accumulating a tool_use block
                                    current_tool_name = content_block["name"].as_str().unwrap_or("").to_string();
                                    current_tool_id = content_block["id"].as_str().unwrap_or("").to_string();
                                    current_tool_input_json.clear();
                                    in_tool_use = true;
                                    let _ = tx.send(StreamEvent::ToolUseStart(current_tool_name.clone()));
                                }
                                Some("text") => {
                                    if !current_text.is_empty() {
                                        accumulated_content.push(json!({
                                            "type": "text",
                                            "text": current_text
                                        }));
                                        current_text.clear();
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Some("content_block_delta") => {
                        if let Some(delta) = event.get("delta") {
                            match delta["type"].as_str() {
                                Some("text_delta") => {
                                    if let Some(text) = delta["text"].as_str() {
                                        current_text.push_str(text);
                                        let _ = tx.send(StreamEvent::Text(text.to_string()));
                                    }
                                }
                                Some("thinking_delta") => {
                                    // Anthropic sends thinking text in delta.thinking
                                    if let Some(text) = delta["thinking"].as_str() {
                                        current_thinking.push_str(text);
                                        let _ = tx.send(StreamEvent::Thinking(text.to_string()));
                                    }
                                }
                                Some("signature_delta") => {
                                    if let Some(sig) = delta["signature"].as_str() {
                                        current_thinking_signature = sig.to_string();
                                    }
                                }
                                Some("input_json_delta") => {
                                    if let Some(json_chunk) = delta["partial_json"].as_str() {
                                        current_tool_input_json.push_str(json_chunk);
                                        let _ = tx.send(StreamEvent::ToolUseDelta(json_chunk.to_string()));
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Some("content_block_stop") => {
                        if in_thinking {
                            // Flush thinking block with signature so it's echoed back in tool loops
                            accumulated_content.push(json!({
                                "type": "thinking",
                                "thinking": current_thinking,
                                "signature": current_thinking_signature
                            }));
                            in_thinking = false;
                        } else if in_tool_use {
                            // Parse the accumulated JSON input
                            let input: Value = if current_tool_input_json.trim().is_empty() {
                                json!({})
                            } else {
                                serde_json::from_str(&current_tool_input_json).unwrap_or(json!({}))
                            };

                            accumulated_content.push(json!({
                                "type": "tool_use",
                                "id": current_tool_id,
                                "name": current_tool_name,
                                "input": input
                            }));

                            // Emit the tool_use to the UI as soon as it's fully parsed,
                            // so the call appears during the assistant's stream — before
                            // we hand off to the tool executor. Without this the call
                            // only becomes visible immediately prior to its result.
                            let _ = tx.send(StreamEvent::ToolUse {
                                tool_name: current_tool_name.clone(),
                                tool_id: current_tool_id.clone(),
                                input: input.clone(),
                            });

                            in_tool_use = false;
                        } else if !current_text.is_empty() {
                            // Flush text block so ordering is preserved
                            accumulated_content.push(json!({
                                "type": "text",
                                "text": current_text
                            }));
                            current_text.clear();
                        }
                    }
                    Some("message_delta") => {
                        if let Some(usage) = event.get("usage") {
                            let input_t = usage["input_tokens"].as_u64().unwrap_or(0);
                            let output_t = usage["output_tokens"].as_u64().unwrap_or(0);
                            let cache_read = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
                            let cache_create = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                            if input_t > 0 || output_t > 0 || cache_read > 0 || cache_create > 0 {
                                tracing::debug!("Token Usage: {} input | {} output | {} cache_read | {} cache_create", input_t, output_t, cache_read, cache_create);
                                let _ = tx.send(StreamEvent::Usage {
                                    input_tokens: input_t,
                                    output_tokens: output_t,
                                    cache_read_input_tokens: cache_read,
                                    cache_creation_input_tokens: cache_create,
                                    model: None,
                                });
                            }
                        }
                    }
                    Some("message_start") => {
                        if let Some(msg) = event.get("message") {
                            if let Some(usage) = msg.get("usage") {
                                let input_t = usage["input_tokens"].as_u64().unwrap_or(0);
                                let output_t = usage["output_tokens"].as_u64().unwrap_or(0);
                                let cache_read = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
                                let cache_create = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                                if input_t > 0 || output_t > 0 || cache_read > 0 || cache_create > 0 {
                                    tracing::debug!("Token Usage: {} input | {} output | {} cache_read | {} cache_create", input_t, output_t, cache_read, cache_create);
                                    let _ = tx.send(StreamEvent::Usage {
                                        input_tokens: input_t,
                                        output_tokens: output_t,
                                        cache_read_input_tokens: cache_read,
                                        cache_creation_input_tokens: cache_create,
                                        model: None,
                                    });
                                }
                            }
                        }
                    }
                    Some("message_stop") => {}
                    _ => {}
                }
            }
        }

        // Process any remaining data in line_buffer (final line without trailing newline)
        let remaining = line_buffer.trim().to_string();
        if remaining.starts_with("data: ") {
            let data_part = &remaining[6..];
            if data_part.trim() != "[DONE]" {
                if let Ok(event) = serde_json::from_str::<Value>(data_part) {
                    if event["type"].as_str() == Some("content_block_stop") {
                        if in_thinking {
                            accumulated_content.push(json!({
                                "type": "thinking",
                                "thinking": current_thinking,
                                "signature": current_thinking_signature
                            }));
                        } else if in_tool_use {
                            let input: Value = if current_tool_input_json.trim().is_empty() {
                                json!({})
                            } else {
                                serde_json::from_str(&current_tool_input_json).unwrap_or(json!({}))
                            };
                            accumulated_content.push(json!({
                                "type": "tool_use",
                                "id": current_tool_id.clone(),
                                "name": current_tool_name.clone(),
                                "input": input.clone()
                            }));
                            let _ = tx.send(StreamEvent::ToolUse {
                                tool_name: current_tool_name.clone(),
                                tool_id: current_tool_id.clone(),
                                input,
                            });
                        }
                    }
                }
            }
        }

        // Return accumulated content in the expected format
        if in_thinking {
            accumulated_content.push(json!({
                "type": "thinking",
                "thinking": current_thinking,
                "signature": current_thinking_signature
            }));
        } else if in_tool_use {
            let input: Value = if current_tool_input_json.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&current_tool_input_json).unwrap_or(json!({}))
            };
            accumulated_content.push(json!({
                "type": "tool_use",
                "id": current_tool_id,
                "name": current_tool_name,
                "input": input
            }));
        } else if !current_text.is_empty() {
            accumulated_content.push(json!({
                "type": "text",
                "text": current_text
            }));
        }

        Ok(json!({
            "content": accumulated_content
        }))
    }

    // Helper methods for token optimization have been replaced 
    // by static-marker prompt caching to prevent history mutations.

    /// Drain all pending steering messages from the channel and inject them
    /// into the conversation as user messages. Returns true if any were injected.
    fn drain_steering(
        steering_rx: &mut Option<mpsc::UnboundedReceiver<String>>,
        messages: &mut Vec<Value>,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) -> bool {
        let rx = match steering_rx.as_mut() {
            Some(rx) => rx,
            None => return false,
        };

        let mut injected = false;
        while let Ok(msg) = rx.try_recv() {
            tracing::info!("Steering message injected: {}", &msg[..msg.floor_char_boundary(80.min(msg.len()))]);
            let _ = tx.send(StreamEvent::SteeringDelivered { message: msg.clone() });
            messages.push(json!({"role": "user", "content": msg}));
            injected = true;
        }
        injected
    }

    /// Annotate a cache breakpoint on the conversation prefix.
    /// To maximize cache hits, we must place stationary boundaries. Modifying an old marker
    /// breaks the cache for that prefix. We retain up to 2 conversational markers.
    fn annotate_cache_breakpoint(messages: &mut Vec<Value>) {
        let user_indices: Vec<usize> = messages.iter().enumerate()
            .filter(|(_, m)| m["role"].as_str() == Some("user"))
            .map(|(i, _)| i)
            .collect();

        if user_indices.is_empty() { return; }

        // Find existing markers
        let mut existing_markers = Vec::new();
        for &idx in &user_indices {
            if let Some(content) = messages[idx]["content"].as_array() {
                if content.last().and_then(|b| b.get("cache_control")).is_some() {
                    existing_markers.push(idx);
                }
            }
        }

        // We only place a new marker if the last one is 4+ user messages away (e.g. 4 tool loops!)
        let target_idx = user_indices[user_indices.len() - 1]; // We can just mark the latest
        let should_add = match existing_markers.last() {
            Some(&last_idx) => user_indices.len() as isize - user_indices.iter().position(|&x| x == last_idx).unwrap_or(0) as isize >= 4,
            None => true,
        };

        if should_add && !existing_markers.contains(&target_idx) {
            existing_markers.push(target_idx);

            // Convert raw string content to block array to allow adding cache_control
            if messages[target_idx]["content"].is_string() {
                if let Some(text) = messages[target_idx]["content"].as_str() {
                    messages[target_idx]["content"] = json!([{"type": "text", "text": text}]);
                }
            }

            if let Some(content) = messages[target_idx]["content"].as_array_mut() {
                if let Some(last_block) = content.last_mut() {
                    last_block["cache_control"] = json!({"type": "ephemeral"});
                }
            }
        }

        // Enforce max 2 conversational markers to avoid Anthropic's 4-marker limit
        if existing_markers.len() > 2 {
            let keep = &existing_markers[existing_markers.len() - 2..];
            for (i, msg) in messages.iter_mut().enumerate() {
                if !keep.contains(&i) && msg["role"].as_str() == Some("user") {
                    if let Some(content) = msg["content"].as_array_mut() {
                        if let Some(last_block) = content.last_mut() {
                            if last_block.get("cache_control").is_some() {
                                last_block.as_object_mut().map(|obj| obj.remove("cache_control"));
                            }
                        }
                    }
                }
            }
        }
    }

    /// Truncate tool results to avoid ballooning message history.
    /// The full result is still sent to the UI — this only caps what goes into
    /// the API messages that are re-sent on every subsequent call.
    const MAX_TOOL_RESULT_CHARS: usize = 30_000;

    fn truncate_tool_result(result: &str) -> String {
        if result.len() <= Self::MAX_TOOL_RESULT_CHARS {
            return result.to_string();
        }
        let truncated: String = result.chars().take(Self::MAX_TOOL_RESULT_CHARS).collect();
        format!("{}\n\n[truncated — {} total chars, showing first {}]",
            truncated, result.len(), Self::MAX_TOOL_RESULT_CHARS)
    }
    
    async fn call_api(&self, messages: &[Value]) -> Result<Value> {
        // Read auth state
        let (auth_token, auth_type) = {
            let a = self.auth.read().await;
            (a.auth_token.clone(), a.auth_type.clone())
        };

        let auth_header = if auth_type == "oauth" {
            ("authorization", format!("Bearer {}", auth_token))
        } else {
            ("x-api-key", auth_token.clone())
        };
        
        let mut request = self.client
            .post("https://api.anthropic.com/v1/messages")
            .header(auth_header.0, auth_header.1)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json");
        
        // Add standard beta headers based on auth type
        if auth_type == "oauth" {
            request = request.header("anthropic-beta", "claude-code-20250219,oauth-2025-04-20");
        }
        
        // Avoid modifying past messages to maintain a 100% stable prefix for Anthropic caching.
        let mut cleaned_messages = messages.to_vec();
        Self::annotate_cache_breakpoint(&mut cleaned_messages);

        let mut body = json!({
            "model": self.model,
            "max_tokens": Self::max_tokens_for_model(&self.model),
            "messages": cleaned_messages,
            "tools": &*self.tools.read().await.tools_schema(),
            "thinking": {
                "type": "enabled",
                "budget_tokens": self.thinking_budget,
                "display": "summarized"
            }
        });

        // Prompt caching: mark the last tool so all tool schemas are cached
        if let Some(tools) = body["tools"].as_array_mut() {
            if let Some(last_tool) = tools.last_mut() {
                last_tool["cache_control"] = json!({"type": "ephemeral"});
            }
        }
        
        if auth_type == "oauth" {
            let mut system_blocks = vec![
                json!({"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."}),
                json!({"type": "text", "text": "You are a helpful AI assistant with access to tools. Use them when needed."}),
            ];
            if let Some(ref prompt) = self.system_prompt {
                system_blocks.push(json!({"type": "text", "text": prompt}));
            }
            // Prompt caching: mark the last system block so entire system prompt is cached
            if let Some(last) = system_blocks.last_mut() {
                last["cache_control"] = json!({"type": "ephemeral"});
            }
            body["system"] = json!(system_blocks);
        } else if let Some(ref prompt) = self.system_prompt {
            body["system"] = json!([
                {"type": "text", "text": prompt, "cache_control": {"type": "ephemeral"}}
            ]);
        }

        let response = request.json(&body).send().await?;
        let json: Value = response.json().await?;
        
        // Debug: print the full response on error
        if json["error"].is_object() {
            eprintln!("API Error Response: {}", serde_json::to_string_pretty(&json).unwrap_or_default());
            if let Some(error_type) = json["error"]["type"].as_str() {
                return Err(RuntimeError::Tool(format!("API Error: {}", error_type)));
            }
        }
        
        Ok(json)
    }
}

impl Clone for Runtime {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            auth: Arc::clone(&self.auth),
            model: self.model.clone(),
            tools: self.tools.clone(),
            system_prompt: self.system_prompt.clone(),
            thinking_budget: self.thinking_budget,
            watcher_exit_path: self.watcher_exit_path.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_max_tokens_for_model() {
        // Opus models should return 128000
        assert_eq!(Runtime::max_tokens_for_model("claude-opus-4-6"), 128000);
        assert_eq!(Runtime::max_tokens_for_model("opus-something"), 128000);
        
        // Non-opus models should return 64000
        assert_eq!(Runtime::max_tokens_for_model("claude-sonnet-4-20250514"), 64000);
        assert_eq!(Runtime::max_tokens_for_model("haiku"), 64000);
        assert_eq!(Runtime::max_tokens_for_model("claude-3-haiku"), 64000);
        assert_eq!(Runtime::max_tokens_for_model("some-other-model"), 64000);
        
        // Edge cases
        assert_eq!(Runtime::max_tokens_for_model(""), 64000);
        assert_eq!(Runtime::max_tokens_for_model("OPUS"), 64000); // Case sensitive - uppercase doesn't match
        assert_eq!(Runtime::max_tokens_for_model("model-opus-end"), 128000); // Contains "opus" anywhere
    }

    #[test]
    fn test_truncate_tool_result() {
        // Short string should remain unchanged
        let short = "This is a short string.";
        assert_eq!(Runtime::truncate_tool_result(short), short);
        
        // Exactly MAX_TOOL_RESULT_CHARS should remain unchanged
        let exact = "x".repeat(Runtime::MAX_TOOL_RESULT_CHARS);
        assert_eq!(Runtime::truncate_tool_result(&exact), exact);
        
        // String longer than MAX_TOOL_RESULT_CHARS should be truncated with notice
        let too_long = "x".repeat(Runtime::MAX_TOOL_RESULT_CHARS + 1);
        let truncated = Runtime::truncate_tool_result(&too_long);
        
        // Should start with the truncated content
        assert!(truncated.starts_with(&"x".repeat(Runtime::MAX_TOOL_RESULT_CHARS)));
        
        // Should contain truncation notice with total char count
        assert!(truncated.contains("[truncated — 30001 total chars, showing first 30000]"));
        
        // Should be longer than MAX_TOOL_RESULT_CHARS (due to notice)
        assert!(truncated.len() > Runtime::MAX_TOOL_RESULT_CHARS);
        
        // Test with a much longer string
        let very_long = "a".repeat(50000);
        let truncated_very_long = Runtime::truncate_tool_result(&very_long);
        assert!(truncated_very_long.contains("[truncated — 50000 total chars, showing first 30000]"));
        assert!(truncated_very_long.starts_with(&"a".repeat(Runtime::MAX_TOOL_RESULT_CHARS)));
    }

    #[test]
    fn test_thinking_level_ranges() {
        // Test the logic by directly checking the ranges that thinking_level uses
        
        // Low range: 0..=2048
        assert_eq!(thinking_level_for_budget(0), "low");
        assert_eq!(thinking_level_for_budget(1024), "low");
        assert_eq!(thinking_level_for_budget(2048), "low");
        
        // Medium range: 2049..=4096  
        assert_eq!(thinking_level_for_budget(2049), "medium");
        assert_eq!(thinking_level_for_budget(3000), "medium");
        assert_eq!(thinking_level_for_budget(4096), "medium");
        
        // High range: 4097..=16384
        assert_eq!(thinking_level_for_budget(4097), "high");
        assert_eq!(thinking_level_for_budget(8192), "high");
        assert_eq!(thinking_level_for_budget(16384), "high");
        
        // XHigh range: _ (everything else)
        assert_eq!(thinking_level_for_budget(16385), "xhigh");
        assert_eq!(thinking_level_for_budget(32768), "xhigh");
        assert_eq!(thinking_level_for_budget(100000), "xhigh");
    }

    #[test]
    fn test_max_tool_result_chars_constant() {
        // Verify the constant value
        assert_eq!(Runtime::MAX_TOOL_RESULT_CHARS, 30000);
    }

    /// Helper function to mirror the thinking_level logic for testing
    /// since we can't easily construct a Runtime instance in tests
    fn thinking_level_for_budget(budget: u32) -> &'static str {
        match budget {
            0..=2048 => "low",
            2049..=4096 => "medium",
            4097..=16384 => "high",
            _ => "xhigh",
        }
    }
}
