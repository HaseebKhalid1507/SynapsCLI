//! Streaming path for OpenAI-compatible providers.
//!
//! Mirrors `ApiMethods::call_api_stream_inner` but speaks OpenAI chat/completions
//! and translates back to Anthropic-shaped events for the rest of the runtime.

use super::translate;
use super::types::{ChatMessage, OaiEvent, ProviderConfig, StreamOptions, ToolCall};
use super::wire::StreamDecoder;
use crate::runtime::types::StreamEvent;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;

/// Run a single streaming request against an OpenAI-compatible endpoint.
///
/// Returns the final assistant response as an Anthropic-shaped content Value
/// (`{"content": [..text.., ..tool_use..]}`) so the outer agent loop can keep
/// using the same handling as the native Anthropic path.
pub(crate) async fn call_oai_stream_inner(
    cfg: &ProviderConfig,
    client: &reqwest::Client,
    tools_schema: &[Value],
    system_prompt: &Option<String>,
    messages: &[Value],
    tx: &mpsc::UnboundedSender<StreamEvent>,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    thinking_budget: u32,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let oai_messages = translate::messages_to_oai(messages, system_prompt);
    let oai_tools = translate::tools_to_oai(tools_schema);
    let tools_opt = if oai_tools.is_empty() { None } else { Some(oai_tools) };

    // Google's OpenAI-compat endpoint rejects stream_options
    let stream_options = if cfg.base_url.contains("googleapis.com") {
        None
    } else {
        Some(StreamOptions { include_usage: true })
    };

    let mut body = serde_json::Map::new();
    body.insert("model".to_string(), json!(cfg.model.clone()));
    body.insert("messages".to_string(), serde_json::to_value(oai_messages)?);
    body.insert("stream".to_string(), json!(true));
    if let Some(stream_options) = stream_options {
        body.insert("stream_options".to_string(), serde_json::to_value(stream_options)?);
    }
    if let Some(max_tokens) = max_tokens {
        body.insert("max_tokens".to_string(), json!(max_tokens));
    }
    if let Some(temperature) = temperature {
        body.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(tools) = tools_opt {
        body.insert("tools".to_string(), serde_json::to_value(tools)?);
    }
    super::reasoning::apply_openai_reasoning_params(
        &mut body,
        super::reasoning::provider_for_key(&cfg.provider),
        &cfg.model,
        thinking_budget,
    );
    let body = Value::Object(body);

    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));

    tracing::debug!(url=%url, model=%cfg.model, "openai stream request");

    let resp = match client
        .post(&url)
        .bearer_auth(&cfg.api_key)
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            if e.is_connect() && url.contains("localhost") {
                return Err(format!(
                    "Can't reach local endpoint at {} — is Ollama/LM Studio running?",
                    url
                ).into());
            }
            return Err(e.into());
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("openai request failed: {status}: {text}").into());
    }

    let mut decoder = StreamDecoder::new();
    let mut accumulated_text = String::new();
    let mut tool_use_blocks: Vec<Value> = Vec::new();
    let mut buf = bytes::BytesMut::with_capacity(8 * 1024);
    let mut sink: Vec<OaiEvent> = Vec::with_capacity(4);
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = tokio::select! {
        chunk = stream.next() => chunk,
        _ = cancel.cancelled() => {
            return Err("request canceled".into());
        }
    } {
        let chunk = chunk?;
        buf.extend_from_slice(&chunk);

        // Scan for newline-delimited SSE lines (SIMD-accelerated via memchr)
        while let Some(nl) = memchr::memchr(b'\n', &buf) {
            let line_bytes = buf.split_to(nl + 1); // O(1) — ref-counted split
            let line = std::str::from_utf8(&line_bytes[..nl]).unwrap_or("");

            sink.clear();
            decoder.push_line(line, &mut sink);
            handle_events(&sink, tx, &mut accumulated_text, &mut tool_use_blocks);
        }
    }

    // Flush any remaining buffered line + final Done
    if !buf.is_empty() {
        let line = std::str::from_utf8(&buf).unwrap_or("");
        sink.clear();
        decoder.push_line(line, &mut sink);
        handle_events(&sink, tx, &mut accumulated_text, &mut tool_use_blocks);
    }
    sink.clear();
    decoder.finish(&mut sink);
    handle_events(&sink, tx, &mut accumulated_text, &mut tool_use_blocks);

    // Build Anthropic-shaped final response
    let mut content: Vec<Value> = Vec::new();
    if !accumulated_text.is_empty() {
        content.push(json!({"type": "text", "text": accumulated_text}));
    }
    content.extend(tool_use_blocks);

    Ok(json!({
        "role": "assistant",
        "content": content,
    }))
}

