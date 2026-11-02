mod session_actor_common;
use session_actor_common::*;
use agent_engine::session::{ClientKind, ClientMeta, ClientTransport, LocalTransport, SessionCommand, SessionEventWire, SessionQuery, CheckpointReason};
use agent_engine::runtime::ephemeral::Operation;
use std::sync::{Arc, Mutex};
use serial_test::serial;

async fn query(t: &mut LocalTransport, id: u64, q: SessionQuery) -> serde_json::Value {
    t.send(SessionCommand::Query { id, query: q }).await.unwrap();
    reply(t, id).await
}
async fn reply(t: &mut LocalTransport, id: u64) -> serde_json::Value {
    let events = until(t, |e| matches!(e, SessionEventWire::QueryResult { id: got, .. } if *got == id)).await;
    match &events.last().unwrap().event { SessionEventWire::QueryResult { value, .. } => value.clone(), _ => unreachable!() }
}
async fn set(t: &mut LocalTransport, id: u64, epoch: &str, generation: u64, operation: Operation) -> serde_json::Value {
    t.send_from_self(SessionCommand::EphemeralLayer { id, epoch: epoch.into(), generation, operation }).await.unwrap();
    reply(t, id).await
}
fn scan(dir: &std::path::Path, sentinel: &str) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap(); let path = entry.path();
        if path.is_dir() { scan(&path, sentinel); }
        else if path.is_file() {
            let bytes = std::fs::read(&path).unwrap();
            assert!(!bytes.windows(sentinel.len()).any(|w| w == sentinel.as_bytes()), "payload persisted to {}", path.display());
        }
    }
}

