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

/// The core runtime — manages API communication, tool execution, authentication,
/// and streaming for all SynapsCLI binaries (chat, chatui, server, agent, watcher).
pub struct Runtime {
    client: Client,
    auth: Arc<RwLock<AuthState>>,
    model: String,
    tools: Arc<RwLock<ToolRegistry>>,
    system_prompt: Option<String>,
    thinking_budget: u32,
    /// User override for context window size (tokens). When set, takes
    /// precedence over the model's auto-detected window from
    /// `models::context_window_for_model`. Lets users cap context at e.g.
    /// 200k even on models that natively support 1M.
    context_window_override: Option<u64>,
    /// Path for watcher_exit tool to write handoff state (agent mode only)
    pub watcher_exit_path: Option<PathBuf>,
    // New configurable fields
    max_tool_output: usize,
    bash_timeout: u64,
    bash_max_timeout: u64,
    subagent_timeout: u64,
    api_retries: u32,
    session_manager: std::sync::Arc<crate::tools::shell::SessionManager>,
    // Held to keep the reaper task alive for the Runtime's lifetime; never read directly.
    #[allow(dead_code)]
    reaper_handle: Option<tokio::task::JoinHandle<()>>,
    #[allow(dead_code)]
    reaper_cancel: Option<tokio_util::sync::CancellationToken>,
}

impl Runtime {
    pub async fn new() -> Result<Self> {
        let (auth_token, auth_type, refresh_token, token_expires) = AuthMethods::get_auth_token()?;

        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| RuntimeError::Config(format!("Failed to build HTTP client: {}", e)))?;

        let session_manager = {
            let config = crate::tools::shell::ShellConfig::default();
            crate::tools::shell::SessionManager::new(config)
        };

        // Start the idle session reaper
        let mgr = session_manager.clone();
        let cancel = tokio_util::sync::CancellationToken::new();
        let reaper_handle = crate::tools::shell::session::start_reaper(mgr, cancel.clone());

