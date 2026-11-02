//! `synaps send --strict`: exact resolution, a receipt for every send, the
//! exit codes 0 sent / 2 usage / 3 not sent / 5 uncertain, and never an
//! inbox. Plain `synaps send` is pinned as the merged plain path (upstream
//! d7a15727b: an addressed event reaches its session or fails; only
//! `--inbox` drops it to the shared inbox).
//!
//! Every test runs the binary against its own scratch run dir, base dir and
//! home. The sessions are endpoints served by this test process.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::Arc;
use std::time::Duration;

use agent_engine::events::registry::SessionEndpoint;
use agent_engine::events::socket::MAX_PAYLOAD;
use agent_engine::events::{EventQueue, Severity};
use serde_json::Value;
use serial_test::serial;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

const HOSTILE: &str = "run $(rm -rf ~) `id` \"quoted\" 'single' \\n\nsecond line";

struct Scratch {
    _tmp: tempfile::TempDir,
    run: PathBuf,
    base: PathBuf,
    home: PathBuf,
}

/// A scratch run dir, base dir and home. The endpoints this process starts
/// read the run dir from the environment, hence `#[serial]`.
fn scratch() -> Scratch {
    let tmp = tempfile::tempdir().unwrap();
    let run = tmp.path().join("run");
    let base = tmp.path().join("base");
    let home = tmp.path().join("home");
    for dir in [&run, &base, &home] {
        std::fs::create_dir_all(dir).unwrap();
    }
    // The canonical path: what the binary compares a card's socket with.
    let run = run.canonicalize().unwrap();
    std::env::set_var("SYNAPS_RUNTIME_DIR", &run);
    Scratch {
        _tmp: tmp,
        run,
        base,
        home,
    }
}

async fn send(s: &Scratch, args: &[&str], stdin: Option<&[u8]>) -> Output {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_synaps"));
    cmd.arg("send")
        .args(args)
        .env("SYNAPS_RUNTIME_DIR", &s.run)
        .env("SYNAPS_BASE_DIR", &s.base)
        .env("HOME", &s.home)
        .env_remove("SYNAPS_PROFILE")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().expect("run synaps");
    if let Some(bytes) = stdin {
        let mut pipe = child.stdin.take().unwrap();
        pipe.write_all(bytes).await.unwrap();
        drop(pipe);
    }
    tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .expect("synaps send finished in time")
        .unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The one JSON line on stdout.
fn json_line(out: &Output) -> Value {
    let text = String::from_utf8(out.stdout.clone()).unwrap();
    assert_eq!(text.lines().count(), 1, "stdout: {text:?}");
    serde_json::from_str(text.trim_end()).unwrap()
}

fn endpoint(id: &str, name: Option<&str>, queue: &Arc<EventQueue>) -> SessionEndpoint {
    SessionEndpoint::start(id, name.map(Into::into), queue.clone()).unwrap()
}

/// A card for `id` naming a listener at `socket`, owned by this process.
fn write_card(run: &Path, id: &str, socket: &Path, receipts: u32) {
    let card = serde_json::json!({
        "kind": "session",
        "session_id": id,
        "name": null,
        "socket_path": socket.to_string_lossy(),
        "pid": std::process::id(),
        "started_at": chrono::Utc::now(),
        "receipts": receipts,
    });
    std::fs::write(run.join(format!("{id}.json")), card.to_string()).unwrap();
}

/// A listener that reads each request to its end and closes without a
/// reply, as a build without receipts does. Returns the connection count.
fn no_reply_listener(path: &Path) -> Arc<std::sync::atomic::AtomicUsize> {
    let listener = UnixListener::bind(path).unwrap();
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = count.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut buf = Vec::new();
            let _ = stream.read_to_end(&mut buf).await;
        }
    });
    count
}

