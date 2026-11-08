//! In-process search for the `grep` and `find` tools: ripgrep's crates
//! (`ignore`, `globset`, `grep-regex`, `grep-searcher`) instead of a spawned
//! GNU grep or find. Nothing is spawned, so both behave the same on Linux,
//! macOS and Windows (no Git for Windows needed) and in the hardened
//! `--no-default-features` build.
//!
//! The shared pieces:
//! - [`walk`]: the directory walk. Sorted by file name, so output and any cut
//!   are deterministic. Hidden files are included and links inside the tree
//!   are not followed. `.gitignore`, `.ignore`, `.git/info/exclude` and the
//!   global git excludes apply as in ripgrep. VCS directories are always
//!   skipped, `node_modules` and `target` unless `no_ignore`.
//! - [`check_root`]: a missing path is `Path not found: …`.
//! - [`Control`]: a cancel flag, the turn's cancellation token and a
//!   deadline, checked between entries and on every read ([`CancelReader`]).
//! - [`run_blocking`]: runs a search on the blocking pool. Dropping the tool's
//!   future (cancellation) sets the cancel flag, so the thread stops too.
//! - [`open_regular`]: opens regular files only, never blocking on a FIFO.
//! - [`Tally`]: per-entry errors, reported in a note instead of dropped.
//!
//! `pattern` builds grep's regex and `include` glob and find's name glob;
//! `content` is the grep engine and its `grep -rn` style output; `files` is
//! the find engine and its `find PATH` style output.

pub(crate) mod content;
pub(crate) mod files;
pub(crate) mod pattern;

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ignore::types::Types;
use ignore::{DirEntry, Walk, WalkBuilder};

use crate::{CancellationToken, RuntimeError};

/// Version-control directories, skipped even with `no_ignore`.
pub(crate) const VCS_DIRS: [&str; 6] = [".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];

/// Directories skipped everywhere (in a git repo or not) unless `no_ignore`.
pub(crate) const HARD_EXCLUDES: [&str; 2] = ["node_modules", "target"];

/// What the walk filters.
#[derive(Clone, Default)]
pub(crate) struct WalkOptions {
    /// Search what ignore files and [`HARD_EXCLUDES`] would skip.
    pub(crate) no_ignore: bool,
    /// The session's working directory, for global git excludes.
    pub(crate) cwd: Option<PathBuf>,
    /// File-name filter (grep's `include`).
    pub(crate) types: Option<Types>,
}

/// The walk under `root`, single-threaded and sorted by file name. The root
/// itself (depth 0) is never filtered: an explicit path is always searched.
pub(crate) fn walk(root: &Path, opts: &WalkOptions) -> Walk {
    // ignore reads a bare `-` as stdin (a `<stdin>` entry); here it's a file
    // named `-`, which a relative path with no session cwd can be.
    let dashed;
    let root = if root == Path::new("-") {
        dashed = Path::new(".").join("-");
        dashed.as_path()
    } else {
        root
    };
    let respect = !opts.no_ignore;
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .follow_links(false)
        .parents(respect)
        .ignore(respect)
        .git_ignore(respect)
        .git_global(respect)
        .git_exclude(respect)
        .sort_by_file_name(|a, b| a.cmp(b));
    if let Some(cwd) = &opts.cwd {
        builder.current_dir(cwd.clone());
    }
    if let Some(types) = &opts.types {
        builder.types(types.clone());
    }
    let no_ignore = opts.no_ignore;
    builder.filter_entry(move |entry| !excluded(entry, no_ignore));
    builder.build()
}

/// A VCS directory (any entry with that name, so a worktree's `.git` file
/// too), or a `node_modules`/`target` directory unless `no_ignore`.
fn excluded(entry: &DirEntry, no_ignore: bool) -> bool {
    let name = entry.file_name();
    if VCS_DIRS.iter().any(|vcs| name == OsStr::new(vcs)) {
        return true;
    }
    !no_ignore
        && entry.file_type().is_some_and(|t| t.is_dir())
        && HARD_EXCLUDES.iter().any(|dir| name == OsStr::new(dir))
}

/// The error for a search path that can't be searched at all: `Path not
/// found: …` when it doesn't exist, else the path and the OS error.
pub(crate) fn check_root(root: &Path) -> Result<(), String> {
    match std::fs::metadata(root) {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            Err(format!("Path not found: {}", root.display()))
        }
        Err(err) => Err(format!("{}: {err}", root.display())),
    }
}

