//! The kernel-audit denial reader: the second denial detector.
//!
//! The output matcher ([`super::spawn`]) reads what a command printed; this
//! reader follows the kernel's own record of each write Landlock refused, so
//! it also sees refusals a tool reports in its own words, or not at all.
//!
//! The daemon follows the audit log ([`super::policy::DEFAULT_AUDIT_LOG`]):
//! the first enforcing spawn opens it and moves to its end, synchronously,
//! before that spawn forks; a thread then reads the new lines. Only
//! Landlock's two record types are read: 1423 (`LANDLOCK_ACCESS`: one
//! refusal, with the blockers and the path) and 1424 (`LANDLOCK_DOMAIN`: a
//! domain's allocation, which comes with its first refusal and names the
//! restricting thread's pid, exe and comm, our spawn [`Tag`]; and its end,
//! with the kernel's count). Every other record (SYSCALL, CWD, PATH,
//! PROCTITLE, …) and the interpreted fields after `\x1d` are never read.
//!
//! A domain is this daemon's when its exe is the daemon's own (a trailing
//! ` (deleted)` ignored on both sides: an install replaces the binary), its
//! comm is a tag this daemon gave out, its pid is that spawn's child or the
//! daemon itself (a PTY spawn restricts a daemon thread), and it is no older
//! than the spawn. Each refusal of such a domain logs one
//! `sandbox.denial … src=audit` line (repeats of the same op on the same path
//! within a minute fold into one later line with their count), and its end
//! logs `sandbox.domain_end`. Other daemons' and test binaries' domains are
//! skipped; domains that can't be joined are only counted.
//!
//! Enforcement never waits for any of this: the open is the one synchronous
//! step, a failed open logs one line and spawns go on, and memory is bounded
//! (the registry, the domain map, the records waiting for their domain, the
//! fold map and the line length all have caps).

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::landlock::{valid_tag, Tag};
use super::spawn::classify;

/// The longest audit line read; a longer one is skipped and counted.
pub const MAX_LINE: usize = 64 * 1024;
/// Spawns the registry holds at most.
pub const REGISTRY_CAP: usize = 4096;
/// How long a spawn stays joinable after its last handle is dropped (its
/// children may outlive it).
pub const KEEP_AFTER_EXIT_MS: u64 = 15 * 60 * 1000;
/// Repeats of one (spawn, op, path) within this window fold into one line.
pub const DEDUP_MS: u64 = 60 * 1000;
/// Fold windows held at most; at the cap the oldest is flushed.
pub const DEDUP_CAP: usize = 4096;
/// Domains remembered at most (oldest forgotten first).
pub const DOMAIN_CAP: usize = 4096;
/// Records held at most while they wait for their domain's record.
pub const PENDING_CAP: usize = 1024;
/// How long a record waits for its domain's record (or a domain for its
/// spawn's pid) before it is counted as unattributed.
pub const PENDING_MS: u64 = 10 * 1000;
/// The `sandbox.audit unattributed` and `dropped` lines: each at most this
/// often, only when nonzero.
pub const REPORT_MS: u64 = 10 * 60 * 1000;
/// A failed open is retried at most this often, by a later enforcing spawn.
pub const RETRY_MS: u64 = 10 * 60 * 1000;
/// How often the reader thread reads the log.
pub const POLL: Duration = Duration::from_secs(1);
/// The most bytes one poll reads (16 reads of [`CHUNK`]); the rest waits for
/// the next poll.
pub const POLL_BYTES: usize = 16 * CHUNK;
/// After this many lines a poll stops at the end of its current read.
pub const POLL_LINES: usize = 10_000;
/// How long the log's name may be missing or not a regular file (a
/// rotation's moment) before the reader says it is unavailable.
pub const NAME_GRACE_MS: u64 = 10 * 1000;
/// New `sandbox.denial` lines one session may log a minute; past that a
/// refusal is only counted.
pub const SESSION_LINES_PER_MIN: u64 = 60;
/// New `sandbox.denial` lines all sessions together may log a minute.
pub const HOST_LINES_PER_MIN: u64 = 600;
/// The most blockers one record may name.
pub const MAX_BLOCKERS: usize = 64;
/// The most `key=value` fields a record may have after its stamp.
pub const MAX_FIELDS: usize = 32;
/// The kernel stamps audit records from its coarse clock, which can run up to
/// a tick behind the clock a spawn's time is read from.
const CLOCK_SLACK_MS: u64 = 50;
/// One read of the log.
const CHUNK: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Warn,
    Info,
}

/// Where the reader's lines go (the daemon: WARN and INFO lines on the
/// sandbox log target).
pub type Sink = Arc<dyn Fn(Level, &str) + Send + Sync>;
/// Unix time in ms (a seam for tests).
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Unix time in ms.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Milliseconds on the monotonic clock (since this process first asked).
pub fn mono_ms() -> u64 {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    u64::try_from(START.get_or_init(std::time::Instant::now).elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

// ── Records ──────────────────────────────────────────────────────────────

/// One Landlock audit record, raw fields only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// The record's time stamp (unix ms).
    pub time_ms: u64,
    pub domain: u64,
    pub body: Body,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    /// 1423: a refusal. For a create or remove the path is the PARENT dir.
    Access { blockers: Vec<String>, path: Option<Vec<u8>> },
    /// 1424 `status=allocated`: the domain's first refusal brought it.
    Allocated { pid: u32, exe: Option<Vec<u8>>, comm: Option<Vec<u8>> },
    /// 1424 `status=deallocated`: the domain ended.
    Deallocated { denials: u64 },
    /// 1423 with no `fs.*` blocker (ptrace, a scope): only counted.
    NonFs,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    /// Not a Landlock record (or a 1424 of a status this doesn't know): not
    /// read further.
    Other,
    /// A Landlock record this can't read: dropped and counted.
    Malformed,
    Record(Record),
}

/// The record types read, in both spellings (auditd before and after it knew
/// Landlock's names).
const ACCESS_TYPES: [&[u8]; 2] = [b"type=UNKNOWN[1423]", b"type=LANDLOCK_ACCESS"];
const DOMAIN_TYPES: [&[u8]; 2] = [b"type=UNKNOWN[1424]", b"type=LANDLOCK_DOMAIN"];

/// Read one audit line. Only the raw part (before `\x1d`) is read, and only
/// when the type is a Landlock one; never panics. Every field must be
/// `key=value` (a key of `[a-z0-9_]`; a value bare, or quoted and closed,
/// of visible ASCII with no `"` inside), no key may repeat, and there are at
/// most [`MAX_FIELDS`]: else the whole record is malformed, so nothing
/// inside a broken field can pass for a field of its own.
pub fn parse(line: &[u8]) -> Parsed {
    let raw = match line.iter().position(|&b| b == 0x1d) {
        Some(i) => &line[..i],
        None => line,
    };
    // auditd never writes a space inside a value (it hex-encodes such a value).
    let mut words = raw.split(|&b| b == b' ').filter(|w| !w.is_empty());
    let access = match words.next() {
        Some(t) if ACCESS_TYPES.contains(&t) => true,
        Some(t) if DOMAIN_TYPES.contains(&t) => false,
        _ => return Parsed::Other,
    };
    let Some(time_ms) = words.next().and_then(stamp) else {
        return Parsed::Malformed;
    };
    let Some(fields) = Fields::read(words) else {
        return Parsed::Malformed;
    };
    let Some(domain) = fields.get(b"domain").and_then(hex_u64) else {
        return Parsed::Malformed;
    };
    let body = if access {
        let Some(list) = fields.get(b"blockers") else {
            return Parsed::Malformed;
        };
        let (mut count, mut fs) = (0, false);
        for b in list.split(|&c| c == b',') {
            count += 1;
            if count > MAX_BLOCKERS || !blocker_ok(b) {
                return Parsed::Malformed;
            }
            fs |= b.starts_with(b"fs.");
        }
        // A ptrace or scope refusal is only counted: nothing more is read.
        if !fs {
            return Parsed::Record(Record { time_ms, domain, body: Body::NonFs });
        }
        let path = match fields.get(b"path").map(value) {
            None => None,
            Some(Some(p)) => p,
            Some(None) => return Parsed::Malformed,
        };
        let blockers = list
            .split(|&c| c == b',')
            .filter(|b| b.starts_with(b"fs."))
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect();
        Body::Access { blockers, path }
    } else {
        match fields.get(b"status") {
            Some(s) if s == b"allocated" => {
                let Some(pid) = fields.get(b"pid").and_then(dec_u64).and_then(|p| u32::try_from(p).ok()) else {
                    return Parsed::Malformed;
                };
                let (exe, comm) = match (fields.get(b"exe").map(value), fields.get(b"comm").map(value)) {
                    (Some(None), _) | (_, Some(None)) => return Parsed::Malformed,
                    (exe, comm) => (exe.flatten().flatten(), comm.flatten().flatten()),
                };
                Body::Allocated { pid, exe, comm }
            }
            Some(s) if s == b"deallocated" => {
                let Some(denials) = fields.get(b"denials").and_then(dec_u64) else {
                    return Parsed::Malformed;
                };
                Body::Deallocated { denials }
            }
            Some(_) => return Parsed::Other,
            None => return Parsed::Malformed,
        }
    };
    Parsed::Record(Record { time_ms, domain, body })
}

/// A record's fields after its stamp, in place (no allocation).
struct Fields<'a> {
    kv: [(&'a [u8], &'a [u8]); MAX_FIELDS],
    n: usize,
}

