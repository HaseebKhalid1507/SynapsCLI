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
    rig_with(h, "context_management.mode = off\n", |body| {
        ("text/event-stream", script(body))
    })
    .await
}

/// [`rig`] booted with `config` as the host's config file, the stub
/// answering each body with `respond`'s (content type, body).
async fn rig_with(
    h: &Home,
    config: &str,
    respond: fn(&str) -> (&'static str, String),
) -> (
    Arc<Mutex<Vec<String>>>,
    Arc<agent_engine::EngineHost>,
    ExtensionManager,
) {
    std::fs::write(h.dir.path().join(".synaps-cli/config"), config).unwrap();
    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured = bodies.clone();
    let app = axum::Router::new().fallback(move |body: String| {
        let captured = captured.clone();
        async move {
            use axum::response::IntoResponse;
            let (content_type, reply) = respond(&body);
            captured.lock().unwrap().push(body);
            ([("content-type", content_type)], reply).into_response()
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

// ── Context management ON ───────────────────────────────────────────────

/// A model that may summarize warm (`warm_summary::WARM_VETTED_MODELS`).
const AUTO_MODEL: &str = "claude-sonnet-4-6";

/// Marks the warm rolling-summary request: the host instruction appended to
/// the closing window's recorded conversation request.
const WARM_MARK: &str = "[Host request at a context rollover";

/// A non-retryable in-stream error; the response ends after it.
const SSE_FORCED_ERROR: &str = "data: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"forced\"}}\n\n";

/// One streamed reply: `block` at index 0 (and `delta` on it), then `stop`.
/// Every count is reported, with a cache write: the transport evidence a
/// warm summary needs from a recorded request. Far below the estimate, so
/// it never anchors the footprint: thresholds act on the estimate alone.
fn sse_auto(block: serde_json::Value, delta: Option<serde_json::Value>, stop: &str) -> String {
    use serde_json::json;
    let usage = json!({
        "input_tokens": 10,
        "output_tokens": 5,
        "cache_creation_input_tokens": 20,
        "cache_read_input_tokens": 0,
    });
    let mut events = vec![
        json!({"type": "message_start", "message": {"id": "msg_a1", "type": "message",
            "role": "assistant", "content": [], "model": AUTO_MODEL, "stop_reason": null,
            "stop_sequence": null, "usage": usage}}),
        json!({"type": "content_block_start", "index": 0, "content_block": block}),
    ];
    events.extend(delta.map(|d| json!({"type": "content_block_delta", "index": 0, "delta": d})));
    events.push(json!({"type": "content_block_stop", "index": 0}));
    events.push(json!({"type": "message_delta",
        "delta": {"stop_reason": stop, "stop_sequence": null}, "usage": usage}));
    events.push(json!({"type": "message_stop"}));
    events.iter().map(|e| format!("data: {e}\n\n")).collect()
}

fn sse_auto_text(text: &str) -> String {
    sse_auto(
        serde_json::json!({"type": "text", "text": ""}),
        Some(serde_json::json!({"type": "text_delta", "text": text})),
        "end_turn",
    )
}

/// The context-managed stand-in. A summary echoes the sentinel when its
/// request carried it, as a model would: the summary a session keeps shows
/// whether its request held the result.
fn script_auto(body: &str) -> (&'static str, String) {
    let echo = if body.contains(SENTINEL) {
        SENTINEL
    } else {
        "clean"
    };
    if body.contains(WARM_MARK) {
        let text = format!("## Window 1\nWARM-SUMMARY {echo}");
        return ("text/event-stream", sse_auto_text(&text));
    }
    if !body.contains("\"stream\":true") {
        // The cold (screened) summary is not streamed.
        let reply = serde_json::json!({
            "id": "msg_c1", "type": "message", "role": "assistant", "model": AUTO_MODEL,
            "content": [{"type": "text", "text": format!("## Window 1\nCOLD-SUMMARY {echo}")}],
            "stop_reason": "end_turn", "stop_sequence": null,
            "usage": {"input_tokens": 3000, "output_tokens": 120},
        });
        return ("application/json", reply.to_string());
    }
    if body.contains(SENTINEL) {
        // The request the memory-only result feeds fails: no reply follows
        // the call, which stays the last assistant message.
        return ("text/event-stream", SSE_FORCED_ERROR.to_string());
    }
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let last = parsed["messages"]
        .as_array()
        .and_then(|m| m.last())
        .map(|m| m["content"].to_string())
        .unwrap_or_default();
    let reply = if last.contains("get-record") {
        let call = serde_json::json!({"type": "tool_use", "id": "toolu_ac1",
            "name": "pria-desktop_pria_vault_get"});
        sse_auto(call, None, "tool_use")
    } else {
        sse_auto_text("hi")
    };
    ("text/event-stream", reply)
}

/// Client-visible events, serialized.
fn wire_of(events: &[agent_engine::session::Envelope]) -> String {
    events
        .iter()
        .filter_map(|e| agent_engine::session::wire::WireEnvelope::for_client(e.clone()))
        .map(|e| serde_json::to_string(&e).unwrap())
        .collect()
}

/// A turn's notices and errors.
fn notices_and_errors(events: &[agent_engine::session::Envelope]) -> (Vec<String>, Vec<String>) {
    use agent_engine::{SessionEvent, StreamEvent};
    let (mut notices, mut errors) = (Vec::new(), Vec::new());
    for e in events {
        match &e.event {
            SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Notice(text))) => {
                notices.push(text.clone())
            }
            SessionEventWire::Stream(StreamEvent::Session(SessionEvent::Error(err))) => {
                errors.push(err.message.clone())
            }
            _ => {}
        }
    }
    (notices, errors)
}

/// `/<name> <arg>` on the session; the reply.
async fn engine(t: &mut LocalTransport, id: u64, name: &str, arg: &str) -> serde_json::Value {
    let command = SessionCommand::EngineCommand {
        id,
        name: name.into(),
        arg: arg.into(),
    };
    t.send(command).await.unwrap();
    let events = until(
        t,
        |e| matches!(e, SessionEventWire::QueryResult { id: got, .. } if *got == id),
    )
    .await;
    match &events.last().unwrap().event {
        SessionEventWire::QueryResult { value, .. } => value.clone(),
        _ => unreachable!(),
    }
}

/// Every file under `dir` (none when it is missing).
fn files_under(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flat_map(|entry| {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files_under(&path)
            } else {
                vec![path]
            }
        })
        .collect()
}

/// The last 1,500 characters of `text`.
fn tail(text: &str) -> String {
    let skip = text.chars().count().saturating_sub(1_500);
    text.chars().skip(skip).collect()
}

/// Context management ON (`context_management.mode = auto`): a memory-only
/// result still live when the window rolls over stays out of the warm
/// summary record (the request the warm summary reuses), every summary
/// request, the summary the session keeps and the archive. Turn 2 calls
/// `pria_vault_get`; the request the raw result feeds fails in-stream, so
/// the call stays the last assistant message and the record of the request
/// before the call covers everything the rollover drops. Turn 3 starts over
/// a lowered rollover threshold with the result still live: the rollover
/// seals the archive, summarizes warm from that record and commits window
/// 2. A substituted request is never recorded (runtime/stream.rs,
/// `!memory_only_substituted`); one that was would replace that record with
/// one that has no transport evidence (a private round is never tapped), and
/// the summary would go cold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_live_memory_only_result_stays_out_of_the_warm_summary_and_the_rollover() {
    let h = Home::new();
    let (bodies, host, mut extensions) =
        rig_with(&h, "context_management.mode = auto\n", script_auto).await;
    let mut config = cfg();
    config.model_override = Some(AUTO_MODEL.into());
    config.persist = true;
    config.memory_only_tools = ik1_globs();
    let handle = host.create_session(config).await.unwrap();
    let (mut t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();

    // ── Turn 1: a large request (60k estimated tokens) for the rollover to drop.
    t.send(submit(&format!("background {}", "pad ".repeat(37_500))))
        .await
        .unwrap();
    let events = until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await;
    let (notices, errors) = notices_and_errors(&events);
    assert!(errors.is_empty(), "turn 1: {errors:?} {notices:?}");

    // ── Turn 2: the memory-only call. The raw result feeds the next request,
    // which fails: no reply follows the call.
    let from = bodies.lock().unwrap().len();
    t.send(submit("get-record")).await.unwrap();
    let events = until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await;
    let wire = wire_of(&events);
    assert_eq!(
        wire.matches("\"memory_only\":true").count(),
        1,
        "the call is memory-only"
    );
    assert!(
        !wire.contains(SENTINEL),
        "an event carried a memory-only result"
    );
    let (notices, errors) = notices_and_errors(&events);
    assert!(
        errors.iter().any(|e| e.contains("invalid_request_error")),
        "turn 2: {errors:?} {notices:?}"
    );
    let sent = streamed_since(&bodies, from);
    assert_eq!(sent.len(), 2, "the call, then the request the result feeds");
    assert!(!sent[0].contains(SENTINEL), "the result before its call");
    assert!(
        sent[1].contains(SENTINEL),
        "the raw result reaches the live request it feeds"
    );
    let fed = bodies.lock().unwrap().len();
    let messages = query(&mut t, 1, SessionQuery::Messages).await.to_string();
    assert!(
        !messages.contains(SENTINEL),
        "the durable history holds a memory-only result"
    );
    assert!(
        messages.contains("(call toolu_ac1)"),
        "the placeholder is kept: {}",
        tail(&messages)
    );
    let held = query(&mut t, 2, SessionQuery::MemoryOnly).await;
    assert_eq!(held["live"], 1, "the result is still live: {held}");

    // ── Lower the rollover threshold under the footprint (session-only).
    let assessment = query(&mut t, 3, SessionQuery::ContextAssessment).await;
    let used = assessment["used_tokens"].as_u64().unwrap();
    assert!(used > 60_000, "the large request counts: {assessment}");
    let rollover = used - 1_000;
    let reply = engine(
        &mut t,
        4,
        "context",
        &format!("auto {} {rollover}", rollover - 1),
    )
    .await;
    assert_eq!(reply["kind"], "output", "{reply}");

    // ── Turn 3 rolls over before its first request, the result still live:
    // the archive is sealed, the summary is warm from the record of the
    // request before the call, window 2 is committed, then the turn's
    // request goes out on the successor.
    t.send(submit("next step")).await.unwrap();
    let events = until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await;
    let (notices, errors) = notices_and_errors(&events);
    assert!(errors.is_empty(), "turn 3: {errors:?} {notices:?}");
    assert!(
        notices.iter().any(
            |n| n.contains("Continued automatically in context window 2")
                && n.contains("Dropped messages were folded into the rolling summary.")
        ),
        "the rollover committed with the summary: {notices:?}"
    );
    let status = engine(&mut t, 5, "context", "status").await;
    let status = status["text"].as_str().unwrap_or_default();
    assert!(
        status.contains("| window 2 |") && !status.contains("| archive none"),
        "{status}"
    );
    let after = bodies.lock().unwrap()[fed..].to_vec();
    let warm: Vec<&String> = after.iter().filter(|b| b.contains(WARM_MARK)).collect();
    let cold = after
        .iter()
        .filter(|b| !b.contains("\"stream\":true"))
        .count();
    assert_eq!(
        warm.len(),
        1,
        "one warm summary ({cold} cold, {} requests)",
        after.len()
    );
    assert!(
        warm[0].contains("get-record") && warm[0].contains("pad pad"),
        "the warm summary reuses the recorded request before the call"
    );
    for body in &after {
        assert!(
            !body.contains(SENTINEL),
            "a request after the fed one carried the result"
        );
    }
    assert!(
        after
            .iter()
            .any(|b| !b.contains(WARM_MARK) && b.contains("next step")),
        "the turn's request went out on the successor"
    );
    let messages = query(&mut t, 6, SessionQuery::Messages).await.to_string();
    assert!(
        messages.contains("WARM-SUMMARY clean"),
        "the kept summary: {}",
        tail(&messages)
    );
    assert!(
        !messages.contains(SENTINEL),
        "the kept history holds a memory-only result"
    );
    let held = query(&mut t, 7, SessionQuery::MemoryOnly).await;
    assert_eq!(
        held["live"], 0,
        "the rollover drops the held result: {held}"
    );
    let archived = files_under(&h.dir.path().join(".synaps-cli/context-archives"));
    assert!(
        archived
            .iter()
            .any(|p| std::fs::read_to_string(p).is_ok_and(|s| s.contains("get-record"))),
        "the rollover archived window 1: {archived:?}"
    );
    end(&mut t).await;
    extensions.shutdown_all().await;
    // Archives, session files, journals, the spy log: nothing holds the result.
    scan(h.dir.path());
}
