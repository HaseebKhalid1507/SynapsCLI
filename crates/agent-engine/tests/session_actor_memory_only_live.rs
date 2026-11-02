//! Memory-only tool globs set on a LIVE session (`Set{MemoryOnlyTools}`,
//! docs/memory-only-results.md). A session created with none starts
//! marking an extension tool's results, by the tool's own name, from the
//! next call. The live list is ADD-ONLY: an identical resend and a superset
//! succeed, a list that drops a glob is refused, counted and changes
//! nothing, and every reply echoes the effective list. A reload record
//! carries the list, and the restored session accepts the same list again.
//! A real extension process (tests/fixtures/extension_fixture.py, mode
//! `memory-only`) returns the sentinel UNMARKED, so only the live-set glob
//! can mark it. A `max_tool_output` set under the placeholder's length
//! withholds the result and says so.
mod session_actor_common;
use session_actor_common::*;

use agent_engine::extensions::hooks::HookBus;
use agent_engine::extensions::manager::ExtensionManager;
use agent_engine::extensions::manifest::ExtensionManifest;
use agent_engine::session::{
    CheckpointReason, ClientKind, ClientMeta, ClientTransport, LocalTransport, SessionCommand,
    SessionEventWire, SessionQuery, SessionSetting, SettingApplied,
};
use serial_test::serial;
use std::sync::{Arc, Mutex};

/// What the fixture returns for any tool but `vault_fetch` (unmarked).
const SENTINEL: &str = "MEMONLY_GLOB_SENTINEL_9d2b9d2b9d2b9d2b";

/// One call of the extension tool `pria_vault_get` by its wire name.
fn sse_get(call_id: &str) -> String {
    format!(
        concat!(
            "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_l1\",\"type\":\"message\",",
            "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
            "\"stop_sequence\":null,\"usage\":{{\"input_tokens\":10,\"output_tokens\":0,",
            "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}}}\n\n",
            "data: {{\"type\":\"content_block_start\",\"index\":0,",
            "\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{id}\",\"name\":\"pria-desktop_pria_vault_get\"}}}}\n\n",
            "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
            "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\",",
            "\"stop_sequence\":null}},\"usage\":{{\"input_tokens\":10,\"output_tokens\":5,",
            "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}\n\n",
            "data: {{\"type\":\"message_stop\"}}\n\n",
        ),
        id = call_id
    )
}

fn globs(names: &[&str]) -> Vec<String> {
    names.iter().map(|g| g.to_string()).collect()
}

/// The globs Pria Desktop sets for IK1.
fn ik1_globs() -> Vec<String> {
    globs(&["pria_vault_*", "pria_memory_*"])
}

/// IK1 plus one more: a superset.
fn wider_globs() -> Vec<String> {
    globs(&["pria_vault_*", "pria_memory_*", "pria_extra_*"])
}

