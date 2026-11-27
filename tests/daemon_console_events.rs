//! Console control events end `synaps daemon --foreground` the way SIGTERM
//! does on Unix: a clean shutdown that removes `daemon.json`. Its own test
//! binary because it may allocate a console, and console control handlers
//! belong to the whole process. Real process, temp HOME, no env mutation.
#![cfg(windows)]

#[path = "support/phase2/mod.rs"]
mod phase2;

use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use agent_engine::daemon::registry;
use windows_sys::Win32::System::Console::{AllocConsole, GenerateConsoleCtrlEvent, GetConsoleWindow, CTRL_BREAK_EVENT};

/// winbase.h; windows-sys has it under `Win32_System_Threading`, a feature
/// this test doesn't otherwise need.
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// Kills the daemon if the test fails before it exits, so no stray process
/// outlives the test (or holds its temp dir).
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Ok(None) = self.0.try_wait() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[test]
fn ctrl_break_shuts_the_foreground_daemon_down_cleanly() {
    // The child must share our console to receive the event.
    // SAFETY: no preconditions.
    if unsafe { GetConsoleWindow() }.is_null() {
        // SAFETY: no preconditions; fails harmlessly if a windowless console exists.
        unsafe { AllocConsole() };
    }

    let home = tempfile::tempdir().unwrap();
    let base = home.path().join(".synaps-cli");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("config"), "").unwrap();
    std::fs::write(base.join("auth.json"), phase2::synthetic_auth_json()).unwrap();
    let run = home.path().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let paths = registry::daemon_paths_in(&run, None);
    let log = home.path().join("daemon.stderr");

    let mut c = Command::new(env!("CARGO_BIN_EXE_synaps"));
    c.args(["daemon", "--foreground"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("SYNAPS_BASE_DIR", &base)
        .env("SYNAPS_RUNTIME_DIR", &run)
        .env("SYNAPS_ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .env("SYNAPS_NO_BOOT_FX", "1")
        .env("TERM", "dumb")
        .current_dir(home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .creation_flags(CREATE_NEW_PROCESS_GROUP);
    for k in ["SystemRoot", "SystemDrive", "WINDIR", "ComSpec", "PATHEXT", "TEMP", "TMP"] {
        if let Some(v) = std::env::var_os(k) {
            c.env(k, v);
        }
    }
    let mut child = KillOnDrop(c.spawn().unwrap());
    let stderr = || std::fs::read_to_string(&log).unwrap_or_default();

    let ready = Instant::now() + Duration::from_secs(30);
    while !(paths.json.exists() && registry::is_alive(&paths)) {
        assert!(child.0.try_wait().unwrap().is_none(), "daemon exited early: {}", stderr());
        assert!(Instant::now() < ready, "daemon not ready in 30 s: {}", stderr());
        std::thread::sleep(Duration::from_millis(50));
    }

    // SAFETY: plain call; the child leads its own process group (id = its pid).
    let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.0.id()) };
    assert_ne!(sent, 0, "GenerateConsoleCtrlEvent: {}", std::io::Error::last_os_error());

    let gone = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(s) = child.0.try_wait().unwrap() {
            break s;
        }
        // The guard kills the daemon as the panic unwinds.
        assert!(Instant::now() < gone, "daemon ignored Ctrl+Break: {}", stderr());
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "a clean exit, not the default handler's STATUS_CONTROL_C_EXIT: {status:?}\n{}", stderr());
    assert!(!paths.json.exists(), "a clean shutdown removes daemon.json");
}
