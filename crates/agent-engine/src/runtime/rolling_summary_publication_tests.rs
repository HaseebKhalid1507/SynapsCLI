// Summary body publication tests, included into
// `rolling_summary::transport_tests::warm_tests::publication_tests`: a
// summary body that the HTTP client's connection task still holds after its
// caller gave up (the call timed out, or was dropped unfinished) is never
// produced, and on a worker run's clock no body is published at or after
// the run deadline. Each summary connection goes to a loopback listener
// that records what it sends and never answers; a seam holds the body stream
// inside its poll, on the connection task, while the caller gives up or the
// deadline passes.
use crate::runtime::budget::UsageCounters;
use crate::runtime::keepwarm::{PublicationSeam, SeamControl, SeamPoint};
use crate::runtime::rolling_summary::{
    AttemptWait, RolloverSummarizer, Spend, Summarized, SummaryPath, SummaryRequest,
};

/// What the listener's next connection sends: read until it closes, its
/// whole body (by `Content-Length`) has arrived, or 10 s pass in silence.
fn record_next(listener: &Arc<tokio::net::TcpListener>) -> tokio::task::JoinHandle<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let listener = Arc::clone(listener);
    tokio::spawn(async move {
        let (mut inbound, _) = listener.accept().await.expect("the summary connects");
        let mut received = Vec::new();
        let mut buf = [0u8; 8192];
        while !whole_request(&received) {
            match tokio::time::timeout(Duration::from_secs(10), inbound.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => received.extend_from_slice(&buf[..n]),
                _ => break,
            }
        }
        received
    })
}

/// Whether `received` holds a request head and the whole body it declares.
fn whole_request(received: &[u8]) -> bool {
    let Some(head) = received.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    String::from_utf8_lossy(&received[..head])
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if !name.trim().eq_ignore_ascii_case("content-length") {
                return None;
            }
            value.trim().parse::<usize>().ok()
        })
        .is_some_and(|declared| received.len() - head - 4 >= declared)
}

/// The listener's recording, once its connection ended.
async fn recorded(recorder: tokio::task::JoinHandle<Vec<u8>>) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(30), recorder)
        .await
        .expect("the summary connection ends")
        .expect("the recorder")
}

/// Waits, off the runtime's threads, until the seam holds the body stream:
/// the instant it did, or `None` after 10 s.
async fn held(reached: std::sync::mpsc::Receiver<()>) -> Option<Instant> {
    tokio::task::spawn_blocking(move || {
        reached
            .recv_timeout(Duration::from_secs(10))
            .ok()
            .map(|()| Instant::now())
    })
    .await
    .expect("the seam waiter")
}

/// A runtime whose summary requests go to `listener`.
struct ColdEndpoint {
    rt: crate::Runtime,
    listener: Arc<tokio::net::TcpListener>,
    _url: crate::test_env::EnvVarGuard,
    _base: crate::test_env::BaseDirGuard,
}

async fn cold_endpoint() -> ColdEndpoint {
    let base = crate::test_env::BaseDirGuard::new();
    let listener = Arc::new(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap());
    let url = crate::test_env::EnvVarGuard::set(
        "SYNAPS_ANTHROPIC_BASE_URL",
        &format!("http://{}", listener.local_addr().unwrap()),
    );
    ColdEndpoint {
        rt: runtime(Some(SUMMARY)),
        listener,
        _url: url,
        _base: base,
    }
}

/// A runtime whose turn 1 went through `listener` to the warm mock, so it
/// holds a warm record. Forwarding has stopped since: the summary's
/// connection, warm or cold, is the listener's next one.
struct WarmEndpoint {
    rt: crate::Runtime,
    first: Turn,
    record: Arc<ws::RequestRecord>,
    listener: Arc<tokio::net::TcpListener>,
    mock: WarmMock,
    _proxy: crate::test_env::EnvVarGuard,
    _mock: crate::test_env::EnvVarGuard,
    _base: crate::test_env::BaseDirGuard,
}