impl<'a> Fields<'a> {
    /// `None`: a field isn't `key=value`, a key repeats (`type` and `msg`
    /// included), or there are more than [`MAX_FIELDS`].
    fn read(words: impl Iterator<Item = &'a [u8]>) -> Option<Fields<'a>> {
        let none: &[u8] = &[];
        let mut f = Fields { kv: [(none, none); MAX_FIELDS], n: 0 };
        for w in words {
            let i = w.iter().position(|&b| b == b'=')?;
            let (k, v) = (&w[..i], &w[i + 1..]);
            if f.n == MAX_FIELDS || !key_ok(k) || !value_ok(v) || k == b"type" || k == b"msg" || f.get(k).is_some() {
                return None;
            }
            f.kv[f.n] = (k, v);
            f.n += 1;
        }
        Some(f)
    }

    fn get(&self, k: &[u8]) -> Option<&'a [u8]> {
        self.kv[..self.n].iter().find(|(key, _)| *key == k).map(|(_, v)| *v)
    }
}

fn key_ok(k: &[u8]) -> bool {
    !k.is_empty() && k.iter().all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Bare, or `"…"` closed, of visible ASCII with no `"` inside.
fn value_ok(v: &[u8]) -> bool {
    let visible = |s: &[u8]| s.iter().all(|&b| (0x21..=0x7e).contains(&b) && b != b'"');
    match v.strip_prefix(b"\"") {
        Some(rest) => rest.strip_suffix(b"\"").is_some_and(visible),
        None => visible(v),
    }
}

/// One blocker: nonempty, of `[a-z0-9_.]`.
fn blocker_ok(b: &[u8]) -> bool {
    !b.is_empty() && b.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'.')
}

/// `msg=audit(<secs>.<ms>:<serial>):` as unix ms.
fn stamp(w: &[u8]) -> Option<u64> {
    let inner = w.strip_prefix(b"msg=audit(")?.strip_suffix(b"):")?;
    let colon = inner.iter().position(|&b| b == b':')?;
    dec_u64(&inner[colon + 1..])?;
    let time = &inner[..colon];
    let dot = time.iter().position(|&b| b == b'.')?;
    let (secs, ms) = (&time[..dot], &time[dot + 1..]);
    if ms.len() != 3 {
        return None;
    }
    dec_u64(secs)?.checked_mul(1000)?.checked_add(dec_u64(ms)?)
}

fn dec_u64(s: &[u8]) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    s.iter().try_fold(0u64, |n, &b| if b.is_ascii_digit() { n.checked_mul(10)?.checked_add(u64::from(b - b'0')) } else { None })
}

fn hex_digit(b: u8) -> Option<u8> {
    (b as char).to_digit(16).and_then(|d| u8::try_from(d).ok())
}

fn hex_u64(s: &[u8]) -> Option<u64> {
    if s.is_empty() || s.len() > 16 {
        return None;
    }
    s.iter().try_fold(0u64, |n, &b| Some((n << 4) | u64::from(hex_digit(b)?)))
}

/// A path, exe or comm value: `"quoted"`, bare hex (auditd's form for a value
/// with a space, quote, control or non-ASCII byte), or `(null)` (`Some(None)`).
/// `None`: malformed.
fn value(v: &[u8]) -> Option<Option<Vec<u8>>> {
    if v == b"(null)" {
        return Some(None);
    }
    if let Some(rest) = v.strip_prefix(b"\"") {
        return rest.strip_suffix(b"\"").map(|inner| Some(inner.to_vec()));
    }
    if v.is_empty() || !v.len().is_multiple_of(2) {
        return None;
    }
    v.chunks(2).map(|p| Some((hex_digit(p[0])? << 4) | hex_digit(p[1])?)).collect::<Option<Vec<u8>>>().map(Some)
}

/// What the kernel refused, from a record's `fs.*` blockers: `rename/link`
/// when `fs.refer` is one (a rename or link across dirs), else the distinct
/// ops joined by `+`, in order.
pub fn op_of(blockers: &[String]) -> String {
    if blockers.iter().any(|b| b == "fs.refer") {
        return "rename/link".into();
    }
    let mut ops: Vec<&str> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for b in blockers {
        let op = match b.as_str() {
            "fs.write_file" => "write",
            "fs.truncate" => "truncate",
            "fs.make_reg" => "create",
            "fs.make_dir" => "mkdir",
            "fs.make_sym" => "symlink",
            "fs.make_char" | "fs.make_block" | "fs.make_fifo" | "fs.make_sock" => "mknod",
            "fs.remove_file" | "fs.remove_dir" => "remove",
            "fs.change_topology" => "mount",
            other => other.strip_prefix("fs.").unwrap_or(other),
        };
        if seen.insert(op) {
            ops.push(op);
        }
    }
    ops.join("+")
}

/// A value as a log field: as is when every byte is visible ASCII other
/// than `"` and `\`, else quoted, with `"` and `\` escaped and every other
/// byte (the space too) as `\xNN`: one field, no controls, and the bytes can
/// be read back.
pub fn render_path(p: &[u8]) -> String {
    let plain = |b: u8| (0x21..=0x7e).contains(&b) && b != b'"' && b != b'\\';
    if !p.is_empty() && p.iter().all(|&b| plain(b)) {
        return p.iter().map(|&b| char::from(b)).collect();
    }
    let mut s = String::with_capacity(p.len() + 2);
    s.push('"');
    for &b in p {
        match b {
            b'"' => s.push_str("\\\""),
            b'\\' => s.push_str("\\\\"),
            0x21..=0x7e => s.push(char::from(b)),
            _ => {
                let _ = write!(s, "\\x{b:02X}");
            }
        }
    }
    s.push('"');
    s
}

