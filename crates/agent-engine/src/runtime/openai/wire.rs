//! SSE wire decoder for OpenAI-compatible streams. Ported from
//! `openai-runtime::stream`, with `StreamEvent` renamed to `OaiEvent`.

use super::types::{FunctionCall, OaiEvent, ToolCall};
use serde::Deserialize;
use std::collections::HashMap;

// ─── Wire types ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RawChunk {
    #[serde(default)]
    pub choices: Vec<RawChoice>,
    #[serde(default)]
    pub usage: Option<RawUsage>,
}

#[derive(Debug, Deserialize)]
pub struct RawChoice {
    #[serde(default)]
    pub delta: Option<RawDelta>,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct RawDelta {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<RawToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
pub struct RawToolCallDelta {
    #[serde(default)]
    pub index: Option<u32>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub function: Option<RawFunctionDelta>,
}

#[derive(Debug, Deserialize)]
pub struct RawFunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RawUsage {
    #[serde(default)]
    pub prompt_tokens: Option<u32>,
    #[serde(default)]
    pub completion_tokens: Option<u32>,
    #[serde(default)]
    pub prompt_tokens_details: Option<RawPromptTokensDetails>,
}

#[derive(Debug, Deserialize)]
pub struct RawPromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u32>,
}

/// Legacy text-only SSE line parser. Kept for simple use-cases; the main
/// decoder path is `StreamDecoder`.
pub fn parse_sse_line(line: &str) -> Option<OaiEvent> {
    let line = line.trim_end_matches('\r');
    if line.is_empty() || line.starts_with(':') {
        return None;
    }
    let data = line.strip_prefix("data:")?.trim_start();
    if data == "[DONE]" {
        return Some(OaiEvent::Done);
    }
    let chunk: RawChunk = serde_json::from_str(data).ok()?;
    let delta = chunk.choices.into_iter().next()?.delta?;
    if let Some(role) = delta.role {
        return Some(OaiEvent::RoleStart(role));
    }
    if let Some(content) = delta.content {
        if !content.is_empty() {
            return Some(OaiEvent::TextDelta(content));
        }
    }
    None
}

// ─── Accumulator ─────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct ToolCallAccumulator {
    pub id: String,
    pub name: String,
    pub arguments: String,
    started: bool,
}

#[derive(Debug)]
pub struct StreamDecoder {
    pub calls: HashMap<u32, ToolCallAccumulator>,
    pub truncated: bool,
    pub completed: bool,
    role_emitted: bool,
    done_emitted: bool,
}

impl Default for StreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamDecoder {
    pub fn new() -> Self {
        Self {
            calls: HashMap::new(),
            truncated: false,
            completed: false,
            role_emitted: false,
            done_emitted: false,
        }
    }

