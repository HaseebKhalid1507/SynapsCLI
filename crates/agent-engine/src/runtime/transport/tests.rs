//! Task 9 tests: IR fixtures, redacted Debug, TranslationReport rules, and
//! Anthropic adapter golden byte identity.

use super::anthropic::{anthropic_translation_report, build_anthropic_request};
use super::ir::{NormalizedBlock, NormalizedRequest, NormalizedRole, SystemSegmentKind};
use super::report::{block_path, synthetic_system_id, TranslationReport};
use crate::runtime::body_golden;
use crate::runtime::helpers::HelperMethods;
use crate::runtime::trace::{TranslationAction, TranslationElement};
use serde_json::{json, Value};
use std::sync::Arc;

/// Sentinel that must never appear in Debug output or report serialization.
const SENTINEL: &str = "TOP-SECRET-CONTENT-SENTINEL-9f3a";

// ── Fixture corpus (cross-provider, normalized semantics) ────────────────

#[derive(serde::Deserialize)]
struct Fixture {
    #[allow(dead_code)]
    description: String,
    request: NormalizedRequest<'static>,
    expected_anthropic: TranslationReport,
}

fn fixture_dir() -> std::path::PathBuf {
    // Workspace-root corpus, reusable by future provider adapters.
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests")
        .join("fixtures")
        .join("request_ir")
}

