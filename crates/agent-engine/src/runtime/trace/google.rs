//! Task 10B: trace wiring for the Google Gemini Code Assist transport
//! (broker-proxied `v1internal:streamGenerateContent`) and the explicit
//! cloud-invoke routes (AWS Bedrock, Azure OpenAI, Google Vertex — all
//! broker-mediated). Mirrors the Anthropic/OpenAI seams:
//!
//! - **Exact bytes only.** The Gemini transport serializes its envelope once
//!   via `ProxyRequest::post_json_exact`; on the **local** broker those very
//!   bytes go on the wire and are the digest preimage. On a **remote**
//!   broker the daemon re-serializes out of process, so records carry
//!   `wire: None` and `TransportKind::CloudProxy` — never a digest of bytes
//!   this process did not send. Cloud-invoke bodies are always serialized
//!   behind the broker boundary (provider-specific signing/shapes), so those
//!   records carry `wire: None` unconditionally.
//! - **One record per actual broker attempt.** The Gemini 429 retry loop
//!   emits one record per `proxy_stream` call; a cancellation observed
//!   during backoff sleep (no send in flight) emits no extra record. A
//!   tool-bearing cloud request fails §5.5 preflight before any broker or
//!   network work and emits **no** attempt record.
//! - **Honest translation reporting.** The Gemini adapter's known losses —
//!   dropped thinking/unknown blocks, dropped internal-only tools, merged
//!   text segments, synthesized function-response names — are reported as
//!   metadata-only entries (positional IDs, never content). Cloud-invoke
//!   routes flatten structured content to text and report the downgrade.
//! - **Correctness firewall.** Everything degrades to no-ops when tracing
//!   is disabled or identity is unrepresentable; provider-controlled error
//!   text never enters a record.

use super::emit::{RequestTracer, TraceContext};
use super::types::{
    EndpointMeta, StopReason, TranslationAction, TranslationElement, TranslationLoss,
    TransportKind, UsageMeta, UsageProvenance,
};
use crate::runtime::google_gemini::translate::GeminiUsage;
use agent_core::auth::CloudProviderId;
use agent_core::prompt::QualifiedModelId;
use serde_json::Value;

/// Tool names the Gemini adapter drops as internal-only (never sent on the
/// Code Assist wire). Shared with `runtime::google_gemini::runtime`'s wire
/// filter so the trace report can never drift from the actual translation.
pub const GEMINI_INTERNAL_TOOLS: [&str; 3] = ["respond", "send_channel", "watcher_exit"];

/// Normalize a Gemini `finishReason` into the trace enum. Unknown values
/// collapse to `Other` — the raw string is never stored.
pub fn stop_reason_from_gemini(raw: &str) -> StopReason {
    match raw {
        "STOP" => StopReason::EndTurn,
        "MAX_TOKENS" => StopReason::MaxTokens,
        "TOOL_CALL" | "FUNCTION_CALL" => StopReason::ToolUse,
        "SAFETY" | "PROHIBITED_CONTENT" | "BLOCKLIST" | "SPII" | "RECITATION" => {
            StopReason::ContentFilter
        }
        _ => StopReason::Other,
    }
}

/// Provider-reported usage from a decoded Gemini stream chunk. Counts the
/// upstream did not report stay `None` — never fabricated zeros.
pub fn usage_from_gemini(usage: &GeminiUsage) -> UsageMeta {
    UsageMeta {
        provenance: UsageProvenance::ProviderReported,
        input_tokens: usage.prompt_tokens,
        output_tokens: usage.candidates_tokens,
        cache_read_tokens: usage.cached_tokens,
        cache_write_tokens: None,
    }
}