async fn warm_endpoint() -> WarmEndpoint {
    let base = crate::test_env::BaseDirGuard::new();
    let (mock, mock_url) = spawn_warm().await;
    let upstream = std::env::var("SYNAPS_ANTHROPIC_BASE_URL")
        .unwrap()
        .trim_start_matches("http://")
        .to_owned();
    let listener = Arc::new(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap());
    let addr = listener.local_addr().unwrap();
    let forwarding = {
        let listener = Arc::clone(&listener);
        tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            while let Ok((mut inbound, _)) = listener.accept().await {
                let upstream = upstream.clone();
                connections.spawn(async move {
                    if let Ok(mut outbound) = tokio::net::TcpStream::connect(&upstream).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                });
            }
        })
    };
    let proxy =
        crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &format!("http://{addr}"));
    let rt = runtime(Some(SUMMARY));
    let (first, record) = warmed(&rt).await;
    forwarding.abort();
    let _ = forwarding.await;
    WarmEndpoint {
        rt,
        first,
        record,
        listener,
        mock,
        _proxy: proxy,
        _mock: mock_url,
        _base: base,
    }
}

/// `warm` with its body stream held at `seam`.
fn warm_held(mut warm: ws::WarmSummarizer, seam: &Arc<PublicationSeam>) -> ws::WarmSummarizer {
    let seam = Arc::clone(seam);
    let inner = Arc::clone(&warm.call);
    warm.call = Arc::new(move |record, send: ws::WarmSend, cancel| {
        send.published.install_seam(Arc::clone(&seam));
        inner(record, send, cancel)
    });
    warm
}

/// One rollover summary through `summarize()`: of `messages[1]` on the
/// warm path when `warm` is given, else of `messages[..4]` on the screened
/// cold path, authorized by `snapshot`.
#[allow(clippy::too_many_arguments)]
async fn one_summary(
    summarizer: &RolloverSummarizer,
    state: &crate::runtime::continuation::SharedContinuation,
    warm: Option<&ws::WarmSummarizer>,
    messages: &[SharedMessage],
    snapshot: ws::SnapshotGate,
    timeout: Duration,
    run_deadline: Option<tokio::time::Instant>,
    usage: &UsageCounters,
) -> Summarized {
    let cancel = tokio_util::sync::CancellationToken::new();
    one_summary_until_cancelled(
        summarizer,
        state,
        warm,
        messages,
        snapshot,
        timeout,
        run_deadline,
        usage,
        &cancel,
    )
    .await
}

/// [`one_summary`] stopped by `cancel`.
#[allow(clippy::too_many_arguments)]
async fn one_summary_until_cancelled(
    summarizer: &RolloverSummarizer,
    state: &crate::runtime::continuation::SharedContinuation,
    warm: Option<&ws::WarmSummarizer>,
    messages: &[SharedMessage],
    snapshot: ws::SnapshotGate,
    timeout: Duration,
    run_deadline: Option<tokio::time::Instant>,
    usage: &UsageCounters,
    cancel: &tokio_util::sync::CancellationToken,
) -> Summarized {
    let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
    let archive = "a".repeat(32);
    let indices = [1usize];
    let dropped = if warm.is_some() {
        vec![messages[1].clone()]
    } else {
        messages[..4].to_vec()
    };
    let logical_id = state.lock().unwrap().logical_id.clone();
    crate::runtime::rolling_summary::summarize(
        summarizer,
        crate::runtime::rolling_summary::SummaryRequest {
            logical_id: &logical_id,
            previous: None,
            dropped: &dropped,
            window: 1,
            archive_id: &archive,
            cap_tokens: SUMMARY,
            timeout,
            cancel,
            accounting: Some(crate::runtime::rolling_summary::SummaryAccounting {
                usage,
                max_cost_usd: None,
                events: &events,
                tap: None,
                ledger: None,
            }),
            snapshot: Some(snapshot),
            run_deadline,
        },
        warm.map(|warm| ws::WarmContext {
            warm,
            state,
            messages,
            dropped: &indices,
            newest_run_start: None,
        }),
    )
    .await
}

