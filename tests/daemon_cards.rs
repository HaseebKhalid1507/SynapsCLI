//! Agent cards through a REAL daemon process (agent-ui C4a exit gate,
//! lifecycle R2-H4): the `synaps` binary runs as `daemon --foreground`, a
//! catalog-1 client attaches over the socket, a loopback model raises and
//! closes cards with the `ui` tool. The daemon is killed with SIGKILL after
//! an acknowledgement and restarted on the same home; the card's state
//! after `continue` is what the acknowledgement promised.
//!
//! The card store is steered through `SYNAPS_TEST_UI_FS` (test builds only,
//! `session::surfaces::ControlledFs`): `stall:<ms>`, `fail` (before the
//! rename), `syncfail` (the directory sync).
#![cfg(unix)]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use agent_engine::daemon::registry;
use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::*;
use agent_engine::session::*;
use agent_engine::ui::{Phase, SurfaceState, Verb};
use axum::response::IntoResponse;
use phase2::{HomeGuard, ANTHROPIC_SSE};
use serial_test::serial;

struct DaemonProc {
    child: Child,
    paths: registry::DaemonPaths,
}

impl DaemonProc {
    /// SIGKILL: no shutdown path runs.
    fn kill9(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for DaemonProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn spawn_daemon(guard: &HomeGuard, url: &str, control: &Path) -> DaemonProc {
    let run = guard.base_dir().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let paths = registry::daemon_paths_in(&run, None);
    let child = Command::new(env!("CARGO_BIN_EXE_synaps"))
        .args(["daemon", "--foreground"])
        .env("HOME", guard.home.path())
        .env("SYNAPS_BASE_DIR", guard.base_dir())
        .env("SYNAPS_RUNTIME_DIR", &run)
        .env("SYNAPS_DAEMON", "1")
        .env("SYNAPS_ANTHROPIC_BASE_URL", url)
        .env("SYNAPS_NO_BOOT_FX", "1")
        .env("SYNAPS_TEST_UI_FS", control)
        .env_remove("SYNAPS_DAEMON_RELOAD_STATE")
        .env_remove("SYNAPS_DAEMON_LOCK_FD")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn daemon");
    let d = DaemonProc { child, paths };
    let t0 = std::time::Instant::now();
    while SocketTransport::ping(&d.paths.sock).await.is_err() {
        assert!(t0.elapsed() < Duration::from_secs(20), "daemon never answered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    d
}

// ── the loopback model ──────────────────────────────────────────────────────

/// One `tool_use` of `ui` with `input`.
fn ui_sse(call: &str, input: &serde_json::Value) -> String {
    let partial = serde_json::to_string(&input.to_string()).unwrap();
    format!(
        concat!(
            "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_{call}\",\"type\":\"message\",",
            "\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"stop_reason\":null,",
            "\"stop_sequence\":null,\"usage\":{{\"input_tokens\":10,\"output_tokens\":0,",
            "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}}}\n\n",
            "data: {{\"type\":\"content_block_start\",\"index\":0,",
            "\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{call}\",\"name\":\"ui\",\"input\":{{}}}}}}\n\n",
            "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{partial}}}}}\n\n",
            "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
            "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\",",
            "\"stop_sequence\":null}},\"usage\":{{\"input_tokens\":10,\"output_tokens\":5,",
            "\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}}\n\n",
            "data: {{\"type\":\"message_stop\"}}\n\n",
        ),
        call = call,
        partial = partial
    )
}

/// A message's text: a string content, or its blocks' `text`s.
fn text_of(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// The last user message: its text, and whether it carries tool results.
fn last_user(body: &str) -> (String, bool) {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let Some(last) = v["messages"].as_array().and_then(|m| m.last()) else {
        return (String::new(), false);
    };
    let tools = last["content"].as_array().is_some_and(|b| b.iter().any(|b| b["type"] == "tool_result"));
    (text_of(&last["content"]), tools)
}

/// The id in the request's `shown` tool result (the card the model raised).
fn shown_card(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v["messages"].as_array()?.iter().rev().find_map(|m| {
        m["content"].as_array()?.iter().filter(|b| b["type"] == "tool_result").find_map(|b| {
            let shown: serde_json::Value = serde_json::from_str(&text_of(&b["content"])).ok()?;
            if shown["status"] != "shown" {
                return None;
            }
            shown["id"].as_str().map(str::to_string)
        })
    })
}

/// "ask me" → the model raises a card; "close it" → it closes the card
/// the request mentions; anything else (a tool result, an answer, a plain
/// prompt) → "hi".
async fn spawn_model() -> String {
    let app = axum::Router::new().fallback(|body: String| async move {
        let (text, tools) = last_user(&body);
        let sse = if tools || text.contains("<ui_answer") {
            ANTHROPIC_SSE.to_string()
        } else if text.contains("close it") {
            let id = shown_card(&body).expect("the request holds the card's `shown` result");
            ui_sse("toolu_close", &serde_json::json!({"close": id}))
        } else if text.contains("ask me") {
            ui_sse(
                "toolu_ask",
                &serde_json::json!({"question": "Which approach?", "options": ["Refactor", "Leave it"], "default": "Refactor"}),
            )
        } else {
            ANTHROPIC_SSE.to_string()
        };
        (axum::http::StatusCode::OK, [("content-type", "text/event-stream")], sse).into_response()
    });
    phase2::serve(app).await
}

// ── the client ──────────────────────────────────────────────────────────────

fn config(cwd: &Path, cards: bool, resume: Option<&str>) -> SessionConfig {
    SessionConfig {
        cwd: Some(cwd.to_path_buf()),
        model_override: Some("claude-sonnet-4-5".into()),
        surfaces: cards.then_some(true),
        continue_session: resume.map(|id| Some(id.to_string())),
        ..Default::default()
    }
}

/// A catalog-1 client (a D3 desk) on a new or continued session.
async fn attach(d: &DaemonProc, cwd: &Path, cards: bool, resume: Option<&str>) -> (SocketTransport, AttachSnapshot) {
    let mut hello = Hello::new(ClientKind::Test);
    hello.client.capabilities.surfaces = 1;
    let conn = SocketTransport::connect(&d.paths.sock, hello).await.unwrap();
    SocketTransport::attach(conn, Attach::create(config(cwd, cards, resume), AttachMode::Mirror)).await.unwrap()
}

async fn until(t: &mut SocketTransport, pred: impl Fn(&SessionEventWire) -> bool) -> Vec<SessionEventWire> {
    let mut seen = Vec::new();
    loop {
        let env = tokio::time::timeout(Duration::from_secs(20), t.next_event())
            .await
            .unwrap_or_else(|_| panic!("timed out; saw {seen:#?}"))
            .unwrap_or_else(|| panic!("connection closed; saw {seen:#?}"));
        let hit = pred(&env.event);
        seen.push(env.event);
        if hit {
            return seen;
        }
    }
}

/// Everything that arrives within `d` (or until the connection closes).
async fn quiet(t: &mut SocketTransport, d: Duration) -> Vec<SessionEventWire> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + d;
    while let Ok(Some(env)) = tokio::time::timeout_at(deadline, t.next_event()).await {
        seen.push(env.event);
    }
    seen
}

fn cards(seen: &[SessionEventWire]) -> Vec<SurfaceState> {
    seen.iter()
        .filter_map(|e| match e {
            SessionEventWire::Surface(s) => Some((**s).clone()),
            _ => None,
        })
        .collect()
}

fn turns(seen: &[SessionEventWire]) -> usize {
    seen.iter().filter(|e| matches!(e, SessionEventWire::TurnStarted { .. })).count()
}

/// The newest history: the last `Conversation` seen, else `snapshot`'s.
fn history(snapshot: &AttachSnapshot, seen: &[SessionEventWire]) -> Vec<agent_engine::SharedMessage> {
    seen.iter()
        .rev()
        .find_map(|e| match e {
            SessionEventWire::Conversation(c) => Some(c.api_messages.clone()),
            _ => None,
        })
        .unwrap_or_else(|| snapshot.conversation.api_messages.clone())
}

fn ui_answers(history: &[agent_engine::SharedMessage], card: &str) -> usize {
    let head = format!("<ui_answer card=\"{card}\"");
    history.iter().map(|m| text_of(&m["content"]).matches(&head).count()).sum()
}

/// "ask me": the turn raises one live card and ends.
async fn raise_card(t: &mut SocketTransport) -> SurfaceState {
    t.send_from_self(SessionCommand::Submit { text: "ask me".into(), attachments: vec![] }).await.unwrap();
    let seen = until(t, |e| matches!(e, SessionEventWire::Idle)).await;
    let card = cards(&seen).pop().unwrap_or_else(|| panic!("no card: {seen:#?}"));
    assert_eq!(card.phase, Phase::Open, "{card:?}");
    card
}

fn answer(card: &SurfaceState, action_id: &str) -> SessionCommand {
    SessionCommand::SurfaceAction {
        surface_id: card.id.clone(),
        contract: card.contract.clone(),
        action_id: action_id.into(),
        expect_rev: 0,
        epoch: card.epoch,
        verb: Verb::Answer,
        values: Some(serde_json::json!({"choice": "Refactor"})),
    }
}

async fn result_for(t: &mut SocketTransport, action_id: &str) -> SurfaceResult {
    let seen = until(t, |e| matches!(e, SessionEventWire::SurfaceResult(r) if r.action_id == action_id)).await;
    match seen.last() {
        Some(SessionEventWire::SurfaceResult(r)) => r.clone(),
        _ => unreachable!(),
    }
}

/// The chat's history is on disk (a `continue` after SIGKILL needs it).
async fn saved(guard: &HomeGuard, journal: &str) {
    let dir = guard.base_dir().join("sessions");
    let t0 = std::time::Instant::now();
    while !["journal", "json"].iter().any(|ext| dir.join(format!("{journal}.{ext}")).exists()) {
        assert!(t0.elapsed() < Duration::from_secs(10), "{journal} was never saved");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
}

fn control(guard: &HomeGuard) -> PathBuf {
    let p = guard.home.path().join("ui-fs-control");
    std::fs::write(&p, "").unwrap();
    p
}

const A1: &str = "0123456789abcdef0123456789abcdef";
const A2: &str = "fedcba9876543210fedcba9876543210";

// ── the reload barrier (§3.10, decision 12) ─────────────────────────────────

/// What every session hears when the reload doesn't happen.
async fn hears_abort(t: &mut SocketTransport) {
    until(t, |e| matches!(e, SessionEventWire::SystemNotice(n) if n.contains("daemon reload failed"))).await;
}

/// A reload aborts when one session's barrier fails, and again when its
/// writer is merely slow; both leave the daemon serving.
async fn reload_aborts_with(store: &str) {
    let guard = HomeGuard::new();
    let url = spawn_model().await;
    let ctl = control(&guard);
    let d = spawn_daemon(&guard, &url, &ctl).await;
    let pid = d.child.id();
    let (mut a, _) = attach(&d, guard.home.path(), true, None).await;
    raise_card(&mut a).await;
    let (mut b, _) = attach(&d, guard.home.path(), false, None).await;

    std::fs::write(&ctl, store).unwrap();
    let t0 = std::time::Instant::now();
    let why = match SocketTransport::reload(&d.paths.sock, true, None, None).await {
        Err(TransportError::Refused(why)) => why,
        other => panic!("the reload went ahead ({other:?}): is SYNAPS_TEST_UI_FS compiled into this binary?"),
    };
    // The CLI prints this: it names the session and the cause.
    assert!(why.contains("agent cards couldn't be saved"), "{why}");
    assert!(why.contains(a.session_id().as_str()) && why.contains("ui_store"), "{why}");
    assert!(!why.contains(b.session_id().as_str()), "only the failing session is named: {why}");
    assert!(t0.elapsed() < Duration::from_secs(30), "{:?}", t0.elapsed());

    // Every session gets `ReloadAborted`, and stays on this daemon.
    hears_abort(&mut a).await;
    hears_abort(&mut b).await;
    let info = registry::read_daemon_json(&d.paths).unwrap();
    assert_eq!((info.pid, info.generation), (pid, 1));
    assert!(SocketTransport::ping(&d.paths.sock).await.is_ok());

    std::fs::write(&ctl, "").unwrap();
    for t in [&mut a, &mut b] {
        t.send_from_self(SessionCommand::Submit { text: "still here".into(), attachments: vec![] }).await.unwrap();
        let seen = until(t, |e| matches!(e, SessionEventWire::Idle)).await;
        assert_eq!(turns(&seen), 1, "{seen:#?}");
    }
}

/// Exit gate: the barrier's commit fails before the rename. The reload
/// aborts, every session gets `ReloadAborted`, and the CLI names the cause.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn daemon_reload_ui_barrier_fails_aborts() {
    reload_aborts_with("fail").await;
}

/// Exit gate (decision 12): the barrier's writer stalls past the commit
/// budget and never fails. Its outcome is unknown, and an unknown barrier
/// is not a success: the reload aborts all the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn daemon_reload_ui_barrier_times_out_aborts() {
    reload_aborts_with("stall:6000").await;
}

// ── kills and restarts (R2-H4, §3.11) ───────────────────────────────────────

/// Exit gate: the agent closes an open card, the client sees the
/// `withdrawn` upsert (it is durable), and the daemon dies at once. After
/// a restart the card is still withdrawn: an answer is refused and starts
/// nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn close_ack_then_kill() {
    let guard = HomeGuard::new();
    let url = spawn_model().await;
    let ctl = control(&guard);
    let d = spawn_daemon(&guard, &url, &ctl).await;
    let (mut a, snap) = attach(&d, guard.home.path(), true, None).await;
    let journal = snap.meta.journal_id.clone();
    let card = raise_card(&mut a).await;
    saved(&guard, &journal).await;

    a.send_from_self(SessionCommand::Submit { text: "close it".into(), attachments: vec![] }).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Surface(s) if s.id == card.id && s.phase != Phase::Open)).await;
    let acked = cards(&seen).pop().unwrap();
    assert_eq!(acked.phase, Phase::Withdrawn, "{acked:?}");
    d.kill9();

