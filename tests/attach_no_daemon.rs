//! Review H2 (re-based on auto-spawn): `--attach` when the daemon is not
//! started falls back to the in-process TUI and must boot like plain
//! `synaps` — no thin-client re-exec, no allocator diet. Two ways in:
//! `SYNAPS_DAEMON=0` (daemon features off: "--attach ignored" notice) and
//! `SYNAPS_DAEMON_AUTOSPAWN=0` with nobody running (exit 3 with the
//! no-daemon message — the TUI does NOT fall back here; only a failed
//! spawn does, see `tests/daemon_autospawn.rs`). Proof for the fallback:
//! the notice is printed and the boot ladder (which only the thin path
//! writes: `main`/`reexec`/`alloc`) is empty.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn run_attach(env: &[(&str, &str)]) -> (Option<std::process::ExitStatus>, String, String) {
    let exe = env!("CARGO_BIN_EXE_synaps");
    let home = tempfile::tempdir().unwrap();
    let ladder = home.path().join("ladder.log");
    let run = home.path().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let mut child = Command::new(exe)
        .arg("--attach")
        .env("HOME", home.path())
        .env("SYNAPS_HOME", home.path())
        .env("SYNAPS_RUNTIME_DIR", &run)
        .env_remove("SYNAPS_DAEMON")
        .env_remove("SYNAPS_DAEMON_AUTOSPAWN")
        .env_remove("SYNAPS_CLIENT_REEXECED")
        .envs(env.iter().copied())
        .env("SYNAPS_MEM_TRACE", "1")
        .env("SYNAPS_MEM_TRACE_FILE", &ladder)
        .env("TERM", "dumb")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break Some(st);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let err = reader.join().unwrap();
    let trace = std::fs::read_to_string(&ladder).unwrap_or_default();
    (status, err, trace)
}

#[test]
fn attach_with_daemon_off_is_not_thin() {
    let (_, err, trace) = run_attach(&[("SYNAPS_DAEMON", "0")]);
    assert!(
        err.contains("--attach ignored") && err.contains("daemon disabled by SYNAPS_DAEMON=0"),
        "expected the disabled notice on stderr, got:\n{err}"
    );
    for stage in ["stage=reexec", "stage=main", "stage=alloc"] {
        assert!(
            !trace.contains(stage),
            "thin-client ladder stage {stage} written on the in-process fallback:\n{trace}"
        );
    }
}

#[test]
fn attach_with_autospawn_off_and_no_daemon_exits_3() {
    let (status, err, trace) = run_attach(&[("SYNAPS_DAEMON_AUTOSPAWN", "0")]);
    assert_eq!(status.and_then(|s| s.code()), Some(3), "stderr:\n{err}");
    assert!(err.contains("no daemon running"), "{err}");
    assert!(err.contains("synaps daemon --detach"), "{err}");
    assert!(!err.contains("starting daemon"), "{err}");
    for stage in ["stage=reexec", "stage=main", "stage=alloc"] {
        assert!(!trace.contains(stage), "ladder stage {stage} written before the refusal:\n{trace}");
    }
}