pub(crate) async fn call_codex_stream_inner(
    cfg: &ProviderConfig,
    client: &reqwest::Client,
    tools_schema: &[Value],
    system_prompt: &Option<String>,
    messages: &[Value],
    tx: &mpsc::UnboundedSender<StreamEvent>,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let creds = if cfg.api_key.is_empty() {
        crate::auth::ensure_fresh_provider_token(client, "openai-codex").await?
    } else {
        crate::auth::OAuthCredentials {
            auth_type: "oauth".to_string(),
            refresh: String::new(),
            access: cfg.api_key.clone(),
            expires: 0,
            account_id: None,
        }
    };
    let account_id = creds
        .account_id
        .clone()
        .or_else(|| crate::auth::extract_codex_account_id(&creds.access))
        .ok_or("Failed to extract ChatGPT account id from Codex token")?;

    let oai_messages = translate::messages_to_oai(messages, system_prompt);
    let oai_tools = translate::tools_to_oai(tools_schema);
    let tools: Vec<Value> = oai_tools
        .into_iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.function.name,
                "description": tool.function.description.unwrap_or_default(),
                "parameters": tool.function.parameters,
            })
        })
        .collect();

    let mut body = json!({
        "model": cfg.model,
        "store": false,
        "stream": true,
        "instructions": system_prompt.clone().unwrap_or_default(),
        "input": codex_input_messages(oai_messages),
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "include": ["reasoning.encrypted_content"],
        "text": { "verbosity": "medium" },
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if let Some(temp) = temperature {
        body["temperature"] = json!(temp);
    }
    if let Some(max) = max_tokens {
        body["max_output_tokens"] = json!(max);
    }

    let url = format!(
        "{}/codex/responses",
        cfg.base_url.trim_end_matches('/').trim_end_matches("/codex")
    );
    tracing::debug!(url=%url, model=%cfg.model, "codex stream request");

    let resp = client
        .post(&url)
        .bearer_auth(&creds.access)
        .header("chatgpt-account-id", account_id)
        .header("originator", "synaps")
        .header("OpenAI-Beta", "responses=experimental")
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("codex request failed: {status}: {text}").into());
    }

    let mut accumulated_text = String::new();
    let mut parser = CodexSseDecoder::default();
    let mut buf = bytes::BytesMut::with_capacity(8 * 1024);
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = tokio::select! {
        chunk = stream.next() => chunk,
        _ = cancel.cancelled() => {
            return Err("request canceled".into());
        }
    } {
        let chunk = chunk?;
        buf.extend_from_slice(&chunk);
        while let Some(nl) = memchr::memchr(b'\n', &buf) {
            let line_bytes = buf.split_to(nl + 1);
            let line = std::str::from_utf8(&line_bytes[..nl]).unwrap_or("");
            parser.push_line(line, tx, &mut accumulated_text);
        }
    }
    if !buf.is_empty() {
        let line = std::str::from_utf8(&buf).unwrap_or("");
        parser.push_line(line, tx, &mut accumulated_text);
    }
    parser.finish();

    let mut content: Vec<Value> = Vec::new();
    if !accumulated_text.is_empty() {
        content.push(json!({"type": "text", "text": accumulated_text}));
    }
    content.extend(translate::tool_calls_to_content_blocks(&parser.completed_tools));

    Ok(json!({
        "role": "assistant",
        "content": content,
    }))
}