    let d = spawn_daemon(&guard, &url, &ctl).await;
    let (mut b, snap) = attach(&d, guard.home.path(), true, Some(&journal)).await;
    let now = snap.surfaces.iter().find(|c| c.id == card.id).cloned().expect("the card survived");
    assert_eq!(now.phase, Phase::Withdrawn, "{now:?}");
    assert!(now.offers.is_empty() && now.rev >= acked.rev, "{now:?}");
    b.send_from_self(answer(&now, A1)).await.unwrap();
    assert_eq!(result_for(&mut b, A1).await.outcome, SurfaceOutcome::Refused);
    let seen = quiet(&mut b, Duration::from_millis(600)).await;
    assert_eq!(turns(&seen), 0, "{seen:#?}");
    drop(d);
}

/// Exit gate: a card's create is acknowledged (its upsert) and the daemon
/// dies mid-turn. After a restart the card is open, at a higher epoch, and
/// nothing ran by itself (decision 11); a fresh answer is accepted, enters
/// context once and starts one turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn new_then_kill_then_resume() {
    let guard = HomeGuard::new();
    let url = spawn_model().await;
    let ctl = control(&guard);
    let d = spawn_daemon(&guard, &url, &ctl).await;
    let (mut a, snap) = attach(&d, guard.home.path(), true, None).await;
    let journal = snap.meta.journal_id.clone();
    a.send_from_self(SessionCommand::Submit { text: "hello".into(), attachments: vec![] }).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    saved(&guard, &journal).await;

    a.send_from_self(SessionCommand::Submit { text: "ask me".into(), attachments: vec![] }).await.unwrap();
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Surface(_))).await;
    let card = cards(&seen).pop().unwrap();
    assert_eq!(card.phase, Phase::Open);
    d.kill9();

    let d = spawn_daemon(&guard, &url, &ctl).await;
    let (mut b, snap) = attach(&d, guard.home.path(), true, Some(&journal)).await;
    let now = snap.surfaces.iter().find(|c| c.id == card.id).cloned().expect("the card survived");
    assert_eq!(now.phase, Phase::Open, "{now:?}");
    assert!(now.epoch > card.epoch && now.answer.is_none(), "{now:?}");
    let seen = quiet(&mut b, Duration::from_millis(600)).await;
    assert_eq!(turns(&seen), 0, "recovery never starts the agent: {seen:#?}");

    b.send_from_self(answer(&now, A1)).await.unwrap();
    assert_eq!(result_for(&mut b, A1).await.outcome, SurfaceOutcome::Accepted);
    let seen = until(&mut b, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(turns(&seen), 1, "{seen:#?}");
    assert_eq!(ui_answers(&history(&snap, &seen), &card.id), 1, "in context once");
    drop(d);
}

