//! Cache-aligned compaction (task #430): the summary request must reuse the
//! session's own prompt prefix (tools, system, model, thinking, history)
//! byte-for-byte and only append the instruction, so the history is read
//! from the prompt cache instead of being resent as one uncached flattened
//! transcript (S347: a 1.6 MB flattened compaction drew HTTP 429 on every
//! retry while the session's cached turns kept succeeding).

#[path = "support/phase2/mod.rs"]
mod support;

use serde_json::Value;
use serial_test::serial;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn compaction_reuses_the_session_prefix_and_appends_only_the_instruction() {
    let _guard = HomeGuard::new();
    let (url, hits, bodies) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let mut rt = Runtime::new().await.expect("runtime");
    rt.set_model("claude-sonnet-4-5".to_string());
    assert!(rt.compaction_reuses_session_model(), "no compaction_model configured");

    let events = drive_runtime_turn(&rt, "please remember the number 1729", false).await;
    let history = history_of(&events);
    assert!(history.len() >= 2, "user + assistant");

    let outcome =
        synaps_cli::runtime::compaction::compact_conversation(&history, &rt, None)
            .await
            .expect("compaction succeeds");
    assert_eq!(outcome.summary_text, "hi", "the stub's reply is the summary");

    let bodies = bodies.lock().unwrap();
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2, "one turn + one compaction");
    let turn: Value = serde_json::from_slice(&bodies[0]).unwrap();
    let compaction: Value = serde_json::from_slice(&bodies[1]).unwrap();

    // The cached prefix: tools → system → messages. Byte-identical up to the
    // moving tail markers.
    for key in ["model", "tools", "system", "thinking", "stream"] {
        assert_eq!(
            unmarked(&compaction[key]),
            unmarked(&turn[key]),
            "`{key}` must match the session's request"
        );
    }
    let turn_msgs = turn["messages"].as_array().unwrap();
    let comp_msgs = compaction["messages"].as_array().unwrap();
    assert_eq!(
        comp_msgs.len(),
        history.len() + 1,
        "history + one appended instruction"
    );
    for (i, m) in turn_msgs.iter().enumerate() {
        assert_eq!(unmarked(&comp_msgs[i]), unmarked(m), "history message {i} unchanged");
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
async fn an_explicitly_different_compaction_model_keeps_the_flattened_path() {
    let _guard = HomeGuard::new();
    let base = synaps_cli::config::base_dir();
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(
        base.join("config"),
        "model = claude-sonnet-4-5\ncompaction_model = claude-haiku-4-5\n",
    )
    .unwrap();
    let rt = Runtime::new().await.expect("runtime");
    assert!(!rt.compaction_reuses_session_model());
}
