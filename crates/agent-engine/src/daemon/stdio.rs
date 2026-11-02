//! A daemon outlives whoever started it, so its stdio must not depend on
//! them.
//!
//! An auto-spawned or `--detach`ed daemon used to get a PIPE for stderr,
//! read by the spawning client only until that client exited. The daemon's
//! next stderr write then failed with EPIPE, `eprintln!` turned that into a
//! panic, and a release build (`panic = "abort"`) ended the daemon with no
//! log line. A reload's next image inherits the same fd, so it died the
//! same way at its first stderr write (its config warnings, at boot).
//!
//! Now:
//! - the spawner hands the child an append-only file, [`stderr_log_path`],
//!   and reads a failure reason from the part of it the child wrote;
//! - a daemon whose stderr is still a pipe or a terminal (or closed) once it
//!   is ready moves it onto that same file, and stdout onto `/dev/null`
//!   ([`detach_stdio`]). That covers daemons spawned by older clients, a
//!   daemon started by hand in a terminal, and every reload's next image.
//!
//! The file is never truncated; Synaps does not rotate it.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// File name of the daemon's stderr, in the profile's `logs/` directory.
pub const STDERR_LOG: &str = "daemon-stderr.log";

/// How much of the stderr file a failed spawn reads back for its reason.
pub const SPAWN_TAIL_BYTES: u64 = 64 * 1024;

/// `<base dir>[/<profile>]/logs/daemon-stderr.log`, the path the supervisor
/// unit (`contrib/systemd/synaps-daemon.service`) appends to as well.
pub fn stderr_log_path(profile: Option<&str>) -> PathBuf {
    let mut dir = crate::config::base_dir();
    if let Some(p) = profile {
        dir.push(p);
    }
    dir.join("logs").join(STDERR_LOG)
}

/// Open `path` for appending (created 0600 if missing; its directory too).
/// Never truncates.
pub fn open_append(path: &Path) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut o = OpenOptions::new();
    o.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

/// What the child appended to `path` since it was `start` bytes long: at
/// most the last `max` bytes of that, lossy UTF-8. Empty when unreadable.
pub fn read_tail(path: &Path, start: u64, max: u64) -> String {
    let Ok(mut f) = File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let from = start.max(len.saturating_sub(max));
    if from >= len || f.seek(SeekFrom::Start(from)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::new();
    let _ = f.take(max).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// What a standard fd points at, as far as detaching is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdioKind {
    /// A pipe (FIFO): its reader can go away (EPIPE).
    Pipe,
    /// A terminal: it can hang up (EIO).
    Terminal,
    /// Not open at all: a later `open` could land on it.
    Closed,
    /// A regular file, `/dev/null`, a socket (journald), …: kept.
    Other,
}

impl StdioKind {
    /// Whether a daemon must move this fd off what it points at.
    pub fn needs_detach(self) -> bool {
        !matches!(self, StdioKind::Other)
    }

    fn as_str(self) -> &'static str {
        match self {
            StdioKind::Pipe => "pipe",
            StdioKind::Terminal => "terminal",
            StdioKind::Closed => "closed",
            StdioKind::Other => "other",
        }
    }
}

#[cfg(unix)]
pub fn classify(fd: std::os::fd::RawFd) -> StdioKind {
    // SAFETY: fstat into a zeroed stat buffer; fd is only inspected.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return StdioKind::Closed;
    }
    if st.st_mode & libc::S_IFMT == libc::S_IFIFO {
        return StdioKind::Pipe;
    }
    // SAFETY: isatty only inspects fd.
    if unsafe { libc::isatty(fd) } == 1 {
        return StdioKind::Terminal;
    }
    StdioKind::Other
}