/// A text field of a line ([`render_path`] of its bytes).
fn field(s: &str) -> String {
    render_path(s.as_bytes())
}

/// `class` when it is only `[a-z0-9_:.-]`; else its prefix (up to the first
/// `:`) and `:unprintable`. A class is never escaped: it is a fixed word.
pub(crate) fn safe_class(class: String) -> String {
    let ok = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b':' | b'.' | b'-'));
    if ok(&class) {
        return class;
    }
    match class.split_once(':') {
        Some((prefix, _)) if ok(prefix) => format!("{prefix}:unprintable"),
        _ => "unprintable".into(),
    }
}

// ── Spawns ───────────────────────────────────────────────────────────────

/// What a denial line needs about its spawn.
#[derive(Clone, Debug)]
pub struct SpawnInfo {
    pub session: String,
    pub tool: &'static str,
    /// `audit_log = off`: no audit lines for this spawn (its domains are
    /// still known as this daemon's, so they aren't unattributed).
    pub quiet: bool,
    pub home: PathBuf,
    pub base: PathBuf,
    /// The policy's `git_common` dirs (their root stays read-only).
    pub git_common: Vec<PathBuf>,
    /// The policy's `cargo_home` dirs.
    pub cargo_home: Vec<PathBuf>,
}

/// The class of a refusal: `by_design:<x>` for a refusal the policy means
/// (still logged; the stop rule skips them), `uncertain:<x>` for one that
/// may or may not be (logged, not exempt), else as the output matcher
/// classes the path, reduced to `[a-z0-9_:.-]`. For a create, mkdir,
/// symlink, mknod, remove or rename/link the kernel names the PARENT dir;
/// for a write or truncate the file. So `by_design` is given only when the
/// record names the by-design object itself.
pub fn class_of(info: &SpawnInfo, op: &str, path: &Path) -> String {
    let names_parent =
        op.split('+').all(|o| matches!(o, "create" | "mkdir" | "symlink" | "mknod" | "remove" | "rename/link"));
    let names_file = op.split('+').all(|o| matches!(o, "write" | "truncate"));
    if info.git_common.iter().any(|g| (names_parent && path == g) || (names_file && path.parent() == Some(g.as_path()))) {
        return "by_design:git_common".into();
    }
    // A create in cargo's home's root names only that dir: it may be one of
    // cargo's own files there (refused by design), or not.
    if op == "create" && info.cargo_home.iter().any(|c| path == c) {
        return "uncertain:cargo_home".into();
    }
    // The session registry's lock, at its one path (the registry reads it
    // then).
    if names_file && path == info.base.join("run/.session-registry.lock") {
        return "by_design:session_registry".into();
    }
    // less's history (through git's pager): a write names the file; a create
    // names only ~/.local/state, where any program may be creating a file.
    let state = info.home.join(".local/state");
    if names_file && path == state.join("lesshst") {
        return "by_design:less_history".into();
    }
    if op == "create" && path == state {
        return "uncertain:local_state".into();
    }
    safe_class(classify(path, "", &info.home, &info.base))
}

struct Entry {
    seq: u64,
    info: Arc<SpawnInfo>,
    pid: Option<u32>,
    /// Unix ms (the join compares it with the kernel's stamps).
    spawn_ms: u64,
    /// Monotonic ms.
    exited_ms: Option<u64>,
}

impl Entry {
    /// Still joinable at `mono`: live, or ended less than
    /// [`KEEP_AFTER_EXIT_MS`] ago.
    fn kept(&self, mono: u64) -> bool {
        self.exited_ms.is_none_or(|t| mono.saturating_sub(t) < KEEP_AFTER_EXIT_MS)
    }
}

#[derive(Default)]
struct Entries {
    map: HashMap<String, Entry>,
    seq: u64,
}

/// The spawns this daemon gave a tag, for the join (in memory: a previous
/// daemon's spawns aren't in it).
pub struct Registry {
    unix: Clock,
    mono: Clock,
    entries: Mutex<Entries>,
}

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct EntryView {
    pub session: String,
    pub tool: &'static str,
    pub quiet: bool,
    pub pid: Option<u32>,
    pub exited: bool,
}

impl Registry {
    /// A registry on one clock (both the join's and the keep window's).
    pub fn new(clock: Clock) -> Arc<Registry> {
        Registry::with_clocks(Arc::clone(&clock), clock)
    }

    /// A registry whose spawn times come from `unix` (the join compares them
    /// with the kernel's stamps) and whose keep window runs on `mono`.
    pub fn with_clocks(unix: Clock, mono: Clock) -> Arc<Registry> {
        Arc::new(Registry { unix, mono, entries: Mutex::new(Entries::default()) })
    }

    /// Register a spawn under `tag`. Drops spawns that exited at least
    /// [`KEEP_AFTER_EXIT_MS`] ago; at [`REGISTRY_CAP`] the earliest-registered
    /// exited spawn makes room, else the earliest-registered one.
    pub fn register(self: &Arc<Self>, tag: &Tag, info: SpawnInfo) -> Registration {
        let (now, mono) = ((self.unix)(), (self.mono)());
        let mut e = lock(&self.entries);
        e.map.retain(|_, x| x.kept(mono));
        if !e.map.contains_key(tag.as_str()) && e.map.len() >= REGISTRY_CAP {
            let victim = e
                .map
                .iter()
                .filter(|(_, x)| x.exited_ms.is_some())
                .min_by_key(|(_, x)| x.seq)
                .or_else(|| e.map.iter().min_by_key(|(_, x)| x.seq))
                .map(|(k, _)| k.clone());
            if let Some(v) = victim {
                e.map.remove(&v);
            }
        }
        e.seq += 1;
        let seq = e.seq;
        e.map.insert(tag.as_str().to_string(), Entry { seq, info: Arc::new(info), pid: None, spawn_ms: now, exited_ms: None });
        Registration(Arc::new(Handle { registry: Arc::downgrade(self), tag: tag.as_str().to_string(), seq }))
    }

