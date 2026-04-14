use reqwest::Client;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use crate::{Result, RuntimeError, ToolRegistry};
use tokio::sync::{mpsc, RwLock};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::sync::CancellationToken;
use futures::stream::Stream;
use std::pin::Pin;

mod types;
mod auth;
mod api;
mod stream;
mod helpers;

pub use types::StreamEvent;
use types::AuthState;
use auth::AuthMethods;
use api::ApiMethods;
use stream::StreamMethods;
use helpers::HelperMethods;

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
        let (auth_token, auth_type, refresh_token, token_expires) = AuthMethods::get_auth_token()?;

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
    pub async fn refresh_if_needed(&self) -> Result<()> {
        AuthMethods::refresh_if_needed(Arc::clone(&self.auth), &self.client).await
    }

    pub async fn run_single(&self, prompt: &str) -> Result<String> {
        // Refresh OAuth token if expired
        self.refresh_if_needed().await?;

        let mut messages = vec![json!({"role": "user", "content": prompt})];
        
        loop {
            let response = ApiMethods::call_api(
                &self.auth,
                &self.client,
                &self.model,
                &*self.tools.read().await,
                &self.system_prompt,
                self.thinking_budget,
                &messages,
            ).await?;
            
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
                                let ctx = crate::ToolContext { tx_delta: None, tx_events: None, watcher_exit_path: self.watcher_exit_path.clone(), tool_register_tx: None };
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
                            "content": HelperMethods::truncate_tool_result(&result)
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
                                        let ctx = crate::ToolContext { tx_delta: None, tx_events: None, watcher_exit_path: exit_path, tool_register_tx: None };
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
                                "content": HelperMethods::truncate_tool_result(&result)
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
            if let Err(e) = StreamMethods::run_stream_internal(
                auth, client, model, tools, system_prompt, thinking_budget,
                messages, tx.clone(), cancel, steering_rx, watcher_exit_path,
            ).await {
                let _ = tx.send(StreamEvent::Error(e.to_string()));
            }
            let _ = tx.send(StreamEvent::Done);
        });

        Box::pin(UnboundedReceiverStream::new(rx))
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
        assert_eq!(HelperMethods::max_tokens_for_model("claude-opus-4-6"), 128000);
        assert_eq!(HelperMethods::max_tokens_for_model("opus-something"), 128000);
        
        // Non-opus models should return 64000
        assert_eq!(HelperMethods::max_tokens_for_model("claude-sonnet-4-20250514"), 64000);
        assert_eq!(HelperMethods::max_tokens_for_model("haiku"), 64000);
        assert_eq!(HelperMethods::max_tokens_for_model("claude-3-haiku"), 64000);
        assert_eq!(HelperMethods::max_tokens_for_model("some-other-model"), 64000);
        
        // Edge cases
        assert_eq!(HelperMethods::max_tokens_for_model(""), 64000);
        assert_eq!(HelperMethods::max_tokens_for_model("OPUS"), 64000); // Case sensitive - uppercase doesn't match
        assert_eq!(HelperMethods::max_tokens_for_model("model-opus-end"), 128000); // Contains "opus" anywhere
    }

    #[test]
    fn test_truncate_tool_result() {
        // Short string should remain unchanged
        let short = "This is a short string.";
        assert_eq!(HelperMethods::truncate_tool_result(short), short);
        
        // Exactly MAX_TOOL_RESULT_CHARS should remain unchanged
        let exact = "x".repeat(30000); // HelperMethods::MAX_TOOL_RESULT_CHARS is 30000
        assert_eq!(HelperMethods::truncate_tool_result(&exact), exact);
        
        // String longer than MAX_TOOL_RESULT_CHARS should be truncated with notice
        let too_long = "x".repeat(30001);
        let truncated = HelperMethods::truncate_tool_result(&too_long);
        
        // Should start with the truncated content
        assert!(truncated.starts_with(&"x".repeat(30000)));
        
        // Should contain truncation notice with total char count
        assert!(truncated.contains("[truncated — 30001 total chars, showing first 30000]"));
        
        // Should be longer than MAX_TOOL_RESULT_CHARS (due to notice)
        assert!(truncated.len() > 30000);
        
        // Test with a much longer string
        let very_long = "a".repeat(50000);
        let truncated_very_long = HelperMethods::truncate_tool_result(&very_long);
        assert!(truncated_very_long.contains("[truncated — 50000 total chars, showing first 30000]"));
        assert!(truncated_very_long.starts_with(&"a".repeat(30000)));
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
        assert_eq!(super::helpers::HelperMethods::MAX_TOOL_RESULT_CHARS, 30000);
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