/// The stub's answer, chosen by the request's LAST message only.
fn script(body: &str) -> String {
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let last = parsed["messages"]
        .as_array()
        .and_then(|m| m.last())
        .map(|m| m["content"].to_string())
        .unwrap_or_default();
    if last.contains("get-record-2") {
        sse_get("toolu_lv2")
    } else if last.contains("get-record") {
        sse_get("toolu_lv1")
    } else {
        SSE_HI.to_string()
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

/// `Set{MemoryOnlyTools}` from the owner; the reply.
async fn set(t: &mut LocalTransport, id: u64, globs: Vec<String>) -> SettingApplied {
    t.send(SessionCommand::Set { id, setting: SessionSetting::MemoryOnlyTools { globs } }).await.unwrap();
    let events = until(t, |e| matches!(e, SessionEventWire::SettingChanged(a) if a.id == id)).await;
    match &events.last().unwrap().event {
        SessionEventWire::SettingChanged(applied) => {
            assert_eq!(applied.setting, "memory_only_tools");
            applied.clone()
        }
        _ => unreachable!(),
    }
}

/// An accepted Set: `ok:true`, and the reply echoes the effective list.
async fn set_ok(t: &mut LocalTransport, id: u64, globs: Vec<String>, effective: Vec<String>) {
    let applied = set(t, id, globs).await;
    assert!(applied.ok, "the live set was refused: {:?}", applied.message);
    assert_eq!(applied.memory_only_tools, Some(effective), "the reply echoes the effective list");
}

/// A refused Set: `ok:false`, the message names every dropped glob, and the
/// reply echoes the list in force, unchanged.
async fn set_refused(t: &mut LocalTransport, id: u64, globs: Vec<String>, dropped: &[&str], effective: Vec<String>) {
    let applied = set(t, id, globs).await;
    assert!(!applied.ok, "a list that drops a glob was accepted");
    let message = applied.message.clone().unwrap_or_default();
    assert!(message.contains("add-only"), "{message}");
    for glob in dropped {
        assert!(message.contains(&format!("`{glob}`")), "the refusal names `{glob}`: {message}");
    }
    assert_eq!(applied.memory_only_tools, Some(effective), "the refusal echoes the unchanged list");
}

fn scan(dir: &std::path::Path) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            scan(&path);
        } else if path.is_file() {
            let bytes = std::fs::read(&path).unwrap();
            assert!(
                !bytes.windows(SENTINEL.len()).any(|w| w == SENTINEL.as_bytes()),
                "memory-only result persisted to {}",
                path.display()
            );
        }
    }
}

/// Streaming request bodies sent since `from`.
fn streamed_since(bodies: &Arc<Mutex<Vec<String>>>, from: usize) -> Vec<String> {
    bodies.lock().unwrap()[from..]
        .iter()
        .filter(|b| b.contains("\"stream\":true"))
        .cloned()
        .collect()
}

/// One turn that calls `pria_vault_get` once: the call is memory-only.
async fn marked_call(t: &mut LocalTransport, bodies: &Arc<Mutex<Vec<String>>>, prompt: &str, call_id: &str) {
    let from = bodies.lock().unwrap().len();
    t.send(submit(prompt)).await.unwrap();
    let events = until(t, |e| matches!(e, SessionEventWire::Idle)).await;
    let wire: String = events
        .iter()
        .filter_map(|e| agent_engine::session::wire::WireEnvelope::for_client(e.clone()))
        .map(|e| serde_json::to_string(&e).unwrap())
        .collect();
    assert_eq!(wire.matches("\"memory_only\":true").count(), 1, "{prompt}: the live-set glob marks the call");
    assert!(!wire.contains(SENTINEL), "an event carried a memory-only result");
    let sent = streamed_since(bodies, from);
    assert_eq!(sent.len(), 2, "prompt, then after the call");
    assert!(sent[1].contains(SENTINEL), "the raw result reaches the live request");
    let messages = query(t, 900, SessionQuery::Messages).await.to_string();
    assert!(!messages.contains(SENTINEL), "the durable history holds a memory-only result");
    assert!(messages.contains(&format!("(call {call_id})")), "the placeholder is kept instead");
}

