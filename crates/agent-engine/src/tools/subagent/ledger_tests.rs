//! L obligations at the run's request ledger (§6, §11; slice 6, dark).
use std::collections::BTreeSet;

use super::*;
use crate::tools::subagent::record::{Amount, RequestPurpose};

fn usage(input: u64, output: u64) -> MeasuredUsage {
    MeasuredUsage {
        input_tokens: input,
        output_tokens: output,
        ..MeasuredUsage::default()
    }
}

/// The ledger's entry for `attempt_id`.
fn entry(handle: &RequestLedgerHandle, id: &RequestId) -> LedgerEntry {
    handle
        .snapshot()
        .entries()
        .find(|e| &e.request_id == id)
        .cloned()
        .expect("the request is in the ledger")
}

fn liability(estimate_micro_usd: u64) -> Amount {
    Amount::Liability { estimate_micro_usd }
}

/// L: each upstream attempt is its own request with a fresh host ID,
/// recorded as an open liability before it is sent; a round that retries
/// has one entry per attempt.
#[test]
fn l1_each_attempt_is_reserved_as_its_own_liability_before_it_is_sent() {
    let ledger = RequestLedgerHandle::new("provider/model-b");
    let first = ledger.reserve(120, UsageReports::Cumulative).unwrap();
    let retry = ledger.reserve(120, UsageReports::Cumulative).unwrap();
    assert_ne!(first.request_id(), retry.request_id());
    let e = entry(&ledger, first.request_id());
    assert_eq!(
        (e.model.as_str(), e.purpose, e.amount),
        ("provider/model-b", RequestPurpose::Conversation, liability(120))
    );
    assert_eq!(ledger.snapshot().len(), 2, "one entry per attempt");
    assert_eq!(first.estimate_micro_usd(), 120);
}

/// L: an attempt's measured usage replaces its liability at its end:
/// the last report of a cumulative route, the sum of a delta route's.
#[test]
fn l2_a_finished_attempt_settles_its_reports_by_the_routes_semantics() {
    let ledger = RequestLedgerHandle::new("m");
    let mut cumulative = ledger.reserve(9, UsageReports::Cumulative).unwrap();
    let id = cumulative.request_id().clone();
    cumulative.observe(usage(100, 1));
    cumulative.observe(usage(100, 40));
    assert_eq!(cumulative.finish(), AttemptEnd::Measured);
    assert_eq!(entry(&ledger, &id).amount, Amount::Measured { usage: usage(100, 40) });

    let mut deltas = ledger.reserve(9, UsageReports::Deltas).unwrap();
    let id = deltas.request_id().clone();
    deltas.observe(MeasuredUsage {
        cache_write_5m: Some(3),
        ..usage(100, 1)
    });
    deltas.observe(MeasuredUsage {
        cache_write_1h: Some(2),
        ..usage(0, 40)
    });
    assert_eq!(deltas.finish(), AttemptEnd::Measured);
    assert_eq!(
        entry(&ledger, &id).amount,
        Amount::Measured {
            usage: MeasuredUsage {
                cache_write_5m: Some(3),
                cache_write_1h: Some(2),
                ..usage(100, 41)
            }
        }
    );
}

/// L: unknown spend. A route whose report semantics are not established,
/// or an attempt that reported nothing, keeps its liability open.
#[test]
fn l3_unestablished_or_missing_reports_keep_the_liability_open() {
    let ledger = RequestLedgerHandle::new("m");
    let mut unknown = ledger.reserve(50, UsageReports::Unknown).unwrap();
    let id = unknown.request_id().clone();
    unknown.observe(usage(10, 10));
    assert_eq!(unknown.finish(), AttemptEnd::Open);
    assert_eq!(entry(&ledger, &id).amount, liability(50));

    let silent = ledger.reserve(60, UsageReports::Cumulative).unwrap();
    let id = silent.request_id().clone();
    assert_eq!(silent.finish(), AttemptEnd::Open);
    assert_eq!(entry(&ledger, &id).amount, liability(60));
}