/// A session's native cold summary times out while the connection task
/// holds its body right before publication: the call is dropped with the
/// timeout, and the body it held is never produced. (The caller still
/// keeps the reservation: its spend is unknown to it.)
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_cold_summary_that_timed_out_never_publishes_its_held_body() {
    let endpoint = cold_endpoint().await;
    let rt = &endpoint.rt;
    let summarizer = rt.rollover_summarizer().expect("summarizer");
    let (seam, SeamControl { reached, release }) = PublicationSeam::new(SeamPoint::BeforePublish);
    let recorder = record_next(&endpoint.listener);
    let usage = UsageCounters::default();
    let messages = history(None);
    let ((out, done_at), held_at) = tokio::join!(
        async {
            let out = one_summary(
                &summarizer,
                &rt.continuation,
                None,
                &messages,
                ws::SnapshotGate::new(&rt.continuation).with_seam(Arc::clone(&seam)),
                Duration::from_secs(2),
                None,
                &usage,
            )
            .await;
            (out, Instant::now())
        },
        held(reached),
    );
    let held_at = held_at.expect("the body poll was held");
    assert!(held_at < done_at, "held before the timeout fired");
    drop(release);
    let received = recorded(recorder).await;
    assert!(matches!(out.text, Err(None)), "{:?}", out.text);
    assert_eq!(out.report.path, SummaryPath::Cold);
    assert_eq!(out.report.terminal, Some("timeout"));
    assert_eq!(body_bytes(&received), 0, "no body byte after the call was dropped");
}

/// A native cold summary cancelled while the connection task holds its body
/// right before publication: the call stops at the cancel (reported as
/// canceled on the cold path, well before its 20 s timeout, so a cancel is
/// never mistaken for an eventual timeout), and the body it held is never
/// produced.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_cancelled_cold_summary_never_publishes_its_held_body() {
    let endpoint = cold_endpoint().await;
    let rt = &endpoint.rt;
    let summarizer = rt.rollover_summarizer().expect("summarizer");
    let (seam, SeamControl { reached, release }) = PublicationSeam::new(SeamPoint::BeforePublish);
    let recorder = record_next(&endpoint.listener);
    let usage = UsageCounters::default();
    let messages = history(None);
    let cancel = tokio_util::sync::CancellationToken::new();
    let started = std::time::Instant::now();
    let (out, held_at) = tokio::join!(
        one_summary_until_cancelled(
            &summarizer,
            &rt.continuation,
            None,
            &messages,
            ws::SnapshotGate::new(&rt.continuation).with_seam(Arc::clone(&seam)),
            Duration::from_secs(20),
            None,
            &usage,
            &cancel,
        ),
        async {
            let at = held(reached).await;
            cancel.cancel();
            at
        },
    );
    let elapsed = started.elapsed();
    assert!(held_at.is_some(), "cancelled while its body poll was held");
    drop(release);
    let received = recorded(recorder).await;
    assert!(out.text.is_err(), "{:?}", out.text);
    assert_eq!(out.report.path, SummaryPath::Cold);
    assert_eq!(out.report.terminal, Some("canceled"));
    assert!(
        elapsed < Duration::from_secs(10),
        "ended at the cancel, not the 20 s timeout: {elapsed:?}"
    );
    assert_eq!(body_bytes(&received), 0, "no body byte after the cancel");
}

