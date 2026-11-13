//! `ClientFrame::SavedSessions`: a thin client lists the saved sessions on
//! disk through the daemon (named ones included, held or not), one row per
//! conversation, and resumes one by name with `Attach::Create{continue}`.
//! Real host, real UDS (a named pipe on Windows), a seeded throwaway HOME.

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agent_core::session::Session;
use agent_engine::daemon::{Daemon, DaemonOpts};
use agent_engine::session::socket_transport::{SavedSessions, SocketTransport};
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::{EngineHost, HostOpts};
use chrono::Utc;
use phase2::{spawn_stub, HomeGuard, Script, ANTHROPIC_SSE};
use serial_test::serial;

const ALPHA: &str = "20261101-100000-aaaa";
const BRAVO: &str = "20261101-090000-bbbb";
const GAMMA_OLD: &str = "20261101-060000-cc01";
const GAMMA: &str = "20261101-080000-cc02";
const DELTA: &str = "20261101-070000-dddd";
const SHORT: &str = "20261101-050000-eeee";

/// Write `sessions/<id>.json` the way a saved session looks on disk.
fn seed(dir: &Path, id: &str, name: Option<&str>, title: &str, hours_ago: i64, msgs: usize, into: Option<&str>) {
    let mut s = Session::new("claude-sonnet-4-5", "medium", None);
    s.id = id.into();
    s.name = name.map(str::to_string);
    s.title = title.into();
    s.updated_at = Utc::now() - chrono::Duration::hours(hours_ago);
    s.created_at = s.updated_at - chrono::Duration::minutes(30);
    s.api_messages = (0..msgs)
        .map(|i| {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            Arc::new(serde_json::json!({"role": role, "content": format!("m{i}")}))
        })
        .collect();
    s.message_count = msgs;
    s.compacted_into = into.map(str::to_string);
    std::fs::write(dir.join(format!("{id}.json")), serde_json::to_vec(&s).unwrap()).unwrap();
}

fn seed_home(guard: &HomeGuard) {
    let dir = guard.base_dir().join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    seed(&dir, ALPHA, Some("alpha"), "Alpha work", 1, 6, None);
    seed(&dir, BRAVO, None, "Bravo, unnamed", 2, 6, None);
    // A LinkedSuccessor chain: the name moved to the head, which holds only
    // the summary (2 messages) and is still listed.
    seed(&dir, GAMMA_OLD, None, "Gamma", 6, 40, Some(GAMMA));
    seed(&dir, GAMMA, Some("gamma"), "↳ Gamma", 3, 2, None);
    seed(&dir, DELTA, Some("delta"), "Delta", 4, 6, None);
    // Noise the Past tab hides: under 3 messages, and a junk id.
    seed(&dir, SHORT, None, "short", 5, 1, None);
    seed(&dir, "scratch", Some("junk"), "junk", 0, 9, None);
    let cwd = guard.home.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let line = serde_json::json!({"session_id": ALPHA, "event": "start", "cwd": cwd});
    std::fs::write(dir.join("index.jsonl"), format!("{line}\n")).unwrap();
}

async fn start(guard: &HomeGuard) -> Daemon {
    let (url, _hits, _bodies) = spawn_stub(Script::Sse(ANTHROPIC_SSE)).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", url);
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

async fn list(sock: &Path, named_only: bool, limit: Option<u32>) -> SavedSessions {
    tokio::time::timeout(Duration::from_secs(20), SocketTransport::saved_sessions(sock, named_only, limit))
        .await
        .expect("timely")
        .expect("saved sessions")
}

fn ids(l: &SavedSessions) -> Vec<&str> {
    l.sessions.iter().map(|s| s.id.as_str()).collect()
}

/// Ids in list order, without `skip` (the held session, whose
/// `updated_at` a resume may move).
fn order_without<'a>(l: &'a SavedSessions, skip: &str) -> Vec<&'a str> {
    ids(l).into_iter().filter(|id| *id != skip).collect()
}

