//! P1a: the child-spend sink and run lease (child_spend.rs).

use super::*;
use serde_json::json;
use std::sync::Barrier;

fn report(n: u64) -> SpendReport {
    SpendReport {
        kind: SpendKind::Turn,
        model: "anthropic/claude-sonnet-4-6".to_string(),
        tokens: SpendTokens {
            input: n,
            ..SpendTokens::default()
        },
        cost_nanousd: 1_000 + n,
    }
}

fn drain(rx: &mut SpendReceiver) -> Vec<SinkMessage> {
    let mut out = Vec::new();
    while let Ok(message) = rx.try_recv() {
        out.push(message);
    }
    out
}

/// (handle, kind, seq-or-next) per message, for compact assertions.
fn shape(messages: &[SinkMessage]) -> Vec<(String, &'static str, u64)> {
    messages
        .iter()
        .map(|m| match m {
            SinkMessage::Spend { run, seq, .. } => (run.handle.clone(), "spend", *seq),
            SinkMessage::RunSealed { run, next_seq } => (run.handle.clone(), "sealed", *next_seq),
            SinkMessage::RunEnded { run, next_seq } => (run.handle.clone(), "ended", *next_seq),
        })
        .collect()
}

#[test]
fn p1_a_runs_reports_are_numbered_from_zero_without_gaps_in_order() {
    let (sink, mut rx) = SpendSink::new();
    let lease = sink.lease("sa_1", "reviewer").unwrap();
    assert_eq!(sink.producers(), 1);
    for n in 0..3 {
        assert_eq!(lease.book(report(n)), BookOutcome::Booked { seq: n });
    }
    let got = drain(&mut rx);
    assert_eq!(
        shape(&got),
        vec![
            ("sa_1".into(), "spend", 0),
            ("sa_1".into(), "spend", 1),
            ("sa_1".into(), "spend", 2),
        ]
    );
    // The report travels as booked.
    match &got[2] {
        SinkMessage::Spend { report: r, run, .. } => {
            assert_eq!(r, &report(2));
            assert_eq!(**run, *lease.identity());
        }
        other => panic!("unexpected {other:?}"),
    }
    // A second run on the same sink numbers its own reports.
    let other = sink.lease("sa_2", "reviewer").unwrap();
    assert_eq!(other.book(report(9)), BookOutcome::Booked { seq: 0 });
    assert_eq!(lease.book(report(3)), BookOutcome::Booked { seq: 3 });
    assert_eq!(
        shape(&drain(&mut rx)),
        vec![("sa_2".into(), "spend", 0), ("sa_1".into(), "spend", 3)]
    );
}

#[test]
fn p2_a_seal_follows_every_published_report_and_stops_publication() {
    let (sink, mut rx) = SpendSink::new();
    let lease = sink.lease("sa_1", "w").unwrap();
    lease.book(report(0));
    lease.book(report(1));
    assert!(lease.seal());
    assert!(!lease.is_live());
    assert_eq!(sink.producers(), 0);
    // Late spend after the seal is not published.
    assert_eq!(
        lease.book(report(2)),
        BookOutcome::NotBooked(NotBooked::RunNotLive)
    );
    assert_eq!(
        shape(&drain(&mut rx)),
        vec![
            ("sa_1".into(), "spend", 0),
            ("sa_1".into(), "spend", 1),
            ("sa_1".into(), "sealed", 2),
        ]
    );
}

#[test]
fn p3_sealing_twice_or_dropping_a_sealed_run_adds_no_terminal_and_no_decrement() {
    let (sink, mut rx) = SpendSink::new();
    let lease = sink.lease("sa_1", "w").unwrap();
    let keep = sink.lease("sa_2", "w").unwrap();
    assert_eq!(sink.producers(), 2);
    assert!(lease.seal());
    assert!(!lease.seal());
    assert!(!lease.clone().seal());
    assert_eq!(sink.producers(), 1, "one decrement for one run");
    drop(lease);
    assert_eq!(sink.producers(), 1, "the drop of a sealed run decrements nothing");
    assert_eq!(shape(&drain(&mut rx)), vec![("sa_1".into(), "sealed", 0)]);
    drop(keep);
    assert_eq!(sink.producers(), 0);
    assert_eq!(shape(&drain(&mut rx)), vec![("sa_2".into(), "ended", 0)]);
}

