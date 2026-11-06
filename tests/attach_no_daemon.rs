//! The TUI (`synaps`, `synaps --attach`) with no daemon to be had: there is
//! no in-process TUI any more, so it says why and how to fix it and exits 3
//! before anything boots — no thin-client re-exec, no allocator diet (the
//! boot ladder, which only the thin path writes: `main`/`reexec`/`alloc`,
//! stays empty), no session journal. Two ways in: `SYNAPS_DAEMON_AUTOSPAWN=0`
//! with nobody running, and an auto-start that fails. `SYNAPS_DAEMON=0` /
//! `SYNAPS_DAEMON_ADOPT=0` no longer select an in-process TUI: a one-line
//! warning, then the same daemon path.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn run_attach(env: &[(&str, &str)]) -> (Option<std::process::ExitStatus>, String, String) {
    run_tui(&["--attach"], env)
}

fn run_tui(args: &[&str], env: &[(&str, &str)]) -> (Option<std::process::ExitStatus>, String, String) {
    let exe = env!("CARGO_BIN_EXE_synaps");
    let home = tempfile::tempdir().unwrap();
    let ladder = home.path().join("ladder.log");
    let run = home.path().join("run");
    let base = home.path().join(".synaps-cli");
    std::fs::create_dir_all(&run).unwrap();
    let mut child = Command::new(exe)
        .args(args)
        .env("HOME", home.path())
        .env("SYNAPS_HOME", home.path())
        .env("SYNAPS_BASE_DIR", &base)
        .env("SYNAPS_RUNTIME_DIR", &run)
        .env_remove("SYNAPS_DAEMON")
        .env_remove("SYNAPS_DAEMON_ADOPT")
        .env_remove("SYNAPS_DAEMON_AUTOSPAWN")
        .env_remove("SYNAPS_PROFILE")
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
    let sessions = std::fs::read_dir(base.join("sessions")).map(|d| d.count()).unwrap_or(0);
    assert_eq!(sessions, 0, "no in-process session was journaled; stderr:\n{err}");
    (status, err, trace)
}

fn assert_refused_before_boot(status: Option<std::process::ExitStatus>, err: &str, trace: &str) {
    assert_eq!(status.and_then(|s| s.code()), Some(3), "stderr:\n{err}");
    assert!(err.contains("no in-process mode"), "{err}");
    assert!(err.contains("daemon --detach"), "names the fix: {err}");
    assert!(!err.contains("running in-process"), "{err}");
    for stage in ["stage=reexec", "stage=main", "stage=alloc"] {
        assert!(!trace.contains(stage), "ladder stage {stage} written before the refusal:\n{trace}");
    }
}

#[test]
fn attach_with_autospawn_off_and_no_daemon_exits_3() {
    let (status, err, trace) = run_attach(&[("SYNAPS_DAEMON_AUTOSPAWN", "0")]);
    assert_refused_before_boot(status, &err, &trace);
    assert!(err.contains("no daemon running"), "{err}");
    assert!(err.contains("synaps daemon --detach"), "{err}");
    assert!(!err.contains("starting daemon"), "{err}");
}

/// Plain `synaps` (adopt) used to boot the in-process TUI quietly when
/// autospawn was off: now the same refusal as `--attach`.
#[test]
fn plain_tui_with_autospawn_off_and_no_daemon_exits_3() {
    for args in [&[][..], &["--no-extensions"], &["--system", "x"]] {
        let (status, err, trace) = run_tui(args, &[("SYNAPS_DAEMON_AUTOSPAWN", "0")]);
        assert_refused_before_boot(status, &err, &trace);
        assert!(err.contains("no daemon running"), "{args:?}: {err}");
        if args == ["--no-extensions"] {
            assert!(err.contains("synaps daemon --no-extensions --detach"), "the private daemon: {err}");
        }
    }
}

/// `SYNAPS_DAEMON=0` / `SYNAPS_DAEMON_ADOPT=0` used to mean "run the TUI
/// in-process". Now: one warning naming the change, then the daemon path
/// (here: refused, autospawn off).
#[test]
fn legacy_in_process_env_is_ignored_with_a_warning() {
    for (k, args) in [("SYNAPS_DAEMON", &["--attach"][..]), ("SYNAPS_DAEMON", &[]), ("SYNAPS_DAEMON_ADOPT", &[])] {
        let (status, err, trace) = run_tui(args, &[(k, "0"), ("SYNAPS_DAEMON_AUTOSPAWN", "0")]);
        assert_refused_before_boot(status, &err, &trace);
        assert!(err.contains(&format!("{k}=0 is ignored")), "{k} {args:?}: {err}");
        assert_eq!(err.matches("is ignored").count(), 1, "one warning line: {err}");
    }
}

/// The auto-start itself fails (the daemon cannot bind its run dir): the
/// reason, the fix, exit 3 — never the old "running in-process" fallback.
#[cfg(unix)]
#[test]
fn failed_autospawn_is_an_error_not_an_in_process_boot() {
    let home = tempfile::tempdir().unwrap();
    // A run dir the daemon cannot create: a regular file in its place.
    let blocker = home.path().join("not-a-dir");
    std::fs::write(&blocker, b"").unwrap();
    let run = blocker.join("run");
    let base = home.path().join(".synaps-cli");
    for args in [&["--attach"][..], &[]] {
        let out = Command::new(env!("CARGO_BIN_EXE_synaps"))
            .args(args)
            .env("HOME", home.path())
            .env("SYNAPS_HOME", home.path())
            .env("SYNAPS_BASE_DIR", &base)
            .env("SYNAPS_RUNTIME_DIR", &run)
            .env_remove("SYNAPS_DAEMON")
            .env_remove("SYNAPS_DAEMON_ADOPT")
            .env_remove("SYNAPS_DAEMON_AUTOSPAWN")
            .env_remove("SYNAPS_PROFILE")
            .env_remove("SYNAPS_CLIENT_REEXECED")
            .env("TERM", "dumb")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(3), "{args:?}: {err}");
        assert!(err.contains("daemon unavailable: could not start it"), "{args:?}: {err}");
        assert!(err.contains("no in-process mode") && err.contains("--foreground"), "{args:?}: {err}");
        assert!(!err.contains("running in-process"), "{err}");
        let sessions = std::fs::read_dir(base.join("sessions")).map(|d| d.count()).unwrap_or(0);
        assert_eq!(sessions, 0, "no in-process session: {err}");
    }
}