/// L: an attempt dropped unsettled (an error path, a dropped producer) or
/// unwound by a panic keeps its liability open.
#[test]
fn l4_a_dropped_or_panicking_attempt_is_unknown_spend() {
    let ledger = RequestLedgerHandle::new("m");
    let mut dropped = ledger.reserve(70, UsageReports::Cumulative).unwrap();
    let dropped_id = dropped.request_id().clone();
    dropped.observe(usage(5, 5));
    drop(dropped);

    let producer = ledger.clone();
    let panicked = std::thread::spawn(move || {
        let mut attempt = producer.reserve(80, UsageReports::Cumulative).unwrap();
        attempt.observe(usage(5, 5));
        let id = attempt.request_id().clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _held = attempt;
            panic!("transport panicked mid-attempt");
        }));
        id
    })
    .join()
    .unwrap();

    assert_eq!(entry(&ledger, &dropped_id).amount, liability(70));
    assert_eq!(entry(&ledger, &panicked).amount, liability(80));
    // The ledger stays usable after the panic.
    let after = ledger.reserve(1, UsageReports::Cumulative).unwrap();
    assert_eq!(after.finish(), AttemptEnd::Open);
}

/// L: a release ends a liability and restates nothing; it is terminal.
#[test]
fn l5_a_release_ends_the_liability() {
    let ledger = RequestLedgerHandle::new("m");
    let unsent = ledger.reserve(90, UsageReports::Cumulative).unwrap();
    let id = unsent.request_id().clone();
    assert_eq!(unsent.release(), AttemptEnd::Released);
    assert_eq!(entry(&ledger, &id).amount, Amount::Released {});
}

/// L: the run's cap. At 4096 requests the next attempt is refused before
/// anything is sent, with its reason; attempts already reserved still
/// settle at capacity.
#[test]
fn l6_a_full_run_refuses_the_next_attempt_and_still_settles_reserved_ones() {
    let ledger = RequestLedgerHandle::new("m");
    let mut held = Vec::new();
    for _ in 0..Ledger::MAX_ENTRIES {
        held.push(ledger.reserve(1, UsageReports::Cumulative).unwrap());
    }
    let refused = ledger.reserve(1, UsageReports::Cumulative).unwrap_err();
    assert_eq!(refused, ReserveError::RunFull);
    assert!(
        refused.to_string().contains("this run reached 4096 provider requests"),
        "{refused}"
    );
    let mut last = held.pop().unwrap();
    let id = last.request_id().clone();
    last.observe(usage(1, 1));
    assert_eq!(last.finish(), AttemptEnd::Measured);
    assert_eq!(entry(&ledger, &id).amount, Amount::Measured { usage: usage(1, 1) });
    assert_eq!(held.pop().unwrap().release(), AttemptEnd::Released);
    assert_eq!(ledger.snapshot().len(), Ledger::MAX_ENTRIES);
    assert_eq!(
        ledger.reserve(1, UsageReports::Cumulative).unwrap_err(),
        ReserveError::RunFull,
        "a settled request keeps its slot"
    );
}

/// L: a same-model summary is attributed by its purpose, in the same
/// ledger, with IDs from the same sequence.
#[test]
fn l7_a_same_model_summary_is_attributed_by_purpose() {
    let worker = RequestLedgerHandle::new("provider/model-b");
    let summary = worker.for_requests("provider/model-b", RequestPurpose::RolloverSummary);
    assert_eq!(summary.purpose(), RequestPurpose::RolloverSummary);
    let mut round = worker.reserve(1, UsageReports::Cumulative).unwrap();
    let mut summarize = summary.reserve(2, UsageReports::Cumulative).unwrap();
    assert_ne!(round.request_id(), summarize.request_id());
    round.observe(usage(10, 1));
    summarize.observe(usage(20, 2));
    let (round_id, summary_id) = (round.request_id().clone(), summarize.request_id().clone());
    round.finish();
    summarize.finish();
    let by_purpose = |id| {
        let e = entry(&worker, id);
        (e.model, e.purpose)
    };
    assert_eq!(
        by_purpose(&round_id),
        ("provider/model-b".to_owned(), RequestPurpose::Conversation)
    );
    assert_eq!(
        by_purpose(&summary_id),
        ("provider/model-b".to_owned(), RequestPurpose::RolloverSummary)
    );
}