#[test]
fn p4_the_last_clones_drop_ends_the_run_once() {
    let (sink, mut rx) = SpendSink::new();
    let lease = sink.lease("sa_1", "w").unwrap();
    let clone = lease.clone();
    clone.book(report(0));
    drop(lease);
    assert_eq!(drain(&mut rx).len(), 1, "a clone's drop is not the end");
    assert_eq!(sink.producers(), 1);
    clone.book(report(1));
    drop(clone);
    assert_eq!(sink.producers(), 0);
    assert_eq!(
        shape(&drain(&mut rx)),
        vec![("sa_1".into(), "spend", 1), ("sa_1".into(), "ended", 2)]
    );
}

#[test]
fn p5_a_closed_sink_publishes_nothing_but_still_counts_producers_down() {
    let (sink, mut rx) = SpendSink::new();
    let a = sink.lease("sa_1", "w").unwrap();
    let b = sink.lease("sa_2", "w").unwrap();
    let c = sink.lease("sa_3", "w").unwrap();
    a.book(report(0));
    assert_eq!(sink.close(), SinkPhase::Open);
    assert_eq!(sink.phase(), SinkPhase::Closed);
    assert_eq!(
        a.book(report(1)),
        BookOutcome::NotBooked(NotBooked::SessionClosed)
    );
    assert!(b.seal());
    drop(c);
    assert_eq!(sink.producers(), 1);
    drop(a);
    assert_eq!(sink.producers(), 0);
    assert_eq!(shape(&drain(&mut rx)), vec![("sa_1".into(), "spend", 0)]);
    // Closing again keeps it closed; it never reopens.
    assert_eq!(sink.begin_closing(), SinkPhase::Closed);
    assert_eq!(sink.phase(), SinkPhase::Closed);
}

#[test]
fn p6_new_runs_are_refused_under_a_sealed_parent_or_an_ending_session() {
    let (sink, mut rx) = SpendSink::new();
    let parent = sink.lease("sa_1", "w").unwrap();
    let child = parent.child("sa_2", "w").unwrap();
    assert_eq!(child.identity().parent_handle.as_deref(), Some("sa_1"));
    assert_eq!(parent.identity().parent_handle, None);
    assert_eq!(sink.producers(), 2);
    assert!(child.sink().same_sink(&sink));

    assert!(parent.seal());
    let refused = parent.child("sa_3", "w").unwrap_err();
    assert_eq!(refused, LeaseRefused::ParentSealed);
    assert_eq!(refused.to_string(), "parent run sealed: no new subagent work");
    // A live child of a sealed run still publishes (it was admitted before).
    assert_eq!(child.book(report(0)), BookOutcome::Booked { seq: 0 });
    assert_eq!(sink.producers(), 1);

    assert_eq!(sink.begin_closing(), SinkPhase::Open);
    let refused = sink.lease("sa_4", "w").unwrap_err();
    assert_eq!(refused, LeaseRefused::SessionEnding);
    assert_eq!(refused.to_string(), "session ending: no new subagent work");
    assert_eq!(
        child.child("sa_5", "w").unwrap_err(),
        LeaseRefused::SessionEnding
    );
    assert_eq!(sink.producers(), 1, "a refusal counts no producer");
    // Closing still publishes reports.
    assert_eq!(child.book(report(1)), BookOutcome::Booked { seq: 1 });
    drop(child);
    assert_eq!(sink.producers(), 0);
    assert_eq!(
        shape(&drain(&mut rx)),
        vec![
            ("sa_1".into(), "sealed", 0),
            ("sa_2".into(), "spend", 0),
            ("sa_2".into(), "spend", 1),
            ("sa_2".into(), "ended", 2),
        ]
    );
}

