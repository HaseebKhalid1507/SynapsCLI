//! Review H2: `--attach` without `SYNAPS_DAEMON=1` falls back to the
//! in-process TUI and must boot like plain `synaps` — no thin-client
//! re-exec, no allocator diet. Proof: the notice is printed and the boot
//! ladder (which only the thin path writes: `main`/`reexec`/`alloc`) is empty.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn attach_without_daemon_is_not_thin() {
    let exe = env!("CARGO_BIN_EXE_synaps");
    let home = tempfile::tempdir().unwrap();
    let ladder = home.path().join("ladder.log");
    let mut child = Command::new(exe)
        .arg("--attach")
        .env("HOME", home.path())
        .env("SYNAPS_HOME", home.path())
        .env_remove("SYNAPS_DAEMON")
        .env_remove("SYNAPS_CLIENT_REEXECED")
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
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let err = reader.join().unwrap();
    assert!(
        err.contains("--attach ignored"),
        "expected the no-daemon notice on stderr, got:\n{err}"
    );
    let trace = std::fs::read_to_string(&ladder).unwrap_or_default();
    for stage in ["stage=reexec", "stage=main", "stage=alloc"] {
        assert!(
            !trace.contains(stage),
            "thin-client ladder stage {stage} written on the in-process fallback:\n{trace}"
        );
    }
}
