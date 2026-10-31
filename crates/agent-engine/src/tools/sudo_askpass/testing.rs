//! Test seams for the askpass path, without a real set-uid sudo.
//!
//! A fake `sudo` script stands in for sudo (it runs `$SUDO_ASKPASS` the way
//! sudo does), and a small Python program speaks the helper's protocol in
//! place of this binary. [`FakeSudoPolicy`] accepts any parent named `sudo`
//! of our own user instead of demanding set-uid root; the token, the
//! socket and the ancestry check stay real.

use super::{procfs, CallerPolicy, Refusal};
use std::path::{Path, PathBuf};

/// The helper protocol in Python: token and prompt in, `+secret` or
/// `-reason` back, the secret to stdout.
const HELPER_PY: &str = r#"#!/usr/bin/env python3
import os, socket, sys
d = os.path.dirname(sys.argv[0])
with open(os.path.join(d, "token")) as f:
    tok = f.read().strip()
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(os.path.join(d, "sock"))
prompt = sys.argv[1] if len(sys.argv) > 1 else ""
s.sendall((tok + "\n" + prompt.replace("\n", " ") + "\n").encode())
s.shutdown(socket.SHUT_WR)
data = b""
while True:
    chunk = s.recv(4096)
    if not chunk:
        break
    data += chunk
if data[:1] == b"+":
    sys.stdout.buffer.write(data[1:] + b"\n")
    sys.exit(0)
sys.stderr.write("askpass refused: " + data[1:].decode(errors="replace") + "\n")
sys.exit(1)
"#;

/// A `sudo` that behaves like the real one for `-A` / `-n`: `-n` refuses
/// to ask; `-A` runs `$SUDO_ASKPASS PROMPT`, reads the password from its
/// stdout and accepts `swordfish`, then runs the command. With
/// `FAKE_SUDO_ECHO` set it prints the password it got (redaction tests).
const FAKE_SUDO: &str = r#"#!/bin/sh
askpass=0; nonint=0
while [ $# -gt 0 ]; do
  case "$1" in
    -A) askpass=1; shift ;;
    -n) nonint=1; shift ;;
    -k) shift ;;
    --) shift; break ;;
    -*) shift ;;
    *) break ;;
  esac
done
if [ $nonint = 1 ]; then echo "sudo: a password is required" >&2; exit 1; fi
if [ $askpass = 0 ]; then echo "sudo: a terminal is required" >&2; exit 1; fi
pw=$("$SUDO_ASKPASS" "[sudo] password for tester: ") || { echo "sudo: no password was provided" >&2; exit 1; }
if [ -n "$FAKE_SUDO_ECHO" ]; then echo "sudo got: $pw"; fi
if [ "$pw" != swordfish ]; then echo "sudo: 1 incorrect password attempt" >&2; exit 1; fi
"$@"
"#;

fn write_executable(path: &Path, body: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

/// Writes `sudo` (fake) and `askpass.py` (protocol helper) into `dir`;
/// returns the helper's path. Put `dir` first on PATH in the command.
pub fn write_fixtures(dir: &Path) -> std::io::Result<PathBuf> {
    write_executable(&dir.join("sudo"), FAKE_SUDO)?;
    let helper = dir.join("askpass.py");
    write_executable(&helper, HELPER_PY)?;
    Ok(helper)
}

/// Any parent named `sudo` run by us counts as sudo; any requester counts
/// as the helper. Ancestry is checked for real.
pub(crate) struct FakeSudoPolicy;

impl CallerPolicy for FakeSudoPolicy {
    fn check_sudo(&self, pid: u32) -> Result<(), Refusal> {
        let status = procfs::status(pid).map_err(|e| Refusal::new(e.to_string()))?;
        // SAFETY: getuid has no preconditions.
        if status.name == "sudo" && status.uids[0] == unsafe { libc::getuid() } {
            Ok(())
        } else {
            Err(Refusal::new("the parent process is not sudo"))
        }
    }

    fn check_helper(&self, _pid: u32) -> Result<(), Refusal> {
        Ok(())
    }
}

/// Run `command` as the bash tool does, with the askpass broker active,
/// `helper` (from [`write_fixtures`]) as sudo's askpass program and
/// [`FakeSudoPolicy`] judging who asks.
pub async fn run_bash_with_test_askpass(
    command: &str,
    timeout: Option<u64>,
    ctx: crate::tools::ToolContext,
    helper: &Path,
) -> crate::Result<String> {
    let config = super::broker::AskpassConfig::for_tests(
        helper.to_path_buf(),
        std::sync::Arc::new(FakeSudoPolicy),
    );
    let broker = super::broker::PreparedBroker::prepare(&config)
        .map_err(|e| crate::RuntimeError::Tool(format!("askpass setup: {e}")))?;
    crate::tools::bash::run_bash(command, timeout, ctx, Some(broker)).await
}