/// A summary future dropped unfinished (its rollover was abandoned) while
/// the connection task holds its body right before publication: warm and
/// native cold alike, the body is never produced.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_dropped_summary_never_publishes_its_held_body() {
    for warm in [true, false] {
        let (seam, SeamControl { reached, release }) =
            PublicationSeam::new(SeamPoint::BeforePublish);
        let usage = UsageCounters::default();
        let received = if warm {
            let endpoint = warm_endpoint().await;
            let rt = &endpoint.rt;
            let summarizer = rt.rollover_summarizer().expect("summarizer");
            let warm_call = warm_held(rt.warm_summarizer(), &seam);
            let recorder = record_next(&endpoint.listener);
            let summary = one_summary(
                &summarizer,
                &rt.continuation,
                Some(&warm_call),
                &endpoint.first.history,
                ws::SnapshotGate::new(&rt.continuation),
                Duration::from_secs(20),
                None,
                &usage,
            );
            let dropped = tokio::select! {
                _ = summary => false,
                at = held(reached) => at.is_some(),
            };
            assert!(dropped, "warm: dropped while its body poll was held");
            drop(release);
            let received = recorded(recorder).await;
            assert_eq!(endpoint.mock.warm_requests(), 0, "the summary never reached the mock");
            received
        } else {
            let endpoint = cold_endpoint().await;
            let rt = &endpoint.rt;
            let summarizer = rt.rollover_summarizer().expect("summarizer");
            let recorder = record_next(&endpoint.listener);
            let messages = history(None);
            let summary = one_summary(
                &summarizer,
                &rt.continuation,
                None,
                &messages,
                ws::SnapshotGate::new(&rt.continuation).with_seam(Arc::clone(&seam)),
                Duration::from_secs(20),
                None,
                &usage,
            );
            let dropped = tokio::select! {
                _ = summary => false,
                at = held(reached) => at.is_some(),
            };
            assert!(dropped, "cold: dropped while its body poll was held");
            drop(release);
            recorded(recorder).await
        };
        assert_eq!(body_bytes(&received), 0, "warm={warm}: no body byte after the drop");
    }
}

/// D (§5): an attempt waits for its reply until the earlier of the run
/// deadline and its summary timeout from its own start, both absolute;
/// none starts once the run deadline has passed. Without a run clock it
/// waits its summary timeout.
#[tokio::test(start_paused = true)]
async fn an_attempt_waits_until_the_earlier_of_its_timeout_and_the_run_deadline() {
    let cancel = tokio_util::sync::CancellationToken::new();
    let now = tokio::time::Instant::now();
    let s = Duration::from_secs;
    let wait = |timeout: Duration, run_deadline: Option<tokio::time::Instant>| {
        SummaryRequest {
            logical_id: "chain",
            previous: None,
            dropped: &[],
            window: 1,
            archive_id: "archive",
            cap_tokens: 1,
            timeout,
            cancel: &cancel,
            accounting: None,
            snapshot: None,
            run_deadline,
        }
        .attempt_wait()
    };
    assert_eq!(wait(s(60), None), Some(AttemptWait::For(s(60))));
    assert_eq!(wait(s(60), Some(now + s(10))), Some(AttemptWait::Until(now + s(10))));
    assert_eq!(wait(s(5), Some(now + s(60))), Some(AttemptWait::Until(now + s(5))));
    assert_eq!(wait(Duration::MAX, Some(now + s(1))), Some(AttemptWait::Until(now + s(1))));
    assert_eq!(wait(s(60), Some(now)), None, "at the run deadline");
    tokio::time::advance(s(2)).await;
    assert_eq!(wait(s(60), Some(now + s(1))), None, "past it");
}

/// D (§5): the body gate closes at the run deadline; the snapshot itself
/// stays valid (a reply published before the deadline is still used).
#[tokio::test(start_paused = true)]
async fn a_body_gate_closes_at_the_run_deadline() {
    let rt = runtime(Some(SUMMARY));
    let now = tokio::time::Instant::now();
    let snapshot = ws::SnapshotGate::new(&rt.continuation);
    assert!(ws::BodyGate::Snapshot(snapshot.clone()).open(), "no run deadline");
    let gated = snapshot
        .clone()
        .with_deadline(Some(now + Duration::from_secs(1)));
    assert!(ws::BodyGate::Snapshot(gated.clone()).open(), "before the deadline");
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(!ws::BodyGate::Snapshot(gated.clone()).open(), "at the deadline");
    assert!(gated.valid(), "the snapshot is still valid");
    assert!(ws::BodyGate::Snapshot(snapshot).open(), "no run deadline: still open");
}

