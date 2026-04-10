use reqwest::Client;
use serde_json::{json, Value};
use serde::{Deserialize};
use std::path::Path;
use crate::{Result, RuntimeError, ToolRegistry};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio::sync::mpsc;
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
    Done,
    Error(String),
}

pub struct Runtime {
    client: Client,
    auth_token: String,
    auth_type: String,
    model: String,
    tools: ToolRegistry,
    system_prompt: Option<String>,
    thinking_budget: u32,
}

impl Runtime {
    pub async fn new() -> Result<Self> {
        let (auth_token, auth_type) = Self::get_auth_token()?;

        Ok(Runtime {
            client: Client::new(),
            auth_token,
            auth_type,
            model: "claude-sonnet-4-20250514".to_string(),
            tools: ToolRegistry::new(),
            system_prompt: None,
            thinking_budget: 4096,
        })
    }

    pub fn set_system_prompt(&mut self, prompt: String) {
        self.system_prompt = Some(prompt);
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

    pub fn thinking_level(&self) -> &str {
        match self.thinking_budget {
            0..=2048 => "low",
            2049..=4096 => "medium",
            4097..=16384 => "high",
            _ => "xhigh",
        }
    }
    
    fn get_auth_token() -> Result<(String, String)> {
        let home = std::env::var("HOME").unwrap();
        let auth_path = Path::new(&home).join(".pi/agent/auth.json");
        
        if auth_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&auth_path) {
                if let Ok(auth) = serde_json::from_str::<PiAuth>(&content) {
                    let creds = &auth.anthropic;
                    if creds.auth_type == "oauth" && creds.access.is_some() {
                        return Ok((creds.access.as_ref().unwrap().clone(), "oauth".to_string()));
                    }
                }
            }
        }
        
        // Fall back to env var
        if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
            return Ok((api_key, "api_key".to_string()));
        }
        
        Err(RuntimeError::Tool("No Anthropic credentials found".to_string()))
    }

    pub async fn run_single(&self, prompt: &str) -> Result<String> {
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

    pub fn run_stream(&self, prompt: String) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send>> {
        self.run_stream_with_messages(vec![json!({"role": "user", "content": prompt})])
    }

    pub fn run_stream_with_messages(&self, messages: Vec<Value>) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send>> {
        let (tx, rx) = mpsc::unbounded_channel();
        let runtime = self.clone();

        tokio::spawn(async move {
            if let Err(e) = runtime.run_stream_internal(messages, tx.clone()).await {
                let _ = tx.send(StreamEvent::Error(e.to_string()));
            }
            let _ = tx.send(StreamEvent::Done);
        });

        Box::pin(UnboundedReceiverStream::new(rx))
    }

    async fn run_stream_internal(&self, initial_messages: Vec<Value>, tx: mpsc::UnboundedSender<StreamEvent>) -> Result<()> {
        let mut messages = initial_messages;
        
        loop {
            let response = self.call_api_stream(&messages, tx.clone()).await?;
            
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
                
                
                // If no tool uses, we're done — send final messages for multi-turn context
                if tool_uses.is_empty() {
                    // Add the final assistant response to messages before sending
                    messages.push(json!({
                        "role": "assistant",
                        "content": content
                    }));
                    let _ = tx.send(StreamEvent::MessageHistory(messages));
                    return Ok(());
                }
                
                // Add assistant's response to conversation
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
                        
                        // Send tool use event
                        let _ = tx.send(StreamEvent::ToolUse {
                            tool_name: tool_name.to_string(),
                            tool_id: tool_id.to_string(),
                            input: input.clone(),
                        });
                        
                        let result = match self.tools.get(tool_name) {
                            Some(tool) => {
                                match tool.execute(input.clone()).await {
                                    Ok(output) => output,
                                    Err(e) => format!("Tool execution failed: {}", e),
                                }
                            }
                            None => format!("Unknown tool: {}", tool_name),
                        };
                        
                        // Send tool result event
                        let _ = tx.send(StreamEvent::ToolResult {
                            tool_id: tool_id.to_string(),
                            result: result.clone(),
                        });
                        
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
            "max_tokens": 8192,
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
                                    if let Some(text) = delta["text"].as_str() {
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
                            let input: Value = match serde_json::from_str(&current_tool_input_json) {
                                Ok(v) => v,
                                Err(e) => {
                                    eprintln!("Warning: failed to parse tool input JSON: {}", e);
                                    json!({})
                                }
                            };

                            accumulated_content.push(json!({
                                "type": "tool_use",
                                "id": current_tool_id,
                                "name": current_tool_name,
                                "input": input
                            }));

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
                    Some("message_delta") | Some("message_stop") => {}
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
                            let input: Value = match serde_json::from_str(&current_tool_input_json) {
                                Ok(v) => v,
                                Err(e) => {
                                    eprintln!("Warning: failed to parse tool input JSON: {}", e);
                                    json!({})
                                }
                            };
                            accumulated_content.push(json!({
                                "type": "tool_use",
                                "id": current_tool_id,
                                "name": current_tool_name,
                                "input": input
                            }));
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
            "max_tokens": 8192,
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
            model: self.model.clone(),
            tools: self.tools.clone(),
            system_prompt: self.system_prompt.clone(),
            thinking_budget: self.thinking_budget,
        }
    }
}
