//! sudo passwords for the bash tool, through sudo's own askpass hook (Linux).
//!
//! The bash tool runs `sudo` as `sudo -A` with `SUDO_ASKPASS` pointing at a
//! private link to this very binary (`synaps-sudo-askpass`). sudo starts that
//! helper only when it really needs a password, passes its prompt as argv[1]
//! and reads the password from the helper's stdout. The helper asks the bash
//! tool call that set it up, over a Unix socket in a 0700 per-call directory,
//! and the tool call raises the secret prompt.
//!
//! Before anything is shown to the user, both ends check who is asking:
//!
//! * the helper's parent must be the system sudo: started by this user and
//!   running set-uid root (`/proc/<pid>/status`: real uid ours, effective and
//!   saved uid 0, name `sudo`). `/proc/<sudo>/exe` is NOT readable to us:
//!   sudo is a non-dumpable set-uid process, so the kernel refuses (EACCES,
//!   checked against sudo 1.9.17). The credentials in `status` are kept by
//!   the kernel and cannot be forged by a process of the same user; if `exe`
//!   is readable (running as root), it must also be a root-owned set-uid file.
//! * the broker re-checks everything from the socket's peer credentials, so
//!   a process that skips the helper and talks to the socket gains nothing:
//!   peer uid ours, peer runs this binary, peer's parent is the system sudo,
//!   and that sudo descends from the shell this tool call spawned, so a
//!   request cannot cross between tool calls or sessions;
//! * a random token, rotated after every use, must match.
//!
//! The prompt names the command sudo is about to run (its argv, sanitised
//! and bounded), so the user can judge the request. The password goes only
//! to the helper, which writes it to sudo's pipe. It never reaches the
//! command's stdin (which is empty) or the model, and it is redacted from
//! the tool output if the command echoes it.
//!
//! Limits: a command inside the tool call that runs `sudo -A` itself still
//! gets a prompt (that is how sudo works), which is why the prompt shows the
//! command. Code running as the same user can ptrace the helper or point its
//! own sudo at its own askpass program; that is the user's account, not a
//! boundary this can draw. Two sudo password requests racing in one call
//! share one token, so the second is refused.

// The broker side only exists with the bash tool (`command-exec`).
#![cfg_attr(not(feature = "command-exec"), allow(dead_code))]

#[cfg(feature = "command-exec")]
pub(crate) mod broker;
mod helper;
pub(crate) mod procfs;
#[cfg(all(feature = "command-exec", any(test, feature = "testing")))]
pub mod testing;
#[cfg(all(test, feature = "command-exec"))]
mod tests_e2e;

pub use helper::run_helper_if_invoked;

/// File name of the helper link; `main` dispatches on it (argv[0]).
pub(crate) const HELPER_NAME: &str = "synaps-sudo-askpass";
/// The broker's socket, next to the helper link.
pub(crate) const SOCKET_NAME: &str = "sock";
/// The current one-shot token, next to the helper link.
pub(crate) const TOKEN_NAME: &str = "token";
/// Upper bound on a password (and on any reply) crossing the socket.
pub(crate) const MAX_REPLY: usize = 4096;
/// Upper bound on the sudo prompt the helper forwards.
pub(crate) const MAX_PROMPT: usize = 512;

/// Why a password request was refused. Never carries a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal(pub(crate) String);