/// Exit gate: `daemon close` ends the session, a system stop (T16): the
/// open card expires, and the client sees it before the session goes. The
/// daemon then dies; after a restart the card is still expired, and an
/// answer is refused and starts nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn expiry_then_daemon_restart() {
    let guard = HomeGuard::new();
    let url = spawn_model().await;
    let ctl = control(&guard);
    let d = spawn_daemon(&guard, &url, &ctl).await;
    let (mut a, snap) = attach(&d, guard.home.path(), true, None).await;
    let journal = snap.meta.journal_id.clone();
    let card = raise_card(&mut a).await;
    saved(&guard, &journal).await;

    SocketTransport::close(&d.paths.sock, a.session_id(), false, true).await.expect("closed");
    let seen = until(&mut a, |e| matches!(e, SessionEventWire::Surface(s) if s.id == card.id && s.phase != Phase::Open)).await;
    let expired = cards(&seen).pop().unwrap();
    assert_eq!(expired.phase, Phase::Expired, "{expired:?}");
    assert!(expired.offers.is_empty() && expired.epoch > card.epoch, "{expired:?}");
    d.kill9();

    let d = spawn_daemon(&guard, &url, &ctl).await;
    let (mut b, snap) = attach(&d, guard.home.path(), true, Some(&journal)).await;
    let now = snap.surfaces.iter().find(|c| c.id == card.id).cloned().expect("the card survived");
    assert_eq!(now.phase, Phase::Expired, "terminal stays terminal: {now:?}");
    b.send_from_self(answer(&now, A1)).await.unwrap();
    assert_eq!(result_for(&mut b, A1).await.outcome, SurfaceOutcome::Refused);
    let seen = quiet(&mut b, Duration::from_millis(600)).await;
    assert_eq!(turns(&seen), 0, "{seen:#?}");
    drop(d);
}

