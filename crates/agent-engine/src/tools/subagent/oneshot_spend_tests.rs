// Child spend (P1f), included in the oneshot gate as `mod spend`. A oneshot
// in a host with a sink books each of its worker's requests through the
// run's tap as the request ends, and sends its parent no aggregate Usage.
// Everything else it sends the provider, gives its parent, returns and logs
// is what the gate's fixtures pin (the gate itself runs with no sink, so a
// host without one keeps the aggregate). The run's lease ends once, when
// its worker is done.

use std::time::{Duration, Instant};

use crate::runtime::child_spend::{
    RunLease, SinkMessage, SpendKind, SpendParent, SpendReceiver, SpendSink, SpendTokens,
};
use crate::StreamEvent;

/// What one request of the gate's stand-in reports (its `usage()`), at
/// its start and at its end alike.
fn reported() -> SpendTokens {
    SpendTokens {
        input: 12,
        output: 3,
        cache_read: 4,
        cache_creation: 5,
        cache_creation_5m: Some(5),
        cache_creation_1h: Some(0),
    }
}

fn fixture(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(ONESHOT_GATE_DIR)
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// A gate fixture of events without its Usage line.
fn without_usage(events: &str) -> String {
    events
        .lines()
        .filter(|line| !line.starts_with("Session(Usage {"))
        .map(|line| format!("{line}\n"))
        .collect()
}

/// The aggregate the gate's fixture sends: input, output, cache read,
/// cache write.
fn aggregate(events: &str) -> (u64, u64, u64, u64) {
    let usage = regex::Regex::new(
        r"input_tokens: (\d+), output_tokens: (\d+), cache_read_input_tokens: (\d+), cache_creation_input_tokens: (\d+)",
    )
    .unwrap();
    let c = usage.captures(events).expect("the fixture sends an aggregate Usage");
    let n = |i: usize| c[i].parse::<u64>().unwrap();
    (n(1), n(2), n(3), n(4))
}

fn policy(concurrent: usize) -> Arc<crate::orchestration::OrchestrationRuntime> {
    let foreground =
        agent_core::prompt::QualifiedModelId::parse("anthropic/claude-sonnet-4-6").unwrap();
    Arc::new(crate::orchestration::OrchestrationRuntime::baseline(foreground, concurrent, 64).unwrap())
}

fn params(timeout: u64) -> Value {
    json!({
        "name": "gate",
        "system_prompt": "You are the oneshot gate's worker.",
        "task": "Read /nonexistent/oneshot-gate.txt and report what it says.",
        "timeout": timeout,
    })
}

/// The base dir with the gate's stand-in login, and the provider pointed at
/// a stand-in answering the `n`th request with `reply(n)`.
struct Scene {
    base: crate::test_env::BaseDirGuard,
    _url: crate::test_env::EnvVarGuard,
    seen: Seen,
    policy: Arc<crate::orchestration::OrchestrationRuntime>,
}

async fn scene(concurrent: usize, reply: impl Fn(usize) -> Response + Clone + Send + Sync + 'static) -> Scene {
    let base = crate::test_env::BaseDirGuard::new();
    std::fs::write(
        base.path().join("auth.json"),
        json!({"anthropic":{"type":"oauth","refresh":"gate-refresh","access":"gate-access","expires":9_999_999_999_999u64}})
            .to_string(),
    )
    .unwrap();
    let (url, seen) = stand_in(reply).await;
    let _url = crate::test_env::EnvVarGuard::set("SYNAPS_ANTHROPIC_BASE_URL", &url);
    Scene {
        base,
        _url,
        seen,
        policy: policy(concurrent),
    }
}

impl Scene {
    /// A tool context on the scene's policy launching under `spend`, its
    /// events into a channel.
    fn context(
        &self,
        spend: Option<SpendParent>,
    ) -> (crate::tools::ToolContext, tokio::sync::mpsc::UnboundedReceiver<StreamEvent>) {
        let mut ctx = crate::tools::test_helpers::create_tool_context();
        ctx.capabilities.orchestration = Some(Arc::clone(&self.policy));
        ctx.capabilities.spend = spend;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.channels.tx_events = Some(tx);
        (ctx, rx)
    }

    fn requests(&self) -> String {
        stable(&render_requests(&self.seen.lock().unwrap()))
    }

    fn log(&self) -> String {
        let logs = self.base.path().join("logs").join("subagents");
        let mut entries: Vec<_> = std::fs::read_dir(&logs)
            .map(|dir| dir.map(|e| e.unwrap().path()).collect())
            .unwrap_or_default();
        entries.sort();
        let mut log = String::new();
        for path in entries {
            log.push_str(&format!("=== {}\n", path.file_name().unwrap().to_string_lossy()));
            log.push_str(&std::fs::read_to_string(&path).unwrap());
        }
        stable(&log)
    }
}

fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<StreamEvent>) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