impl Refusal {
    pub(crate) fn new(why: impl Into<String>) -> Self {
        Self(why.into())
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Who may ask. Production uses [`SystemPolicy`]; tests inject fakes for
/// the parts that need a real set-uid sudo.
pub(crate) trait CallerPolicy: Send + Sync {
    /// `pid` is the system sudo.
    fn check_sudo(&self, pid: u32) -> Result<(), Refusal>;
    /// `pid` runs the askpass helper (this binary).
    fn check_helper(&self, pid: u32) -> Result<(), Refusal>;
    /// `pid` is `root` or one of its descendants.
    fn check_ancestry(&self, pid: u32, root: u32) -> Result<(), Refusal> {
        if procfs::descends_from(pid, root) {
            Ok(())
        } else {
            Err(Refusal::new(
                "sudo is not running inside this bash tool call",
            ))
        }
    }
}

/// The real checks, against `/proc`.
pub(crate) struct SystemPolicy;

impl CallerPolicy for SystemPolicy {
    fn check_sudo(&self, pid: u32) -> Result<(), Refusal> {
        let status = procfs::status(pid)
            .map_err(|e| Refusal::new(format!("cannot inspect the parent process: {e}")))?;
        let exe = procfs::exe_metadata(pid);
        // SAFETY: getuid has no preconditions and cannot fail.
        judge_sudo(&status, unsafe { libc::getuid() }, exe)
    }

    fn check_helper(&self, pid: u32) -> Result<(), Refusal> {
        let ours = procfs::exe_metadata(std::process::id())
            .map_err(|e| Refusal::new(format!("cannot inspect this binary: {e}")))?;
        let theirs = procfs::exe_metadata(pid)
            .map_err(|e| Refusal::new(format!("cannot inspect the requester: {e}")))?;
        use std::os::unix::fs::MetadataExt;
        if (ours.dev(), ours.ino()) == (theirs.dev(), theirs.ino()) {
            Ok(())
        } else {
            Err(Refusal::new(
                "the requester is not the synaps askpass helper",
            ))
        }
    }
}

/// Is the process described by `status` (and its `exe`, normally
/// unreadable) the system sudo, started by the user `my_uid`?
pub(crate) fn judge_sudo(
    status: &procfs::ProcStatus,
    my_uid: u32,
    exe: std::io::Result<std::fs::Metadata>,
) -> Result<(), Refusal> {
    if status.name != "sudo" {
        return Err(Refusal::new("the parent process is not sudo"));
    }
    let [real, effective, saved, _fs] = status.uids;
    if real != my_uid {
        return Err(Refusal::new("sudo was started by another user"));
    }
    if effective != 0 || saved != 0 {
        return Err(Refusal::new("the parent sudo is not running set-uid root"));
    }
    match exe {
        Ok(meta) => {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let mode = meta.permissions().mode();
            if !meta.is_file() || meta.uid() != 0 || mode & 0o4000 == 0 || mode & 0o022 != 0 {
                return Err(Refusal::new(
                    "the parent sudo is not a root-owned set-uid binary",
                ));
            }
            Ok(())
        }
        // The expected case: a set-uid process is not dumpable, so its exe
        // link is closed to its own (unprivileged) user.
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Ok(()),
        Err(e) => Err(Refusal::new(format!("cannot inspect the parent sudo: {e}"))),
    }
}

/// One line of untrusted text made safe to show: control and invisible
/// formatting characters (bidi overrides, zero-width) are escaped, and the
/// result is cut to `max` characters with an ellipsis.
pub(crate) fn sanitize_line(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut count = 0;
    for ch in text.chars() {
        let piece = if is_unsafe_char(ch) {
            ch.escape_unicode().to_string()
        } else {
            ch.to_string()
        };
        let len = piece.chars().count();
        if count + len > max {
            out.push('…');
            return out;
        }
        out.push_str(&piece);
        count += len;
    }
    out
}

fn is_unsafe_char(ch: char) -> bool {
    ch.is_control()
        || matches!(ch,
            '\u{200B}'..='\u{200F}'
            | '\u{2028}'..='\u{202E}'
            | '\u{2060}'..='\u{2069}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}')
}

/// The command sudo will run, from its argv, quoted like a shell would
/// need it, sanitised and bounded.
pub(crate) fn display_command(argv: &[String], max: usize) -> String {
    let mut words = vec!["sudo".to_string()];
    words.extend(argv.iter().skip(1).map(|arg| shell_quote(arg)));
    sanitize_line(&words.join(" "), max)
}

fn shell_quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if plain {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// Title of the secret prompt (a `Secret` prompt: masked input).
pub(crate) const PROMPT_TITLE: &str = "sudo password required";

/// The body of the secret prompt: what sudo will run, then sudo's prompt.
pub(crate) fn prompt_text(sudo_argv: &[String], sudo_prompt: &str) -> String {
    let command = display_command(sudo_argv, 240);
    let asked = sanitize_line(sudo_prompt.trim(), 120);
    if asked.is_empty() {
        format!("sudo is asking for your password to run:\n{command}")
    } else {
        format!("sudo is asking for your password to run:\n{command}\n{asked}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(name: &str, uids: [u32; 4]) -> procfs::ProcStatus {
        procfs::ProcStatus {
            name: name.into(),
            ppid: 1,
            uids,
        }
    }

    fn denied() -> std::io::Result<std::fs::Metadata> {
        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
    }

    /// What sudo 1.9.17 looks like from its own (unprivileged) user.
    #[test]
    fn a_setuid_root_sudo_started_by_us_is_accepted() {
        assert_eq!(
            judge_sudo(&status("sudo", [1000, 0, 0, 0]), 1000, denied()),
            Ok(())
        );
    }

    #[test]
    fn anything_else_is_refused() {
        let cases = [
            (status("bash", [1000, 0, 0, 0]), "not sudo"),
            (
                status("sudo", [1000, 1000, 1000, 1000]),
                "not running set-uid root",
            ),
            (
                status("sudo", [1000, 0, 1000, 0]),
                "not running set-uid root",
            ),
            (status("sudo", [1001, 0, 0, 0]), "another user"),
        ];
        for (st, why) in cases {
            let err = judge_sudo(&st, 1000, denied()).unwrap_err();
            assert!(err.0.contains(why), "{st:?}: {err}");
        }
        let gone = Err(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(judge_sudo(&status("sudo", [1000, 0, 0, 0]), 1000, gone).is_err());
    }

    /// When `exe` is readable it must be a root-owned set-uid file: a file
    /// of ours (not root-owned, not set-uid) is refused.
    #[test]
    fn a_readable_exe_must_be_a_root_owned_setuid_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sudo");
        std::fs::write(&file, b"x").unwrap();
        let meta = std::fs::metadata(&file);
        let err = judge_sudo(&status("sudo", [1000, 0, 0, 0]), 1000, meta).unwrap_err();
        assert!(err.0.contains("root-owned set-uid"), "{err}");
    }

    /// The real policy against real processes: neither this test process
    /// nor a program renamed `sudo` (same name, our uid) is the system sudo.
    #[test]
    fn the_system_policy_refuses_impostors() {
        let me = std::process::id();
        assert!(SystemPolicy
            .check_sudo(me)
            .unwrap_err()
            .0
            .contains("not sudo"));

        // A link, not a copy: exec'ing a freshly written file races other
        // tests' forks (ETXTBSY). The process is still named `sudo`.
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("sudo");
        std::os::unix::fs::symlink("/bin/sleep", &fake).unwrap();
        let mut child = std::process::Command::new(&fake).arg("30").spawn().unwrap();
        let pid = child.id();
        procfs::tests::wait_for_name(pid, "sudo");
        let verdict = SystemPolicy.check_sudo(pid);
        let _ = child.kill();
        let _ = child.wait();
        assert!(verdict.unwrap_err().0.contains("not running set-uid root"));
    }

    #[test]
    fn the_system_policy_knows_its_own_binary() {
        assert_eq!(SystemPolicy.check_helper(std::process::id()), Ok(()));
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        procfs::tests::wait_for_name(child.id(), "sleep");
        let verdict = SystemPolicy.check_helper(child.id());
        let _ = child.kill();
        let _ = child.wait();
        assert!(verdict
            .unwrap_err()
            .0
            .contains("not the synaps askpass helper"));
    }

    #[test]
    fn prompt_text_names_the_command_and_is_bounded() {
        let argv: Vec<String> = ["sudo", "-A", "apt", "install", "it's here"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let text = prompt_text(&argv, "[sudo] password for me: ");
        assert_eq!(
            text,
            "sudo is asking for your password to run:\n\
             sudo -A apt install 'it'\\''s here'\n[sudo] password for me:"
        );

        let long: Vec<String> = std::iter::once("sudo".to_string())
            .chain(std::iter::repeat_n("x".repeat(50), 40))
            .collect();
        let text = prompt_text(&long, &"p".repeat(1000));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].chars().count(), 241, "240 + ellipsis");
        assert!(lines[1].ends_with('…'));
        assert_eq!(lines[2].chars().count(), 121);
    }

    /// A command line cannot draw its own lines, escape sequences or
    /// reordered text into the prompt.
    #[test]
    fn prompt_text_escapes_control_and_invisible_characters() {
        let argv: Vec<String> = ["sudo", "rm\n-rf /\x1b[2J", "a\u{202e}b", "z\u{200b}w"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let text = prompt_text(&argv, "pw\r\n:");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text:?}");
        assert!(lines[1].contains(r"\u{a}"), "{text:?}");
        assert!(lines[1].contains(r"\u{1b}"), "{text:?}");
        assert!(lines[1].contains(r"\u{202e}"), "{text:?}");
        assert!(lines[1].contains(r"\u{200b}"), "{text:?}");
        assert!(!text.contains('\u{1b}') && !text.contains('\u{202e}') && !text.contains('\r'));
        assert_eq!(lines[2], r"pw\u{d}\u{a}:");
    }
}
