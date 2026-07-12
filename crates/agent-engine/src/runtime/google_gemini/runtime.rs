//! Runtime dispatch for `WireProtocol::GoogleGeminiCodeAssist`.
//!
//! Translates the runtime's Anthropic-shaped `SharedMessage`/tool-schema into
//! Gemini `ChatTurn`/`ToolSpec`, invokes the broker-proxied
//! [`super::stream::stream_gemini`], and forwards decoded events onto the
//! runtime event bus while aggregating a final Anthropic-shaped content Value
//! for the outer agent loop.
//!
//! The broker credential boundary is preserved: this module never touches the
//! OAuth access token, refresh token, or auth.json — it hands the request to
//! `CredentialBroker::proxy_stream` and consumes bytes.

use std::sync::Arc;

use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::stream::{stream_gemini, StreamError};
use super::translate::{ChatTurn, GeminiStreamEvent, ToolSpec};
use crate::auth::CredentialBroker;
use crate::runtime::openai::types::ProviderConfig;
use crate::runtime::types::{LlmEvent, StreamEvent};

/// Translate tool schemas (Anthropic-shaped: `{name, description, input_schema}`)
/// into Gemini `ToolSpec`s. Internal-only tool names are dropped.
fn tools_to_gemini(schema: &[Value]) -> Vec<ToolSpec> {
    schema
        .iter()
        .filter_map(|t| {
            let name = t.get("name")?.as_str()?.to_string();
            if name.is_empty()
                || name == "respond"
                || name == "send_channel"
                || name == "watcher_exit"
            {
                return None;
            }
            let description = t
                .get("description")
                .and_then(|d| d.as_str())
                .map(|s| s.to_string());
            let parameters_json_schema = t.get("input_schema").cloned();
            Some(ToolSpec {
                name,
                description,
                parameters_json_schema,
            })
        })
        .collect()
}