    pub fn len(&self) -> usize {
        lock(&self.entries).map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The spawn behind `tag`, if still joinable (live, or ended less than
    /// [`KEEP_AFTER_EXIT_MS`] ago): its info, its child's pid if known, and
    /// when it was registered (unix ms).
    fn lookup(&self, tag: &str) -> Option<(Arc<SpawnInfo>, Option<u32>, u64)> {
        let mono = (self.mono)();
        lock(&self.entries).map.get(tag).filter(|e| e.kept(mono)).map(|e| (Arc::clone(&e.info), e.pid, e.spawn_ms))
    }

    fn update(&self, tag: &str, seq: u64, f: impl FnOnce(&mut Entry)) {
        if let Some(e) = lock(&self.entries).map.get_mut(tag).filter(|e| e.seq == seq) {
            f(e);
        }
    }

    #[cfg(test)]
    pub(crate) fn entry(&self, tag: &str) -> Option<EntryView> {
        lock(&self.entries).map.get(tag).map(|e| EntryView {
            session: e.info.session.clone(),
            tool: e.info.tool,
            quiet: e.info.quiet,
            pid: e.pid,
            exited: e.exited_ms.is_some(),
        })
    }
}

/// A spawn's place in the registry. When the last clone is dropped the spawn
/// counts as exited (and stays joinable for [`KEEP_AFTER_EXIT_MS`]).
#[derive(Clone, Debug)]
pub struct Registration(Arc<Handle>);

#[derive(Debug)]
struct Handle {
    registry: Weak<Registry>,
    tag: String,
    seq: u64,
}

impl Registration {
    /// The spawn's child pid (a bash child restricts itself, so its domain
    /// names the child).
    pub fn set_pid(&self, pid: u32) {
        if let Some(r) = self.0.registry.upgrade() {
            r.update(&self.0.tag, self.0.seq, |e| e.pid = Some(pid));
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if let Some(r) = self.registry.upgrade() {
            let now = (r.mono)();
            r.update(&self.tag, self.seq, |e| e.exited_ms = Some(now));
        }
    }
}

// ── Following the file ───────────────────────────────────────────────────

/// Open failed on something that isn't a regular file.
#[derive(Debug)]
struct NotAFile;

impl std::fmt::Display for NotAFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not a regular file")
    }
}

impl std::error::Error for NotAFile {}

/// Open a regular file for reading: never a symlink (`O_NOFOLLOW`), never
/// blocking on a FIFO; close-on-exec, so a reload's exec closes it.
fn open_file(path: &Path) -> io::Result<(File, std::fs::Metadata)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| if e.raw_os_error() == Some(libc::ELOOP) { io::Error::new(io::ErrorKind::InvalidInput, NotAFile) } else { e })?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, NotAFile));
    }
    Ok((file, meta))
}

/// How the follower opens the log (a seam for tests).
pub(crate) type Opener = Arc<dyn Fn(&Path) -> io::Result<(File, std::fs::Metadata)> + Send + Sync>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Skip {
    No,
    /// The line being written when the file was opened.
    Uncounted,
    /// A line over [`MAX_LINE`].
    Counted,
}

/// What one [`Follower::step`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Read this many bytes (0: it moved to a new file under the name).
    Read(usize),
    /// At the end of the file it follows.
    End,
}

/// Follows one log file by name: new whole lines only, from where it was
/// opened.
pub struct Follower {
    path: PathBuf,
    open: Opener,
    file: File,
    /// The open file's (dev, ino).
    id: (u64, u64),
    offset: u64,
    partial: Vec<u8>,
    skip: Skip,
    long: u64,
    buf: Vec<u8>,
    /// Why the name doesn't lead to a regular file, as last seen.
    problem: Option<String>,
}

impl Follower {
    /// Open `path` at its end. A line being written then is skipped (not
    /// counted as over-long).
    pub fn open_at_end(path: &Path) -> io::Result<Follower> {
        Follower::open_at_end_with(path, Arc::new(open_file))
    }

    /// [`Follower::open_at_end`], opening the log with `open`.
    pub(crate) fn open_at_end_with(path: &Path, open: Opener) -> io::Result<Follower> {
        let (file, meta) = open(path)?;
        let len = meta.len();
        let mut last = [0u8; 1];
        let mid_line = len > 0 && file.read_at(&mut last, len - 1)? == 1 && last[0] != b'\n';
        Ok(Follower {
            path: path.to_path_buf(),
            open,
            file,
            id: (meta.dev(), meta.ino()),
            offset: len,
            partial: Vec::new(),
            skip: if mid_line { Skip::Uncounted } else { Skip::No },
            long: 0,
            buf: vec![0; CHUNK],
            problem: None,
        })
    }

    /// Give each new whole line to `line`, to the end ([`Follower::step`]
    /// until [`Step::End`]).
    pub fn poll(&mut self, line: &mut dyn FnMut(&[u8])) -> io::Result<()> {
        while let Step::Read(_) = self.step(line)? {}
        Ok(())
    }

    /// One read (at most [`CHUNK`] bytes), its whole lines to `line`. A line
    /// over [`MAX_LINE`] is skipped and counted; a partial last line waits
    /// for its newline. A file truncated below where this was is read again
    /// from its start. At the end of the file it holds it looks at the name
    /// (never following a symlink): another regular file there (a rotation)
    /// is opened and read from its start, unless the open finds the file
    /// held (the name came back to it: its place is kept); while the name is
    /// missing or isn't a regular file the old file is kept and
    /// [`Follower::name_problem`] says why.
    pub fn step(&mut self, line: &mut dyn FnMut(&[u8])) -> io::Result<Step> {
        if self.file.metadata()?.len() < self.offset {
            self.restart();
        }
        let n = loop {
            match self.file.read_at(&mut self.buf, self.offset) {
                Ok(n) => break n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        };
        if n > 0 {
            self.offset += n as u64;
            let chunk = std::mem::take(&mut self.buf);
            self.split(&chunk[..n], line);
            self.buf = chunk;
            return Ok(Step::Read(n));
        }
        Ok(if self.check_name() { Step::Read(0) } else { Step::End })
    }

    /// Why the log's name doesn't lead to a regular file (as a reason
    /// field), as the last look at the end of the file found.
    pub fn name_problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }

    /// At the end of the file held: whether the name now leads to another
    /// regular file, opened and to be read from its start.
    fn check_name(&mut self) -> bool {
        let found = match std::fs::symlink_metadata(&self.path) {
            Err(e) => Err(reason(&e)),
            Ok(m) if !m.file_type().is_file() => Err("not-a-file".to_string()),
            Ok(m) if (m.dev(), m.ino()) == self.id => Ok(None),
            Ok(_) => match (self.open)(&self.path) {
                Err(e) => Err(reason(&e)),
                // The name came back to the file held: keep its place.
                Ok((_, meta)) if (meta.dev(), meta.ino()) == self.id => Ok(None),
                Ok(opened) => Ok(Some(opened)),
            },
        };
        match found {
            Err(r) => {
                self.problem = Some(r);
                false
            }
            Ok(None) => {
                self.problem = None;
                false
            }
            Ok(Some((file, meta))) => {
                self.problem = None;
                self.file = file;
                self.id = (meta.dev(), meta.ino());
                self.restart();
                true
            }
        }
    }

    /// Over-long lines skipped since the last call.
    pub fn take_long(&mut self) -> u64 {
        std::mem::take(&mut self.long)
    }

    /// Where the next read starts.
    #[cfg(test)]
    pub(crate) fn offset(&self) -> u64 {
        self.offset
    }

    fn restart(&mut self) {
        self.offset = 0;
        self.partial.clear();
        self.skip = Skip::No;
    }

    fn split(&mut self, mut rest: &[u8], line: &mut dyn FnMut(&[u8])) {
        while !rest.is_empty() {
            let Some(i) = rest.iter().position(|&b| b == b'\n') else {
                if self.skip == Skip::No {
                    if self.partial.len() + rest.len() > MAX_LINE {
                        self.partial.clear();
                        self.skip = Skip::Counted;
                    } else {
                        self.partial.extend_from_slice(rest);
                    }
                }
                return;
            };
            let head = &rest[..i];
            rest = &rest[i + 1..];
            match self.skip {
                Skip::Counted => self.long += 1,
                Skip::Uncounted => {}
                Skip::No if self.partial.len() + head.len() > MAX_LINE => {
                    self.partial.clear();
                    self.long += 1;
                }
                Skip::No if self.partial.is_empty() => line(head),
                Skip::No => {
                    self.partial.extend_from_slice(head);
                    line(&self.partial);
                    self.partial.clear();
                }
            }
            self.skip = Skip::No;
        }
    }
}

