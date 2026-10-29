//! `synaps send --session X` that matches no running session is an error, not
//! a silent drop into the shared inbox: with the inbox gated on an attached
//! input owner, a misaddressed event would otherwise sit there undelivered
//! while the sender saw exit 0. `--inbox` opts back in to the drop.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Run `synaps send` against an empty, private HOME/base/runtime dir.
fn send(home: &Path, args: &[&str]) -> Output {
    let base = home.join(".synaps-cli");
    let run = home.join("run");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::create_dir_all(&run).unwrap();
    Command::new(env!("CARGO_BIN_EXE_synaps"))
        .arg("send")
        .args(args)
        .env("HOME", home)
        .env("SYNAPS_HOME", home)
        .env("SYNAPS_BASE_DIR", &base)
        .env("SYNAPS_RUNTIME_DIR", &run)
        .env("SYNAPS_DAEMON", "0")
        .env_remove("SYNAPS_PROFILE")
        .env_remove("SYNAPS_CLIENT_REEXECED")
        .output()
        .unwrap()
}

fn inbox_files(home: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(home.join(".synaps-cli").join("inbox"))
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

#[test]
fn a_session_that_matches_nothing_exits_non_zero_and_writes_no_inbox_file() {
    let home = tempfile::tempdir().unwrap();
    let out = send(home.path(), &["hello", "--session", "nomatch"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("no running session matches"), "stderr: {stderr}");
    assert!(stderr.contains("--inbox"), "the opt-in is named: {stderr}");
    assert!(inbox_files(home.path()).is_empty(), "nothing may reach the inbox");
}

#[test]
fn a_session_miss_with_inbox_leaves_the_event_in_the_shared_inbox() {
    let home = tempfile::tempdir().unwrap();
    let out = send(home.path(), &["hello", "--session", "nomatch", "--inbox"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    let files = inbox_files(home.path());
    assert_eq!(files.len(), 1, "{files:?}");
    assert!(files[0].extension().is_some_and(|e| e == "json"), "{files:?}");
    assert!(stderr.contains("shared inbox"), "stderr: {stderr}");
}

const SID: &str = "20261019-094545-abcd";

/// A live-looking card for `SID` (this test's pid), whose socket is the
/// legacy `<id>.sock` in the run dir, which nothing may be listening on.
fn card(home: &Path) -> PathBuf {
    let run = home.join("run");
    std::fs::create_dir_all(&run).unwrap();
    let sock = run.join(format!("{SID}.sock"));
    let reg = serde_json::json!({
        "kind": "session",
        "session_id": SID,
        "name": "foreman",
        "socket_path": sock,
        "pid": std::process::id(),
        "started_at": "2026-10-19T09:45:45Z",
    });
    std::fs::write(run.join(format!("{SID}.json")), reg.to_string()).unwrap();
    sock
}

/// The 2026-10-19 incident: axon-run's event for one session hit a socket
/// failure, fell back to the shared inbox, and every live session took it.
/// An addressed event must reach that session or fail, never another one.
#[test]
fn an_addressed_send_whose_socket_refuses_fails_after_retries_and_writes_no_inbox_file() {
    let home = tempfile::tempdir().unwrap();
    card(home.path());
    let out = send(home.path(), &["hello", "--session", SID]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("failed 3 times"), "it retried: {stderr}");
    assert!(stderr.contains("nothing was delivered"), "stderr: {stderr}");
    assert!(stderr.contains("--inbox"), "the opt-in is named: {stderr}");
    assert!(inbox_files(home.path()).is_empty(), "nothing may reach the shared inbox");

    // By name too: the same rule, whatever the query.
    let out = send(home.path(), &["hello", "--session", "foreman"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(inbox_files(home.path()).is_empty());
}

#[test]
fn a_refused_addressed_send_with_inbox_leaves_the_event_in_the_shared_inbox() {
    let home = tempfile::tempdir().unwrap();
    card(home.path());
    let out = send(home.path(), &["hello", "--session", SID, "--inbox"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    assert_eq!(inbox_files(home.path()).len(), 1, "stderr: {stderr}");
    assert!(stderr.contains("shared inbox"), "stderr: {stderr}");
}

#[test]
fn a_broadcast_that_cannot_reach_a_session_exits_non_zero_and_writes_no_inbox_file() {
    let home = tempfile::tempdir().unwrap();
    card(home.path());
    let out = send(home.path(), &["hello", "--broadcast"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains(&format!("Not delivered to session {SID}")), "stderr: {stderr}");
    assert!(inbox_files(home.path()).is_empty(), "a broadcast never re-routes via the inbox");
}

/// A socket that comes up a moment late (a daemon mid-reload, a busy
/// accept loop) still gets the event: the retry is what makes failing
/// instead of falling back safe.
// The late endpoint is a Unix socket bound by hand; on Windows a session
// endpoint is a named pipe.
#[cfg(unix)]
#[test]
fn an_addressed_send_reaches_a_socket_that_comes_up_during_the_retries() {
    use std::io::Read;
    let home = tempfile::tempdir().unwrap();
    let sock = card(home.path());
    let listener = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let (mut conn, _) = l.accept().unwrap();
        let mut got = String::new();
        conn.read_to_string(&mut got).unwrap();
        got
    });
    let out = send(home.path(), &["hello from axon-run", "--session", SID]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    let got = listener.join().unwrap();
    assert!(got.contains("hello from axon-run"), "{got}");
    assert!(inbox_files(home.path()).is_empty());
}