fn rendered(events: &[StreamEvent]) -> String {
    stable(&events.iter().map(|e| format!("{e:?}\n")).collect::<String>())
}

/// The orchestration id of the oneshot that announced itself in `events`.
fn started(events: &[StreamEvent]) -> String {
    events
        .iter()
        .find_map(|e| match e {
            StreamEvent::Agent(crate::AgentEvent::SubagentStart { subagent_id, .. }) => {
                Some(format!("sa_{subagent_id}"))
            }
            _ => None,
        })
        .expect("the oneshot announced itself")
}

/// The session's messages up to and including the first run terminal,
/// waiting at most `bound` for it.
async fn until_terminal(rx: &mut SpendReceiver, bound: Duration) -> Vec<SinkMessage> {
    let mut out = Vec::new();
    let waited = Instant::now();
    loop {
        while let Ok(message) = rx.try_recv() {
            let terminal = matches!(
                message,
                SinkMessage::RunSealed { .. } | SinkMessage::RunEnded { .. }
            );
            out.push(message);
            if terminal {
                return out;
            }
        }
        assert!(waited.elapsed() < bound, "the run ends: {out:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// `spend <seq>`, `sealed <next>` or `ended <next>` per message.
fn shape(messages: &[SinkMessage]) -> Vec<String> {
    messages
        .iter()
        .map(|m| match m {
            SinkMessage::Spend { seq, .. } => format!("spend {seq}"),
            SinkMessage::RunSealed { next_seq, .. } => format!("sealed {next_seq}"),
            SinkMessage::RunEnded { next_seq, .. } => format!("ended {next_seq}"),
        })
        .collect()
}

/// Every message is worker `id`'s run (one run), under `parent`, and every
/// report is one turn request as the stand-in reported it, at the worker's
/// model. Returns the booked totals.
fn assert_one_run(messages: &[SinkMessage], id: &str, parent: Option<&str>) -> SpendTokens {
    let mut runs = Vec::new();
    let mut total = SpendTokens::default();
    for m in messages {
        let run = match m {
            SinkMessage::Spend { run, report, .. } => {
                assert_eq!(report.kind, SpendKind::Turn, "{m:?}");
                assert_eq!(report.tokens, reported(), "{m:?}");
                assert!(report.model.ends_with("claude-sonnet-4-6"), "{m:?}");
                assert!(report.cost_nanousd > 0, "{m:?}");
                total.input += report.tokens.input;
                total.output += report.tokens.output;
                total.cache_read += report.tokens.cache_read;
                total.cache_creation += report.tokens.cache_creation;
                run
            }
            SinkMessage::RunSealed { run, .. } | SinkMessage::RunEnded { run, .. } => run,
        };
        assert_eq!(run.handle, id, "{m:?}");
        assert_eq!(run.parent_handle.as_deref(), parent, "{m:?}");
        assert_eq!(run.agent, "gate", "{m:?}");
        runs.push(run.run_nonce);
    }
    runs.dedup();
    assert_eq!(runs.len(), 1, "one run: {messages:?}");
    total
}

fn usage_events(events: &[StreamEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, StreamEvent::Session(crate::SessionEvent::Usage { .. })))
        .count()
}

/// A finished run in a host with a sink: each request books once, under
/// the worker's run, and the aggregate Usage is gone. The requests, the
/// other events, the result and the log (its Tokens line included) are the
/// gate's; the booked totals are the aggregate the gate sends.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn sp1_a_finished_oneshot_books_each_request_and_sends_no_aggregate() {
    let scene = scene(8, |n| if n == 0 { tool_use_reply(n) } else { text_reply(n) }).await;
    let (sink, mut rx) = SpendSink::new();
    let (ctx, mut events) = scene.context(Some(SpendParent::Session(sink.clone())));
    let result = SubagentTool.execute(params(30), ctx).await.expect("a oneshot returns text");
    let events = drain(&mut events);

    assert_eq!(scene.requests(), fixture("finished_requests.txt"));
    assert_eq!(rendered(&events), without_usage(&fixture("finished_events.txt")));
    assert_eq!(usage_events(&events), 0);
    assert_eq!(stable(&result), fixture("finished_result.txt"));
    assert_eq!(scene.log(), fixture("finished_log.txt"));

    let id = started(&events);
    let messages = until_terminal(&mut rx, Duration::from_secs(10)).await;
    assert_eq!(shape(&messages), ["spend 0", "spend 1", "ended 2"]);
    let total = assert_one_run(&messages, &id, None);
    assert_eq!(
        (total.input, total.output, total.cache_read, total.cache_creation),
        aggregate(&fixture("finished_events.txt")),
        "one accounting path, the same total"
    );
    assert_eq!(sink.producers(), 0, "the run's lease has ended");
    assert!(rx.try_recv().is_err(), "nothing after the run's end");
}

/// A run that times out while its second request is held: the TIMED OUT
/// result, events and log are the gate's without the aggregate. The held
/// request books what its start reported when the worker stops (the
/// aggregate never counted it), then the run ends.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn sp2_a_timed_out_oneshot_books_the_held_request_at_its_end() {
    let scene = scene(8, |n| if n == 0 { tool_use_reply(n) } else { held_reply(n) }).await;
    let (sink, mut rx) = SpendSink::new();
    let (ctx, mut events) = scene.context(Some(SpendParent::Session(sink.clone())));
    let result = SubagentTool.execute(params(3), ctx).await.expect("a oneshot returns text");
    let events = drain(&mut events);

    assert_eq!(scene.requests(), fixture("timed_out_requests.txt"));
    assert_eq!(rendered(&events), without_usage(&fixture("timed_out_events.txt")));
    assert_eq!(stable(&result), fixture("timed_out_result.txt"));
    assert_eq!(scene.log(), fixture("timed_out_log.txt"));

    let id = started(&events);
    let messages = until_terminal(&mut rx, Duration::from_secs(20)).await;
    assert_eq!(shape(&messages), ["spend 0", "spend 1", "ended 2"]);
    assert_one_run(&messages, &id, None);
    assert_eq!(sink.producers(), 0);
}