// ── The join and the lines ───────────────────────────────────────────────

enum Domain {
    Ours { tag: String, info: Arc<SpawnInfo>, ptrace: u64 },
    /// Another program's (another exe): skipped.
    Foreign,
    /// Couldn't be joined: its records are counted.
    Unattributed,
}

/// Records of a domain not known yet.
struct Pending {
    since: u64,
    records: Vec<Record>,
    /// Its allocation record, waiting for its spawn's pid.
    alloc: Option<Record>,
}

impl Pending {
    fn len(&self) -> usize {
        self.records.len() + usize::from(self.alloc.is_some())
    }
}

/// One (tag, op, path) fold window.
struct Window {
    start: u64,
    seq: u64,
    extra: u64,
    /// The denial line up to ` n=`.
    line: String,
}

type WindowKey = (String, String, Option<Vec<u8>>);

enum Join {
    Ours(String, Arc<SpawnInfo>),
    /// The spawn's child pid isn't known yet.
    Wait,
    No,
}

/// One line budget: a line costs a minute's worth of units, and `per_min`
/// units come back each ms, up to `per_min` lines' worth.
#[derive(Clone, Copy)]
struct Bucket {
    units: u64,
    at: u64,
}

const MINUTE_MS: u64 = 60_000;
/// Sessions with a line budget of their own, at most (the oldest goes first).
const BUDGET_CAP: usize = 1024;

impl Bucket {
    fn full(per_min: u64, now: u64) -> Bucket {
        Bucket { units: per_min.saturating_mul(MINUTE_MS), at: now }
    }

    /// Fill to `now`; whether a line's worth is there.
    fn fill(&mut self, per_min: u64, now: u64) -> bool {
        let back = now.saturating_sub(self.at).saturating_mul(per_min);
        self.units = self.units.saturating_add(back).min(per_min.saturating_mul(MINUTE_MS));
        self.at = self.at.max(now);
        self.units >= MINUTE_MS
    }
}

/// The reader's state, owned by its thread: which domains are whose, the
/// records waiting for their domain, the fold windows, the line budgets and
/// the counts. Every time it is given is the monotonic clock's.
pub struct Tail {
    exe: Option<Vec<u8>>,
    pid: u32,
    domains: HashMap<u64, Domain>,
    order: VecDeque<u64>,
    pending: HashMap<u64, Pending>,
    /// (domain, since) in the order they began waiting; an entry whose
    /// domain no longer waits since then is stale.
    pending_order: VecDeque<(u64, u64)>,
    pending_records: usize,
    /// Waiting domains that hold their allocation record.
    waiting: usize,
    windows: HashMap<Arc<WindowKey>, Window>,
    /// (seq, key) in the order the windows opened; stale when the window
    /// with that key has another seq or is gone.
    window_order: VecDeque<(u64, Arc<WindowKey>)>,
    seq: u64,
    /// New denial lines a minute: (per session, host-wide).
    rates: (u64, u64),
    host: Bucket,
    sessions: HashMap<String, Bucket>,
    session_order: VecDeque<String>,
    /// (domains, records) not attributed since the last report.
    unattributed: (u64, u64),
    /// (malformed, over-long) since the last report.
    dropped: (u64, u64),
    /// Refusals over the line budget since the last report.
    suppressed: u64,
    reported: (Option<u64>, Option<u64>, Option<u64>),
    readable: bool,
    /// When the log's name was first seen not leading to a regular file.
    name_since: Option<u64>,
}

fn strip_deleted(exe: &[u8]) -> &[u8] {
    exe.strip_suffix(b" (deleted)").unwrap_or(exe)
}

impl Tail {
    /// A tail for the daemon whose exe is `exe` and pid `pid` (`exe` `None`:
    /// unknown, so no domain is attributed).
    pub fn new(exe: Option<Vec<u8>>, pid: u32) -> Tail {
        Tail {
            exe: exe.map(|e| strip_deleted(&e).to_vec()),
            pid,
            domains: HashMap::new(),
            order: VecDeque::new(),
            pending: HashMap::new(),
            pending_order: VecDeque::new(),
            pending_records: 0,
            waiting: 0,
            windows: HashMap::new(),
            window_order: VecDeque::new(),
            seq: 0,
            rates: (SESSION_LINES_PER_MIN, HOST_LINES_PER_MIN),
            host: Bucket::full(HOST_LINES_PER_MIN, 0),
            sessions: HashMap::new(),
            session_order: VecDeque::new(),
            unattributed: (0, 0),
            dropped: (0, 0),
            suppressed: 0,
            reported: (None, None, None),
            readable: true,
            name_since: None,
        }
    }

    /// The line budgets, per session and host-wide (lines a minute).
    pub(crate) fn set_budget(&mut self, session_per_min: u64, host_per_min: u64) {
        self.rates = (session_per_min, host_per_min);
        self.host = Bucket::full(host_per_min, 0);
        self.sessions.clear();
        self.session_order.clear();
    }

    /// Read `f`'s new lines and give `out` the lines due at `now` (the
    /// monotonic clock, ms). One poll reads at most [`POLL_BYTES`], stops
    /// after the read that passes [`POLL_LINES`], and asks `stop` after each
    /// read; the timeouts and caps are kept after each read too, and the
    /// counts are reported once, after the reads.
    /// Then the log's state: `unavailable` when a read fails, or when its
    /// name has led to no regular file for [`NAME_GRACE_MS`]; `available`
    /// when that ends. Each said once, at its change.
    pub fn poll(
        &mut self,
        f: &mut Follower,
        reg: &Registry,
        now: u64,
        stop: &dyn Fn() -> bool,
        out: &mut dyn FnMut(Level, String),
    ) {
        let (mut bytes, mut lines, mut steps) = (0usize, 0usize, 0usize);
        let mut failed = None;
        loop {
            let mut n = 0usize;
            let step = f.step(&mut |line| {
                n += 1;
                self.line(line, reg, now, out);
            });
            lines += n;
            steps += 1;
            self.dropped.1 += f.take_long();
            self.keep(reg, now, out);
            match step {
                Ok(Step::Read(read)) => {
                    bytes += read;
                    // Steps of 0 bytes (a new file under the name) count too.
                    if bytes >= POLL_BYTES || lines >= POLL_LINES || steps >= 2 * (POLL_BYTES / CHUNK) || stop() {
                        break;
                    }
                }
                Ok(Step::End) => break,
                Err(e) => {
                    failed = Some(reason(&e));
                    break;
                }
            }
        }
        // The counts once a poll (a poll is bounded), after the lines of
        // the records they count.
        self.report(now, out);
        let problem = f.name_problem().map(str::to_owned);
        if problem.is_none() {
            self.name_since = None;
        } else if self.name_since.is_none() {
            self.name_since = Some(now);
        }
        let long_enough = self.name_since.is_some_and(|t| now.saturating_sub(t) >= NAME_GRACE_MS);
        match failed.or(problem.filter(|_| long_enough)) {
            Some(r) if self.readable => {
                self.readable = false;
                out(Level::Warn, format!("sandbox.audit unavailable reason={r}"));
            }
            None if !self.readable => {
                self.readable = true;
                out(Level::Warn, "sandbox.audit available".into());
            }
            _ => {}
        }
    }

