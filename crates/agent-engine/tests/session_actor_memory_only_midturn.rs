//! A memory-only `Set` that lands while a turn is IN FLIGHT
//! (docs/memory-only-results.md, "Setting the globs on a live session").
//! The session's glob list is one shared handle read at each mark check, so
//! the Set reaches:
//! (a) the parent's next `pria_vault_get` call in the SAME turn;
//! (b) a worker spawned after the Set;
//! (c) a worker already running when the Set lands (its later call).
//! A real extension process (tests/fixtures/extension_fixture.py, mode
//! `memory-only`) returns the sentinel UNMARKED, so only the glob set
//! mid-turn can mark it. The provider stub HOLDS the parent's request after
//! the first worker starts, and that worker's first request, until the Set
//! is applied. Workers are reactive (`subagent_start`): each one's full
//! history is written to its archive under HOME when it ends, so the
//! placeholder there (and no sentinel byte anywhere under HOME) shows its
//! call was marked.
mod session_actor_common;
use session_actor_common::*;

use agent_engine::extensions::hooks::HookBus;
use agent_engine::extensions::manager::ExtensionManager;
use agent_engine::extensions::manifest::ExtensionManifest;
use agent_engine::EngineHost;
use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, LocalTransport, SessionCommand, SessionEventWire, SessionQuery,
    SessionSetting,
};
use serde_json::{json, Value};
use serial_test::serial;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What the fixture returns for any tool but `vault_fetch` (unmarked).
const SENTINEL: &str = "MEMONLY_GLOB_SENTINEL_9d2b9d2b9d2b9d2b";
/// The extension tool `pria_vault_get` by its wire name.
const VAULT: &str = "pria-desktop_pria_vault_get";

fn sse(events: Vec<Value>) -> &'static str {
    let mut out = String::new();
    for event in events {
        out.push_str("data: ");
        out.push_str(&event.to_string());
        out.push_str("\n\n");
    }
    Box::leak(out.into_boxed_str())
}

