//! Worker record files under the trusted archive root (dark; §4 "Paths").
//!
//! An explicit `archive_path` is a claim, not a grant. It must name
//! `<root>/<process-dir>/<sa_N>.json` or `<sa_N>.head.json`; the sibling is
//! derived from that name. Below the trusted root nothing is followed: the
//! process directory and both record files are opened relative to the
//! already-open parent directory with `O_NOFOLLOW`, so a component swapped
//! for a symlink at any time fails the open itself (no check-then-open
//! window). The trusted root (it may sit behind a host-configured link) must
//! be a directory of this user that group and other cannot write; below it
//! every opened object must belong to this user with no group or other
//! permission bits: the process directory, and regular single-link record
//! files (no FIFO wait: opens are non-blocking). A file's size is checked on
//! its open handle before anything is allocated, and the read is capped,
//! except for a v1 final (the legacy archive): its writer never had a cap,
//! so it is read whole as before (see [`MAX_RECORD_FILE_BYTES`]).
//!
//! Trust model: the root and its ancestors are host configuration
//! (`<base>/subagent-history`), never chosen by a model, and are trusted; the
//! root is reopened by path and may be reached through a host-configured
//! link. These checks keep a caller-supplied path from reaching anything but
//! a private record of this user below that root. They are not a defence
//! against a malicious writer running as the same user, who can rewrite
//! records in place; record integrity and backend ownership checks
//! ([`super::lineage::admit_record`]) still apply, and reading both siblings
//! is no writer-quiescence barrier (the caller supplies the writer state).
//!
//! Hosts must not relocate, rename or replace an active root (or a process
//! directory below it): a writer keeps writing through the directory it
//! opened, while [`location_of`]/[`location_in`] spell its records under the
//! root's pathname from when it was opened, so after a move every read here
//! finds nothing of the run (a refusal, never another record). See
//! `docs/specs/subagent-context-rollover.md` §3.3 "Root relocation".
//!
//! Portability: Unix only (`openat`, `fstatat`, `O_NOFOLLOW`, `O_NONBLOCK`);
//! other platforms get an `Unsupported` error. Tests run on Linux;
//! macOS is covered only by CI's `cargo check`.
//!
//! Live users: resume admission ([`super::admission::admit_location`]) reads
//! every record file here, v1 archives included, rollover on or off. The
//! collect of a reaped record uses [`load`] only for a v2 final its handle
//! names; a v1 archive is collected by [`super::archive::read_archive`] as
//! before the port (`collect::archived_result`), so resume and collect do
//! not share one admission predicate.
//!
//! Legacy layout: an earlier writer named its process directory
//! `YYYYMMDD-HHMMSS-<pid>`, and real history still holds v1 archives there.
//! [`locate`] admits that spelling for a v1 final only (a head, or a v2
//! final, found there is refused), with every check above unchanged. The
//! writer's own names ([`location_of`], [`location_in`]) never use it.
// Off Unix the tests are compiled out too (`mod tests` below is Unix only).
#![cfg_attr(any(not(test), not(unix)), allow(dead_code))]

use std::path::{Path, PathBuf};

use super::record::{
    is_valid_handle_id, record_file_name, resolve, Binding, Candidate, Expectations, FinalRecord,
    RecordKind, Resolution, ResolveInput, WorkerRecord, WriterState, LEGACY_VERSION,
};
#[cfg(unix)]
use super::record::{parse_final, parse_head};

/// Largest record file read, except a v1 final: a head (v2 only) or a v2
/// final over it is refused before anything is allocated. A v1 final (the
/// legacy archive) is read whole, as before the port: its writer has no cap
/// and real archives exceed this (strict r1 A-B1(a)). It is told apart by a
/// structural probe of a bounded prefix for its top-level `version` member
/// ([`top_level_version`]), which is not JSON validation: an over-limit file
/// the probe calls v1 is read whole and only then fully decoded, so a
/// malformed one, or one with a second `version`, is refused after that read
/// (an allocation caveat, not an admission: strict r2 part A).
pub const MAX_RECORD_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Bytes of an over-limit final read to find its top-level `version`.
const VERSION_SNIFF_BYTES: usize = 64 * 1024;