/// D (§5): the clock is the last check before publication. A body gate
/// whose validity check waits on the continuation lock while the run
/// deadline passes stays closed once the lock is released, for the
/// screened snapshot and for the warm lease. The lock is held from before
/// the check starts until after the deadline, so the check's validity step
/// spans the expiry. The start signal and `!is_finished()` do not prove the
/// checker had reached the lock before the deadline (a checker scheduled
/// late reads the clock after it whatever the order), so this test cannot
/// fail on the fixed order and can, rarely, pass on the old one; reverting
/// the order in either gate was shown red.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_gate_whose_validity_waits_past_the_run_deadline_stays_closed() {
    let endpoint = warm_endpoint().await;
    let rt = &endpoint.rt;
    /// A body gate for a given run deadline.
    type GateAt<'a> = Box<dyn Fn(tokio::time::Instant) -> ws::BodyGate + 'a>;
    let gates: [(&str, GateAt<'_>); 2] = [
        (
            "snapshot",
            Box::new(|deadline| {
                ws::BodyGate::Snapshot(
                    ws::SnapshotGate::new(&rt.continuation).with_deadline(Some(deadline)),
                )
            }),
        ),
        (
            "warm lease",
            Box::new(|deadline| {
                ws::BodyGate::Lease(
                    ws::WarmLease::new(
                        rt.continuation.clone(),
                        Arc::clone(&endpoint.record),
                        Arc::clone(&rt.tools),
                    )
                    .with_deadline(Some(deadline)),
                )
            }),
        ),
    ];
    for (kind, gate) in gates {
        let far = tokio::time::Instant::now() + Duration::from_secs(3600);
        assert!(gate(far).open(), "{kind}: valid and in time");
        let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
        let gate = gate(deadline);
        let held = rt.continuation.lock().unwrap();
        let (started, checking) = std::sync::mpsc::channel();
        let check = std::thread::spawn(move || {
            started.send(()).unwrap();
            gate.open()
        });
        checking
            .recv_timeout(Duration::from_secs(10))
            .expect("the check started");
        while tokio::time::Instant::now() < deadline + Duration::from_millis(50) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !check.is_finished(),
            "{kind}: the check waits on the continuation lock past the deadline"
        );
        drop(held);
        assert!(
            !check.join().unwrap(),
            "{kind}: a gate validated after the deadline stays closed"
        );
    }
}

/// D (§5): a warm body held right before publication while the run
/// deadline passes is refused there, though its caller still waits (as
/// when a stalled executor polls the transport before the caller's timer):
/// once released, no body byte goes out and nothing counts as sent.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_warm_body_held_past_the_run_deadline_is_never_published() {
    let endpoint = warm_endpoint().await;
    let (seam, SeamControl { reached, release }) = PublicationSeam::new(SeamPoint::BeforePublish);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut send = send_for(&endpoint.record);
    send.run_deadline = Some(deadline);
    send.published.install_seam(seam);
    let recorder = record_next(&endpoint.listener);
    let caller = endpoint.rt.clone();
    let record = Arc::clone(&endpoint.record);
    let call =
        tokio::spawn(async move { caller.warm_summary_call(record, send, Default::default()).await });
    let held_at = held(reached).await.expect("the body poll was held");
    assert!(
        tokio::time::Instant::from_std(held_at) < deadline,
        "held before the run deadline"
    );
    tokio::time::sleep_until(deadline + Duration::from_millis(100)).await;
    assert!(!call.is_finished(), "the call still waits for its body");
    drop(release);
    let reply = tokio::time::timeout(Duration::from_secs(20), call)
        .await
        .expect("the warm call ends")
        .unwrap();
    let received = recorded(recorder).await;
    assert!(
        matches!(reply.terminal, ws::WarmTerminal::NotSent(_)),
        "{:?}",
        reply.terminal
    );
    assert!(!reply.dispatched && reply.usage.is_none());
    assert_eq!(body_bytes(&received), 0, "no body byte at or after the deadline");
    assert_eq!(endpoint.mock.warm_requests(), 0);
}