/// One assistant response calling `calls` (id, tool, input) in order.
fn tool_round(msg_id: &str, calls: &[(&str, &str, Value)]) -> &'static str {
    let usage = json!({"input_tokens": 10, "output_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0});
    let mut events = vec![json!({"type": "message_start", "message": {
        "id": msg_id, "type": "message", "role": "assistant", "content": [], "model": MODEL,
        "stop_reason": null, "stop_sequence": null, "usage": usage}})];
    for (index, (id, name, input)) in calls.iter().enumerate() {
        events.push(json!({"type": "content_block_start", "index": index,
            "content_block": {"type": "tool_use", "id": id, "name": name}}));
        events.push(json!({"type": "content_block_delta", "index": index,
            "delta": {"type": "input_json_delta", "partial_json": input.to_string()}}));
        events.push(json!({"type": "content_block_stop", "index": index}));
    }
    events.push(json!({"type": "message_delta", "delta": {"stop_reason": "tool_use", "stop_sequence": null},
        "usage": usage}));
    events.push(json!({"type": "message_stop"}));
    sse(events)
}

/// The `tool_use_id`s of every tool result in a request's messages.
fn tool_result_ids(request: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    for message in request["messages"].as_array().into_iter().flatten() {
        for block in message["content"].as_array().into_iter().flatten() {
            if block["type"] == "tool_result" {
                if let Some(id) = block["tool_use_id"].as_str() {
                    ids.push(id.to_owned());
                }
            }
        }
    }
    ids
}

/// The stub's answer, and whether to hold it until the Set is applied.
/// Workers are told apart by their system prompt; each conversation's step
/// by the tool results it holds so far (a finished worker's notice may land
/// in the parent's request, so never by the last message alone).
fn script(body: &str) -> (&'static str, bool) {
    let parsed: Value = serde_json::from_str(body).unwrap_or_default();
    let system = parsed["system"].to_string();
    let ids = tool_result_ids(&parsed);
    let has = |id: &str| ids.iter().any(|i| i == id);
    if system.contains("midturn-worker-a") {
        // (c) Already running when the Set lands: its first request is held.
        if has("toolu_w1") {
            (SSE_HI, false)
        } else {
            (tool_round("msg_wa", &[("toolu_w1", VAULT, json!({}))]), true)
        }
    } else if system.contains("midturn-worker-b") {
        // (b) Spawned after the Set.
        if has("toolu_w2") {
            (SSE_HI, false)
        } else {
            (tool_round("msg_wb", &[("toolu_w2", VAULT, json!({}))]), false)
        }
    } else if has("toolu_m3") {
        // The parent's answer, and any wake for a finished worker.
        (SSE_HI, false)
    } else if has("toolu_m2") {
        let start = json!({"task": "second-worker", "name": "worker-b", "system_prompt": "You are midturn-worker-b."});
        (tool_round("msg_m3", &[("toolu_m3", "subagent_start", start)]), false)
    } else if has("toolu_m1") {
        // (a) The parent's next call in the same turn: held until the Set.
        (tool_round("msg_m2", &[("toolu_m2", VAULT, json!({}))]), true)
    } else if ids.is_empty() && parsed["messages"].to_string().contains("mid-turn") {
        let start = json!({"task": "worker-fetch", "name": "worker-a", "system_prompt": "You are midturn-worker-a."});
        (tool_round("msg_m1", &[("toolu_m1", "subagent_start", start)]), false)
    } else {
        (SSE_HI, false)
    }
}

async fn query(t: &mut LocalTransport, id: u64, q: SessionQuery) -> serde_json::Value {
    t.send(SessionCommand::Query { id, query: q }).await.unwrap();
    let events = until(t, |e| matches!(e, SessionEventWire::QueryResult { id: got, .. } if *got == id)).await;
    match &events.last().unwrap().event {
        SessionEventWire::QueryResult { value, .. } => value.clone(),
        _ => unreachable!(),
    }
}

fn files_under(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files_under(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

fn holds_sentinel(bytes: &[u8]) -> bool {
    bytes.windows(SENTINEL.len()).any(|w| w == SENTINEL.as_bytes())
}

/// The FIRST streaming request that holds the result of `call_id`: the live
/// request right after the call.
fn request_after(bodies: &Arc<Mutex<Vec<String>>>, call_id: &str) -> Option<String> {
    bodies
        .lock()
        .unwrap()
        .iter()
        .filter(|b| b.contains("\"stream\":true"))
        .find(|b| {
            let parsed: Value = serde_json::from_str(b).unwrap_or_default();
            tool_result_ids(&parsed).iter().any(|id| id == call_id)
        })
        .cloned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn memory_only_set_mid_turn_marks_the_same_turn_and_workers() {
    let h = Home::new();
    std::fs::write(h.dir.path().join(".synaps-cli/config"), "context_management = off\n").unwrap();
    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let held = Arc::new(AtomicUsize::new(0));
    let (release_tx, release_rx) = tokio::sync::watch::channel(false);
    let (captured, held_by_stub) = (bodies.clone(), held.clone());
    let app = axum::Router::new().fallback(move |body: String| {
        let (captured, held, mut release) = (captured.clone(), held_by_stub.clone(), release_rx.clone());
        async move {
            use axum::response::IntoResponse;
            let streaming = body.contains("\"stream\":true");
            let (sse, hold) = script(&body);
            captured.lock().unwrap().push(body);
            if !streaming {
                return axum::Json(json!({"id":"summary","type":"message","role":"assistant","model":MODEL,"stop_reason":"end_turn","content":[{"type":"text","text":"summary"}],"usage":{"input_tokens":10,"output_tokens":5}})).into_response();
            }
            if hold && !*release.borrow() {
                held.fetch_add(1, Ordering::SeqCst);
                while !*release.borrow_and_update() {
                    if release.changed().await.is_err() {
                        break;
                    }
                }
            }
            ([("content-type", "text/event-stream")], sse).into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", format!("http://{addr}"));
    let host = host().await;
    // Workers start from the PROCESS host's registry (`spawn_runtime`), so
    // this test's host is installed as it: the extension tool loaded below
    // reaches the workers. The only test in this binary.
    assert!(EngineHost::install(Arc::clone(&host)).is_ok(), "precondition: no other process host");

    // A real extension process `pria-desktop` registering `pria_vault_get`,
    // in the host registry the session's workers start from.
    let fx = h.dir.path().join("pria-desktop");
    std::fs::create_dir_all(&fx).unwrap();
    let tools_json = fx.join("tools.json");
    std::fs::write(
        &tools_json,
        serde_json::to_vec(&json!([
            {"name": "pria_vault_get", "description": "get a vault record", "input_schema": {"type": "object"}},
        ]))
        .unwrap(),
    )
    .unwrap();
    let script_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/extension_fixture.py");
    let manifest: ExtensionManifest = serde_json::from_value(json!({
        "runtime": "process",
        "command": "python3",
        "args": [
            script_path.display().to_string(),
            fx.join("spy.log").display().to_string(),
            tools_json.display().to_string(),
            "memory-only",
        ],
        "permissions": ["tools.register"],
    }))
    .unwrap();
    let mut extensions = ExtensionManager::new_with_tools(Arc::new(HookBus::new()), Arc::clone(&host.parts().tools));
    extensions.load("pria-desktop", &manifest).await.unwrap();
    assert!(
        host.parts().tools.read().await.get("pria-desktop:pria_vault_get").is_some(),
        "precondition: the extension tool is registered"
    );

    // An existing session with no memory-only globs.
    let mut config = cfg();
    config.persist = true;
    assert!(config.memory_only_tools.is_empty(), "precondition: no globs at create");
    let handle = host.create_session(config).await.unwrap();
    let (mut t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui)).await.unwrap();

    // ── One turn: worker A starts, then the stub holds the parent's next
    // request and worker A's first one.
    t.send(submit("mid-turn")).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while held.load(Ordering::SeqCst) < 2 {
        assert!(tokio::time::Instant::now() < deadline, "precondition: the parent and worker A reach the stub");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // ── The Set lands mid-turn (the turn is in flight, worker A running).
    t.send(SessionCommand::Set {
        id: 1,
        setting: SessionSetting::MemoryOnlyTools { globs: vec!["pria_vault_*".into(), "pria_memory_*".into()] },
    })
    .await
    .unwrap();
    let mut seen = until(&mut t, |e| matches!(e, SessionEventWire::SettingChanged(a) if a.id == 1)).await;
    match &seen.last().unwrap().event {
        SessionEventWire::SettingChanged(applied) => {
            assert!(applied.ok, "the mid-turn set was refused: {:?}", applied.message);
        }
        _ => unreachable!(),
    }
    assert!(
        !seen.iter().any(|e| matches!(e.event, SessionEventWire::Idle)),
        "precondition: the Set landed while the turn was in flight"
    );
    release_tx.send(true).unwrap();
    seen.extend(until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await);

    // ── (a) The same turn's later call is memory-only: flagged on the wire,
    // the placeholder in the history, the raw only in the live request.
    let wire: Vec<String> = seen
        .iter()
        .filter_map(|e| agent_engine::session::wire::WireEnvelope::for_client(e.clone()))
        .map(|e| serde_json::to_string(&e).unwrap())
        .collect();
    assert!(
        wire.iter().any(|w| w.contains("\"toolu_m2\"") && w.contains("\"memory_only\":true")),
        "(a) the call after the mid-turn Set is flagged memory-only"
    );
    assert!(wire.iter().all(|w| !w.contains(SENTINEL)), "an event carried a memory-only result");
    let after_m2 = request_after(&bodies, "toolu_m2").expect("a request after the parent's vault call");
    assert!(after_m2.contains(SENTINEL), "(a) the raw result reaches the live request");
    let messages = query(&mut t, 2, SessionQuery::MemoryOnly).await;
    assert_eq!(messages["live"], 1, "(a) the parent holds one live raw result");
    let messages = query(&mut t, 3, SessionQuery::Messages).await.to_string();
    assert!(!messages.contains(SENTINEL), "(a) the durable history holds a memory-only result");
    assert!(messages.contains("(call toolu_m2)"), "(a) the placeholder is kept instead");

    // ── (b), (c) Each worker's archive (its full history, on disk, written
    // when it ends) holds the placeholder, not the raw; the raw reached the
    // worker's own live request.
    let archives = h.dir.path().join(".synaps-cli/subagent-history");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let mut files = Vec::new();
        files_under(&archives, &mut files);
        let contents: Vec<(std::path::PathBuf, Vec<u8>)> =
            files.into_iter().filter_map(|p| std::fs::read(&p).ok().map(|b| (p, b))).collect();
        for (path, bytes) in &contents {
            assert!(!holds_sentinel(bytes), "a worker's memory-only result persisted to {}", path.display());
        }
        let has = |needle: &str| {
            contents.iter().any(|(_, b)| b.windows(needle.len()).any(|w| w == needle.as_bytes()))
        };
        let (a, b) = (has("(call toolu_w1)"), has("(call toolu_w2)"));
        if a && b {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "worker archives: (c) running worker marked={a}, (b) worker spawned after the Set marked={b}; files: {:?}",
            contents.iter().map(|(p, _)| p.display().to_string()).collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for call in ["toolu_w1", "toolu_w2"] {
        let after = request_after(&bodies, call).unwrap_or_else(|| panic!("{call}: a worker request after its vault call"));
        assert!(after.contains(SENTINEL), "{call}: the raw result reaches the worker's live request");
    }

    end(&mut t).await;
    extensions.shutdown_all().await;
    // Session files, journals, worker archives, the spy log: no raw byte.
    let mut files = Vec::new();
    files_under(h.dir.path(), &mut files);
    assert!(!files.is_empty());
    for path in files {
        let bytes = std::fs::read(&path).unwrap();
        assert!(!holds_sentinel(&bytes), "memory-only result persisted to {}", path.display());
    }
}