fn load_fixtures() -> Vec<(String, Fixture, Value)> {
    let dir = fixture_dir();
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("missing fixture dir {} — {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    names.sort();
    assert!(
        names.len() >= 6,
        "expected the full request_ir corpus (text/reasoning/tool/media/unknown x2), found {}",
        names.len()
    );
    names
        .into_iter()
        .map(|p| {
            let raw = std::fs::read_to_string(&p).expect("read fixture");
            let value: Value = serde_json::from_str(&raw).expect("fixture is JSON");
            let fixture: Fixture = serde_json::from_str(&raw)
                .unwrap_or_else(|e| panic!("fixture {} does not parse: {e}", p.display()));
            (
                p.file_stem().unwrap().to_string_lossy().into_owned(),
                fixture,
                value,
            )
        })
        .collect()
}

/// Every fixture parses into the IR and serializes back to the exact same
/// normalized JSON (roundtrip; field defaults are canonical in fixtures).
#[test]
fn fixtures_parse_and_roundtrip() {
    for (name, fixture, raw) in load_fixtures() {
        let reserialized = serde_json::to_value(&fixture.request).expect("serialize IR");
        assert_eq!(
            reserialized, raw["request"],
            "fixture `{name}`: IR does not roundtrip to its normalized JSON"
        );
        // And the report analysis matches the recorded expectation.
        let report = anthropic_translation_report(&fixture.request, "api_key");
        assert_eq!(
            report, fixture.expected_anthropic,
            "fixture `{name}`: Anthropic translation report diverges from expectation"
        );
    }
}

/// Supported fixtures are lossless; the deliberately unsupported fixture
/// yields the exact explicit entry — never silent disappearance.
#[test]
fn unsupported_fixture_yields_exact_report_entry() {
    let fixtures = load_fixtures();
    let (_, unsupported, _) = fixtures
        .iter()
        .find(|(n, _, _)| n == "unknown_foreign_unsupported")
        .expect("deliberately-unsupported fixture present");
    let report = anthropic_translation_report(&unsupported.request, "api_key");
    assert_eq!(report.entries.len(), 1, "exactly one explicit entry");
    let entry = &report.entries[0];
    assert_eq!(entry.action, TranslationAction::Unsupported);
    assert_eq!(entry.element, TranslationElement::MessageBlock);
    assert_eq!(entry.element_id, Some(block_path(1, 0)));

    for (name, fixture, _) in &fixtures {
        if name == "unknown_foreign_unsupported" || name == "media_other_unsupported" {
            continue;
        }
        assert!(
            anthropic_translation_report(&fixture.request, "api_key").is_lossless(),
            "supported fixture `{name}` must be lossless for Anthropic"
        );
    }
}

/// `MediaKind::Other` (audio/video/provider-specific attachments) has no
/// Anthropic wire representation: exactly one explicit `Unsupported`
/// `MessageBlock` entry at the block's structural position.
#[test]
fn media_other_fixture_yields_exact_unsupported_entry() {
    let fixtures = load_fixtures();
    let (_, other, _) = fixtures
        .iter()
        .find(|(n, _, _)| n == "media_other_unsupported")
        .expect("media_other_unsupported fixture present");
    let report = anthropic_translation_report(&other.request, "api_key");
    assert_eq!(report.entries.len(), 1, "exactly one explicit entry");
    let entry = &report.entries[0];
    assert_eq!(entry.action, TranslationAction::Unsupported);
    assert_eq!(entry.element, TranslationElement::MessageBlock);
    assert_eq!(entry.element_id, Some(block_path(0, 1)));
}

/// Structural semantics preserved through parse: system order, block order,
/// and tool-result error state.
#[test]
fn fixture_structure_preserves_order_and_error_state() {
    let fixtures = load_fixtures();
    let (_, text, _) = fixtures.iter().find(|(n, _, _)| n == "text_basic").unwrap();
    assert_eq!(text.request.system.len(), 1);
    assert_eq!(text.request.system[0].kind, SystemSegmentKind::Primary);
    assert_eq!(text.request.messages.len(), 3);
    assert_eq!(text.request.messages[0].role, NormalizedRole::User);
    assert_eq!(text.request.messages[1].role, NormalizedRole::Assistant);

    let (_, tools, _) = fixtures
        .iter()
        .find(|(n, _, _)| n == "tool_call_result_error")
        .unwrap();
    let results = &tools.request.messages[2].blocks;
    match (&results[0], &results[1]) {
        (
            NormalizedBlock::ToolResult {
                call_id: a,
                is_error: false,
                ..
            },
            NormalizedBlock::ToolResult {
                call_id: b,
                is_error: true,
                ..
            },
        ) => {
            assert_eq!(a, "toolu_01");
            assert_eq!(b, "toolu_02");
        }
        other => panic!("tool_result blocks lost order or error state: {other:?}"),
    }
}

// ── Redacted Debug ───────────────────────────────────────────────────────

fn sentinel_ir() -> NormalizedRequest<'static> {
    serde_json::from_value(json!({
        "system": [{"kind": "primary", "text": SENTINEL}],
        "messages": [
            {"role": "user", "blocks": [
                {"type": "text", "text": SENTINEL},
                {"type": "reasoning", "text": SENTINEL, "signature": SENTINEL},
                {"type": "tool_call", "id": "toolu_01", "name": "t", "input": {"arg": SENTINEL}},
                {"type": "tool_result", "call_id": "toolu_01", "is_error": true, "content": SENTINEL},
                {"type": "media", "media_kind": "image", "source": {"data": SENTINEL}},
                {"type": "unknown", "provider": "openai", "payload": {"data": SENTINEL}}
            ]}
        ]
    }))
    .expect("sentinel IR parses")
}

/// Debug of the whole content-bearing IR must never print content.
#[test]
fn debug_output_redacts_all_content() {
    let ir = sentinel_ir();
    let debug = format!("{ir:#?}");
    assert!(
        !debug.contains(SENTINEL),
        "IR Debug leaked content:\n{debug}"
    );
    assert!(
        debug.contains("<content redacted>"),
        "redaction marker present"
    );
}

/// No report field can carry content: serialize a report built from the
/// sentinel IR and assert the sentinel is absent.
#[test]
fn report_serialization_never_contains_content() {
    let ir = sentinel_ir();
    let report = anthropic_translation_report(&ir, "oauth");
    assert!(
        !report.is_lossless(),
        "sentinel IR contains a foreign opaque block + oauth synthesis — report must not be empty"
    );
    let serialized = serde_json::to_string(&report).unwrap();
    assert!(
        !serialized.contains(SENTINEL),
        "report leaked content: {serialized}"
    );
    let debug = format!("{report:?}");
    assert!(!debug.contains(SENTINEL), "report Debug leaked content");
}

