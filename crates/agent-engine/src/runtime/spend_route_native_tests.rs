//! Child spend on the native Anthropic route, against the root's own
//! booking of the same bytes (P1 review r1, F1).
//!
//! The native route is the documented exception to the guard's "an explicit
//! zero replaces" rule: Anthropic's `message_delta` usage is cumulative, and
//! the route has always read a zero or omitted counter there as "not
//! reported" and kept `message_start`'s. The values the route prices for the
//! root (`UsageCounters::record_priced`, and the turn's one Usage event) are
//! authoritative, and a worker's child share books exactly those. Each case
//! runs one request to its end with both a spend tap and the root's usage
//! counters, and compares the two.
use super::*;

use crate::runtime::budget::UsageCounters;

/// What one native request booked on each side.
struct Booked {
    /// The child share: every report the tap booked.
    child: Vec<(u64, SpendReport)>,
    /// The root's priced bookings for the same request.
    root: Arc<UsageCounters>,
    /// The request's Usage events, as debug lines.
    usage_events: Vec<String>,
}

/// Run one request on the native route, whose stand-in answers with
/// `events`, to its end; return both sides' bookings.
async fn native_root_and_child(events: Vec<serde_json::Value>) -> Booked {
    let (spend, log) = tap();
    let body = sse(&events);
    let (url, _) = stand_in(Arc::clone(&log), move |_| stream_ok(body.clone())).await;
    let root = Arc::new(UsageCounters::default());
    let options = super::super::api::ApiOptions {
        anthropic_base_url: Some(url),
        spend_tap: Some(spend),
        usage_counters: Some(Arc::clone(&root)),
        ..Default::default()
    };
    let auth = Arc::new(tokio::sync::RwLock::new(super::super::types::AuthState {
        auth_token: "sk-test".to_string(),
        auth_type: "api_key".to_string(),
        refresh_token: None,
        token_expires: Some(9_999_999_999_999),
        bound_credential: None,
        auth_file_mtime: None,
    }));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    super::super::api::ApiMethods::call_api_stream_inner(
        &auth,
        &reqwest::Client::new(),
        SONNET,
        &crate::tools::ToolRegistry::new(),
        &None,
        0,
        agent_core::reasoning::ReasoningLevel::Adaptive,
        &history(),
        tx,
        &tokio_util::sync::CancellationToken::new(),
        0,
        0,
        &options,
        super::super::telemetry::TelemetryLevel::Off,
    )
    .await
    .expect("the request completes");
    let mut usage_events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        let line = format!("{event:?}");
        if line.contains("Usage {") {
            usage_events.push(line);
        }
    }
    Booked {
        child: booked(&log),
        root,
        usage_events,
    }
}

/// A complete answer: `start_usage` on message_start (none if `None`), the
/// text, and `delta_usage` on message_delta (none if `None`).
fn answer(
    start_usage: Option<serde_json::Value>,
    delta_usage: Option<serde_json::Value>,
) -> Vec<serde_json::Value> {
    let mut first = start(serde_json::Value::Null);
    match start_usage {
        Some(u) => first["message"]["usage"] = u,
        None => {
            first["message"].as_object_mut().unwrap().remove("usage");
        }
    }
    let mut events = vec![first];
    events.extend(text("spend ok"));
    events.push(json!({"type":"content_block_stop","index":0}));
    let mut delta = json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null}});
    if let Some(u) = delta_usage {
        delta["usage"] = u;
    }
    events.push(delta);
    events.push(json!({"type":"message_stop"}));
    events
}

/// The child booked one report, equal in tokens and price to the root's one
/// booking of the same request; returns the report.
fn assert_child_equals_root(b: &Booked) -> SpendReport {
    assert_eq!(b.root.records(), 1, "the root priced the request once");
    assert_eq!(b.child.len(), 1, "the child share books the request once");
    let report = b.child[0].1.clone();
    let t = &report.tokens;
    assert_eq!(
        (t.input, t.output, t.cache_read, t.cache_creation),
        b.root.totals(),
        "the child books the root's tokens"
    );
    assert_eq!(
        report.cost_usd(),
        b.root.cost_usd(),
        "the child books the root's priced value"
    );
    assert_eq!(b.usage_events.len(), 1, "the root emits one Usage event");
    let usage = &b.usage_events[0];
    let counts = format!(
        "Usage {{ input_tokens: {}, output_tokens: {}, cache_read_input_tokens: {}, \
         cache_creation_input_tokens: {}, cache_creation_5m: {:?}, cache_creation_1h: {:?},",
        t.input, t.output, t.cache_read, t.cache_creation, t.cache_creation_5m, t.cache_creation_1h
    );
    assert!(usage.contains(&counts), "the Usage event carries {counts}: {usage}");
    report
}

