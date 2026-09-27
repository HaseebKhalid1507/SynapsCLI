//! F24: `--continue <old>` follows `compacted_into` to the successor.
//!
//! Verifies:
//! 1. After LinkedSuccessor compaction, the predecessor has `compacted_into` set.
//! 2. In-process `--continue <old>` while the successor is daemon-live refuses
//!    naming the successor (lock or compaction-into error).
//! 3. After daemon stop, `--continue <old>` follows the chain to the successor.
//! 4. `SessionLock::try_acquire` on a compacted predecessor returns
//!    `CompactedInto` naming the successor.

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::sync::Arc;
use std::time::Duration;

use agent_engine::daemon::{Daemon, DaemonOpts};
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::{EngineHost, HostOpts};
use phase2::{HomeGuard, ANTHROPIC_SSE};
use serial_test::serial;

/// Stub that serves both streaming turns and the compaction summary.
async fn stub_compact() -> String {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let hits = Arc::new(AtomicUsize::new(0));
    let h = Arc::clone(&hits);
    let app = axum::Router::new().fallback(move |body: String| {
        let h = Arc::clone(&h);
        async move {
            h.fetch_add(1, Ordering::SeqCst);
            if body.contains("\"stream\":true") {
                return (
                    axum::http::StatusCode::OK,
                    [("content-type", "text/event-stream")],
                    ANTHROPIC_SSE.to_string(),
                )
                    .into_response();
            }
            // Non-streaming compaction call
            axum::Json(serde_json::json!({
                "id": "msg_c", "type": "message", "role": "assistant",
                "model": "claude-sonnet-4-5", "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "SUMMARY: the user said hello."}],
                "usage": {"input_tokens": 50, "output_tokens": 10}
            }))
            .into_response()
        }
    });
    use axum::response::IntoResponse;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn next(t: &mut SocketTransport) -> Envelope {
    tokio::time::timeout(Duration::from_secs(10), t.next_event())
        .await
        .expect("timely")
        .expect("open")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn continue_old_follows_compacted_into() {
    let guard = HomeGuard::new();
    let url = stub_compact().await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    // ── 1. Start daemon with one session ──
    let host: Arc<EngineHost> =
        EngineHost::boot_and_install(HostOpts { profile: None, no_extensions: true })
            .await
            .expect("host boot");
    let d = Daemon::start(
        host.clone(),
        DaemonOpts {
            runtime_dir: Some(guard.base_dir().join("run")),
            ..Default::default()
        },
    )
    .await
    .expect("daemon start");
    let sock = d.paths.sock.clone();

    // Create a session via the daemon with LinkedSuccessor policy.
    let cwd = guard.home.path().to_path_buf();
    let conn = SocketTransport::connect(&sock, Hello::new(ClientKind::Test))
        .await
        .unwrap();
    let (mut t, snap) = SocketTransport::attach(
        conn,
        Attach::Create {
            config: SessionConfig {
                cwd: Some(cwd.clone()),
                model_override: Some("claude-sonnet-4-5".into()),
                persist: true,
                await_extensions: true,
                compaction_policy: CompactionPolicyWire::LinkedSuccessor,
                ..Default::default()
            },
            mode: AttachMode::Mirror,
        },
    )
    .await
    .expect("daemon attach");
    let old_id = snap.meta.journal_id.clone();

    // ── 2. Run two turns so there's enough to compact ──
    for msg in ["hello 1", "hello 2"] {
        t.send(SessionCommand::Submit {
            text: msg.into(),
            attachments: vec![],
        })
        .await
        .unwrap();
        loop {
            let e = next(&mut t).await;
            if matches!(e.event, SessionEventWire::Idle) {
                break;
            }
        }
    }

    // ── 3. Trigger LinkedSuccessor compaction ──
    t.send(SessionCommand::Compact { instructions: None })
        .await
        .unwrap();
    let mut new_id = String::new();
    loop {
        let e = next(&mut t).await;
        match e.event {
            SessionEventWire::CompactionApplied {
                previous_session_id,
                session_id,
                ..
            } => {
                assert_eq!(previous_session_id, old_id);
                assert_ne!(session_id, old_id);
                new_id = session_id;
            }
            SessionEventWire::Idle => break,
            _ => {}
        }
    }
    assert!(!new_id.is_empty(), "compaction must produce a successor");

    // Verify on disk: old session has compacted_into, successor exists.
    let old_session =
        agent_engine::core::session::Session::load(&old_id).expect("old session on disk");
    assert_eq!(
        old_session.compacted_into.as_deref(),
        Some(new_id.as_str()),
        "predecessor must have compacted_into set"
    );

    // ── 4. SessionLock on the old id must refuse with CompactedInto ──
    let sessions_dir = agent_core::session_lock::sessions_dir();
    let holder = agent_core::session_lock::LockHolder {
        pid: std::process::id(),
        kind: "tui".to_string(),
    };
    let err =
        agent_core::session_lock::SessionLock::try_acquire(&sessions_dir, &old_id, holder);
    let err = err.expect_err("lock on compacted session must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("compacted into"),
        "error must say 'compacted into': {msg}"
    );
    assert!(
        msg.contains(&new_id),
        "error must name the successor: {msg}"
    );

    // ── 5. follow_compaction_chain from old resolves to new ──
    let resolved = agent_core::session::follow_compaction_chain(old_session).unwrap();
    assert_eq!(resolved.session.id, new_id);
    assert!(resolved.compaction_notice.is_some());

    // ── 6. Stop daemon → lock released ──
    SocketTransport::shutdown(&sock, false).await.unwrap();
    tokio::time::timeout(Duration::from_secs(15), d.wait())
        .await
        .expect("daemon wait");
    tokio::time::sleep(Duration::from_millis(100)).await;

    // After daemon stop, the successor lock should be acquirable.
    let holder2 = agent_core::session_lock::LockHolder {
        pid: std::process::id(),
        kind: "tui".to_string(),
    };
    let lock =
        agent_core::session_lock::SessionLock::try_acquire(&sessions_dir, &new_id, holder2);
    lock.expect("after daemon stop, successor lock must be acquirable");
}
