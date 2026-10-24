//! A session's first turn never sends its request before `on_session_start`
//! has run (nor does a keep-warm ping; a manual `/compact` builds its own
//! prompt and does not wait).
//!
//! The hook's answer (a session-scoped injection) goes into the system prompt
//! of every turn request. A session created with `await_extensions = false`
//! (the TUI quick-start path, and every session a daemon reload restores) runs
//! the hook in the background once extension discovery is done. A turn
//! submitted before that waits for the hook; otherwise its first request goes
//! out without the injection and the next one changes the cached system
//! prompt.
//!
//! Here discovery is held open for 1.5 s by an extension whose `initialize`
//! sleeps, and the turn is submitted at once.

#![cfg(unix)]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_engine::session::transport::{ClientTransport, LocalTransport};
use agent_engine::session::*;
use agent_engine::{EngineHost, HostOpts};
use phase2::{spawn_stub, HomeGuard, Script, ANTHROPIC_SSE};
use serial_test::serial;

/// What the fixture injects from `on_session_start`.
const SENTINEL: &str = "SLOW-START-SENTINEL";

fn install_slow_start_plugin(base: &Path) -> PathBuf {
    let dir = base.join("plugins/slow-start");
    std::fs::create_dir_all(dir.join(".synaps-plugin")).unwrap();
    std::fs::create_dir_all(dir.join("extensions")).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/slow_session_start_extension.py"),
        dir.join("extensions/slow_session_start_extension.py"),
    )
    .unwrap();
    std::fs::write(
        dir.join(".synaps-plugin/plugin.json"),
        r#"{
  "name": "slow-start",
  "version": "0.1.0",
  "extension": {
    "protocol_version": 1,
    "runtime": "process",
    "command": "python3",
    "args": ["extensions/slow_session_start_extension.py"],
    "permissions": ["session.lifecycle"],
    "hooks": [{"hook": "on_session_start"}]
  }
}
"#,
    )
    .unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_turn_submitted_before_the_session_start_hook_waits_for_it() {
    let guard = HomeGuard::new();
    let plugin = install_slow_start_plugin(&guard.base_dir());
    std::fs::write(plugin.join("init-ms"), "1500").unwrap();
    let (url, _hits, bodies) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let host: Arc<EngineHost> = EngineHost::boot_and_install(HostOpts {
        profile: None,
        no_extensions: false,
        mcp_discovery: agent_engine::McpDiscovery::Inline,
    })
    .await
    .expect("host boot");
    // Discovery runs in the background, as in a daemon or the TUI.
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let _loader = agent_engine::extensions::loader::spawn_discover_and_load(
        Arc::clone(host.ext_manager()),
        tx,
        None,
    );

    let handle = host
        .create_session(SessionConfig {
            cwd: Some(guard.home.path().to_path_buf()),
            model_override: Some("claude-sonnet-4-5".into()),
            persist: false,
            await_extensions: false,
            ..Default::default()
        })
        .await
        .expect("create session");
    let (mut t, _snap) = LocalTransport::attach(handle, ClientMeta::new(ClientKind::Test))
        .await
        .expect("attach");
    t.send(SessionCommand::Submit { text: "first".into(), attachments: vec![] })
        .await
        .unwrap();
    loop {
        let e = tokio::time::timeout(Duration::from_secs(30), t.next_event())
            .await
            .expect("turn hung")
            .expect("alive");
        if matches!(e.event, SessionEventWire::Idle) {
            break;
        }
    }

    let first = {
        let bodies = bodies.lock().unwrap_or_else(|p| p.into_inner());
        assert!(!bodies.is_empty(), "the turn sent a request");
        bodies[0].clone()
    };
    let first: serde_json::Value = serde_json::from_slice(&first).expect("json body");
    let system = first["system"].to_string();
    assert!(
        system.contains(SENTINEL),
        "the first request carries the session-start injection: {system}"
    );
    let log = std::fs::read_to_string(plugin.join("session-start.jsonl")).unwrap_or_default();
    assert_eq!(log.lines().count(), 1, "the hook ran once: {log}");

    t.send(SessionCommand::End { reason: EndReason::ClientQuit }).await.ok();
}