/// Why a search stopped before the end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    Cancelled,
    TimedOut,
}

/// A search's cancel flag, turn cancellation and deadline.
#[derive(Clone, Debug)]
pub(crate) struct Control {
    cancel: Arc<AtomicBool>,
    turn: Option<CancellationToken>,
    deadline: Option<Instant>,
}

impl Control {
    pub(crate) fn new(timeout: Duration) -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
            turn: None,
            deadline: Instant::now().checked_add(timeout),
        }
    }

    /// Also stop when `turn` (the tool call's turn) is cancelled.
    pub(crate) fn with_turn(mut self, turn: Option<CancellationToken>) -> Self {
        self.turn = turn;
        self
    }

    pub(crate) fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Whether to stop now, and why.
    pub(crate) fn stop(&self) -> Option<Stop> {
        if self.cancel.load(Ordering::Relaxed)
            || self
                .turn
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
        {
            return Some(Stop::Cancelled);
        }
        match self.deadline {
            Some(deadline) if Instant::now() >= deadline => Some(Stop::TimedOut),
            _ => None,
        }
    }
}

/// Sets the cancel flag when the tool's future is dropped.
struct CancelOnDrop(Control);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Runs `job` on the blocking pool with a [`Control`] whose deadline is
/// `timeout` from now and which follows `turn`. If this future is dropped
/// (the tool call was cancelled), the control's cancel flag is set and the
/// job winds down.
pub(crate) async fn run_blocking<T, F>(
    timeout: Duration,
    turn: Option<CancellationToken>,
    job: F,
) -> crate::Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Control) -> T + Send + 'static,
{
    let control = Control::new(timeout).with_turn(turn);
    let _cancel_on_drop = CancelOnDrop(control.clone());
    tokio::task::spawn_blocking(move || job(&control))
        .await
        .map_err(|e| RuntimeError::Tool(format!("search task failed: {e}")))
}

/// A reader that fails once the search should stop, so a long file (or a
/// slow one, like a file under `/proc`) can't outlive the deadline.
pub(crate) struct CancelReader<'c, R> {
    inner: R,
    control: &'c Control,
}

impl<'c, R> CancelReader<'c, R> {
    pub(crate) fn new(inner: R, control: &'c Control) -> Self {
        Self { inner, control }
    }
}

impl<R: Read> Read for CancelReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.control.stop().is_some() {
            return Err(io::Error::other("search stopped"));
        }
        self.inner.read(buf)
    }
}

/// Opens `path` for reading if it is a regular file. On Unix the open is
/// non-blocking and the type is checked on the open descriptor, so a FIFO
/// (even one swapped in after the walk saw a regular file) can't block.
pub(crate) fn open_regular(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NONBLOCK);
    }
    let file = opts.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    #[cfg(unix)]
    clear_nonblock(&file)?;
    Ok(file)
}

