//! Non-streaming Anthropic API calls (`call_api`, `call_api_simple`).
//!
//! Extracted from `api.rs`. The streaming path lives in `api.rs`; this file
//! holds the synchronous variants used by `Runtime::run_single` and
//! `Runtime::compact_call`. All methods are additional `impl ApiMethods`
//! blocks — the struct itself is defined in `api.rs`.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use serde_json::{json, Value};
use reqwest::Client;
use crate::{Result, RuntimeError, ToolRegistry};
use super::api::{ApiMethods, ApiOptions};
use super::types::AuthState;
use super::helpers::HelperMethods;

impl ApiMethods {
    /// Concatenate the `text` fields of every block in an Anthropic-shaped
    /// response `content` array. Returns the empty string if the value is
    /// not an array or contains no text blocks.
    pub(super) fn concat_response_text(response: &Value) -> String {
        response["content"]
            .as_array()
            .map(|content| {
                content
                    .iter()
                    .filter_map(|item| item["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default()
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
        options: &ApiOptions,
    ) -> Result<Value> {
        // Route through OpenAI-compat provider if model resolves to one
        let tools_schema = tools.tools_schema();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        if let Some(result) = crate::runtime::openai::try_route(
            model, client, &tools_schema, system_prompt, messages, &tx,
            None, None, thinking_budget, &tokio_util::sync::CancellationToken::new(),
        ).await {
            drop(tx);
            while rx.recv().await.is_some() {}
            return result.map_err(|e| RuntimeError::Config(format!("openai provider: {e}")));
        }

        // Read auth state
        let (auth_token, auth_type) = {
            let a = auth.read().await;
            (a.auth_token.clone(), a.auth_type.clone())
        };

        if auth_type == "none" {
            return Err(RuntimeError::Auth(
                "No Anthropic credentials. Run `synaps login` or set ANTHROPIC_API_KEY, or switch to a provider model with `/model groq/llama-3.3-70b-versatile`.".to_string()
            ));
        }

        let auth_header = if auth_type == "oauth" {
            ("authorization".to_string(), format!("Bearer {}", auth_token))
        } else {
            ("x-api-key".to_string(), auth_token.clone())
        };

        // Avoid modifying past messages to maintain a 100% stable prefix for Anthropic caching.
        let mut cleaned_messages = messages.to_vec();
        // Strip empty/invalid thinking blocks before they hit the API. See
        // `sanitize_thinking_blocks` for the failure mode this guards against.
        HelperMethods::sanitize_thinking_blocks(&mut cleaned_messages);
        HelperMethods::annotate_cache_breakpoint(&mut cleaned_messages);

        let thinking_level = crate::core::models::thinking_level_for_budget(thinking_budget);

        let mut body = json!({
            "model": model,
            "max_tokens": HelperMethods::max_tokens_for_model(model),
            "messages": cleaned_messages,
            "tools": &*tools.tools_schema(),
            "thinking": if crate::core::models::model_supports_adaptive_thinking(model) {
                json!({ "type": "adaptive", "display": "summarized" })
            } else {
                // Legacy path: budget_tokens must be >= 1024. Fall back to "high"
                // if the sentinel 0 (adaptive) leaked through for a legacy model.
                let budget = if thinking_budget == 0 { crate::core::models::DEFAULT_LEGACY_ADAPTIVE_FALLBACK } else { thinking_budget };
                json!({
                    "type": "enabled",
                    "budget_tokens": budget,
                    "display": "summarized"
                })
            }
        });

        if crate::core::models::model_supports_adaptive_thinking(model) {
            if let Some(effort) = crate::core::models::effort_for_thinking_level(thinking_level) {
                body["output_config"] = json!({"effort": effort});
            }
        }

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
                let mut betas: Vec<&str> = Vec::new();
                if auth_type == "oauth" {
                    betas.push("claude-code-20250219");
                    betas.push("oauth-2025-04-20");
                }
                if options.use_1m_context && crate::core::models::model_supports_1m(model) {
                    betas.push("context-1m-2025-08-07");
                }
                if !betas.is_empty() {
                    req = req.header("anthropic-beta", betas.join(","));
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

        // Log usage for cache analysis
        if let Some(usage) = json.get("usage") {
            let input_t = usage["input_tokens"].as_u64().unwrap_or(0);
            let output_t = usage["output_tokens"].as_u64().unwrap_or(0);
            let cache_read = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
            let cache_create = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            HelperMethods::log_usage(input_t, cache_read, cache_create, output_t);
        }

        Ok(json)
    }

    /// Simple non-streaming API call without tools (used for compaction).
    /// Uses a caller-supplied system prompt (replaces the runtime's) and forces
    /// "low" effort on adaptive models — summarization doesn't benefit from
    /// heavy reasoning budgets.
    pub(super) async fn call_api_simple(
        auth: &Arc<RwLock<AuthState>>,
        client: &Client,
        model: &str,
        system_prompt: &str,
        thinking_budget: u32,
        messages: &[Value],
        max_retries: u32,
    ) -> Result<String> {
        let tools_schema = Arc::new(Vec::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let routed_system_prompt = Some(system_prompt.to_string());
        if let Some(result) = crate::runtime::openai::try_route(
            model,
            client,
            &tools_schema,
            &routed_system_prompt,
            messages,
            &tx,
            None,
            None,
            thinking_budget,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        {
            drop(tx);
            while rx.recv().await.is_some() {}
            let response =
                result.map_err(|e| RuntimeError::Config(format!("openai provider: {e}")))?;
            return Ok(Self::concat_response_text(&response));
        }

        let (auth_header_name, auth_header_value, auth_type) = Self::build_auth_header(auth).await;

        // Fail early with a clear message if no Anthropic credentials
        if auth_type == "none" {
            return Err(RuntimeError::Auth(
                "No API key or OAuth token found. Run `synaps login` to authenticate.".to_string()
            ));
        }

        let mut body = json!({
            "model": model,
            "max_tokens": HelperMethods::max_tokens_for_model(model),
            "messages": messages,
            "thinking": if crate::core::models::model_supports_adaptive_thinking(model) {
                json!({ "type": "adaptive", "display": "summarized" })
            } else {
                let budget = if thinking_budget == 0 {
                    crate::core::models::DEFAULT_LEGACY_ADAPTIVE_FALLBACK
                } else {
                    thinking_budget
                };
                json!({
                    "type": "enabled",
                    "budget_tokens": budget,
                    "display": "summarized"
                })
            }
        });

        // Force low effort on adaptive models — compaction is a structured
        // summarization task; heavy reasoning wastes tokens.
        if crate::core::models::model_supports_adaptive_thinking(model) {
            body["output_config"] = json!({"effort": "low"});
        }

        if auth_type == "oauth" {
            let system_blocks = vec![
                json!({"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."}),
                json!({"type": "text", "text": "You are a helpful AI assistant with access to tools. Use them when needed."}),
                json!({"type": "text", "text": system_prompt}),
            ];
            body["system"] = json!(system_blocks);
        } else {
            body["system"] = json!([
                {"type": "text", "text": system_prompt}
            ]);
        }

        // Retry loop for transient errors
        let json: Value = {
            let mut last_err = String::new();
            let mut result_json = None;
            for attempt in 0..=max_retries {
                if attempt > 0 {
                    let delay = Duration::from_millis(1000 * 2u64.pow(attempt - 1));
                    tracing::warn!("Compaction API retry {}/{} after {:?}: {}", attempt, max_retries, delay, last_err);
                    tokio::time::sleep(delay).await;
                }

                let mut req = client
                    .post("https://api.anthropic.com/v1/messages")
                    .header(auth_header_name.clone(), auth_header_value.clone())
                    .header("anthropic-version", "2023-06-01")
                    .header("content-type", "application/json");
                if let Some(beta) = Self::build_beta_header(&auth_type, &ApiOptions::default(), model) {
                    req = req.header("anthropic-beta", beta);
                }

                match req.json(&body).send().await {
                    Ok(resp) => {
                        let status = resp.status();
                        if status.is_success() {
                            match resp.json::<Value>().await {
                                Ok(j) => {
                                    if j["error"].is_object() {
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

        // Log usage
        if let Some(usage) = json.get("usage") {
            let input_t = usage["input_tokens"].as_u64().unwrap_or(0);
            let output_t = usage["output_tokens"].as_u64().unwrap_or(0);
            let cache_read = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
            let cache_create = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            HelperMethods::log_usage(input_t, cache_read, cache_create, output_t);
        }

        // Extract text content from the response (skip thinking blocks)
        let mut out = String::new();
        if let Some(content) = json["content"].as_array() {
            for block in content {
                if block["type"].as_str() == Some("text") {
                    if let Some(t) = block["text"].as_str() {
                        out.push_str(t);
                    }
                }
            }
        }

        if out.is_empty() {
            return Err(RuntimeError::Tool("Compaction returned empty response".to_string()));
        }

        Ok(out)
    }
}

#[cfg(test)]
mod concat_response_text_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_text_from_single_block() {
        let v = json!({"content": [{"type": "text", "text": "hello"}]});
        assert_eq!(ApiMethods::concat_response_text(&v), "hello");
    }

    #[test]
    fn concatenates_multiple_text_blocks() {
        let v = json!({"content": [
            {"type": "text", "text": "alpha "},
            {"type": "text", "text": "beta"},
        ]});
        assert_eq!(ApiMethods::concat_response_text(&v), "alpha beta");
    }

    #[test]
    fn skips_non_text_blocks() {
        let v = json!({"content": [
            {"type": "tool_use", "name": "bash"},
            {"type": "text", "text": "result"},
        ]});
        assert_eq!(ApiMethods::concat_response_text(&v), "result");
    }

    #[test]
    fn returns_empty_for_missing_content() {
        let v = json!({"role": "assistant"});
        assert_eq!(ApiMethods::concat_response_text(&v), "");
    }

    #[test]
    fn returns_empty_for_non_array_content() {
        let v = json!({"content": "stringified"});
        assert_eq!(ApiMethods::concat_response_text(&v), "");
    }
}
