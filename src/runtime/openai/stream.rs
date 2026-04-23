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
    model: &str,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let oai_messages = translate::messages_to_oai(messages, system_prompt);
    let oai_tools = translate::tools_to_oai(tools_schema);
    let tools_opt = if oai_tools.is_empty() { None } else { Some(oai_tools) };

    let body = ChatRequest {
        model: cfg.model.clone(),
        messages: oai_messages,
        stream: true,
        stream_options: Some(StreamOptions { include_usage: true }),
        max_tokens: None,
        temperature: None,
        tools: tools_opt,
        tool_choice: None,
    };

    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));

    tracing::debug!(url=%url, model=%model, "openai stream request");

    let resp = client
        .post(&url)
        .bearer_auth(&cfg.api_key)
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("openai request failed: {status}: {text}").into());
    }

    let mut decoder = StreamDecoder::new();
    let mut accumulated_text = String::new();
    let mut tool_use_blocks: Vec<Value> = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buf.extend_from_slice(&chunk);

        // Scan for newline-delimited SSE lines
        loop {
            let nl = match buf.iter().position(|&b| b == b'\n') {
                Some(i) => i,
                None => break,
            };
            let line_bytes: Vec<u8> = buf.drain(..=nl).collect();
            let line = std::str::from_utf8(&line_bytes[..line_bytes.len() - 1])
                .unwrap_or("")
                .to_string();

            let mut sink: Vec<OaiEvent> = Vec::new();
            decoder.push_line(&line, &mut sink);
            handle_events(&sink, tx, &mut accumulated_text, &mut tool_use_blocks);
        }
    }

    // Flush any remaining buffered line + final Done
    if !buf.is_empty() {
        let line = std::str::from_utf8(&buf).unwrap_or("").to_string();
        let mut sink: Vec<OaiEvent> = Vec::new();
        decoder.push_line(&line, &mut sink);
        handle_events(&sink, tx, &mut accumulated_text, &mut tool_use_blocks);
    }
    let mut sink: Vec<OaiEvent> = Vec::new();
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
