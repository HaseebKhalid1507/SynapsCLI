//! Memory-only tool results (docs/memory-only-results.md): the real-path
//! no-disk sentinel. A real extension process (tests/fixtures/
//! extension_fixture.py, mode `memory-only`) makes both sentinels itself;
//! one is marked by `_meta["synaps/memory_only"]`, the other by the
//! session's glob. Each must reach the live provider request and nothing
//! else: no event, query, fork, summary request, reload record, restored
//! request, trace or file.
mod session_actor_common;
use session_actor_common::*;

use agent_engine::extensions::hooks::HookBus;
use agent_engine::extensions::manager::ExtensionManager;
use agent_engine::extensions::manifest::ExtensionManifest;
use agent_engine::session::{
    CheckpointReason, ClientKind, ClientMeta, ClientTransport, LocalTransport, SessionCommand,
    SessionEventWire, SessionQuery,
};
use serial_test::serial;
use std::sync::{Arc, Mutex};

/// What the fixture returns for `vault_fetch` (`_meta`-marked).
const META_SENTINEL: &str = "MEMONLY_META_SENTINEL_5c1e5c1e5c1e5c1e";
/// What the fixture returns for `notes` (unmarked; the session glob marks it).
const GLOB_SENTINEL: &str = "MEMONLY_GLOB_SENTINEL_9d2b9d2b9d2b9d2b";
/// Every placeholder starts with this.
const PLACEHOLDER: &str = "[memory-only result of ";

/// One `tool_use` block (serial tool site).
const SSE_TOOL_ONE: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_m1\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_mo1\",\"name\":\"memfix_vault_fetch\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// Two `tool_use` blocks in one response (parallel tool site).
const SSE_TOOL_TWO: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_m2\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_mo2\",\"name\":\"memfix_notes\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":1,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_mo3\",\"name\":\"memfix_vault_fetch\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// One `tool_use` block for the reload leg.
const SSE_TOOL_FOUR: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_m4\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_mo4\",\"name\":\"memfix_vault_fetch\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// The stub's answer, chosen by the request's LAST message only.
fn script(body: &str) -> &'static str {
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let last = parsed["messages"]
        .as_array()
        .and_then(|m| m.last())
        .map(|m| m["content"].to_string())
        .unwrap_or_default();
    if last.contains("toolu_mo1") {
        SSE_TOOL_TWO
    } else if last.contains("fetch-one") {
        SSE_TOOL_ONE
    } else if last.contains("fetch-four") {
        SSE_TOOL_FOUR
    } else {
        SSE_HI
    }
}

async fn query(t: &mut LocalTransport, id: u64, q: SessionQuery) -> serde_json::Value {
    t.send(SessionCommand::Query { id, query: q }).await.unwrap();
    reply(t, id).await
}

async fn reply(t: &mut LocalTransport, id: u64) -> serde_json::Value {
    let events = until(t, |e| matches!(e, SessionEventWire::QueryResult { id: got, .. } if *got == id)).await;
    match &events.last().unwrap().event {
        SessionEventWire::QueryResult { value, .. } => value.clone(),
        _ => unreachable!(),
    }
}

fn holds(text: &str) -> bool {
    text.contains(META_SENTINEL) || text.contains(GLOB_SENTINEL)
}

fn scan(dir: &std::path::Path) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            scan(&path);
        } else if path.is_file() {
            let bytes = std::fs::read(&path).unwrap();
            for sentinel in [META_SENTINEL, GLOB_SENTINEL] {
                assert!(
                    !bytes.windows(sentinel.len()).any(|w| w == sentinel.as_bytes()),
                    "memory-only result persisted to {}",
                    path.display()
                );
            }
        }
    }
}

