//! The child-spend off gate for the session actor (P1b): a session whose
//! turns start no subagent shows its clients, its status query, its attach
//! snapshots and its saved files exactly what it did before child spend.
//! One persisted session runs one turn against a loopback stand-in; the
//! test compares, with fixtures recorded on the pre-P1 code:
//! - the turn's client-wire lines (every event a Full client receives,
//!   in the daemon's socket encoding, with its sequence number);
//! - the status query's JSON;
//! - a Full and a Digest attach snapshot, in the daemon's socket encoding;
//! - the session's saved files (snapshot and journal) after it ends.
//!
//! Every capture is compared as the exact text it was written as (compact
//! JSON in serializer order, file bytes as saved), with narrow textual
//! normalization applied to all of them alike: the session's ids (handle
//! and journal), RFC 3339 timestamps, the stand-in's port, the host pid
//! (this test process's) and the test's working directory (set by the test
//! to the crate's directory; the session index's start record carries it)
//! are replaced by placeholders, as are the turn's id (a random UUID), a
//! roster entry's `attached_at_ms` and a digest's `messages_hash` (it
//! hashes the messages' wall times and turn id); an attach
//! snapshot's client list (built from a map) is compared in id order; and
//! the additive snapshot keys `child_cost`, `child_tokens` and
//! `child_share_exact` (with their values) are cut out wherever they
//! appear (the spec's documented additions to status, digest and attach).
//! Nothing else is normalized; key order is compared.
//!
//! Provenance: the fixtures were written at 5f49b0a7 (before child spend),
//! twice, with identical results, by
//! `SPEND_GATE_WRITE=1 cargo test -p synaps-engine --test spend_gate_actor`.
//! The working directory placeholder came later: the one change to them is
//! `<CWD>` in place of the recording worktree's crate directory, on the
//! session index's start line.
//!
//! Re-recorded at the 0.11.1 parity merge, by the same command: the merged
//! wire's additive fields (session meta's ten, attach `roster` and
//! `client_input`, a client's `name` and `mode` on join, status
//! `cache_ttl`, `forked_from` and `pinned`), the messages' `_synaps_at`
//! and `_synaps_turn` (saved as `_message_times` and `_turn_ids`), a
//! digest's `times`, and the merged default system prompt. Child spend's
//! additions stay cut out, so the gate still holds.

mod session_actor_common;
use session_actor_common::*;

use agent_engine::session::wire::{encode_line, AttachedWire, WireSessionEvent};
use agent_engine::session::{
    AttachMode, ClientKind, ClientMeta, ClientTransport, EndReason, HistoryMode, LocalTransport,
    SessionCommand, SessionConfig, SessionEventWire, SessionQuery,
};
use serial_test::serial;