/// Exit gate (§3.4): an answer's write outlives the 2 s budget, so the
/// client gets `pending`; then the write fails.
/// - Before the rename (session A): the final result is `refused{storage,
///   retry}` ("Not sent"), and after a SIGKILL and restart the card is open
///   again with no answer anywhere.
/// - After it (session B, the directory sync fails): it stays `pending`,
///   nothing is delivered; after the restart the published image wins and
///   recovery puts the answer into context once, with no wake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn timeout_pending_vs_write_failure() {
    let guard = HomeGuard::new();
    let url = spawn_model().await;
    let ctl = control(&guard);
    let d = spawn_daemon(&guard, &url, &ctl).await;
    let (mut a, snap_a) = attach(&d, guard.home.path(), true, None).await;
    let (mut b, snap_b) = attach(&d, guard.home.path(), true, None).await;
    let (ja, jb) = (snap_a.meta.journal_id.clone(), snap_b.meta.journal_id.clone());
    let card_a = raise_card(&mut a).await;
    let card_b = raise_card(&mut b).await;
    saved(&guard, &ja).await;
    saved(&guard, &jb).await;

    std::fs::write(&ctl, "stall:3000,fail").unwrap();
    a.send_from_self(answer(&card_a, A1)).await.unwrap();
    assert_eq!(result_for(&mut a, A1).await.outcome, SurfaceOutcome::Pending);
    let last = result_for(&mut a, A1).await;
    assert_eq!(
        (last.outcome, last.reason, last.retry),
        (SurfaceOutcome::Refused, Some(SurfaceRefusal::Storage), Some(true)),
        "{last:?}"
    );

    std::fs::write(&ctl, "stall:3000,syncfail").unwrap();
    b.send_from_self(answer(&card_b, A2)).await.unwrap();
    assert_eq!(result_for(&mut b, A2).await.outcome, SurfaceOutcome::Pending);
    // The write lands at 3 s; the sync is retried 3 times, 1 s apart.
    let seen = quiet(&mut b, Duration::from_secs(7)).await;
    assert!(
        !seen.iter().any(|e| matches!(e, SessionEventWire::SurfaceResult(r) if r.outcome == SurfaceOutcome::Accepted)),
        "never acknowledged: {seen:#?}"
    );
    assert_eq!(turns(&seen), 0, "nothing is delivered from it: {seen:#?}");
    let seen = quiet(&mut a, Duration::from_millis(100)).await;
    assert_eq!(turns(&seen), 0, "{seen:#?}");
    std::fs::write(&ctl, "").unwrap();
    d.kill9();

    let d = spawn_daemon(&guard, &url, &ctl).await;
    let (mut a, snap) = attach(&d, guard.home.path(), true, Some(&ja)).await;
    let now = snap.surfaces.iter().find(|c| c.id == card_a.id).cloned().expect("card A survived");
    assert_eq!(now.phase, Phase::Open, "the old image stood: {now:?}");
    assert!(now.answer.is_none(), "{now:?}");
    let seen = quiet(&mut a, Duration::from_millis(600)).await;
    assert_eq!(turns(&seen), 0, "{seen:#?}");
    assert_eq!(ui_answers(&history(&snap, &seen), &card_a.id), 0);

    let (mut b, snap) = attach(&d, guard.home.path(), true, Some(&jb)).await;
    let now = snap.surfaces.iter().find(|c| c.id == card_b.id).cloned().expect("card B survived");
    assert!(now.answer.as_ref().is_some_and(|x| x.rev == 1 && !x.wake), "the new image won: {now:?}");
    let seen = quiet(&mut b, Duration::from_millis(600)).await;
    assert_eq!(turns(&seen), 0, "no wake: {seen:#?}");
    assert_eq!(ui_answers(&history(&snap, &seen), &card_b.id), 1, "in context once");
    drop(d);
}

