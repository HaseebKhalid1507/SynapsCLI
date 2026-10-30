//! The real `synaps-sudo-askpass` helper (this binary, started through its
//! link, as sudo would) refuses to ask anything unless its parent is the
//! system sudo: set-uid root and started by this user. Only the refusals
//! can be tested without a real sudo; `scripts/smoke/sudo-askpass.sh`
//! covers the rest by hand.
#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A private dir laid out like the bash tool's: helper link, token, and a
/// listening socket that must never be contacted.
struct Rig {
    dir: tempfile::TempDir,
    listener: std::os::unix::net::UnixListener,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::os::unix::fs::symlink(
        env!("CARGO_BIN_EXE_synaps"),
        dir.path().join("synaps-sudo-askpass"),
    )
    .unwrap();
    let token = dir.path().join("token");
    std::fs::write(&token, "0".repeat(64)).unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(dir.path().join("sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    Rig { dir, listener }
}

impl Rig {
    fn helper(&self) -> PathBuf {
        self.dir.path().join("synaps-sudo-askpass")
    }

    fn contacted(&self) -> bool {
        self.listener.accept().is_ok()
    }
}

fn write_script(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Run a script this process just wrote. Another test thread that forks
/// while the script's write fd is open holds a copy of it until that
/// child's own exec, and exec of a file open for writing fails with
/// ETXTBSY; retry briefly instead of failing on that window.
fn output_of_fresh_script(cmd: &mut Command) -> std::process::Output {
    const ETXTBSY: i32 = 26;
    for _ in 0..50 {
        match cmd.output() {
            Err(e) if e.raw_os_error() == Some(ETXTBSY) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            r => return r.unwrap(),
        }
    }
    cmd.output().unwrap()
}

#[test]
fn started_by_anything_but_sudo_it_refuses_and_prints_nothing() {
    let rig = rig();
    let out = Command::new(rig.helper())
        .arg("[sudo] password for me: ")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(out.stdout.is_empty(), "nothing on stdout");
    assert!(
        stderr.contains("synaps-sudo-askpass: the parent process is not sudo"),
        "{stderr}"
    );
    assert!(
        !rig.contacted(),
        "refused before reaching the bash tool call"
    );
}

/// A program named `sudo` of our own is not the system sudo: it is not
/// running set-uid root.
#[test]
fn a_fake_sudo_is_refused() {
    let rig = rig();
    let bin = tempfile::tempdir().unwrap();
    let fake = bin.path().join("sudo");
    write_script(
        &fake,
        "#!/bin/sh\npw=$(\"$SUDO_ASKPASS\" '[sudo] password for me: ')\necho \"exit:$? got:[$pw]\"\n",
    );
    let out = output_of_fresh_script(Command::new(&fake).env("SUDO_ASKPASS", rig.helper()));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.contains("exit:1 got:[]"), "{stdout} {stderr}");
    assert!(stderr.contains("not running set-uid root"), "{stderr}");
    assert!(!rig.contacted());
}

/// Under its own name the binary is the CLI as always.
#[test]
fn the_cli_is_untouched_under_its_own_name() {
    let out = Command::new(env!("CARGO_BIN_EXE_synaps"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("synaps"));
}
