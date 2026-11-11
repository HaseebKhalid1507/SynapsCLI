//! Context rollover must self-heal an unproductive soft boundary, never an
//! over-capacity request or an ambiguous durable head. Loopback inference only.
#[path = "support/phase2/mod.rs"]
mod support;
#[path = "support/notify_hooks.rs"]
mod notify_hooks;

use futures::StreamExt;
use serde_json::json;
use serial_test::serial;
use std::sync::{atomic::Ordering, Arc};
use std::time::Duration;
use support::*;
use synaps_cli::runtime::budget::{TurnBudget, TurnRole};
use synaps_cli::{BudgetDimension, Runtime, SessionEvent, SharedMessage, StreamEvent, TurnOutcome};
use tokio_util::sync::CancellationToken;

// Repeated small checkpoints leave too little removable material to justify a
// new context. IDs are distinct: dependency closure must not pin old calls just
// because an unrealistic fixture reused an identifier.
fn checkpoint_sse(id: usize) -> String {
    [
        json!({"type":"message_start","message":{"id":format!("msg_{id}"),"role":"assistant","type":"message","content":[],"usage":{"input_tokens":10,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":format!("cp_{id}"),"name":"context_checkpoint","input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"phase\":\"new_task\",\"note\":\"Continue retained work; do not replay\"}"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":10}}),
        json!({"type":"message_stop"}),
    ]
    .iter()
    .map(|v| format!("data: {v}\n\n"))
    .collect()
}

async fn checkpoint_server() -> (String, Arc<std::sync::atomic::AtomicUsize>, Bodies) {
    use axum::{body::Bytes, routing::post, Router};
    use std::sync::{atomic::AtomicUsize, Mutex};
    let hits = Arc::new(AtomicUsize::new(0));
    let bodies: Bodies = Arc::new(Mutex::new(Vec::new()));
    let counter = hits.clone();
    let recorded = bodies.clone();
    let app = Router::new().route(
        "/v1/messages",
        post(move |body: Bytes| {
            let index = counter.fetch_add(1, Ordering::SeqCst);
            recorded.lock().unwrap().push(body.to_vec());
            async move {
                (
                    [("content-type", "text/event-stream")],
                    checkpoint_sse(index),
                )
            }
        }),
    );
    (serve(app).await, hits, bodies)
}

async fn runtime(rounds: u32) -> Runtime {
    let mut rt = Runtime::new().await.unwrap();
    rt.set_model("claude-sonnet-4-5".into());
    rt.set_context_window(Some(1_000_000));
    rt.set_turn_budget(TurnBudget {
        max_provider_rounds: Some(rounds),
        max_round_renewals: 0,
        ..TurnBudget::for_role(TurnRole::Foreground)
    });
    rt.context_management_command("auto 20000 800000").unwrap();
    rt
}

fn user(content: String) -> SharedMessage {
    Arc::new(json!({"role":"user", "content":content}))
}

/// A message as the user wrote it: without the host's turn-identity tag,
/// which turn admission adds to the request that starts a turn.
fn untagged(message: &SharedMessage) -> serde_json::Value {
    let mut message = (**message).clone();
    if let Some(object) = message.as_object_mut() {
        object.remove("_synaps_turn");
    }
    message
}

fn final_history(events: &[StreamEvent]) -> &[SharedMessage] {
    events
        .iter()
        .rev()
        .find_map(|event| match event {
            StreamEvent::Session(SessionEvent::MessageHistory(h)) => Some(h.as_slice()),
            _ => None,
        })
        .expect("retained history")
}

#[tokio::test]
#[serial]
async fn small_repeated_checkpoints_self_heal_and_preserve_round_budget() {
    let _home = HomeGuard::new();
    let (url, hits, bodies) = checkpoint_server().await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", url);
    let rt = runtime(6).await;
    let pinned = user("Human constraint: do not deploy. ".repeat(2500));
    let events = drive_runtime_history_turn(&rt, vec![pinned.clone()]).await;
    assert_eq!(
        hits.load(Ordering::SeqCst),
        6,
        "recover without extra provider calls"
    );
    let errors = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Session(SessionEvent::Error(error)) => Some(error),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0].outcome,
        TurnOutcome::BudgetExceeded {
            dimension: BudgetDimension::ProviderRounds
        }
    );
    assert_eq!(events.iter().filter(|event| matches!(event, StreamEvent::Session(SessionEvent::Notice(s)) if s.starts_with("Context rollover deferred:"))).count(), 1);
    assert!(!events.iter().any(|event| matches!(
        event,
        StreamEvent::Session(SessionEvent::ContextHeadCheckpoint { .. })
    )));
    assert!(rt.context_management_status().contains("window 1"));
    let history = final_history(&events);
    assert_eq!(history.first().map(untagged), Some((*pinned).clone()));
    for id in 0..6 {
        let id = format!("cp_{id}");
        let blocks = history
            .iter()
            .filter_map(|m| m["content"].as_array())
            .flatten()
            .collect::<Vec<_>>();
        assert_eq!(
            blocks
                .iter()
                .filter(|b| b["type"] == "tool_use" && b["id"] == id)
                .count(),
            1
        );
        assert_eq!(
            blocks
                .iter()
                .filter(|b| b["type"] == "tool_result" && b["tool_use_id"] == id)
                .count(),
            1
        );
    }
    let bodies = bodies.lock().unwrap();
    // One preparation notice, one no-shrink transition, then quiet even as
    // the model repeats phase reports and the host retries the no-shrink check.
    for (index, body) in bodies.iter().enumerate() {
        let request: serde_json::Value = serde_json::from_slice(body).unwrap();
        let messages = request["messages"].to_string();
        assert_eq!(
            messages.matches("[Host context advisory,").count(),
            usize::from(index < 2)
        );
        assert_eq!(
            messages.contains("do not repeat context_checkpoint merely to force rollover"),
            index == 1
        );
    }
    assert!(!serde_json::to_string(history)
        .unwrap()
        .contains("[Host context advisory,"));
}

#[tokio::test]
#[serial]
async fn retained_over_capacity_history_stops_before_network() {
    let _home = HomeGuard::new();
    let (url, hits, _) = checkpoint_server().await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", url);
    let mut rt = runtime(6).await;
    rt.set_context_window(Some(200_000));
    rt.context_management_command("auto 20000 150000").unwrap();
    let pinned = user("Do not discard these requirements. ".repeat(25000));
    let events = drive_runtime_history_turn(&rt, vec![pinned.clone()]).await;
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    let retained: Vec<_> = final_history(&events).iter().map(untagged).collect();
    assert_eq!(retained, vec![(*pinned).clone()]);
    assert!(events.iter().any(|event| matches!(event, StreamEvent::Session(SessionEvent::Error(e)) if e.message.contains("hard capacity requires a smaller request"))));
    assert!(!events.iter().any(|event| matches!(event, StreamEvent::Session(SessionEvent::Notice(s)) if s.starts_with("Context rollover deferred:"))));
}

#[tokio::test]
#[serial]
async fn productive_rollover_still_waits_for_ack_and_failed_ack_blocks_inference() {
    for accept in [false, true] {
        let _home = HomeGuard::new();
        let (url, hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
        std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", url);
        let rt = runtime(6).await;
        // Force soft rollover at a threshold, without a model checkpoint round.
        // Size the bands from the runtime's own fixed request part (system
        // prompt + tool schemas, assessed over no history), not from the
        // default tool registry's size: the successor target (2/3 of
        // pressure) then leaves ~10k tokens for its messages, room for the
        // pinned first request beside the continuation envelope.
        let base = rt.assess_context(&[]).await.used_tokens();
        let pressure = (base + 10_000) * 3 / 2;
        let rollover = pressure + 10_000;
        rt.context_management_command(&format!("auto {pressure} {rollover}"))
            .unwrap();
        let history = vec![
            user("Do not deploy.".into()),
            // Alone over the rollover band: ~10.8 conservative tokens a repeat.
            Arc::new(
                json!({"role":"assistant", "content":"Archivable prior evidence. ".repeat(rollover as usize / 10)}),
            ),
            user("Continue only remaining work.".into()),
            Arc::new(json!({"role":"assistant", "content":"Last bounded result."})),
        ];
        let mut stream = rt
            .run_stream_with_messages(history.clone(), CancellationToken::new(), None, None, false)
            .await;
        let mut heads = 0;
        let mut events = Vec::new();
        while let Some(event) = tokio::time::timeout(Duration::from_secs(30), stream.next())
            .await
            .unwrap()
        {
            if let StreamEvent::Session(SessionEvent::ContextHeadCheckpoint {
                ref receipt, ..
            }) = event
            {
                heads += 1;
                assert_eq!(hits.load(Ordering::SeqCst), 0, "no inference before ack");
                // Model the frontend's ACK contract; persistence is covered by
                // separate durable-head tests. Never acknowledge a real save here.
                if accept {
                    receipt.complete(Ok(()));
                } else {
                    receipt.complete(Err(std::io::Error::other("fixture save failure")));
                }
            }
            let done = matches!(event, StreamEvent::Session(SessionEvent::Done));
            events.push(event);
            if done {
                break;
            }
        }
        assert_eq!(heads, 1);
        assert_eq!(hits.load(Ordering::SeqCst), usize::from(accept));
        if accept {
            assert!(rt.context_management_status().contains("window 2"));
            let retained: Vec<_> = final_history(&events).iter().map(untagged).collect();
            assert!(retained.contains(&history[0]) && retained.contains(&history[2]));
            assert!(!retained.contains(&history[1]));
        } else {
            assert!(rt.context_management_status().contains("window 1"));
            assert!(events.iter().any(|event| matches!(event, StreamEvent::Session(SessionEvent::Error(e)) if e.message.contains("durability was not acknowledged"))));
        }
    }
}

/// The api-log names the context window of every request after a rollover,
/// and marks the window's first request (the one that writes the
/// successor's prompt cache).
#[tokio::test]
#[serial]
async fn foreground_rollover_marks_the_first_request_of_the_new_window() {
    let home = HomeGuard::new();
    let (url, hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", url);
    let mut rt = runtime(6).await;
    rt.set_telemetry_level(synaps_cli::runtime::telemetry::TelemetryLevel::Basic);
    rt.context_management_command("auto 20000 30000").unwrap();
    let history = vec![
        user("Do not deploy.".into()),
        Arc::new(json!({"role":"assistant", "content":"Archivable prior evidence. ".repeat(4000)})),
        user("Continue only remaining work.".into()),
        Arc::new(json!({"role":"assistant", "content":"Last bounded result."})),
    ];
    let mut stream = rt
        .run_stream_with_messages(history, CancellationToken::new(), None, None, false)
        .await;
    while let Some(event) = tokio::time::timeout(Duration::from_secs(30), stream.next())
        .await
        .unwrap()
    {
        if let StreamEvent::Session(SessionEvent::ContextHeadCheckpoint { ref receipt, .. }) = event {
            receipt.complete(Ok(()));
        }
        if matches!(event, StreamEvent::Session(SessionEvent::Done)) {
            break;
        }
    }
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    rt.telemetry_writer().unwrap().shutdown(Duration::from_secs(5));
    let log = std::fs::read_to_string(home.home.path().join(".cache/synaps/api-log.jsonl")).unwrap();
    let records: Vec<serde_json::Value> = log.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(records.len(), 1, "{log}");
    assert_eq!(records[0]["rollover_window"], 2, "{log}");
    assert_eq!(records[0]["rollover"], true, "{log}");
    assert!(records[0].get("worker").is_none());
}

// ── on_context_archived: a rollover announces its archive, and no hook can
// ── hold a rollover up ────────────────────────────────────────────────────

const ARCHIVED: &str = "on_context_archived";
const CHAT_ID: &str = "chat-under-test";

/// What one rollover turn did, as the user and the provider see it.
#[derive(Debug, PartialEq)]
struct Rolled {
    provider_requests: usize,
    head_saves: usize,
    /// "window N", from the runtime's status line.
    window: String,
    /// The final history, without its per-run parts (see `rollover_turn`).
    retained: String,
    errors: Vec<String>,
}

struct RolloverRun {
    rolled: Rolled,
    /// From starting the turn to its `Done`.
    elapsed: Duration,
    /// The id in the new window's context marker, when the window rolled.
    archive_id: Option<String>,
    /// That marker (`_synaps_context`); `null` when the window did not roll.
    marker: serde_json::Value,
    /// What the fixture extension logged, read after it was shut down.
    hook_events: Vec<serde_json::Value>,
}

/// One turn of chat `CHAT_ID`, running in `chat_folder`, over a history
/// that is due for rollover. With `hook_mode`, the notify-only fixture is
/// loaded in that mode and subscribed to `on_context_archived`; the run
/// waits for `expect_events` of them before shutting the extension down.
/// With `break_store`, the archive store cannot be opened, so the seal fails.
async fn rollover_turn(
    home: &HomeGuard,
    chat_folder: &std::path::Path,
    hook_mode: Option<&str>,
    expect_events: usize,
    break_store: bool,
) -> RolloverRun {
    rollover_turn_over(home, chat_folder, hook_mode, expect_events, break_store, Vec::new()).await
}

/// A tool call whose input is nested deeper than one archive admits, and its
/// result. A window holding them is over a per-archive limit however small
/// it is, so it is sealed as several archives.
fn deep_tool_call() -> Vec<SharedMessage> {
    let mut input = json!("leaf");
    for _ in 0..40 {
        input = json!({"nested": input});
    }
    vec![
        Arc::new(json!({"role":"assistant", "content":[
            {"type":"tool_use", "id":"deep", "name":"plot", "input":input}
        ]})),
        Arc::new(json!({"role":"user", "content":[
            {"type":"tool_result", "tool_use_id":"deep", "content":"plotted"}
        ]})),
    ]
}

/// [`rollover_turn`] with `older` between the archivable evidence and the
/// current request, where a rollover drops it. `expect_events` is per
/// archive of the sealed window.
async fn rollover_turn_over(
    home: &HomeGuard,
    chat_folder: &std::path::Path,
    hook_mode: Option<&str>,
    expect_events: usize,
    break_store: bool,
    older: Vec<SharedMessage>,
) -> RolloverRun {
    use synaps_cli::extensions::manager::ExtensionManager;
    let (url, hits, _) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", url);
    let mut rt = runtime(6).await;
    rt.set_cwd(Some(chat_folder.to_path_buf()));
    let mut extensions = ExtensionManager::new(rt.hook_bus().clone());
    let log = match hook_mode {
        Some(mode) => Some(
            notify_hooks::load(&mut extensions, &home.base_dir(), "archive-indexer", mode, &[ARCHIVED])
                .await,
        ),
        None => None,
    };
    if break_store {
        // A file where the store's directory would be.
        std::fs::write(home.base_dir().join("context-archives"), b"not a directory").unwrap();
    }
    // Bands as in `productive_rollover_still_waits_for_ack_...` above.
    let base = rt.assess_context(&[]).await.used_tokens();
    let pressure = (base + 10_000) * 3 / 2;
    let rollover = pressure + 10_000;
    rt.context_management_command(&format!("auto {pressure} {rollover}"))
        .unwrap();
    let mut history = vec![
        user("Do not deploy.".into()),
        Arc::new(
            json!({"role":"assistant", "content":"Archivable prior evidence. ".repeat(rollover as usize / 10)}),
        ),
        user("Continue only remaining work.".into()),
        Arc::new(json!({"role":"assistant", "content":"Last bounded result."})),
    ];
    let current = history.split_off(2);
    history.extend(older);
    history.extend(current);
    rt.reset_context_continuation(CHAT_ID, &history);

    let started = std::time::Instant::now();
    let mut stream = rt
        .run_stream_with_messages(history, CancellationToken::new(), None, None, false)
        .await;
    let mut events = Vec::new();
    let mut head_saves = 0;
    while let Some(event) = tokio::time::timeout(Duration::from_secs(30), stream.next())
        .await
        .expect("rollover turn hung beyond 30 s")
    {
        if let StreamEvent::Session(SessionEvent::ContextHeadCheckpoint { ref receipt, .. }) = event {
            head_saves += 1;
            receipt.complete(Ok(()));
        }
        let done = matches!(event, StreamEvent::Session(SessionEvent::Done));
        events.push(event);
        if done {
            break;
        }
    }
    let elapsed = started.elapsed();

    // A window sealed as several archives sends one event for each.
    let marker = final_history(&events)
        .iter()
        .find_map(|message| message.get("_synaps_context"))
        .cloned()
        .unwrap_or_default();
    let archives = marker["archives"].as_array().map_or(1, Vec::len);
    if let Some(log) = &log {
        notify_hooks::wait_for(log, expect_events * archives).await;
    }
    // Stops the extension; a delivery still in flight ends with it.
    extensions.shutdown_all().await;
    let hook_events = log.as_deref().map(notify_hooks::logged).unwrap_or_default();

    // Without what differs from one run to the next by design: the turn
    // tag, the reply's wall-clock stamp and (below) the archive's id.
    let retained: Vec<serde_json::Value> = final_history(&events)
        .iter()
        .map(|message| {
            let mut message = untagged(message);
            if let Some(object) = message.as_object_mut() {
                object.remove("_synaps_at");
            }
            message
        })
        .collect();
    let archive_id = retained
        .iter()
        .find_map(|message| message["_synaps_context"]["archive"].as_str())
        .map(str::to_owned);
    let mut retained = serde_json::to_string(&retained).unwrap();
    if let Some(id) = &archive_id {
        retained = retained.replace(id, "<archive>");
    }
    let errors = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Session(SessionEvent::Error(error)) => Some(error.message.clone()),
            _ => None,
        })
        .collect();
    RolloverRun {
        rolled: Rolled {
            provider_requests: hits.load(Ordering::SeqCst),
            head_saves,
            window: rt
                .context_management_status()
                .split(" | ")
                .nth(1)
                .unwrap_or_default()
                .to_owned(),
            retained,
            errors,
        },
        elapsed,
        archive_id,
        marker,
        hook_events,
    }
}

/// End to end: the rollover's archive reaches a real extension process with
/// the id the new window names, the file that holds it, and the chat's
/// folder (which is not the folder this process runs in).
#[tokio::test]
#[serial]
async fn a_rollover_announces_its_archive_to_an_extension() {
    let home = HomeGuard::new();
    let chat = tempfile::tempdir().unwrap();
    assert_ne!(chat.path(), std::env::current_dir().unwrap());
    let run = rollover_turn(&home, chat.path(), Some("record"), 1, false).await;
    assert_eq!(run.rolled.errors, Vec::<String>::new());
    assert_eq!((run.rolled.provider_requests, run.rolled.head_saves), (1, 1));
    assert_eq!(run.rolled.window, "window 2");

    assert_eq!(run.hook_events.len(), 1, "one archive, one event: {:?}", run.hook_events);
    let params = &run.hook_events[0]["params"];
    assert_eq!(params["kind"], ARCHIVED);
    assert_eq!(params["session_id"], CHAT_ID);
    assert_eq!(params["cwd"], json!(chat.path().to_str().unwrap()));
    for unused in ["tool_name", "tool_input", "tool_output", "message", "transcript"] {
        assert!(params[unused].is_null(), "{unused}: {params}");
    }
    let data = &params["data"];
    let id = run.archive_id.as_deref().expect("the new window names its archive");
    assert_eq!(data["archive_id"], json!(id));
    assert_eq!(data["window"], json!(2));
    assert_eq!(data["source_message_count"], json!(4));
    assert_eq!((&data["part"], &data["parts"]), (&json!(1), &json!(1)));
    assert_eq!((&data["first_source_index"], &data["reduced_messages"]), (&json!(0), &json!(0)));

    // `path` is the file that holds the archive: a live record of that id,
    // under the store of the project the event names.
    let path = std::path::PathBuf::from(data["path"].as_str().expect("a path"));
    assert!(path.is_absolute() && path.starts_with(home.base_dir()), "{path:?}");
    let record: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(record["id"], json!(id));
    assert_eq!(record["body"]["state"], "live");
    assert_eq!(record["body"]["source_message_count"], data["source_message_count"]);
    assert_eq!(
        json!(record["body"]["messages"].as_array().unwrap().len()),
        data["message_count"]
    );
    let located = agent_core::context_archive::archive_file(
        &home.base_dir(),
        data["project_key"].as_str().unwrap(),
        id,
    )
    .unwrap();
    assert_eq!(located.as_deref(), Some(path.as_path()));
}

/// Fail-open, by result and by time. A hook that never answers, answers
/// with an error, answers `block`, or whose process exits mid-call leaves
/// the rollover exactly as it is with no extension at all, and the turn
/// does not wait for it: a turn that waited on the hung one would take the
/// 5 s handler timeout longer than the baseline; the bound is half of that.
#[tokio::test]
#[serial]
async fn no_hook_can_change_or_hold_up_a_rollover() {
    let chat = tempfile::tempdir().unwrap();
    let baseline = {
        let home = HomeGuard::new();
        rollover_turn(&home, chat.path(), None, 0, false).await
    };
    assert_eq!(baseline.rolled.errors, Vec::<String>::new());
    assert_eq!((baseline.rolled.provider_requests, baseline.rolled.head_saves), (1, 1));
    assert_eq!(baseline.rolled.window, "window 2");
    assert!(baseline.hook_events.is_empty());

    for mode in ["hang", "error", "block", "exit"] {
        let home = HomeGuard::new();
        let hooked = rollover_turn(&home, chat.path(), Some(mode), 1, false).await;
        assert_eq!(hooked.rolled, baseline.rolled, "{mode}");
        assert!(
            hooked.elapsed < baseline.elapsed + Duration::from_millis(2500),
            "{mode}: the turn took {:?}, {:?} without an extension",
            hooked.elapsed,
            baseline.elapsed
        );
        // The hook was really sent, to a process that then did as `mode` says.
        let event = &hooked.hook_events[0];
        assert_eq!(event["params"]["kind"], ARCHIVED, "{mode}");
        assert_eq!(event["params"]["data"]["archive_id"], json!(hooked.archive_id), "{mode}");
        // Nothing is left running: every process the extension started,
        // the hung one included, is gone once the extension is shut down.
        #[cfg(target_os = "linux")]
        for logged in &hooked.hook_events {
            assert!(notify_hooks::exited(logged["pid"].as_u64().unwrap()).await, "{mode}");
        }
    }
}

/// Only an archive that was sealed is announced: when the seal fails the
/// turn stops with the history kept, and no event is sent.
#[tokio::test]
#[serial]
async fn a_failed_seal_announces_nothing() {
    let home = HomeGuard::new();
    let chat = tempfile::tempdir().unwrap();
    let run = rollover_turn(&home, chat.path(), Some("record"), 0, true).await;
    assert!(
        run.rolled.errors.iter().any(|e| e.contains("archive commit failed; history retained")),
        "{:?}",
        run.rolled.errors
    );
    assert_eq!((run.rolled.provider_requests, run.rolled.head_saves), (0, 0));
    assert_eq!(run.rolled.window, "window 1");
    assert_eq!(run.archive_id, None);
    // The extension was up for the whole turn and through its shutdown.
    assert_eq!(run.hook_events, Vec::<serde_json::Value>::new());
}

/// End to end for a window over what one archive holds, which used to end
/// the turn with "archive commit failed; history retained" on every attempt.
/// The turn goes on in window 2; the new window tells the model where each
/// part is and the host records them; and a real extension process is sent
/// one event per part, in order, each with the file that holds that part.
#[tokio::test]
#[serial]
async fn a_split_rollover_announces_every_part_to_an_extension() {
    let home = HomeGuard::new();
    let chat = tempfile::tempdir().unwrap();
    let run =
        rollover_turn_over(&home, chat.path(), Some("record"), 1, false, deep_tool_call()).await;
    assert_eq!(run.rolled.errors, Vec::<String>::new());
    assert_eq!((run.rolled.provider_requests, run.rolled.head_saves), (1, 1));
    assert_eq!(run.rolled.window, "window 2");

    let archives = run.marker["archives"].as_array().expect("the marker records the parts");
    assert!(archives.len() > 1, "{}", run.marker);
    assert_eq!(run.marker["archive"], archives.last().unwrap()["id"], "resume reads the last part");
    assert_eq!(run.archive_id.as_deref(), run.marker["archive"].as_str());
    assert_eq!(run.hook_events.len(), archives.len(), "one event per part: {:?}", run.hook_events);

    let mut covered = 0;
    let mut reduced = 0;
    let mut files = std::collections::HashSet::new();
    for (index, (event, recorded)) in run.hook_events.iter().zip(archives).enumerate() {
        let params = &event["params"];
        assert_eq!(params["kind"], ARCHIVED);
        assert_eq!(params["session_id"], CHAT_ID);
        assert_eq!(params["cwd"], json!(chat.path().to_str().unwrap()));
        let data = &params["data"];
        assert_eq!((&data["part"], &data["parts"]), (&json!(index + 1), &json!(archives.len())));
        assert_eq!(data["window"], json!(2));
        assert_eq!(data["archive_id"], recorded["id"]);
        assert_eq!(data["first_source_index"], json!(covered));
        assert_eq!(data["first_source_index"], recorded["first"]);
        assert_eq!(data["source_message_count"], recorded["count"]);
        assert_eq!(data["message_count"], recorded["messages"]);
        assert_eq!(data["reduced_messages"], recorded.get("reduced").cloned().unwrap_or(json!(0)));
        covered += data["source_message_count"].as_u64().unwrap();
        reduced += data["reduced_messages"].as_u64().unwrap();

        // Its own file: a live record of that archive, where the store has it.
        let id = data["archive_id"].as_str().unwrap();
        let path = std::path::PathBuf::from(data["path"].as_str().expect("a path"));
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(record["id"], json!(id));
        assert_eq!(record["body"]["state"], "live");
        assert_eq!(record["body"]["source_message_count"], data["source_message_count"]);
        let located = agent_core::context_archive::archive_file(
            &home.base_dir(),
            data["project_key"].as_str().unwrap(),
            id,
        )
        .unwrap();
        assert_eq!(located.as_deref(), Some(path.as_path()));
        assert!(files.insert(path), "each part has its own file");
    }
    assert_eq!(covered, 6, "the parts cover the six messages of the window");
    assert_eq!(reduced, 1, "the call with the deep input is stored reduced");

    // The model is told every part, in order, and that one message is reduced.
    let text = &run.rolled.retained;
    let mut from = 0;
    for recorded in archives {
        let id = recorded["id"].as_str().unwrap();
        let listed = if id == run.archive_id.as_deref().unwrap() { "<archive>" } else { id };
        let at = text[from..].find(&format!("ctx-{listed} (")).expect("every part is listed, in order");
        from += at + 1;
    }
    assert!(text.contains("Earlier eligible source evidence, in source order: ctx-"), "{text}");
    assert!(text.contains("1 message was too large for one archive"), "{text}");
}