/// Metadata-only report of the rewrites the Gemini adapter performs on the
/// normalized (Anthropic-shaped) request. Element IDs are tool stable IDs or
/// positional paths (`messages[i]`, `messages[i].blocks[j]`, `tools[k]`) —
/// never content.
///
/// Reported honestly, mirroring `runtime::google_gemini::runtime`'s
/// `tools_to_gemini` / `messages_to_gemini_turns` exactly:
/// - internal-only tools are dropped from the wire tool list (reported by
///   their stable name — the shared [`GEMINI_INTERNAL_TOOLS`] rule);
/// - tools with a missing or empty `name` are dropped from the wire tool
///   list (reported by position only — absent/empty content is no ID);
/// - messages with roles other than user/assistant are dropped entirely;
/// - messages whose content is an empty string, or neither string nor array,
///   produce no turn at all — dropped entirely;
/// - `thinking`/unknown blocks are dropped (not representable);
/// - adjacent text blocks the translator actually concatenates into one turn
///   are reported as one `Merged` entry per run (ID = the run's first
///   contributing block). Runs are broken only by the translator's buffer
///   flushes — `tool_use` (assistant) / `tool_result` (user); dropped blocks
///   do *not* break a run because the text buffer survives them;
/// - `tool_result` blocks lose their `tool_use_id` — the wire
///   `functionResponse` name is synthesized from the id→name map.
pub fn gemini_translation_losses(
    messages: &[crate::SharedMessage],
    tools_schema: &[Value],
) -> Vec<TranslationLoss> {
    let mut losses = Vec::new();
    for (k, tool) in tools_schema.iter().enumerate() {
        match tool.get("name").and_then(Value::as_str) {
            Some(name) if !name.is_empty() => {
                if GEMINI_INTERNAL_TOOLS.contains(&name) {
                    losses.push(TranslationLoss {
                        action: TranslationAction::Dropped,
                        element: TranslationElement::Tool,
                        element_id: super::types::TraceId::new(name).ok(),
                    });
                }
            }
            // The wire filter (`tools_to_gemini`) rejects tools with a
            // missing or empty name — report structurally, by position.
            _ => losses.push(TranslationLoss {
                action: TranslationAction::Dropped,
                element: TranslationElement::Tool,
                element_id: super::types::TraceId::new(format!("tools[{k}]")).ok(),
            }),
        }
    }
    for (i, message) in messages.iter().enumerate() {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        if role != "user" && role != "assistant" {
            losses.push(TranslationLoss {
                action: TranslationAction::Dropped,
                element: TranslationElement::Other,
                element_id: super::types::TraceId::new(format!("messages[{i}]")).ok(),
            });
            continue;
        }
        let blocks = match message.get("content") {
            Some(Value::String(s)) if !s.is_empty() => continue, // kept verbatim
            Some(Value::Array(blocks)) => blocks,
            // Empty string, or neither string nor array (including absent):
            // the translator emits no turn — the message is dropped whole.
            _ => {
                losses.push(TranslationLoss {
                    action: TranslationAction::Dropped,
                    element: TranslationElement::Other,
                    element_id: super::types::TraceId::new(format!("messages[{i}]")).ok(),
                });
                continue;
            }
        };
        // Mirror the translator's text buffer: a "run" is the set of text
        // blocks concatenated into one wire turn. Only the flushing block
        // type for this role breaks a run; dropped blocks do not.
        let flush_type = if role == "assistant" {
            "tool_use"
        } else {
            "tool_result"
        };
        let mut run_len = 0usize; // text blocks that actually contributed
        let mut run_start = 0usize;
        let flush_run = |run_len: &mut usize, run_start: usize, losses: &mut Vec<_>| {
            if *run_len > 1 {
                losses.push(TranslationLoss {
                    action: TranslationAction::Merged,
                    element: TranslationElement::MessageBlock,
                    element_id: super::types::TraceId::new(format!(
                        "messages[{i}].blocks[{run_start}]"
                    ))
                    .ok(),
                });
            }
            *run_len = 0;
        };
        for (j, block) in blocks.iter().enumerate() {
            let btype = block.get("type").and_then(Value::as_str).unwrap_or("");
            if btype == "text" {
                // Only blocks that append text feed the buffer; blocks with
                // an absent/non-string text field contribute nothing and so
                // cannot participate in a merge.
                if block
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|t| !t.is_empty())
                {
                    if run_len == 0 {
                        run_start = j;
                    }
                    run_len += 1;
                }
                continue;
            }
            if btype == flush_type {
                flush_run(&mut run_len, run_start, &mut losses);
                if btype == "tool_result" {
                    // The wire functionResponse name is synthesized from the
                    // tool_use id→name map; the vendor id itself is not
                    // representable on the Gemini wire.
                    losses.push(TranslationLoss {
                        action: TranslationAction::Synthesized,
                        element: TranslationElement::MessageBlock,
                        element_id: super::types::TraceId::new(format!(
                            "messages[{i}].blocks[{j}]"
                        ))
                        .ok(),
                    });
                }
                continue;
            }
            // Not representable for this role — dropped, but the text buffer
            // survives, so the current run continues.
            losses.push(TranslationLoss {
                action: TranslationAction::Dropped,
                element: TranslationElement::MessageBlock,
                element_id: super::types::TraceId::new(format!("messages[{i}].blocks[{j}]")).ok(),
            });
        }
        flush_run(&mut run_len, run_start, &mut losses);
    }
    losses
}