/// A run whose only request the provider refuses: nothing to book, the
/// run ends with no report, and the error result is the gate's.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn sp3_a_failed_oneshot_books_nothing_and_ends_its_run() {
    let scene = scene(8, |_| invalid_reply()).await;
    let (sink, mut rx) = SpendSink::new();
    let (ctx, mut events) = scene.context(Some(SpendParent::Session(sink.clone())));
    let result = SubagentTool.execute(params(30), ctx).await.expect("a oneshot returns text");
    let events = drain(&mut events);

    assert_eq!(scene.requests(), fixture("failed_requests.txt"));
    assert_eq!(rendered(&events), fixture("failed_events.txt"));
    assert_eq!(stable(&result), fixture("failed_result.txt"));
    assert_eq!(scene.log(), fixture("failed_log.txt"));

    let id = started(&events);
    let messages = until_terminal(&mut rx, Duration::from_secs(10)).await;
    assert_eq!(shape(&messages), ["ended 0"]);
    assert_one_run(&messages, &id, None);
    assert_eq!(sink.producers(), 0);
}

/// A oneshot a worker launches books under its own run, which names the
/// worker as its parent; the worker's run books nothing for it, so the
/// session counts each request once.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn sp4_a_nested_oneshot_books_once_under_its_own_run() {
    let scene = scene(8, |n| if n == 0 { tool_use_reply(n) } else { text_reply(n) }).await;
    let (sink, mut rx) = SpendSink::new();
    let outer: RunLease = sink.lease("sa_outer", "outer").unwrap();
    let (ctx, mut events) = scene.context(Some(SpendParent::Run(outer.clone())));
    SubagentTool.execute(params(30), ctx).await.expect("a oneshot returns text");
    let events = drain(&mut events);
    assert_eq!(usage_events(&events), 0);

    let id = started(&events);
    let messages = until_terminal(&mut rx, Duration::from_secs(10)).await;
    assert_eq!(shape(&messages), ["spend 0", "spend 1", "ended 2"]);
    assert_one_run(&messages, &id, Some("sa_outer"));
    assert!(outer.is_live(), "the delegating run goes on");
    assert_eq!(sink.producers(), 1);
    drop(outer);
    match rx.try_recv() {
        Ok(SinkMessage::RunEnded { run, next_seq: 0 }) => assert_eq!(run.handle, "sa_outer"),
        other => panic!("the delegating run ends with no report of its own: {other:?}"),
    }
}