/// The same for the native cold transport: its snapshot gate carries the
/// run deadline to the body, refused there while the caller still waits;
/// the call reports nothing sent.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_cold_body_held_past_the_run_deadline_is_never_published() {
    let endpoint = cold_endpoint().await;
    let rt = &endpoint.rt;
    let summarizer = rt.rollover_summarizer().expect("summarizer");
    let (seam, SeamControl { reached, release }) = PublicationSeam::new(SeamPoint::BeforePublish);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let gate = ws::SnapshotGate::new(&rt.continuation)
        .with_seam(seam)
        .with_deadline(Some(deadline));
    let recorder = record_next(&endpoint.listener);
    let message = msg(json!({"role":"user","content":"summarize the dropped history"}));
    let call = tokio::spawn((summarizer.call)(vec![message], 1_000, Some(gate)));
    let held_at = held(reached).await.expect("the body poll was held");
    assert!(
        tokio::time::Instant::from_std(held_at) < deadline,
        "held before the run deadline"
    );
    tokio::time::sleep_until(deadline + Duration::from_millis(100)).await;
    assert!(!call.is_finished(), "the call still waits for its body");
    drop(release);
    let reply = tokio::time::timeout(Duration::from_secs(20), call)
        .await
        .expect("the cold call ends")
        .unwrap();
    let received = recorded(recorder).await;
    assert!(reply.text.is_err(), "{:?}", reply.text);
    assert_eq!(reply.spend, Spend::NotDispatched, "nothing was sent");
    assert_eq!(body_bytes(&received), 0, "no body byte at or after the deadline");
}

/// Through `summarize()` on a run clock: warm and native cold bodies held
/// right before publication when the run deadline passes. The attempt
/// gives up at the deadline (not at its 20 s timeout) and, once the
/// connection task is released, no body byte goes out.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn held_bodies_send_nothing_once_the_run_deadline_passes() {
    for warm in [true, false] {
        let (out, received, waited) =
            held_through_the_deadline(warm, SeamPoint::BeforePublish, false).await;
        assert!(waited < Duration::from_secs(10), "warm={warm}: {waited:?}");
        assert!(matches!(out.text, Err(None)), "warm={warm}: {:?}", out.text);
        assert_eq!(out.report.terminal, Some("timeout"), "warm={warm}");
        if warm {
            assert_eq!(out.report.path, SummaryPath::None, "withdrawn: not sent");
            assert_eq!(out.report.id, None);
        } else {
            assert_eq!(out.report.path, SummaryPath::Cold);
        }
        assert_eq!(body_bytes(&received), 0, "warm={warm}: no body byte");
    }
}

/// A body whose publication won before the run deadline stays sent: held
/// after the publication, before it is yielded, while the deadline passes,
/// it goes out once released, and a warm attempt is reported as sent.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_body_published_before_the_run_deadline_stays_sent() {
    for warm in [true, false] {
        let (out, received, _) = held_through_the_deadline(warm, SeamPoint::BeforeYield, false).await;
        assert_eq!(out.report.terminal, Some("timeout"), "warm={warm}");
        if warm {
            assert_eq!(out.report.path, SummaryPath::Warm, "published first: sent");
            assert!(out.report.id.is_some(), "a sent request keeps its id");
        } else {
            assert_eq!(out.report.path, SummaryPath::Cold);
        }
        assert!(body_bytes(&received) > 0, "warm={warm}: the published body went out");
    }
}

