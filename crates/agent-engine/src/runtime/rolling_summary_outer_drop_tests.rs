// Outer-drop tests, included into
// `rolling_summary::transport_tests::warm_tests::publication_tests::outer_drop_tests`
// (`docs/specs/subagent-context-rollover.md` §12 item 9). The real
// `prepare_rollover`, driven by a v2 worker's stream turn (its run's request
// ledger installed, on its run clock), seals the closing window and sends
// one rollover summary, warm or native cold, whose body an armed seam holds
// on the HTTP client's connection task. The test then drops the turn's own
// future: it aborts the turn task while the caller's stream stays alive, so
// the turn's token is never cancelled and no summary wrapper is dropped on
// its own. Then it releases the connection task. Held before publication,
// no body byte goes out, no successor head is checkpointed or committed,
// the window, head and binding stay as they were, nothing adopts a summary
// later, and the summary's reservation stays held as the existing policy
// holds any attempt dropped unsettled (no refund). Held after a won
// publication, the body stays sent. The seal is not asserted absent: it
// follows the recovery rules.
use crate::runtime::budget::{RunClock, TurnBudget, TurnRole};
use crate::tools::ledger_record::{Amount, RequestPurpose};
use crate::tools::{LedgerSlot, RequestLedgerHandle};

/// (window, head archive, binding epoch, durability blocked).
type ChainState = (u64, Option<String>, uuid::Uuid, bool);

fn chain_state(rt: &crate::Runtime) -> ChainState {
    let s = rt.continuation.lock().unwrap();
    (s.window, s.latest_archive.clone(), s.epoch(), s.durability_blocked)
}

/// What a turn dropped while its summary body was held left behind.
struct Dropped {
    /// What the summary's connection sent, once released.
    received: Vec<u8>,
    /// Successor heads the caller's stream carried (each acknowledged, as
    /// a frontend would).
    heads: usize,
    /// The chain before the turn, right after the drop, and once the body
    /// was released and had time to act.
    states: [ChainState; 3],
    /// The run ledger's rollover-summary entries at the end.
    summaries: Vec<Amount>,
}

/// Make `rt` a v2 worker run's runtime: on a run clock whose deadline is
/// far off, with its run's request ledger installed.
fn worker(rt: &mut crate::Runtime) -> RequestLedgerHandle {
    rt.set_turn_budget(TurnBudget {
        run_clock: Some(RunClock::started_at(
            tokio::time::Instant::now(),
            Some(Duration::from_secs(300)),
            None,
        )),
        ..TurnBudget::for_role(TurnRole::Worker)
    });
    let slot = LedgerSlot::default();
    rt.arm_request_ledger(slot.clone());
    assert!(slot.install(RequestLedgerHandle::new("claude-sonnet-4-6")));
    rt.request_ledger().expect("the run's ledger")
}

/// One turn of `messages` on `rt`, whose rollover summary body (`warm`, or
/// native cold) is held at `at` on its way to `listener`; then the turn's
/// own future is dropped and the body released.
async fn dropped_while_held(
    rt: &crate::Runtime,
    ledger: &RequestLedgerHandle,
    listener: &Arc<tokio::net::TcpListener>,
    messages: Vec<SharedMessage>,
    warm: bool,
    at: SeamPoint,
) -> Dropped {
    let case = format!("warm={warm} {at:?}");
    let logical_id = rt.continuation.lock().unwrap().logical_id.clone();
    let before = chain_state(rt);
    let (seam, SeamControl { reached, release }) = PublicationSeam::new(at);
    let _seam = crate::runtime::keepwarm::armed_seam::arm(&logical_id, warm, seam);
    let (_spawn, spawned) = crate::runtime::turn_task_seam::arm(&logical_id);
    let recorder = record_next(listener);
    let mut stream = rt
        .run_stream_with_messages(messages, Default::default(), None, None, false)
        .await;
    // The caller keeps its stream and reads it to its end.
    let events = tokio::spawn(async move {
        let mut heads = 0;
        while let Some(event) = stream.next().await {
            if let agent_core::StreamEvent::Session(
                agent_core::SessionEvent::ContextHeadCheckpoint { receipt, .. },
            ) = event
            {
                heads += 1;
                receipt.complete(Ok(()));
            }
        }
        heads
    });
    held(reached)
        .await
        .unwrap_or_else(|| panic!("{case}: the summary body was held"));
    let turn = spawned.try_recv().expect("the turn task's abort handle");
    assert!(spawned.try_recv().is_err(), "{case}: one turn task");
    turn.abort();
    let heads = tokio::time::timeout(Duration::from_secs(20), events)
        .await
        .unwrap_or_else(|_| panic!("{case}: the turn's events end once its future is dropped"))
        .expect("the event reader");
    let dropped = chain_state(rt);
    drop(release);
    let received = recorded(recorder).await;
    // Anything the drop left running would act by now.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after = chain_state(rt);
    let summaries = ledger
        .snapshot()
        .entries()
        .filter(|entry| entry.purpose == RequestPurpose::RolloverSummary)
        .map(|entry| entry.amount.clone())
        .collect();
    Dropped {
        received,
        heads,
        states: [before, dropped, after],
        summaries,
    }
}