fn codex_input_messages(messages: Vec<ChatMessage>) -> Vec<Value> {
    let mut out = Vec::new();
    for msg in messages {
        if let Some(tool_calls) = msg.tool_calls {
            for call in tool_calls {
                // The Responses API rejects `id` values that are not the
                // original `fc_…` output-item id. We only carry the
                // `call_…` correlation id today (see types::ToolCall),
                // so emit `id` *only* when the value actually starts
                // with `fc`. `call_id` is sufficient on its own to
                // correlate the eventual `function_call_output`.
                let mut item = json!({
                    "type": "function_call",
                    "call_id": call.id,
                    "name": call.function.name,
                    "arguments": call.function.arguments,
                });
                if call.id.starts_with("fc") {
                    item["id"] = Value::from(call.id.clone());
                }
                out.push(item);
            }
            continue;
        }
        if msg.role == "tool" {
            out.push(json!({
                "type": "function_call_output",
                "call_id": msg.tool_call_id.unwrap_or_default(),
                "output": msg.content.unwrap_or_default(),
            }));
            continue;
        }
        out.push(json!({
            "role": msg.role,
            "content": msg.content.unwrap_or_default(),
        }));
    }
    out
}

#[derive(Default)]
struct CodexSseDecoder {
    buffer: String,
    active_tools: Vec<CodexToolAccumulator>,
    completed_tools: Vec<ToolCall>,
}

#[derive(Default)]
struct CodexToolAccumulator {
    id: String,
    name: String,
    arguments: String,
    started: bool,
}

