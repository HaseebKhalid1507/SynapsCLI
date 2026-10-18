//! Handshake + refusal paths over a real UDS (PLAN-phase2 §5.3), driven with
//! a raw client so the daemon's behaviour is tested, not the transport's.

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::sync::Arc;
use std::time::Duration;

use agent_engine::daemon::{self, Daemon, DaemonOpts};
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::{EngineHost, HostOpts};
use phase2::HomeGuard;
use serial_test::serial;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

async fn start(runtime_dir: &std::path::Path) -> Daemon {
    let host: Arc<EngineHost> =
        EngineHost::boot_and_install(HostOpts { profile: None, no_extensions: true, mcp_discovery: agent_engine::McpDiscovery::Inline }).await.expect("host boot");
    Daemon::start(host, DaemonOpts { runtime_dir: Some(runtime_dir.to_path_buf()), factory: Some(daemon::echo_factory()), ..Default::default() })
        .await
        .expect("daemon start")
}

struct Raw {
    r: BufReader<tokio::net::unix::OwnedReadHalf>,
    w: tokio::net::unix::OwnedWriteHalf,
}

impl Raw {
    async fn connect(p: &std::path::Path) -> Self {
        let (r, w) = UnixStream::connect(p).await.unwrap().into_split();
        Self { r: BufReader::new(r), w }
    }
    async fn send_line(&mut self, s: &str) {
        self.w.write_all(s.as_bytes()).await.unwrap();
        self.w.write_all(b"\n").await.unwrap();
    }
    /// For oversize probes: the daemon may close before the tail is written.
    async fn send_line_lossy(&mut self, s: &str) {
        let _ = self.w.write_all(s.as_bytes()).await;
        let _ = self.w.write_all(b"\n").await;
    }
    async fn send(&mut self, f: &ClientFrame) {
        self.w.write_all(encode_line(f).unwrap().as_bytes()).await.unwrap();
    }
    /// `None` = EOF.
    async fn recv(&mut self) -> Option<DaemonFrame> {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(3), self.r.read_line(&mut line)).await.expect("timely").unwrap();
        if n == 0 {
            return None;
        }
        Some(decode_line(line.trim_end()).unwrap())
    }
}