/// Back to blocking reads, now the file is known to be regular (some FUSE
/// file systems honour `O_NONBLOCK` on regular files).
#[cfg(unix)]
fn clear_nonblock(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is an open descriptor owned by `file` for this call.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above; only the status flags change.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Per-file errors: how many, and the first one.
#[derive(Debug, Default)]
pub(crate) struct Tally {
    pub(crate) count: usize,
    pub(crate) first: Option<String>,
}

impl Tally {
    pub(crate) fn add(&mut self, err: impl std::fmt::Display) {
        self.count += 1;
        if self.first.is_none() {
            self.first = Some(err.to_string().replace('\n', "; "));
        }
    }

    /// The footer note, if there were errors.
    pub(crate) fn note(&self) -> Option<String> {
        let first = self.first.as_ref()?;
        let s = if self.count == 1 { "" } else { "s" };
        Some(format!(
            "[{} error{s} while searching, results may be incomplete. First: {first}]",
            self.count
        ))
    }

    /// The tool error when nothing could be read at all.
    pub(crate) fn failure(&self) -> String {
        let first = self.first.as_deref().unwrap_or("search failed");
        match self.count {
            0 | 1 => first.to_string(),
            2 => format!("{first} (and 1 more error)"),
            n => format!("{first} (and {} more errors)", n - 1),
        }
    }
}

/// The footer for output stopped at `cap` bytes, for grep and find. The
/// wording is grep's old one; the total is a lower bound, as the search
/// stops at the cap.
pub(crate) fn truncation_footer(cap: usize) -> String {
    format!("... (output truncated, more than {cap} total bytes)")
}

/// The footer for partial results.
pub(crate) fn stop_note(tool: &str, stop: Stop, timeout: Duration) -> String {
    match stop {
        Stop::TimedOut => format!(
            "... ({tool} timed out after {}s; results are partial, more may be missing)",
            timeout.as_secs()
        ),
        Stop::Cancelled => format!("... ({tool} was cancelled; results are partial)"),
    }
}

/// Appends `note` after a blank line.
pub(crate) fn append_note(text: &mut String, note: &str) {
    if !text.is_empty() {
        let end = text.trim_end_matches('\n').len();
        text.truncate(end);
        text.push_str("\n\n");
    }
    text.push_str(note);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `-` root is walked as the path `./-`, never as ignore's stdin.
    #[test]
    fn a_dash_root_is_a_path_not_stdin() {
        let dashed = Path::new(".").join("-");
        match walk(Path::new("-"), &WalkOptions::default())
            .next()
            .unwrap()
        {
            Ok(entry) => assert_eq!(entry.path(), dashed),
            Err(err) => assert!(
                err.to_string().contains(&dashed.display().to_string()),
                "{err}"
            ),
        }
    }

    #[test]
    fn control_reports_cancel_before_deadline() {
        let control = Control::new(Duration::from_secs(60));
        assert_eq!(control.stop(), None);
        let shared = control.clone();
        shared.cancel();
        assert_eq!(control.stop(), Some(Stop::Cancelled));
        assert_eq!(Control::new(Duration::ZERO).stop(), Some(Stop::TimedOut));
    }

    #[test]
    fn turn_cancellation_stops_the_search() {
        let turn = CancellationToken::new();
        let control = Control::new(Duration::from_secs(60)).with_turn(Some(turn.clone()));
        assert_eq!(control.stop(), None);
        turn.cancel();
        assert_eq!(control.stop(), Some(Stop::Cancelled));
    }

    #[test]
    fn cancel_reader_fails_once_stopped() {
        let control = Control::new(Duration::from_secs(60));
        let mut reader = CancelReader::new(&b"abc"[..], &control);
        let mut buf = [0u8; 8];
        assert_eq!(reader.read(&mut buf).unwrap(), 3);
        control.cancel();
        assert!(reader.read(&mut buf).is_err());
    }

    /// Dropping the future (tool cancellation) stops the blocking job.
    #[tokio::test]
    async fn dropping_the_future_cancels_the_job() {
        let (tx, rx) = std::sync::mpsc::channel();
        let task = tokio::spawn(run_blocking(
            Duration::from_secs(60),
            None,
            move |control| {
                let start = Instant::now();
                while control.stop().is_none() && start.elapsed() < Duration::from_secs(10) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                let _ = tx.send(control.stop());
            },
        ));
        // Let the task start the job, then cancel it.
        tokio::time::sleep(Duration::from_millis(50)).await;
        task.abort();
        let stop = tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(10)))
            .await
            .unwrap()
            .expect("job never finished");
        assert_eq!(stop, Some(Stop::Cancelled));
    }

    #[test]
    fn tally_keeps_the_first_error_on_one_line() {
        let mut tally = Tally::default();
        assert_eq!(tally.note(), None);
        tally.add("a: one\nmore");
        tally.add("b: two");
        assert_eq!(
            tally.note().unwrap(),
            "[2 errors while searching, results may be incomplete. First: a: one; more]"
        );
        assert_eq!(tally.failure(), "a: one; more (and 1 more error)");
        tally.add("c: three");
        assert_eq!(tally.failure(), "a: one; more (and 2 more errors)");
    }

    #[test]
    fn notes_follow_a_blank_line() {
        let mut text = "a:1:x\n".to_string();
        append_note(&mut text, "[n1]");
        append_note(&mut text, "[n2]");
        assert_eq!(text, "a:1:x\n\n[n1]\n\n[n2]");
        let mut empty = String::new();
        append_note(&mut empty, "[n]");
        assert_eq!(empty, "[n]");
    }

    #[cfg(unix)]
    #[test]
    fn open_regular_refuses_a_fifo_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        let err = open_regular(&fifo).unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");
        // A regular file opens and reads normally (blocking mode restored).
        let file = dir.path().join("f");
        std::fs::write(&file, "x").unwrap();
        let mut s = String::new();
        open_regular(&file).unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "x");
    }
}