    pub fn push_line<E: Extend<OaiEvent>>(&mut self, line: &str, sink: &mut E) {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            return;
        }
        let payload = match line.strip_prefix("data:").map(str::trim_start) {
            Some(p) => p,
            None => return,
        };
        if payload == "[DONE]" {
            self.finish(sink);
            return;
        }
        match serde_json::from_str::<RawChunk>(payload) {
            Ok(chunk) => self.push_chunk(chunk, sink),
            // Privacy (spec §5.1): `payload` is provider-controlled and may
            // echo the request — never include its bytes in a warning that
            // feeds the log sink. serde's error carries position info only.
            Err(e) => sink.extend(Some(OaiEvent::Warning(format!(
                "sse parse error: {e} (payload {} bytes, not logged)",
                payload.len()
            )))),
        }
    }

    fn push_chunk<E: Extend<OaiEvent>>(&mut self, chunk: RawChunk, sink: &mut E) {
        for choice in chunk.choices {
            let is_finish = choice.finish_reason.is_some();
            if let Some(delta) = choice.delta {
                if let Some(role) = delta.role {
                    if !self.role_emitted {
                        self.role_emitted = true;
                        sink.extend(Some(OaiEvent::RoleStart(role)));
                    }
                }
                if let Some(text) = delta.content {
                    if !text.is_empty() {
                        sink.extend(Some(OaiEvent::TextDelta(text)));
                    }
                }
                // Process tool_calls — but de-dup finish-chunk re-sends.
                // Some providers re-send the full tool_calls on the finish frame.
                // We only skip if the chunk has finish_reason AND the tool_call
                // has an id (indicating a full re-send, not a final argument delta).
                if let Some(tcs) = delta.tool_calls {
                    for tc in tcs {
                        let is_resend =
                            is_finish && tc.id.as_ref().is_some_and(|id| !id.is_empty());
                        if !is_resend {
                            self.apply_tool_call_delta(tc, sink);
                        }
                    }
                }
            }
            if let Some(reason) = choice.finish_reason {
                match reason.as_str() {
                    "tool_calls" => self.flush_complete(sink),
                    "length" => {
                        if !self.calls.is_empty() {
                            self.truncated = true;
                            self.flush_complete(sink);
                        }
                    }
                    "stop" | "content_filter" => {}
                    // Privacy (spec §5.1): the value is provider-controlled;
                    // keep it out of warnings that feed the log sink.
                    _ => sink.extend(Some(OaiEvent::Warning(
                        "unknown finish_reason (value not logged)".to_string(),
                    ))),
                }
            }
        }
        if let Some(u) = chunk.usage {
            let cached = u
                .prompt_tokens_details
                .and_then(|d| d.cached_tokens)
                .unwrap_or(0);
            sink.extend(Some(OaiEvent::Usage {
                prompt_tokens: u.prompt_tokens.unwrap_or(0),
                completion_tokens: u.completion_tokens.unwrap_or(0),
                cached_tokens: cached,
            }));
        }
    }

    fn apply_tool_call_delta<E: Extend<OaiEvent>>(&mut self, tc: RawToolCallDelta, sink: &mut E) {
        let idx = tc.index.unwrap_or(0);
        let acc = self.calls.entry(idx).or_default();

        if let Some(id) = tc.id {
            if !id.is_empty() {
                acc.id = id;
            }
        }
        if let Some(f) = tc.function {
            if let Some(n) = f.name {
                if !n.is_empty() {
                    acc.name = n;
                }
            }
            if !acc.started && !acc.id.is_empty() && !acc.name.is_empty() {
                acc.started = true;
                sink.extend(Some(OaiEvent::ToolCallStart {
                    index: idx,
                    id: acc.id.clone(),
                    name: acc.name.clone(),
                }));
            }
            if let Some(args) = f.arguments {
                if !args.is_empty() {
                    acc.arguments.push_str(&args);
                    sink.extend(Some(OaiEvent::ToolCallArgumentsDelta {
                        index: idx,
                        id: acc.id.clone(),
                        delta: args,
                    }));
                }
            }
        } else if !acc.started && !acc.id.is_empty() && !acc.name.is_empty() {
            acc.started = true;
            sink.extend(Some(OaiEvent::ToolCallStart {
                index: idx,
                id: acc.id.clone(),
                name: acc.name.clone(),
            }));
        }
    }

    pub fn finish<E: Extend<OaiEvent>>(&mut self, sink: &mut E) {
        self.flush_complete(sink);
        if !self.done_emitted {
            self.done_emitted = true;
            sink.extend(Some(OaiEvent::Done));
        }
    }

    fn flush_complete<E: Extend<OaiEvent>>(&mut self, sink: &mut E) {
        if self.completed || self.calls.is_empty() {
            return;
        }
        self.completed = true;
        let mut entries: Vec<(u32, ToolCallAccumulator)> = self.calls.drain().collect();
        entries.sort_by_key(|(k, _)| *k);
        let calls: Vec<ToolCall> = entries
            .into_iter()
            .map(|(_, acc)| ToolCall {
                id: acc.id,
                kind: "function".to_string(),
                function: FunctionCall {
                    name: acc.name,
                    arguments: acc.arguments,
                },
            })
            .collect();
        sink.extend(Some(OaiEvent::ToolCallsComplete {
            calls,
            truncated: self.truncated,
        }));
    }
}

#[cfg(test)]
mod tests {
    //! Phase 1 privacy (spec §5.1): SSE payloads are provider-controlled and
    //! may echo the request. Decoder warnings feed `tracing::warn!` (see
    //! `translate::oai_event_to_stream_event`), so no provider-controlled
    //! bytes may ride along in a warning message.

    use super::*;

    #[test]
    fn parse_error_warning_never_contains_payload_bytes() {
        const SENTINEL: &str = "PH1-WIRE-SENTINEL-4c1a-RAW";
        let mut decoder = StreamDecoder::new();
        let mut sink: Vec<OaiEvent> = Vec::new();
        decoder.push_line(&format!("data: {{\"broken\": \"{SENTINEL}"), &mut sink);
        let [OaiEvent::Warning(w)] = sink.as_slice() else {
            panic!("malformed payload must produce exactly one warning, got {sink:?}");
        };
        assert!(w.contains("sse parse error"), "{w}");
        assert!(
            !w.contains(SENTINEL),
            "provider payload bytes leaked into a warning destined for logs: {w}"
        );
    }

    #[test]
    fn unknown_finish_reason_warning_never_contains_provider_value() {
        const SENTINEL: &str = "PH1-FINISH-SENTINEL-7e2b-RAW";
        let mut decoder = StreamDecoder::new();
        let mut sink: Vec<OaiEvent> = Vec::new();
        decoder.push_line(
            &format!("data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"{SENTINEL}\"}}]}}"),
            &mut sink,
        );
        let warning = sink.iter().find_map(|e| match e {
            OaiEvent::Warning(w) => Some(w.clone()),
            _ => None,
        });
        let w = warning.expect("unknown finish_reason must still warn");
        assert!(
            !w.contains(SENTINEL),
            "provider-controlled finish_reason leaked into a warning destined for logs: {w}"
        );
    }
}