/// One end-to-end no-disk sentinel: request inclusion, clear, ownership,
/// stale generations, compaction exclusion, reload serialization and all files.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn ephemeral_layer_no_disk_sentinel() {
    const SENTINEL: &str = "EPHEMERAL_LAYER_SENTINEL_9a71_DO_NOT_PERSIST";
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
            captured.lock().unwrap().push(body);
            if streaming { ([("content-type", "text/event-stream")], SSE_HI).into_response() }
            else { axum::Json(serde_json::json!({"id":"summary","type":"message","role":"assistant","model":MODEL,"stop_reason":"end_turn","content":[{"type":"text","text":"summary"}],"usage":{"input_tokens":10,"output_tokens":5}})).into_response() }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", format!("http://{addr}"));
    let host = host().await;
    let mut config = cfg(); config.persist = true; config.ephemeral_layer_wait_ms = Some(50);
    config.compaction_policy = agent_engine::session::CompactionPolicyWire::LinkedSuccessor;
    let handle = host.create_session(config).await.unwrap();
    let (mut t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui)).await.unwrap();
    let status = query(&mut t, 1, SessionQuery::EphemeralLayer).await;
    assert_eq!(status["state"], "needs_supply");
    let epoch = status["epoch"].as_str().unwrap().to_owned();
    let command = SessionCommand::EphemeralLayer { id: 2, epoch: epoch.clone(), generation: 1, operation: Operation::Replace { text: SENTINEL.into() } };
    assert!(!format!("{command:?}").contains(SENTINEL));
    // Explicit transport round trip must preserve text without Debug echo.
    let wire = serde_json::to_vec(&command).unwrap();
    let command: SessionCommand = serde_json::from_slice(&wire).unwrap();
    t.send_from_self(command).await.unwrap();
    assert_eq!(reply(&mut t, 2).await["outcome"], "applied");
    assert_eq!(set(&mut t, 3, &epoch, 1, Operation::Clear).await["outcome"], "stale_generation");
    t.send(SessionCommand::EngineCommand { id: 30, name: "trace".into(), arg: "next content".into() }).await.unwrap();
    let _ = reply(&mut t, 30).await;
    t.send(submit("first")).await.unwrap();
    let events = until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(!format!("{events:?}").contains(SENTINEL));
    assert!(bodies.lock().unwrap()[0].contains(SENTINEL));
    assert!(!query(&mut t, 4, SessionQuery::View).await.to_string().contains(SENTINEL));
    assert!(!query(&mut t, 5, SessionQuery::PromptInspection).await.to_string().contains(SENTINEL));
    assert!(!query(&mut t, 6, SessionQuery::Messages).await.to_string().contains(SENTINEL));
    // A fork gets no layer; request text cannot arrive through Runtime::clone/ForkSeed.
    t.send_from_self(SessionCommand::Fork { label: "ephemeral-child".into() }).await.unwrap();
    let events = until(&mut t, |e| matches!(e, SessionEventWire::Forked { .. } | SessionEventWire::Refused { .. })).await;
    assert!(!format!("{events:?}").contains(SENTINEL));
    let child_id = match &events.last().unwrap().event {
        SessionEventWire::Forked { session_id, .. } => session_id.clone(),
        other => panic!("fork failed: {other:?}"),
    };
    let child = host.attach(&agent_engine::session::SessionId::from(child_id.as_str())).unwrap();
    let (mut child, _) = LocalTransport::attach(child, ClientMeta::new(ClientKind::Tui)).await.unwrap();
    assert_eq!(query(&mut child, 31, SessionQuery::EphemeralLayer).await["state"], "cleared");
    child.send(submit("child")).await.unwrap();
    until(&mut child, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(!bodies.lock().unwrap().last().unwrap().contains(SENTINEL));
    // Compaction is a real summary request while the parent layer is installed.
    t.send(SessionCommand::Compact { instructions: None }).await.unwrap();
    let events = until(&mut t, |e| matches!(e, SessionEventWire::CompactionApplied { .. } | SessionEventWire::CompactionFailed { .. })).await;
    assert!(matches!(events.last().unwrap().event, SessionEventWire::CompactionApplied { .. }));
    assert!(!bodies.lock().unwrap().last().unwrap().contains(SENTINEL));
    let after = query(&mut t, 32, SessionQuery::EphemeralLayer).await;
    assert_eq!(after["state"], "needs_supply");
    assert_ne!(after["epoch"], epoch);
    let epoch = after["epoch"].as_str().unwrap().to_owned();
    assert_eq!(set(&mut t, 7, &epoch, 2, Operation::Clear).await["outcome"], "applied");
    t.send(submit("second")).await.unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(!bodies.lock().unwrap().last().unwrap().contains(SENTINEL));
    // Reinstall before checkpoint: prove reload serialization excludes a LIVE layer.
    assert_eq!(set(&mut t, 33, &epoch, 3, Operation::Replace { text: SENTINEL.into() }).await["outcome"], "applied");
    t.send(SessionCommand::Checkpoint { reason: CheckpointReason::Reload, deadline: None, skip_quiesce_grace: false }).await.unwrap();
    let events = until(&mut t, |e| matches!(e, SessionEventWire::QueryResult { id, .. } if *id == agent_engine::session::wire::CHECKPOINT_QUERY_ID)).await;
    assert!(!format!("{events:?}").contains(SENTINEL));
    let value = match &events.last().unwrap().event {
        SessionEventWire::QueryResult { value, .. } => value.clone(), _ => unreachable!(),
    };
    let record: agent_engine::session::SessionReloadRecord = serde_json::from_value(value["record"].clone()).unwrap();
    let encoded = serde_json::to_vec(&record).unwrap();
    assert!(!encoded.windows(SENTINEL.len()).any(|w| w == SENTINEL.as_bytes()));
    assert!(!format!("{:?}", record.settings_replay).contains(SENTINEL));
    std::fs::write(h.dir.path().join("reload-proof.json"), &encoded).unwrap();
    // Simulate the reload-created actor using the actual serialized metadata.
    // New runtime has no access to the prior Layer allocation.
    let mut restored = record.config;
    restored.continue_session = None;
    restored.name = None;
    let restored = host.create_session(restored).await.unwrap();
    let (mut restored, _) = LocalTransport::attach(restored, ClientMeta::new(ClientKind::Tui)).await.unwrap();
    let status = query(&mut restored, 34, SessionQuery::EphemeralLayer).await;
    assert_eq!(status["state"], "needs_supply");
    assert_ne!(status["epoch"], epoch);
    restored.send(submit("restored without resupply")).await.unwrap();
    until(&mut restored, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(!bodies.lock().unwrap().last().unwrap().contains(SENTINEL));
    assert_eq!(query(&mut restored, 35, SessionQuery::EphemeralLayer).await["state"], "missing");
    // Includes session JSON, journal/snapshots, trace/log artifacts and any context archives.
    scan(h.dir.path(), SENTINEL);
}
