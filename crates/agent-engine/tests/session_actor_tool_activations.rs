//! A session's exact tool activations outlive its runtime: a daemon reload
//! (`Checkpoint{Reload}` record → the next image's create) and a park →
//! unpark both rebuild the runtime, and the conversation keeps the tools it
//! activated — without re-activation, and with the same tools array at the
//! start of the cached prompt prefix.
//!
//! In-process actor, loopback Anthropic SSE stub that records request
//! bodies, isolated HOME with progressive tool disclosure on.

mod session_actor_common;
use session_actor_common::*;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_engine::session::wire::CHECKPOINT_QUERY_ID;
use agent_engine::session::{
    CheckpointReason, ClientKind, ClientMeta, ClientTransport, EndReason, LocalTransport,
    SessionCommand, SessionConfig, SessionEventWire, SessionHandle, SessionLifecycle,
    SessionReloadRecord,
};
use agent_engine::tools::activation::CarriedProvenance;
use axum::response::IntoResponse;
use serial_test::serial;

const ACTIVATE_STATUS: &str = concat!(
    r#"data: {"type":"message_start","message":{"id":"msg_a","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-5","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
    "\n\n",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_act","name":"activate_tools"}}"#,
    "\n\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"tools\":[\"builtin:subagent_status\"]}"}}"#,
    "\n\n",
    r#"data: {"type":"content_block_stop","index":0}"#,
    "\n\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}"#,
    "\n\n",
    r#"data: {"type":"message_stop"}"#,
    "\n\n",
);

const CALL_STATUS: &str = concat!(
    r#"data: {"type":"message_start","message":{"id":"msg_s","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-5","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
    "\n\n",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_status","name":"subagent_status"}}"#,
    "\n\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
    "\n\n",
    r#"data: {"type":"content_block_stop","index":0}"#,
    "\n\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}"#,
    "\n\n",
    r#"data: {"type":"message_stop"}"#,
    "\n\n",
);

type Bodies = Arc<Mutex<Vec<serde_json::Value>>>;