#[test]
fn p7_a_close_racing_bookings_never_loses_a_booked_report_or_leaves_a_gap() {
    for _ in 0..200 {
        let (sink, mut rx) = SpendSink::new();
        let threads = 4;
        let leases: Vec<RunLease> = (0..threads)
            .map(|i| sink.lease(&format!("sa_{i}"), "w").unwrap())
            .collect();
        let start = Arc::new(Barrier::new(threads + 1));
        let mut joins = Vec::new();
        for lease in leases {
            let start = Arc::clone(&start);
            joins.push(std::thread::spawn(move || {
                start.wait();
                let outcomes: Vec<BookOutcome> = (0..50).map(|n| lease.book(report(n))).collect();
                (lease, outcomes)
            }));
        }
        start.wait();
        std::thread::yield_now();
        sink.close();
        let results: Vec<_> = joins.into_iter().map(|j| j.join().unwrap()).collect();
        let mut kept = Vec::new();
        let results: Vec<_> = results
            .into_iter()
            .map(|(lease, outcomes)| {
                let handle = lease.identity().handle.clone();
                kept.push(lease);
                (handle, outcomes)
            })
            .collect();
        // Dropped after the close: no terminal is published.
        drop(kept);
        let got = drain(&mut rx);
        assert_eq!(sink.producers(), 0);
        for (handle, outcomes) in results {
            // Once refused, never booked again; booked seqs are 0..k.
            let booked: Vec<u64> = outcomes
                .iter()
                .take_while(|o| matches!(o, BookOutcome::Booked { .. }))
                .map(|o| match o {
                    BookOutcome::Booked { seq } => *seq,
                    _ => unreachable!(),
                })
                .collect();
            assert!(outcomes[booked.len()..]
                .iter()
                .all(|o| *o == BookOutcome::NotBooked(NotBooked::SessionClosed)));
            assert_eq!(booked, (0..booked.len() as u64).collect::<Vec<_>>());
            let received: Vec<u64> = got
                .iter()
                .filter_map(|m| match m {
                    SinkMessage::Spend { run, seq, .. } if run.handle == handle => Some(*seq),
                    _ => None,
                })
                .collect();
            assert_eq!(received, booked, "every booked report arrives, in order");
        }
        assert!(
            got.iter().all(|m| matches!(m, SinkMessage::Spend { .. })),
            "the leases dropped after the close send no terminal"
        );
    }
}

/// The deterministic variant of p7, which can pass with no report
/// accepted at all: here each thread books its first report before the
/// close (a barrier orders them), then races the close with the rest. Each
/// first report is booked, and every booked report arrives exactly once, in
/// order. A lease that books nothing turns this red.
#[test]
fn p7b_reports_accepted_before_a_close_each_arrive_exactly_once() {
    for _ in 0..50 {
        let (sink, mut rx) = SpendSink::new();
        let threads = 4;
        let leases: Vec<RunLease> = (0..threads)
            .map(|i| sink.lease(&format!("sa_{i}"), "w").unwrap())
            .collect();
        let accepted = Arc::new(Barrier::new(threads + 1));
        let mut joins = Vec::new();
        for lease in leases {
            let accepted = Arc::clone(&accepted);
            joins.push(std::thread::spawn(move || {
                let mut outcomes = vec![lease.book(report(0))];
                accepted.wait();
                outcomes.extend((1..50).map(|n| lease.book(report(n))));
                (lease, outcomes)
            }));
        }
        accepted.wait();
        sink.close();
        let results: Vec<_> = joins.into_iter().map(|j| j.join().unwrap()).collect();
        let got = drain(&mut rx);
        for (lease, outcomes) in &results {
            let handle = &lease.identity().handle;
            assert_eq!(
                outcomes[0],
                BookOutcome::Booked { seq: 0 },
                "{handle}: the report accepted before the close is booked"
            );
            let booked: Vec<u64> = outcomes
                .iter()
                .map_while(|o| match o {
                    BookOutcome::Booked { seq } => Some(*seq),
                    _ => None,
                })
                .collect();
            assert!(outcomes[booked.len()..]
                .iter()
                .all(|o| *o == BookOutcome::NotBooked(NotBooked::SessionClosed)));
            assert_eq!(booked, (0..booked.len() as u64).collect::<Vec<_>>());
            let received: Vec<u64> = got
                .iter()
                .filter_map(|m| match m {
                    SinkMessage::Spend { run, seq, .. } if &run.handle == handle => Some(*seq),
                    _ => None,
                })
                .collect();
            assert_eq!(
                received, booked,
                "{handle}: every booked report arrives exactly once, in order"
            );
        }
        drop(results);
        assert_eq!(sink.producers(), 0);
    }
}