    /// The timeouts and caps, after each read.
    fn keep(&mut self, reg: &Registry, now: u64, out: &mut dyn FnMut(Level, String)) {
        if self.waiting > 0 {
            self.retry_waiting(reg, now, out);
        }
        self.expire_pending(now);
        if self.pending_order.len() > 2 * PENDING_CAP {
            let pending = &self.pending;
            self.pending_order.retain(|(d, since)| pending.get(d).is_some_and(|p| p.since == *since));
        }
        self.expire_windows(now, out);
    }

    fn line(&mut self, line: &[u8], reg: &Registry, now: u64, out: &mut dyn FnMut(Level, String)) {
        match parse(line) {
            Parsed::Other => {}
            Parsed::Malformed => self.dropped.0 += 1,
            Parsed::Record(rec) => self.record(rec, reg, now, out),
        }
    }

    fn record(&mut self, rec: Record, reg: &Registry, now: u64, out: &mut dyn FnMut(Level, String)) {
        if self.domains.contains_key(&rec.domain) {
            return self.known(rec, now, out);
        }
        let waiting = self.pending.get(&rec.domain).is_some_and(|p| p.alloc.is_some());
        match rec.body {
            Body::Allocated { .. } if !waiting => self.allocated(rec, reg, now, out),
            // The end of a domain never seen: it began before the reader did.
            Body::Deallocated { .. } if !self.pending.contains_key(&rec.domain) => {
                self.unattributed.0 += 1;
                self.unattributed.1 += 1;
            }
            _ => self.queue(rec, now),
        }
    }

    fn queue(&mut self, rec: Record, now: u64) {
        match self.admit(rec.domain, now) {
            Some(p) => p.records.push(rec),
            None => {
                self.unattributed.1 += 1;
                return;
            }
        }
        self.pending_records += 1;
    }

    /// Room for one more held record of `d`, on both routes (a record whose
    /// domain record hasn't come, a domain record waiting for its spawn's
    /// pid): at [`PENDING_CAP`] held records the oldest waiting domain is
    /// given up. `None`: that was `d` itself (the record is then counted).
    fn admit(&mut self, d: u64, now: u64) -> Option<&mut Pending> {
        while self.pending_records >= PENDING_CAP {
            let Some((old, since)) = self.pending_order.pop_front() else {
                break;
            };
            if self.pending.get(&old).is_some_and(|p| p.since == since) {
                self.give_up(old);
            }
        }
        if self.pending_records >= PENDING_CAP || self.domains.contains_key(&d) {
            return None;
        }
        match self.pending.entry(d) {
            std::collections::hash_map::Entry::Occupied(o) => Some(o.into_mut()),
            std::collections::hash_map::Entry::Vacant(v) => {
                self.pending_order.push_back((d, now));
                Some(v.insert(Pending { since: now, records: Vec::new(), alloc: None }))
            }
        }
    }

    fn take_pending(&mut self, d: u64) -> Option<Pending> {
        let p = self.pending.remove(&d)?;
        self.pending_records = self.pending_records.saturating_sub(p.len());
        if p.alloc.is_some() {
            self.waiting = self.waiting.saturating_sub(1);
        }
        Some(p)
    }

    /// Count a pending domain and its records as unattributed.
    fn give_up(&mut self, d: u64) {
        if let Some(p) = self.take_pending(d) {
            self.unattributed.0 += 1;
            self.unattributed.1 += p.len() as u64;
            self.insert_domain(d, Domain::Unattributed);
        }
    }

    /// Give up the domains that have waited [`PENDING_MS`] (oldest first:
    /// they wait in that order).
    fn expire_pending(&mut self, now: u64) {
        while let Some(&(d, since)) = self.pending_order.front() {
            let live = self.pending.get(&d).is_some_and(|p| p.since == since);
            if live && now.saturating_sub(since) < PENDING_MS {
                break;
            }
            self.pending_order.pop_front();
            if live {
                self.give_up(d);
            }
        }
    }

    fn insert_domain(&mut self, d: u64, dom: Domain) {
        if self.domains.insert(d, dom).is_none() {
            self.order.push_back(d);
            while self.domains.len() > DOMAIN_CAP {
                match self.order.pop_front() {
                    Some(old) => {
                        self.domains.remove(&old);
                    }
                    None => break,
                }
            }
            if self.order.len() > 2 * DOMAIN_CAP {
                let domains = &self.domains;
                self.order.retain(|x| domains.contains_key(x));
            }
        }
    }

    fn join(&self, rec: &Record, reg: &Registry) -> Join {
        let Body::Allocated { pid, comm, .. } = &rec.body else {
            return Join::No;
        };
        let Some(tag) = comm.as_deref().and_then(|c| std::str::from_utf8(c).ok()).filter(|t| valid_tag(t)) else {
            return Join::No;
        };
        let Some((info, spawn_pid, spawn_ms)) = reg.lookup(tag) else {
            return Join::No;
        };
        // A domain older than the spawn: another image's spawn with this tag.
        if spawn_ms > rec.time_ms.saturating_add(CLOCK_SLACK_MS) {
            return Join::No;
        }
        if *pid == self.pid || spawn_pid == Some(*pid) {
            return Join::Ours(tag.to_string(), info);
        }
        if spawn_pid.is_none() {
            Join::Wait
        } else {
            Join::No
        }
    }

    fn allocated(&mut self, rec: Record, reg: &Registry, now: u64, out: &mut dyn FnMut(Level, String)) {
        let d = rec.domain;
        let exe = match &rec.body {
            Body::Allocated { exe, .. } => exe.as_deref(),
            _ => return,
        };
        // With its own exe unknown the daemon can't tell its domains from
        // another program's: it attributes none.
        let mine = match (&self.exe, exe) {
            (None, _) => None,
            (Some(me), Some(e)) => Some(me.as_slice() == strip_deleted(e)),
            (Some(_), None) => Some(false),
        };
        let join = match mine {
            None => Join::No,
            Some(true) => self.join(&rec, reg),
            Some(false) => {
                self.take_pending(d);
                self.insert_domain(d, Domain::Foreign);
                return;
            }
        };
        match join {
            Join::Ours(tag, info) => self.adopt(d, tag, info, now, out),
            Join::Wait => {
                match self.admit(d, now) {
                    Some(p) => p.alloc = Some(rec),
                    None => {
                        self.unattributed.1 += 1;
                        return;
                    }
                }
                self.pending_records += 1;
                self.waiting += 1;
            }
            Join::No => {
                let n = 1 + self.take_pending(d).map_or(0, |p| p.len());
                self.unattributed.0 += 1;
                self.unattributed.1 += n as u64;
                self.insert_domain(d, Domain::Unattributed);
            }
        }
    }

    /// The domain is this spawn's: its waiting records are read in order.
    fn adopt(&mut self, d: u64, tag: String, info: Arc<SpawnInfo>, now: u64, out: &mut dyn FnMut(Level, String)) {
        self.insert_domain(d, Domain::Ours { tag, info, ptrace: 0 });
        if let Some(p) = self.take_pending(d) {
            for r in p.records {
                self.known(r, now, out);
            }
        }
    }

