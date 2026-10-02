//! Cache-aligned compaction (task #430): the summary request must reuse the
//! session's own prompt prefix (tools, system, model, thinking, history)
//! byte-for-byte and only append the instruction, so the history is read
//! from the prompt cache instead of being resent as one uncached flattened
//! transcript (S347: a 1.6 MB flattened compaction drew HTTP 429 on every
//! retry while the session's cached turns kept succeeding).

#[path = "support/phase2/mod.rs"]
mod support;

use serde_json::{json, Value};
use serial_test::serial;
use std::sync::Arc;
use support::*;
use synaps_cli::runtime::{Runtime, SessionEvent, StreamEvent};

fn history_of(events: &[StreamEvent]) -> Vec<synaps_cli::SharedMessage> {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            StreamEvent::Session(SessionEvent::MessageHistory(h)) => Some(h.clone()),
            _ => None,
        })
        .expect("turn must surface message history")
}

/// A message with every `cache_control` marker removed (markers sit on the
/// newest content of each request, so they legitimately move).
fn unmarked(v: &Value) -> Value {
    match v {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| k.as_str() != "cache_control")
                .map(|(k, x)| (k.clone(), unmarked(x)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(unmarked).collect()),
        other => other.clone(),
    }
}

/// The reference is the session's real NEXT turn, not the turn that built
/// the history: a round only coerces its newest message into blocks to stamp
/// the cache marker, so once a message is history every later round resends
/// it in its stored form. The compaction request has to be exactly that next
/// turn with the instruction in place of the next user prompt. If it is, it
/// reads the same cache entries any next turn would.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn compaction_is_the_sessions_next_request_with_the_instruction_appended() {
    let _guard = HomeGuard::new();
    let (url, hits, bodies) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let mut rt = Runtime::new().await.expect("runtime");
    rt.set_model("claude-sonnet-4-5".to_string());
    assert!(rt.compaction_reuses_session_model(), "no compaction_model configured");

    let events = drive_runtime_turn(&rt, "please remember the number 1729", false).await;
    let history = history_of(&events);
    assert!(history.len() >= 2, "user + assistant");

    // The session's real next turn over the same history.
    let mut next = history.clone();
    next.push(Arc::new(json!({"role": "user", "content": "what was the number?"})));
    drive_runtime_history_turn(&rt, next).await;

    let outcome =
        synaps_cli::runtime::compaction::compact_conversation(&history, &rt, None)
            .await
            .expect("compaction succeeds");
    assert_eq!(outcome.summary_text, "hi", "the stub's reply is the summary");

    let bodies = bodies.lock().unwrap();
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "turn 1 + next turn + one compaction"
    );
    let next_turn: Value = serde_json::from_slice(&bodies[1]).unwrap();
    let compaction: Value = serde_json::from_slice(&bodies[2]).unwrap();

    // The cached prefix: tools → system → messages. Byte-identical up to the
    // moving tail markers.
    for key in ["model", "tools", "system", "thinking", "stream"] {
        assert_eq!(
            unmarked(&compaction[key]),
            unmarked(&next_turn[key]),
            "`{key}` must match the session's next request"
        );
    }
    let next_msgs = next_turn["messages"].as_array().unwrap();
    let comp_msgs = compaction["messages"].as_array().unwrap();
    assert_eq!(next_msgs.len(), history.len() + 1, "history + the next prompt");
    assert_eq!(
        comp_msgs.len(),
        history.len() + 1,
        "history + one appended instruction"
    );
    for i in 0..history.len() {
        assert_eq!(
            unmarked(&comp_msgs[i]),
            unmarked(&next_msgs[i]),
            "history message {i} must be sent exactly as the next turn sends it"
        );
    }
    let last = comp_msgs.last().unwrap();
    assert_eq!(last["role"], "user");
    let text = last["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("[Context compaction request"), "{text}");
    assert!(text.contains("Do not call any tools"));
    assert!(!text.contains("<conversation>"), "no flattened transcript");
    assert!(
        last["content"][0].get("cache_control").is_some(),
        "the instruction carries the tail marker, so the history before it is a cache hit"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn compaction_model_naming_the_session_model_still_counts_as_the_session_model() {
    let _guard = HomeGuard::new();
    let mut rt = Runtime::new().await.expect("runtime");
    rt.set_model("claude-sonnet-4-5".to_string());
    rt.set_compaction_model(Some("anthropic/claude-sonnet-4-5".to_string()));
    assert!(rt.compaction_reuses_session_model());
}

/// An explicitly different summarizer gets the old flattened request: one
/// user message carrying the `<conversation>` transcript, sent to the
/// configured compaction model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_explicitly_different_compaction_model_keeps_the_flattened_path() {
    let _guard = HomeGuard::new();
    let (url, hits, bodies) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let mut rt = Runtime::new().await.expect("runtime");
    rt.set_model("claude-sonnet-4-5".to_string());
    rt.set_compaction_model(Some("claude-haiku-4-5".to_string()));
    assert!(!rt.compaction_reuses_session_model());

    let events = drive_runtime_turn(&rt, "please remember the number 1729", false).await;
    let history = history_of(&events);
    synaps_cli::runtime::compaction::compact_conversation(&history, &rt, None)
        .await
        .expect("compaction succeeds");

    let bodies = bodies.lock().unwrap();
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2, "one turn + one compaction");
    let compaction: Value = serde_json::from_slice(&bodies[1]).unwrap();
    assert_eq!(compaction["model"], "claude-haiku-4-5");
    let msgs = compaction["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1, "one flattened user message");
    assert!(
        msgs[0].to_string().contains("<conversation>"),
        "the transcript is flattened into the single message"
    );
}