/// Make `fd` refer to `file`'s open file, consuming `file`. `fd` ends up
/// without CLOEXEC, so it survives a reload exec.
///
/// When `fd` was closed, the open of `file` may have landed on that very
/// number: then `dup2(fd, fd)` would do nothing (CLOEXEC stays set) and
/// dropping `file` would close `fd` again. That descriptor is kept instead:
/// CLOEXEC cleared, ownership given up. Otherwise `dup2`, and `file`'s own
/// descriptor is closed.
#[cfg(unix)]
fn point_fd_at(fd: std::os::fd::RawFd, file: File) -> io::Result<()> {
    use std::os::fd::{AsRawFd, IntoRawFd};
    if file.as_raw_fd() == fd {
        clear_cloexec(fd)?;
        let _kept = file.into_raw_fd();
        return Ok(());
    }
    loop {
        // SAFETY: both descriptors are open; dup2 replaces `fd` atomically.
        if unsafe { libc::dup2(file.as_raw_fd(), fd) } >= 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

#[cfg(unix)]
fn clear_cloexec(fd: std::os::fd::RawFd) -> io::Result<()> {
    // SAFETY: fcntl only reads and sets the descriptor flags of `fd`.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// What [`detach_stdio`] did, for the log.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Detached {
    /// stderr was a pipe/terminal/closed and now appends to this file.
    pub stderr_to: Option<PathBuf>,
    /// stderr went to `/dev/null` instead (the file could not be opened).
    pub stderr_to_null: bool,
    pub stderr_was: Option<&'static str>,
    /// stdout was a pipe/terminal/closed and now goes to `/dev/null`.
    pub stdout_to_null: bool,
    pub stdout_was: Option<&'static str>,
    /// Anything that failed (the fd is then left as it was).
    pub errors: Vec<String>,
}

/// After the ready byte: move a pipe/terminal/closed stderr onto the
/// append-only stderr file (or `/dev/null` when that cannot be opened), and
/// such a stdout onto `/dev/null`. A file, `/dev/null` or a socket (a
/// supervisor's journal) is left alone.
#[cfg(unix)]
pub fn detach_stdio(profile: Option<&str>) -> Detached {
    detach_fds(libc::STDOUT_FILENO, libc::STDERR_FILENO, &stderr_log_path(profile))
}

#[cfg(unix)]
pub(crate) fn detach_fds(
    out_fd: std::os::fd::RawFd,
    err_fd: std::os::fd::RawFd,
    stderr_log: &Path,
) -> Detached {
    let mut done = Detached::default();
    let err_kind = classify(err_fd);
    if err_kind.needs_detach() {
        done.stderr_was = Some(err_kind.as_str());
        if err_kind == StdioKind::Terminal {
            // Someone may be watching: say where the rest goes (lossy).
            let line = format!("synaps daemon: stderr continues in {}\n", stderr_log.display());
            // SAFETY: err_fd is open (a terminal); the buffer is valid.
            let _ = unsafe { libc::write(err_fd, line.as_ptr().cast(), line.len()) };
        }
        let target = match open_append(stderr_log) {
            Ok(f) => Ok((f, true)),
            Err(e) => {
                done.errors.push(format!("open {}: {e}", stderr_log.display()));
                open_null().map(|f| (f, false))
            }
        };
        match target.and_then(|(f, is_log)| point_fd_at(err_fd, f).map(|()| is_log)) {
            Ok(true) => done.stderr_to = Some(stderr_log.to_path_buf()),
            Ok(false) => done.stderr_to_null = true,
            Err(e) => done.errors.push(format!("stderr: {e}")),
        }
    }
    let out_kind = classify(out_fd);
    if out_kind.needs_detach() {
        done.stdout_was = Some(out_kind.as_str());
        match open_null().and_then(|f| point_fd_at(out_fd, f)) {
            Ok(()) => done.stdout_to_null = true,
            Err(e) => done.errors.push(format!("stdout: {e}")),
        }
    }
    done
}

#[cfg(unix)]
fn open_null() -> io::Result<File> {
    OpenOptions::new().write(true).open("/dev/null")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [0i32; 2];
        // SAFETY: pipe(2) on a valid 2-slot array.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: both fds are fresh and owned only here.
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    fn write_raw(fd: &OwnedFd, bytes: &[u8]) -> io::Result<usize> {
        // SAFETY: fd is open; the buffer is valid for its length.
        let n = unsafe { libc::write(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    #[test]
    fn classify_tells_pipes_files_sockets_and_closed_fds_apart() {
        let (r, w) = pipe();
        assert_eq!(classify(w.as_raw_fd()), StdioKind::Pipe);
        assert_eq!(classify(r.as_raw_fd()), StdioKind::Pipe);
        let dir = tempfile::tempdir().unwrap();
        let f = File::create(dir.path().join("f")).unwrap();
        assert_eq!(classify(f.as_raw_fd()), StdioKind::Other);
        let null = open_null().unwrap();
        assert_eq!(classify(null.as_raw_fd()), StdioKind::Other);
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        assert_eq!(classify(a.as_raw_fd()), StdioKind::Other, "a journal socket is kept");
        // Not a reused number: other test threads open fds concurrently.
        assert_eq!(classify(-1), StdioKind::Closed);
        drop(r);
    }

    #[test]
    fn a_dead_stderr_pipe_is_moved_onto_the_append_file_and_stdout_onto_null() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("prof").join(STDERR_LOG);
        // Earlier content survives: append, never truncate.
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "earlier generation\n").unwrap();
        // stderr: a pipe whose reader has gone (the spawning client exited).
        let (err_r, err_w) = pipe();
        drop(err_r);
        assert_eq!(
            write_raw(&err_w, b"x").expect_err("dead pipe").kind(),
            io::ErrorKind::BrokenPipe
        );
        // stdout: a pipe with a reader still attached.
        let (out_r, out_w) = pipe();

        let done = detach_fds(out_w.as_raw_fd(), err_w.as_raw_fd(), &log);
        assert_eq!(done.errors, Vec::<String>::new());
        assert_eq!(done.stderr_to.as_deref(), Some(log.as_path()));
        assert_eq!(done.stderr_was, Some("pipe"));
        assert!(done.stdout_to_null);
        assert_eq!(done.stdout_was, Some("pipe"));

        // The same fd number now appends to the file.
        assert_eq!(classify(err_w.as_raw_fd()), StdioKind::Other);
        write_raw(&err_w, b"synaps: config warning: x\n").expect("stderr fd writes to the file");
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "earlier generation\nsynaps: config warning: x\n"
        );
        // stdout no longer feeds the pipe.
        assert_eq!(classify(out_w.as_raw_fd()), StdioKind::Other);
        write_raw(&out_w, b"lost\n").unwrap();
        drop(out_w);
        let mut got = Vec::new();
        File::from(out_r).read_to_end(&mut got).unwrap();
        assert!(got.is_empty(), "stdout still reached the pipe: {got:?}");
    }

    #[test]
    fn a_new_stderr_file_is_private_and_files_and_sockets_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join(STDERR_LOG);
        // A regular-file stderr (a supervisor's append: file) is kept.
        let kept = dir.path().join("kept.log");
        let err_file = open_append(&kept).unwrap();
        let (out_sock, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let done = detach_fds(out_sock.as_raw_fd(), err_file.as_raw_fd(), &log);
        assert_eq!(done, Detached::default());
        assert!(!log.exists(), "nothing to detach, yet the stderr file was created");
        (&err_file).write_all(b"still mine\n").unwrap();
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "still mine\n");

        // A pipe stderr with no file yet: the file is created 0600.
        let (_r, w) = pipe();
        let done = detach_fds(open_null().unwrap().as_raw_fd(), w.as_raw_fd(), &log);
        assert_eq!(done.stderr_to.as_deref(), Some(log.as_path()));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&log).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{mode:o}");
    }

    #[test]
    fn an_unopenable_stderr_file_falls_back_to_dev_null() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the file should be: open fails.
        let log = dir.path().join(STDERR_LOG);
        std::fs::create_dir(&log).unwrap();
        let (_r, w) = pipe();
        let done = detach_fds(open_null().unwrap().as_raw_fd(), w.as_raw_fd(), &log);
        assert!(done.stderr_to_null, "{done:?}");
        assert_eq!(done.stderr_to, None);
        assert_eq!(done.errors.len(), 1, "{done:?}");
        assert_eq!(classify(w.as_raw_fd()), StdioKind::Other);
    }

    #[test]
    fn read_tail_returns_only_what_was_appended_since_start_capped() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join(STDERR_LOG);
        std::fs::write(&log, "old daemon said this\n").unwrap();
        let start = std::fs::metadata(&log).unwrap().len();
        let mut f = open_append(&log).unwrap();
        f.write_all(b"boot\nsynaps daemon: socket in use\n").unwrap();
        assert_eq!(read_tail(&log, start, SPAWN_TAIL_BYTES), "boot\nsynaps daemon: socket in use\n");
        assert_eq!(read_tail(&log, start, 10), "et in use\n", "only the last max bytes");
        let end = std::fs::metadata(&log).unwrap().len();
        assert_eq!(read_tail(&log, end, SPAWN_TAIL_BYTES), "", "nothing new");
        assert_eq!(read_tail(&dir.path().join("missing"), 0, SPAWN_TAIL_BYTES), "");
    }

    fn fd_flags(fd: std::os::fd::RawFd) -> i32 {
        // SAFETY: fcntl F_GETFD only reads the descriptor flags.
        unsafe { libc::fcntl(fd, libc::F_GETFD) }
    }

    #[test]
    fn an_open_that_landed_on_the_closed_fd_keeps_it_open_and_inheritable() {
        // A closed stderr: the open of the stderr file lands on that very
        // number (the lowest free fd). Modelled with a test-owned fd: the
        // file's own descriptor is the target.
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join(STDERR_LOG);
        let f = open_append(&log).unwrap();
        let n = f.as_raw_fd();
        assert_ne!(fd_flags(n) & libc::FD_CLOEXEC, 0, "std opens with O_CLOEXEC");

        point_fd_at(n, f).expect("the fd is kept");

        let flags = fd_flags(n);
        assert!(flags >= 0, "the fd the open landed on was closed again: {}", io::Error::last_os_error());
        assert_eq!(flags & libc::FD_CLOEXEC, 0, "CLOEXEC is still set: a reload exec would close it");
        let line = b"synaps: config warning: x\n";
        // SAFETY: n is open (checked above); the buffer is valid.
        let wrote = unsafe { libc::write(n, line.as_ptr().cast(), line.len()) };
        assert_eq!(wrote, line.len() as isize, "{}", io::Error::last_os_error());
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "synaps: config warning: x\n");
        // SAFETY: n is open and, after point_fd_at, owned by nobody else.
        drop(unsafe { OwnedFd::from_raw_fd(n) });
    }

    #[test]
    fn pointing_another_fd_at_a_file_dups_it_without_cloexec() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join(STDERR_LOG);
        let other = File::create(dir.path().join("other")).unwrap();
        let fd = other.as_raw_fd();
        assert_ne!(fd_flags(fd) & libc::FD_CLOEXEC, 0, "std opens with O_CLOEXEC");
        point_fd_at(fd, open_append(&log).unwrap()).unwrap();
        assert_eq!(fd_flags(fd) & libc::FD_CLOEXEC, 0, "dup2 clears CLOEXEC");
        (&other).write_all(b"via the dup\n").unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "via the dup\n");
        assert_eq!(std::fs::read_to_string(dir.path().join("other")).unwrap(), "");
    }
}
