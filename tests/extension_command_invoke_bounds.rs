//! CP-11 fix-3: bounded `command.invoke` output collection.
//!
//! Adversarial oracle: a hostile extension floods `command.output`
//! notifications (matching the caller's `request_id`) while the
//! `command.invoke` JSON-RPC response remains pending. The host must NOT
//! retain the aggregate flood in memory — in particular, the moment the
//! call resolves (BEFORE any UI drain — the TUI consumes command output
//! post-hoc), retained command-output bytes must not exceed the
//! invocation-local budget plus its bounded control reserve.
//!
//! The pre-fix architecture (`mpsc::UnboundedSender<InvokeCommandEvent>`
//! drained only after the call resolved) parked the ENTIRE flood in the
//! sink: this file's oracle failed RED against it, retaining all
//! 41,943,040 flood bytes across 641 events. Neither the 4 MiB per-frame
//! reader cap, the 120 s invoke timeout, nor the bounded notification
//! queue bounded aggregate queued bytes while the invocation loop kept
//! forwarding. The fix pairs a BOUNDED sink with an eagerly concurrent
//! budget-enforcing collector (`extensions::invoke_output`).
//!
//! MUTATION CHECK: restoring the post-hoc unbounded behavior or
//! disabling the budget (`InvokeOutputBudget::unlimited()`) makes the
//! bounded-retention assertions here fail again — proven by the
//! `disabled_budget_retains_entire_flood` unit oracle in
//! `invoke_output.rs` and by rerunning this file's flood test against
//! the mutations during review.

use std::path::PathBuf;

use synaps_cli::extensions::invoke_output::{
    invoke_event_channel, InvokeOutputBudget, InvokeOutputReport, INVOKE_CONTROL_RESERVE_BYTES,
    INVOKE_RETAINED_BYTE_BUDGET,
};
use synaps_cli::extensions::runtime::process::ProcessExtension;
use synaps_cli::extensions::runtime::ExtensionHandler;

/// Flood shape: 640 x 64 KiB = 40 MiB of hostile command output.
const FLOOD_EVENTS: u64 = 640;
const FLOOD_PAD_BYTES: u64 = 65536;
const FLOOD_TOTAL_BYTES: u64 = FLOOD_EVENTS * FLOOD_PAD_BYTES;

/// Retention ceiling the host must obey when the call resolves: the
/// invocation byte budget plus the bounded control reserve. Far below
/// the 40 MiB flood.
const RETAINED_CEILING_BYTES: u64 =
    (INVOKE_RETAINED_BYTE_BUDGET + INVOKE_CONTROL_RESERVE_BYTES) as u64;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

async fn spawn_flood_fixture(id: &str) -> ProcessExtension {
    let fixture = fixture_path("flood_command_output_extension.py");
    assert!(fixture.exists(), "fixture missing: {:?}", fixture);
    let handler = ProcessExtension::spawn(id, "python3", &[fixture.to_string_lossy().to_string()])
        .await
        .expect("spawn fixture");
    handler
        .initialize_for_test(None)
        .await
        .expect("initialize fixture");
    handler
}

/// Assert the exact counter identities that make the accounting lossless.
fn assert_exact_accounting(report: &InvokeOutputReport) {
    let c = &report.counters;
    assert_eq!(
        c.consumed_payload_bytes,
        c.retained_payload_bytes + c.truncated_payload_bytes + c.dropped_payload_bytes,
        "byte conservation: consumed == retained + truncated + dropped"
    );
    assert_eq!(
        c.consumed_events,
        c.retained_events + c.dropped_events,
        "event conservation: consumed == retained + dropped"
    );
    assert_eq!(
        report.events.len() as u64,
        c.retained_events,
        "report holds exactly the retained events"
    );
    assert!(
        c.produced_events >= c.consumed_events
            && c.produced_payload_bytes >= c.consumed_payload_bytes,
        "production can only exceed consumption"
    );
}

