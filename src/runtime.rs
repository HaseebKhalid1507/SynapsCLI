use reqwest::Client;
use serde_json::{json, Value};
use serde::{Deserialize};
use std::path::Path;
use crate::{Result, RuntimeError, ToolRegistry};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio::sync::mpsc;
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
    ToolUse {
        tool_name: String,
        tool_id: String,
        input: Value,
    },
    ToolResult {
        tool_id: String,
        result: String,
    },
    /// Full message history after the tool loop completes, for multi-turn context
    MessageHistory(Vec<Value>),
    Usage { input_tokens: u64, output_tokens: u64 },
    Done,
    Error(String),
}

pub struct Runtime {
    client: Client,
    auth_token: String,
    auth_type: String,
    /// Stored refresh token for auto-refresh (only set when auth_type == "oauth")
    refresh_token: Option<String>,
    /// Token expiry in epoch millis (only set when auth_type == "oauth")
    token_expires: Option<u64>,
    model: String,
    tools: ToolRegistry,
    system_prompt: Option<String>,
    thinking_budget: u32,
}

impl Runtime {
    pub async fn new() -> Result<Self> {
        let (auth_token, auth_type, refresh_token, token_expires) = Self::get_auth_token()?;

        Ok(Runtime {
            client: Client::new(),
            auth_token,
            auth_type,
            refresh_token,
            token_expires,
            model: "claude-opus-4-6".to_string(),
            tools: ToolRegistry::new(),
            system_prompt: None,
            thinking_budget: 4096,
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

    pub fn model(&self) -> &str {
        &self.model
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
    pub async fn refresh_if_needed(&mut self) -> Result<()> {
        if self.auth_type != "oauth" {
            return Ok(());
        }

        // Fast path: if our in-memory token isn't expired, skip the lock entirely
        let in_memory_expired = match self.token_expires {
            Some(exp) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;
                now >= exp
            }
            None => false,
        };

        if !in_memory_expired {
            return Ok(());
        }

        eprintln!("\x1b[2m  ↻ checking / refreshing OAuth token...\x1b[0m");

        // Slow path: delegate to auth.rs which handles locking, re-read,
        // conditional refresh, and persistence.
        let creds = crate::auth::ensure_fresh_token()
            .await
            .map_err(|e| RuntimeError::Tool(format!(
                "Token refresh failed: {}. Run `login` to re-authenticate.", e
            )))?;

        // Update in-memory state with whatever ensure_fresh_token returned
        // (either a freshly refreshed token, or the current one if another
        // instance beat us to the refresh).
        self.auth_token = creds.access;
        self.refresh_token = Some(creds.refresh);
        self.token_expires = Some(creds.expires);

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
        let home = std::env::var("HOME").unwrap();
        let auth_path = Path::new(&home).join(".synaps-cli/auth.json");

        if auth_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&auth_path) {
                if let Ok(auth) = serde_json::from_str::<PiAuth>(&content) {
                    let creds = &auth.anthropic;
                    if creds.auth_type == "oauth" && creds.access.is_some() {
                        return Ok((
                            creds.access.as_ref().unwrap().clone(),
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
        
        Err(RuntimeError::Tool("No Anthropic credentials found. Run `login` to authenticate.".to_string()))
    }

    pub async fn run_single(&mut self, prompt: &str) -> Result<String> {
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
                
                // Execute tools and add results
                let mut tool_results = Vec::new();
                
                for tool_use in tool_uses {
                    if let (Some(tool_name), Some(tool_id)) = (
                        tool_use["name"].as_str(),
                        tool_use["id"].as_str()
                    ) {
                        let input = &tool_use["input"];
                        
                        let result = match self.tools.get(tool_name) {
                            Some(tool) => {
                                match tool.execute(input.clone()).await {
                                    Ok(output) => output,
                                    Err(e) => format!("Tool execution failed: {}", e),
                                }
                            }
                            None => format!("Unknown tool: {}", tool_name),
                        };
                        
                        tool_results.push(json!({
                            "type": "tool_result",
                            "tool_use_id": tool_id,
                            "content": result
                        }));
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

    pub async fn run_stream(&mut self, prompt: String, cancel: CancellationToken) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send>> {
        self.run_stream_with_messages(vec![json!({"role": "user", "content": prompt})], cancel).await
    }

    pub async fn run_stream_with_messages(&mut self, messages: Vec<Value>, cancel: CancellationToken) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send>> {
        let (tx, rx) = mpsc::unbounded_channel();

        // Refresh OAuth token if expired BEFORE cloning into the task,
        // so all subsequent API calls in the loop use the fresh token.
        if let Err(e) = self.refresh_if_needed().await {
            let _ = tx.send(StreamEvent::Error(e.to_string()));
            let _ = tx.send(StreamEvent::Done);
            return Box::pin(UnboundedReceiverStream::new(rx));
        }

        let runtime = self.clone();

        tokio::spawn(async move {
            if let Err(e) = runtime.run_stream_internal(messages, tx.clone(), cancel).await {
                let _ = tx.send(StreamEvent::Error(e.to_string()));
            }
            let _ = tx.send(StreamEvent::Done);
        });

        Box::pin(UnboundedReceiverStream::new(rx))
    }

    async fn run_stream_internal(&self, initial_messages: Vec<Value>, tx: mpsc::UnboundedSender<StreamEvent>, cancel: CancellationToken) -> Result<()> {
        let mut messages = initial_messages;

        loop {
            // Check for cancellation before each API call
            if cancel.is_cancelled() {
                let _ = tx.send(StreamEvent::MessageHistory(messages));
                return Ok(());
            }

            let response = match self.call_api_stream(&messages, tx.clone()).await {
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

                // If no tool uses, we're done
                if tool_uses.is_empty() {
                    let _ = tx.send(StreamEvent::MessageHistory(messages));
                    return Ok(());
                }

                // Execute tools and add results. We must always produce a tool_result for
                // every tool_use we just pushed onto the assistant message — otherwise the
                // next API call will fail with "tool_use ids were found without tool_result
                // blocks". On cancellation we synthesize a "Cancelled by user" result for any
                // remaining tools so message history stays valid.
                let mut tool_results = Vec::new();
                let mut cancelled = false;

                for tool_use in tool_uses {
                    let tool_id = tool_use["id"].as_str().unwrap_or("").to_string();
                    let tool_name = tool_use["name"].as_str().unwrap_or("").to_string();
                    let input = tool_use["input"].clone();

                    if tool_id.is_empty() || tool_name.is_empty() {
                        continue;
                    }

                    // If cancellation already happened, fill in remaining slots with a
                    // synthetic cancelled result rather than breaking out of the loop.
                    if cancelled || cancel.is_cancelled() {
                        cancelled = true;
                        tool_results.push(json!({
                            "type": "tool_result",
                            "tool_use_id": tool_id,
                            "content": "Cancelled by user"
                        }));
                        continue;
                    }

                    // Note: StreamEvent::ToolUse is already emitted inside call_api_stream
                    // the moment the tool_use content block closes, so the UI can render
                    // the call as part of the assistant's stream instead of only just
                    // before the result lands.

                    let result = match self.tools.get(&tool_name) {
                        Some(tool) => {
                            // Race tool execution against cancellation
                            tokio::select! {
                                res = tool.execute(input.clone()) => {
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

                    // Send tool result event
                    let _ = tx.send(StreamEvent::ToolResult {
                        tool_id: tool_id.clone(),
                        result: result.clone(),
                    });

                    tool_results.push(json!({
                        "type": "tool_result",
                        "tool_use_id": tool_id,
                        "content": result
                    }));
                }

                // Add tool results to conversation — always, so the assistant's tool_use
                // blocks have matching tool_result blocks even on cancellation.
                messages.push(json!({
                    "role": "user",
                    "content": tool_results
                }));

                // Send updated history after each tool loop iteration
                let _ = tx.send(StreamEvent::MessageHistory(messages.clone()));

                if cancelled {
                    return Ok(());
                }

                // Continue the loop to get Claude's response with tool results
            } else {
                let _ = tx.send(StreamEvent::MessageHistory(messages));
                return Err(RuntimeError::Tool("Invalid response format".to_string()));
            }
        }
    }

    async fn call_api_stream(&self, messages: &[Value], tx: mpsc::UnboundedSender<StreamEvent>) -> Result<Value> {
        let auth_header = if self.auth_type == "oauth" {
            ("authorization", format!("Bearer {}", self.auth_token))
        } else {
            ("x-api-key", self.auth_token.clone())
        };
        
        let mut request = self.client
            .post("https://api.anthropic.com/v1/messages")
            .header(auth_header.0, auth_header.1)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json");
        
        // Add standard beta headers based on auth type
        if self.auth_type == "oauth" {
            request = request.header("anthropic-beta", "claude-code-20250219,oauth-2025-04-20");
        }
        
        let mut body = json!({
            "model": self.model,
            "max_tokens": 128000,
            "messages": messages,
            "tools": self.tools.tools_schema(),
            "stream": true,
            "thinking": {
                "type": "enabled",
                "budget_tokens": self.thinking_budget,
                "display": "summarized"
            }
        });
        
        if self.auth_type == "oauth" {
            let mut system_blocks = vec![
                json!({"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude.", "cache_control": {"type": "ephemeral"}}),
                json!({"type": "text", "text": "You are a helpful AI assistant with access to tools. Use them when needed."}),
            ];
            if let Some(ref prompt) = self.system_prompt {
                system_blocks.push(json!({"type": "text", "text": prompt}));
            }
            body["system"] = json!(system_blocks);
        } else if let Some(ref prompt) = self.system_prompt {
            body["system"] = json!([
                {"type": "text", "text": prompt}
            ]);
        }

        let response = request.json(&body).send().await?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(RuntimeError::Tool(format!("API Error: {}", error_text)));
        }
        
        let mut stream = response.bytes_stream();
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
            let chunk = chunk?;
            let chunk_str = String::from_utf8_lossy(&chunk);
            line_buffer.push_str(&chunk_str);

            // Process complete lines from the buffer
            while let Some(newline_pos) = line_buffer.find('\n') {
                let line = line_buffer[..newline_pos].trim_end().to_string();
                line_buffer = line_buffer[newline_pos + 1..].to_string();

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
                            if input_t > 0 || output_t > 0 {
                                let _ = tx.send(StreamEvent::Usage { input_tokens: input_t, output_tokens: output_t });
                            }
                        }
                    }
                    Some("message_start") => {
                        if let Some(msg) = event.get("message") {
                            if let Some(usage) = msg.get("usage") {
                                let input_t = usage["input_tokens"].as_u64().unwrap_or(0);
                                let output_t = usage["output_tokens"].as_u64().unwrap_or(0);
                                if input_t > 0 || output_t > 0 {
                                    let _ = tx.send(StreamEvent::Usage { input_tokens: input_t, output_tokens: output_t });
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
        if !current_text.is_empty() {
            accumulated_content.push(json!({
                "type": "text",
                "text": current_text
            }));
        }

        Ok(json!({
            "content": accumulated_content
        }))
    }
    
    async fn call_api(&self, messages: &[Value]) -> Result<Value> {
        let auth_header = if self.auth_type == "oauth" {
            ("authorization", format!("Bearer {}", self.auth_token))
        } else {
            ("x-api-key", self.auth_token.clone())
        };
        
        let mut request = self.client
            .post("https://api.anthropic.com/v1/messages")
            .header(auth_header.0, auth_header.1)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json");
        
        // Add standard beta headers based on auth type
        if self.auth_type == "oauth" {
            request = request.header("anthropic-beta", "claude-code-20250219,oauth-2025-04-20");
        }
        
        let mut body = json!({
            "model": self.model,
            "max_tokens": 128000,
            "messages": messages,
            "tools": self.tools.tools_schema(),
            "thinking": {
                "type": "enabled",
                "budget_tokens": self.thinking_budget,
                "display": "summarized"
            }
        });
        
        if self.auth_type == "oauth" {
            let mut system_blocks = vec![
                json!({"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude.", "cache_control": {"type": "ephemeral"}}),
                json!({"type": "text", "text": "You are a helpful AI assistant with access to tools. Use them when needed."}),
            ];
            if let Some(ref prompt) = self.system_prompt {
                system_blocks.push(json!({"type": "text", "text": prompt}));
            }
            body["system"] = json!(system_blocks);
        } else if let Some(ref prompt) = self.system_prompt {
            body["system"] = json!([
                {"type": "text", "text": prompt}
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
            auth_token: self.auth_token.clone(),
            auth_type: self.auth_type.clone(),
            refresh_token: self.refresh_token.clone(),
            token_expires: self.token_expires,
            model: self.model.clone(),
            tools: self.tools.clone(),
            system_prompt: self.system_prompt.clone(),
            thinking_budget: self.thinking_budget,
        }
    }
}
