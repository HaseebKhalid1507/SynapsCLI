//! Translation between the runtime's generic chat/tool shape and the Gemini
//! Code Assist `streamGenerateContent` wire format (Vertex-style contents +
//! function calls). Also decodes the SSE line format used by
//! `cloudcode-pa.googleapis.com` when called with `?alt=sse`.

use serde::{Deserialize, Serialize};

/// Maximum bytes we will buffer for a single SSE line before failing closed.
/// Google's Code Assist emits reasonably small JSON chunks; refusing anything
/// bigger prevents a hostile upstream from ballooning memory.
pub const MAX_INBOUND_LINE_BYTES: usize = 1_048_576; // 1 MiB per line

// ── Outbound request ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct GeminiRole;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GeminiRoleName {
    User,
    Model,
    Function,
}

#[derive(Debug, Clone, Serialize)]
pub struct GeminiContent {
    pub role: GeminiRoleName,
    pub parts: Vec<GeminiPart>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum GeminiPart {
    Text {
        text: String,
    },
    FunctionCall {
        #[serde(rename = "functionCall")]
        function_call: GeminiFunctionCall,
    },
    FunctionResponse {
        #[serde(rename = "functionResponse")]
        function_response: GeminiFunctionResponse,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GeminiFunctionCall {
    pub name: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GeminiFunctionResponse {
    pub name: String,
    pub response: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct GeminiToolFunctionDeclaration {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "parametersJsonSchema"
    )]
    pub parameters_json_schema: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GeminiTool {
    #[serde(rename = "functionDeclarations")]
    pub function_declarations: Vec<GeminiToolFunctionDeclaration>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GeminiSystemInstruction {
    pub role: GeminiRoleName,
    pub parts: Vec<GeminiPart>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GeminiGenerateRequestInner {
    pub contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "systemInstruction")]
    pub system_instruction: Option<GeminiSystemInstruction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<GeminiTool>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "generationConfig")]
    pub generation_config: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "session_id")]
    pub session_id: Option<String>,
}

/// Envelope matching the Code Assist `streamGenerateContent` request shape:
/// `{ model, project?, user_prompt_id?, request: VertexGenerateContentRequest }`.
#[derive(Debug, Clone, Serialize)]
pub struct GeminiGenerateRequest {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "user_prompt_id")]
    pub user_prompt_id: Option<String>,
    pub request: GeminiGenerateRequestInner,
}

/// Minimal, generic input shape callers assemble before calling the translator.
/// Kept intentionally small: text / tool-call / tool-response only. Media
/// payloads are unsupported in this experimental slice.
#[derive(Debug, Clone)]
pub enum ChatTurn {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    ToolCall {
        name: String,
        args: serde_json::Value,
    },
    ToolResult {
        name: String,
        result: serde_json::Value,
    },
}

#[derive(Debug, Clone, Default)]
pub struct ToolSpec {
    pub name: String,
    pub description: Option<String>,
    pub parameters_json_schema: Option<serde_json::Value>,
}

/// Translate a normalized conversation + tool set into a
/// `GeminiGenerateRequest` ready for `/v1internal:streamGenerateContent`.
pub fn translate_generate_content_request(
    model: impl Into<String>,
    project: Option<String>,
    system_prompt: Option<String>,
    turns: &[ChatTurn],
    tools: &[ToolSpec],
) -> GeminiGenerateRequest {
    let contents: Vec<GeminiContent> = turns
        .iter()
        .map(|t| match t {
            ChatTurn::User { text } => GeminiContent {
                role: GeminiRoleName::User,
                parts: vec![GeminiPart::Text { text: text.clone() }],
            },
            ChatTurn::Assistant { text } => GeminiContent {
                role: GeminiRoleName::Model,
                parts: vec![GeminiPart::Text { text: text.clone() }],
            },
            ChatTurn::ToolCall { name, args } => GeminiContent {
                role: GeminiRoleName::Model,
                parts: vec![GeminiPart::FunctionCall {
                    function_call: GeminiFunctionCall {
                        name: name.clone(),
                        args: args.clone(),
                    },
                }],
            },
            ChatTurn::ToolResult { name, result } => GeminiContent {
                role: GeminiRoleName::Function,
                parts: vec![GeminiPart::FunctionResponse {
                    function_response: GeminiFunctionResponse {
                        name: name.clone(),
                        response: result.clone(),
                    },
                }],
            },
        })
        .collect();

    let system_instruction = system_prompt.map(|s| GeminiSystemInstruction {
        role: GeminiRoleName::User,
        parts: vec![GeminiPart::Text { text: s }],
    });

    let tools_wire = if tools.is_empty() {
        None
    } else {
        Some(vec![GeminiTool {
            function_declarations: tools
                .iter()
                .map(|t| GeminiToolFunctionDeclaration {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters_json_schema: t.parameters_json_schema.clone(),
                })
                .collect(),
        }])
    };

    GeminiGenerateRequest {
        model: model.into(),
        project,
        user_prompt_id: None,
        request: GeminiGenerateRequestInner {
            contents,
            system_instruction,
            tools: tools_wire,
            generation_config: None,
            session_id: None,
        },
    }
}