/// Whether an over-limit final's first bytes say it is a v1 archive: its
/// top-level `version` (within [`VERSION_SNIFF_BYTES`]) is
/// [`LEGACY_VERSION`].
fn opens_as_v1(bytes: &[u8]) -> bool {
    let prefix = &bytes[..bytes.len().min(VERSION_SNIFF_BYTES)];
    top_level_version(prefix) == Some(u64::from(LEGACY_VERSION))
}

/// The value of the top-level `version` member of the JSON object `prefix`
/// opens, when that member and all of its value lie within `prefix`. A
/// structural version probe over the object's members, not JSON validation:
/// the values before it are skipped by their delimiters (a string to its
/// closing quote, escapes skipped but not checked; nested objects and arrays
/// by counting brackets, without matching their kinds; other scalars to the
/// next delimiter, not decoded), each key is decoded as JSON (an escaped
/// spelling counts), and the value is read as
/// [`super::record::parse_final`]'s probe reads it, a `u64`. It stops at the
/// first top-level `version`; a later duplicate is not seen. So any layout is
/// found: compact or pretty-printed, `version` first or later, but a
/// malformed value before the member can still pass: `Some(1)` only permits
/// the whole read, and the full decode ([`super::record::parse_final`]) still
/// decides. `None` when `prefix` opens no object, its member structure breaks
/// before the member (a key, `:` or `,` missing where one must be, a string
/// that does not close), has no such member, or ends first (a trailing `1`
/// may be `10`).
pub(super) fn top_level_version(prefix: &[u8]) -> Option<u64> {
    let mut at = skip_json_ws(prefix, 0);
    if prefix.get(at) != Some(&b'{') {
        return None;
    }
    at += 1;
    loop {
        at = skip_json_ws(prefix, at);
        let key_end = json_string_end(prefix, at)?;
        let key: String = serde_json::from_slice(&prefix[at..key_end]).ok()?;
        at = skip_json_ws(prefix, key_end);
        if prefix.get(at) != Some(&b':') {
            return None;
        }
        at = skip_json_ws(prefix, at + 1);
        let value_end = json_value_end(prefix, at)?;
        if key == "version" {
            return serde_json::from_slice::<u64>(&prefix[at..value_end]).ok();
        }
        at = skip_json_ws(prefix, value_end);
        if prefix.get(at) != Some(&b',') {
            return None;
        }
        at += 1;
    }
}