impl CodexSseDecoder {
    fn push_line(
        &mut self,
        line: &str,
        tx: &mpsc::UnboundedSender<StreamEvent>,
        text_acc: &mut String,
    ) {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            if !self.buffer.is_empty() {
                let payload = std::mem::take(&mut self.buffer);
                self.push_payload(&payload, tx, text_acc);
            }
            return;
        }
        let Some(data) = line.strip_prefix("data:").map(str::trim_start) else {
            return;
        };
        if data == "[DONE]" {
            self.finish();
            return;
        }
        self.buffer.push_str(data);
    }

    fn push_payload(
        &mut self,
        payload: &str,
        tx: &mpsc::UnboundedSender<StreamEvent>,
        text_acc: &mut String,
    ) {
        let Ok(event) = serde_json::from_str::<Value>(payload) else {
            return;
        };
        let event_type = event.get("type").and_then(Value::as_str).unwrap_or_default();
        match event_type {
            "response.output_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    text_acc.push_str(delta);
                    let _ = tx.send(StreamEvent::Llm(crate::runtime::types::LlmEvent::Text(
                        delta.to_string(),
                    )));
                }
            }
            "response.output_item.added" => {
                if let Some(item) = event.get("item") {
                    let idx = event.get("output_index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    self.add_tool_from_item(idx, item, tx);
                }
            }
            "response.function_call_arguments.delta" => {
                let idx = event.get("output_index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let delta = event.get("delta").and_then(Value::as_str).unwrap_or_default();
                if !delta.is_empty() {
                    let tool = self.ensure_tool(idx);
                    tool.arguments.push_str(delta);
                    let _ = tx.send(StreamEvent::Llm(
                        crate::runtime::types::LlmEvent::ToolUseDelta(delta.to_string()),
                    ));
                }
            }
            "response.output_item.done" => {
                if let Some(item) = event.get("item") {
                    let idx = event.get("output_index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    self.complete_tool_from_item(idx, item, tx);
                }
            }
            "response.completed" | "response.done" => {
                self.push_usage(&event, tx);
                self.finish();
            }
            _ => {}
        }
    }

    fn ensure_tool(&mut self, idx: usize) -> &mut CodexToolAccumulator {
        while self.active_tools.len() <= idx {
            self.active_tools.push(CodexToolAccumulator::default());
        }
        &mut self.active_tools[idx]
    }

    fn add_tool_from_item(
        &mut self,
        idx: usize,
        item: &Value,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) {
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return;
        }
        let tool = self.ensure_tool(idx);
        if let Some(id) = item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
        {
            tool.id = id.to_string();
        }
        if let Some(name) = item.get("name").and_then(Value::as_str) {
            tool.name = name.to_string();
        }
        if !tool.started && !tool.name.is_empty() {
            tool.started = true;
            let _ = tx.send(StreamEvent::Llm(
                crate::runtime::types::LlmEvent::ToolUseStart(tool.name.clone()),
            ));
        }
    }

    fn complete_tool_from_item(
        &mut self,
        idx: usize,
        item: &Value,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) {
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return;
        }
        let tool = self.ensure_tool(idx);
        if let Some(id) = item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
        {
            tool.id = id.to_string();
        }
        if let Some(name) = item.get("name").and_then(Value::as_str) {
            tool.name = name.to_string();
        }
        if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
            tool.arguments = arguments.to_string();
        }
        if !tool.started && !tool.name.is_empty() {
            tool.started = true;
            let _ = tx.send(StreamEvent::Llm(
                crate::runtime::types::LlmEvent::ToolUseStart(tool.name.clone()),
            ));
        }
        let completed = if !tool.id.is_empty() && !tool.name.is_empty() {
            Some(ToolCall {
                id: tool.id.clone(),
                kind: "function".to_string(),
                function: super::types::FunctionCall {
                    name: tool.name.clone(),
                    arguments: tool.arguments.clone(),
                },
            })
        } else {
            None
        };
        if let Some(call) = completed {
            if self.completed_tools.iter().any(|done| done.id == call.id) {
                return;
            }
            self.completed_tools.push(ToolCall {
                id: call.id,
                kind: call.kind,
                function: call.function,
            });
        }
    }

    fn push_usage(&self, event: &Value, tx: &mpsc::UnboundedSender<StreamEvent>) {
        let usage = event
            .get("response")
            .and_then(|r| r.get("usage"))
            .or_else(|| event.get("usage"));
        let input = usage
            .and_then(|u| u.get("input_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let output = usage
            .and_then(|u| u.get("output_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if input > 0 || output > 0 {
            let _ = tx.send(StreamEvent::Session(crate::runtime::types::SessionEvent::Usage {
                input_tokens: input,
                output_tokens: output,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                model: None,
            }));
        }
    }

    fn finish(&mut self) {
        for tool in self.active_tools.drain(..) {
            if !tool.id.is_empty()
                && !tool.name.is_empty()
                && !self.completed_tools.iter().any(|done| done.id == tool.id)
            {
                self.completed_tools.push(ToolCall {
                    id: tool.id,
                    kind: "function".to_string(),
                    function: super::types::FunctionCall {
                        name: tool.name,
                        arguments: tool.arguments,
                    },
                });
            }
        }
    }
}

fn handle_events(
    events: &[OaiEvent],
    tx: &mpsc::UnboundedSender<StreamEvent>,
    text_acc: &mut String,
    tool_blocks: &mut Vec<Value>,
) {
    for ev in events {
        if let OaiEvent::TextDelta(t) = ev {
            text_acc.push_str(t);
        }
        if let OaiEvent::ToolCallsComplete { calls, .. } = ev {
            tool_blocks.extend(translate::tool_calls_to_content_blocks(calls));
        }
        if let Some(se) = translate::oai_event_to_llm(ev) {
            let _ = tx.send(se);
        }
    }
}

#[cfg(test)]
mod codex_input_messages_tests {
    //! Regression tests for the Codex Responses-API `input` shape.
    //!
    //! Background: the Responses API distinguishes two ids per tool
    //! invocation — `id` (the *output item id*, prefix `fc_…`) and
    //! `call_id` (the *function call id*, prefix `call_…`). When echoing
    //! a previous `function_call` back as an input item, supplying an
    //! `id` whose value is *not* a `fc_…` triggers
    //!
    //!   400 Bad Request: Invalid 'input[N].id': 'call_…'.
    //!   Expected an ID that begins with 'fc'.
    //!
    //! `id` is *optional* on input items — only `call_id` is required to
    //! correlate the eventual `function_call_output`. We elect not to
    //! emit `id` unless we actually have a real `fc_…` value to send.

    use super::*;
    use super::super::types::{ChatMessage, FunctionCall, ToolCall};

    fn sample_tool_call() -> ToolCall {
        ToolCall {
            id: "call_nZYquCuGUh8Qs9H51dwHMDgs".to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: "bash".to_string(),
                arguments: r#"{"command":"ls"}"#.to_string(),
            },
        }
    }

    #[test]
    fn function_call_input_omits_non_fc_id() {
        let messages = vec![ChatMessage::assistant_tool_calls(vec![sample_tool_call()])];
        let out = codex_input_messages(messages);
        assert_eq!(out.len(), 1, "one tool_call → one input item");
        let item = &out[0];
        assert_eq!(item.get("type").and_then(Value::as_str), Some("function_call"));
        assert!(
            item.get("id").is_none(),
            "must not echo a non-`fc_` id back; got {:?}",
            item.get("id"),
        );
        assert_eq!(
            item.get("call_id").and_then(Value::as_str),
            Some("call_nZYquCuGUh8Qs9H51dwHMDgs"),
        );
        assert_eq!(item.get("name").and_then(Value::as_str), Some("bash"));
    }

    #[test]
    fn function_call_input_keeps_real_fc_id() {
        // If we ever do have a genuine `fc_…` id (round-tripped from the
        // Responses API), we *should* echo it.
        let mut call = sample_tool_call();
        call.id = "fc_abc123".to_string();
        let messages = vec![ChatMessage::assistant_tool_calls(vec![call])];
        let out = codex_input_messages(messages);
        let item = &out[0];
        assert_eq!(item.get("id").and_then(Value::as_str), Some("fc_abc123"));
        assert_eq!(item.get("call_id").and_then(Value::as_str), Some("fc_abc123"));
    }

    #[test]
    fn function_call_output_round_trips_call_id() {
        // The follow-up tool message must reference the original call_id.
        let messages = vec![ChatMessage::tool_result(
            "call_nZYquCuGUh8Qs9H51dwHMDgs",
            "bash",
            "total 0",
        )];
        let out = codex_input_messages(messages);
        let item = &out[0];
        assert_eq!(
            item.get("type").and_then(Value::as_str),
            Some("function_call_output"),
        );
        assert_eq!(
            item.get("call_id").and_then(Value::as_str),
            Some("call_nZYquCuGUh8Qs9H51dwHMDgs"),
        );
        assert_eq!(item.get("output").and_then(Value::as_str), Some("total 0"));
    }
}

#[cfg(test)]
mod codex_decoder_tests {
    //! Regression tests for `CodexSseDecoder`.
    //!
    //! The decoder is sync — we drive it via `push_line` and capture
    //! emitted `StreamEvent`s from an `unbounded_channel` using
    //! `try_recv`, no async runtime needed.

    use super::*;
    use crate::runtime::types::{LlmEvent, SessionEvent, StreamEvent};

    fn collect_events(rx: &mut mpsc::UnboundedReceiver<StreamEvent>) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    fn drive(lines: &[&str]) -> (CodexSseDecoder, String, Vec<StreamEvent>) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut decoder = CodexSseDecoder::default();
        let mut text_acc = String::new();
        for line in lines {
            decoder.push_line(line, &tx, &mut text_acc);
        }
        let events = collect_events(&mut rx);
        (decoder, text_acc, events)
    }

    #[test]
    fn text_delta_aggregates_into_text_acc_and_emits_text_events() {
        let lines = [
            r#"data: {"type":"response.output_text.delta","delta":"Hello, "}"#,
            "",
            r#"data: {"type":"response.output_text.delta","delta":"world!"}"#,
            "",
        ];
        let (_decoder, text_acc, events) = drive(&lines);
        assert_eq!(text_acc, "Hello, world!");
        let texts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Llm(LlmEvent::Text(t)) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["Hello, ", "world!"]);
    }

    #[test]
    fn single_function_call_completes_via_output_item_done() {
        let lines = [
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_abc","name":"bash"}}"#,
            "",
            r#"data: {"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"cmd\""}"#,
            "",
            r#"data: {"type":"response.function_call_arguments.delta","output_index":0,"delta":":\"ls\"}"}"#,
            "",
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call_abc","name":"bash","arguments":"{\"cmd\":\"ls\"}"}}"#,
            "",
        ];
        let (decoder, _text, events) = drive(&lines);

        assert_eq!(decoder.completed_tools.len(), 1);
        let tool = &decoder.completed_tools[0];
        assert_eq!(tool.id, "call_abc");
        assert_eq!(tool.function.name, "bash");
        assert_eq!(tool.function.arguments, r#"{"cmd":"ls"}"#);

        // Exactly one ToolUseStart for the tool.
        let starts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Llm(LlmEvent::ToolUseStart(name)) => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(starts, vec!["bash"], "exactly one ToolUseStart");

        // Two argument deltas streamed.
        let deltas: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Llm(LlmEvent::ToolUseDelta(d)) => Some(d.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec![r#"{"cmd""#, r#":"ls"}"#]);
    }

    #[test]
    fn parallel_tool_calls_indexed_separately() {
        let lines = [
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"bash"}}"#,
            "",
            r#"data: {"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call_2","name":"read"}}"#,
            "",
            r#"data: {"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"path\":\"a\"}"}"#,
            "",
            r#"data: {"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"cmd\":\"ls\"}"}"#,
            "",
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"bash","arguments":"{\"cmd\":\"ls\"}"}}"#,
            "",
            r#"data: {"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","call_id":"call_2","name":"read","arguments":"{\"path\":\"a\"}"}}"#,
            "",
        ];
        let (decoder, _text, _events) = drive(&lines);

        assert_eq!(decoder.completed_tools.len(), 2);
        let mut by_id: std::collections::BTreeMap<&str, &ToolCall> = std::collections::BTreeMap::new();
        for tool in &decoder.completed_tools {
            by_id.insert(tool.id.as_str(), tool);
        }
        assert_eq!(by_id["call_1"].function.name, "bash");
        assert_eq!(by_id["call_1"].function.arguments, r#"{"cmd":"ls"}"#);
        assert_eq!(by_id["call_2"].function.name, "read");
        assert_eq!(by_id["call_2"].function.arguments, r#"{"path":"a"}"#);
    }

    #[test]
    fn response_completed_emits_usage_event() {
        let lines = [
            r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":42,"output_tokens":17}}}"#,
            "",
        ];
        let (_decoder, _text, events) = drive(&lines);
        let usage = events.iter().find_map(|e| match e {
            StreamEvent::Session(SessionEvent::Usage {
                input_tokens,
                output_tokens,
                ..
            }) => Some((*input_tokens, *output_tokens)),
            _ => None,
        });
        assert_eq!(usage, Some((42, 17)));
    }

    #[test]
    fn response_completed_with_zero_usage_emits_nothing() {
        let lines = [
            r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":0,"output_tokens":0}}}"#,
            "",
        ];
        let (_decoder, _text, events) = drive(&lines);
        let any_usage = events.iter().any(|e| matches!(e, StreamEvent::Session(SessionEvent::Usage { .. })));
        assert!(!any_usage, "zero-token usage should be suppressed");
    }

    #[test]
    fn done_marker_finishes_decoder() {
        let lines = [
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_x","name":"bash"}}"#,
            "",
            r#"data: {"type":"response.function_call_arguments.delta","output_index":0,"delta":"{}"}"#,
            "",
            "data: [DONE]",
            "",
        ];
        let (decoder, _text, _events) = drive(&lines);
        // active_tools promoted to completed_tools by finish().
        assert_eq!(decoder.completed_tools.len(), 1);
        assert_eq!(decoder.completed_tools[0].id, "call_x");
        assert_eq!(decoder.completed_tools[0].function.arguments, "{}");
    }

    #[test]
    fn finish_is_idempotent_no_double_emit() {
        let lines = [
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call_y","name":"bash","arguments":"{}"}}"#,
            "",
        ];
        let (mut decoder, _text, _events) = drive(&lines);
        assert_eq!(decoder.completed_tools.len(), 1);

        // Calling finish() again must not duplicate the tool.
        decoder.finish();
        assert_eq!(
            decoder.completed_tools.len(),
            1,
            "finish() called twice must not double-emit"
        );
    }

    #[test]
    fn finish_drains_active_tools_for_state_hygiene() {
        // After [DONE], any leftover active tool entries should have been
        // promoted *and* drained from `active_tools`. This guards against
        // future code paths that re-call finish() (or new event types
        // that would otherwise re-iterate the old buffer).
        let lines = [
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_z","name":"bash"}}"#,
            "",
            "data: [DONE]",
            "",
        ];
        let (decoder, _text, _events) = drive(&lines);
        assert_eq!(decoder.completed_tools.len(), 1);
        assert!(
            decoder.active_tools.is_empty(),
            "active_tools must be drained after finish()"
        );
    }

    #[test]
    fn unknown_event_types_are_ignored() {
        let lines = [
            r#"data: {"type":"response.future_unknown_event","payload":{"x":1}}"#,
            "",
            r#"data: {"type":"response.output_text.delta","delta":"hi"}"#,
            "",
        ];
        let (_decoder, text_acc, _events) = drive(&lines);
        assert_eq!(text_acc, "hi");
    }
}
