//! Session-identity differential test (T3): in-process vs daemon sessions
//! must produce identical env/cwd in tool execs. Proves F25 is fixed.
//!
//! Two runs of a bash tool that dumps `env | sort` and `pwd`:
//!   (a) daemon session created from a client with a synthetic env,
//!       but the daemon itself has a DIFFERENT env (leak canary)
//!   (b) in-process session with the same synthetic env
//!
//! The tool outputs must match. A var present in the daemon env but absent
//! from the client env must NOT appear (leak test).

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use agent_engine::daemon::{Daemon, DaemonOpts};
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::{EngineHost, HostOpts, LlmEvent, StreamEvent};
use phase2::{spawn_stub, HomeGuard, Script};
use serial_test::serial;

/// SSE: bash tool_use calling `env | sort; echo ---; pwd`
const SSE_BASH_ENV: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_id1\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_env\",\"name\":\"bash\",\"input\":{}}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",",
    "\"partial_json\":\"{\\\"command\\\":\\\"env | sort; echo ---; pwd\\\"}\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":5,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// SSE: plain text "done" (continuation after tool result)
const SSE_DONE: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_id2\",\"type\":\"message\",",
    "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
    "\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",",
    "\"stop_sequence\":null},\"usage\":{\"input_tokens\":10,\"output_tokens\":1,",
    "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// Collect ToolResult text from a SocketTransport event stream.
async fn collect_tool_result_socket(t: &mut SocketTransport) -> String {
    let mut result = String::new();
    loop {
        let e = tokio::time::timeout(Duration::from_secs(15), t.next_event())
            .await
            .expect("timely")
            .expect("open");
        match &e.event {
            SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::ToolResult {
                result: r,
                ..
            })) => {
                result = r.clone();
            }
            SessionEventWire::Idle => break,
            _ => {}
        }
    }
    result
}

/// Collect ToolResult text from a LocalTransport event stream.
async fn collect_tool_result_local(
    t: &mut agent_engine::session::transport::LocalTransport,
) -> String {
    use agent_engine::session::transport::ClientTransport;
    let mut result = String::new();
    loop {
        let e = match t.next_event().await {
            Some(e) => e,
            None => break,
        };
        match &e.event {
            SessionEventWire::Stream(StreamEvent::Llm(LlmEvent::ToolResult {
                result: r,
                ..
            })) => {
                result = r.clone();
            }
            SessionEventWire::Idle => break,
            _ => {}
        }
    }
    result
}

/// Parse `env | sort; echo ---; pwd` output into (env_map, cwd).
fn parse_env_output(raw: &str) -> (HashMap<String, String>, String) {
    let parts: Vec<&str> = raw.splitn(2, "\n---\n").collect();
    let env_section = if parts.len() == 2 { parts[0] } else { raw };
    let cwd = if parts.len() == 2 {
        parts[1].trim().to_string()
    } else {
        String::new()
    };

    let mut env_map = HashMap::new();
    for line in env_section.lines() {
        if let Some((k, v)) = line.split_once('=') {
            env_map.insert(k.to_string(), v.to_string());
        }
    }
    (env_map, cwd)
}

/// The synthetic client env that both runs share.
fn synthetic_env() -> Vec<(String, String)> {
    vec![
        ("HOME".into(), "/tmp/jt_test_home".into()),
        ("JT_MARK".into(), "session_identity_x".into()),
        ("LANG".into(), "en_US.UTF-8".into()),
        ("PATH".into(), "/tmp/fake_venv/bin:/usr/local/bin:/usr/bin:/bin".into()),
        ("TERM".into(), "xterm-256color".into()),
        ("VIRTUAL_ENV".into(), "/tmp/fake_venv".into()),
    ]
}

