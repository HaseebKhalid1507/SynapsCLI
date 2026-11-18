//! Telling a connection from inside a Synaps sandbox apart (Linux).
//!
//! Every sandboxed process runs with `no_new_privs`: a confined spawn sets
//! `PR_SET_NO_NEW_PRIVS` before `landlock_restrict_self` (the kernel refuses
//! the restrict without it, and a failed `prctl` fails the spawn). The flag
//! is copied by fork, kept across exec and can never be cleared, so
//! `setsid`, a double fork, `nohup` or a move to another cgroup (such as
//! `systemd-run --user --scope`) doesn't remove it from anything the
//! sandboxed process starts.
//!
//! The daemon reads the pid of the process that connected (`SO_PEERCRED`,
//! taken by the kernel at `connect`), and its `NoNewPrivs:` line in
//! `/proc/<pid>/status`. Where the kernel has `SO_PEERPIDFD` (6.5 and
//! later) it also holds a pidfd for that same process and checks it is still
//! alive after the read, so the pid it read was not reused by another
//! process; a kernel that has `SO_PEERPIDFD` but can't give one (the process
//! is gone) is a refusal too. A peer with the flag set, an unknown pid, a
//! status that can't be read or parsed, or a peer gone before the check
//! counts as sandboxed.
//!
//! The check runs only while the sandbox is on under the daemon's base dir
//! ([`sandbox_on_under`]: this daemon's config, the base config or any
//! profile's says `sandbox.confine = enforce`, read again at each accept).
//! Otherwise the uid check alone admits, as without a sandbox, so a client
//! that runs with `no_new_privs` for its own reasons (a systemd unit with
//! `NoNewPrivileges=`, some containers, a flatpak/bubblewrap terminal) is
//! served; with the sandbox on it is refused too.
//!
//! On a kernel with no usable Landlock no process can be in a Synaps
//! sandbox, and nothing is refused. Programs running as the owner that a
//! sandboxed command asks to connect for it (`systemd-run --user`, D-Bus
//! services, a tmux server) are not sandboxed and are admitted (see the
//! sandbox docs, "Not covered").

/// Why the daemon refuses a client from inside a sandbox.
pub const SANDBOXED_PEER: &str = "this client runs inside a Synaps sandbox (its process has no_new_privs set), \
so the daemon refuses it: a sandboxed command can't drive sessions or change the daemon";

/// Added to [`SANDBOXED_PEER`] when the client was gone before the check.
const ENDED: &str = "the client's process ended before it could be checked";

/// Whether the sandbox is on under `base`: a session started there would be
/// confined, under this daemon's `profile` or under any profile folder
/// `<base>/<name>` that has a `config` file (each read the way a session
/// start reads it: the base config's `enforce` holds for every profile, and
/// a config that exists but can't be read counts as `enforce`). Read fresh
/// from those owner-set files at each call. A base folder that exists but
/// can't be listed counts as on.
pub fn sandbox_on_under(base: &std::path::Path, profile: Option<&str>) -> bool {
    use crate::config::{sandbox_settings_in, SandboxConfine};
    let enforced = |profile: Option<&str>| sandbox_settings_in(base, profile).confine == SandboxConfine::Enforce;
    if enforced(profile) {
        return true;
    }
    let mut entries = match std::fs::read_dir(base) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    entries.any(|entry| match entry {
        Err(_) => true,
        Ok(entry) => {
            let name = entry.file_name();
            name.to_str().is_some_and(|name| entry.path().join("config").exists() && enforced(Some(name)))
        }
    })
}

/// Why the daemon must refuse the peer of the connected socket `fd`: only
/// while the sandbox is on under `base` ([`sandbox_on_under`]) and the peer
/// runs inside a sandbox ([`sandboxed_peer`]). `None` admits it.
#[cfg(unix)]
pub fn refusal(fd: std::os::fd::RawFd, base: &std::path::Path, profile: Option<&str>) -> Option<String> {
    if !sandbox_on_under(base, profile) {
        return None;
    }
    sandboxed_peer(fd)
}