/// Every log line the process emits, at TRACE for every target.
#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
    type Writer = LogCapture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn memory_only_no_disk_sentinel() {
    // The only test in this binary, so the one global subscriber: every
    // thread's events, every target, TRACE.
    let logs = LogCapture::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(logs.clone())
            .finish(),
    )
    .expect("the only subscriber in this test binary");
    let h = Home::new();
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
            let streaming = body.contains("\"stream\":true");
            let sse = script(&body);
            captured.lock().unwrap().push(body);
            if streaming {
                ([("content-type", "text/event-stream")], sse).into_response()
            } else {
                axum::Json(serde_json::json!({"id":"summary","type":"message","role":"assistant","model":MODEL,"stop_reason":"end_turn","content":[{"type":"text","text":"summary"}],"usage":{"input_tokens":10,"output_tokens":5}})).into_response()
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", format!("http://{addr}"));
    let host = host().await;

    // A real extension process, `memfix`, on the host's own registry.
    let fx = h.dir.path().join("memfix");
    std::fs::create_dir_all(&fx).unwrap();
    let tools_json = fx.join("tools.json");
    std::fs::write(
        &tools_json,
        serde_json::to_vec(&serde_json::json!([
            {"name": "vault_fetch", "description": "fetch a vault record", "input_schema": {"type": "object"}},
            {"name": "notes", "description": "read notes", "input_schema": {"type": "object"}},
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
    extensions.load("memfix", &manifest).await.unwrap();
    assert!(host.parts().tools.read().await.get("memfix:vault_fetch").is_some(), "precondition: the extension tool is registered");

    let mut config = cfg();
    config.persist = true;
    config.compaction_policy = agent_engine::session::CompactionPolicyWire::LinkedSuccessor;
    // The glob in the extension's runtime-name form (`plugin:tool`).
    config.memory_only_tools = vec!["memfix:notes".into()];
    let handle = host.create_session(config).await.unwrap();
    let (mut t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui)).await.unwrap();

    // ── Turn 1: `_meta` result on the serial site, then the glob result and
    // a second `_meta` result on the parallel site.
    let from = bodies.lock().unwrap().len();
    t.send(submit("fetch-one")).await.unwrap();
    let events = until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(!holds(&format!("{events:?}")), "an event carried a memory-only result");
    let wire: String = events
        .iter()
        .filter_map(|e| agent_engine::session::wire::WireEnvelope::for_client(e.clone()))
        .map(|e| serde_json::to_string(&e).unwrap())
        .collect();
    assert!(!holds(&wire));
    assert_eq!(wire.matches("\"memory_only\":true").count(), 3, "each placeholder is flagged on the wire");
    let sent = streamed_since(&bodies, from);
    assert_eq!(sent.len(), 3, "prompt, after the first call, after the batch");
    assert!(!holds(&sent[0]));
    assert!(sent[1].contains(META_SENTINEL), "the `_meta` result reaches the live request");
    assert!(!sent[1].contains("(call toolu_mo1)"), "swapped, not the placeholder");
    assert!(sent[2].contains(META_SENTINEL) && sent[2].contains(GLOB_SENTINEL), "the glob result reaches the live request");
    let status = query(&mut t, 1, SessionQuery::MemoryOnly).await;
    assert_eq!(status["kind"], "memory_only");
    assert_eq!(status["live"], 3);
    assert_eq!(status["live_bytes"], 3 * META_SENTINEL.len());
    assert_eq!(status["refused"], 0);
    assert!(!holds(&status.to_string()));
    let messages = query(&mut t, 2, SessionQuery::Messages).await.to_string();
    assert!(!holds(&messages), "the durable history holds a memory-only result");
    for call in ["toolu_mo1", "toolu_mo2", "toolu_mo3"] {
        assert!(messages.contains(&format!("(call {call})")), "placeholder for {call}");
    }
    assert!(messages.contains(PLACEHOLDER));
    assert!(!holds(&query(&mut t, 3, SessionQuery::View).await.to_string()));
    assert!(!holds(&query(&mut t, 4, SessionQuery::PromptInspection).await.to_string()));

    // ── Turn 2 under `/trace next content`: still live in the request; the
    // substituted request is never traced.
    t.send(SessionCommand::EngineCommand { id: 5, name: "trace".into(), arg: "next content".into() }).await.unwrap();
    let _ = reply(&mut t, 5).await;
    let from = bodies.lock().unwrap().len();
    t.send(submit("again")).await.unwrap();
    let events = until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(!holds(&format!("{events:?}")));
    let sent = streamed_since(&bodies, from);
    assert_eq!(sent.len(), 1);
    assert!(sent[0].contains(META_SENTINEL) && sent[0].contains(GLOB_SENTINEL));

    // ── A fork gets the placeholders and no raw text.
    t.send_from_self(SessionCommand::Fork { label: "memory-only-child".into() }).await.unwrap();
    let events = until(&mut t, |e| matches!(e, SessionEventWire::Forked { .. } | SessionEventWire::Refused { .. })).await;
    assert!(!holds(&format!("{events:?}")));
    let child_id = match &events.last().unwrap().event {
        SessionEventWire::Forked { session_id, .. } => session_id.clone(),
        other => panic!("fork failed: {other:?}"),
    };
    let child = host.attach(&agent_engine::session::SessionId::from(child_id.as_str())).unwrap();
    let (mut child, _) = LocalTransport::attach(child, ClientMeta::new(ClientKind::Tui)).await.unwrap();
    assert_eq!(query(&mut child, 6, SessionQuery::MemoryOnly).await["live"], 0);
    let from = bodies.lock().unwrap().len();
    child.send(submit("child")).await.unwrap();
    until(&mut child, |e| matches!(e, SessionEventWire::Idle)).await;
    let sent = streamed_since(&bodies, from);
    assert_eq!(sent.len(), 1);
    assert!(!holds(&sent[0]), "the fork's request carries a memory-only result");
    assert!(sent[0].contains("(call toolu_mo1)"), "the fork sees the placeholder");

    // ── Compaction: the summary request never sees the raw text, and the
    // results do not survive it.
    let from = bodies.lock().unwrap().len();
    t.send(SessionCommand::Compact { instructions: None }).await.unwrap();
    let events = until(&mut t, |e| matches!(e, SessionEventWire::CompactionApplied { .. } | SessionEventWire::CompactionFailed { .. })).await;
    assert!(matches!(events.last().unwrap().event, SessionEventWire::CompactionApplied { .. }));
    assert!(!holds(&format!("{events:?}")));
    let summary_requests: Vec<String> = bodies.lock().unwrap()[from..].to_vec();
    assert!(!summary_requests.is_empty(), "precondition: a summary request was sent");
    assert!(summary_requests.iter().all(|b| !holds(b)), "the summary request carries a memory-only result");
    assert_eq!(query(&mut t, 7, SessionQuery::MemoryOnly).await["live"], 0);
    let from = bodies.lock().unwrap().len();
    t.send(submit("after compaction")).await.unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(streamed_since(&bodies, from).iter().all(|b| !holds(b)));

    // ── Reload while a result is live: the record holds none, and the
    // restored session gets none back (no replay).
    let from = bodies.lock().unwrap().len();
    t.send(submit("fetch-four")).await.unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await;
    let sent = streamed_since(&bodies, from);
    assert_eq!(sent.len(), 2);
    assert!(sent[1].contains(META_SENTINEL), "live again after a new call");
    assert_eq!(query(&mut t, 8, SessionQuery::MemoryOnly).await["live"], 1);
    let journal = query(&mut t, 9, SessionQuery::Status).await["session"].as_str().unwrap().to_owned();
    t.send(SessionCommand::Checkpoint { reason: CheckpointReason::Reload, deadline: None, skip_quiesce_grace: false }).await.unwrap();
    let events = until(&mut t, |e| matches!(e, SessionEventWire::QueryResult { id, .. } if *id == agent_engine::session::wire::CHECKPOINT_QUERY_ID)).await;
    assert!(!holds(&format!("{events:?}")));
    let value = match &events.last().unwrap().event {
        SessionEventWire::QueryResult { value, .. } => value.clone(),
        _ => unreachable!(),
    };
    let record: agent_engine::session::SessionReloadRecord = serde_json::from_value(value["record"].clone()).unwrap();
    let encoded = serde_json::to_vec(&record).unwrap();
    assert!(!holds(&String::from_utf8_lossy(&encoded)), "the reload record carries a memory-only result");
    assert_eq!(record.config.memory_only_tools, vec!["memfix:notes".to_string()], "the policy survives a reload");
    std::fs::write(h.dir.path().join("reload-proof.json"), &encoded).unwrap();
    end(&mut t).await;
    // The old actor is gone first, as it is when a reload's new image
    // starts: a create matches a still-live handle by journal id.
    tokio::time::timeout(std::time::Duration::from_secs(30), handle.closed())
        .await
        .expect("the ended session's actor exits");
    // The reload-created actor: same journal, a new process-lifetime store.
    let mut restored = record.config;
    restored.name = None;
    restored.await_extensions = false;
    restored.continue_session = Some(Some(journal));
    restored.continue_lookup = agent_engine::session::ContinueLookup::ExactJournalId;
    let restored = host.create_session(restored).await.unwrap();
    let (mut restored, _) = match LocalTransport::attach(restored, ClientMeta::new(ClientKind::Tui)).await {
        Ok(attached) => attached,
        Err(e) => {
            let text = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
            let notable: Vec<&str> =
                text.lines().filter(|l| (l.contains("WARN") || l.contains("ERROR")) && !holds(l)).collect();
            panic!("attach to the restored session failed: {e:?}\n{}", notable.join("\n"));
        }
    };
    assert_eq!(query(&mut restored, 10, SessionQuery::MemoryOnly).await["live"], 0);
    let messages = query(&mut restored, 11, SessionQuery::Messages).await.to_string();
    assert!(messages.contains("(call toolu_mo4)"), "the restored history keeps the placeholder");
    assert!(!holds(&messages));
    let from = bodies.lock().unwrap().len();
    restored.send(submit("restored")).await.unwrap();
    until(&mut restored, |e| matches!(e, SessionEventWire::Idle)).await;
    let sent = streamed_since(&bodies, from);
    assert_eq!(sent.len(), 1);
    assert!(!holds(&sent[0]), "a restored session replayed a memory-only result");
    assert!(sent[0].contains("(call toolu_mo4)"));
    end(&mut restored).await;
    end(&mut child).await;
    extensions.shutdown_all().await;

    // Session files, journals, snapshots, trace bundles, logs, the spy log.
    scan(h.dir.path());
    // And no log line at any level.
    let logs = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
    assert!(!logs.is_empty(), "precondition: the log capture saw the run");
    assert!(!holds(&logs), "a log line carried a memory-only result");
}
