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
use std::time::Duration;

use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::setup::setup_user;
use super::stream::{stream_gemini, StreamError};
use super::translate::{ChatTurn, GeminiStreamEvent, ToolSpec};
use crate::auth::CredentialBroker;
use crate::runtime::openai::types::ProviderConfig;
use crate::runtime::types::{LlmEvent, SessionEvent, StreamEvent};

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
                                let mut result = match block.get("content") {
                                    Some(Value::String(s)) => json!({ "output": s }),
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
                                    Some(Value::Object(_)) => block["content"].clone(),
                                    Some(other) => json!({ "output": other }),
                                    None => json!({}),
                                };
                                if let Some(is_error) = block.get("is_error") {
                                    result["is_error"] = is_error.clone();
                                }
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
                                let args = block.get("input").cloned().unwrap_or_else(|| json!({}));
                                let thought_signature = block
                                    .get("thought_signature")
                                    .and_then(|value| value.as_str())
                                    .map(str::to_owned);
                                turns.push(ChatTurn::ToolCall {
                                    name,
                                    args,
                                    thought_signature,
                                });
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

    // Resolve the Code Assist project id through the broker before streaming.
    // Code Assist rejects `streamGenerateContent` without a project on the
    // envelope; the broker owns the OAuth token so `setup_user` never touches
    // secrets directly. We honor GOOGLE_CLOUD_PROJECT / GOOGLE_CLOUD_PROJECT_ID
    // as an override, matching the reference client.
    let env_project = gemini_project_env();
    let user = setup_user(broker.as_ref(), env_project)
        .await
        .map_err(|e| Box::<dyn std::error::Error + Send + Sync>::from(format!("{e}")))?;
    let project_id = user.project_id;

    tracing::debug!(
        provider = %cfg.provider,
        model = %cfg.model,
        project = %project_id,
        "google-gemini stream request via broker proxy"
    );

    let mut stream = {
        // ═══ RESET-HONORING 429 RETRY ═══════════════════════════════════════
        // Code Assist enforces small per-model windows and reports the reset
        // delay in the 429 body. Failing fast here made every agentic step die
        // on tiny-quota accounts; retrying immediately would burn the window
        // and extend the reset. So: honor the reported reset (plus a small
        // buffer), bounded by MAX_GEMINI_429_RETRIES / MAX_GEMINI_429_WAIT.
        // Non-429 errors keep failing fast — they are not capacity.
        let mut attempt: u32 = 0;
        loop {
            match stream_gemini(
                broker.as_ref(),
                cfg.model.clone(),
                Some(project_id.clone()),
                system_prompt.clone(),
                &turns,
                &tools,
                cancel.clone(),
            )
            .await
            {
                Ok(stream) => break stream,
                Err(e) => {
                    // Full text is used ONLY for 429/reset classification —
                    // the broker flattens the upstream status and rate-limit
                    // hint into it. Anything surfaced must be the redacted
                    // form: the snippet is provider-controlled and may echo
                    // the request (spec §5.1).
                    let text = format!("{e}");
                    let redacted = crate::runtime::openai::net::redact_provider_proxy_error(&text);
                    let Some(reset_secs) = code_assist_429_reset(&text) else {
                        return Err(Box::<dyn std::error::Error + Send + Sync>::from(redacted));
                    };
                    if attempt >= MAX_GEMINI_429_RETRIES {
                        return Err(Box::<dyn std::error::Error + Send + Sync>::from(redacted));
                    }
                    attempt += 1;
                    let wait = reset_secs
                        .map(|s| Duration::from_secs(s.saturating_add(1)))
                        .unwrap_or(DEFAULT_GEMINI_429_WAIT)
                        .min(MAX_GEMINI_429_WAIT);
                    let notice = format!(
                        "⚠ Gemini rate limited — resuming in {}s ({}/{})",
                        wait.as_secs(),
                        attempt,
                        MAX_GEMINI_429_RETRIES
                    );
                    tracing::warn!(
                        provider = %cfg.provider,
                        model = %cfg.model,
                        wait_secs = wait.as_secs(),
                        attempt,
                        "google-gemini 429 rate limited; honoring reported reset"
                    );
                    let _ = tx.send(StreamEvent::Session(SessionEvent::Notice(notice)));
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {}
                        _ = cancel.cancelled() => {
                            return Err("operation canceled".into());
                        }
                    }
                }
            }
        }
    };

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
                let mut tool_block = json!({
                    "type": "tool_use",
                    "id": tool_id,
                    "name": call.name,
                    "input": input,
                });
                if let Some(signature) = call.thought_signature {
                    tool_block["thought_signature"] = Value::String(signature);
                }
                content_blocks.push(tool_block);
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
                // Mid-stream broker errors are transport-level, but redact
                // defensively: any proxy-flattened upstream body snippet is
                // provider-controlled (spec §5.1).
                return Err(format!(
                    "google-gemini: {}",
                    crate::runtime::openai::net::redact_provider_proxy_error(&e.to_string())
                )
                .into());
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