fn assert_newest_first(l: &SavedSessions) {
    assert!(l.sessions.windows(2).all(|w| w[0].updated_at >= w[1].updated_at), "{:?}", ids(l));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn saved_sessions_lists_conversations_and_a_name_resumes_through_the_daemon() {
    let guard = HomeGuard::new();
    seed_home(&guard);
    let d = start(&guard).await;
    let sock = d.paths.sock.clone();

    // The feature is advertised, so a client may ask.
    let conn = SocketTransport::connect(&sock, Hello::new(ClientKind::Test)).await.unwrap();
    assert!(conn.welcome.supports(FEATURE_SAVED_SESSIONS), "{:?}", conn.welcome.features);
    assert!(conn.welcome.features.iter().any(|f| f == "saved_sessions"));
    conn.bye().await;

    // Nothing held yet: one row per conversation, noise and junk out.
    let all = list(&sock, false, None).await;
    assert!(!all.truncated);
    assert_eq!(ids(&all), [ALPHA, BRAVO, GAMMA, DELTA]);
    assert_newest_first(&all);
    assert!(all.sessions.iter().all(|s| !s.live));
    let gamma = all.sessions.iter().find(|s| s.id == GAMMA).unwrap();
    assert_eq!(gamma.name.as_deref(), Some("gamma"));
    assert_eq!(gamma.message_count, Some(2));
    assert!(gamma.created_at < gamma.updated_at - chrono::Duration::hours(2), "the chain's start, not the head's");
    let alpha = &all.sessions[0];
    assert_eq!(alpha.title, "Alpha work");
    assert_eq!(alpha.model, "claude-sonnet-4-5");
    assert_eq!(alpha.cwd.as_deref(), Some(guard.home.path().join("proj").as_path()));

    // Resume by NAME over the daemon path (`resolve_session`: chain →
    // name → id): the held session is that journal.
    let conn = SocketTransport::connect(&sock, Hello::new(ClientKind::Test)).await.unwrap();
    let config = SessionConfig {
        cwd: Some(guard.home.path().to_path_buf()),
        env: None,
        persist: true,
        continue_session: Some(Some("delta".into())),
        ..Default::default()
    };
    let (t, snap) = SocketTransport::attach(conn, Attach::create(config, AttachMode::Mirror)).await.expect("continue by name");
    assert_eq!(snap.meta.journal_id, DELTA, "`delta` resolved to its saved session");
    assert_eq!(snap.meta.name.as_deref(), Some("delta"));
    assert!(snap.conversation.messages_len >= 6, "history restored: {}", snap.conversation.messages_len);

    // It is now held: `live`, matched by journal id.
    let all = list(&sock, false, None).await;
    assert_eq!(all.sessions.len(), 4, "{:?}", ids(&all));
    assert_newest_first(&all);
    assert_eq!(order_without(&all, DELTA), [ALPHA, BRAVO, GAMMA]);
    for s in &all.sessions {
        assert_eq!(s.live, s.id == DELTA, "{}", s.id);
    }

    // named_only: unnamed BRAVO goes; the chain counts by its head's name.
    let named = list(&sock, true, None).await;
    assert_eq!(order_without(&named, DELTA), [ALPHA, GAMMA]);
    assert!(named.sessions.iter().any(|s| s.id == DELTA && s.live));
    assert!(named.sessions.iter().all(|s| s.name.is_some()));

    // limit / truncated.
    let two = list(&sock, false, Some(2)).await;
    assert_eq!(two.sessions.len(), 2);
    assert!(two.truncated);
    assert_eq!(ids(&two), ids(&all)[..2]);
    let exact = list(&sock, false, Some(4)).await;
    assert!(!exact.truncated, "exactly `limit` matched");
    let named_two = list(&sock, true, Some(2)).await;
    assert!(named_two.truncated && named_two.sessions.len() == 2);

    drop(t);
    SocketTransport::shutdown(&sock, false).await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), d.wait()).await.expect("daemon wait");
}