    fn retry_waiting(&mut self, reg: &Registry, now: u64, out: &mut dyn FnMut(Level, String)) {
        let pending = &self.pending;
        let waiting: Vec<u64> = self
            .pending_order
            .iter()
            .filter(|(d, since)| pending.get(d).is_some_and(|p| p.since == *since && p.alloc.is_some()))
            .map(|(d, _)| *d)
            .collect();
        for d in waiting {
            let Some(alloc) = self.pending.get(&d).and_then(|p| p.alloc.clone()) else {
                continue;
            };
            match self.join(&alloc, reg) {
                Join::Ours(tag, info) => self.adopt(d, tag, info, now, out),
                Join::Wait => {}
                Join::No => self.give_up(d),
            }
        }
    }

    /// A record of a domain already known.
    fn known(&mut self, rec: Record, now: u64, out: &mut dyn FnMut(Level, String)) {
        let d = rec.domain;
        let end = matches!(rec.body, Body::Deallocated { .. });
        let ours = match self.domains.get_mut(&d) {
            None | Some(Domain::Foreign) => None,
            Some(Domain::Unattributed) => {
                self.unattributed.1 += 1;
                None
            }
            // A ptrace or scope refusal: only counted.
            Some(Domain::Ours { ptrace, .. }) if matches!(rec.body, Body::NonFs) => {
                *ptrace += 1;
                None
            }
            Some(Domain::Ours { info, .. }) if info.quiet && !end => None,
            Some(Domain::Ours { tag, info, ptrace }) => Some((tag.clone(), Arc::clone(info), *ptrace)),
        };
        if end {
            self.domains.remove(&d);
        }
        let Some((tag, info, ptrace)) = ours else {
            return;
        };
        match rec.body {
            Body::Access { blockers, path } if !info.quiet => {
                self.denial((tag, op_of(&blockers), path), &info, now, out);
            }
            Body::Deallocated { denials } => {
                self.flush_where(|k, _| k.0 == tag, out);
                if denials > 0 && !info.quiet {
                    out(
                        Level::Warn,
                        format!(
                            "sandbox.domain_end session={} tag={} denials={denials} ptrace={ptrace}",
                            field(&info.session),
                            field(&tag)
                        ),
                    );
                }
            }
            _ => {}
        }
    }

    /// Whether a new denial line of `session` fits the line budgets (and
    /// takes its share of both).
    fn budget(&mut self, session: &str, now: u64) -> bool {
        let (per_session, per_host) = self.rates;
        let host = self.host.fill(per_host, now);
        if !self.sessions.contains_key(session) {
            while self.sessions.len() >= BUDGET_CAP {
                match self.session_order.pop_front() {
                    Some(old) => {
                        self.sessions.remove(&old);
                    }
                    None => break,
                }
            }
            self.sessions.insert(session.to_string(), Bucket::full(per_session, now));
            self.session_order.push_back(session.to_string());
        }
        let Some(b) = self.sessions.get_mut(session) else {
            return false;
        };
        if !(b.fill(per_session, now) && host) {
            return false;
        }
        b.units -= MINUTE_MS;
        self.host.units -= MINUTE_MS;
        true
    }

    /// One refusal: a line for the first of a window (within the line
    /// budgets, else only counted), a count for repeats.
    fn denial(&mut self, key: WindowKey, info: &SpawnInfo, now: u64, out: &mut dyn FnMut(Level, String)) {
        if let Some(w) = self.windows.get_mut(&key) {
            if now.saturating_sub(w.start) < DEDUP_MS {
                w.extra += 1;
                return;
            }
        }
        // A window past its minute that the tick hasn't closed yet.
        self.close(&key, out);
        if !self.budget(&info.session, now) {
            self.suppressed += 1;
            return;
        }
        if self.windows.len() >= DEDUP_CAP {
            if let Some(oldest) = self.oldest_window() {
                self.close(&oldest, out);
            }
        }
        let (tag, op, path) = &key;
        let (class, shown) = match path {
            Some(p) => (class_of(info, op, Path::new(OsStr::from_bytes(p))), render_path(p)),
            None => ("unknown".to_string(), "-".to_string()),
        };
        let line = format!(
            "sandbox.denial session={} tool={} tag={} op={} class={class} path={shown}",
            field(&info.session),
            info.tool,
            field(tag),
            field(op)
        );
        out(Level::Warn, format!("{line} n=1 src=audit"));
        self.seq += 1;
        let key = Arc::new(key);
        self.window_order.push_back((self.seq, Arc::clone(&key)));
        self.windows.insert(key, Window { start: now, seq: self.seq, extra: 0, line });
        if self.window_order.len() > 2 * DEDUP_CAP {
            let windows = &self.windows;
            self.window_order.retain(|(seq, k)| windows.get(&**k).is_some_and(|w| w.seq == *seq));
        }
    }

    /// The oldest open window, its stale entries dropped on the way.
    fn oldest_window(&mut self) -> Option<Arc<WindowKey>> {
        while let Some((seq, k)) = self.window_order.pop_front() {
            if self.windows.get(&*k).is_some_and(|w| w.seq == seq) {
                return Some(k);
            }
        }
        None
    }

    /// Close the windows past their minute, oldest first (they opened in
    /// that order).
    fn expire_windows(&mut self, now: u64, out: &mut dyn FnMut(Level, String)) {
        while let Some((seq, k)) = self.window_order.front().map(|(s, k)| (*s, Arc::clone(k))) {
            let start = self.windows.get(&*k).filter(|w| w.seq == seq).map(|w| w.start);
            if start.is_some_and(|s| now.saturating_sub(s) < DEDUP_MS) {
                break;
            }
            self.window_order.pop_front();
            if start.is_some() {
                self.close(&k, out);
            }
        }
    }

    /// Close the windows `pick` selects (a spawn's, at its domain's end),
    /// oldest first; one with repeats logs their count.
    fn flush_where(&mut self, pick: impl Fn(&WindowKey, &Window) -> bool, out: &mut dyn FnMut(Level, String)) {
        let mut done: Vec<(u64, u64, Arc<WindowKey>)> =
            self.windows.iter().filter(|(k, w)| pick(k, w)).map(|(k, w)| (w.start, w.seq, Arc::clone(k))).collect();
        done.sort_by_key(|(start, seq, _)| (*start, *seq));
        for (_, _, k) in done {
            self.close(&k, out);
        }
    }

    fn close(&mut self, key: &WindowKey, out: &mut dyn FnMut(Level, String)) {
        if let Some(w) = self.windows.remove(key) {
            if w.extra > 0 {
                out(Level::Warn, format!("{} n={} src=audit", w.line, w.extra));
            }
        }
    }

    /// The counts, each at most every [`REPORT_MS`] (on the monotonic
    /// clock), only when nonzero.
    fn report(&mut self, now: u64, out: &mut dyn FnMut(Level, String)) {
        let due = |last: Option<u64>| last.is_none_or(|t| now.saturating_sub(t) >= REPORT_MS);
        if self.unattributed != (0, 0) && due(self.reported.0) {
            let (d, r) = std::mem::take(&mut self.unattributed);
            out(Level::Info, format!("sandbox.audit unattributed domains={d} records={r}"));
            self.reported.0 = Some(now);
        }
        if self.dropped != (0, 0) && due(self.reported.1) {
            let (m, l) = std::mem::take(&mut self.dropped);
            out(Level::Info, format!("sandbox.audit dropped malformed={m} long={l}"));
            self.reported.1 = Some(now);
        }
        if self.suppressed != 0 && due(self.reported.2) {
            let n = std::mem::take(&mut self.suppressed);
            out(Level::Info, format!("sandbox.audit suppressed lines={n}"));
            self.reported.2 = Some(now);
        }
    }
}