/// Read `GOOGLE_CLOUD_PROJECT` (or the `_ID` alias) if set, matching the
/// reference client. Empty values are treated as unset. The runtime forwards
/// this to `setup_user`, which keeps the setup module env-free.
fn gemini_project_env() -> Option<String> {
    for key in ["GOOGLE_CLOUD_PROJECT", "GOOGLE_CLOUD_PROJECT_ID"] {
        if let Ok(v) = std::env::var(key) {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
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

/// Additional attempts allowed after a Code Assist 429 rate-limit rejection.
/// OAuth Code Assist quotas reset on short windows (observed 14–53s), so a
/// bounded reset-honoring retry converts tiny-quota accounts from hard
/// failure into slow-but-working sessions.
const MAX_GEMINI_429_RETRIES: u32 = 4;
/// Cap on any single reset wait so a hostile/garbled hint cannot stall us.
const MAX_GEMINI_429_WAIT: Duration = Duration::from_secs(120);
/// Wait used when a 429 carries no parsable reset hint.
const DEFAULT_GEMINI_429_WAIT: Duration = Duration::from_secs(20);

/// Detect a Code Assist rate-limit rejection in a broker transport error.
///
/// Returns `None` when the error is not a 429 rate limit; `Some(reset_secs)`
/// when it is, with the upstream-reported reset delay parsed from the
/// `"Your quota will reset after Ns"` message text when present. The broker
/// deliberately flattens upstream status to text (`provider request failed:
/// 429 Too Many Requests: …`), so this is the transport contract we parse —
/// a status marker alone is not enough, a rate-limit marker must accompany it.
fn code_assist_429_reset(err: &str) -> Option<Option<u64>> {
    let is_429 = err.contains("429");
    let has_rate_limit_marker = err.contains("RESOURCE_EXHAUSTED")
        || err.contains("RATE_LIMIT_EXCEEDED")
        || err.contains("Too Many Requests");
    if !is_429 || !has_rate_limit_marker {
        return None;
    }
    let secs = err.split("reset after").nth(1).and_then(|rest| {
        let rest = rest.trim_start();
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if rest[digits.len()..].starts_with('s') {
            digits.parse::<u64>().ok()
        } else {
            None
        }
    });
    Some(secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{
        AccessToken, BrokerError, OAuthProviderId, ProviderStatus, ProxyByteStream, ProxyRequest,
        ProxyResponse,
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
        async fn access_token(&self, _p: OAuthProviderId) -> Result<AccessToken, BrokerError> {
            Err(BrokerError::NotConfigured("stub".into()))
        }
        async fn proxy(&self, r: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
            // Serve a minimal Code Assist `loadCodeAssist` response so
            // `setup_user` can resolve a project id without secrets.
            if r.path == "/v1internal:loadCodeAssist" {
                return Ok(ProxyResponse {
                    status: 200,
                    body: r#"{"cloudaicompanionProject":"test-proj","currentTier":{"id":"STANDARD","name":"Std","hasOnboardedPreviously":true}}"#.to_string(),
                });
            }
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
        let msgs: Vec<crate::SharedMessage> = vec![Arc::new(json!({"role":"user","content":"hi"}))];

        let out = call_google_gemini_stream_inner(&cfg(), &broker, &[], &None, &msgs, &tx, &cancel)
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
                StreamEvent::Llm(LlmEvent::ToolUse {
                    tool_name, input, ..
                }) => {
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

    #[tokio::test]
    async fn streamed_tool_call_thought_signature_survives_shared_message_round_trip() {
        let first_broker: Arc<dyn CredentialBroker> = Arc::new(StubBroker::new(vec![chunk(
            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"bash\",\"args\":{\"command\":\"printf ok\"}},\"thoughtSignature\":\"opaque-signature\"}]},\"finishReason\":\"TOOL_CALL\"}]}}\n",
        )]));
        let (tx, _rx) = mpsc::unbounded_channel();
        let first_messages: Vec<crate::SharedMessage> =
            vec![Arc::new(json!({"role":"user","content":"run it"}))];
        let first = call_google_gemini_stream_inner(
            &cfg(),
            &first_broker,
            &[],
            &None,
            &first_messages,
            &tx,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();

        // Model the generic agent loop: retain the aggregated assistant content,
        // append its ordinary tool result, then translate the next request.
        let next_messages: Vec<crate::SharedMessage> = vec![
            Arc::new(json!({"role":"user","content":"run it"})),
            Arc::new(json!({"role":"assistant","content":first["content"].clone()})),
            Arc::new(json!({"role":"user","content":[{
                "type":"tool_result",
                "tool_use_id":"gemini_call_1",
                "content":"ok"
            }]})),
        ];
        let second_stub = Arc::new(StubBroker::new(vec![chunk(
            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"done\"}]},\"finishReason\":\"STOP\"}]}}\n",
        )]));
        let seen = second_stub.seen.clone();
        let second_broker: Arc<dyn CredentialBroker> = second_stub;
        call_google_gemini_stream_inner(
            &cfg(),
            &second_broker,
            &[],
            &None,
            &next_messages,
            &tx,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();

        let request = seen.lock().unwrap().take().unwrap();
        let body = request.body.unwrap();
        let call_part = &body["request"]["contents"][1]["parts"][0];
        assert_eq!(call_part["functionCall"]["name"], "bash");
        assert_eq!(call_part["thoughtSignature"], "opaque-signature");
        assert_eq!(body["request"]["contents"][2]["role"], "user");
        assert_eq!(
            body["request"]["contents"][2]["parts"][0]["functionResponse"]["response"],
            json!({"output":"ok"})
        );
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
    fn messages_to_gemini_turns_wraps_exact_task_md_read_result_and_preserves_error() {
        const TASK_MD: &str = r#"# Storm Passage — Black-Box Foreman Simulation

This is a fictional maritime command exercise. You are captain of the research vessel **Resolute**, 18 nautical miles from the lee shore, attempting to reach the protected waters of Northstar Harbor during a rapidly intensifying storm.

Initial state:
- Time: 21:40 local; darkness; visibility 0.8 nm.
- Wind: 45 kt gusting 62 from WSW, forecast to veer west within 90 minutes.
- Seas: 7–9 m, confused; barometer falling rapidly.
- Ship: twin-engine, port engine cooling alarm intermittent; steering hydraulic pressure fluctuating.
- Crew: 22; one deckhand has a suspected fractured wrist.
- Navigation: primary radar has intermittent clutter; GPS is available but must not be trusted alone.
- Options: continue 18 nm toward Northstar Harbor, divert 11 nm to exposed Greyhaven roads, or gain sea room before reassessing.
- Fuel endurance: 8 hours at reduced speed, 5 hours at full power.

Run a bounded command simulation to a decisive outcome. Act as foreground captain/foreman, not as the sole expert.

Requirements:
1. Before making the final passage plan, dispatch same-provider subagents as at least these independent bridge roles: navigation officer, chief engineer, and safety/weather officer. Use provider-qualified worker models. Do not use another provider.
2. Track every handle. Poll fairly; a long-running tool call or elapsed time alone is not a stall. Steer before replacing any worker. Do not duplicate an active assignment.
3. Collect and reconcile every officer report. Inspect their evidence critically; resolve disagreements explicitly.
4. Simulate at least four timed decision points with changing conditions. At each point record observed state, alternatives, chosen action, risk controls, and trigger for changing course.
5. Do not browse the web or claim live weather. This is a closed fictional exercise using only the supplied facts and clearly labeled assumptions.
6. Write `captains-log.md` with the full decision timeline and `outcome.json` with fields: `outcome`, `crew_status`, `ship_status`, `route`, `decision_points`, `workers_dispatched`, `workers_collected`, `workers_reconciled`, `verification`.
7. Independently verify both files for internal consistency and valid JSON. Completion is forbidden while required workers are running, terminal-but-uncollected, or collected-but-unreconciled.

Begin now and continue autonomously until the exercise reaches a verified safe or failed outcome. Do not ask the user for tactical choices.
"#;
        let msgs: Vec<crate::SharedMessage> = vec![
            Arc::new(json!({"role":"assistant","content":[
                {"type":"tool_use","id":"read-task","name":"read","input":{"path":"TASK.md"}}
            ]})),
            Arc::new(json!({"role":"user","content":[
                {
                    "type":"tool_result",
                    "tool_use_id":"read-task",
                    "content": TASK_MD,
                    "is_error": false
                }
            ]})),
        ];

        let turns = messages_to_gemini_turns(&msgs);
        assert!(matches!(
            &turns[1],
            ChatTurn::ToolResult { name, result }
                if name == "read"
                    && result == &json!({"output": TASK_MD, "is_error": false})
        ));
    }

    #[test]
    fn messages_to_gemini_turns_preserves_object_tool_results_and_error_metadata() {
        let msgs: Vec<crate::SharedMessage> = vec![
            Arc::new(json!({"role":"assistant","content":[
                {"type":"tool_use","id":"t1","name":"bash","input":{"command":"false"}}
            ]})),
            Arc::new(json!({"role":"user","content":[
                {
                    "type":"tool_result",
                    "tool_use_id":"t1",
                    "content":{"output":"exit 1","status":1},
                    "is_error":true
                }
            ]})),
        ];

        let turns = messages_to_gemini_turns(&msgs);
        assert!(matches!(
            &turns[1],
            ChatTurn::ToolResult { name, result }
                if name == "bash"
                    && result == &json!({"output":"exit 1","status":1,"is_error":true})
        ));
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

    #[tokio::test]
    async fn resolves_project_via_broker_and_includes_it_in_stream_request() {
        // Regression: previously the runtime called stream_gemini with
        // project=None, causing Code Assist to reject the request. The runtime
        // must resolve the user's project through the broker (setup_user) and
        // put it on the envelope so /v1internal:streamGenerateContent succeeds.
        let stub = Arc::new(StubBroker::new(vec![chunk(
            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}]}}\n",
        )]));
        let seen = stub.seen.clone();
        let broker: Arc<dyn CredentialBroker> = stub;
        let (tx, _rx) = mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let msgs: Vec<crate::SharedMessage> = vec![Arc::new(json!({"role":"user","content":"hi"}))];

        call_google_gemini_stream_inner(&cfg(), &broker, &[], &None, &msgs, &tx, &cancel)
            .await
            .expect("stream should succeed once project is resolved");

        let request = seen
            .lock()
            .unwrap()
            .take()
            .expect("stream request should be recorded");
        assert_eq!(request.path, "/v1internal:streamGenerateContent");
        let body = request.body.as_ref().expect("stream request has body");
        assert_eq!(
            body["project"].as_str(),
            Some("test-proj"),
            "runtime must forward the setup-resolved project id on the envelope: {body}",
        );
    }

    /// Live Code Assist 429 body as surfaced through the broker transport
    /// error (2026-07-13, gemini-2.5-pro, project witty-bonito-c3bk6).
    const LIVE_429_TEXT: &str = "gemini stream error: broker transport error: \
        provider request failed: 429 Too Many Requests: {   \"error\": {     \
        \"code\": 429,     \"message\": \"You have exhausted your capacity on \
        this model. Your quota will reset after 30s.\",     \"status\": \
        \"RESOURCE_EXHAUSTED\",     \"details\": [       {         \"@type\": \
        \"type.googleapis.com/google.rpc.ErrorInfo\",         \"reason\": \
        \"RATE_LIMIT_EXCEEDED\",         \"domain\": \
        \"cloudcode-pa.googleapis.com\"       }     ]   } }";

    #[test]
    fn code_assist_429_reset_parses_live_reset_hint() {
        assert_eq!(code_assist_429_reset(LIVE_429_TEXT), Some(Some(30)));
    }

    #[test]
    fn code_assist_429_reset_handles_429_without_hint() {
        let text = "provider request failed: 429 Too Many Requests: RESOURCE_EXHAUSTED";
        assert_eq!(code_assist_429_reset(text), Some(None));
    }

    #[test]
    fn code_assist_429_reset_ignores_non_rate_limit_errors() {
        for text in [
            "provider request failed: 404 Not Found: NOT_FOUND",
            "provider request failed: 400 Bad Request: INVALID_ARGUMENT",
            "broker transport error: connection reset",
            // A 429 status alone without a rate-limit marker is not enough.
            "some error mentioning 429 with no markers",
        ] {
            assert_eq!(code_assist_429_reset(text), None, "{text}");
        }
    }

    /// Broker whose `proxy_stream` rejects the first N attempts with the live
    /// Code Assist 429 transport error, then serves the given chunks.
    struct RateLimitedBroker {
        failures_remaining: Mutex<u32>,
        calls: Mutex<u32>,
        chunks: Mutex<Option<Vec<Result<bytes::Bytes, BrokerError>>>>,
    }

    impl RateLimitedBroker {
        fn new(failures: u32, chunks: Vec<Result<bytes::Bytes, BrokerError>>) -> Self {
            Self {
                failures_remaining: Mutex::new(failures),
                calls: Mutex::new(0),
                chunks: Mutex::new(Some(chunks)),
            }
        }
        fn calls(&self) -> u32 {
            *self.calls.lock().unwrap()
        }
    }

    #[async_trait]
    impl CredentialBroker for RateLimitedBroker {
        async fn access_token(&self, _p: OAuthProviderId) -> Result<AccessToken, BrokerError> {
            Err(BrokerError::NotConfigured("stub".into()))
        }
        async fn proxy(&self, r: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
            if r.path == "/v1internal:loadCodeAssist" {
                return Ok(ProxyResponse {
                    status: 200,
                    body: r#"{"cloudaicompanionProject":"test-proj","currentTier":{"id":"STANDARD","name":"Std","hasOnboardedPreviously":true}}"#.to_string(),
                });
            }
            Err(BrokerError::Denied("not implemented".into()))
        }
        async fn proxy_stream(
            &self,
            _request: ProxyRequest,
        ) -> Result<ProxyByteStream, BrokerError> {
            *self.calls.lock().unwrap() += 1;
            let mut failures = self.failures_remaining.lock().unwrap();
            if *failures > 0 {
                *failures -= 1;
                return Err(BrokerError::Transport(
                    "provider request failed: 429 Too Many Requests: {\"error\":{\"code\":429,\
                     \"message\":\"You have exhausted your capacity on this model. Your quota \
                     will reset after 1s.\",\"status\":\"RESOURCE_EXHAUSTED\"}}"
                        .to_string(),
                ));
            }
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

    #[tokio::test(start_paused = true)]
    async fn retries_429_after_reported_reset_and_succeeds() {
        let stub = Arc::new(RateLimitedBroker::new(
            1,
            vec![chunk(
                "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}]}}\n",
            )],
        ));
        let broker: Arc<dyn CredentialBroker> = stub.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let msgs: Vec<crate::SharedMessage> = vec![Arc::new(json!({"role":"user","content":"hi"}))];

        let out = call_google_gemini_stream_inner(&cfg(), &broker, &[], &None, &msgs, &tx, &cancel)
            .await
            .expect("429 followed by success must recover");
        drop(tx);

        assert_eq!(out["content"][0]["text"], "ok");
        assert_eq!(stub.calls(), 2, "one 429 rejection, one successful retry");

        // The wait is honored and user-visible.
        let mut saw_notice = false;
        while let Ok(ev) = rx.try_recv() {
            if let StreamEvent::Session(crate::runtime::types::SessionEvent::Notice(n)) = ev {
                assert!(n.to_lowercase().contains("rate limit"), "{n}");
                saw_notice = true;
            }
        }
        assert!(saw_notice, "429 backoff must emit a user-visible notice");
    }

    #[tokio::test(start_paused = true)]
    async fn does_not_retry_non_429_stream_errors() {
        struct HardFailBroker {
            calls: Mutex<u32>,
        }
        #[async_trait]
        impl CredentialBroker for HardFailBroker {
            async fn access_token(&self, _p: OAuthProviderId) -> Result<AccessToken, BrokerError> {
                Err(BrokerError::NotConfigured("stub".into()))
            }
            async fn proxy(&self, r: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
                if r.path == "/v1internal:loadCodeAssist" {
                    return Ok(ProxyResponse {
                        status: 200,
                        body: r#"{"cloudaicompanionProject":"test-proj","currentTier":{"id":"STANDARD","name":"Std","hasOnboardedPreviously":true}}"#.to_string(),
                    });
                }
                Err(BrokerError::Denied("not implemented".into()))
            }
            async fn proxy_stream(&self, _r: ProxyRequest) -> Result<ProxyByteStream, BrokerError> {
                *self.calls.lock().unwrap() += 1;
                Err(BrokerError::Transport(
                    "provider request failed: 404 Not Found: NOT_FOUND".to_string(),
                ))
            }
            async fn anthropic_usage(&self) -> Result<Value, BrokerError> {
                Err(BrokerError::Denied("not implemented".into()))
            }
            async fn capabilities(&self) -> Result<Vec<ProviderStatus>, BrokerError> {
                Ok(vec![])
            }
        }

        let stub = Arc::new(HardFailBroker {
            calls: Mutex::new(0),
        });
        let calls = || *stub.calls.lock().unwrap();
        let broker: Arc<dyn CredentialBroker> = stub.clone();
        let (tx, _rx) = mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let msgs: Vec<crate::SharedMessage> = vec![Arc::new(json!({"role":"user","content":"hi"}))];

        let err = call_google_gemini_stream_inner(&cfg(), &broker, &[], &None, &msgs, &tx, &cancel)
            .await
            .expect_err("404 must fail immediately");
        assert!(err.to_string().contains("404"), "{err}");
        assert_eq!(calls(), 1, "non-429 errors must not be retried");
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_after_429_retry_budget_is_exhausted() {
        let stub = Arc::new(RateLimitedBroker::new(u32::MAX, vec![]));
        let broker: Arc<dyn CredentialBroker> = stub.clone();
        let (tx, _rx) = mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let msgs: Vec<crate::SharedMessage> = vec![Arc::new(json!({"role":"user","content":"hi"}))];

        let err = call_google_gemini_stream_inner(&cfg(), &broker, &[], &None, &msgs, &tx, &cancel)
            .await
            .expect_err("persistent 429 must eventually surface");
        assert!(err.to_string().contains("429"), "{err}");
        assert!(
            !err.to_string().contains("You have exhausted"),
            "provider 429 body text must not surface (spec §5.1): {err}"
        );
        assert_eq!(
            stub.calls(),
            1 + MAX_GEMINI_429_RETRIES,
            "429 retries must stop at the budget"
        );
    }

    /// Phase 1 privacy (spec §5.1): the broker flattens an upstream HTTP
    /// failure into a transport error that carries a response-body snippet.
    /// A hostile provider echoes the full request there; the runtime must
    /// surface status + provider label only — never the snippet.
    #[tokio::test(start_paused = true)]
    async fn non_429_broker_error_never_surfaces_provider_body() {
        const SENTINEL: &str = "PH1-GEMINI-SENTINEL-5a9c-RAW";
        struct EchoFailBroker;
        #[async_trait]
        impl CredentialBroker for EchoFailBroker {
            async fn access_token(&self, _p: OAuthProviderId) -> Result<AccessToken, BrokerError> {
                Err(BrokerError::NotConfigured("stub".into()))
            }
            async fn proxy(&self, r: ProxyRequest) -> Result<ProxyResponse, BrokerError> {
                if r.path == "/v1internal:loadCodeAssist" {
                    return Ok(ProxyResponse {
                        status: 200,
                        body: r#"{"cloudaicompanionProject":"test-proj","currentTier":{"id":"STANDARD","name":"Std","hasOnboardedPreviously":true}}"#.to_string(),
                    });
                }
                Err(BrokerError::Denied("not implemented".into()))
            }
            async fn proxy_stream(&self, _r: ProxyRequest) -> Result<ProxyByteStream, BrokerError> {
                Err(BrokerError::Transport(format!(
                    "provider request failed: 500 Internal Server Error: \
                     ECHOED:{{\"request\":{{\"contents\":[{{\"parts\":[{{\"text\":\
                     \"{SENTINEL}\"}}]}}]}}}}"
                )))
            }
            async fn anthropic_usage(&self) -> Result<Value, BrokerError> {
                Err(BrokerError::Denied("not implemented".into()))
            }
            async fn capabilities(&self) -> Result<Vec<ProviderStatus>, BrokerError> {
                Ok(vec![])
            }
        }

        let broker: Arc<dyn CredentialBroker> = Arc::new(EchoFailBroker);
        let (tx, _rx) = mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let msgs: Vec<crate::SharedMessage> = vec![Arc::new(json!({"role":"user","content":"hi"}))];

        let err = call_google_gemini_stream_inner(&cfg(), &broker, &[], &None, &msgs, &tx, &cancel)
            .await
            .expect_err("500 must fail")
            .to_string();

        assert!(err.contains("500"), "status must survive: {err}");
        for banned in ["ECHOED", SENTINEL, "\"contents\""] {
            assert!(
                !err.contains(banned),
                "provider body content `{banned}` leaked into the surfaced error: {err}"
            );
        }
    }
}