// ── two desks on one session (Appendix B #1, #3, #22) and a reload (#13) ────

/// A capable desk attached to an existing session.
async fn join(d: &DaemonProc, sid: &SessionId, mode: AttachMode) -> (SocketTransport, AttachSnapshot) {
    let mut hello = Hello::new(ClientKind::Attach);
    hello.client.capabilities.surfaces = 1;
    let conn = SocketTransport::connect(&d.paths.sock, hello).await.unwrap();
    SocketTransport::attach(conn, Attach::Existing { session_id: sid.clone(), mode }).await.unwrap()
}

/// A new session both desks may type into, with cards on.
async fn shared(d: &DaemonProc, cwd: &Path) -> (SocketTransport, AttachSnapshot) {
    let mut hello = Hello::new(ClientKind::Test);
    hello.client.capabilities.surfaces = 1;
    let conn = SocketTransport::connect(&d.paths.sock, hello).await.unwrap();
    let cfg = SessionConfig { client_input: Some(ClientInput::Shared), ..config(cwd, true, None) };
    SocketTransport::attach(conn, Attach::create(cfg, AttachMode::Mirror)).await.unwrap()
}

/// Re-send `action` while it is refused `busy` (the outbox's backoff).
/// The final result, and every event that came meanwhile.
async fn settle_result(t: &mut SocketTransport, card: &SurfaceState, action_id: &str) -> (SurfaceResult, Vec<SessionEventWire>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut all = Vec::new();
    loop {
        let seen = until(t, |e| matches!(e, SessionEventWire::SurfaceResult(r) if r.action_id == action_id)).await;
        let r = match seen.last() {
            Some(SessionEventWire::SurfaceResult(r)) => r.clone(),
            _ => unreachable!(),
        };
        all.extend(seen);
        if r.reason != Some(SurfaceRefusal::Busy) || tokio::time::Instant::now() > deadline {
            return (r, all);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        t.send_from_self(answer(card, action_id)).await.unwrap();
    }
}

/// Appendix B #1, over the socket: two desks answer the same card at once.
/// One is `accepted`, the other `resolved`; one message reaches history,
/// and a later attach finds one receipt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn two_desks_answer_at_once() {
    let guard = HomeGuard::new();
    let url = spawn_model().await;
    let ctl = control(&guard);
    let d = spawn_daemon(&guard, &url, &ctl).await;
    let (mut a, _) = shared(&d, guard.home.path()).await;
    let card = raise_card(&mut a).await;
    let (mut b, snap) = join(&d, a.session_id(), AttachMode::Mirror).await;
    assert!(snap.surfaces.iter().any(|c| c.id == card.id), "the second desk sees the card");

    a.send_from_self(answer(&card, A1)).await.unwrap();
    b.send_from_self(answer(&card, A2)).await.unwrap();
    let (ra, mut seen) = settle_result(&mut a, &card, A1).await;
    let (rb, _) = settle_result(&mut b, &card, A2).await;
    let outcomes = [(ra.outcome, ra.reason), (rb.outcome, rb.reason)];
    assert!(
        outcomes.contains(&(SurfaceOutcome::Accepted, None))
            && outcomes.contains(&(SurfaceOutcome::Refused, Some(SurfaceRefusal::Resolved))),
        "{outcomes:?}"
    );
    // The winner's turn may already be over while desk A was retrying.
    let over = |s: &[SessionEventWire]| {
        s.iter().skip_while(|e| !matches!(e, SessionEventWire::TurnStarted { .. })).any(|e| matches!(e, SessionEventWire::Idle))
    };
    if !over(&seen) {
        seen.extend(until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await);
    }
    seen.extend(quiet(&mut a, Duration::from_millis(300)).await);
    assert_eq!(turns(&seen), 1, "{seen:#?}");
    let (_, snap) = join(&d, a.session_id(), AttachMode::Observe).await;
    assert_eq!(ui_answers(&snap.conversation.api_messages, &card.id), 1, "one message in history");
    let now = snap.surfaces.iter().find(|c| c.id == card.id).expect("the receipt");
    assert_eq!((now.phase, now.actions.len()), (Phase::Answered, 1), "{now:?}");
    drop(d);
}

