//! Fixture-driven GitHub Copilot routing tests (experimental broker).
//!
//! Every assertion is grounded in `supported_endpoints`, `policy.state`,
//! `model_picker_enabled` and `vendor` observed on the captured personal
//! catalog. Nothing here relies on model-name string heuristics.
//!
//! Not an official GitHub product API. Community-observed shape only.

use std::collections::{BTreeMap, HashSet};

use synaps_cli::runtime::openai::catalog::{
    parse_copilot_catalog_entries, selectable_copilot_entries, CopilotEndpoint,
    CopilotPolicyState, COPILOT_FALLBACK_MODELS,
};
use synaps_cli::runtime::openai::{resolve_route, AuthPolicy, WireProtocol};

const FIXTURE: &str = include_str!(
    "../crates/agent-engine/src/runtime/openai/catalog/fixtures/github_copilot_models.json"
);

/// Ground-truth expectations, hand-derived by reading the fixture.
///
/// The tuples are `(id, is_selectable, expected_wire_after_endpoint_review)`.
///
/// Selectability = `type == chat` AND `model_picker_enabled == true`
/// AND `policy.state != disabled` AND at least one reviewed endpoint
/// (`/responses` or `/chat/completions`; `/v1/messages` is not currently
/// broker-reviewed, `ws:/responses` is out of scope).
fn expected() -> BTreeMap<&'static str, (bool, Option<WireProtocol>)> {
    let mut m = BTreeMap::new();
    // OpenAI vendor family — prefer /responses when advertised.
    m.insert("gpt-5.3-codex", (true, Some(WireProtocol::OpenAiResponses)));
    m.insert("gpt-5.4", (true, Some(WireProtocol::OpenAiResponses)));
    m.insert("gpt-5.4-mini", (true, Some(WireProtocol::OpenAiResponses)));
    m.insert("gpt-5.6-luna", (true, Some(WireProtocol::OpenAiResponses)));
    m.insert("gpt-5.6-terra", (true, Some(WireProtocol::OpenAiResponses)));
    // Azure OpenAI vendor treated like OpenAI: /responses preferred when supported.
    m.insert("gpt-5-mini", (true, Some(WireProtocol::OpenAiResponses)));
    // Disabled OpenAI policy state — filtered.
    m.insert("gpt-5.5", (false, Some(WireProtocol::OpenAiResponses)));
    // Anthropic vendor: /v1/messages is unreviewed for broker → fall back
    // to /chat/completions where advertised.
    m.insert(
        "claude-sonnet-4.6",
        (true, Some(WireProtocol::OpenAiChatCompletions)),
    );
    m.insert(
        "claude-sonnet-5",
        (true, Some(WireProtocol::OpenAiChatCompletions)),
    );
    m.insert(
        "claude-haiku-4.5",
        (true, Some(WireProtocol::OpenAiChatCompletions)),
    );
    // Disabled Anthropic policy state — filtered from picker even though
    // /chat/completions is advertised.
    m.insert(
        "claude-fable-5",
        (false, Some(WireProtocol::OpenAiChatCompletions)),
    );
    m.insert(
        "claude-opus-4.7",
        (false, Some(WireProtocol::OpenAiChatCompletions)),
    );
    m.insert(
        "claude-opus-4.8",
        (false, Some(WireProtocol::OpenAiChatCompletions)),
    );
    m.insert(
        "claude-opus-4.8-fast",
        (false, Some(WireProtocol::OpenAiChatCompletions)),
    );
    // Google Gemini via Copilot: /chat/completions only.
    m.insert(
        "gemini-3.1-pro-preview",
        (true, Some(WireProtocol::OpenAiChatCompletions)),
    );
    m.insert(
        "gemini-3.5-flash",
        (true, Some(WireProtocol::OpenAiChatCompletions)),
    );
    // No advertised endpoints — fail-closed, not selectable, no routing.
    m.insert("gemini-3-flash-preview", (false, None));
    // Non-chat / utility rows — never routable, never in picker.
    m.insert(
        "trajectory-compaction",
        (false, Some(WireProtocol::OpenAiChatCompletions)),
    );
    m.insert("text-embedding-3-small", (false, None));
    m.insert("gpt-41-copilot", (false, None));
    m
}

#[test]
fn every_fixture_row_matches_expected_selectability_and_endpoint_pick() {
    let entries = parse_copilot_catalog_entries(FIXTURE).expect("fixture parses");
    let by_id: BTreeMap<_, _> = entries.iter().map(|e| (e.id.as_str(), e)).collect();
    for (id, (want_selectable, want_wire)) in expected() {
        let entry = by_id
            .get(id)
            .unwrap_or_else(|| panic!("fixture missing id `{id}` — refresh expectations"));
        assert_eq!(
            entry.is_selectable_for_picker(),
            want_selectable,
            "selectability drift for `{id}`"
        );
        assert_eq!(
            entry.preferred_wire_protocol(),
            want_wire,
            "endpoint pick drift for `{id}`"
        );
    }
}

