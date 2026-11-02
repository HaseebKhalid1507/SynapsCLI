//! The reload's worker-record check (spec §5): the verdict table, the three
//! steps' filters, the settle wait, and the exact refusal and `--force`
//! texts.

use std::time::Duration;

use super::*;
use crate::tools::worker_records::{PendingOp, PublicationState, RunId};

fn entry(handle: &str, session: Option<&str>, committed: Option<u64>, state: PublicationState) -> WriterEntry {
    WriterEntry {
        run_id: RunId::random(),
        handle: handle.to_owned(),
        session: session.map(str::to_owned),
        committed,
        state,
    }
}

#[test]
fn the_verdict_table() {
    use PublicationState as P;
    use WriterVerdict as V;
    let cases: Vec<(Option<u64>, PublicationState, WriterVerdict)> = vec![
        // A launch deciding its spawn head.
        (None, P::NoHead, V::Starting),
        (None, P::Pending(PendingOp::Head(0)), V::Publishing),
        (None, P::PrePublicationFailure { committed: None, attempted: 0 }, V::Starting),
        (None, P::Uncertain { attempted: 0 }, V::Starting),
        (None, P::KeptAfterUncertain { committed: None }, V::Starting),
        // A live run with a committed head.
        (Some(2), P::Committed(2), V::Durable),
        (Some(2), P::Pending(PendingOp::Head(3)), V::Publishing),
        (Some(2), P::Pending(PendingOp::Republish(3)), V::Publishing),
        (Some(2), P::Pending(PendingOp::Report), V::Durable),
        (Some(2), P::PrePublicationFailure { committed: Some(2), attempted: 3 }, V::Durable),
        (Some(2), P::Uncertain { attempted: 3 }, V::Durable),
        (Some(2), P::KeptAfterUncertain { committed: Some(2) }, V::Durable),
        (Some(2), P::Unresolved, V::Unresolved),
        (None, P::Unresolved, V::Unresolved),
        // Finishing.
        (Some(2), P::Pending(PendingOp::Final), V::Publishing),
        (Some(2), P::FinalNotPublished, V::Durable),
        (Some(2), P::FinalUncertain, V::Durable),
        (Some(2), P::Final, V::Durable),
        // A durable final whose head removal is in flight passes.
        (Some(2), P::Pending(PendingOp::RemoveHead), V::Durable),
    ];
    for (committed, state, want) in cases {
        let e = entry("sa_1", None, committed, state.clone());
        assert_eq!(e.verdict(), want, "{committed:?} {state:?}");
    }
}

#[test]
fn reasons_and_codes() {
    assert_eq!(WriterVerdict::Durable.reason(), None);
    assert_eq!(
        WriterVerdict::Publishing.reason(),
        Some("a record write is still in flight")
    );
    assert_eq!(
        WriterVerdict::Starting.reason(),
        Some("its spawn record is not written yet")
    );
    assert_eq!(
        WriterVerdict::Unresolved.reason(),
        Some("no durable record: its publication is unresolved")
    );
    let codes: Vec<_> = [
        WriterVerdict::Durable,
        WriterVerdict::Publishing,
        WriterVerdict::Starting,
        WriterVerdict::Unresolved,
    ]
    .iter()
    .map(|v| v.code())
    .collect();
    assert_eq!(codes, ["durable", "publishing", "starting", "unresolved"]);
}

#[test]
fn each_step_takes_its_own_runs() {
    let entries = vec![
        entry("sa_1", None, Some(1), PublicationState::Committed(1)),
        entry("sa_2", None, Some(1), PublicationState::Pending(PendingOp::Head(2))),
        entry("sa_3", None, None, PublicationState::NoHead),
        entry("sa_4", None, Some(1), PublicationState::Unresolved),
    ];
    let unresolved: Vec<_> = unresolved(&entries).into_iter().map(|u| u.worker).collect();
    assert_eq!(unresolved, ["sa_4"], "step 1 refuses only an unresolved record");
    assert_eq!(writing(&entries), 2, "step 2: a write in flight and a launch are busy");
    let left: Vec<_> = unpreserved(&entries).into_iter().map(|u| u.worker).collect();
    assert_eq!(left, ["sa_2", "sa_3", "sa_4"], "step 3 names every run not durable");
}

#[test]
fn the_refusal_text() {
    let runs = vec![
        Unpreserved {
            worker: "sa_3".into(),
            session: Some("s-1".into()),
            verdict: WriterVerdict::Publishing,
        },
        Unpreserved {
            worker: "sa_4".into(),
            session: None,
            verdict: WriterVerdict::Unresolved,
        },
    ];
    assert_eq!(
        refusal(&runs),
        "worker records are not durable: sa_3 (session s-1, a record write is still in flight), \
         sa_4 (no durable record: its publication is unresolved); the daemon keeps running this \
         image; no worker was stopped and any paused turn resumes in place; retry, or reload \
         with --force to go ahead without them"
    );
}

