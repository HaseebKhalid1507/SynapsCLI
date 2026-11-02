//! `synaps sandbox check <file>`: the operators' validator for sandbox
//! policy files. Same parse + validation as a spawn; exit 0 = valid, 1 =
//! invalid; prints the rules; writes nothing; hidden from `--help`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Fx {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

/// The fixture lives in cargo's per-target scratch dir, never under /tmp: a
/// base dir under a baseline directory (/tmp, /dev/shm) refuses every policy.
fn fx() -> Fx {
    let tmp = tempfile::Builder::new().prefix("sbx-check-").tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("tempdir");
    let root = std::fs::canonicalize(tmp.path()).expect("canonical");
    for d in ["home/base/sandbox/sessions", "lane"] {
        std::fs::create_dir_all(root.join(d)).expect("mkdir");
    }
    Fx { _tmp: tmp, root }
}

fn synaps(root: &Path, args: &[&str]) -> Output {
    let child = Command::new(env!("CARGO_BIN_EXE_synaps"))
        .args(args)
        .env("HOME", root.join("home"))
        .env("SYNAPS_BASE_DIR", root.join("home/base"))
        .env_remove("SYNAPS_PROFILE")
        .env("TERM", "dumb")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn synaps");
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(out) = rx.try_recv() {
            return out.expect("wait synaps");
        }
        if Instant::now() > deadline {
            let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
            panic!("synaps {args:?} did not exit in 30 s");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn tree(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).expect("read_dir") {
            let p = e.expect("entry").path();
            if p.is_dir() {
                stack.push(p.clone());
            }
            out.push(p);
        }
    }
    out.sort();
    out
}

#[test]
fn sandbox_check_valid_file_exits_0_and_lists_rules() {
    let fx = fx();
    let file = fx.root.join("home/base/sandbox/sessions/s1.conf.new");
    std::fs::write(&file, format!("mode = enforce\nlane = demo\nwrite = {}\n", fx.root.join("lane").display())).unwrap();
    let before = tree(&fx.root);
    let out = synaps(&fx.root, &["sandbox", "check", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "stdout: {stdout}\nstderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.starts_with(&format!("sandbox policy {}: OK", file.display())), "{stdout}");
    assert!(
        stdout.lines().any(|l| l.split_whitespace().collect::<Vec<_>>()
            == ["work", "dir", "write", "(line", "3)", fx.root.join("lane").to_str().unwrap(), "[all-write]"]),
        "{stdout}"
    );
    assert!(stdout.contains("landlock: "), "{stdout}");
    assert_eq!(tree(&fx.root), before, "the validator must write nothing (no logs under the base dir)");
}

#[test]
fn sandbox_check_invalid_file_exits_1_with_the_refusal() {
    let fx = fx();
    let file = fx.root.join("bad.conf");
    // The base dir itself: a sandboxed command could rewrite its own policy.
    std::fs::write(&file, format!("mode = enforce\ncontact = Ask R&R.\nwrite = {}\n", fx.root.join("home/base").display())).unwrap();
    let before = tree(&fx.root);
    let out = synaps(&fx.root, &["sandbox", "check", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains(&format!("{}:3: ", file.display())) && stdout.contains("overlaps the Synaps base dir"), "{stdout}");
    assert!(stdout.contains("agent sees: sandbox policy invalid for this session") && stdout.contains("Ask R&R."), "{stdout}");
    let missing = fx.root.join("missing.conf");
    let out = synaps(&fx.root, &["sandbox", "check", missing.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stdout).contains("no such file"));
    assert_eq!(tree(&fx.root), before, "the validator must write nothing");
}

#[test]
fn sandbox_check_shows_tools_confined_only_when_set() {
    let fx = fx();
    let dir = fx.root.join("home/base/sandbox/sessions");
    let plain = format!("mode = enforce\nwrite = {}\n", fx.root.join("lane").display());
    let (p, c, bad) = (dir.join("p.conf.new"), dir.join("c.conf.new"), dir.join("bad.conf.new"));
    std::fs::write(&p, &plain).unwrap();
    std::fs::write(&c, format!("{plain}tools = confined\ntool = ext__ok__tool\n")).unwrap();
    std::fs::write(&bad, "mode = off\ntools = confined\n").unwrap();
    let before = tree(&fx.root);
    let out = synaps(&fx.root, &["sandbox", "check", p.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(!stdout.lines().any(|l| l.starts_with("tools")), "nothing new without the key: {stdout}");
    let out = synaps(&fx.root, &["sandbox", "check", c.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    let line = stdout.lines().find(|l| l.starts_with("tools = confined: ")).unwrap_or_else(|| panic!("{stdout}"));
    assert!(line.ends_with("the built-in list and 1 named (ext__ok__tool)"), "{line}");
    let out = synaps(&fx.root, &["sandbox", "check", bad.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("`tools = confined` needs `mode = enforce`"), "{stdout}");
    assert_eq!(tree(&fx.root), before, "the validator must write nothing");
}

#[test]
fn sandbox_check_is_hidden_from_help() {
    let fx = fx();
    let out = synaps(&fx.root, &["--help"]);
    assert_eq!(out.status.code(), Some(0));
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("retention"), "help lists the visible subcommands: {help}");
    assert!(!help.contains("sandbox"), "the operator subcommand stays hidden: {help}");
}