        Ok(Runtime {
            client,
            auth: Arc::new(RwLock::new(AuthState {
                auth_token,
                auth_type,
                refresh_token,
                token_expires,
            })),
            model: crate::models::default_model().to_string(),
            tools: Arc::new(RwLock::new(ToolRegistry::new())),
            system_prompt: None,
            thinking_budget: 4096,
            context_window_override: None,
            watcher_exit_path: None,
            max_tool_output: 30000,
            bash_timeout: 30,
            bash_max_timeout: 300,
            subagent_timeout: 300,
            api_retries: 3,
            session_manager,
            reaper_handle: Some(reaper_handle),
            reaper_cancel: Some(cancel),
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

    pub fn set_context_window(&mut self, window: Option<u64>) {
        self.context_window_override = window;
    }

    /// Effective context window for the current model — user override if set,
    /// otherwise the model's native window from `models::context_window_for_model`.
    pub fn context_window(&self) -> u64 {
        self.context_window_override
            .unwrap_or_else(|| crate::models::context_window_for_model(&self.model))
    }

    /// Apply a parsed config file to this runtime (model, thinking budget, etc.)
    pub fn apply_config(&mut self, config: &crate::config::SynapsConfig) {
        if let Some(ref model) = config.model {
            self.set_model(model.clone());
        }
        if let Some(budget) = config.thinking_budget {
            self.set_thinking_budget(budget);
        }
        self.context_window_override = config.context_window;
        self.max_tool_output = config.max_tool_output;
        self.bash_timeout = config.bash_timeout;
        self.bash_max_timeout = config.bash_max_timeout;
        self.subagent_timeout = config.subagent_timeout;
        self.api_retries = config.api_retries;
    }

    pub fn thinking_budget(&self) -> u32 {
        self.thinking_budget
    }

    pub fn max_tool_output(&self) -> usize {
        self.max_tool_output
    }

    pub fn bash_timeout(&self) -> u64 {
        self.bash_timeout
    }

    pub fn bash_max_timeout(&self) -> u64 {
        self.bash_max_timeout
    }

    pub fn subagent_timeout(&self) -> u64 {
        self.subagent_timeout
    }

    pub fn api_retries(&self) -> u32 {
        self.api_retries
    }

    pub fn set_max_tool_output(&mut self, v: usize) {
        self.max_tool_output = v;
    }

    pub fn set_bash_timeout(&mut self, v: u64) {
        self.bash_timeout = v;
    }

    pub fn set_bash_max_timeout(&mut self, v: u64) {
        self.bash_max_timeout = v;
    }

    pub fn set_subagent_timeout(&mut self, v: u64) {
        self.subagent_timeout = v;
    }

    pub fn set_api_retries(&mut self, v: u32) {
        self.api_retries = v;
    }

    pub fn thinking_level(&self) -> &str {
        crate::core::models::thinking_level_for_budget(self.thinking_budget)
    }

    /// Check if the OAuth token is expired and refresh it if needed.
    pub async fn refresh_if_needed(&self) -> Result<()> {
        AuthMethods::refresh_if_needed(Arc::clone(&self.auth), &self.client).await
    }

    /// Make a simple non-streaming API call for compaction (no tools).
    ///
    /// Uses a dedicated summarization system prompt (not the user's), omits
    /// all tools, and returns the raw text response. Caller supplies the
    /// full message array including the serialized conversation.
    pub async fn compact_call(&self, messages: Vec<Value>) -> Result<String> {
        self.refresh_if_needed().await?;

        const COMPACTION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI coding assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

        ApiMethods::call_api_simple(
            &self.auth,
            &self.client,
            &self.model,
            COMPACTION_SYSTEM_PROMPT,
            self.thinking_budget,
            &messages,
            self.api_retries,
        ).await
    }

    /// Run a single prompt synchronously (non-streaming). Handles tool execution
    /// internally, looping until the model produces a final text response.
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
                self.api_retries,
                &api::ApiOptions {
                    use_1m_context: self.context_window_override == Some(1_000_000),
                },
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
                                let ctx = crate::ToolContext { 
                                    tx_delta: None, 
                                    tx_events: None, 
                                    watcher_exit_path: self.watcher_exit_path.clone(), 
                                    tool_register_tx: None, 
                                    session_manager: Some(self.session_manager.clone()),
                                    max_tool_output: self.max_tool_output,
                                    bash_timeout: self.bash_timeout,
                                    bash_max_timeout: self.bash_max_timeout,
                                    subagent_timeout: self.subagent_timeout,
                                };
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
                            "content": HelperMethods::truncate_tool_result(&result, self.max_tool_output)
                        }));
                    }
                } else {
                    // Multiple tools — run in parallel with JoinSet
                    let mut join_set = tokio::task::JoinSet::new();
                    
                    // Capture config values before spawning (can't borrow &self in 'static spawn)
                    let cfg_max_tool_output = self.max_tool_output;
                    let cfg_bash_timeout = self.bash_timeout;
                    let cfg_bash_max_timeout = self.bash_max_timeout;
                    let cfg_subagent_timeout = self.subagent_timeout;
                    let session_mgr = self.session_manager.clone();
                    
                    for tool_use in &tool_uses {
                        if let (Some(tool_name), Some(tool_id)) = (
                            tool_use["name"].as_str().map(|s| s.to_string()),
                            tool_use["id"].as_str().map(|s| s.to_string()),
                        ) {
                            let input = tool_use["input"].clone();
                            let tool = self.tools.read().await.get(&tool_name).cloned();
                            let exit_path = self.watcher_exit_path.clone();
                            let session_mgr_inner = session_mgr.clone();
                            
                            join_set.spawn(async move {
                                let result = match tool {
                                    Some(t) => {
                                        let ctx = crate::ToolContext { 
                                            tx_delta: None, 
                                            tx_events: None, 
                                            watcher_exit_path: exit_path, 
                                            tool_register_tx: None, 
                                            session_manager: Some(session_mgr_inner),
                                            max_tool_output: cfg_max_tool_output,
                                            bash_timeout: cfg_bash_timeout,
                                            bash_max_timeout: cfg_bash_max_timeout,
                                            subagent_timeout: cfg_subagent_timeout,
                                        };
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
                                "content": HelperMethods::truncate_tool_result(&result, self.max_tool_output)
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

    /// Run a prompt as a cancellable stream of [`StreamEvent`]s. Convenience wrapper
    /// around [`run_stream_with_messages`] for single-turn usage.
    pub async fn run_stream(&self, prompt: String, cancel: CancellationToken) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send>> {
        self.run_stream_with_messages(vec![json!({"role": "user", "content": prompt})], cancel, None).await
    }

    /// Run a multi-turn conversation as a cancellable stream of [`StreamEvent`]s.
    /// This is the main entry point for chat UIs and agents. Handles tool execution,
    /// API retries, and dynamic tool registration (MCP) internally.
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
        let max_tool_output = self.max_tool_output;
        let bash_timeout = self.bash_timeout;
        let bash_max_timeout = self.bash_max_timeout;
        let subagent_timeout = self.subagent_timeout;
        let api_retries = self.api_retries;
        let session_manager = self.session_manager.clone();
        // Opt into the 1M-context beta header only when the user explicitly
        // requested 1M (via context_window setting). Default 200k matches
        // Anthropic's claude-code default and gives smarter inference.
        let options = api::ApiOptions {
            use_1m_context: self.context_window_override == Some(1_000_000),
        };

        tokio::spawn(async move {
            if let Err(e) = StreamMethods::run_stream_internal(
                auth, client, model, tools, system_prompt, thinking_budget,
                messages, tx.clone(), cancel, steering_rx, watcher_exit_path,
                max_tool_output, bash_timeout, bash_max_timeout, subagent_timeout, api_retries,
                session_manager, options,
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
            context_window_override: self.context_window_override,
            watcher_exit_path: self.watcher_exit_path.clone(),
            max_tool_output: self.max_tool_output,
            bash_timeout: self.bash_timeout,
            bash_max_timeout: self.bash_max_timeout,
            subagent_timeout: self.subagent_timeout,
            api_retries: self.api_retries,
            session_manager: self.session_manager.clone(),
            reaper_handle: None,  // Cloned runtimes don't own the reaper
            reaper_cancel: None,  // Cloned runtimes don't own the reaper
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
        let default_max = 30000;
        
        // Short string should remain unchanged
        let short = "This is a short string.";
        assert_eq!(HelperMethods::truncate_tool_result(short, default_max), short);
        
        // Exactly max should remain unchanged
        let exact = "x".repeat(30000);
        assert_eq!(HelperMethods::truncate_tool_result(&exact, default_max), exact);
        
        // String longer than max should be truncated with notice
        let too_long = "x".repeat(30001);
        let truncated = HelperMethods::truncate_tool_result(&too_long, default_max);
        
        // Should start with the truncated content
        assert!(truncated.starts_with(&"x".repeat(30000)));
        
        // Should contain truncation notice with total char count
        assert!(truncated.contains("[truncated — 30001 total chars, showing first 30000]"));
        
        // Should be longer than max (due to notice)
        assert!(truncated.len() > 30000);
        
        // Test with a much longer string
        let very_long = "a".repeat(50000);
        let truncated_very_long = HelperMethods::truncate_tool_result(&very_long, default_max);
        assert!(truncated_very_long.contains("[truncated — 50000 total chars, showing first 30000]"));
        assert!(truncated_very_long.starts_with(&"a".repeat(30000)));
        
        // Test with custom limit
        let custom_truncated = HelperMethods::truncate_tool_result(&very_long, 100);
        assert!(custom_truncated.starts_with(&"a".repeat(100)));
        assert!(custom_truncated.contains("[truncated — 50000 total chars, showing first 100]"));
    }

    #[test]
    fn test_thinking_level_ranges() {
        use crate::core::models::thinking_level_for_budget;

        // Sentinel 0 = "adaptive" (S172 — model decides)
        assert_eq!(thinking_level_for_budget(0), "adaptive");

        // Low range: 1..=2048
        assert_eq!(thinking_level_for_budget(1), "low");
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
}