/// THE ORACLE: hostile 40 MiB command-output flood; when `command.invoke`
/// resolves — before any UI consumption (the collector's report IS the
/// only retention, so absent/slow UI drain cannot change it) — retained
/// output bytes must be bounded, the call must complete without
/// deadlock, the terminal result must be valid, and the accounting must
/// be exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hostile_command_output_flood_is_bounded_when_invoke_resolves() {
    use synaps_cli::extensions::commands::CommandOutputEvent;
    use synaps_cli::extensions::runtime::InvokeCommandEvent;

    let handler = spawn_flood_fixture("flood-cmd-output").await;

    // Production wiring shape (mirrors ExtensionManager::
    // invoke_command_collected): bounded sink + eagerly concurrent
    // collector joined with the invocation.
    let (sink, collector) = invoke_event_channel(InvokeOutputBudget::default());
    let invoke = handler.invoke_command(
        "flood",
        vec![FLOOD_EVENTS.to_string(), FLOOD_PAD_BYTES.to_string()],
        "req-flood-1",
        sink,
    );
    let (result, report) = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        tokio::join!(invoke, collector.collect())
    })
    .await
    .expect("command.invoke must not deadlock under a hostile flood");

    // Valid terminal result.
    let value = result.expect("command.invoke must resolve Ok");
    assert_eq!(value["status"], "ok");
    assert_eq!(value["payload_bytes"], FLOOD_TOTAL_BYTES);

    // Exact production accounting: every flood byte/event was counted at
    // the producer (640 pads + 1 Done marker), and the eager collector
    // consumed every one of them.
    let c = &report.counters;
    assert_eq!(c.produced_events, FLOOD_EVENTS + 1);
    assert_eq!(c.produced_payload_bytes, FLOOD_TOTAL_BYTES);
    assert_eq!(c.consumed_events, c.produced_events);
    assert_eq!(c.consumed_payload_bytes, c.produced_payload_bytes);

    // Fixed retention: the byte budget divides the pad size exactly, so
    // exactly 4 x 64 KiB text events are retained, nothing is truncated,
    // and the remaining 636 pads are dropped whole with exact byte
    // accounting. The terminal Done marker survives via the control
    // reserve.
    assert!(
        c.retained_payload_bytes <= RETAINED_CEILING_BYTES,
        "retained bytes {} must stay under ceiling {}",
        c.retained_payload_bytes,
        RETAINED_CEILING_BYTES,
    );
    assert_eq!(c.retained_payload_bytes, INVOKE_RETAINED_BYTE_BUDGET as u64);
    assert_eq!(c.retained_events, 5); // 4 full pads + Done marker
    assert_eq!(c.truncated_events, 0);
    assert_eq!(c.truncated_payload_bytes, 0);
    assert_eq!(c.dropped_events, FLOOD_EVENTS - 4);
    assert_eq!(
        c.dropped_payload_bytes,
        FLOOD_TOTAL_BYTES - INVOKE_RETAINED_BYTE_BUDGET as u64
    );
    assert!(c.saw_done, "terminal Done marker must be observed");
    assert_exact_accounting(&report);

    // User-visible head retention: first retained event is the first pad,
    // last retained event is the terminal marker.
    assert!(matches!(
        &report.events[0],
        InvokeCommandEvent::Output(CommandOutputEvent::Text { content })
            if content.len() == FLOOD_PAD_BYTES as usize && content.starts_with('x')
    ));
    assert_eq!(
        report.events.last(),
        Some(&InvokeCommandEvent::Output(CommandOutputEvent::Done))
    );
    // The budget limit is user-visible with exact totals.
    let notice = report.limit_notice().expect("flood must report limits");
    assert!(!notice.severity_error);
    assert!(notice.message.contains(&FLOOD_TOTAL_BYTES.to_string()));

    handler.shutdown().await;
}