#[test]
fn p8_a_seal_racing_bookings_arrives_after_every_report_it_counts() {
    for _ in 0..200 {
        let (sink, mut rx) = SpendSink::new();
        let lease = sink.lease("sa_1", "w").unwrap();
        let start = Arc::new(Barrier::new(4));
        let mut joins = Vec::new();
        for _ in 0..3 {
            let lease = lease.clone();
            let start = Arc::clone(&start);
            joins.push(std::thread::spawn(move || {
                start.wait();
                (0..40).map(|n| lease.book(report(n))).collect::<Vec<_>>()
            }));
        }
        start.wait();
        std::thread::yield_now();
        assert!(lease.seal());
        let outcomes: Vec<BookOutcome> = joins
            .into_iter()
            .flat_map(|j| j.join().unwrap())
            .collect();
        drop(lease);
        let got = drain(&mut rx);
        let booked = outcomes
            .iter()
            .filter(|o| matches!(o, BookOutcome::Booked { .. }))
            .count() as u64;
        assert!(outcomes.iter().all(|o| matches!(
            o,
            BookOutcome::Booked { .. } | BookOutcome::NotBooked(NotBooked::RunNotLive)
        )));
        let (last, spends) = got.split_last().unwrap();
        match last {
            SinkMessage::RunSealed { run, next_seq } => {
                assert_eq!(run.handle, "sa_1");
                assert_eq!(*next_seq, booked, "the seal counts every published report");
            }
            other => panic!("the last message is not the seal: {other:?}"),
        }
        let seqs: Vec<u64> = spends
            .iter()
            .map(|m| match m {
                SinkMessage::Spend { seq, .. } => *seq,
                other => panic!("a terminal before the last message: {other:?}"),
            })
            .collect();
        assert_eq!(seqs, (0..booked).collect::<Vec<_>>());
        assert_eq!(sink.producers(), 0);
    }
}

#[test]
fn p9_a_poisoned_run_or_gate_lock_does_not_stop_publication_or_panic_on_drop() {
    let (sink, mut rx) = SpendSink::new();
    let lease = sink.lease("sa_1", "w").unwrap();
    // Poison the run mutex.
    {
        let l = lease.clone();
        let _ = std::thread::spawn(move || {
            let _guard = l.inner.run.lock().unwrap();
            panic!("poison the run");
        })
        .join();
    }
    assert!(lease.inner.run.is_poisoned());
    assert_eq!(lease.book(report(0)), BookOutcome::Booked { seq: 0 });
    // Poison the gate.
    {
        let s = sink.clone();
        let _ = std::thread::spawn(move || {
            let _guard = s.inner.gate.lock().unwrap();
            panic!("poison the gate");
        })
        .join();
    }
    assert!(sink.inner.gate.is_poisoned());
    assert_eq!(lease.book(report(1)), BookOutcome::Booked { seq: 1 });
    let child = lease.child("sa_2", "w").unwrap();
    assert!(child.seal());
    assert_eq!(sink.producers(), 1);
    drop(child);
    drop(lease);
    assert_eq!(sink.producers(), 0);
    assert_eq!(
        shape(&drain(&mut rx)),
        vec![
            ("sa_1".into(), "spend", 0),
            ("sa_1".into(), "spend", 1),
            ("sa_2".into(), "sealed", 0),
            ("sa_1".into(), "ended", 2),
        ]
    );
}

#[test]
fn p10_a_gone_receiver_is_reported_once_and_then_the_sink_reads_closed() {
    let (sink, rx) = SpendSink::new();
    let a = sink.lease("sa_1", "w").unwrap();
    let b = sink.lease("sa_2", "w").unwrap();
    drop(rx);
    assert_eq!(a.book(report(0)), BookOutcome::ReceiverGone);
    assert_eq!(sink.phase(), SinkPhase::Closed);
    assert_eq!(
        a.book(report(1)),
        BookOutcome::NotBooked(NotBooked::SessionClosed)
    );
    assert!(b.seal());
    drop(b);
    drop(a);
    assert_eq!(sink.producers(), 0);
    assert_eq!(
        sink.lease("sa_3", "w").unwrap_err(),
        LeaseRefused::SessionEnding
    );
}

