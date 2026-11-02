//! The child share in a conversation (child spend): exact for a
//! conversation created in this process; after a restore, taken from the
//! last save (clamped, never added back into the totals) and not exact;
//! saved with the snapshot and shown in the snapshot clients get.
use super::*;
use crate::runtime::child_spend::{SpendKind, SpendReport, SpendTokens};
use agent_core::core::session::KeepwarmTokens;
use serde_json::json;
use std::sync::Arc;

fn report(input: u64, output: u64, cost_nanousd: u64) -> SpendReport {
    SpendReport {
        kind: SpendKind::Turn,
        model: "anthropic/claude-sonnet-4-5".to_string(),
        tokens: SpendTokens {
            input,
            output,
            ..SpendTokens::default()
        },
        cost_nanousd,
    }
}

/// A saved session with a child share of `child_cost` (tokens 40/20 and
/// cache buckets) under totals of `cost`, 100 in, 10 out.
fn saved_with_share(cost: f64, child_cost: f64) -> Session {
    let mut s = Session::new("m", "medium", None);
    s.api_messages.push(Arc::new(json!({"role": "user", "content": "q"})));
    s.session_cost = cost;
    s.total_input_tokens = 100;
    s.total_output_tokens = 10;
    s.child_cost = child_cost;
    s.child_tokens = KeepwarmTokens {
        input: 40,
        output: 20,
        cache_read: 300,
        cache_creation: 5,
    };
    s
}

#[test]
fn a_new_conversation_has_an_exact_empty_child_share() {
    let c = ConversationState::new(Session::new("m", "medium", None));
    assert!(c.child_share_exact);
    assert_eq!(c.child_cost, 0.0);
    assert_eq!(c.child_tokens, Default::default());
    assert!(c.snapshot(0).child_share_exact);
}

#[test]
fn a_restore_takes_the_saved_share_clamped_and_is_not_exact() {
    let c = ConversationState::from_resumed(saved_with_share(1.0, 0.25));
    assert!(!c.child_share_exact);
    assert_eq!(c.session_cost, 1.0, "never added back into the totals");
    assert_eq!(c.total_input_tokens, 100);
    assert_eq!(c.child_cost, 0.25);
    assert_eq!(c.child_tokens.input, 40);
    assert_eq!(c.child_tokens.output, 10, "clamped under the output total");
    assert_eq!(c.child_tokens.cache_read, 0, "the cache totals restart, so does their share");
    assert_eq!(c.child_tokens.cache_creation, 0);
    let snap = c.snapshot(0);
    assert_eq!(snap.child_cost, 0.25);
    assert!(!snap.child_share_exact);

    // A stale label above the inclusive cost is clamped under it.
    let c = ConversationState::from_resumed(saved_with_share(0.1, 0.25));
    assert_eq!(c.child_cost, 0.1);
}

#[test]
fn after_a_restore_a_booking_adds_to_the_totals_and_the_share_once() {
    let mut c = ConversationState::from_resumed(saved_with_share(1.0, 0.25));
    c.book_child_spend(&report(5, 1, 50_000_000));
    assert!((c.session_cost - 1.05).abs() < 1e-12, "{}", c.session_cost);
    assert!((c.child_cost - 0.30).abs() < 1e-12, "{}", c.child_cost);
    assert_eq!(c.total_input_tokens, 105);
    assert_eq!(c.child_tokens.input, 45);
    assert!(!c.child_share_exact, "still as of the last save for this load");
}

#[test]
fn a_save_carries_the_child_share_and_the_snapshot_shows_it() {
    let mut c = ConversationState::new(Session::new("m", "medium", None));
    c.api_messages.push(Arc::new(json!({"role": "user", "content": "q"})));
    c.book_child_spend(&report(7, 3, 20_000_000));
    let saved = c.prepare_save().expect("a history to save");
    assert!((saved.session_cost - 0.02).abs() < 1e-12);
    assert!((saved.child_cost - 0.02).abs() < 1e-12);
    assert_eq!(saved.child_tokens.input, 7);
    assert_eq!(saved.child_tokens.output, 3);
    let snap = c.snapshot(0);
    assert_eq!(snap.child_cost, c.child_cost);
    assert_eq!(snap.child_tokens.input, 7);
    assert_eq!(snap.child_tokens.output, 3);
    assert!(snap.child_share_exact);

    // What was saved restores to the same share (not exact).
    let back = ConversationState::from_resumed(saved);
    assert!((back.child_cost - 0.02).abs() < 1e-12);
    assert_eq!(back.child_tokens.input, 7);
    assert!(!back.child_share_exact);
}