/// Why the daemon must refuse the peer of the connected socket `fd`, or
/// `None` when it is not sandboxed.
#[cfg(target_os = "linux")]
pub fn sandboxed_peer(fd: std::os::fd::RawFd) -> Option<String> {
    if crate::sandbox::landlock::usable_abi().is_err() {
        return None;
    }
    let pid = match peer_pid(fd) {
        Ok(pid) if pid > 0 => pid,
        Ok(_) => return Some(format!("{SANDBOXED_PEER} (the client's process is unknown)")),
        Err(e) => return Some(format!("{SANDBOXED_PEER} (the client's credentials can't be read: {e})")),
    };
    // The pidfd pins the process that connected, so a pid reused after it
    // ended can't pass for it.
    let pidfd = match peer_pidfd(fd) {
        Ok(pidfd) => pidfd,
        Err(_) => return Some(format!("{SANDBOXED_PEER} ({ENDED})")),
    };
    let flag = std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .as_deref()
        .and_then(no_new_privs);
    if pidfd.as_ref().is_some_and(|p| !alive(p)) {
        return Some(format!("{SANDBOXED_PEER} ({ENDED})"));
    }
    match flag {
        Some(false) => None,
        Some(true) => Some(SANDBOXED_PEER.to_string()),
        None => Some(format!("{SANDBOXED_PEER} (the client's process status can't be read)")),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn sandboxed_peer(_fd: i32) -> Option<String> {
    None
}

/// The value of the `NoNewPrivs:` line of a `/proc/<pid>/status` text.
pub fn no_new_privs(status: &str) -> Option<bool> {
    let value = status.lines().find_map(|line| line.strip_prefix("NoNewPrivs:"))?;
    match value.trim() {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn peer_pid(fd: std::os::fd::RawFd) -> std::io::Result<libc::pid_t> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into `cred`, which is a
    // valid, writable `ucred`.
    let r = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(cred.pid)
}

/// A pidfd for the process that connected. `Ok(None)` where the kernel has
/// no `SO_PEERPIDFD` (before 6.5); an error where it has one and can't give
/// a pidfd (the process is gone).
#[cfg(target_os = "linux")]
fn peer_pidfd(fd: std::os::fd::RawFd) -> std::io::Result<Option<std::os::fd::OwnedFd>> {
    use std::os::fd::FromRawFd;
    let mut pidfd: libc::c_int = -1;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into `pidfd`.
    let r = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERPIDFD,
            (&mut pidfd as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    if r != 0 {
        let e = std::io::Error::last_os_error();
        return if e.raw_os_error() == Some(libc::ENOPROTOOPT) { Ok(None) } else { Err(e) };
    }
    if pidfd < 0 {
        return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
    }
    // SAFETY: the kernel returned a new pidfd (close-on-exec) that we own.
    Ok(Some(unsafe { std::os::fd::OwnedFd::from_raw_fd(pidfd) }))
}

/// Whether the process `pidfd` refers to still exists (signal 0).
#[cfg(target_os = "linux")]
fn alive(pidfd: &std::os::fd::OwnedFd) -> bool {
    use std::os::fd::AsRawFd;
    // SAFETY: signal 0 only checks; `pidfd` is open; no siginfo is passed.
    let r = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            0,
            std::ptr::null::<libc::siginfo_t>(),
            0u32,
        )
    };
    r == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_is_read_from_the_status_text() {
        assert_eq!(no_new_privs("Name:\tsh\nNoNewPrivs:\t1\nSeccomp:\t0\n"), Some(true));
        assert_eq!(no_new_privs("Name:\tsh\nNoNewPrivs:\t0\n"), Some(false));
        assert_eq!(no_new_privs("Name:\tsh\n"), None);
        assert_eq!(no_new_privs("NoNewPrivs:\tyes\n"), None);
    }

    /// Writes `text` to `path`, making its folder.
    #[cfg(unix)]
    fn put(path: &std::path::Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_sandbox_is_on_when_this_daemons_config_or_any_profile_under_the_base_enforces() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        let on = |profile: Option<&str>| sandbox_on_under(&base, profile);
        assert!(!on(None), "no base folder: nothing confines");
        std::fs::create_dir(&base).unwrap();
        assert!(!on(None) && !on(Some("work")), "no config anywhere");
        put(&base.join("config"), "sandbox.confine = off\n");
        put(&base.join("plain/config"), "model = m\n");
        std::fs::create_dir_all(base.join("plugins/p")).unwrap();
        assert!(!on(None) && !on(Some("work")), "everything off");

        put(&base.join("other/config"), "sandbox.confine = enforce\n");
        assert!(on(None), "a sibling profile's enforce, with this daemon's own config off");
        assert!(on(Some("work")), "the same for a daemon of a profile with no config of its own");
        std::fs::remove_file(base.join("other/config")).unwrap();
        assert!(!on(None), "read fresh: the sibling's enforce is gone");

        put(&base.join("work/config"), "sandbox.confine = enforce\n");
        assert!(on(Some("work")), "this daemon's own profile");
        assert!(on(None), "seen from the default profile's daemon too");
        std::fs::remove_file(base.join("work/config")).unwrap();
        put(&base.join("team/work/config"), "sandbox.confine = enforce\n");
        assert!(on(Some("team/work")), "this daemon's own profile, below the top level");
        std::fs::remove_file(base.join("team/work/config")).unwrap();

        put(&base.join("config"), "sandbox.confine = enforce\n");
        assert!(on(None) && on(Some("work")), "the base config");
        put(&base.join("work/config"), "sandbox.confine = off\n");
        assert!(on(Some("work")), "a profile can't switch the base config's enforce off");
        std::fs::remove_file(base.join("work/config")).unwrap();
        put(&base.join("config"), "sandbox.confine = off\n");

        std::fs::create_dir_all(base.join("odd/config")).unwrap();
        assert!(on(None), "a profile config that exists but can't be read counts as enforce");
        std::fs::remove_dir(base.join("odd/config")).unwrap();
        assert!(!on(None));

        // A base folder that can't be listed counts as on (its config file
        // can still be read: the folder keeps its search right).
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o300)).unwrap();
        let listable = std::fs::read_dir(&base).is_ok();
        let verdict = on(None);
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
        if listable {
            eprintln!("the_sandbox_is_on_...: the base folder stays listable (root?), that case is not checked");
        } else {
            assert!(verdict, "an unlistable base folder counts as on");
        }
    }

    /// The check is gated on the config: with the sandbox off everywhere
    /// under the base a client with no_new_privs (a systemd unit with
    /// `NoNewPrivileges=`, say) is served as without a sandbox; once this
    /// daemon's config or a sibling profile's enforces, it is refused.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_no_new_privs_client_is_refused_only_while_the_sandbox_is_on() {
        use std::os::fd::AsRawFd;
        if !crate::sandbox::testkit::kernel_ok("a_no_new_privs_client_is_refused_only_while_the_sandbox_is_on") {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let sock = base.join("peer.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let verdict = || {
            let mut child = client(&sock, true);
            let (stream, _) = listener.accept().unwrap();
            let verdict = refusal(stream.as_raw_fd(), base, None);
            drop(stream);
            let _ = child.kill();
            let _ = child.wait();
            verdict
        };
        assert_eq!(verdict(), None, "sandbox off: the uid check alone admits");
        put(&base.join("other/config"), "sandbox.confine = enforce\n");
        assert_eq!(verdict().as_deref(), Some(SANDBOXED_PEER), "a sibling profile enforces");
        std::fs::remove_file(base.join("other/config")).unwrap();
        assert_eq!(verdict(), None, "read again at each accept");
        put(&base.join("config"), "sandbox.confine = enforce\n");
        assert_eq!(verdict().as_deref(), Some(SANDBOXED_PEER), "this daemon's own config enforces");
    }

    /// The flag of this test process (printed so a run log shows it: the
    /// controls below need an unsandboxed test runner).
    #[cfg(target_os = "linux")]
    fn own_flag() -> bool {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let flag = no_new_privs(&status).expect("a NoNewPrivs line");
        eprintln!("test process NoNewPrivs: {}", u8::from(flag));
        flag
    }

    /// A child that connects to `sock` and waits; `confine` makes it set
    /// `no_new_privs` (as every sandboxed process has) before exec.
    #[cfg(target_os = "linux")]
    fn client(sock: &std::path::Path, no_new_privs: bool) -> std::process::Child {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new("python3");
        cmd.arg("-c")
            .arg("import socket, sys; s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.recv(1)")
            .arg(sock)
            .stdin(std::process::Stdio::null());
        if no_new_privs {
            // SAFETY: prctl only, in the forked child before exec.
            unsafe {
                cmd.pre_exec(|| {
                    let (zero, one): (libc::c_ulong, libc::c_ulong) = (0, 1);
                    if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, one, zero, zero, zero) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        cmd.spawn().expect("spawn python3")
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_client_with_no_new_privs_is_refused_and_one_without_is_not() {
        use std::os::fd::AsRawFd;
        if !crate::sandbox::testkit::kernel_ok("a_client_with_no_new_privs_is_refused_and_one_without_is_not") {
            return;
        }
        assert!(!own_flag(), "the test runner itself runs with no_new_privs: the control can't run");
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("peer.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        for (flag, refused) in [(true, true), (false, false)] {
            let mut child = client(&sock, flag);
            let (stream, _) = listener.accept().unwrap();
            let verdict = sandboxed_peer(stream.as_raw_fd());
            drop(stream);
            let _ = child.kill();
            let _ = child.wait();
            assert_eq!(verdict.is_some(), refused, "no_new_privs {flag}: {verdict:?}");
            if let Some(why) = verdict {
                assert_eq!(why, SANDBOXED_PEER);
            }
        }
    }

    /// A peer that is gone by the time the daemon looks is refused.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_client_that_ended_before_the_check_is_refused() {
        use std::os::fd::AsRawFd;
        if !crate::sandbox::testkit::kernel_ok("a_client_that_ended_before_the_check_is_refused") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("peer.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let mut child = client(&sock, false);
        let (stream, _) = listener.accept().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        let why = sandboxed_peer(stream.as_raw_fd()).expect("a gone peer is refused");
        assert!(why.starts_with(SANDBOXED_PEER), "{why}");
        // Where the kernel has SO_PEERPIDFD the pidfd tells a gone peer
        // apart, so a reused pid can't stand in for it.
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        if peer_pidfd(a.as_raw_fd()).expect("a live peer").is_some() {
            assert_eq!(why, format!("{SANDBOXED_PEER} ({ENDED})"));
        } else {
            eprintln!("a_client_that_ended_before_the_check_is_refused: no SO_PEERPIDFD on this kernel");
        }
    }
}
