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