/// Cancellation: dropping the invocation mid-flood (the caller aborts;
/// same shape as the 120 s timeout dropping `invoke_future`) must drop
/// the sink, complete the collector promptly with bounded retention and
/// coherent counters, and release the transport so shutdown does not
/// hang behind a producer blocked on a full channel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_invoke_mid_flood_releases_collector_and_producers() {
    let handler = std::sync::Arc::new(spawn_flood_fixture("flood-cmd-cancel").await);

    let (sink, collector) = invoke_event_channel(InvokeOutputBudget::default());
    let invoke_handler = std::sync::Arc::clone(&handler);
    let invoke_task = tokio::spawn(async move {
        invoke_handler
            .invoke_command("flood_forever", vec![], "req-cancel-1", sink)
            .await
    });
    let collect_task = tokio::spawn(collector.collect());

    // Let the hostile flood run, then cancel the invocation.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    invoke_task.abort();
    let _ = invoke_task.await;

    // The dropped sink closes the channel: the collector must finish
    // promptly (no dangling forwarder can keep it alive).
    let report = tokio::time::timeout(std::time::Duration::from_secs(5), collect_task)
        .await
        .expect("collector must complete promptly after cancellation")
        .expect("collector task must not panic");
    assert!(
        report.counters.retained_payload_bytes <= RETAINED_CEILING_BYTES,
        "retention must stay bounded up to the cancellation instant"
    );
    assert_exact_accounting(&report);
    assert!(!report.counters.saw_done, "flood_forever never terminates");

    // Transport released: shutdown must not hang behind a blocked
    // producer or a stuck notification forwarder.
    tokio::time::timeout(std::time::Duration::from_secs(10), handler.shutdown())
        .await
        .expect("shutdown must complete promptly after cancellation");
}

/// Behavior preservation: a legitimate small command's output is
/// retained verbatim — every event, in order, with zero drops or
/// truncation and produced == consumed == retained.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bounded_command_output_is_preserved_verbatim() {
    use synaps_cli::extensions::commands::CommandOutputEvent;
    use synaps_cli::extensions::runtime::InvokeCommandEvent;
    use synaps_cli::extensions::tasks::{TaskEvent, TaskKind};

    let handler = spawn_flood_fixture("small-cmd-output").await;

    let (sink, collector) = invoke_event_channel(InvokeOutputBudget::default());
    let invoke = handler.invoke_command("small", vec![], "req-small-1", sink);
    let (result, report) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        tokio::join!(invoke, collector.collect())
    })
    .await
    .expect("small command must resolve");

    assert_eq!(result.expect("ok")["status"], "ok");
    let c = &report.counters;
    assert_eq!(c.dropped_events, 0);
    assert_eq!(c.truncated_events, 0);
    assert_eq!(c.produced_events, c.retained_events);
    assert_eq!(c.produced_payload_bytes, c.retained_payload_bytes);
    assert!(c.saw_done);
    assert!(
        report.limit_notice().is_none(),
        "no limit notice for bounded output"
    );
    assert_exact_accounting(&report);

    assert_eq!(
        report.events,
        vec![
            InvokeCommandEvent::Output(CommandOutputEvent::Text {
                content: "hello".into()
            }),
            InvokeCommandEvent::Output(CommandOutputEvent::System {
                content: "working".into()
            }),
            InvokeCommandEvent::Task(TaskEvent::Start {
                id: "t1".into(),
                label: "Fetching".into(),
                kind: TaskKind::Download,
            }),
            InvokeCommandEvent::Task(TaskEvent::Update {
                id: "t1".into(),
                current: Some(1),
                total: Some(2),
                message: None,
            }),
            InvokeCommandEvent::Task(TaskEvent::Log {
                id: "t1".into(),
                line: "fetched shard 1".into(),
            }),
            InvokeCommandEvent::Task(TaskEvent::Done {
                id: "t1".into(),
                error: None,
            }),
            InvokeCommandEvent::Output(CommandOutputEvent::Table {
                headers: vec!["name".into(), "value".into()],
                rows: vec![
                    vec!["alpha".into(), "1".into()],
                    vec!["beta".into(), "2".into()],
                ],
            }),
            InvokeCommandEvent::Output(CommandOutputEvent::Error {
                content: "one minor problem".into()
            }),
            InvokeCommandEvent::Output(CommandOutputEvent::Done),
        ],
        "bounded output must be preserved verbatim, in order"
    );

    handler.shutdown().await;
}
