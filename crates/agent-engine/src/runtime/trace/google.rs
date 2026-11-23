//! Task 10B: trace wiring for the explicit cloud-invoke routes (AWS Bedrock,
//! Azure OpenAI, Google Vertex — all broker-mediated). Mirrors the
//! Anthropic/OpenAI seams:
//!
//! - **Exact bytes only.** Cloud-invoke bodies are always serialized behind
//!   the broker boundary (provider-specific signing/shapes), so those
//!   records carry `wire: None` unconditionally — never a digest of bytes
//!   this process did not send.
//! - **One record per actual broker attempt.** A tool-bearing cloud request
//!   fails §5.5 preflight before any broker or network work and emits
//!   **no** attempt record.
//! - **Honest translation reporting.** Cloud-invoke routes flatten
//!   structured content to text and report the downgrade as metadata-only
//!   entries (positional IDs, never content).
//! - **Correctness firewall.** Everything degrades to no-ops when tracing
//!   is disabled or identity is unrepresentable; provider-controlled error
//!   text never enters a record.

use super::emit::{RequestTracer, TraceContext};
use super::types::{
    EndpointMeta, TranslationAction, TranslationElement, TranslationLoss, TransportKind,
    UsageMeta, UsageProvenance,
};
use agent_core::auth::CloudProviderId;
use agent_core::prompt::QualifiedModelId;
use serde_json::Value;

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
