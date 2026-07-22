//! Anthropic reference provider adapter (Task 9, spec §6.3).
//!
//! Contract: [`build_anthropic_request`] returns **both** the wire request
//! and a [`TranslationReport`]. Behavior preservation is absolute for
//! supported input: the wire body is produced by the byte-identity-gated
//! compatibility serializer [`RequestBody`] (see `runtime::request` and the
//! `runtime::body_golden` fixtures) borrowing the original `SharedMessage`
//! history — this adapter adds analysis, never a `Value` round-trip or a
//! second full-history copy. Serializing the wire body *from* the IR (full
//! canonicalization) awaits the non-Anthropic provider adapters (Task 10+).

use serde_json::Value;

use super::super::request::RequestBody;
use super::ir::{NormalizedBlock, NormalizedRequest, NormalizedRole};
use super::report::TranslationReport;
use crate::core::config::CacheTtl;
use crate::SharedMessage;

/// Wire request + translation report for one Anthropic `/v1/messages` call.
pub(in crate::runtime) struct AnthropicRequestParts<'a> {
    /// The exact body to serialize and send — byte-identical to the legacy
    /// assembly (golden-gated).
    pub body: RequestBody<'a>,
    /// Every semantic loss/rewrite this adapter performed. Lossless for all
    /// supported (Anthropic-shaped) input.
    pub report: TranslationReport,
}

/// Build the Anthropic wire request plus its translation report.
/// Parameters mirror [`RequestBody::new`] exactly; `messages` must already
/// be sanitized + cache-annotated.
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime) fn build_anthropic_request<'a>(
    model: &'a str,
    messages: &'a [SharedMessage],
    tools_schema: &'a [Value],
    system_prompt: &'a Option<String>,
    auth_type: &str,
    thinking_budget: u32,
    reasoning_level: agent_core::reasoning::ReasoningLevel,
    execution_plan: Option<&crate::runtime::openai::catalog::AnthropicExecutionPlan>,
    ttl: CacheTtl,
    stream: bool,
) -> AnthropicRequestParts<'a> {
    let ir = NormalizedRequest::from_anthropic_history(system_prompt.as_deref(), messages);
    let report = anthropic_translation_report(&ir, auth_type);
    let body = RequestBody::new(
        model,
        messages,
        tools_schema,
        system_prompt,
        auth_type,
        thinking_budget,
        reasoning_level,
        execution_plan,
        ttl,
        stream,
    );
    AnthropicRequestParts { body, report }
}

/// Analyze a normalized request for elements the Anthropic wire cannot
/// represent faithfully, plus adapter-side synthesis. Deterministic:
/// entries appear in structural order (system first, then messages).
///
/// Rules:
/// - OAuth transport synthesizes the fixed identity system blocks (see
///   `HelperMethods::build_system_blocks`): reported as `Synthesized`
///   `SystemSegment` entries with symbolic `system.synthetic[N]` IDs.
/// - `Unknown` blocks tagged `provider: "anthropic"` pass through verbatim
///   on the wire — no entry.
/// - `Unknown` blocks from any other provider are `Unsupported`.
/// - `System`/`Tool` roles have no Anthropic wire representation and would
///   be role-rewritten by a canonicalizing serializer: `Downgraded`.
pub(crate) fn anthropic_translation_report(
    ir: &NormalizedRequest<'_>,
    auth_type: &str,
) -> TranslationReport {
    use crate::runtime::trace::{TranslationAction, TranslationElement};

    let mut report = TranslationReport::lossless();

    // OAuth synthesizes the three fixed identity blocks ahead of any IR
    // system segment (transport identity, product identity, guidance) —
    // see `HelperMethods::build_system_blocks`. Symbolic IDs: these
    // elements have no pre-translation position.
    if auth_type == "oauth" {
        for i in 0..3 {
            report.push(
                TranslationAction::Synthesized,
                TranslationElement::SystemSegment,
                Some(super::report::synthetic_system_id(i)),
            );
        }
    }

    for (mi, message) in ir.messages.iter().enumerate() {
        // Anthropic wire has only user/assistant roles; a canonicalizing
        // serializer would rewrite System/Tool roles — report Downgraded.
        // NOTE (Task 10 canonicalization risk): today the wire body is the
        // byte-identity legacy serializer, so no rewrite actually occurs;
        // when Task 10 serializes from the IR, the rewrite becomes real and
        // this entry must stay in lockstep with it.
        if matches!(message.role, NormalizedRole::System | NormalizedRole::Tool) {
            report.push(
                TranslationAction::Downgraded,
                TranslationElement::Other,
                Some(super::report::message_path(mi)),
            );
        }
        for (bi, block) in message.blocks.iter().enumerate() {
            match block {
                // Foreign opaque provider block: Anthropic cannot
                // represent it — explicit, never silent.
                NormalizedBlock::Unknown { provider, .. } if provider != "anthropic" => {
                    report.push(
                        TranslationAction::Unsupported,
                        TranslationElement::MessageBlock,
                        Some(super::report::block_path(mi, bi)),
                    );
                }
                // Anthropic-tagged opaque blocks pass through verbatim.
                NormalizedBlock::Unknown { .. } => {}
                // Media kinds beyond image/document have no Anthropic
                // wire representation.
                NormalizedBlock::Media {
                    media_kind: super::ir::MediaKind::Other,
                    ..
                } => {
                    report.push(
                        TranslationAction::Unsupported,
                        TranslationElement::MessageBlock,
                        Some(super::report::block_path(mi, bi)),
                    );
                }
                NormalizedBlock::Text { .. }
                | NormalizedBlock::Reasoning { .. }
                | NormalizedBlock::ToolCall { .. }
                | NormalizedBlock::ToolResult { .. }
                | NormalizedBlock::Media { .. } => {}
            }
        }
    }
    report
}
