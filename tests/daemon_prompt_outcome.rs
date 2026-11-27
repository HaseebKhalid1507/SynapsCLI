//! Contract v2 §3 over the socket: a late or unknown `Answer` is answered
//! `AnswerRefused`, to its sender alone. A current client decodes it; an
//! older client, whose decoder has no such event, reads it as its generic
//! unknown event and keeps the connection (no reconnect). The raw-frame
//! client plays that older reader: it sees exactly what is on the wire.

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agent_engine::daemon::{Daemon, DaemonOpts};
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::{EngineHost, HostOpts};
use phase2::{spawn_stub, HomeGuard, Script, ANTHROPIC_SSE};
use serial_test::serial;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

const QUIET: Duration = Duration::from_millis(500);

async fn boot(guard: &HomeGuard) -> Daemon {
    let (url, _hits, _bodies) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host: Arc<EngineHost> = EngineHost::boot_and_install(HostOpts {
        profile: None,
        no_extensions: true,
        mcp_discovery: agent_engine::McpDiscovery::Inline,
    })
    .await
    .expect("host boot");
    Daemon::start(host, DaemonOpts { runtime_dir: Some(guard.base_dir().join("run")), ..Default::default() })
        .await
        .expect("daemon start")
}

fn config(guard: &HomeGuard) -> SessionConfig {
    SessionConfig {
        cwd: Some(guard.home.path().to_path_buf()),
        model_override: Some("claude-sonnet-4-5".into()),
        persist: false,
        ..Default::default()
    }
}

/// An older reader: raw lines, decoded only when asked.
struct Raw {
    lines: tokio::io::Lines<tokio::io::BufReader<agent_engine::ipc::ClientReadHalf>>,
    w: agent_engine::ipc::ClientWriteHalf,
    sid: SessionId,
}

impl Raw {
    async fn attach(sock: &Path, sid: &SessionId) -> Raw {
        let (r, w) = agent_engine::ipc::split_client(agent_engine::ipc::connect(sock).await.unwrap());
        let mut raw = Raw { lines: tokio::io::BufReader::new(r).lines(), w, sid: sid.clone() };
        raw.write(&ClientFrame::Hello(Hello::new(ClientKind::Attach))).await;
        raw.until(|l| matches!(decode_line::<DaemonFrame>(l), Ok(DaemonFrame::Welcome(_)))).await;
        raw.write(&ClientFrame::Attach(Attach::Existing { session_id: sid.clone(), mode: AttachMode::Mirror })).await;
        raw.until(|l| matches!(decode_line::<DaemonFrame>(l), Ok(DaemonFrame::Attached(_)))).await;
        raw
    }

    async fn write(&mut self, f: &ClientFrame) {
        self.w.write_all(encode_line(f).unwrap().as_bytes()).await.unwrap();
    }

    async fn send(&mut self, cmd: SessionCommand) {
        let session_id = self.sid.clone();
        self.write(&ClientFrame::Cmd { session_id, cmd }).await;
    }

    async fn line(&mut self, budget: Duration) -> Option<String> {
        tokio::time::timeout(budget, self.lines.next_line()).await.ok()?.unwrap()
    }

    async fn until(&mut self, pred: impl Fn(&str) -> bool) -> Vec<String> {
        let mut seen = Vec::new();
        loop {
            let l = self.line(Duration::from_secs(10)).await.expect("a frame in time");
            let done = pred(l.as_str());
            seen.push(l);
            if done {
                return seen;
            }
        }
    }

    async fn drain(&mut self) -> Vec<String> {
        let mut seen = Vec::new();
        while let Some(l) = self.line(QUIET).await {
            seen.push(l);
        }
        seen
    }
}

/// `line` as a decoder without `answer_refused` reads it: the tag is one
/// it does not know (as `driver_armed_round_trips_and_old_client_skips`).
fn as_older_client(line: &str) -> DaemonFrame {
    decode_line::<DaemonFrame>(&line.replace(r#""ev":"answer_refused""#, r#""ev":"zz_from_the_future""#))
        .expect("an older client decodes the frame")
}

fn query_result(id: u64) -> impl Fn(&str) -> bool {
    move |l: &str| {
        matches!(
            decode_line::<DaemonFrame>(l),
            Ok(DaemonFrame::Event(WireEnvelope { event: WireSessionEvent::QueryResult { id: rid, .. }, .. })) if rid == id
        )
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_late_answer_is_refused_to_its_sender_and_an_older_client_skips_it() {
    let guard = HomeGuard::new();
    let d = boot(&guard).await;
    let sock = d.paths.sock.clone();
    let conn = SocketTransport::connect(&sock, Hello::new(ClientKind::Test)).await.unwrap();
    assert!(conn.welcome.supports(FEATURE_PROMPT_OUTCOME), "advertised");
    let mut a = SocketTransport::attach(conn, Attach::create(config(&guard), AttachMode::Mirror)).await.unwrap().0;
    let sid = a.session_id().clone();
    let mut old = Raw::attach(&sock, &sid).await;
    old.drain().await;

    // The older reader answers a prompt that is not pending, then asks
    // something: the refusal is its own, it decodes as an unknown event,
    // and the connection answers the query after it.
    old.send(SessionCommand::Answer { prompt_id: 99, value: Some("y".into()) }).await;
    old.send(SessionCommand::Query { id: 5, query: SessionQuery::View }).await;
    let lines = old.until(query_result(5)).await;
    let refusals: Vec<&String> = lines.iter().filter(|l| l.contains(r#""ev":"answer_refused""#)).collect();
    assert_eq!(refusals.len(), 1, "{lines:#?}");
    assert!(refusals[0].contains(r#""prompt_id":99"#) && !refusals[0].contains("outcome"), "not remembered: {}", refusals[0]);
    match as_older_client(refusals[0]) {
        DaemonFrame::Event(env) => assert!(matches!(env.event, WireSessionEvent::Unknown), "{:?}", env.event),
        other => panic!("{other:?}"),
    }

    // The current client heard none of it, and its own late answer is
    // decoded, and is its own.
    a.send(SessionCommand::Answer { prompt_id: 98, value: None }).await.unwrap();
    let mut seen = Vec::new();
    let refused = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let env = a.next_event().await.expect("open");
            if let SessionEventWire::AnswerRefused { prompt_id, outcome } = env.event {
                break (prompt_id, outcome);
            }
            seen.push(env.event);
        }
    })
    .await
    .expect("a's refusal");
    assert_eq!(refused, (98, None));
    assert!(!seen.iter().any(|e| matches!(e, SessionEventWire::AnswerRefused { .. })), "{seen:#?}");
    let after = old.drain().await;
    assert!(!after.iter().any(|l| l.contains("answer_refused")), "{after:#?}");

    drop((a, old));
    SocketTransport::shutdown(&sock, true).await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), d.wait()).await.expect("daemon wait");
}