#[test]
fn p11_a_run_identity_is_fresh_per_lease_with_a_bounded_flat_agent_label() {
    let (sink, _rx) = SpendSink::new();
    let label = format!("  re\nview\u{2028}er\t{}\u{1b}  ", "x".repeat(100));
    let a = sink.lease("sa_1", &label).unwrap();
    let b = sink.lease("sa_1", "w").unwrap();
    let id = a.identity();
    assert_eq!(id.handle, "sa_1");
    assert_ne!(id.run_nonce, b.identity().run_nonce, "a resume is a new run");
    let hex = id.run_hex();
    assert_eq!(hex.len(), 16);
    assert!(hex.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_eq!(u64::from_str_radix(&hex, 16).unwrap(), id.run_nonce);
    assert_eq!(id.agent.chars().count(), MAX_AGENT_CHARS);
    assert!(id.agent.starts_with("re view er "));
    assert!(!id.agent.chars().any(|c| c.is_control()));
    let zero = RunIdentity {
        run_nonce: 0x0a,
        ..id.clone()
    };
    assert_eq!(zero.run_hex(), "000000000000000a");
}

#[test]
fn p12_a_report_is_priced_like_the_sessions_own_usage_counters() {
    let tokens = SpendTokens {
        input: 1_200,
        output: 340,
        cache_read: 45_000,
        cache_creation: 2_100,
        cache_creation_5m: Some(2_100),
        cache_creation_1h: Some(0),
    };
    let model = "claude-sonnet-4-6";
    let r = SpendReport::priced(SpendKind::Summary, model, tokens);
    assert_eq!(r.kind.wire_name(), "summary");
    assert_eq!(SpendKind::Turn.wire_name(), "turn");
    let counters = crate::runtime::budget::UsageCounters::default();
    counters.record_priced(model, 1_200, 340, 45_000, 2_100, Some(2_100), Some(0));
    assert!(r.cost_nanousd > 0);
    assert_eq!(r.cost_usd(), counters.cost_usd());
    // An unknown model keeps the table's nonzero fallback, as the session does.
    let unknown = SpendReport::priced(SpendKind::Turn, "nobody/unknown-model", tokens);
    assert!(unknown.cost_nanousd > 0);
}

#[test]
fn p13_producers_count_live_leases_across_admit_seal_and_drop() {
    let (sink, _rx) = SpendSink::new();
    let a = sink.lease("sa_1", "w").unwrap();
    let b = a.child("sa_2", "w").unwrap();
    let c = b.child("sa_3", "w").unwrap();
    assert_eq!(sink.producers(), 3);
    b.seal();
    assert_eq!(sink.producers(), 2);
    drop(b);
    assert_eq!(sink.producers(), 2);
    drop(c);
    assert_eq!(sink.producers(), 1);
    drop(a);
    assert_eq!(sink.producers(), 0);
}

// ═══ P1c: per-attempt booking ══════════════════════════════════════════════

fn tap() -> (SpendTap, SpendReceiver) {
    let (sink, rx) = SpendSink::new();
    let lease = sink.lease("sa_1", "w").unwrap();
    (SpendTap::new(lease), rx)
}

fn reported(input: u64, output: u64) -> ReportedUsage {
    ReportedUsage {
        input: Some(input),
        output: Some(output),
        cache_read: Some(0),
        cache_creation: Some(0),
        ..ReportedUsage::default()
    }
}

/// The booked reports' (kind, model, tokens), in order.
fn booked(rx: &mut SpendReceiver) -> Vec<(SpendKind, String, SpendTokens)> {
    drain(rx)
        .into_iter()
        .filter_map(|m| match m {
            SinkMessage::Spend { report, .. } => Some((report.kind, report.model, report.tokens)),
            _ => None,
        })
        .collect()
}

/// Negative control: an inert guard has no sink to publish to, so this
/// passes for any guard that publishes nothing. It shows the inert path
/// neither panics nor needs a sink; that a guard with a tap publishes is
/// a2-a9's (and the route matrix's) to show.
#[test]
fn a1_a_guard_without_a_tap_is_inert() {
    let guard = AttemptSpend::new(None, SpendKind::Turn, "claude-sonnet-4-6");
    assert!(guard.is_inert());
    guard.observe(&reported(5, 1));
    guard.observer().observe(&reported(6, 2));
    guard.settle();
    let dropped = AttemptSpend::inert();
    dropped.observe(&reported(5, 1));
    drop(dropped);
    // Nothing to see: no sink was involved at all.
}

#[test]
fn a2_the_last_report_with_counts_is_booked_once_as_a_whole() {
    let (tap, mut rx) = tap();
    let guard = AttemptSpend::new(Some(&tap), SpendKind::Turn, "claude-sonnet-4-6");
    guard.observe(&ReportedUsage {
        input: Some(12),
        output: Some(0),
        cache_read: Some(4),
        cache_creation: Some(5),
        cache_creation_5m: Some(5),
        cache_creation_1h: Some(0),
    });
    // A later partial report replaces the earlier one; it is not merged.
    guard.observe(&ReportedUsage {
        output: Some(3),
        ..ReportedUsage::default()
    });
    guard.settle();
    assert_eq!(
        booked(&mut rx),
        vec![(
            SpendKind::Turn,
            "claude-sonnet-4-6".to_string(),
            SpendTokens {
                output: 3,
                ..SpendTokens::default()
            }
        )]
    );
}

#[test]
fn a3_a_report_without_counts_is_ignored() {
    let (tap, mut rx) = tap();
    let guard = AttemptSpend::new(Some(&tap), SpendKind::Turn, "m");
    guard.observe(&reported(10, 3));
    guard.observe(&ReportedUsage::from_anthropic_json(&json!({})));
    guard.observe(&ReportedUsage::from_anthropic_json(&serde_json::Value::Null));
    guard.observe(&ReportedUsage::default());
    guard.settle();
    let got = booked(&mut rx);
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].2.input, got[0].2.output), (10, 3));
}