/// Turn 2 over a warm endpoint's turn 1, with the thresholds lowered so it
/// rolls over at its first round and summarizes warm.
async fn warm_turn(at: SeamPoint) -> Dropped {
    let mut endpoint = warm_endpoint().await;
    let ledger = worker(&mut endpoint.rt);
    set_thresholds(&endpoint.rt, 60_000, 80_000);
    let mut next = endpoint.first.history.clone();
    next.push(msg(
        json!({"role":"user","content":"carry on in the next window"}),
    ));
    let dropped =
        dropped_while_held(&endpoint.rt, &ledger, &endpoint.listener, next, true, at).await;
    assert_eq!(endpoint.mock.warm_requests(), 0, "the summary never reached the mock");
    dropped
}

/// A turn over a history past the rollover threshold on a runtime with no
/// warm record: it rolls over at its first round and summarizes cold.
async fn cold_turn(at: SeamPoint) -> Dropped {
    let mut endpoint = cold_endpoint().await;
    let ledger = worker(&mut endpoint.rt);
    dropped_while_held(&endpoint.rt, &ledger, &endpoint.listener, history(None), false, at).await
}

/// Held before publication, the dropped turn left nothing behind but its
/// seal and its held reservation.
fn assert_nothing_went_out(dropped: &Dropped, case: &str) {
    assert_eq!(body_bytes(&dropped.received), 0, "{case}: no body byte");
    assert_eq!(dropped.heads, 0, "{case}: no successor head checkpoint");
    let [before, at_drop, after] = &dropped.states;
    assert_eq!(at_drop, before, "{case}: window, head and binding unchanged");
    assert_eq!(after, before, "{case}: no late commit or summary adoption");
    assert!(
        matches!(dropped.summaries.as_slice(), [Amount::Liability { .. }]),
        "{case}: one summary attempt, still held (no refund): {:?}",
        dropped.summaries
    );
}

/// od1: a warm summary body held right before publication when the turn's
/// own future is dropped is never produced (mutant: the warm attempt
/// detached from the turn's future).
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn od1_a_dropped_turn_never_sends_its_held_warm_summary_body() {
    let dropped = warm_turn(SeamPoint::BeforePublish).await;
    assert_nothing_went_out(&dropped, "warm");
}

/// od2: the same for a native cold summary body (mutant: the cold attempt
/// detached from the turn's future).
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn od2_a_dropped_turn_never_sends_its_held_cold_summary_body() {
    let dropped = cold_turn(SeamPoint::BeforePublish).await;
    assert_nothing_went_out(&dropped, "cold");
}

/// od3, the control: a body published before the drop, held right before
/// it is yielded, goes out once released; the dropped turn still commits
/// nothing, and the sent attempt stays held.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn od3_a_body_published_before_the_drop_stays_sent() {
    for warm in [true, false] {
        let dropped = if warm {
            warm_turn(SeamPoint::BeforeYield).await
        } else {
            cold_turn(SeamPoint::BeforeYield).await
        };
        assert!(body_bytes(&dropped.received) > 0, "warm={warm}: published first: sent");
        assert_eq!(dropped.heads, 0, "warm={warm}");
        let [before, at_drop, after] = &dropped.states;
        assert_eq!((at_drop, after), (before, before), "warm={warm}");
        assert!(
            matches!(dropped.summaries.as_slice(), [Amount::Liability { .. }]),
            "warm={warm}: {:?}",
            dropped.summaries
        );
    }
}