/// An explicit all-zero final delta does not replace the start's counts on
/// this route: the child books the root's priced value, 82,200 nanodollars
/// (12 input, 1 output, 4 read, 5 written with the delta's zero TTL split,
/// so the write is priced as 1h).
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn native_an_explicit_zero_final_delta_books_the_roots_priced_start() {
    let _base = crate::test_env::BaseDirGuard::new();
    let b = native_root_and_child(answer(Some(usage(12, 1, 4, 5)), Some(usage(0, 0, 0, 0)))).await;
    let report = assert_child_equals_root(&b);
    assert_eq!(report.cost_nanousd, 82_200);
    assert_eq!(
        report.tokens,
        SpendTokens {
            input: 12,
            output: 1,
            cache_read: 4,
            cache_creation: 5,
            cache_creation_5m: Some(0),
            cache_creation_1h: Some(0),
        }
    );
}

/// A final delta that omits the input (and cache) counters keeps the start's;
/// its output count replaces the start's.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn native_an_omitted_input_on_the_final_delta_books_the_roots_priced_value() {
    let _base = crate::test_env::BaseDirGuard::new();
    let b = native_root_and_child(answer(
        Some(usage(12, 1, 4, 5)),
        Some(json!({"output_tokens": 3})),
    ))
    .await;
    let report = assert_child_equals_root(&b);
    assert_eq!(report.tokens, tokens(12, 3, 4, 5));
}

/// A final delta that reports some counters (input and output, no cache
/// counters and no TTL split) keeps the start's for the rest.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn native_a_partial_final_report_books_the_roots_priced_value() {
    let _base = crate::test_env::BaseDirGuard::new();
    let b = native_root_and_child(answer(
        Some(usage(12, 1, 4, 5)),
        Some(json!({"input_tokens": 30, "output_tokens": 7})),
    ))
    .await;
    let report = assert_child_equals_root(&b);
    assert_eq!(report.tokens, tokens(30, 7, 4, 5));
}

/// A stream that states no counts (no usage objects at all, or only zeros)
/// is priced by neither side: the root records nothing and the child books
/// no report.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn native_a_count_less_stream_books_nothing_on_either_side() {
    let _base = crate::test_env::BaseDirGuard::new();
    for (case, events) in [
        ("no usage objects", answer(None, None)),
        ("all-zero usage objects", answer(Some(usage(0, 0, 0, 0)), Some(usage(0, 0, 0, 0)))),
    ] {
        let b = native_root_and_child(events).await;
        assert_eq!(b.root.records(), 0, "{case}: the root prices nothing");
        assert_eq!(b.root.cost_usd(), 0.0, "{case}");
        assert!(b.usage_events.is_empty(), "{case}: no Usage event");
        assert!(b.child.is_empty(), "{case}: the child books no report, got {:?}", b.child);
    }
}

/// `answer`'s stream with a second counted `message_delta` before
/// `message_stop`, the shape Anthropic documents: "One or more message_delta
/// events", whose usage counts "are cumulative". With `cut`, the stream ends
/// after the second delta, before its `message_stop`.
fn two_counted_deltas(cut: bool) -> Vec<serde_json::Value> {
    let mut events = answer(Some(usage(12, 1, 4, 5)), Some(usage(12, 3, 4, 5)));
    let stop = events.pop().expect("message_stop");
    events.push(json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":usage(12, 7, 4, 5)}));
    if !cut {
        events.push(stop);
    }
    events
}

/// Two counted deltas: the counts are cumulative, so the last one is the
/// request's usage. The root prices it once and emits one Usage event with
/// it, and the child books the same: 160,950 nanodollars (12 input, 7
/// output, 4 read, 5 written at 5m). The shape of the r2 review's probe,
/// where the root priced and emitted both deltas.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn native_two_counted_deltas_book_the_last_once_on_both_sides() {
    let _base = crate::test_env::BaseDirGuard::new();
    let b = native_root_and_child(two_counted_deltas(false)).await;
    let report = assert_child_equals_root(&b);
    assert_eq!(report.tokens, tokens(12, 7, 4, 5));
    assert_eq!(report.cost_nanousd, 160_950);
}

/// A stream that ends after its counted delta, before `message_stop`: the
/// delta's counts are emitted and priced once, at the stream's end, on both
/// sides (not the start's).
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn native_a_counted_delta_without_message_stop_books_it_once_at_the_end() {
    let _base = crate::test_env::BaseDirGuard::new();
    let mut events = answer(Some(usage(12, 1, 4, 5)), Some(usage(12, 3, 4, 5)));
    events.pop().expect("message_stop");
    let b = native_root_and_child(events).await;
    let report = assert_child_equals_root(&b);
    assert_eq!(report.tokens, tokens(12, 3, 4, 5));
}

/// Two counted deltas, then the stream ends before `message_stop`: the last
/// delta is booked once on both sides.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn native_two_counted_deltas_then_a_cut_book_the_last_once() {
    let _base = crate::test_env::BaseDirGuard::new();
    let b = native_root_and_child(two_counted_deltas(true)).await;
    let report = assert_child_equals_root(&b);
    assert_eq!(report.tokens, tokens(12, 7, 4, 5));
    assert_eq!(report.cost_nanousd, 160_950);
}