#[test]
fn a4_an_explicit_zero_is_a_report_and_replaces() {
    let (tap, mut rx) = tap();
    let guard = AttemptSpend::new(Some(&tap), SpendKind::Turn, "m");
    guard.observe(&reported(10, 3));
    guard.observe(&ReportedUsage {
        output: Some(0),
        ..ReportedUsage::default()
    });
    guard.settle();
    assert_eq!(
        booked(&mut rx),
        vec![(SpendKind::Turn, "m".to_string(), SpendTokens::default())]
    );
}

#[test]
fn a5_settling_books_once_and_the_drop_after_it_books_nothing() {
    let (tap, mut rx) = tap();
    let guard = AttemptSpend::new(Some(&tap), SpendKind::Turn, "m");
    let observer = guard.observer();
    guard.observe(&reported(1, 1));
    guard.settle();
    // A late report through an observer can't re-arm the settled slot.
    observer.observe(&reported(9, 9));
    drop(observer);
    let got = booked(&mut rx);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].2.input, 1);
}

#[test]
fn a6_a_dropped_guard_books_what_it_retained_and_nothing_without_a_report() {
    let (tap, mut rx) = tap();
    {
        let guard = AttemptSpend::new(Some(&tap), SpendKind::Turn, "m");
        guard.observe(&reported(7, 2));
        // e.g. a cancel or an error return: no explicit settle.
    }
    {
        let _unreported = AttemptSpend::new(Some(&tap), SpendKind::Turn, "m");
    }
    AttemptSpend::new(Some(&tap), SpendKind::Turn, "m").settle();
    let got = booked(&mut rx);
    assert_eq!(got.len(), 1, "unknown spend books nothing, never a zero");
    assert_eq!((got[0].2.input, got[0].2.output), (7, 2));
}

#[test]
fn a7_an_observer_reports_before_settlement_and_never_settles() {
    let (tap, mut rx) = tap();
    let guard = AttemptSpend::new(Some(&tap), SpendKind::Turn, "m");
    let observer = guard.observer();
    let forwarder = std::thread::spawn(move || {
        observer.observe(&reported(3, 1));
        observer.observe(&reported(3, 5));
        drop(observer);
    });
    forwarder.join().unwrap();
    assert!(booked(&mut rx).is_empty(), "an observer's drop books nothing");
    guard.settle();
    let got = booked(&mut rx);
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].2.input, got[0].2.output), (3, 5));
}

