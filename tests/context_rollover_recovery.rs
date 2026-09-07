//! Context rollover must self-heal an unproductive soft boundary, never an
//! over-capacity request or an ambiguous durable head. Loopback inference only.
#[path = "support/phase2/mod.rs"]
mod support;

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
        max_provider_rounds: rounds,
        max_round_renewals: 0,
        ..TurnBudget::for_role(TurnRole::Foreground)
    });
    rt.context_management_command("auto 20000 800000").unwrap();
    rt
}

fn user(content: String) -> SharedMessage {
    Arc::new(json!({"role":"user", "content":content}))
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
    assert_eq!(history.first(), Some(&pinned));
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
    let recovered: serde_json::Value = serde_json::from_slice(&bodies[2]).unwrap();
    assert!(recovered["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["content"]
            .to_string()
            .contains("do not repeat context_checkpoint merely to force rollover")));
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
    assert_eq!(final_history(&events), &[pinned]);
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
        rt.context_management_command("auto 20000 30000").unwrap();
        let history = vec![
            user("Do not deploy.".into()),
            Arc::new(
                json!({"role":"assistant", "content":"Archivable prior evidence. ".repeat(4000)}),
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
            let retained = final_history(&events);
            assert!(retained.contains(&history[0]) && retained.contains(&history[2]));
            assert!(!retained.contains(&history[1]));
        } else {
            assert!(rt.context_management_status().contains("window 1"));
            assert!(events.iter().any(|event| matches!(event, StreamEvent::Session(SessionEvent::Error(e)) if e.message.contains("durability was not acknowledged"))));
        }
    }
}