/// A oneshot whose delegating run is sealed, or whose session is ending,
/// is refused before it launches: no request, no event, no slot held (the
/// next oneshot is admitted under max_concurrent 1).
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn sp5_a_refused_lease_launches_nothing_and_holds_no_slot() {
    let scene = scene(1, text_reply).await;
    let (sink, mut rx) = SpendSink::new();

    let outer = sink.lease("sa_sealed", "outer").unwrap();
    assert!(outer.seal());
    let (ctx, mut events) = scene.context(Some(SpendParent::Run(outer.clone())));
    let err = SubagentTool.execute(params(30), ctx).await.expect_err("refused");
    assert!(err.to_string().contains("parent run sealed"), "{err}");
    assert!(drain(&mut events).is_empty(), "no event for a refused launch");

    let closing = {
        let (closing, _closing_rx) = SpendSink::new();
        closing.begin_closing();
        let (ctx, mut events) = scene.context(Some(SpendParent::Session(closing.clone())));
        let err = SubagentTool.execute(params(30), ctx).await.expect_err("refused");
        assert!(err.to_string().contains("session ending"), "{err}");
        assert!(drain(&mut events).is_empty(), "no event for a refused launch");
        closing
    };
    assert_eq!(closing.producers(), 0);
    assert!(scene.seen.lock().unwrap().is_empty(), "nothing was sent");
    assert!(scene.policy.unreconciled_runtime_handles().is_empty());

    let (ctx, mut events) = scene.context(Some(SpendParent::Session(sink.clone())));
    let next = SubagentTool.execute(params(30), ctx).await;
    assert_eq!(next.expect("admitted under max_concurrent 1"), "[subagent:gate] gate ok");
    let id = started(&drain(&mut events));
    let messages = until_terminal(&mut rx, Duration::from_secs(10)).await;
    // The sealed run's terminal comes first.
    assert_eq!(shape(&messages), ["sealed 0"]);
    let messages = until_terminal(&mut rx, Duration::from_secs(10)).await;
    assert_eq!(shape(&messages), ["spend 0", "ended 1"]);
    assert_one_run(&messages, &id, None);
}

/// A oneshot dropped while its worker's request is held (a cancelled
/// turn): the held request books what its start reported once the worker
/// stops, the run ends once, and the parent gets no Usage and no Done.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn sp6_a_dropped_oneshot_books_its_held_request_once() {
    let scene = scene(8, held_reply).await;
    let (sink, mut rx) = SpendSink::new();
    let (ctx, mut events) = scene.context(Some(SpendParent::Session(sink.clone())));
    let call = tokio::spawn(async move { SubagentTool.execute(params(30), ctx).await });
    let waited = Instant::now();
    while scene.seen.lock().unwrap().is_empty() {
        assert!(waited.elapsed() < Duration::from_secs(30), "the worker sends its request");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let id = started(&drain(&mut events));
    call.abort();
    assert!(call.await.unwrap_err().is_cancelled(), "the call was dropped");

    let messages = until_terminal(&mut rx, Duration::from_secs(25)).await;
    assert_eq!(shape(&messages), ["spend 0", "ended 1"], "stopped, not sealed");
    assert_one_run(&messages, &id, None);
    assert_eq!(sink.producers(), 0);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(rx.try_recv().is_err(), "nothing after the run's end");
    let late = drain(&mut events);
    assert_eq!(usage_events(&late), 0, "{late:?}");
    assert!(
        !late
            .iter()
            .any(|e| matches!(e, StreamEvent::Agent(crate::AgentEvent::SubagentDone { .. }))),
        "{late:?}"
    );
}