/// L: the estimate is a liability amount only. It never refuses (any
/// amount is reserved), rounds up to a whole micro-USD, and is zero only
/// for a free model.
#[test]
fn l8_the_estimate_never_refuses_and_rounds_up() {
    let ledger = RequestLedgerHandle::new("m");
    assert!(ledger.reserve(u64::MAX, UsageReports::Cumulative).is_ok());
    // Sonnet-class rates: 3 USD / Mtok in, 15 USD / Mtok out.
    assert_eq!(estimate_micro_usd("claude-sonnet-4-6", 1_000_000, 0), 3_000_000);
    assert_eq!(estimate_micro_usd("claude-sonnet-4-6", 0, 1_000), 15_000);
    assert_eq!(estimate_micro_usd("claude-sonnet-4-6", 1, 0), 3, "rounded up");
    assert_eq!(estimate_micro_usd("inferx/deepseek-v4-flash", 1_000, 1_000), 0);
    assert!(
        estimate_micro_usd("claude-sonnet-4-6", 10, 64_000)
            > estimate_micro_usd("claude-sonnet-4-6", 10, 1_000),
        "the whole output allowance is priced"
    );
}

/// L: at a fractional rate the estimate is the exact price rounded up to
/// a whole micro-USD, never below it and at most one micro-USD above it
/// (floating-point noise): checked against integer arithmetic in
/// hundredths of a micro-USD for every token count up to 2,000.
#[test]
fn l8_a_fractional_rate_rounds_up_never_down() {
    // (model, input and output rate in hundredths of a micro-USD per token)
    let rates = [
        ("claude-haiku-5-5", 10, 50),
        ("claude-3-haiku-20240307", 25, 125),
        ("claude-3-5-haiku-20241022", 80, 400),
        ("claude-sonnet-4-6", 300, 1_500),
    ];
    for (model, input_rate, output_rate) in rates {
        for n in 0..=2_000u64 {
            for (input, output) in [(n, 0), (0, n), (n, n / 3)] {
                let exact = (input * input_rate + output * output_rate).div_ceil(100);
                let estimate = estimate_micro_usd(model, input, output);
                assert!(
                    (exact..=exact + 1).contains(&estimate),
                    "{model} {input}+{output}: {estimate} for {exact}"
                );
            }
        }
    }
    assert_eq!(estimate_micro_usd("claude-haiku-5-5", 1, 0), 1, "0.1 micro-USD is 1");
    assert_eq!(estimate_micro_usd("claude-haiku-5-5", 11, 0), 2, "1.1 micro-USD is 2");
}

/// L: token counts at `u64::MAX` saturate instead of wrapping or failing:
/// a price beyond `u64` micro-USD is `u64::MAX`; one within it stays below.
#[test]
fn l8_the_estimate_saturates_at_u64_max() {
    assert_eq!(estimate_micro_usd("claude-sonnet-4-6", u64::MAX, 0), u64::MAX);
    assert_eq!(estimate_micro_usd("claude-sonnet-4-6", 0, u64::MAX), u64::MAX);
    assert_eq!(estimate_micro_usd("claude-sonnet-4-6", u64::MAX, u64::MAX), u64::MAX);
    // Haiku 5.5's large-prompt tier, 0.5 USD / Mtok in: about u64::MAX / 2.
    let half = estimate_micro_usd("claude-haiku-5-5", u64::MAX, 0);
    assert!((u64::MAX / 2..u64::MAX).contains(&half), "{half}");
    assert_eq!(estimate_micro_usd("inferx/deepseek-v4-flash", u64::MAX, u64::MAX), 0);
}