/// Appendix B #3, over the socket: a desk's connection drops before its
/// `surface_result` arrives. It reconnects and its outbox re-sends the
/// same action: `duplicate` with the logged `answer_rev`, no second
/// delivery, no second turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_lost_result_is_a_duplicate_after_reconnect() {
    let guard = HomeGuard::new();
    let url = spawn_model().await;
    let ctl = control(&guard);
    let d = spawn_daemon(&guard, &url, &ctl).await;
    let (mut a, _) = shared(&d, guard.home.path()).await;
    let sid = a.session_id().clone();
    let card = raise_card(&mut a).await;
    let (mut watch, _) = join(&d, &sid, AttachMode::Observe).await;
    a.send_from_self(answer(&card, A1)).await.unwrap();
    // The receipt reaches the watcher; desk A never reads its result.
    let mut seen =
        until(&mut watch, |e| matches!(e, SessionEventWire::Surface(s) if s.id == card.id && s.phase == Phase::Answered)).await;
    drop(a);
    seen.extend(until(&mut watch, |e| matches!(e, SessionEventWire::Idle)).await);
    assert_eq!(turns(&seen), 1, "{seen:#?}");

    let (mut a2, snap) = join(&d, &sid, AttachMode::Mirror).await;
    a2.send_from_self(answer(&card, A1)).await.unwrap();
    let r = result_for(&mut a2, A1).await;
    assert_eq!((r.outcome, r.answer_rev), (SurfaceOutcome::Duplicate, Some(1)), "{r:?}");
    let seen = quiet(&mut a2, Duration::from_millis(600)).await;
    assert_eq!(turns(&seen), 0, "{seen:#?}");
    assert_eq!(ui_answers(&history(&snap, &seen), &card.id), 1, "no second delivery");
    drop(d);
}

