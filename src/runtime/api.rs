use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;
use serde_json::{json, Value};
use reqwest::Client;
use futures::StreamExt;
use crate::{Result, RuntimeError, ToolRegistry};
use super::types::{AuthState, StreamEvent};
use super::helpers::HelperMethods;

pub(super) struct ApiMethods;

impl ApiMethods {
    #[allow(dead_code, clippy::too_many_arguments)]
    pub(super) async fn call_api_stream(
        auth: &Arc<RwLock<AuthState>>,
        client: &Client,
        model: &str,
        tools: &ToolRegistry,
        system_prompt: &Option<String>,
        thinking_budget: u32,
        messages: &[Value],
        tx: mpsc::UnboundedSender<StreamEvent>,
        max_retries: u32,
    ) -> Result<Value> {
        Self::call_api_stream_inner(auth, client, model, tools, system_prompt, thinking_budget, messages, tx, &CancellationToken::new(), max_retries, false).await
    }

    /// Static inner version — used by both `call_api_stream` (instance) and
    /// `run_stream_internal` (spawned task) so there's one implementation.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn call_api_stream_inner(
        auth: &Arc<RwLock<AuthState>>,
        client: &Client,
        model: &str,
        tools: &ToolRegistry,
        system_prompt: &Option<String>,
        thinking_budget: u32,
        messages: &[Value],
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: &CancellationToken,
        max_retries: u32,
        auto_cache: bool,
    ) -> Result<Value> {
        // Read auth state for this API call
        let (auth_token, auth_type) = {
            let a = auth.read().await;
            (a.auth_token.clone(), a.auth_type.clone())
        };

        let auth_header = if auth_type == "oauth" {
            ("authorization".to_string(), format!("Bearer {}", auth_token))
        } else {
            ("x-api-key".to_string(), auth_token.clone())
        };
        
        tracing::info!(model = %model, "Starting API request");
        
        // Cache strategy: manual breakpoints (default) or automatic (top-level cache_control)
        let mut cleaned_messages = messages.to_vec();
        if !auto_cache {
            HelperMethods::annotate_cache_breakpoint(&mut cleaned_messages);
        }

        let mut body = json!({
            "model": model,
            "max_tokens": HelperMethods::max_tokens_for_model(model),
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

        // Auto-cache: add top-level cache_control for automatic breakpoint management
        if auto_cache {
            body["cache_control"] = json!({"type": "ephemeral"});
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

        // Retry loop for transient API errors (429, 529, 500, 502, 503)
        let response = {
            let mut last_err = String::new();
            let mut response = None;

            for attempt in 0..=max_retries {
                if attempt > 0 {
                    let delay = Duration::from_millis(1000 * 2u64.pow(attempt - 1)); // 1s, 2s, 4s
                    tracing::warn!("API retry {}/{} after {:?}: {}", attempt, max_retries, delay, last_err);
                    let _ = tx.send(StreamEvent::Text(format!("\n⏳ API error, retrying ({}/{})...\n", attempt, max_retries)));
                    tokio::time::sleep(delay).await;

                    if cancel.is_cancelled() {
                        return Err(RuntimeError::Cancelled);
                    }
                }

                // Rebuild request (consumed on send)
                let mut req = client
                    .post("https://api.anthropic.com/v1/messages")
                    .header(auth_header.0.clone(), auth_header.1.clone())
                    .header("anthropic-version", "2023-06-01")
                    .header("content-type", "application/json");
                if auth_type == "oauth" {
                    req = req.header("anthropic-beta", "claude-code-20250219,oauth-2025-04-20");
                }

                match req.json(&body).send().await {
                    Ok(resp) => {
                        let status = resp.status();
                        if status.is_success() {
                            response = Some(resp);
                            break;
                        }
                        let is_retryable = matches!(status.as_u16(), 429 | 500 | 502 | 503 | 529);
                        let error_text = resp.text().await.unwrap_or_default();
                        if !is_retryable || attempt == max_retries {
                            return Err(RuntimeError::Tool(format!("API Error ({}): {}", status, error_text)));
                        }
                        last_err = format!("{}: {}", status, error_text);
                    }
                    Err(e) => {
                        if attempt == max_retries {
                            return Err(RuntimeError::Api(e));
                        }
                        last_err = e.to_string();
                    }
                }
            }

            response.ok_or_else(|| RuntimeError::Tool(format!("API failed after {} retries: {}", max_retries, last_err)))?
        };
        
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
        if let Some(data_part) = remaining.strip_prefix("data: ") {
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
    
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn call_api(
        auth: &Arc<RwLock<AuthState>>,
        client: &Client,
        model: &str,
        tools: &ToolRegistry,
        system_prompt: &Option<String>,
        thinking_budget: u32,
        messages: &[Value],
        max_retries: u32,
    ) -> Result<Value> {
        // Read auth state
        let (auth_token, auth_type) = {
            let a = auth.read().await;
            (a.auth_token.clone(), a.auth_type.clone())
        };

        let auth_header = if auth_type == "oauth" {
            ("authorization".to_string(), format!("Bearer {}", auth_token))
        } else {
            ("x-api-key".to_string(), auth_token.clone())
        };
        
        // Avoid modifying past messages to maintain a 100% stable prefix for Anthropic caching.
        let mut cleaned_messages = messages.to_vec();
        HelperMethods::annotate_cache_breakpoint(&mut cleaned_messages);

        let mut body = json!({
            "model": model,
            "max_tokens": HelperMethods::max_tokens_for_model(model),
            "messages": cleaned_messages,
            "tools": &*tools.tools_schema(),
            "thinking": {
                "type": "enabled",
                "budget_tokens": thinking_budget,
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

        // Retry loop for transient API errors (429, 529, 500, 502, 503)
        let json: Value = {
            let mut last_err = String::new();

            let mut result_json = None;
            for attempt in 0..=max_retries {
                if attempt > 0 {
                    let delay = Duration::from_millis(1000 * 2u64.pow(attempt - 1));
                    tracing::warn!("API retry {}/{} after {:?}: {}", attempt, max_retries, delay, last_err);
                    tokio::time::sleep(delay).await;
                }

                let mut req = client
                    .post("https://api.anthropic.com/v1/messages")
                    .header(auth_header.0.clone(), auth_header.1.clone())
                    .header("anthropic-version", "2023-06-01")
                    .header("content-type", "application/json");
                if auth_type == "oauth" {
                    req = req.header("anthropic-beta", "claude-code-20250219,oauth-2025-04-20");
                }

                match req.json(&body).send().await {
                    Ok(resp) => {
                        let status = resp.status();
                        if status.is_success() {
                            match resp.json::<Value>().await {
                                Ok(j) => {
                                    if j["error"].is_object() {
                                        eprintln!("API Error Response: {}", serde_json::to_string_pretty(&j).unwrap_or_default());
                                        if let Some(error_type) = j["error"]["type"].as_str() {
                                            return Err(RuntimeError::Tool(format!("API Error: {}", error_type)));
                                        }
                                    }
                                    result_json = Some(j);
                                    break;
                                }
                                Err(e) => {
                                    if attempt == max_retries {
                                        return Err(RuntimeError::Api(e));
                                    }
                                    last_err = e.to_string();
                                }
                            }
                        } else {
                            let is_retryable = matches!(status.as_u16(), 429 | 500 | 502 | 503 | 529);
                            let error_text = resp.text().await.unwrap_or_default();
                            if !is_retryable || attempt == max_retries {
                                return Err(RuntimeError::Tool(format!("API Error ({}): {}", status, error_text)));
                            }
                            last_err = format!("{}: {}", status, error_text);
                        }
                    }
                    Err(e) => {
                        if attempt == max_retries {
                            return Err(RuntimeError::Api(e));
                        }
                        last_err = e.to_string();
                    }
                }
            }

            result_json.ok_or_else(|| RuntimeError::Tool(format!("API failed after {} retries: {}", max_retries, last_err)))?
        };
        
        Ok(json)
    }
}