/// A loopback provider stub that records every request body, a host, and
/// a real extension process `pria-desktop` registering `pria_vault_get`:
/// runtime name `pria-desktop:pria_vault_get`, wire name
/// `pria-desktop_pria_vault_get`. `pria_vault_*` matches neither; it
/// matches the tool's own name.
async fn rig(h: &Home) -> (Arc<Mutex<Vec<String>>>, Arc<agent_engine::EngineHost>, ExtensionManager) {
    std::fs::write(
        h.dir.path().join(".synaps-cli/config"),
        "context_management.mode = off\n",
    )
    .unwrap();
    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured = bodies.clone();
    let app = axum::Router::new().fallback(move |body: String| {
        let captured = captured.clone();
        async move {
            use axum::response::IntoResponse;
            let sse = script(&body);
            captured.lock().unwrap().push(body);
            ([("content-type", "text/event-stream")], sse).into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", format!("http://{addr}"));
    let host = host().await;

    let fx = h.dir.path().join("pria-desktop");
    std::fs::create_dir_all(&fx).unwrap();
    let tools_json = fx.join("tools.json");
    std::fs::write(
        &tools_json,
        serde_json::to_vec(&serde_json::json!([
            {"name": "pria_vault_get", "description": "get a vault record", "input_schema": {"type": "object"}},
        ]))
        .unwrap(),
    )
    .unwrap();
    let script_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/extension_fixture.py");
    let manifest: ExtensionManifest = serde_json::from_value(serde_json::json!({
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
    (bodies, host, extensions)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn memory_only_tools_set_on_a_live_session_mark_the_next_calls() {
    let h = Home::new();
    let (bodies, host, mut extensions) = rig(&h).await;

    // An existing session: created with no memory-only globs (and no
    // config-key globs, so the effective list is the session's).
    let mut config = cfg();
    config.persist = true;
    assert!(config.memory_only_tools.is_empty(), "precondition: no globs at create");
    let handle = host.create_session(config).await.unwrap();
    let (mut t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui)).await.unwrap();

    // ── The owner sets the IK1 globs on the live session; the next call
    // of `pria_vault_get` is memory-only.
    set_ok(&mut t, 1, ik1_globs(), ik1_globs()).await;
    marked_call(&mut t, &bodies, "get-record", "toolu_lv1").await;
    assert_eq!(query(&mut t, 2, SessionQuery::MemoryOnly).await["live"], 1);

    // ── Add-only. An identical resend: ok, nothing changes.
    set_ok(&mut t, 3, ik1_globs(), ik1_globs()).await;
    // A superset adds the new glob.
    set_ok(&mut t, 4, wider_globs(), wider_globs()).await;
    // Dropping any glob is refused, names it, and changes nothing.
    set_refused(&mut t, 5, ik1_globs(), &["pria_extra_*"], wider_globs()).await;
    set_refused(&mut t, 6, Vec::new(), &["pria_vault_*", "pria_memory_*", "pria_extra_*"], wider_globs()).await;
    let status = query(&mut t, 7, SessionQuery::MemoryOnly).await;
    assert_eq!(status["set_refused"], 2, "the memory_only query counts the refusals: {status}");
    // Unchanged: the held result is still live, and a new call still marks.
    assert_eq!(status["live"], 1);
    marked_call(&mut t, &bodies, "get-record-2", "toolu_lv2").await;
    assert_eq!(query(&mut t, 8, SessionQuery::MemoryOnly).await["live"], 2);

    // ── A reload record carries the union (no result text).
    let journal = query(&mut t, 9, SessionQuery::Status).await["session"].as_str().unwrap().to_owned();
    t.send(SessionCommand::Checkpoint { reason: CheckpointReason::Reload, deadline: None, skip_quiesce_grace: false }).await.unwrap();
    let events = until(&mut t, |e| matches!(e, SessionEventWire::QueryResult { id, .. } if *id == agent_engine::session::wire::CHECKPOINT_QUERY_ID)).await;
    let value = match &events.last().unwrap().event {
        SessionEventWire::QueryResult { value, .. } => value.clone(),
        _ => unreachable!(),
    };
    let record: agent_engine::session::SessionReloadRecord = serde_json::from_value(value["record"].clone()).unwrap();
    assert_eq!(record.config.memory_only_tools, wider_globs(), "the live-set globs survive a reload");
    assert!(!String::from_utf8_lossy(&serde_json::to_vec(&record).unwrap()).contains(SENTINEL));
    end(&mut t).await;
    tokio::time::timeout(std::time::Duration::from_secs(30), handle.closed())
        .await
        .expect("the ended session's actor exits");

    // ── The restored session (same journal, from the record) takes the
    // same list again: ok, idempotent, echoed.
    let mut restored = record.config;
    restored.name = None;
    restored.await_extensions = false;
    restored.continue_session = Some(Some(journal));
    restored.continue_lookup = agent_engine::session::ContinueLookup::ExactJournalId;
    let restored = host.create_session(restored).await.unwrap();
    let (mut restored, _) = LocalTransport::attach(restored, ClientMeta::new(ClientKind::Tui)).await.unwrap();
    set_ok(&mut restored, 10, wider_globs(), wider_globs()).await;
    assert_eq!(query(&mut restored, 11, SessionQuery::MemoryOnly).await["set_refused"], 0);
    end(&mut restored).await;
    extensions.shutdown_all().await;

    // Session files, journals, the spy log: nothing holds the result.
    scan(h.dir.path());
}

/// A `max_tool_output` under the placeholder's length: a truncated
/// placeholder can never be swapped back, so the call fails closed. It
/// must not be silent: the result says the memory-only result was withheld
/// because max_tool_output is too small (in the model's history and in the
/// user's event), nothing is held for it, and the raw text reaches nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_max_tool_output_under_the_placeholder_withholds_the_result_and_says_so() {
    let h = Home::new();
    let (bodies, host, mut extensions) = rig(&h).await;
    let mut config = cfg();
    config.persist = true;
    config.memory_only_tools = ik1_globs();
    let handle = host.create_session(config).await.unwrap();
    let (mut t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui)).await.unwrap();

    // 60 bytes: the placeholder (`[memory-only result of
    // pria-desktop_pria_vault_get (call toolu_lv1) is not kept ...`) is longer.
    let placeholder = agent_engine::runtime::memory_only::placeholder("pria-desktop_pria_vault_get", "toolu_lv1");
    assert!(placeholder.len() > 60, "precondition: the placeholder does not fit");
    t.send(SessionCommand::Set { id: 1, setting: SessionSetting::MaxToolOutput { bytes: 60 } }).await.unwrap();
    let events = until(&mut t, |e| matches!(e, SessionEventWire::SettingChanged(a) if a.id == 1)).await;
    match &events.last().unwrap().event {
        SessionEventWire::SettingChanged(applied) => assert!(applied.ok, "{:?}", applied.message),
        _ => unreachable!(),
    }

    let from = bodies.lock().unwrap().len();
    t.send(submit("get-record")).await.unwrap();
    let events = until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await;
    let wire: String = events
        .iter()
        .filter_map(|e| agent_engine::session::wire::WireEnvelope::for_client(e.clone()))
        .map(|e| serde_json::to_string(&e).unwrap())
        .collect();
    assert!(!wire.contains(SENTINEL), "an event carried a memory-only result");
    assert!(
        wire.contains("memory-only result withheld") && wire.contains("max_tool_output"),
        "the user's tool-result event says why: {wire}"
    );
    assert_eq!(wire.matches("\"memory_only\":true").count(), 1, "the notice is flagged memory-only");
    let sent = streamed_since(&bodies, from);
    assert_eq!(sent.len(), 2, "prompt, then after the call");
    assert!(!sent[1].contains(SENTINEL), "fails closed: the raw is never swapped in");
    assert!(sent[1].contains("[memory-only result withheld"), "the model sees why: {}", sent[1]);
    let status = query(&mut t, 2, SessionQuery::MemoryOnly).await;
    assert_eq!(status["live"], 0, "nothing is held for a result that can never be swapped: {status}");
    let messages = query(&mut t, 3, SessionQuery::Messages).await.to_string();
    assert!(!messages.contains(SENTINEL), "the durable history holds a memory-only result");
    assert!(messages.contains("[memory-only result withheld"), "the history keeps the notice: {messages}");
    end(&mut t).await;
    extensions.shutdown_all().await;
    scan(h.dir.path());
}