/// Appendix B #22, over the socket: an observing desk can't answer
/// (`forbidden`); the owner's answer is accepted and the observer gets
/// the receipt; a catalog-0 observer gets no card traffic at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_observer_desk_is_forbidden_and_sees_the_receipt() {
    let guard = HomeGuard::new();
    let url = spawn_model().await;
    let ctl = control(&guard);
    let d = spawn_daemon(&guard, &url, &ctl).await;
    let (mut a, _) = attach(&d, guard.home.path(), true, None).await;
    let card = raise_card(&mut a).await;
    let (mut o, _) = join(&d, a.session_id(), AttachMode::Observe).await;
    let conn = SocketTransport::connect(&d.paths.sock, Hello::new(ClientKind::Attach)).await.unwrap();
    let (mut old, snap_old) =
        SocketTransport::attach(conn, Attach::Existing { session_id: a.session_id().clone(), mode: AttachMode::Observe })
            .await
            .unwrap();
    assert!(snap_old.surfaces.is_empty(), "{:?}", snap_old.surfaces);

    o.send_from_self(answer(&card, A2)).await.unwrap();
    let r = result_for(&mut o, A2).await;
    assert_eq!((r.outcome, r.reason, r.retry), (SurfaceOutcome::Refused, Some(SurfaceRefusal::Forbidden), Some(false)));
    a.send_from_self(answer(&card, A1)).await.unwrap();
    assert_eq!(result_for(&mut a, A1).await.outcome, SurfaceOutcome::Accepted);
    let seen = until(&mut o, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(cards(&seen).iter().any(|c| c.id == card.id && c.phase == Phase::Answered), "{seen:#?}");
    let seen = until(&mut old, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(
        !seen.iter().any(|e| matches!(e, SessionEventWire::Surface(_) | SessionEventWire::SurfaceResult(_))),
        "{seen:#?}"
    );
    drop(d);
}

/// Appendix B #13: an answer accepted just before a reload is in history
/// once after the exec, and nothing wakes for it there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_answer_just_before_a_reload_is_in_context_once_after_it() {
    let guard = HomeGuard::new();
    let url = spawn_model().await;
    let ctl = control(&guard);
    let d = spawn_daemon(&guard, &url, &ctl).await;
    let (mut a, _) = attach(&d, guard.home.path(), true, None).await;
    let card = raise_card(&mut a).await;
    a.send_from_self(answer(&card, A1)).await.unwrap();
    assert_eq!(result_for(&mut a, A1).await.outcome, SurfaceOutcome::Accepted);
    let generation = SocketTransport::reload(&d.paths.sock, true, None, None).await.expect("reload accepted");
    assert_eq!(generation, 2);
    while let Ok(Some(_)) = tokio::time::timeout(Duration::from_secs(10), a.next_event()).await {}
    let snap = tokio::time::timeout(Duration::from_secs(15), a.reconnect(AttachMode::Mirror))
        .await
        .expect("reconnect budget")
        .expect("reconnected");
    let seen = quiet(&mut a, Duration::from_millis(800)).await;
    assert_eq!(turns(&seen), 0, "no wake after the exec: {seen:#?}");
    assert_eq!(ui_answers(&history(&snap, &seen), &card.id), 1, "present once");
    drop(d);
}