fn inbox_files(s: &Scratch) -> usize {
    std::fs::read_dir(s.base.join("inbox")).map_or(0, |dir| dir.count())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_strict_send_is_confirmed_by_its_receipt() {
    let s = scratch();
    let queue = Arc::new(EventQueue::new(16));
    let _session = endpoint("strict-a", Some("alpha"), &queue);
    for target in ["strict-a", "alpha"] {
        let args = [
            "--strict",
            "--json",
            "--session",
            target,
            "--source",
            "tester",
            "--severity",
            "HIGH",
            HOSTILE,
        ];
        let out = send(&s, &args, None).await;
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        let line = json_line(&out);
        let queued = queue.pop().expect("queued");
        assert_eq!(line["result"], "sent", "{line}");
        assert_eq!(line["target"], target);
        assert_eq!(line["session_id"], "strict-a");
        assert_eq!(line["event_id"], queued.id.as_str());
        assert_eq!(
            (&line["why"], &line["detail"]),
            (&Value::Null, &Value::Null)
        );
        // A child of the session's process is no session itself.
        assert_eq!(line["peer_verified"], false);
        assert_eq!(queued.content.text, HOSTILE);
        assert_eq!(queued.source.source_type, "tester");
        assert_eq!(queued.content.severity, Some(Severity::High));
        let expected = format!(
            "Event sent to session strict-a via socket. Receipt {}.",
            queued.id
        );
        assert!(
            stderr(&out).lines().any(|l| l == expected),
            "{}",
            stderr(&out)
        );
    }
    assert!(queue.is_empty());
    assert!(!s.base.join("inbox").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_message_can_come_from_a_file_or_stdin() {
    let s = scratch();
    let queue = Arc::new(EventQueue::new(16));
    let _session = endpoint("strict-b", None, &queue);
    let body = format!("{HOSTILE}\n\u{1F600} and a tab\tend\n");
    let file = s.base.join("body.txt");
    std::fs::write(&file, &body).unwrap();
    let path = file.to_str().unwrap();
    let out = send(
        &s,
        &["--strict", "--session", "strict-b", "--body-file", path],
        None,
    )
    .await;
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(queue.pop().expect("queued").content.text, body);
    let args = ["--strict", "--session", "strict-b", "--body-file", "-"];
    let out = send(&s, &args, Some(body.as_bytes())).await;
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(queue.pop().expect("queued").content.text, body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_strict_send_never_falls_back() {
    let s = scratch();
    let queue = Arc::new(EventQueue::new(16));
    let _session = endpoint("strict-c1234", Some("gamma-long"), &queue);
    for target in ["strict-c", "strict-", "gamma", "STRICT-C1234", "nobody"] {
        let out = send(&s, &["--strict", "--json", "--session", target, "hi"], None).await;
        assert_eq!(out.status.code(), Some(3), "{target}: {}", stderr(&out));
        let line = json_line(&out);
        assert_eq!(line["result"], "not_sent", "{line}");
        assert_eq!(line["why"], "no_live_session", "{line}");
        assert_eq!(
            (&line["session_id"], &line["event_id"]),
            (&Value::Null, &Value::Null)
        );
        assert!(!stderr(&out).contains("inbox"), "{}", stderr(&out));
    }
    assert!(queue.is_empty());
    assert!(!s.base.join("inbox").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_send_that_cannot_be_confirmed_exits_5_and_an_old_receiver_gets_nothing() {
    let s = scratch();
    let unconfirmed = no_reply_listener(&s.run.join("quiet.sock"));
    write_card(&s.run, "strict-quiet", &s.run.join("quiet.sock"), 1);
    let old = no_reply_listener(&s.run.join("old.sock"));
    write_card(&s.run, "strict-old", &s.run.join("old.sock"), 0);

    let out = send(
        &s,
        &["--strict", "--json", "--session", "strict-quiet", "hi"],
        None,
    )
    .await;
    assert_eq!(out.status.code(), Some(5), "{}", stderr(&out));
    let line = json_line(&out);
    assert_eq!(line["result"], "uncertain", "{line}");
    assert_eq!(line["why"], "no_receipt", "{line}");
    assert!(line["event_id"].is_string(), "{line}");
    assert_eq!(unconfirmed.load(std::sync::atomic::Ordering::SeqCst), 1);

    let out = send(
        &s,
        &["--strict", "--json", "--session", "strict-old", "hi"],
        None,
    )
    .await;
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    let line = json_line(&out);
    assert_eq!(line["why"], "receiver_too_old", "{line}");
    assert_eq!(old.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(!s.base.join("inbox").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_message_larger_than_a_session_takes_is_not_sent() {
    let s = scratch();
    let queue = Arc::new(EventQueue::new(16));
    let _session = endpoint("strict-big", None, &queue);
    // Over the limit by itself, and at the limit but over it once encoded.
    for size in [MAX_PAYLOAD + 1, MAX_PAYLOAD] {
        let file = s.base.join("big.txt");
        std::fs::write(&file, "x".repeat(size)).unwrap();
        let args = [
            "--strict",
            "--json",
            "--session",
            "strict-big",
            "--body-file",
            file.to_str().unwrap(),
        ];
        let out = send(&s, &args, None).await;
        assert_eq!(out.status.code(), Some(3), "{size}: {}", stderr(&out));
        let line = json_line(&out);
        assert_eq!(line["result"], "not_sent", "{line}");
        assert_eq!(line["why"], "oversized", "{line}");
    }
    assert!(queue.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn usage_errors_exit_2_and_send_nothing() {
    let s = scratch();
    let queue = Arc::new(EventQueue::new(16));
    let _session = endpoint("strict-u", None, &queue);
    let file = s.base.join("body.txt");
    std::fs::write(&file, "x").unwrap();
    let file = file.to_str().unwrap();
    let missing = s.base.join("absent.txt");
    let missing = missing.to_str().unwrap();
    let cases: Vec<Vec<&str>> = vec![
        vec!["--strict", "hi"],
        vec!["--strict", "--session", "", "hi"],
        vec!["--strict", "--broadcast", "--session", "strict-u", "hi"],
        vec!["--json", "--session", "strict-u", "hi"],
        vec!["--body-file", file, "--session", "strict-u"],
        vec![
            "--strict",
            "--session",
            "strict-u",
            "--severity",
            "urgent",
            "hi",
        ],
        vec!["--strict", "--session", "strict-u", " \n\t "],
        vec![
            "--strict",
            "--session",
            "strict-u",
            "--body-file",
            file,
            "hi",
        ],
        vec!["--strict", "--session", "strict-u"],
        vec![
            "--strict",
            "--session",
            "strict-u",
            "--source",
            "has space",
            "hi",
        ],
        vec!["--strict", "--session", "strict-u", "--body-file", missing],
    ];
    for case in &cases {
        for json in [false, true] {
            let mut args = case.clone();
            if json && !args.contains(&"--json") {
                args.push("--json");
            }
            let out = send(&s, &args, None).await;
            assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr(&out));
            assert!(
                stderr(&out).starts_with("error: "),
                "{args:?}: {}",
                stderr(&out)
            );
            if json {
                let line = json_line(&out);
                assert_eq!(line["result"], "usage", "{args:?}: {line}");
                assert!(line["detail"].is_string(), "{line}");
            }
        }
    }
    assert!(queue.is_empty());
    assert!(!s.base.join("inbox").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_plain_send_is_unchanged() {
    let s = scratch();
    // The plain path is upstream's (d7a15727b): an event addressed to a
    // session reaches it or fails, and only `--inbox` drops it to the
    // shared inbox.
    let out = send(
        &s,
        &["hello", "--session", "nope", "--severity", "urgent"],
        None,
    )
    .await;
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(
        err.contains("no running session matches \"nope\"; nothing was delivered."),
        "{err}"
    );
    assert!(
        err.contains("Add --inbox to leave it in the shared inbox instead."),
        "{err}"
    );
    assert!(out.stdout.is_empty());
    assert_eq!(inbox_files(&s), 0);

    let out = send(
        &s,
        &[
            "hello",
            "--session",
            "nope",
            "--severity",
            "urgent",
            "--inbox",
        ],
        None,
    )
    .await;
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(
        err.contains("No session matching \"nope\" found — dropping to inbox (--inbox)."),
        "{err}"
    );
    assert!(err.contains("Event written to inbox: "), "{err}");
    assert!(out.stdout.is_empty());
    assert_eq!(inbox_files(&s), 1);

    // A unique prefix still resolves, and the line has no receipt.
    let queue = Arc::new(EventQueue::new(16));
    let _session = endpoint("plain-12345", None, &queue);
    let out = send(&s, &["hi", "--session", "plain-1"], None).await;
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stderr(&out)
            .lines()
            .any(|l| l == "Event sent to session plain-12345 via socket."),
        "{}",
        stderr(&out)
    );
    // A plain send has no receipt: its process can exit before the
    // listener has queued the event (the identity step runs first), so
    // wait for it, bounded.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let queued = loop {
        if let Some(event) = queue.pop() {
            break event;
        }
        assert!(tokio::time::Instant::now() < deadline, "queued");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(queued.content.text, "hi");
    assert_eq!(inbox_files(&s), 1);

    let out = send(&s, &[], None).await;
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}