/// L: a price that is not a finite amount is never free: positive
/// infinity, and anything not a number, is the largest estimate; a tiny
/// positive price is at least one micro-USD; only zero (or less) is zero.
#[test]
fn l8_a_nonfinite_price_is_never_free() {
    assert_eq!(micro_usd_ceil(f64::INFINITY), u64::MAX);
    assert_eq!(micro_usd_ceil(f64::NAN), u64::MAX);
    assert_eq!(micro_usd_ceil(1e30), u64::MAX);
    assert_eq!(micro_usd_ceil(1e-12), 1);
    assert_eq!(micro_usd_ceil(0.0), 0);
    assert_eq!(micro_usd_ceil(-0.0), 0);
    assert_eq!(micro_usd_ceil(-1.0), 0);
    assert_eq!(micro_usd_ceil(f64::NEG_INFINITY), 0);
}

/// L: a snapshot is a valid stored ledger (strict round trip), open
/// liabilities included, and senders on several threads never share an ID.
#[test]
fn l9_snapshots_round_trip_and_concurrent_senders_get_distinct_ids() {
    let ledger = RequestLedgerHandle::new("m");
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let ledger = ledger.clone();
            std::thread::spawn(move || {
                (0..50)
                    .map(|i| {
                        let mut a = ledger.reserve(i, UsageReports::Cumulative).unwrap();
                        let id = a.request_id().clone();
                        if i % 2 == 0 {
                            a.observe(usage(i, i));
                            a.finish();
                        }
                        id
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let ids: BTreeSet<_> = threads
        .into_iter()
        .flat_map(|t| t.join().unwrap())
        .collect();
    assert_eq!(ids.len(), 400);
    let snapshot = ledger.snapshot();
    assert_eq!(snapshot.len(), 400);
    let open = snapshot
        .entries()
        .filter(|e| matches!(e.amount, Amount::Liability { .. }))
        .count();
    assert_eq!(open, 200);
    let stored = serde_json::to_string(&snapshot).unwrap();
    let back: Ledger = serde_json::from_str(&stored).unwrap();
    assert_eq!(back, snapshot);
}

/// An attempt names itself in the run's ledger events by its request ID,
/// purpose and model; usage maps both ways without loss.
#[test]
fn an_attempts_identity_and_usage_map_onto_the_ledger_events() {
    let worker = RequestLedgerHandle::new("provider/model-b");
    let summary = worker.for_requests("provider/model-c", RequestPurpose::RolloverSummary);
    let round = worker.reserve(1, UsageReports::Cumulative).unwrap();
    let summarize = summary.reserve(1, UsageReports::Cumulative).unwrap();
    assert_eq!(
        round.usage_request(),
        agent_core::UsageRequest {
            request_id: round.request_id().as_str().to_owned(),
            purpose: agent_core::UsagePurpose::Conversation,
            model: "provider/model-b".into(),
        }
    );
    assert_eq!(
        summarize.usage_request().purpose,
        agent_core::UsagePurpose::RolloverSummary
    );
    assert_eq!(summarize.usage_request().model, "provider/model-c");
    let full = MeasuredUsage {
        input_tokens: 1,
        output_tokens: 2,
        cache_read_tokens: 3,
        cache_write_tokens: 4,
        cache_write_5m: Some(5),
        cache_write_1h: None,
    };
    assert_eq!(measured(&counts(&full)), full);
    assert_eq!(counts(&full).cache_read_input_tokens, 3);
    assert_eq!(counts(&full).cache_creation_input_tokens, 4);
}

// ── The transports' attempt reports ───────────────────────────────────────

type Rx = tokio::sync::mpsc::UnboundedReceiver<agent_core::StreamEvent>;

fn reports() -> (tokio::sync::mpsc::UnboundedSender<agent_core::StreamEvent>, Rx) {
    tokio::sync::mpsc::unbounded_channel()
}

/// The liability events `rx` holds, as (request ID, estimate, state).
fn liabilities(rx: &mut Rx) -> Vec<(String, u64, agent_core::LiabilityState)> {
    let mut out = Vec::new();
    while let Ok(event) = rx.try_recv() {
        match event {
            agent_core::StreamEvent::Session(agent_core::SessionEvent::UsageLiability {
                request,
                estimate_micro_usd,
                state,
            }) => out.push((request.request_id, estimate_micro_usd, state)),
            other => panic!("unexpected event {other:?}"),
        }
    }
    out
}

fn reported(input: u64, output: u64) -> UsageCounts {
    UsageCounts {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: 4,
        cache_creation_input_tokens: 5,
        cache_creation_5m: Some(5),
        cache_creation_1h: None,
    }
}

/// L: a transport's attempt is in the ledger as an open liability from its
/// reservation on, before anything is reported; dropped before it ends (an
/// error path, a cancel), it stays open and says so.
#[test]
fn k1_a_reported_attempt_is_reserved_first_and_a_dropped_one_is_open() {
    use agent_core::LiabilityState::Open;
    let ledger = RequestLedgerHandle::new("provider/model-b");
    let (tx, mut rx) = reports();
    let report = AttemptReport::reserve(&ledger, 9, UsageReports::Cumulative, &tx).unwrap();
    let id = RequestId::parse(&report.request().request_id).unwrap();
    assert_eq!(entry(&ledger, &id).amount, liability(9));
    assert!(liabilities(&mut rx).is_empty(), "nothing reported yet");
    drop(report);
    assert_eq!(entry(&ledger, &id).amount, liability(9));
    assert_eq!(liabilities(&mut rx), vec![(id.as_str().to_owned(), 9, Open)]);
}

/// L: an attempt that ends with its final report observed is measured
/// silently (its `UsageAttempt` already reported it); without one it is
/// open, and a release ends it; both are reported.
#[test]
fn k2_an_attempt_ends_measured_open_or_released() {
    use agent_core::LiabilityState::{Open, Released};
    let ledger = RequestLedgerHandle::new("provider/model-b");
    let (tx, mut rx) = reports();

    let mut measured_one = AttemptReport::reserve(&ledger, 9, UsageReports::Cumulative, &tx).unwrap();
    let id = RequestId::parse(&measured_one.request().request_id).unwrap();
    measured_one.observe(&reported(1, 1));
    measured_one.observe(&reported(12, 3));
    assert_eq!(measured_one.finish(), AttemptEnd::Measured);
    assert_eq!(
        entry(&ledger, &id).amount,
        Amount::Measured {
            usage: measured(&reported(12, 3))
        },
        "a cumulative route settles with its last report"
    );
    assert!(liabilities(&mut rx).is_empty());

    let open = AttemptReport::reserve(&ledger, 7, UsageReports::Cumulative, &tx).unwrap();
    let open_id = open.request().request_id.clone();
    assert_eq!(open.finish(), AttemptEnd::Open);
    assert_eq!(liabilities(&mut rx), vec![(open_id, 7, Open)]);

    let freed = AttemptReport::reserve(&ledger, 5, UsageReports::Cumulative, &tx).unwrap();
    let freed_id = RequestId::parse(&freed.request().request_id).unwrap();
    assert_eq!(freed.release(), AttemptEnd::Released);
    assert_eq!(entry(&ledger, &freed_id).amount, Amount::Released {});
    assert_eq!(
        liabilities(&mut rx),
        vec![(freed_id.as_str().to_owned(), 5, Released)]
    );
}

/// L14: without a ledgered request the usage event is exactly the plain
/// `Usage` the transport always sent; with one, the attempt's.
#[test]
fn k3_the_usage_event_is_the_plain_usage_unless_the_request_is_ledgered() {
    let plain = usage_event(None, reported(12, 3), Some("m".into()));
    assert_eq!(
        format!("{plain:?}"),
        format!(
            "{:?}",
            agent_core::SessionEvent::Usage {
                input_tokens: 12,
                output_tokens: 3,
                cache_read_input_tokens: 4,
                cache_creation_input_tokens: 5,
                cache_creation_5m: Some(5),
                cache_creation_1h: None,
                model: Some("m".into()),
            }
        )
    );
    let ledger = RequestLedgerHandle::new("provider/model-b");
    let (tx, _rx) = reports();
    let report = AttemptReport::reserve(&ledger, 1, UsageReports::Cumulative, &tx).unwrap();
    match usage_event(Some(report.request()), reported(12, 3), None) {
        agent_core::SessionEvent::UsageAttempt { request, usage } => {
            assert_eq!(&request, report.request());
            assert_eq!(usage, reported(12, 3));
        }
        other => panic!("expected the attempt's usage, got {other:?}"),
    }
}

/// L8: a request's estimate covers its serialized body conservatively and
/// its whole output allowance.
#[test]
fn k4_a_requests_estimate_covers_its_body_and_output_allowance() {
    let body = "x".repeat(25_000);
    let est = request_estimate("claude-sonnet-4-6", body.as_bytes(), 1000);
    // 25 000 ASCII bytes are at least 10 000 conservative tokens.
    assert!(est >= estimate_micro_usd("claude-sonnet-4-6", 10_000, 1000), "{est}");
    assert!(est > request_estimate("claude-sonnet-4-6", body.as_bytes(), 0));
}

// ── A v2 oneshot's aggregate (8f) ──────────────────────────────────────────────

/// One attempt of `handle`'s that reported `usage` and finished measured.
fn measured_attempt(handle: &RequestLedgerHandle, usage: MeasuredUsage) {
    let mut attempt = handle.reserve(1, UsageReports::Cumulative).unwrap();
    attempt.observe(usage);
    assert_eq!(attempt.finish(), AttemptEnd::Measured);
}

/// f1 (8f, Q5): a v2 oneshot's aggregate is its ledger's measured
/// conversation on the worker's model. A summary on the same model, a
/// conversation request on another model, an open and a released attempt
/// add nothing, and a report delivered again counts once.
#[test]
fn f1_the_aggregate_is_the_measured_conversation_on_the_workers_model() {
    let worker = RequestLedgerHandle::new("provider/model-b");
    measured_attempt(
        &worker,
        MeasuredUsage {
            input_tokens: 10,
            output_tokens: 1,
            cache_read_tokens: 2,
            cache_write_tokens: 4,
            cache_write_5m: Some(4),
            cache_write_1h: None,
        },
    );
    measured_attempt(
        &worker,
        MeasuredUsage {
            input_tokens: 5,
            output_tokens: 2,
            cache_read_tokens: 1,
            cache_write_tokens: 3,
            cache_write_5m: None,
            cache_write_1h: Some(3),
        },
    );
    measured_attempt(&worker.for_summary("provider/model-b"), usage(20, 2));
    measured_attempt(
        &worker.for_requests("provider/model-c", RequestPurpose::Conversation),
        usage(7, 7),
    );
    let mut open = worker.reserve(1, UsageReports::Cumulative).unwrap();
    open.observe(usage(9, 9));
    drop(open);
    let released = worker.reserve(1, UsageReports::Cumulative).unwrap();
    assert_eq!(released.release(), AttemptEnd::Released);

    let mut ledger = worker.snapshot();
    let again = ledger
        .entries()
        .find(|e| {
            e.model == "provider/model-b"
                && e.purpose == RequestPurpose::Conversation
                && matches!(e.amount, Amount::Measured { .. })
        })
        .cloned()
        .unwrap();
    assert_eq!(
        ledger.settle(again),
        Ok(crate::tools::subagent::record::Settled::Duplicate),
        "a report delivered again"
    );
    assert_eq!(
        conversation_aggregate(&ledger, "provider/model-b"),
        MeasuredUsage {
            input_tokens: 15,
            output_tokens: 3,
            cache_read_tokens: 3,
            cache_write_tokens: 7,
            cache_write_5m: Some(4),
            cache_write_1h: Some(3),
        }
    );
    assert_eq!(
        conversation_aggregate(&ledger, "provider/model-c"),
        usage(7, 7),
        "by model"
    );
    assert_eq!(
        conversation_aggregate(&Ledger::default(), "provider/model-b"),
        MeasuredUsage::default()
    );
}

/// f3 (8f): a run's aggregate is claimed once in this process; another
/// run's claim is its own.
#[test]
fn f3_a_runs_aggregate_is_claimed_once() {
    let run = crate::tools::subagent::record::RunId::random();
    let other = crate::tools::subagent::record::RunId::random();
    assert!(claim_aggregate(run.as_str()), "the run's first publication");
    assert!(!claim_aggregate(run.as_str()), "never a second one");
    assert!(!claim_aggregate(run.as_str()));
    assert!(claim_aggregate(other.as_str()), "another run's is its own");
}