/// `stub_seq` that also records every request body.
async fn recording_stub(seq: &'static [&'static str]) -> (String, Bodies) {
    let bodies: Bodies = Default::default();
    let seen = Arc::clone(&bodies);
    let app = axum::Router::new().fallback(move |body: String| {
        let seen = Arc::clone(&seen);
        async move {
            let mut seen = seen.lock().unwrap();
            let i = seen.len();
            seen.push(serde_json::from_str(&body).unwrap_or_default());
            (
                axum::http::StatusCode::OK,
                [("content-type", "text/event-stream")],
                seq[i.min(seq.len() - 1)].to_string(),
            )
                .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), bodies)
}

struct Grace;
impl Grace {
    fn zero() -> Self {
        std::env::set_var("SYNAPS_DAEMON_PARK_GRACE_SECS", "0");
        Self
    }
}
impl Drop for Grace {
    fn drop(&mut self) {
        std::env::remove_var("SYNAPS_DAEMON_PARK_GRACE_SECS");
    }
}

fn progressive_home() -> Home {
    let h = Home::new();
    std::fs::write(
        h.dir.path().join(".synaps-cli").join("config"),
        "progressive_tool_disclosure = true\n",
    )
    .unwrap();
    h
}

fn pcfg() -> SessionConfig {
    SessionConfig {
        persist: true,
        ..cfg()
    }
}

async fn attach(handle: &SessionHandle) -> LocalTransport {
    LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap()
        .0
}

async fn record(
    handle: &SessionHandle,
    t: &mut LocalTransport,
    reason: CheckpointReason,
) -> SessionReloadRecord {
    handle
        .send(SessionCommand::Checkpoint { reason })
        .await
        .unwrap();
    let seen = until(
        t,
        |e| matches!(e, SessionEventWire::QueryResult { id, .. } if *id == CHECKPOINT_QUERY_ID),
    )
    .await;
    let SessionEventWire::QueryResult { value, .. } = &seen.last().unwrap().event else {
        unreachable!()
    };
    serde_json::from_value(value["record"].clone()).unwrap()
}

fn activated_ids(record: &SessionReloadRecord) -> Vec<String> {
    record
        .tool_activations
        .iter()
        .map(|c| c.tool_id.clone())
        .collect()
}

/// A turn whose model calls `subagent_status`; the event texts, for the
/// gate's verdict.
async fn call_status(t: &mut LocalTransport) -> String {
    t.send(submit("status?")).await.unwrap();
    let seen = until(t, |e| matches!(e, SessionEventWire::Idle)).await;
    seen.iter()
        .map(|e| format!("{:?}", e.event))
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_status_authorized(events: &str) {
    assert!(events.contains("toolu_status"), "the tool ran: {events}");
    assert!(
        !events.contains("not activated for this session"),
        "the carried activation must authorize without re-activation: {events}"
    );
}

/// Names of the non-deferred tools (the cached prefix) of a request body.
fn loaded(body: &serde_json::Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t.get("defer_loading").is_none())
        .filter_map(|t| t["name"].as_str().map(String::from))
        .collect()
}

/// A progressive session that activated `subagent_status` in its first turn.
async fn session_with_activation() -> (SessionHandle, LocalTransport, Bodies) {
    let (url, bodies) = recording_stub(&[ACTIVATE_STATUS, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(pcfg()).await.unwrap();
    let mut t = attach(&handle).await;
    t.send(submit("activate")).await.unwrap();
    let seen = until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await;
    let events = seen
        .iter()
        .map(|e| format!("{:?}", e.event))
        .collect::<String>();
    assert!(events.contains("toolu_act"), "activate_tools ran: {events}");
    assert!(
        !loaded(&bodies.lock().unwrap()[0]).contains(&"subagent_status".to_string()),
        "progressive: subagent_status starts deferred"
    );
    (handle, t, bodies)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn activations_survive_a_daemon_reload() {
    let _h = progressive_home();
    let (handle, mut t, _) = session_with_activation().await;

    // What `daemon reload` records for the session…
    let rec = record(&handle, &mut t, CheckpointReason::Reload).await;
    assert_eq!(
        activated_ids(&rec),
        vec!["builtin:subagent_status".to_string()]
    );
    assert_eq!(
        rec.tool_activations[0].provenance,
        CarriedProvenance::BuiltinRuntime
    );
    let journal = handle.journal_id();
    t.send(SessionCommand::End {
        reason: EndReason::ClientQuit,
    })
    .await
    .unwrap();
    while handle.is_alive() {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // …and how the next image rebuilds it (`reload::rehydrate`).
    let (url, bodies) = recording_stub(&[CALL_STATUS, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let reborn = host
        .create_session(SessionConfig {
            continue_session: Some(Some(journal)),
            ..rec.config
        })
        .await
        .unwrap();
    assert!(reborn.meta().continued);
    reborn
        .send(SessionCommand::RestoreToolActivations {
            activations: rec.tool_activations.clone(),
        })
        .await
        .unwrap();
    let mut t = attach(&reborn).await;

    // A second reload before any turn hands the same activations on.
    let again = record(&reborn, &mut t, CheckpointReason::HostRequest).await;
    assert_eq!(again.tool_activations, rec.tool_activations);

    assert_status_authorized(&call_status(&mut t).await);
    assert!(!bodies.lock().unwrap().is_empty());
    let after = record(&reborn, &mut t, CheckpointReason::HostRequest).await;
    assert_eq!(
        after.tool_activations, rec.tool_activations,
        "kept by the retained set"
    );
    end(&mut t).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn activations_survive_park_and_unpark() {
    let _h = progressive_home();
    let _g = Grace::zero();
    let (handle, t, _) = session_with_activation().await;

    t.send(SessionCommand::Detach {
        client: t.client_id(),
    })
    .await
    .unwrap();
    drop(t);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while handle.lifecycle() != SessionLifecycle::Parked {
        assert!(tokio::time::Instant::now() < deadline, "grace 0 → parked");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let (url, _) = recording_stub(&[CALL_STATUS, SSE_HI]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let mut t = attach(&handle).await; // unparks
    assert_status_authorized(&call_status(&mut t).await);
    let rec = record(&handle, &mut t, CheckpointReason::HostRequest).await;
    assert_eq!(
        activated_ids(&rec),
        vec!["builtin:subagent_status".to_string()]
    );
    end(&mut t).await;
}

/// The record of a session parked when the reload checkpoints it carries
/// the activations the dropped runtime held, plus any handed to it while
/// parked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_parked_sessions_reload_record_carries_its_activations() {
    let _h = progressive_home();
    let _g = Grace::zero();
    let (handle, t, _) = session_with_activation().await;
    t.send(SessionCommand::Detach {
        client: t.client_id(),
    })
    .await
    .unwrap();
    drop(t);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while handle.lifecycle() != SessionLifecycle::Parked {
        assert!(tokio::time::Instant::now() < deadline, "grace 0 → parked");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // A hand-off to a parked session (reload restores it Parked) is kept
    // for its unpark without waking it.
    let extra = agent_engine::tools::activation::CarriedActivation {
        tool_id: "builtin:subagent_steer".into(),
        schema_digest: "ef".repeat(32),
        provenance: CarriedProvenance::BuiltinRuntime,
    };
    handle
        .send(SessionCommand::RestoreToolActivations {
            activations: vec![extra],
        })
        .await
        .unwrap();
    let mut rx = handle.subscribe();
    handle
        .send(SessionCommand::Checkpoint {
            reason: CheckpointReason::Reload,
        })
        .await
        .unwrap();
    let rec: SessionReloadRecord = loop {
        let env = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("timely")
            .expect("alive");
        if let SessionEventWire::QueryResult { id, value } = env.event {
            if id == CHECKPOINT_QUERY_ID {
                assert_eq!(value["parked"], true, "answered without unparking");
                break serde_json::from_value(value["record"].clone()).unwrap();
            }
        }
    };
    assert_eq!(
        activated_ids(&rec),
        vec![
            "builtin:subagent_status".to_string(),
            "builtin:subagent_steer".to_string()
        ]
    );
    assert_eq!(handle.lifecycle(), SessionLifecycle::Parked);
}