#[test]
fn a8_a_summary_guard_books_its_kind_and_model() {
    let (tap, mut rx) = tap();
    let guard = AttemptSpend::new(Some(&tap), SpendKind::Summary, "claude-haiku-4-5");
    guard.observe(&reported(100, 20));
    drop(guard);
    let got = booked(&mut rx);
    assert_eq!(got[0].0, SpendKind::Summary);
    assert_eq!(got[0].1, "claude-haiku-4-5");
    // The tap books directly too (the summary settlement's path).
    assert_eq!(
        tap.book(SpendKind::Summary, "m", SpendTokens::default()),
        BookOutcome::Booked { seq: 1 }
    );
}

#[test]
fn a9_a_usage_object_states_only_the_counts_it_carries() {
    let full = ReportedUsage::from_anthropic_json(&json!({
        "input_tokens": 12, "output_tokens": 3,
        "cache_read_input_tokens": 4, "cache_creation_input_tokens": 5,
        "cache_creation": {"ephemeral_5m_input_tokens": 5, "ephemeral_1h_input_tokens": 0},
    }));
    assert_eq!(
        full,
        ReportedUsage {
            input: Some(12),
            output: Some(3),
            cache_read: Some(4),
            cache_creation: Some(5),
            cache_creation_5m: Some(5),
            cache_creation_1h: Some(0),
        }
    );
    assert_eq!(
        full.tokens(),
        SpendTokens {
            input: 12,
            output: 3,
            cache_read: 4,
            cache_creation: 5,
            cache_creation_5m: Some(5),
            cache_creation_1h: Some(0),
        }
    );
    let partial = ReportedUsage::from_anthropic_json(
        &json!({"output_tokens": 7, "input_tokens": null, "note": "x"}),
    );
    assert!(partial.has_counts());
    assert_eq!(partial.input, None);
    assert_eq!(partial.tokens().input, 0);
    assert_eq!(partial.tokens().cache_creation_5m, None);
    let none = ReportedUsage::from_anthropic_json(&json!({"input_tokens": -1, "output_tokens": "3"}));
    assert!(!none.has_counts(), "not unsigned integers: not stated");
    let split_only = ReportedUsage::from_anthropic_json(
        &json!({"cache_creation": {"ephemeral_5m_input_tokens": 9}}),
    );
    assert!(!split_only.has_counts(), "a TTL split alone is not a count");
}

/// Negative control: this passes for a guard that never publishes at all.
/// It shows only that a seal stops publication; that the same guard on a
/// live run publishes is a2's to show.
#[test]
fn a10_a_guard_on_a_sealed_run_publishes_nothing() {
    let (tap, mut rx) = tap();
    let guard = AttemptSpend::new(Some(&tap), SpendKind::Turn, "m");
    guard.observe(&reported(4, 4));
    assert!(tap.lease().seal());
    guard.settle();
    let got = drain(&mut rx);
    assert!(
        got.iter().all(|m| !matches!(m, SinkMessage::Spend { .. })),
        "late spend after a seal is not published: {got:?}"
    );
}

// P1g: the session's dedupe (SpendSeqs).

fn run(handle: &str, nonce: u64) -> RunIdentity {
    RunIdentity {
        handle: handle.to_string(),
        run_nonce: nonce,
        agent: "worker".to_string(),
        parent_handle: None,
    }
}

#[test]
fn q1_reports_in_order_are_each_booked() {
    let mut seqs = SpendSeqs::default();
    let a = run("sa_1", 7);
    for seq in 0..5 {
        assert_eq!(seqs.accept(&a, seq), SeqVerdict::Book, "seq {seq}");
    }
    assert_eq!(seqs.open_runs(), 1);
}

#[test]
fn q2_a_lower_seq_is_a_duplicate_and_changes_nothing() {
    let mut seqs = SpendSeqs::default();
    let a = run("sa_1", 7);
    assert_eq!(seqs.accept(&a, 0), SeqVerdict::Book);
    assert_eq!(seqs.accept(&a, 1), SeqVerdict::Book);
    assert_eq!(seqs.accept(&a, 0), SeqVerdict::Duplicate { expected: 2 });
    assert_eq!(seqs.accept(&a, 1), SeqVerdict::Duplicate { expected: 2 });
    // The duplicate moved nothing: the next one is still 2.
    assert_eq!(seqs.accept(&a, 2), SeqVerdict::Book);
}

