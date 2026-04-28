//! Streaming path for OpenAI-compatible providers.
//!
//! Mirrors `ApiMethods::call_api_stream_inner` but speaks OpenAI chat/completions
//! and translates back to Anthropic-shaped events for the rest of the runtime.

use super::translate;
use super::types::{ChatRequest, OaiEvent, ProviderConfig, StreamOptions};
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
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let (oai_tools, name_map) = translate::tools_to_oai(tools_schema);
    let oai_messages = translate::messages_to_oai(messages, system_prompt, &name_map);
    let tools_opt = if oai_tools.is_empty() { None } else { Some(oai_tools) };

    // Google's OpenAI-compat endpoint rejects stream_options
    let stream_options = if cfg.base_url.contains("googleapis.com") {
        None
    } else {
        Some(StreamOptions { include_usage: true })
    };

    let body = ChatRequest {
        model: cfg.model.clone(),
        messages: oai_messages,
        stream: true,
        stream_options,
        max_tokens,
        temperature,
        tools: tools_opt,
        tool_choice: None,
    };

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
            return Err("request cancelled".into());
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
            handle_events(&sink, tx, &mut accumulated_text, &mut tool_use_blocks, &name_map);
        }
    }

    // Flush any remaining buffered line + final Done
    if !buf.is_empty() {
        let line = std::str::from_utf8(&buf).unwrap_or("");
        sink.clear();
        decoder.push_line(line, &mut sink);
        handle_events(&sink, tx, &mut accumulated_text, &mut tool_use_blocks, &name_map);
    }
    sink.clear();
    decoder.finish(&mut sink);
    handle_events(&sink, tx, &mut accumulated_text, &mut tool_use_blocks, &name_map);

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

fn handle_events(
    events: &[OaiEvent],
    tx: &mpsc::UnboundedSender<StreamEvent>,
    text_acc: &mut String,
    tool_blocks: &mut Vec<Value>,
    name_map: &translate::ToolNameMap,
) {
    for ev in events {
        if let OaiEvent::TextDelta(t) = ev {
            text_acc.push_str(t);
        }
        if let OaiEvent::ToolCallsComplete { calls, .. } = ev {
            tool_blocks.extend(translate::tool_calls_to_content_blocks(calls, name_map));
        }
        if let Some(se) = translate::oai_event_to_llm(ev) {
            let _ = tx.send(se);
        }
    }
}