/// `from` moved past JSON whitespace (space, tab, LF, CR).
fn skip_json_ws(bytes: &[u8], from: usize) -> usize {
    from + bytes[from.min(bytes.len())..]
        .iter()
        .take_while(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        .count()
}

/// The end (exclusive) of the JSON string that starts at `at`, if it closes
/// within `bytes`.
fn json_string_end(bytes: &[u8], at: usize) -> Option<usize> {
    if bytes.get(at) != Some(&b'"') {
        return None;
    }
    let mut i = at + 1;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// The end (exclusive) of the JSON value that starts at `at`, if it ends
/// within `bytes`. A scalar must be followed by a delimiter there.
fn json_value_end(bytes: &[u8], at: usize) -> Option<usize> {
    match bytes.get(at)? {
        b'"' => json_string_end(bytes, at),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut i = at;
            while let Some(&b) = bytes.get(i) {
                match b {
                    b'"' => {
                        i = json_string_end(bytes, i)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(i + 1);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            None
        }
        _ => {
            let len = bytes[at..]
                .iter()
                .position(|b| matches!(b, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r'))?;
            (len > 0).then_some(at + len)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordFileError {
    /// The path is not `<root>/<process-dir>/<record file>`.
    NotUnderRoot,
    /// A component is not a valid process directory or record file name, or
    /// a legacy process directory holds a record other than a v1 final.
    BadName,
    /// A component below the root is a symlink.
    Symlink,
    /// Not a directory or regular file where one is required.
    NotRegular,
    /// Owned by another user, or readable by group/other, or hard-linked.
    NotPrivate,
    /// The file exceeds [`MAX_RECORD_FILE_BYTES`] (its size as reported).
    TooLarge(u64),
    /// The opened record was unlinked or replaced before it was checked.
    Replaced,
    Io(String),
    /// Record files need descriptor-relative no-follow opens (Unix only).
    #[cfg(not(unix))]
    Unsupported,
}

impl std::fmt::Display for RecordFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotUnderRoot => {
                f.write_str("archive_path must name a worker record file under the archive root")
            }
            Self::BadName => f.write_str("archive_path names no worker record file"),
            Self::Symlink => f.write_str("archive_path crosses a symlink"),
            Self::NotRegular => f.write_str("archive_path is not a regular record file"),
            Self::NotPrivate => f.write_str("archive record is not private to this user"),
            Self::TooLarge(n) => write!(
                f,
                "archive record is {n} bytes, over the {MAX_RECORD_FILE_BYTES}-byte limit"
            ),
            Self::Replaced => f.write_str("archive record was replaced while it was being read"),
            Self::Io(e) => write!(f, "archive record unreadable: {e}"),
            #[cfg(not(unix))]
            Self::Unsupported => f.write_str("worker record files require Unix"),
        }
    }
}

impl std::error::Error for RecordFileError {}

/// A validated record locator: trusted root, one process directory, handle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordLocation {
    root: PathBuf,
    process_dir: String,
    handle: String,
    kind: RecordKind,
    /// The process directory has the legacy spelling (v1 finals only).
    legacy: bool,
}

impl RecordLocation {
    pub fn handle(&self) -> &str {
        &self.handle
    }
    /// The process directory name below the root: `<pid>-<uuid>`, or the
    /// legacy `YYYYMMDD-HHMMSS-<pid>` (see the module docs).
    pub fn process_dir(&self) -> &str {
        &self.process_dir
    }
    /// The kind the caller's path named (the sibling is the other kind).
    pub fn kind(&self) -> RecordKind {
        self.kind
    }
    /// The trusted root, canonicalized.
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// The full path of the record this locator names.
    pub fn path(&self) -> PathBuf {
        let name = super::record::record_file_name(&self.handle, self.kind)
            .expect("a located handle is valid");
        self.root.join(&self.process_dir).join(name)
    }
}

/// Validate `raw` (with `~/` expanded) against the trusted `root`. The path
/// is checked by spelling: absolute, without `.`, `..` or empty components,
/// naming exactly `<root>/<pid>-<uuid>/<sa_N>.json` or
/// `.../<sa_N>.head.json`, or a v1 archive's `<sa_N>.json` in a legacy
/// `<root>/YYYYMMDD-HHMMSS-<pid>/`. Only the trusted root is resolved: it is
/// canonicalized (the one I/O step here), and the path may spell it as
/// configured or canonically. Nothing below the root is resolved or followed;
/// [`load`] opens it without following links.
pub fn locate(raw: &str, root: &Path) -> Result<RecordLocation, RecordFileError> {
    let expanded = match raw.strip_prefix("~/") {
        Some(rest) => {
            let home = dirs::home_dir()
                .ok_or_else(|| RecordFileError::Io("no home directory to expand '~'".into()))?;
            format!(
                "{}/{rest}",
                home.to_str().ok_or(RecordFileError::NotUnderRoot)?
            )
        }
        None => raw.to_owned(),
    };
    let parts = lexical_components(&expanded).ok_or(RecordFileError::NotUnderRoot)?;
    let canonical = root
        .canonicalize()
        .map_err(|e| RecordFileError::Io(format!("archive root: {e}")))?;
    let skip = [root, canonical.as_path()]
        .iter()
        .find_map(|r| {
            let prefix = root_components(r)?;
            parts.starts_with(prefix.as_slice()).then_some(prefix.len())
        })
        .ok_or(RecordFileError::NotUnderRoot)?;
    let [process_dir, file] = &parts[skip..] else {
        return Err(RecordFileError::NotUnderRoot);
    };
    let legacy = if is_process_dir_name(process_dir) {
        false
    } else if is_legacy_process_dir_name(process_dir) {
        true
    } else {
        return Err(RecordFileError::BadName);
    };
    let (handle, kind) = parse_record_file_name(file).ok_or(RecordFileError::BadName)?;
    // A legacy directory holds v1 archives, and a v1 archive has no head.
    if legacy && kind == RecordKind::Head {
        return Err(RecordFileError::BadName);
    }
    Ok(RecordLocation {
        root: canonical,
        process_dir: (*process_dir).to_owned(),
        handle,
        kind,
        legacy,
    })
}

/// The location of `handle`'s record of `kind` in `process_dir` below the
/// trusted `root`, under [`locate`]'s name rules for this crate's own process
/// directories (never the legacy spelling): how the writer names what it
/// publishes, so it re-reads it through the same checks.
pub fn location_of(
    root: &Path,
    process_dir: &str,
    handle: &str,
    kind: RecordKind,
) -> Result<RecordLocation, RecordFileError> {
    if !is_process_dir_name(process_dir) {
        return Err(RecordFileError::BadName);
    }
    record_file_name(handle, kind).map_err(|_| RecordFileError::BadName)?;
    let root = root
        .canonicalize()
        .map_err(|e| RecordFileError::Io(format!("archive root: {e}")))?;
    location_in(&root, process_dir, handle, kind)
}

/// [`location_of`] below a `root` that is already canonical (the root a
/// [`RecordDir`](super::record_store::RecordDir) canonicalized when it was
/// opened): the same name rules, no filesystem access.
pub fn location_in(
    root: &Path,
    process_dir: &str,
    handle: &str,
    kind: RecordKind,
) -> Result<RecordLocation, RecordFileError> {
    if !is_process_dir_name(process_dir) {
        return Err(RecordFileError::BadName);
    }
    record_file_name(handle, kind).map_err(|_| RecordFileError::BadName)?;
    Ok(RecordLocation {
        root: root.to_path_buf(),
        process_dir: process_dir.to_owned(),
        handle: handle.to_owned(),
        kind,
        legacy: false,
    })
}

/// Components of an absolute path spelled without `.`, `..`, empty
/// components or a trailing slash; `None` otherwise.
fn lexical_components(path: &str) -> Option<Vec<&str>> {
    let rest = path.strip_prefix('/')?;
    let parts: Vec<&str> = rest.split('/').collect();
    parts
        .iter()
        .all(|p| !p.is_empty() && *p != "." && *p != "..")
        .then_some(parts)
}

/// Normal components of an absolute trusted root (lexically).
fn root_components(root: &Path) -> Option<Vec<&str>> {
    use std::path::Component;
    let mut out = Vec::new();
    for (i, component) in root.components().enumerate() {
        match component {
            Component::RootDir if i == 0 => {}
            Component::Normal(name) => out.push(name.to_str()?),
            _ => return None,
        }
    }
    (root.is_absolute() && !out.is_empty()).then_some(out)
}

/// `<pid>-<uuid>`: this crate's process directory spelling (canonical
/// decimal pid, lowercase hyphenated UUID).
fn is_process_dir_name(name: &str) -> bool {
    let Some((pid, uuid)) = name.split_once('-') else {
        return false;
    };
    is_canonical_pid(pid)
        && uuid::Uuid::try_parse(uuid).is_ok_and(|u| u.hyphenated().to_string() == uuid)
}

/// `YYYYMMDD-HHMMSS-<pid>`: the process directory an earlier writer made,
/// `format!("{}-{}", Utc::now().format("%Y%m%d-%H%M%S"), process::id())`
/// (replaced by `<pid>-<uuid>` in 4e9b2462). A real date and time, then a
/// canonical decimal pid. Only [`locate`] admits it, for a v1 final.
fn is_legacy_process_dir_name(name: &str) -> bool {
    let (Some(stamp), Some(pid)) = (name.get(..15), name.get(16..)) else {
        return false;
    };
    let shaped = stamp
        .bytes()
        .enumerate()
        .all(|(i, b)| if i == 8 { b == b'-' } else { b.is_ascii_digit() });
    shaped
        && name.as_bytes()[15] == b'-'
        && chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%d-%H%M%S").is_ok()
        && is_canonical_pid(pid)
}

/// A `u32` in canonical decimal: digits only, no leading zero.
fn is_canonical_pid(pid: &str) -> bool {
    !pid.is_empty()
        && !pid.starts_with('0')
        && pid.bytes().all(|b| b.is_ascii_digit())
        && pid.parse::<u32>().is_ok()
}

/// `<sa_N>.head.json` or `<sa_N>.json`, with the handle canonical.
fn parse_record_file_name(name: &str) -> Option<(String, RecordKind)> {
    let stem = name.strip_suffix(".json")?;
    let (handle, kind) = match stem.strip_suffix(".head") {
        Some(handle) => (handle, RecordKind::Head),
        None => (stem, RecordKind::Final),
    };
    let handle = handle.to_owned();
    (is_valid_handle_id(&handle) && record_file_name(&handle, kind).ok()? == name)
        .then_some((handle, kind))
}

/// Both candidates of a location, read without following any link below the
/// root. A missing file is `Absent`; a file that cannot be parsed is
/// `Unreadable` (the resolver decides); a provenance or size violation
/// refuses the whole load.
#[derive(Debug)]
pub struct LoadedRecords {
    pub head: Candidate<WorkerRecord>,
    pub final_record: Candidate<FinalRecord>,
}

pub fn load(location: &RecordLocation) -> Result<LoadedRecords, RecordFileError> {
    #[cfg(test)]
    if let Some(limit) = seams::record_limit() {
        return load_limited(location, limit);
    }
    load_limited(location, MAX_RECORD_FILE_BYTES)
}

/// [`load`] with a whole-file byte limit per record.
fn load_limited(location: &RecordLocation, limit: u64) -> Result<LoadedRecords, RecordFileError> {
    #[cfg(unix)]
    {
        unix::load(location, limit)
    }
    #[cfg(not(unix))]
    {
        let _ = (location, limit);
        Err(RecordFileError::Unsupported)
    }
}

/// The steps of a load, in order; test builds call [`seams`] at each one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// The trusted root is open and checked.
    RootOpened,
    /// The process directory is open and checked.
    DirOpened,
    /// A record file is open, not yet checked.
    RecordOpened(RecordKind),
    /// A record file is checked (type, owner, mode, links, size), not read.
    RecordChecked(RecordKind),
}

#[cfg_attr(not(test), allow(unused_variables))]
fn step(step: Step) {
    #[cfg(test)]
    seams::at(step);
}

/// Test-only seams: a callback at each [`Step`] of a load on this thread, so
/// a test can replace a component between two steps deterministically, and
/// an override of the owner the checks expect. Both belong to one [`Guard`]
/// on one thread: the guard cannot leave its thread, a second guard on the
/// same thread is refused, and an owner override needs a guard.
#[cfg(test)]
pub(crate) mod seams {
    use std::cell::{Cell, RefCell};
    use std::marker::PhantomData;

    use super::Step;

    type Hook = Box<dyn FnMut(Step)>;

    thread_local! {
        static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
        static OWNER: Cell<Option<u32>> = const { Cell::new(None) };
        static LIMIT: Cell<Option<u64>> = const { Cell::new(None) };
        static GUARDED: Cell<bool> = const { Cell::new(false) };
    }

    /// Clears both seams of its thread when dropped, also on panic. Neither
    /// `Send` nor `Sync`: it is dropped on the thread whose seams it set.
    pub struct Guard(PhantomData<std::rc::Rc<()>>);

    impl Drop for Guard {
        fn drop(&mut self) {
            HOOK.with(|h| h.borrow_mut().take());
            OWNER.with(|o| o.set(None));
            LIMIT.with(|l| l.set(None));
            GUARDED.with(|g| g.set(false));
        }
    }

    /// Make [`super::load`] on this thread use `limit` instead of
    /// [`super::MAX_RECORD_FILE_BYTES`] until this thread's guard drops, so a
    /// test can put a small record over the cap without writing 64 MiB.
    /// Panics without a guard on this thread.
    pub fn limit_records(limit: u64) {
        assert!(
            GUARDED.with(Cell::get),
            "limit_records needs an active seam guard on this thread"
        );
        LIMIT.with(|l| l.set(Some(limit)));
    }

    pub(super) fn record_limit() -> Option<u64> {
        LIMIT.with(Cell::get)
    }

    /// Call `hook` at every step of loads on this thread until the guard
    /// drops. Panics when this thread already has a guard (no nesting).
    pub fn hook(hook: impl FnMut(Step) + 'static) -> Guard {
        assert!(
            !GUARDED.with(Cell::get),
            "a record-file seam guard is already active on this thread"
        );
        GUARDED.with(|g| g.set(true));
        HOOK.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
        Guard(PhantomData)
    }

    /// Make the checks expect `uid` as the owner (`None`: the effective UID)
    /// until this thread's guard drops. May be called from a hook to change
    /// it between steps. Panics without a guard on this thread.
    pub fn expect_owner(uid: Option<u32>) {
        assert!(
            GUARDED.with(Cell::get),
            "expect_owner needs an active seam guard on this thread"
        );
        OWNER.with(|o| o.set(uid));
    }

    pub(super) fn expected_owner() -> Option<u32> {
        OWNER.with(Cell::get)
    }

    pub(super) fn at(step: Step) {
        HOOK.with(|h| {
            if let Some(hook) = h.borrow_mut().as_mut() {
                hook(step);
            }
        });
    }
}

#[cfg(unix)]
pub(super) mod unix {
    use std::ffi::CString;
    use std::fs::File;
    use std::io::Read as _;
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
    use std::os::unix::fs::MetadataExt as _;

    use super::{
        parse_final, parse_head, record_file_name, step, Candidate, FinalRecord, LoadedRecords,
        RecordFileError, RecordKind, RecordLocation, Step,
    };

    /// The owner every opened object must have: the effective UID.
    pub(in crate::tools::subagent) fn euid() -> u32 {
        #[cfg(test)]
        if let Some(uid) = super::seams::expected_owner() {
            return uid;
        }
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() }
    }

    pub(in crate::tools::subagent) fn io(e: std::io::Error) -> RecordFileError {
        RecordFileError::Io(e.to_string())
    }

    /// `openat(dir, name, flags)` relative to an open directory handle.
    pub(in crate::tools::subagent) fn open_at(
        dir: &OwnedFd,
        name: &str,
        flags: libc::c_int,
    ) -> std::io::Result<OwnedFd> {
        let name = CString::new(name).map_err(std::io::Error::other)?;
        // SAFETY: valid directory fd and NUL-terminated name; the returned fd
        // is owned exactly once below.
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOCTTY,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh descriptor owned by nobody else.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// After a failed no-follow open: was the name a link? Classification
    /// only; the open has already failed, so nothing is followed.
    pub(in crate::tools::subagent) fn is_symlink_at(dir: &OwnedFd, name: &str) -> bool {
        let Ok(name) = CString::new(name) else {
            return false;
        };
        let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: valid fd, NUL-terminated name and a writable stat buffer.
        let rc = unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                name.as_ptr(),
                st.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        // SAFETY: fstatat succeeded, so the buffer is initialized.
        rc == 0 && (unsafe { st.assume_init() }.st_mode & libc::S_IFMT) == libc::S_IFLNK
    }

    pub(in crate::tools::subagent) fn private_dir(
        meta: &std::fs::Metadata,
        mask: u32,
    ) -> Result<(), RecordFileError> {
        if !meta.is_dir() {
            return Err(RecordFileError::NotRegular);
        }
        if meta.uid() != euid() || meta.mode() & mask != 0 {
            return Err(RecordFileError::NotPrivate);
        }
        Ok(())
    }

    /// Open the process directory below the trusted root without following
    /// a link; `None` when it does not exist.
    pub(in crate::tools::subagent) fn open_process_dir(
        location: &RecordLocation,
    ) -> Result<Option<OwnedFd>, RecordFileError> {
        let root = File::open(&location.root).map_err(io)?;
        // The trusted root: a directory of this user that nobody else may
        // write (it may be reached through a host-configured link).
        private_dir(&root.metadata().map_err(io)?, 0o022)?;
        let root = OwnedFd::from(root);
        step(Step::RootOpened);
        let dir = match open_at(
            &root,
            &location.process_dir,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        ) {
            Ok(fd) => fd,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
            Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)) => {
                return Err(if is_symlink_at(&root, &location.process_dir) {
                    RecordFileError::Symlink
                } else {
                    RecordFileError::NotRegular
                });
            }
            Err(e) => return Err(io(e)),
        };
        let file = File::from(dir);
        private_dir(&file.metadata().map_err(io)?, 0o077)?;
        step(Step::DirOpened);
        Ok(Some(OwnedFd::from(file)))
    }

    /// Read one record file relative to the open process directory: no link,
    /// no FIFO wait, regular, private, single-link, at most `limit` bytes
    /// checked on its handle before allocation. `Ok(None)` when absent.
    fn read_record(
        dir: &OwnedFd,
        name: &str,
        kind: RecordKind,
        limit: u64,
    ) -> Result<Option<Vec<u8>>, RecordFileError> {
        let fd = match open_at(
            dir,
            name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        ) {
            Ok(fd) => fd,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
            Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
                return Err(RecordFileError::Symlink)
            }
            Err(e) => {
                return Err(if is_symlink_at(dir, name) {
                    RecordFileError::Symlink
                } else {
                    io(e)
                })
            }
        };
        let mut file = File::from(fd);
        step(Step::RecordOpened(kind));
        let meta = file.metadata().map_err(io)?;
        if !meta.file_type().is_file() {
            return Err(RecordFileError::NotRegular);
        }
        // Unlinked since it was opened (renamed over or removed): not the
        // record that is on disk now, and not one known to be complete.
        if meta.nlink() == 0 {
            return Err(RecordFileError::Replaced);
        }
        if meta.uid() != euid() || meta.mode() & 0o077 != 0 || meta.nlink() != 1 {
            return Err(RecordFileError::NotPrivate);
        }
        if meta.len() > limit {
            // A v1 final keeps its pre-port reading, whole (A-B1(a)); told
            // by its `version` in a bounded prefix, so nothing else is
            // allocated first.
            if kind == RecordKind::Final {
                let mut bytes = Vec::with_capacity(super::VERSION_SNIFF_BYTES);
                (&mut file)
                    .take(super::VERSION_SNIFF_BYTES as u64)
                    .read_to_end(&mut bytes)
                    .map_err(io)?;
                if super::opens_as_v1(&bytes) {
                    step(Step::RecordChecked(kind));
                    file.read_to_end(&mut bytes).map_err(io)?;
                    return Ok(Some(bytes));
                }
            }
            return Err(RecordFileError::TooLarge(meta.len()));
        }
        step(Step::RecordChecked(kind));
        // The size bounds the allocation; a file that grows meanwhile is cut
        // at the limit plus one byte and refused (a v1 final is read on).
        let mut bytes = Vec::with_capacity(meta.len() as usize);
        (&mut file)
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(io)?;
        if bytes.len() as u64 > limit {
            if kind == RecordKind::Final && super::opens_as_v1(&bytes) {
                file.read_to_end(&mut bytes).map_err(io)?;
                return Ok(Some(bytes));
            }
            return Err(RecordFileError::TooLarge(bytes.len() as u64));
        }
        Ok(Some(bytes))
    }

    pub(super) fn load(
        location: &RecordLocation,
        limit: u64,
    ) -> Result<LoadedRecords, RecordFileError> {
        let name =
            |kind| record_file_name(&location.handle, kind).map_err(|_| RecordFileError::BadName);
        let (head_name, final_name) = (name(RecordKind::Head)?, name(RecordKind::Final)?);
        let Some(dir) = open_process_dir(location)? else {
            return Ok(LoadedRecords {
                head: Candidate::Absent,
                final_record: Candidate::Absent,
            });
        };
        let head = read_record(&dir, &head_name, RecordKind::Head, limit)?;
        let final_bytes = read_record(&dir, &final_name, RecordKind::Final, limit)?;
        let final_record = match final_bytes {
            None => Candidate::Absent,
            Some(bytes) => match parse_final(&bytes) {
                Ok(record) => Candidate::Present(record),
                Err(e) => Candidate::Unreadable(e.to_string()),
            },
        };
        // A legacy process directory is read for its v1 final alone: a head
        // or a v2 final there is no record that layout can hold.
        if location.legacy
            && (head.is_some() || matches!(final_record, Candidate::Present(FinalRecord::V2(_))))
        {
            return Err(RecordFileError::BadName);
        }
        Ok(LoadedRecords {
            head: match head {
                None => Candidate::Absent,
                Some(bytes) => match parse_head(&bytes) {
                    Ok(record) => Candidate::Present(record),
                    Err(e) => Candidate::Unreadable(e.to_string()),
                },
            },
            final_record,
        })
    }
}

/// Load a location and resolve it (§3.4) with the caller's writer state and
/// the current binding as an expectation. Selection only: lineage and store
/// checks follow in [`super::lineage::admit_record`].
pub fn resolve_location(
    location: &RecordLocation,
    writer: WriterState,
    binding: Option<Binding>,
) -> Result<Resolution, RecordFileError> {
    let loaded = load(location)?;
    Ok(resolve(ResolveInput {
        head: loaded.head,
        final_record: loaded.final_record,
        writer,
        expect: Expectations {
            handle_id: location.handle.clone(),
            run_id: None,
            binding,
        },
    }))
}

#[cfg(all(test, unix))]
#[path = "record_file_tests.rs"]
mod tests;