/// Cut the keys the spec adds to status, digest and attach (compact JSON:
/// never the first key of their object) out of `text`.
fn strip_additive(text: &str) -> String {
    let text = regex::Regex::new(r#","child_cost":[^,}\]]+"#)
        .unwrap()
        .replace_all(text, "");
    let text = regex::Regex::new(r#","child_tokens":\{[^{}]*\}"#)
        .unwrap()
        .replace_all(&text, "");
    regex::Regex::new(r#","child_share_exact":(true|false)"#)
        .unwrap()
        .replace_all(&text, "")
        .into_owned()
}

struct Normalizer {
    ids: Vec<String>,
    port: String,
    /// The working directory, as a JSON string's contents.
    cwd: String,
}

impl Normalizer {
    fn text(&self, text: &str) -> String {
        let mut text = strip_additive(text);
        // First, so no later replacement can touch a part of the path.
        text = text.replace(&self.cwd, "<CWD>");
        for id in self.ids.iter().filter(|id| !id.is_empty()) {
            text = text.replace(id.as_str(), "<session>");
        }
        let text = text.replace(&self.port, "<port>");
        // The host pid (attach meta, the lock file): this test process's.
        let pid = format!(r"\b{}\b", std::process::id());
        let text = regex::Regex::new(&pid).unwrap().replace_all(&text, "<pid>");
        let text =
            regex::Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})")
                .unwrap()
                .replace_all(&text, "<ts>");
        // PARITY (upstream turn ids, 3bc5a7bcf message times, 790d R2): the
        // turn id is a random UUID, a roster entry's attach time is wall
        // epoch ms, and a digest's history hash covers both.
        let text =
            regex::Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
                .unwrap()
                .replace_all(&text, "<uuid>");
        let text = regex::Regex::new(r#""attached_at_ms":\d+"#)
            .unwrap()
            .replace_all(&text, r#""attached_at_ms":<ms>"#);
        regex::Regex::new(r#""messages_hash":\d+"#)
            .unwrap()
            .replace_all(&text, r#""messages_hash":<hash>"#)
            .into_owned()
    }
}

/// `value` as compact JSON in serializer order, one line.
fn json_line(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).unwrap() + "\n"
}

/// A client-wire line (the socket encoding).
fn wire_line(event: SessionEventWire) -> String {
    let line = encode_line(&WireSessionEvent::for_client(event).unwrap()).unwrap();
    if line.ends_with('\n') {
        line
    } else {
        line + "\n"
    }
}

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/spend_gate")
        .join(name)
}

fn check(name: &str, actual: &str) {
    let path = fixture(name);
    if std::env::var_os("SPEND_GATE_WRITE").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    if expected != actual {
        let line = expected
            .lines()
            .zip(actual.lines())
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| expected.lines().count().min(actual.lines().count()));
        panic!(
            "{name} differs from the pre-P1 fixture at line {}:\n  expected: {:?}\n  actual:   {:?}",
            line + 1,
            expected.lines().nth(line),
            actual.lines().nth(line),
        );
    }
}

/// Sets the process's working directory for the test and restores it.
/// The binary has this one test, and it is serial.
struct CwdGuard {
    previous: std::path::PathBuf,
}

impl CwdGuard {
    fn set(dir: &std::path::Path) -> Self {
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();
        Self { previous }
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.previous);
    }
}

/// `dir` as it appears inside a JSON string.
fn json_contents(dir: &std::path::Path) -> String {
    let quoted = serde_json::to_string(&dir.to_string_lossy()).unwrap();
    quoted[1..quoted.len() - 1].to_string()
}

fn meta(history: HistoryMode) -> ClientMeta {
    let mut m = ClientMeta::new(ClientKind::Tui);
    m.history = history;
    m.tail_items = 8;
    m
}

/// A persisted session, one turn, no subagent: the same client-wire
/// lines, status, attach snapshots and saved files as before child spend.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn spend_gate_actor_turn_status_attach_and_files_are_unchanged_without_children() {
    let h = Home::new();
    // A test-controlled working directory: the session index's start record
    // carries it, normalized to <CWD>.
    let cwd = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let _cwd = CwdGuard::set(cwd);
    let (url, _hits) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host
        .create_session(SessionConfig {
            persist: true,
            ..cfg()
        })
        .await
        .unwrap();
    let handle_id = handle.id.as_str().to_string();

    let (mut t, _) =
        LocalTransport::attach_with(handle.clone(), meta(HistoryMode::Full), AttachMode::Mirror)
            .await
            .unwrap();
    t.send_from_self(submit("hello")).await.unwrap();
    let seen = until(&mut t, |e| matches!(e, SessionEventWire::Idle)).await;
    let mut turn = String::new();
    for env in seen {
        turn.push_str(&format!("{} {}", env.seq, wire_line(env.event)));
    }

    t.send_from_self(SessionCommand::Query {
        id: 77,
        query: SessionQuery::Status,
    })
    .await
    .unwrap();
    let seen = until(&mut t, |e| {
        matches!(e, SessionEventWire::QueryResult { id: 77, .. })
    })
    .await;
    let status = match &seen.last().unwrap().event {
        SessionEventWire::QueryResult { value, .. } => value.clone(),
        _ => unreachable!(),
    };
    // Normalize with both the handle's id and the journal's.
    let n = Normalizer {
        ids: vec![
            handle_id,
            status["session"].as_str().unwrap_or_default().to_string(),
        ],
        port: url.rsplit(':').next().unwrap().to_string(),
        cwd: json_contents(&std::env::current_dir().unwrap()),
    };
    check("actor_turn_wire.txt", &n.text(&turn));
    check("actor_status.json", &n.text(&json_line(&status)));

    for (name, history) in [("full", HistoryMode::Full), ("digest", HistoryMode::Digest)] {
        let (other, snap) =
            LocalTransport::attach_with(handle.clone(), meta(history), AttachMode::Observe)
                .await
                .unwrap();
        let mut wire = AttachedWire::new(other.client_id(), snap);
        // The actor lists its clients from a map: compare them in id order.
        wire.clients.sort_by_key(|(id, _)| id.0);
        check(
            &format!("actor_attach_{name}.json"),
            &n.text(&json_line(&wire)),
        );
        drop(other);
    }

    t.send(SessionCommand::End {
        reason: EndReason::ClientQuit,
    })
    .await
    .unwrap();
    until(&mut t, |e| matches!(e, SessionEventWire::Ended { .. })).await;
    drop(t);
    drop(handle);

    // The saved files, in name order.
    let dir = h.dir.path().join(".synaps-cli").join("sessions");
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    let mut saved = String::new();
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let body = std::fs::read_to_string(&path).unwrap_or_else(|e| format!("<unreadable: {e}>"));
        saved.push_str(&format!("=== {}\n", n.text(&name)));
        saved.push_str(&n.text(&body));
        if !body.ends_with('\n') {
            saved.push('\n');
        }
    }
    check("actor_saved_files.txt", &saved);
}