// ── Inbound SSE decoding ─────────────────────────────────────────────────────

/// A decoded, runtime-facing event from the Gemini stream. Text fragments and
/// tool calls are surfaced separately so the runtime can route them without
/// re-parsing the vendor shape.
#[derive(Debug, Clone, PartialEq)]
pub enum GeminiStreamEvent {
    TextDelta(String),
    ToolCall(GeminiFunctionCall),
    Finish {
        reason: Option<String>,
    },
    /// The upstream chunk was well-formed JSON but did not carry text/tool/
    /// finish info. Callers usually drop these.
    Ignored,
}

/// A single `data: ...` payload as decoded from the Code Assist SSE stream.
/// The full wire shape looks like `{"response": {...vertex...}}` optionally
/// alongside `traceId` and credit accounting we do not surface.
#[derive(Debug, Clone, Deserialize)]
struct CaResponseEnvelope {
    #[serde(default)]
    response: Option<VertexResponse>,
}

#[derive(Debug, Clone, Deserialize)]
struct VertexResponse {
    #[serde(default)]
    candidates: Vec<VertexCandidate>,
}

#[derive(Debug, Clone, Deserialize)]
struct VertexCandidate {
    #[serde(default)]
    content: Option<VertexContent>,
    #[serde(default, rename = "finishReason")]
    finish_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct VertexContent {
    #[serde(default)]
    parts: Vec<VertexPart>,
}

#[derive(Debug, Clone, Deserialize)]
struct VertexPart {
    #[serde(default)]
    text: Option<String>,
    #[serde(default, rename = "functionCall")]
    function_call: Option<GeminiFunctionCall>,
}

/// Decode one inbound SSE `data:` line into runtime events. Empty/comment
/// lines yield an empty vec; `data: [DONE]` yields `Finish { reason: None }`.
/// Malformed JSON is surfaced as an `Err` so the caller can decide whether to
/// terminate the stream (spec: fail closed on structurally invalid frames).
pub fn from_stream_line(line: &str) -> Result<Vec<GeminiStreamEvent>, String> {
    if line.len() > MAX_INBOUND_LINE_BYTES {
        return Err(format!(
            "gemini: stream line exceeded {MAX_INBOUND_LINE_BYTES}-byte cap"
        ));
    }
    let trimmed = line.trim_start();
    // SSE: only `data:` lines carry payload. Ignore comments (`:`), event ids,
    // and blank lines.
    let payload = if let Some(rest) = trimmed.strip_prefix("data:") {
        rest.trim()
    } else {
        return Ok(Vec::new());
    };
    if payload.is_empty() {
        return Ok(Vec::new());
    }
    if payload == "[DONE]" {
        return Ok(vec![GeminiStreamEvent::Finish { reason: None }]);
    }
    let env: CaResponseEnvelope = serde_json::from_str(payload)
        .map_err(|e| format!("gemini: malformed stream chunk: {e}"))?;

    let mut out = Vec::new();
    let Some(response) = env.response else {
        return Ok(vec![GeminiStreamEvent::Ignored]);
    };
    for candidate in response.candidates {
        if let Some(content) = candidate.content {
            for part in content.parts {
                if let Some(text) = part.text {
                    if !text.is_empty() {
                        out.push(GeminiStreamEvent::TextDelta(text));
                    }
                }
                if let Some(call) = part.function_call {
                    out.push(GeminiStreamEvent::ToolCall(call));
                }
            }
        }
        if let Some(reason) = candidate.finish_reason {
            out.push(GeminiStreamEvent::Finish {
                reason: Some(reason),
            });
        }
    }
    if out.is_empty() {
        out.push(GeminiStreamEvent::Ignored);
    }
    Ok(out)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn translate_produces_ca_envelope_with_contents_and_tools() {
        let req = translate_generate_content_request(
            "gemini-2.5-pro",
            Some("my-proj".into()),
            Some("be brief".into()),
            &[
                ChatTurn::User { text: "hi".into() },
                ChatTurn::Assistant {
                    text: "hello".into(),
                },
                ChatTurn::User {
                    text: "list files".into(),
                },
            ],
            &[ToolSpec {
                name: "list_files".into(),
                description: Some("list working directory".into()),
                parameters_json_schema: Some(json!({"type":"object","properties":{}})),
            }],
        );
        assert_eq!(req.model, "gemini-2.5-pro");
        assert_eq!(req.project.as_deref(), Some("my-proj"));
        // Vertex envelope shape.
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["request"]["contents"][0]["role"], "user");
        assert_eq!(v["request"]["contents"][0]["parts"][0]["text"], "hi");
        assert_eq!(v["request"]["contents"][1]["role"], "model");
        assert_eq!(
            v["request"]["systemInstruction"]["parts"][0]["text"],
            "be brief"
        );
        assert_eq!(
            v["request"]["tools"][0]["functionDeclarations"][0]["name"],
            "list_files"
        );
        assert!(
            v["request"]["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"].is_object()
        );
    }