// ── Adapter report rules ─────────────────────────────────────────────────

/// OAuth transport synthesizes the three fixed identity system blocks —
/// reported as Synthesized entries with symbolic IDs, deterministically.
#[test]
fn oauth_synthesized_identity_blocks_are_reported() {
    let ir = NormalizedRequest::default();
    let report = anthropic_translation_report(&ir, "oauth");
    assert_eq!(report.entries.len(), 3);
    for (i, entry) in report.entries.iter().enumerate() {
        assert_eq!(entry.action, TranslationAction::Synthesized);
        assert_eq!(entry.element, TranslationElement::SystemSegment);
        assert_eq!(entry.element_id, Some(synthetic_system_id(i)));
    }
    // API-key transport synthesizes nothing.
    assert!(anthropic_translation_report(&ir, "api_key").is_lossless());
}

/// System/Tool roles have no Anthropic wire representation → Downgraded.
#[test]
fn foreign_roles_are_reported_downgraded() {
    let ir: NormalizedRequest<'static> = serde_json::from_value(json!({
        "messages": [
            {"role": "system", "blocks": [{"type": "text", "text": "x"}]},
            {"role": "user", "blocks": [{"type": "text", "text": "y"}]},
            {"role": "tool", "blocks": [{"type": "text", "text": "z"}]}
        ]
    }))
    .unwrap();
    let report = anthropic_translation_report(&ir, "api_key");
    assert_eq!(report.entries.len(), 2);
    assert!(report.entries.iter().all(|e| {
        e.action == TranslationAction::Downgraded && e.element == TranslationElement::Other
    }));
    assert_eq!(
        report.entries[0].element_id.as_ref().unwrap().as_str(),
        "messages[0]"
    );
    assert_eq!(
        report.entries[1].element_id.as_ref().unwrap().as_str(),
        "messages[2]"
    );
}

// ── History normalization (production borrow path) ───────────────────────

#[test]
fn anthropic_history_normalizes_borrowed_without_loss() {
    let messages: Vec<crate::SharedMessage> = vec![
        Arc::new(json!({"role": "user", "content": "hi"})),
        Arc::new(json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "hm", "signature": "sig"},
            {"type": "redacted_thinking", "data": "opaque"},
            {"type": "text", "text": "ok"},
            {"type": "tool_use", "id": "toolu_9", "name": "get_weather", "input": {"location": "Tokyo"}}
        ]})),
        Arc::new(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_9", "is_error": true, "content": "boom"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aa"}},
            {"type": "mystery_block", "stuff": 1}
        ]})),
    ];
    let ir = NormalizedRequest::from_anthropic_history(Some("sys"), &messages);
    assert_eq!(ir.system.len(), 1);
    assert_eq!(ir.messages.len(), 3);
    assert!(matches!(
        ir.messages[0].blocks[0],
        NormalizedBlock::Text { .. }
    ));
    assert!(matches!(
        ir.messages[1].blocks[1],
        NormalizedBlock::Reasoning { redacted: true, .. }
    ));
    match &ir.messages[2].blocks[0] {
        NormalizedBlock::ToolResult {
            call_id, is_error, ..
        } => {
            assert_eq!(call_id, "toolu_9");
            assert!(*is_error, "tool-result error state preserved");
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }
    // Unknown Anthropic block: retained in IR, tagged anthropic, no loss.
    match &ir.messages[2].blocks[2] {
        NormalizedBlock::Unknown { provider, .. } => assert_eq!(provider, "anthropic"),
        other => panic!("expected Unknown, got {other:?}"),
    }
    assert!(
        anthropic_translation_report(&ir, "api_key").is_lossless(),
        "supported Anthropic-shaped history must be lossless"
    );
}

// ── Golden byte identity through the adapter (every existing mode) ───────