/// A "daemon-only" var that must NOT leak into session tools.
const DAEMON_LEAK_VAR: &str = "JT_DAEMON_ONLY_LEAK_CANARY";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn session_env_differential_in_process_vs_daemon() {
    let guard = HomeGuard::new();
    let (url, _hits, _bodies) =
        spawn_stub(Script::SeqSse(&[SSE_BASH_ENV, SSE_DONE, SSE_BASH_ENV, SSE_DONE])).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let test_cwd = guard.home.path().join("testdir");
    std::fs::create_dir_all(&test_cwd).unwrap();

    let env = synthetic_env();

    // ── (a) daemon session with client env ──────────────────────────────
    // Set the "daemon-only" leak canary in the process env (the daemon's env)
    std::env::set_var(DAEMON_LEAK_VAR, "leaked!");

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

    let conn = SocketTransport::connect(&sock, Hello::new(ClientKind::Test))
        .await
        .unwrap();
    let (mut transport, _snap) = SocketTransport::attach(
        conn,
        Attach::Create {
            config: SessionConfig {
                cwd: Some(test_cwd.clone()),
                env: Some(env.clone()),
                model_override: Some("claude-sonnet-4-5".into()),
                persist: false,
                ..Default::default()
            },
            mode: AttachMode::Mirror,
        },
    )
    .await
    .expect("attach create");

    // Submit a prompt to trigger the tool call
    transport
        .send(SessionCommand::Submit {
            text: "dump env".into(),
            attachments: vec![],
        })
        .await
        .unwrap();

    let daemon_result = collect_tool_result_socket(&mut transport).await;
    assert!(
        !daemon_result.is_empty(),
        "daemon tool result must not be empty"
    );

    // Shut down daemon
    SocketTransport::shutdown(&sock, false).await.unwrap();

    // Remove the leak canary
    std::env::remove_var(DAEMON_LEAK_VAR);

    // ── (b) in-process session with the same env ────────────────────────
    let host2: Arc<EngineHost> =
        EngineHost::boot_and_install(HostOpts { profile: None, no_extensions: true })
            .await
            .expect("host boot 2");

    let handle = host2
        .create_session(SessionConfig {
            cwd: Some(test_cwd.clone()),
            env: Some(env.clone()),
            model_override: Some("claude-sonnet-4-5".into()),
            persist: false,
            ..Default::default()
        })
        .await
        .expect("create in-process session");

    let (mut local, _snap) = agent_engine::session::transport::LocalTransport::attach(
        handle,
        ClientMeta::new(ClientKind::Test),
    )
    .await
    .expect("local attach");

    use agent_engine::session::transport::ClientTransport;
    local
        .send(SessionCommand::Submit {
            text: "dump env".into(),
            attachments: vec![],
        })
        .await
        .unwrap();

    let inproc_result = collect_tool_result_local(&mut local).await;
    assert!(
        !inproc_result.is_empty(),
        "in-process tool result must not be empty"
    );

    // ── diff ────────────────────────────────────────────────────────────
    let (daemon_env, daemon_cwd) = parse_env_output(&daemon_result);
    let (inproc_env, inproc_cwd) = parse_env_output(&inproc_result);

    // CWD must match
    assert_eq!(
        daemon_cwd, inproc_cwd,
        "cwd differs: daemon={daemon_cwd:?} vs inproc={inproc_cwd:?}"
    );
    assert_eq!(
        daemon_cwd,
        test_cwd.to_string_lossy(),
        "cwd must be the test directory"
    );

    // Leak test: daemon-only var must NOT be visible
    assert!(
        !daemon_env.contains_key(DAEMON_LEAK_VAR),
        "daemon-only var {DAEMON_LEAK_VAR} leaked into session tool!"
    );

    // Synthetic env vars must be present in both
    for (k, v) in &synthetic_env() {
        let d_val = daemon_env
            .get(k)
            .unwrap_or_else(|| panic!("daemon missing synthetic var {k}"));
        let i_val = inproc_env
            .get(k)
            .unwrap_or_else(|| panic!("in-process missing synthetic var {k}"));
        assert_eq!(
            d_val, v,
            "daemon env {k}: expected {v:?}, got {d_val:?}"
        );
        assert_eq!(
            i_val, v,
            "in-process env {k}: expected {v:?}, got {i_val:?}"
        );
    }

    // Compare: daemon and inproc must agree on shared keys
    // (modulo bash-injected process vars)
    let ignore = ["SHLVL", "_", "PWD", "OLDPWD"];
    for (k, dv) in &daemon_env {
        if ignore.contains(&k.as_str()) {
            continue;
        }
        if let Some(iv) = inproc_env.get(k) {
            assert_eq!(
                dv, iv,
                "env var {k} differs: daemon={dv:?} vs inproc={iv:?}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn attach_existing_does_not_change_env() {
    let guard = HomeGuard::new();
    let (url, _hits, _bodies) =
        spawn_stub(Script::SeqSse(&[SSE_BASH_ENV, SSE_DONE, SSE_BASH_ENV, SSE_DONE])).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);

    let test_cwd = guard.home.path().join("envtest");
    std::fs::create_dir_all(&test_cwd).unwrap();

    let env = synthetic_env();

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

    // First client: create session with synthetic env
    let conn1 = SocketTransport::connect(&sock, Hello::new(ClientKind::Test))
        .await
        .unwrap();
    let (mut t1, snap1) = SocketTransport::attach(
        conn1,
        Attach::Create {
            config: SessionConfig {
                cwd: Some(test_cwd.clone()),
                env: Some(env.clone()),
                model_override: Some("claude-sonnet-4-5".into()),
                persist: false,
                ..Default::default()
            },
            mode: AttachMode::Mirror,
        },
    )
    .await
    .expect("attach create");

    let session_id = snap1.meta.id.clone();

    // Submit first turn — get env
    t1.send(SessionCommand::Submit {
        text: "dump env".into(),
        attachments: vec![],
    })
    .await
    .unwrap();
    let result1 = collect_tool_result_socket(&mut t1).await;

    // Second client: attach to EXISTING session with DIFFERENT env
    let different_env = vec![("COMPLETELY_DIFFERENT".into(), "yes".into())];
    let mut hello2 = Hello::new(ClientKind::Test);
    hello2.env = Some(different_env);

    let conn2 = SocketTransport::connect(&sock, hello2).await.unwrap();
    let (_t2, _snap2) = SocketTransport::attach(
        conn2,
        Attach::Existing {
            session_id,
            mode: AttachMode::Mirror,
        },
    )
    .await
    .expect("attach existing");

    // Submit second turn through original transport
    t1.send(SessionCommand::Submit {
        text: "dump env again".into(),
        attachments: vec![],
    })
    .await
    .unwrap();
    let result2 = collect_tool_result_socket(&mut t1).await;

    // Parse and compare: env must be the CREATOR's, not the second client's
    let (env1, _) = parse_env_output(&result1);
    let (env2, _) = parse_env_output(&result2);

    assert_eq!(
        env1.get("JT_MARK"),
        Some(&"session_identity_x".to_string()),
        "first turn must have creator's JT_MARK"
    );
    assert_eq!(
        env2.get("JT_MARK"),
        Some(&"session_identity_x".to_string()),
        "second turn must still have creator's JT_MARK"
    );
    assert!(
        !env2.contains_key("COMPLETELY_DIFFERENT"),
        "second client's env must NOT override the session env"
    );

    SocketTransport::shutdown(&sock, false).await.unwrap();
}