/// Translate Anthropic-shaped `SharedMessage`s into a flat sequence of Gemini
/// `ChatTurn`s. Text and tool-use/tool-result blocks are preserved; `thinking`
/// blocks and other unrepresentable content are dropped.
fn messages_to_gemini_turns(messages: &[crate::SharedMessage]) -> Vec<ChatTurn> {
    // Build tool_use_id → tool_name map from assistant turns so tool_result
    // blocks can be attached with the correct function name.
    let mut id_to_name: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for msg in messages {
        if msg.get("role").and_then(|r| r.as_str()) == Some("assistant") {
            if let Some(Value::Array(blocks)) = msg.get("content") {
                for block in blocks {
                    if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        if let (Some(id), Some(name)) = (
                            block.get("id").and_then(|v| v.as_str()),
                            block.get("name").and_then(|v| v.as_str()),
                        ) {
                            id_to_name.insert(id.to_string(), name.to_string());
                        }
                    }
                }
            }
        }
    }

    let mut turns: Vec<ChatTurn> = Vec::new();

    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let content = msg.get("content");

        match role {
            "user" => match content {
                Some(Value::String(s)) if !s.is_empty() => {
                    turns.push(ChatTurn::User { text: s.clone() });
                }
                Some(Value::Array(blocks)) => {
                    let mut text_buf = String::new();
                    for block in blocks {
                        let btype = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match btype {
                            "text" => {
                                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                                    text_buf.push_str(t);
                                }
                            }
                            "tool_result" => {
                                if !text_buf.is_empty() {
                                    turns.push(ChatTurn::User {
                                        text: std::mem::take(&mut text_buf),
                                    });
                                }
                                let tool_id = block
                                    .get("tool_use_id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let name = id_to_name.get(&tool_id).cloned().unwrap_or_default();
                                let result = match block.get("content") {
                                    Some(Value::String(s)) => json!(s),
                                    Some(Value::Array(arr)) => {
                                        let text = arr
                                            .iter()
                                            .filter_map(|b| {
                                                b.get("text")
                                                    .and_then(|t| t.as_str())
                                                    .map(String::from)
                                            })
                                            .collect::<Vec<_>>()
                                            .join("");
                                        json!({ "output": text })
                                    }
                                    Some(other) => other.clone(),
                                    None => json!({}),
                                };
                                turns.push(ChatTurn::ToolResult { name, result });
                            }
                            _ => {}
                        }
                    }
                    if !text_buf.is_empty() {
                        turns.push(ChatTurn::User { text: text_buf });
                    }
                }
                _ => {}
            },
            "assistant" => match content {
                Some(Value::String(s)) if !s.is_empty() => {
                    turns.push(ChatTurn::Assistant { text: s.clone() });
                }
                Some(Value::Array(blocks)) => {
                    let mut text_buf = String::new();
                    for block in blocks {
                        let btype = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match btype {
                            "text" => {
                                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                                    text_buf.push_str(t);
                                }
                            }
                            "tool_use" => {
                                if !text_buf.is_empty() {
                                    turns.push(ChatTurn::Assistant {
                                        text: std::mem::take(&mut text_buf),
                                    });
                                }
                                let name = block
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let args = block
                                    .get("input")
                                    .cloned()
                                    .unwrap_or_else(|| json!({}));
                                turns.push(ChatTurn::ToolCall { name, args });
                            }
                            // `thinking` and other unknown block types are not
                            // representable on the Gemini wire — drop.
                            _ => {}
                        }
                    }
                    if !text_buf.is_empty() {
                        turns.push(ChatTurn::Assistant { text: text_buf });
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    turns
}

/// Streamed Gemini turn: forwards text/tool events onto `tx` and returns an
/// Anthropic-shaped `{content, stop_reason, usage}` Value for the outer loop.
///
/// The broker owns the OAuth token and pins the upstream host; this function
/// never touches secrets directly.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn call_google_gemini_stream_inner(
    cfg: &ProviderConfig,
    broker: &Arc<dyn CredentialBroker>,
    tools_schema: &[Value],
    system_prompt: &Option<String>,
    messages: &[crate::SharedMessage],
    tx: &mpsc::UnboundedSender<StreamEvent>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let turns = messages_to_gemini_turns(messages);
    let tools = tools_to_gemini(tools_schema);

    tracing::debug!(
        provider = %cfg.provider,
        model = %cfg.model,
        "google-gemini stream request via broker proxy"
    );

    let mut stream = stream_gemini(
        broker.as_ref(),
        cfg.model.clone(),
        None, // project id is broker-owned; runtime does not surface it here
        system_prompt.clone(),
        &turns,
        &tools,
        cancel.clone(),
    )
    .await
    .map_err(|e| Box::<dyn std::error::Error + Send + Sync>::from(format!("{e}")))?;

    let mut assembled_text = String::new();
    let mut content_blocks: Vec<Value> = Vec::new();
    let mut stop_reason: Option<String> = None;
    let mut tool_seq: u64 = 0;

    while let Some(event) = stream.next().await {
        match event {
            Ok(GeminiStreamEvent::TextDelta(delta)) => {
                assembled_text.push_str(&delta);
                let _ = tx.send(StreamEvent::Llm(LlmEvent::Text(delta)));
            }
            Ok(GeminiStreamEvent::ToolCall(call)) => {
                // Flush any buffered text as a `text` block before the tool_use.
                if !assembled_text.is_empty() {
                    content_blocks.push(json!({
                        "type": "text",
                        "text": std::mem::take(&mut assembled_text),
                    }));
                }
                tool_seq += 1;
                // Gemini function calls have no vendor tool-call id, so we
                // synthesize a stable per-turn id for the downstream loop.
                let tool_id = format!("gemini_call_{tool_seq}");
                let _ = tx.send(StreamEvent::Llm(LlmEvent::ToolUseStart {
                    tool_name: call.name.clone(),
                    tool_id: tool_id.clone(),
                }));
                let input = call.args.clone();
                let _ = tx.send(StreamEvent::Llm(LlmEvent::ToolUse {
                    tool_name: call.name.clone(),
                    tool_id: tool_id.clone(),
                    input: input.clone(),
                }));
                content_blocks.push(json!({
                    "type": "tool_use",
                    "id": tool_id,
                    "name": call.name,
                    "input": input,
                }));
            }
            Ok(GeminiStreamEvent::Finish { reason }) => {
                if let Some(r) = reason {
                    stop_reason = Some(map_finish_reason(&r));
                }
            }
            Ok(GeminiStreamEvent::Ignored) => {}
            Err(StreamError::Cancelled) => {
                return Err("operation canceled".into());
            }
            Err(e) => {
                return Err(format!("google-gemini: {e}").into());
            }
        }
    }

    if !assembled_text.is_empty() {
        content_blocks.push(json!({
            "type": "text",
            "text": std::mem::take(&mut assembled_text),
        }));
    }

    Ok(json!({
        "content": content_blocks,
        "stop_reason": stop_reason.unwrap_or_else(|| "end_turn".to_string()),
        "usage": {},
    }))
}

/// Map Gemini's `finishReason` values onto Anthropic-style stop reasons the
/// outer agent loop already knows how to interpret.
fn map_finish_reason(reason: &str) -> String {
    match reason {
        "STOP" => "end_turn".to_string(),
        "MAX_TOKENS" => "max_tokens".to_string(),
        // Tool-call-driven stop maps to Anthropic's `tool_use`.
        "TOOL_CALL" | "FUNCTION_CALL" => "tool_use".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{
        AccessToken, BrokerError, OAuthProviderId, ProxyByteStream, ProxyRequest, ProxyResponse,
        ProviderStatus,
    };
    use async_trait::async_trait;
    use futures::stream;
    use std::sync::Mutex;

    struct StubBroker {
        chunks: Mutex<Option<Vec<Result<bytes::Bytes, BrokerError>>>>,
        seen: Arc<Mutex<Option<ProxyRequest>>>,
    }

    impl StubBroker {
        fn new(chunks: Vec<Result<bytes::Bytes, BrokerError>>) -> Self {
            Self {
                chunks: Mutex::new(Some(chunks)),
                seen: Arc::default(),
            }
        }
    }

    #[async_trait]
    impl CredentialBroker for StubBroker {
        async fn access_token(
            &self,
            _p: OAuthProviderId,
        ) -> Result<AccessToken, BrokerError> {
            Err(BrokerError::NotConfigured("stub".into()))
        }
        async fn proxy(&self, _r: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
            Err(BrokerError::Denied("not implemented".into()))
        }
        async fn proxy_stream(
            &self,
            request: ProxyRequest,
        ) -> Result<ProxyByteStream, BrokerError> {
            *self.seen.lock().unwrap() = Some(request);
            let chunks = self.chunks.lock().unwrap().take().unwrap_or_default();
            Ok(Box::pin(stream::iter(chunks)))
        }
        async fn anthropic_usage(&self) -> Result<Value, BrokerError> {
            Err(BrokerError::Denied("not implemented".into()))
        }
        async fn capabilities(&self) -> Result<Vec<ProviderStatus>, BrokerError> {
            Ok(vec![])
        }
    }

    fn chunk(s: &str) -> Result<bytes::Bytes, BrokerError> {
        Ok(bytes::Bytes::copy_from_slice(s.as_bytes()))
    }

    fn cfg() -> ProviderConfig {
        ProviderConfig {
            base_url: "https://cloudcode-pa.googleapis.com".into(),
            model: "gemini-2.5-pro".into(),
            provider: "google-gemini".into(),
        }
    }

    #[tokio::test]
    async fn forwards_text_deltas_and_returns_content_blocks() {
        let broker: Arc<dyn CredentialBroker> = Arc::new(StubBroker::new(vec![
            chunk("data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hi \"}]}}]}}\n"),
            chunk("data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"there\"}]},\"finishReason\":\"STOP\"}]}}\n"),
        ]));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let msgs: Vec<crate::SharedMessage> =
            vec![Arc::new(json!({"role":"user","content":"hi"}))];

        let out = call_google_gemini_stream_inner(
            &cfg(),
            &broker,
            &[],
            &None,
            &msgs,
            &tx,
            &cancel,
        )
        .await
        .unwrap();
        drop(tx);

        // Aggregated content block preserves streamed text.
        assert_eq!(out["content"][0]["type"], "text");
        assert_eq!(out["content"][0]["text"], "Hi there");
        assert_eq!(out["stop_reason"], "end_turn");

        // Text events were forwarded in order.
        let mut collected = String::new();
        while let Ok(ev) = rx.try_recv() {
            if let StreamEvent::Llm(LlmEvent::Text(t)) = ev {
                collected.push_str(&t);
            }
        }
        assert_eq!(collected, "Hi there");
    }

    #[tokio::test]
    async fn forwards_tool_calls_and_maps_to_tool_use_content_block() {
        let broker: Arc<dyn CredentialBroker> = Arc::new(StubBroker::new(vec![
            chunk("data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"looking\"}]}}]}}\n"),
            chunk("data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"search\",\"args\":{\"q\":\"rust\"}}}]},\"finishReason\":\"TOOL_CALL\"}]}}\n"),
        ]));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let msgs: Vec<crate::SharedMessage> =
            vec![Arc::new(json!({"role":"user","content":"find rust"}))];
        let tools = vec![json!({
            "name": "search",
            "description": "search the web",
            "input_schema": {"type":"object","properties":{"q":{"type":"string"}}}
        })];

        let out = call_google_gemini_stream_inner(
            &cfg(),
            &broker,
            &tools,
            &Some("be helpful".into()),
            &msgs,
            &tx,
            &cancel,
        )
        .await
        .unwrap();
        drop(tx);

        // Content includes both the buffered text and the tool_use block.
        assert_eq!(out["content"][0]["type"], "text");
        assert_eq!(out["content"][0]["text"], "looking");
        assert_eq!(out["content"][1]["type"], "tool_use");
        assert_eq!(out["content"][1]["name"], "search");
        assert_eq!(out["content"][1]["input"]["q"], "rust");
        assert_eq!(out["stop_reason"], "tool_use");

        let mut saw_tool_start = false;
        let mut saw_tool_use = false;
        let mut text = String::new();
        while let Ok(ev) = rx.try_recv() {
            match ev {
                StreamEvent::Llm(LlmEvent::Text(t)) => text.push_str(&t),
                StreamEvent::Llm(LlmEvent::ToolUseStart { tool_name, .. }) => {
                    assert_eq!(tool_name, "search");
                    saw_tool_start = true;
                }
                StreamEvent::Llm(LlmEvent::ToolUse { tool_name, input, .. }) => {
                    assert_eq!(tool_name, "search");
                    assert_eq!(input["q"], "rust");
                    saw_tool_use = true;
                }
                _ => {}
            }
        }
        assert_eq!(text, "looking");
        assert!(saw_tool_start);
        assert!(saw_tool_use);
    }

    #[test]
    fn messages_to_gemini_turns_maps_tool_use_and_tool_result_roles() {
        let msgs: Vec<crate::SharedMessage> = vec![
            Arc::new(json!({"role":"user","content":"do it"})),
            Arc::new(json!({"role":"assistant","content":[
                {"type":"text","text":"ok"},
                {"type":"tool_use","id":"t1","name":"do","input":{"x":1}}
            ]})),
            Arc::new(json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":"t1","content":"done"}
            ]})),
        ];
        let turns = messages_to_gemini_turns(&msgs);
        assert!(matches!(&turns[0], ChatTurn::User { text } if text == "do it"));
        assert!(matches!(&turns[1], ChatTurn::Assistant { text } if text == "ok"));
        assert!(matches!(&turns[2], ChatTurn::ToolCall { name, .. } if name == "do"));
        assert!(matches!(&turns[3], ChatTurn::ToolResult { name, .. } if name == "do"));
    }

    #[test]
    fn tools_to_gemini_drops_internal_only_tools() {
        let tools = vec![
            json!({"name": "respond"}),
            json!({"name": "search", "description": "d", "input_schema": {"type":"object"}}),
        ];
        let out = tools_to_gemini(&tools);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "search");
        assert_eq!(out[0].description.as_deref(), Some("d"));
        assert!(out[0].parameters_json_schema.is_some());
    }
}