#[test]
fn q3_a_higher_seq_is_a_gap_booked_and_resynced() {
    let mut seqs = SpendSeqs::default();
    let a = run("sa_1", 7);
    assert_eq!(seqs.accept(&a, 0), SeqVerdict::Book);
    assert_eq!(seqs.accept(&a, 3), SeqVerdict::Gap { expected: 1 });
    // Resynced to the one after the gap.
    assert_eq!(seqs.accept(&a, 4), SeqVerdict::Book);
    assert_eq!(seqs.accept(&a, 2), SeqVerdict::Duplicate { expected: 5 });
    // A first report that is not 0 is a gap too.
    let b = run("sa_2", 9);
    assert_eq!(seqs.accept(&b, 1), SeqVerdict::Gap { expected: 0 });
}

#[test]
fn q4_a_resume_is_a_separate_run_and_so_is_another_handle() {
    let mut seqs = SpendSeqs::default();
    let first = run("sa_1", 7);
    let resumed = run("sa_1", 8);
    let other = run("sa_2", 7);
    assert_eq!(seqs.accept(&first, 0), SeqVerdict::Book);
    assert_eq!(seqs.accept(&first, 1), SeqVerdict::Book);
    // Same handle, new nonce: starts at 0 again, not a duplicate.
    assert_eq!(seqs.accept(&resumed, 0), SeqVerdict::Book);
    // Same nonce, other handle: its own count.
    assert_eq!(seqs.accept(&other, 0), SeqVerdict::Book);
    assert_eq!(seqs.open_runs(), 3);
    // And a real lease's resume does get a new nonce.
    let (sink, _rx) = SpendSink::new();
    let one = sink.lease("sa_9", "w").unwrap();
    let two = sink.lease("sa_9", "w").unwrap();
    assert_ne!(one.identity().run_nonce, two.identity().run_nonce);
}

#[test]
fn q5_a_run_end_checks_the_count_and_forgets_the_run() {
    let mut seqs = SpendSeqs::default();
    let a = run("sa_1", 7);
    assert_eq!(seqs.accept(&a, 0), SeqVerdict::Book);
    assert_eq!(seqs.accept(&a, 1), SeqVerdict::Book);
    assert_eq!(seqs.end(&a, 2), Ok(()));
    assert_eq!(seqs.open_runs(), 0);
    // A run that published nothing ends cleanly at 0.
    let quiet = run("sa_2", 1);
    assert_eq!(seqs.end(&quiet, 0), Ok(()));
    // A count that differs reports what was accepted.
    let b = run("sa_3", 1);
    assert_eq!(seqs.accept(&b, 0), SeqVerdict::Book);
    assert_eq!(seqs.end(&b, 3), Err(1));
    assert_eq!(seqs.open_runs(), 0);
    // A run that never sent a report but says it published one.
    assert_eq!(seqs.end(&run("sa_4", 1), 1), Err(0));
}

// P1g2: a reload checkpoint's admissions reopen; an end's do not.

#[test]
fn z1_a_closing_sink_reopens_and_a_closed_one_stays_closed() {
    let (sink, mut rx) = SpendSink::new();
    assert_eq!(sink.begin_closing(), SinkPhase::Open);
    assert!(sink.lease("sa_1", "w").is_err(), "closing refuses runs");
    assert_eq!(sink.reopen(), SinkPhase::Closing);
    assert_eq!(sink.phase(), SinkPhase::Open);
    let lease = sink.lease("sa_1", "w").expect("open again");
    assert_eq!(lease.book(report(1)), BookOutcome::Booked { seq: 0 });
    assert_eq!(sink.reopen(), SinkPhase::Open, "open stays open");
    assert_eq!(sink.phase(), SinkPhase::Open);
    assert_eq!(sink.close(), SinkPhase::Open);
    assert_eq!(sink.reopen(), SinkPhase::Closed);
    assert_eq!(sink.phase(), SinkPhase::Closed, "a closed sink stays closed");
    assert!(sink.lease("sa_2", "w").is_err());
    assert_eq!(
        lease.book(report(2)),
        BookOutcome::NotBooked(NotBooked::SessionClosed)
    );
    let got = drain(&mut rx);
    assert_eq!(got.len(), 1, "{got:?}");
}