#[test]
fn a_long_list_names_eight_and_counts_the_rest() {
    let runs: Vec<_> = (1..=11)
        .map(|n| Unpreserved {
            worker: format!("sa_{n}"),
            session: None,
            verdict: WriterVerdict::Starting,
        })
        .collect();
    let text = refusal(&runs);
    assert!(text.contains("sa_8 (its spawn record is not written yet), and 3 more;"), "{text}");
    assert!(!text.contains("sa_9 "), "{text}");
}

#[test]
fn the_forced_line() {
    assert_eq!(forced_line(&[]), None, "nothing waived, no line");
    let waived: Vec<WaivedWorker> = [
        ("sa_3", Some("s-1"), WriterVerdict::Publishing),
        ("sa_4", None, WriterVerdict::Starting),
        ("sa_5", None, WriterVerdict::Unresolved),
    ]
    .into_iter()
    .map(|(worker, session, verdict)| {
        Unpreserved {
            worker: worker.into(),
            session: session.map(Into::into),
            verdict,
        }
        .waived()
    })
    .collect();
    assert_eq!(
        waived[0],
        WaivedWorker {
            worker: "sa_3".into(),
            session: Some("s-1".into()),
            why: "publishing".into()
        }
    );
    assert_eq!(
        forced_line(&waived).unwrap(),
        "reload forced: worker records not preserved: sa_3 (session s-1, a record write was \
         still in flight), sa_4 (its spawn record was not written yet), sa_5 (no durable \
         record: its publication was unresolved)"
    );
}

/// A shared table a test changes while `settle` reads it.
fn table(entries: Vec<WriterEntry>) -> std::sync::Arc<std::sync::Mutex<Vec<WriterEntry>>> {
    std::sync::Arc::new(std::sync::Mutex::new(entries))
}

#[tokio::test(start_paused = true)]
async fn settle_waits_for_a_write_that_lands_in_time() {
    let t = table(vec![entry(
        "sa_1",
        None,
        Some(1),
        PublicationState::Pending(PendingOp::Head(2)),
    )]);
    let writer = std::sync::Arc::clone(&t);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        writer.lock().unwrap()[0].state = PublicationState::Committed(2);
        writer.lock().unwrap()[0].committed = Some(2);
    });
    let t0 = tokio::time::Instant::now();
    let left = settle(WORKER_RECORD_WAIT, || t.lock().unwrap().clone()).await;
    assert!(left.is_empty(), "{left:?}");
    let took = t0.elapsed();
    assert!(took >= Duration::from_secs(2) && took < Duration::from_millis(2100), "{took:?}");
}

#[tokio::test(start_paused = true)]
async fn settle_gives_a_stuck_write_the_whole_wait_and_names_it() {
    let t = table(vec![
        entry("sa_1", Some("s-1"), Some(1), PublicationState::Pending(PendingOp::Final)),
        entry("sa_2", None, Some(1), PublicationState::Committed(1)),
    ]);
    let t0 = tokio::time::Instant::now();
    let left = settle(WORKER_RECORD_WAIT, || t.lock().unwrap().clone()).await;
    assert_eq!(t0.elapsed(), WORKER_RECORD_WAIT);
    assert_eq!(
        left,
        vec![Unpreserved {
            worker: "sa_1".into(),
            session: Some("s-1".into()),
            verdict: WriterVerdict::Publishing,
        }]
    );
}

#[tokio::test(start_paused = true)]
async fn settle_does_not_wait_for_an_unresolved_record() {
    let t = table(vec![entry("sa_1", None, Some(1), PublicationState::Unresolved)]);
    let t0 = tokio::time::Instant::now();
    let left = settle(WORKER_RECORD_WAIT, || t.lock().unwrap().clone()).await;
    assert_eq!(t0.elapsed(), Duration::ZERO, "nothing is coming for it");
    assert_eq!(left.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn settle_with_every_record_durable_answers_at_once() {
    let t = table(vec![
        entry("sa_1", None, Some(1), PublicationState::Committed(1)),
        entry("sa_2", None, Some(3), PublicationState::Final),
    ]);
    let t0 = tokio::time::Instant::now();
    assert!(settle(WORKER_RECORD_WAIT, || t.lock().unwrap().clone()).await.is_empty());
    assert_eq!(t0.elapsed(), Duration::ZERO);
}

#[test]
fn the_wait_is_five_seconds() {
    assert_eq!(WORKER_RECORD_WAIT, Duration::from_secs(5));
}