/// A cold attempt after an unsent warm one (the warm transport sent
/// nothing) keeps the run deadline: its body, held right before
/// publication when the deadline passes, never goes out.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(synaps_base_dir, stream_relay)]
async fn a_cold_attempt_after_an_unsent_warm_one_keeps_the_run_deadline() {
    let (out, received, waited) =
        held_through_the_deadline(false, SeamPoint::BeforePublish, true).await;
    assert!(waited < Duration::from_secs(10), "{waited:?}");
    assert_eq!(out.report.warm_miss, Some(ws::WarmMiss::PreSend), "the warm attempt");
    assert_eq!(out.report.path, SummaryPath::Cold);
    assert_eq!(out.report.terminal, Some("timeout"));
    assert_eq!(body_bytes(&received), 0, "no cold body byte");
}

/// One summary through `summarize()` on a run deadline 2 s ahead (summary
/// timeout 20 s), its body held at `at` until the summary has given up:
/// the summary, what the listener received once the body was released,
/// and how long the summary waited. `warm` sends it warm; `after_warm`
/// sends it cold after a warm attempt that sent nothing.
async fn held_through_the_deadline(
    warm: bool,
    at: SeamPoint,
    after_warm: bool,
) -> (Summarized, Vec<u8>, Duration) {
    let (seam, SeamControl { reached, release }) = PublicationSeam::new(at);
    let usage = UsageCounters::default();
    let run = |summarizer: RolloverSummarizer,
               state: crate::runtime::continuation::SharedContinuation,
               warm: Option<ws::WarmSummarizer>,
               messages: Vec<SharedMessage>,
               snapshot: ws::SnapshotGate| {
        let usage = &usage;
        async move {
            let started = tokio::time::Instant::now();
            let deadline = started + Duration::from_secs(2);
            let (out, held_at) = tokio::join!(
                one_summary(
                    &summarizer,
                    &state,
                    warm.as_ref(),
                    &messages,
                    snapshot,
                    Duration::from_secs(20),
                    Some(deadline),
                    usage,
                ),
                held(reached),
            );
            let waited = started.elapsed();
            let held_at = held_at.expect("the body poll was held");
            assert!(
                tokio::time::Instant::from_std(held_at) < deadline,
                "held before the run deadline"
            );
            (out, waited)
        }
    };
    if warm || after_warm {
        let endpoint = warm_endpoint().await;
        let rt = &endpoint.rt;
        let summarizer = rt.rollover_summarizer().expect("summarizer");
        let mut warm_call = rt.warm_summarizer();
        let snapshot = ws::SnapshotGate::new(&rt.continuation);
        let snapshot = if after_warm {
            warm_call.call = Arc::new(|_, _, _| {
                Box::pin(async {
                    ws::WarmReply {
                        terminal: ws::WarmTerminal::NotSent(ws::WarmMiss::PreSend),
                        usage: None,
                        dispatched: false,
                        failure: None,
                        stop: None,
                    }
                })
            });
            snapshot.with_seam(Arc::clone(&seam))
        } else {
            warm_call = warm_held(warm_call, &seam);
            snapshot
        };
        let recorder = record_next(&endpoint.listener);
        let (out, waited) = run(
            summarizer,
            rt.continuation.clone(),
            Some(warm_call),
            endpoint.first.history.clone(),
            snapshot,
        )
        .await;
        drop(release);
        let received = recorded(recorder).await;
        if at == SeamPoint::BeforePublish {
            assert_eq!(endpoint.mock.warm_requests(), 0, "nothing reached the mock");
        }
        (out, received, waited)
    } else {
        let endpoint = cold_endpoint().await;
        let rt = &endpoint.rt;
        let summarizer = rt.rollover_summarizer().expect("summarizer");
        let recorder = record_next(&endpoint.listener);
        let (out, waited) = run(
            summarizer,
            rt.continuation.clone(),
            None,
            history(None),
            ws::SnapshotGate::new(&rt.continuation).with_seam(Arc::clone(&seam)),
        )
        .await;
        drop(release);
        (out, recorded(recorder).await, waited)
    }
}