#[tokio::test]
#[serial]
async fn handshake_refuses_version_mismatch_and_protocol_violations() {
    let guard = HomeGuard::new();
    let d = start(&guard.base_dir().join("run")).await;
    let sock = d.paths.sock.clone();

    // version mismatch → Refused{Version} then EOF
    let mut c = Raw::connect(&sock).await;
    let mut hello = Hello::new(ClientKind::Test);
    hello.protocol_version = 99;
    c.send(&ClientFrame::Hello(hello)).await;
    match c.recv().await {
        Some(DaemonFrame::Refused { reason: RefuseReason::Version { daemon_version, min, max }, message }) => {
            assert_eq!((daemon_version, min, max), (PROTOCOL_VERSION, PROTOCOL_MIN, PROTOCOL_MAX));
            assert!(message.contains("99"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert!(c.recv().await.is_none(), "daemon closes after refusal");

    // Hello not first → Refused{Protocol} then EOF
    let mut c = Raw::connect(&sock).await;
    c.send(&ClientFrame::Ping).await;
    assert!(matches!(c.recv().await, Some(DaemonFrame::Refused { reason: RefuseReason::Protocol, .. })));
    assert!(c.recv().await.is_none());

    // malformed first frame → Refused{Protocol}
    let mut c = Raw::connect(&sock).await;
    c.send_line("{not json").await;
    assert!(matches!(c.recv().await, Some(DaemonFrame::Refused { reason: RefuseReason::Protocol, .. })));

    // > rpc's 1 MiB but under DAEMON_MAX_FRAME_BYTES → accepted (Pong)
    let mut c = Raw::connect(&sock).await;
    c.send(&ClientFrame::Hello(Hello::new(ClientKind::Test))).await;
    assert!(matches!(c.recv().await, Some(DaemonFrame::Welcome(_))));
    let mid = format!("{{\"type\":\"ping\",\"pad\":\"{}\"}}", "x".repeat(RPC_MAX_FRAME_BYTES * 2));
    c.send_line(&mid).await;
    assert!(matches!(c.recv().await, Some(DaemonFrame::Pong { .. })));

    // oversize frame (> DAEMON_MAX_FRAME_BYTES = 64 MiB) → Error + close
    let big = format!("{{\"type\":\"ping\",\"pad\":\"{}\"}}", "x".repeat(DAEMON_MAX_FRAME_BYTES + 10));
    c.send_line_lossy(&big).await;
    match c.recv().await {
        Some(DaemonFrame::Error { message, .. }) => assert!(message.contains("64 MiB"), "{message}"),
        other => panic!("{other:?}"),
    }
    assert!(c.recv().await.is_none());

    // different binary version, same protocol → allowed (Welcome carries daemon_version)
    let mut c = Raw::connect(&sock).await;
    let mut hello = Hello::new(ClientKind::Test);
    hello.client_version = "0.0.0-other".into();
    c.send(&ClientFrame::Hello(hello)).await;
    match c.recv().await {
        Some(DaemonFrame::Welcome(w)) => {
            assert_eq!(w.protocol_version, PROTOCOL_VERSION);
            assert_eq!(w.daemon_version, binary_version());
            assert_eq!(w.pid, std::process::id());
        }
        other => panic!("{other:?}"),
    }
    // Cmd before Attach → Error, connection stays open; Sessions still answered
    c.send(&ClientFrame::Cmd { session_id: "x".into(), cmd: SessionCommand::Save }).await;
    assert!(matches!(c.recv().await, Some(DaemonFrame::Error { .. })));
    c.send(&ClientFrame::Sessions).await;
    assert!(matches!(c.recv().await, Some(DaemonFrame::SessionList { .. })));
    c.send(&ClientFrame::Bye).await;
    assert!(matches!(c.recv().await, Some(DaemonFrame::Bye { .. })));

    d.shutdown_token().cancel();
    d.wait().await;
}

#[tokio::test]
#[serial]
async fn control_fast_path_allocates_no_session() {
    let guard = HomeGuard::new();
    let d = start(&guard.base_dir().join("run")).await;
    let sock = d.paths.sock.clone();
    let mut c = Raw::connect(&sock).await;
    c.send(&ClientFrame::Hello(Hello::new(ClientKind::Test))).await;
    assert!(matches!(c.recv().await, Some(DaemonFrame::Welcome(_))));
    for _ in 0..3 {
        c.send(&ClientFrame::Ping).await;
        match c.recv().await {
            Some(DaemonFrame::Pong { sessions, pid, .. }) => {
                assert_eq!(sessions, 0);
                assert_eq!(pid, std::process::id());
            }
            other => panic!("{other:?}"),
        }
        c.send(&ClientFrame::Sessions).await;
        assert!(matches!(c.recv().await, Some(DaemonFrame::SessionList { sessions }) if sessions.is_empty()));
    }
    assert!(d.state.live_sessions().is_empty());
    assert!(d.state.host.sessions().is_empty());
    d.shutdown_token().cancel();
    d.wait().await;
}

#[tokio::test]
#[serial]
async fn create_refuses_relative_or_missing_cwd() {
    let guard = HomeGuard::new();
    let d = start(&guard.base_dir().join("run")).await;
    let sock = d.paths.sock.clone();
    let mut c = Raw::connect(&sock).await;
    c.send(&ClientFrame::Hello(Hello::new(ClientKind::Test))).await;
    assert!(matches!(c.recv().await, Some(DaemonFrame::Welcome(_))));
    c.send(&ClientFrame::Attach(Attach::Create {
        config: SessionConfig { cwd: Some("/definitely/not/here".into()), ..Default::default() },
        mode: AttachMode::Mirror,
    }))
    .await;
    assert!(matches!(c.recv().await, Some(DaemonFrame::Error { message, .. }) if message.contains("cwd")));
    assert!(d.state.live_sessions().is_empty());
    d.shutdown_token().cancel();
    d.wait().await;
}

/// `synaps auth state` on a real daemon: it answers from the broker on the
/// host's token cache (the one every session of the daemon uses), with no
/// session. A clear for one account drops that account's cooldown in that
/// broker and keeps the other's, writes nothing in the credential store,
/// and the connection stays usable. The client API gets the same report.
#[tokio::test]
#[serial]
async fn auth_state_reads_and_clears_the_host_broker_without_writing_credentials() {
    use synaps_cli::auth::{
        broker_from_source, Account, AuthStateQuery, CredentialRef, CredentialSource, OAuthProviderId,
    };
    let guard = HomeGuard::new();
    let auth_json = guard.base_dir().join("auth.json");
    let mut store: serde_json::Value = serde_json::from_str(&phase2::synthetic_auth_json()).unwrap();
    for label in ["a", "b"] {
        store[format!("anthropic@{label}")] = serde_json::json!({
            "type": "oauth", "access": format!("anthropic-{label}-access"), "refresh": format!("refresh-{label}-SECRET"), "expires": 9_999_999_999_999u64,
        });
    }
    std::fs::write(&auth_json, store.to_string()).unwrap();
    let store_before = std::fs::read(&auth_json).unwrap();
    let d = start(&guard.base_dir().join("run")).await;
    let sock = d.paths.sock.clone();

    let broker = broker_from_source(&CredentialSource::Local, &d.state.host.parts().token_cache, reqwest::Client::new());
    let until = agent_core::epoch_millis() + 3_600_000;
    for label in ["a", "b"] {
        let cred = CredentialRef::new(OAuthProviderId::Anthropic, Account::named(label).unwrap());
        broker.report_cooldown(&cred, Some(until), "anthropic_quota_exhausted").await.unwrap();
    }
    let anthropic = AuthStateQuery { provider: Some("anthropic".into()), ..AuthStateQuery::default() };
    let cooldowns = |report: &synaps_cli::auth::AuthStateReport| {
        let mut rows = report.accounts.iter().map(|a| (a.label.clone(), a.cooldown.as_ref().map(|c| c.until_ms))).collect::<Vec<_>>();
        rows.sort();
        rows
    };

    let mut c = Raw::connect(&sock).await;
    c.send(&ClientFrame::Hello(Hello::new(ClientKind::Test))).await;
    assert!(matches!(c.recv().await, Some(DaemonFrame::Welcome(_))));
    c.send(&ClientFrame::AuthState(anthropic.clone())).await;
    let report = match c.recv().await {
        Some(DaemonFrame::AuthState { report }) => report,
        other => panic!("{other:?}"),
    };
    assert!(report.live && report.broker_active, "{report:?}");
    assert_eq!(cooldowns(&report), vec![("a".to_string(), Some(until)), ("b".to_string(), Some(until)), ("default".to_string(), None)]);

    c.send(&ClientFrame::AuthState(AuthStateQuery { account: Some("b".into()), clear: true, ..anthropic.clone() })).await;
    match c.recv().await {
        Some(DaemonFrame::AuthState { report }) => {
            assert_eq!(report.cleared, vec!["anthropic:b".to_string()]);
            assert_eq!(cooldowns(&report), vec![("b".to_string(), None)]);
        }
        other => panic!("{other:?}"),
    }
    let rows = broker.accounts(OAuthProviderId::Anthropic).await.unwrap();
    let cooldown_of = |label: &str| rows.iter().find(|r| r.label == label).unwrap().cooldown_until;
    assert_eq!((cooldown_of("a"), cooldown_of("b")), (Some(until), None), "the clear reached the broker sessions use");

    c.send(&ClientFrame::Ping).await;
    assert!(matches!(c.recv().await, Some(DaemonFrame::Pong { sessions: 0, .. })));
    assert_eq!(std::fs::read(&auth_json).unwrap(), store_before, "auth state wrote the credential store");
    assert!(d.state.live_sessions().is_empty());

    match agent_engine::session::socket_transport::SocketTransport::auth_state(&sock, anthropic).await {
        Ok(agent_engine::session::socket_transport::AuthStateReply::Report(report)) => {
            assert!(report.live, "{report:?}");
            assert_eq!(cooldowns(&report), vec![("a".to_string(), Some(until)), ("b".to_string(), None), ("default".to_string(), None)]);
        }
        other => panic!("{other:?}"),
    }
    d.shutdown_token().cancel();
    d.wait().await;
}

/// r2 (r1 limit): the `synaps auth state` binary with no daemon running
/// (autospawn off; scratch HOME, base and runtime dirs) prints the account
/// list with nothing recorded, marked not live, with the no-daemon notice on
/// stderr; `--clear` clears nothing and says so. The store is never written.
/// Every token is far from expiry, so nothing here could ever refresh.
#[test]
#[serial]
fn auth_state_cli_without_a_daemon_lists_accounts_and_refuses_to_clear() {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::Instant;
    let home = tempfile::tempdir().unwrap();
    let base = home.path().join("base");
    let run = home.path().join("run");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::create_dir_all(&run).unwrap();
    let auth_json = base.join("auth.json");
    let mut store = serde_json::Map::new();
    for label in ["a", "b"] {
        store.insert(
            format!("anthropic@{label}"),
            serde_json::json!({
                "type": "oauth", "access": format!("anthropic-{label}-access"),
                "refresh": format!("refresh-{label}-SECRET"), "expires": 9_999_999_999_999u64,
            }),
        );
    }
    std::fs::write(&auth_json, serde_json::Value::Object(store).to_string()).unwrap();
    std::fs::write(base.join("config"), "").unwrap();
    let store_before = std::fs::read(&auth_json).unwrap();

    let run_cli = |args: &[&str]| -> (Option<i32>, String, String) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_synaps"))
            .args(args)
            .env("HOME", home.path())
            .env("SYNAPS_BASE_DIR", &base)
            .env("SYNAPS_RUNTIME_DIR", &run)
            .env("SYNAPS_DAEMON_AUTOSPAWN", "0")
            .env("TERM", "dumb")
            .env_remove("SYNAPS_PROFILE")
            .env_remove("SYNAPS_AUTH_ENDPOINT")
            .env_remove("SYNAPS_MACHINE_TOKEN")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let out = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stdout.read_to_string(&mut s);
            s
        });
        let err = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s);
            s
        });
        let deadline = Instant::now() + Duration::from_secs(30);
        let code = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status.code();
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("synaps {args:?} did not exit within 30s");
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        (code, out.join().unwrap(), err.join().unwrap())
    };

    let (code, out, err) = run_cli(&["auth", "state", "--provider", "claude", "--json"]);
    assert_eq!(code, Some(0), "stderr:\n{err}");
    let report: serde_json::Value =
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(
        (report["live"].as_bool(), report["broker_active"].as_bool()),
        (Some(false), Some(false)),
        "{report}"
    );
    let mut labels: Vec<_> = report["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["label"].as_str().unwrap().to_string())
        .collect();
    labels.sort();
    assert_eq!(labels, vec!["a", "b"], "{report}");
    assert!(
        report["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a.get("cooldown").is_none() && a.get("last_usage").is_none()),
        "{report}"
    );
    assert!(err.contains("no daemon running"), "{err}");
    let mut seen = vec![out, err];

    let (code, out, err) = run_cli(&["auth", "state", "--provider", "claude", "--clear"]);
    assert!(
        matches!(code, Some(c) if c != 0),
        "a clear without a daemon must fail; stderr:\n{err}"
    );
    assert!(err.contains("nothing cleared"), "{err}");
    seen.extend([out, err]);
    for text in &seen {
        for secret in ["SECRET", "anthropic-a-access", "anthropic-b-access"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
    }
    assert_eq!(
        std::fs::read(&auth_json).unwrap(),
        store_before,
        "auth state wrote the credential store"
    );
}

/// 4/4b: `Welcome.features` lists "children", and the `children` query is a
/// control frame answered before Attach and on an attached connection, from
/// the process-wide ledger (session filter, sid filter, unknown session).
#[tokio::test]
#[serial]
async fn children_query_is_answered_before_and_after_attach() {
    use agent_engine::session::children;
    let guard = HomeGuard::new();
    let d = start(&guard.base_dir().join("run")).await;
    let sock = d.paths.sock.clone();
    let me = children::identify(std::process::id()).expect("own stat");
    // A live process leading its own session, for the second session's
    // record. A made-up identity would be dropped by the ledger's sweep
    // (a live pid with other start_ticks is a reused pid).
    let mut sleeper = std::process::Command::new("setsid").args(["sleep", "30"]).spawn().expect("setsid sleep");
    let other = {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(id) = children::identify(sleeper.id()).filter(|id| id.sid == id.pid) {
                break id;
            }
            assert!(std::time::Instant::now() < deadline, "the setsid child never led its session");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    assert_ne!(other.sid, me.sid);
    let spawn = |tool_id: &str, child: children::ChildIdentity| children::ChildSpawn {
        tool_id: tool_id.into(),
        tool_name: "bash".into(),
        handle_id: None,
        shell_session_id: None,
        child,
    };
    assert!(children::record_spawn("hs-children-a", spawn("t-me", me.clone())));
    assert!(children::record_spawn("hs-children-b", spawn("t-other", other.clone())));

    let mut c = Raw::connect(&sock).await;
    c.send(&ClientFrame::Hello(Hello::new(ClientKind::Test))).await;
    match c.recv().await {
        Some(DaemonFrame::Welcome(w)) => assert!(w.features.iter().any(|f| f == "children"), "{:?}", w.features),
        other => panic!("{other:?}"),
    }
    c.send(&ClientFrame::Children { id: Some(11), session_id: Some("hs-children-a".into()), sid: None, members: true })
        .await;
    match c.recv().await {
        Some(DaemonFrame::ChildList(r)) => {
            assert_eq!((r.id, r.supported, r.boot_id.as_deref()), (Some(11), true, Some(me.boot_id.as_str())));
            assert_eq!(r.sessions.len(), 1);
            let entry = &r.sessions[0].children[0];
            assert_eq!((entry.tool_id.as_str(), &entry.child), ("t-me", &me));
            let members = entry.members.as_ref().expect("members asked for");
            assert!(members.iter().any(|m| m.pid == me.pid && m.start_ticks == me.start_ticks));
            assert!(r.host.is_empty(), "the host list comes only without a session filter");
        }
        other => panic!("{other:?}"),
    }
    c.send(&ClientFrame::Children { id: None, session_id: None, sid: Some(me.sid), members: false }).await;
    match c.recv().await {
        Some(DaemonFrame::ChildList(r)) => {
            let ids: Vec<&str> = r.sessions.iter().map(|s| s.session_id.as_str()).collect();
            assert!(ids.contains(&"hs-children-a") && !ids.contains(&"hs-children-b"), "{ids:?}");
            assert!(r.sessions.iter().flat_map(|s| &s.children).all(|e| e.child.sid == me.sid && e.members.is_none()));
        }
        other => panic!("{other:?}"),
    }
    c.send(&ClientFrame::Children { id: Some(12), session_id: Some("hs-no-such".into()), sid: None, members: false })
        .await;
    assert!(matches!(c.recv().await, Some(DaemonFrame::ChildList(r)) if r.id == Some(12) && r.sessions.is_empty()));

    // On an attached connection too.
    c.send(&ClientFrame::Attach(Attach::Create { config: SessionConfig::default(), mode: AttachMode::Mirror })).await;
    assert!(matches!(c.recv().await, Some(DaemonFrame::Attached(_))));
    c.send(&ClientFrame::Children { id: Some(13), session_id: Some("hs-children-b".into()), sid: None, members: false })
        .await;
    loop {
        match c.recv().await {
            Some(DaemonFrame::Event(_)) => continue,
            Some(DaemonFrame::ChildList(r)) => {
                assert_eq!(r.id, Some(13));
                let entry = &r.sessions[0].children[0];
                assert_eq!((entry.tool_id.as_str(), &entry.child, entry.state), ("t-other", &other, children::ChildState::Running));
                break;
            }
            frame => panic!("{frame:?}"),
        }
    }
    c.send(&ClientFrame::Bye).await;
    d.shutdown_token().cancel();
    d.wait().await;
    let _ = sleeper.kill();
    let _ = sleeper.wait();
}

/// 4b: `thinking_options` is a control frame (before Attach and attached).
/// No model = what a new session on this daemon gets; a malformed id gets
/// `options: null`; "thinking_options" is in `Welcome.features`.
#[tokio::test]
#[serial]
async fn thinking_options_query_is_answered_before_and_after_attach() {
    use agent_engine::runtime::openai::catalog::validation::{thinking_options_for_model, OptionsBasis};
    let guard = HomeGuard::new();
    let d = start(&guard.base_dir().join("run")).await;
    let sock = d.paths.sock.clone();
    let mut c = Raw::connect(&sock).await;
    c.send(&ClientFrame::Hello(Hello::new(ClientKind::Test))).await;
    match c.recv().await {
        Some(DaemonFrame::Welcome(w)) => assert!(w.features.iter().any(|f| f == "thinking_options"), "{:?}", w.features),
        other => panic!("{other:?}"),
    }
    c.send(&ClientFrame::ThinkingOptions { id: Some(21), model: None, warm: false }).await;
    match c.recv().await {
        Some(DaemonFrame::ThinkingOptionsResult { id, model, options }) => {
            assert_eq!(id, Some(21));
            assert_eq!(model, d.state.host.new_session_model());
            assert_eq!(options.expect("answered").levels, thinking_options_for_model(&model));
        }
        other => panic!("{other:?}"),
    }
    c.send(&ClientFrame::ThinkingOptions { id: Some(22), model: Some("a b".into()), warm: false }).await;
    assert!(matches!(
        c.recv().await,
        Some(DaemonFrame::ThinkingOptionsResult { id: Some(22), options: None, model }) if model == "a b"
    ));
    c.send(&ClientFrame::Attach(Attach::Create { config: SessionConfig::default(), mode: AttachMode::Mirror })).await;
    assert!(matches!(c.recv().await, Some(DaemonFrame::Attached(_))));
    c.send(&ClientFrame::ThinkingOptions { id: Some(23), model: Some("xai-auth/grok-4.5".into()), warm: false }).await;
    loop {
        match c.recv().await {
            Some(DaemonFrame::Event(_)) => continue,
            Some(DaemonFrame::ThinkingOptionsResult { id, options, .. }) => {
                let options = options.expect("answered");
                assert_eq!((id, options.basis, options.exact), (Some(23), OptionsBasis::Static, true));
                assert_eq!(options.default_level.as_deref(), Some("high"));
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    c.send(&ClientFrame::Bye).await;
    d.shutdown_token().cancel();
    d.wait().await;
}