// ── The reader ───────────────────────────────────────────────────────────

pub struct Config {
    pub path: PathBuf,
    /// The daemon's own exe (`None`: unknown, and then no domain is
    /// attributed: every allocation is counted as unattributed).
    pub exe: Option<Vec<u8>>,
    pub pid: u32,
    pub poll: Duration,
    /// Unix time (for the join only).
    pub clock: Clock,
    /// The monotonic clock (every elapsed-time gate).
    pub mono: Clock,
    /// The line budgets: new `src=audit` lines a minute, per session and
    /// host-wide.
    pub lines_per_min: (u64, u64),
    pub sink: Sink,
}

impl Config {
    /// The daemon's: the default audit log, this process's exe (taken now,
    /// ` (deleted)` stripped) and pid, lines to the sandbox log target.
    pub fn daemon() -> Config {
        Config {
            path: PathBuf::from(super::policy::DEFAULT_AUDIT_LOG),
            exe: std::fs::read_link("/proc/self/exe").ok().map(|p| strip_deleted(p.as_os_str().as_bytes()).to_vec()),
            pid: std::process::id(),
            poll: POLL,
            clock: Arc::new(now_ms),
            mono: Arc::new(mono_ms),
            lines_per_min: (SESSION_LINES_PER_MIN, HOST_LINES_PER_MIN),
            sink: Arc::new(|level: Level, line: &str| match level {
                Level::Warn => super::log_line(line),
                Level::Info => super::log_info(line),
            }),
        }
    }
}

#[derive(Clone, Copy)]
enum Status {
    Never,
    Down { last_try: u64 },
    Up,
}

#[derive(Default)]
struct Stop {
    stopped: Mutex<bool>,
    wake: Condvar,
}

/// The audit log's reader: one per daemon ([`enable_daemon`]).
pub struct Reader {
    path: PathBuf,
    exe: Option<Vec<u8>>,
    pid: u32,
    poll: Duration,
    mono: Clock,
    lines_per_min: (u64, u64),
    sink: Sink,
    registry: Arc<Registry>,
    status: Mutex<Status>,
    stop: Arc<Stop>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Reader {
    pub fn new(cfg: Config) -> Arc<Reader> {
        Arc::new(Reader {
            registry: Registry::with_clocks(cfg.clock, Arc::clone(&cfg.mono)),
            path: cfg.path,
            exe: cfg.exe,
            pid: cfg.pid,
            poll: cfg.poll,
            mono: cfg.mono,
            lines_per_min: cfg.lines_per_min,
            sink: cfg.sink,
            status: Mutex::new(Status::Never),
            stop: Arc::new(Stop::default()),
            thread: Mutex::new(None),
        })
    }

    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// The one synchronous step, in an enforcing spawn's prepare before it
    /// forks: the first time, open the log at its end and start the thread.
    /// A failed open logs `sandbox.audit unavailable reason=<r>` once and is
    /// retried by a later spawn at most every [`RETRY_MS`] on the monotonic
    /// clock (a failed retry says nothing; one that works logs
    /// `sandbox.audit available`).
    pub fn before_spawn(self: &Arc<Self>, log: &mut dyn FnMut(String)) {
        let mut status = lock(&self.status);
        let now = (self.mono)();
        match *status {
            Status::Up => return,
            Status::Down { last_try } if now.saturating_sub(last_try) < RETRY_MS => return,
            _ => {}
        }
        if *lock(&self.stop.stopped) {
            return;
        }
        match Follower::open_at_end(&self.path).and_then(|f| self.start(f)) {
            Ok(()) => {
                if matches!(*status, Status::Down { .. }) {
                    log("sandbox.audit available".into());
                }
                *status = Status::Up;
            }
            Err(e) => {
                if matches!(*status, Status::Never) {
                    log(format!("sandbox.audit unavailable reason={}", reason(&e)));
                }
                *status = Status::Down { last_try: now };
            }
        }
    }

    fn start(&self, mut f: Follower) -> io::Result<()> {
        let registry = Arc::clone(&self.registry);
        let stop = Arc::clone(&self.stop);
        let mono = Arc::clone(&self.mono);
        let sink = Arc::clone(&self.sink);
        let poll = self.poll;
        let mut tail = Tail::new(self.exe.clone(), self.pid);
        tail.set_budget(self.lines_per_min.0, self.lines_per_min.1);
        // Not "sbx-…": that would be a valid spawn tag.
        let handle = std::thread::Builder::new().name("synaps-audit".into()).spawn(move || loop {
            let stopping = || *lock(&stop.stopped);
            tail.poll(&mut f, &registry, mono(), &stopping, &mut |level, line| sink(level, &line));
            let stopped = lock(&stop.stopped);
            if *stopped {
                return;
            }
            let (stopped, _) = stop.wake.wait_timeout(stopped, poll).unwrap_or_else(|p| p.into_inner());
            if *stopped {
                return;
            }
        })?;
        *lock(&self.thread) = Some(handle);
        Ok(())
    }

    /// Stop the thread after its current read (at once if it is waiting);
    /// never waits for it. A stopped reader doesn't open the log again.
    pub fn stop(&self) {
        *lock(&self.stop.stopped) = true;
        self.stop.wake.notify_all();
    }

    #[cfg(test)]
    pub(crate) fn join(&self) {
        let handle = lock(&self.thread).take();
        if let Some(h) = handle {
            let _ = h.join();
        }
    }
}

static GLOBAL: OnceLock<Arc<Reader>> = OnceLock::new();

/// The reader for this process's enforcing spawns: the daemon's, once
/// [`enable_daemon`] made it; `None` in any other process (a CLI, a test
/// binary), whose spawns then open no audit log and start no thread.
pub fn daemon_reader() -> Option<Arc<Reader>> {
    GLOBAL.get().cloned()
}

/// Make the daemon's reader (the daemon, at its start, before any session).
/// It opens nothing until the first enforcing spawn.
pub fn enable_daemon() {
    GLOBAL.get_or_init(|| Reader::new(Config::daemon()));
}

/// Stop the daemon's reader, if there is one (daemon shutdown).
pub fn stop_global() {
    if let Some(r) = GLOBAL.get() {
        r.stop();
    }
}

/// Why the log can't be read, as a log field: an errno name, `not-a-file`,
/// or `errno-<n>`.
pub fn reason(e: &io::Error) -> String {
    if e.get_ref().is_some_and(|inner| inner.is::<NotAFile>()) {
        return "not-a-file".into();
    }
    let Some(n) = e.raw_os_error() else {
        return format!("{:?}", e.kind());
    };
    let name = match n {
        libc::ENOENT => "ENOENT",
        libc::EACCES => "EACCES",
        libc::EPERM => "EPERM",
        libc::ENOTDIR => "ENOTDIR",
        libc::ELOOP => "ELOOP",
        libc::EIO => "EIO",
        libc::EMFILE => "EMFILE",
        libc::ENFILE => "ENFILE",
        libc::ENOMEM => "ENOMEM",
        libc::EAGAIN => "EAGAIN",
        libc::EINTR => "EINTR",
        libc::ESTALE => "ESTALE",
        _ => return format!("errno-{n}"),
    };
    name.into()
}

#[cfg(test)]
mod tests;