/// Begin tracing one Gemini Code Assist request. Delegates to the shared
/// OpenAI-compatible builder (same normalized message walker, no cache
/// section, wire claimed only from `exact_sent_bytes`). Returns `None` when
/// tracing is disabled or identity is unrepresentable.
#[allow(clippy::too_many_arguments)]
pub async fn begin_gemini_tracer(
    trace: &TraceContext,
    provider: &str,
    model: &str,
    transport: TransportKind,
    url: &str,
    exact_sent_bytes: Option<&[u8]>,
    messages: &[crate::SharedMessage],
    system_prompt: Option<&str>,
    tools_schema: &[Value],
) -> Option<RequestTracer> {
    super::openai::begin_openai_tracer(
        trace,
        provider,
        model,
        transport,
        url,
        exact_sent_bytes,
        messages,
        system_prompt,
        tools_schema,
        gemini_translation_losses(messages, tools_schema),
    )
    .await
}

/// Static, safe endpoint identity for a broker-mediated cloud invocation.
/// The real host is broker-owned and region/deployment-specific; recording
/// a provider-qualified name under the RFC-reserved `.invalid` TLD keeps the
/// record honest — it can never be mistaken for an observed endpoint.
fn cloud_endpoint(provider: CloudProviderId) -> Option<EndpointMeta> {
    EndpointMeta::new(
        format!("{}.cloud-broker.invalid", provider.as_str()),
        "/invoke",
    )
    .ok()
}

/// Cloud-invoke translation report: the broker route is text-only, so any
/// message whose content is not already a plain string is flattened to text
/// (a structural downgrade), reported per message index.
pub fn cloud_translation_losses(messages: &[crate::SharedMessage]) -> Vec<TranslationLoss> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, m)| !m.get("content").map(Value::is_string).unwrap_or(false))
        .map(|(i, _)| TranslationLoss {
            action: TranslationAction::Downgraded,
            element: TranslationElement::MessageBlock,
            element_id: super::types::TraceId::new(format!("messages[{i}]")).ok(),
        })
        .collect()
}

/// Begin tracing one explicit cloud invocation (AWS Bedrock, Azure OpenAI,
/// Google Vertex). Always `TransportKind::CloudProxy` with `wire: None`:
/// the provider body is serialized and signed behind the broker boundary,
/// so this process never holds the exact bytes sent. Returns `None` when
/// tracing is disabled or identity is unrepresentable.
pub async fn begin_cloud_invoke_tracer(
    trace: &TraceContext,
    provider: CloudProviderId,
    cloud_model: &str,
    messages: &[crate::SharedMessage],
    system_prompt: Option<&str>,
) -> Option<RequestTracer> {
    if !trace.enabled() {
        return None;
    }
    let endpoint = cloud_endpoint(provider)?;
    let model = QualifiedModelId::parse(cloud_model).ok()?;
    let key = trace.digest_key().await;
    let mut structure = super::anthropic::anthropic_request_structure(
        key.as_deref(),
        &[],
        messages,
        system_prompt,
        &[],
        None,
        false,
        false,
        cloud_translation_losses(messages),
        None,
    );
    // No exact bytes exist in this process — never claim a wire digest.
    structure.wire = None;
    // Prompt-cache markers are not sent on the cloud-invoke route.
    structure.cache = Default::default();
    RequestTracer::begin(
        trace,
        None,
        model,
        TransportKind::CloudProxy,
        endpoint,
        structure,
    )
}

/// Provider-reported usage from a broker `CloudEvent::Usage`. The broker
/// event carries exactly input/output totals — nothing else is claimed.
pub fn usage_from_cloud_event(input_tokens: u64, output_tokens: u64) -> UsageMeta {
    UsageMeta {
        provenance: UsageProvenance::ProviderReported,
        input_tokens: Some(input_tokens),
        output_tokens: Some(output_tokens),
        cache_read_tokens: None,
        cache_write_tokens: None,
    }
}