    #[test]
    fn translate_serializes_tool_calls_and_results_with_correct_roles() {
        let req = translate_generate_content_request(
            "gemini-2.5-flash",
            None,
            None,
            &[
                ChatTurn::User {
                    text: "search".into(),
                },
                ChatTurn::ToolCall {
                    name: "search".into(),
                    args: json!({"q": "rust"}),
                },
                ChatTurn::ToolResult {
                    name: "search".into(),
                    result: json!({"hits": 3}),
                },
            ],
            &[],
        );
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["request"]["contents"][1]["role"], "model");
        assert_eq!(
            v["request"]["contents"][1]["parts"][0]["functionCall"]["name"],
            "search"
        );
        assert_eq!(v["request"]["contents"][2]["role"], "function");
        assert_eq!(
            v["request"]["contents"][2]["parts"][0]["functionResponse"]["name"],
            "search"
        );
        // tools omitted → key absent, not null.
        assert!(v["request"].get("tools").is_none());
    }

    #[test]
    fn sse_ignores_blank_and_comment_lines() {
        assert!(from_stream_line("").unwrap().is_empty());
        assert!(from_stream_line("   ").unwrap().is_empty());
        assert!(from_stream_line(": keep-alive").unwrap().is_empty());
        assert!(from_stream_line("event: message").unwrap().is_empty());
        assert!(from_stream_line("id: 42").unwrap().is_empty());
    }

    #[test]
    fn sse_decodes_text_delta_from_ca_envelope() {
        let line =
            r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"Hello"}]}}]}}"#;
        let events = from_stream_line(line).unwrap();
        assert_eq!(events, vec![GeminiStreamEvent::TextDelta("Hello".into())]);
    }

    #[test]
    fn sse_decodes_function_call_and_finish_reason() {
        let line = r#"data: {"response":{"candidates":[{"content":{"parts":[{"functionCall":{"name":"search","args":{"q":"rust"}}}]},"finishReason":"STOP"}]}}"#;
        let events = from_stream_line(line).unwrap();
        assert_eq!(
            events,
            vec![
                GeminiStreamEvent::ToolCall(GeminiFunctionCall {
                    name: "search".into(),
                    args: json!({"q":"rust"})
                }),
                GeminiStreamEvent::Finish {
                    reason: Some("STOP".into())
                },
            ]
        );
    }

    #[test]
    fn sse_done_sentinel_yields_finish() {
        let events = from_stream_line("data: [DONE]").unwrap();
        assert_eq!(events, vec![GeminiStreamEvent::Finish { reason: None }]);
    }

    #[test]
    fn sse_malformed_json_is_a_typed_error() {
        assert!(from_stream_line("data: {not json").is_err());
        assert!(from_stream_line("data: garbage")
            .unwrap_err()
            .contains("malformed"));
    }

    #[test]
    fn sse_oversize_line_fails_closed() {
        let big = format!("data: {}", "x".repeat(MAX_INBOUND_LINE_BYTES));
        assert!(from_stream_line(&big).unwrap_err().contains("cap"));
    }

    #[test]
    fn sse_empty_response_envelope_is_ignored() {
        let events = from_stream_line("data: {}").unwrap();
        assert_eq!(events, vec![GeminiStreamEvent::Ignored]);
    }
}