fn adapter_bytes(s: &body_golden::Scenario) -> (Vec<u8>, TranslationReport) {
    let mut cleaned = s.messages.to_vec();
    HelperMethods::sanitize_thinking_blocks(&mut cleaned);
    HelperMethods::annotate_cache_breakpoint(&mut cleaned, s.ttl);
    let parts = build_anthropic_request(
        s.model,
        &cleaned,
        &s.tools,
        &s.system_prompt,
        s.auth_type,
        s.thinking_budget,
        s.reasoning_level,
        None,
        s.ttl,
        s.stream,
    );
    let bytes = serde_json::to_vec(&parts.body).expect("serialize adapter body");
    (bytes, parts.report)
}

/// THE Task 9 gate: for every golden mode, the adapter's wire bytes are
/// identical to the frozen legacy assembly (live) and the committed fixture,
/// and the report is lossless for api_key scenarios / exactly the oauth
/// identity synthesis otherwise. Runs twice per scenario → deterministic.
#[test]
fn adapter_golden_byte_identity_all_modes() {
    let dir = body_golden::fixture_dir();
    let default_identity = body_golden::identity_is_default();
    for s in body_golden::scenarios() {
        let old = body_golden::old_body_bytes(&s);
        let (bytes_a, report_a) = adapter_bytes(&s);
        let (bytes_b, report_b) = adapter_bytes(&s);
        assert_eq!(
            bytes_a, bytes_b,
            "scenario `{}`: nondeterministic bytes",
            s.name
        );
        assert_eq!(
            report_a, report_b,
            "scenario `{}`: nondeterministic report",
            s.name
        );
        assert_eq!(
            bytes_a,
            old,
            "scenario `{}`: adapter bytes diverge from legacy assembly\nold: {}\nnew: {}",
            s.name,
            String::from_utf8_lossy(&old),
            String::from_utf8_lossy(&bytes_a),
        );
        if !(s.identity_sensitive && !default_identity) {
            let path = dir.join(format!("{}.json", s.name));
            let fixture = std::fs::read(&path)
                .unwrap_or_else(|e| panic!("missing fixture {} — {e}", path.display()));
            assert_eq!(
                bytes_a, fixture,
                "scenario `{}`: adapter bytes diverge from committed golden fixture",
                s.name
            );
        }
        // Report expectations: supported history is lossless; oauth adds
        // exactly the three synthesized identity segments.
        if s.auth_type == "oauth" {
            assert_eq!(report_a.entries.len(), 3, "scenario `{}`", s.name);
            assert!(report_a
                .entries
                .iter()
                .all(|e| e.action == TranslationAction::Synthesized));
        } else {
            assert!(
                report_a.is_lossless(),
                "scenario `{}`: supported input must be lossless, got {:?}",
                s.name,
                report_a
            );
        }
    }
}

// ── Trace integration: report populates translation_losses ───────────────

#[test]
fn report_flows_into_trace_translation_losses() {
    use crate::runtime::trace as tr;
    let sink = tr::CollectingTraceSink::new();
    let ctx = tr::TraceContext::with_sink(sink.clone());
    let mut report = TranslationReport::lossless();
    report.push(
        TranslationAction::Unsupported,
        TranslationElement::MessageBlock,
        Some(block_path(1, 0)),
    );
    let structure = tr::RequestStructure {
        translation: report.clone().into_losses(),
        ..Default::default()
    };
    let tracer = tr::RequestTracer::begin(
        &ctx,
        agent_core::prompt::QualifiedModelId::parse("anthropic/claude-sonnet-4-6").unwrap(),
        tr::TransportKind::AnthropicMessages,
        tr::EndpointMeta::new("api.anthropic.com", "/v1/messages").unwrap(),
        structure,
    )
    .expect("tracer begins");
    tracer.finish(
        tr::AttemptClock::start(),
        Some(200),
        None,
        None,
        None,
        agent_core::TurnOutcome::Completed,
    );
    let records = sink.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].translation_losses, report.into_losses());
}