#[test]
fn selectable_helper_matches_expected_set_and_excludes_disabled_and_endpointless() {
    let selectable = selectable_copilot_entries(FIXTURE).expect("fixture parses");
    let got: HashSet<_> = selectable.iter().map(|e| e.id.as_str().to_string()).collect();
    let want: HashSet<_> = expected()
        .into_iter()
        .filter_map(|(id, (sel, _))| sel.then(|| id.to_string()))
        .collect();
    assert_eq!(got, want);
    // Explicit sanity: nothing disabled or endpointless slipped through.
    for banned in [
        "claude-fable-5",
        "claude-opus-4.7",
        "claude-opus-4.8",
        "claude-opus-4.8-fast",
        "gpt-5.5",
        "gemini-3-flash-preview",
        "trajectory-compaction",
        "text-embedding-3-small",
        "gpt-41-copilot",
    ] {
        assert!(!got.contains(banned), "`{banned}` must not be selectable");
    }
}

#[test]
fn resolve_route_matches_endpoint_evidence_for_every_selectable_id() {
    for (id, (selectable, want_wire)) in expected() {
        let route = resolve_route(&format!("github-copilot/{id}"));
        if selectable {
            let route = route.unwrap_or_else(|| panic!("`{id}` must resolve"));
            assert_eq!(route.provider, "github-copilot");
            assert_eq!(route.auth, AuthPolicy::BrokerProxy);
            assert_eq!(
                Some(route.wire),
                want_wire,
                "wire protocol drift for `{id}`"
            );
        }
    }
}

#[test]
fn resolve_route_fails_closed_for_disabled_endpointless_and_unknown() {
    // Disabled policy state → not routable even if the wire is advertised.
    for id in [
        "claude-fable-5",
        "claude-opus-4.7",
        "claude-opus-4.8",
        "claude-opus-4.8-fast",
        "gpt-5.5",
    ] {
        assert!(
            resolve_route(&format!("github-copilot/{id}")).is_none(),
            "`{id}` is disabled in fixture — must not route"
        );
    }
    // No advertised supported_endpoints → fail closed.
    assert!(resolve_route("github-copilot/gemini-3-flash-preview").is_none());
    // Non-chat catalog rows must not be routable.
    for id in [
        "trajectory-compaction",
        "text-embedding-3-small",
        "gpt-41-copilot",
    ] {
        assert!(
            resolve_route(&format!("github-copilot/{id}")).is_none(),
            "`{id}` is non-chat / utility — must not route"
        );
    }
    // Guessed/unobserved names still fail closed.
    for id in ["auto", "gpt-5.6-sol", "gpt-4.1", "gemini-3-pro"] {
        assert!(resolve_route(&format!("github-copilot/{id}")).is_none());
    }
}

#[test]
fn expanded_curated_fallback_covers_every_selectable_fixture_id() {
    let selectable = selectable_copilot_entries(FIXTURE).expect("fixture");
    let selectable_ids: HashSet<_> = selectable.iter().map(|e| e.id.as_str()).collect();
    let fallback_ids: HashSet<_> = COPILOT_FALLBACK_MODELS.iter().map(|d| d.id).collect();
    assert_eq!(
        fallback_ids, selectable_ids,
        "curated fallback must exactly mirror the fixture's selectable set"
    );
}

#[test]
fn copilot_endpoint_parsing_is_exhaustive_over_fixture_strings() {
    // Sanity: the parser recognises every string the fixture actually uses.
    let entries = parse_copilot_catalog_entries(FIXTURE).expect("fixture");
    let mut saw_reviewed = false;
    let mut saw_v1_messages = false;
    let mut saw_ws = false;
    for e in &entries {
        for ep in &e.supported_endpoints {
            match ep {
                CopilotEndpoint::Responses | CopilotEndpoint::ChatCompletions => {
                    saw_reviewed = true;
                    assert!(ep.is_reviewed());
                }
                CopilotEndpoint::V1Messages => {
                    saw_v1_messages = true;
                    assert!(!ep.is_reviewed(), "/v1/messages is not broker-reviewed yet");
                }
                CopilotEndpoint::WsResponses => {
                    saw_ws = true;
                    assert!(!ep.is_reviewed(), "ws:/responses is out of scope");
                }
                CopilotEndpoint::Other(_) => {
                    panic!("unexpected endpoint literal in fixture: {ep:?}")
                }
            }
        }
    }
    assert!(saw_reviewed, "fixture must exercise reviewed endpoints");
    assert!(saw_v1_messages, "fixture must exercise /v1/messages");
    assert!(saw_ws, "fixture must exercise ws:/responses");
}

#[test]
fn policy_state_disabled_is_surfaced_verbatim() {
    let entries = parse_copilot_catalog_entries(FIXTURE).expect("fixture");
    let by_id: BTreeMap<_, _> = entries.iter().map(|e| (e.id.as_str(), e)).collect();
    assert_eq!(
        by_id.get("claude-fable-5").unwrap().policy_state,
        CopilotPolicyState::Disabled
    );
    assert_eq!(
        by_id.get("claude-sonnet-4.6").unwrap().policy_state,
        CopilotPolicyState::Enabled
    );
    // No `policy` field on gpt-5.3-codex in fixture — must surface as Unspecified,
    // not silently coerced to Enabled.
    assert_eq!(
        by_id.get("gpt-5.3-codex").unwrap().policy_state,
        CopilotPolicyState::Unspecified
    );
}
