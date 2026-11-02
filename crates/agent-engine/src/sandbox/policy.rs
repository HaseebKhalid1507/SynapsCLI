//! Per-session sandbox policy: the file format, its validation, the global
//! kill switch, and the validator report behind `synaps sandbox check`.
//!
//! One file per session, `<base>/sandbox/sessions/<session-id>.conf`, read at
//! every bash/PTY spawn. No file means no sandbox, and the spawn path stays
//! exactly as it was. `<base>/sandbox/OFF` turns every sandbox off at once.
//! A file that can't be read, parsed or validated refuses that session's
//! spawns (fail closed). `docs/sandbox.md` documents the format.
//!
//! Validation never widens, and it checks the object that was actually
//! opened, not a path string: every path is opened `O_PATH` (following
//! symlinks), the kernel's name for that open object (`/proc/self/fd/N`) is
//! checked, and the Landlock rule is later built from the same fd. A path is
//! refused if it is `/`, has fewer than 3 components, is `$HOME` or an
//! ancestor of it, or overlaps the Synaps base dir (equal to, under, or an
//! ancestor of it); if the opened object is, under another name, the
//! directory of `$HOME`, the base dir, one of their ancestors, or the base
//! dir's `sandbox/` or `sandbox/sessions/`; or if it is a file with more than
//! one hard link. A grant equal to, under, or an ancestor of one of owner
//! rule 27c's `$HOME` paths (`super::protected`: the Jev state dir, the Jev
//! guard's config, the Synaps config) is refused by name, and, under another
//! name, by the identity of the root or of a directory above it, whatever
//! the base dir is. `device = /dev/<name>` must name a node directly under
//! /dev, by its text and by the kernel's name for the opened object, that is
//! a character device with exactly one link; only it may have 2 components
//! (`/dev/<name>`). A baseline directory (`/tmp`, ...) that contains `$HOME`
//! or the base dir refuses the whole policy, and so does a protected root that
//! can't be resolved and statted. A path that doesn't exist is skipped
//! (narrower is safe) and reported.
//!
//! Trust assumption: the protected tree and the mount namespace aren't
//! rearranged by unconfined processes after validation (a bind mount deeper
//! inside an allowed directory, or a protected subtree moved into one, isn't
//! chased). Within it, a sandboxed command can't rewrite the policy that
//! governs it, and swapping a checked path for a symlink afterwards changes
//! nothing.
//!
//! Off Linux (no Landlock, no `O_PATH`, no `/proc`) the same reader, parser
//! and checks run, but a path is named by `canonicalize` and statted by
//! that name (`Named`) instead of through an `O_PATH` fd: there is no
//! rule to build from an fd, and every enforcing session's spawns are
//! refused there anyway (`super::landlock::Unavailable::UnsupportedOs`).

use std::fmt;
use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt};

#[cfg(not(unix))]
use super::nonunix::{FileTypeExt, MetadataExt, OsStrExt};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

#[cfg(target_os = "linux")]
use super::landlock::Ruleset;
use super::landlock::{
    handled_for_abi, Unavailable, ACCESS_FS_MAKE_BLOCK, ACCESS_FS_MAKE_CHAR, ACCESS_FS_MAKE_DIR,
    ACCESS_FS_MAKE_FIFO, ACCESS_FS_MAKE_REG, ACCESS_FS_MAKE_SOCK, ACCESS_FS_MAKE_SYM, ACCESS_FS_REFER,
    ACCESS_FS_REMOVE_DIR, ACCESS_FS_REMOVE_FILE, ACCESS_FS_TRUNCATE, ACCESS_FS_WRITE_FILE, FILE_WRITE_RIGHTS,
    WRITE_RIGHTS,
};

#[cfg(all(test, target_os = "linux"))]
mod tests;

/// Policy files, relative to the Synaps base dir.
pub const SESSIONS_DIR: &str = "sandbox/sessions";
/// The sandbox's own directory, relative to the Synaps base dir.
pub const SANDBOX_DIR: &str = "sandbox";
/// The fleet-wide kill switch, relative to the Synaps base dir.
pub const KILL_SWITCH_FILE: &str = "sandbox/OFF";
/// Policy files are small; a larger one is refused.
pub const MAX_POLICY_BYTES: u64 = 64 * 1024;
/// The kernel audit log the daemon's denial reader follows (the only one: a
/// policy's `audit_log` can only turn it off for its session).
pub const DEFAULT_AUDIT_LOG: &str = "/var/log/audit/audit.log";
/// The advice sentence of a refusal when the file sets no `contact`.
pub const DEFAULT_CONTACT: &str = "Ask the operator who manages this session's sandbox policy.";
/// Logged once when the kill switch appears.
pub const LINE_GLOBAL_OFF: &str = "sandbox.off reason=global";
/// Logged once when the kill switch goes away again.
pub const LINE_GLOBAL_ON: &str = "sandbox.on reason=global-cleared";

/// At most this many rules per policy, the baseline and every expansion
/// included; a file that would build more is refused.
pub const MAX_RULES: usize = 256;

const MAX_CONTACT_CHARS: usize = 200;

/// At most this many `tool =` lines per policy (`tools = confined`).
pub const MAX_NAMED_TOOLS: usize = 64;

/// The rights the baseline grants beneath /proc: procfs files can be written
/// (`> /dev/stdout` resolves to /proc/self/fd/1), never created or removed.
const PROC_RIGHTS: u64 = ACCESS_FS_WRITE_FILE | ACCESS_FS_TRUNCATE | ACCESS_FS_MAKE_REG;

/// Always allowed while sandboxed: (path, rights, is a directory rule). A
/// baseline directory must not contain `$HOME` or the base dir.
pub const BASELINE: &[(&str, u64, bool)] = &[
    ("/tmp", WRITE_RIGHTS, true),
    ("/dev/shm", WRITE_RIGHTS, true),
    ("/dev/null", FILE_WRITE_RIGHTS, false),
    ("/dev/zero", FILE_WRITE_RIGHTS, false),
    ("/dev/tty", FILE_WRITE_RIGHTS, false),
    ("/dev/ptmx", FILE_WRITE_RIGHTS, false),
    ("/dev/pts", FILE_WRITE_RIGHTS, true),
    ("/proc", PROC_RIGHTS, true),
];

/// What `git_common = <repo>/.git` expands to. Everything else in the common
/// dir (config, hooks/, packed-refs, index, info/, rr-cache/) stays
/// read-only: a writable hook or config would run the agent's code in the
/// next unsandboxed git.
pub const GIT_COMMON_DIRS: &[&str] = &["objects", "refs", "logs", "worktrees"];
/// What `cargo_home = <dir>` expands to (directories).
pub const CARGO_HOME_DIRS: &[&str] = &["registry", "git"];
/// What `cargo_home = <dir>` expands to (files; cargo's lock and cache db).
pub const CARGO_HOME_FILES: &[&str] = &[".package-cache", ".package-cache-mutate", ".global-cache"];

/// Where the policy lives and what it must never cover.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyEnv {
    pub home: PathBuf,
    pub base_dir: PathBuf,
}

impl PolicyEnv {
    /// `$HOME` and the Synaps base dir of this process.
    pub fn from_process() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("/"));
        PolicyEnv { home, base_dir: crate::config::base_dir() }
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.base_dir.join(SESSIONS_DIR)
    }

    pub fn kill_switch(&self) -> PathBuf {
        self.base_dir.join(KILL_SWITCH_FILE)
    }

    /// The policy file for `session_id`; `None` when the id can't name a
    /// file (then no policy can exist for it).
    pub fn policy_path(&self, session_id: &str) -> Option<PathBuf> {
        valid_session_id(session_id).then(|| self.sessions_dir().join(format!("{session_id}.conf")))
    }
}

/// Session ids that map to a policy file name: ASCII letters, digits, `-`,
/// `_`, `.`, not starting with `.`, at most 128 bytes.
pub fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('.')
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Enforce,
    Off,
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Enforce => "enforce",
            Mode::Off => "off",
        }
    }
}

/// Whether the session's subagents are sandboxed. Always `Enforce` for an
/// enforcing session; the default for a `mode = off` session too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subagents {
    Enforce,
    Off,
}

impl Subagents {
    pub fn as_str(&self) -> &'static str {
        match self {
            Subagents::Enforce => "enforce",
            Subagents::Off => "off",
        }
    }
}

/// Whether the session's in-process tools are held to the policy too
/// (`tools = confined`, only with `mode = enforce`; see `super::confine`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tools {
    /// The default: only bash/PTY children are confined (and 27c's paths).
    Open,
    /// The file tools are held to the write set, worker tools are refused,
    /// and every other tool runs only if built in or named by `tool =`.
    Confined,
}

impl Tools {
    pub fn as_str(&self) -> &'static str {
        match self {
            Tools::Open => "open",
            Tools::Confined => "confined",
        }
    }
}

/// Work rules are what a scoped subagent's write set is cut from; infra
/// rules (caches, build dirs, the git common subset) every subagent keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleClass {
    Baseline,
    Work,
    Infra,
}

impl RuleClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            RuleClass::Baseline => "baseline",
            RuleClass::Work => "work",
            RuleClass::Infra => "infra",
        }
    }
}

/// One path-beneath rule as it will be applied. Only [`parse`] makes one,
/// so its rights are always nonzero and match its type.
#[derive(Clone, Debug)]
pub struct Rule {
    /// The kernel's name for the opened object when it was checked.
    path: PathBuf,
    /// The opened object itself (`O_PATH`): validation ran on it, and
    /// [`Policy::to_ruleset`] builds the rule from it, never by reopening
    /// `path`. Off Linux, the object as named and statted (`Named`).
    fd: Arc<Handle>,
    access: u64,
    dir: bool,
    class: RuleClass,
    /// The key that produced it (`baseline` for the baseline).
    key: &'static str,
    /// Line in the policy file (0 for the baseline).
    line: usize,
}

impl Rule {
    /// The kernel's name for the object when it was checked.
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn access(&self) -> u64 {
        self.access
    }
    pub fn is_dir(&self) -> bool {
        self.dir
    }
    pub fn class(&self) -> RuleClass {
        self.class
    }
    pub fn key(&self) -> &'static str {
        self.key
    }
    pub fn line(&self) -> usize {
        self.line
    }
    /// The identity (dev, ino) of the opened object the rule is built from,
    /// if it can be statted.
    pub fn identity(&self) -> Option<(u64, u64)> {
        fd_metadata(&self.fd).ok().map(|m| (m.dev(), m.ino()))
    }
}

/// A path named by the file (or an expansion) that doesn't exist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    pub line: usize,
    pub key: &'static str,
    pub path: PathBuf,
}

/// A parsed, validated policy. Only [`parse`] makes one.
#[derive(Clone, Debug)]
pub struct Policy {
    file: PathBuf,
    /// First 16 hex digits of the file's SHA-256 (log lines, change detection).
    hash: String,
    mode: Mode,
    required: bool,
    lane: Option<String>,
    subagents: Subagents,
    tools: Tools,
    /// The `tool =` names, in file order (only with `tools = confined`).
    named_tools: Vec<String>,
    /// `None` when `audit_log = off`.
    audit_log: Option<PathBuf>,
    contact: String,
    /// Baseline first, then the file's rules in file order.
    rules: Vec<Rule>,
    skipped: Vec<Skipped>,
}

impl Policy {
    pub fn file(&self) -> &Path {
        &self.file
    }
    /// First 16 hex digits of the file's SHA-256.
    pub fn hash(&self) -> &str {
        &self.hash
    }
    pub fn mode(&self) -> Mode {
        self.mode
    }
    pub fn required(&self) -> bool {
        self.required
    }
    pub fn lane(&self) -> Option<&str> {
        self.lane.as_deref()
    }
    pub fn subagents(&self) -> Subagents {
        self.subagents
    }
    pub fn tools(&self) -> Tools {
        self.tools
    }
    /// The `tool =` names, in file order.
    pub fn named_tools(&self) -> &[String] {
        &self.named_tools
    }
    /// Whether this policy confines the session's in-process tools: `mode =
    /// enforce` with `tools = confined`.
    pub fn confines_tools(&self) -> bool {
        self.mode == Mode::Enforce && self.tools == Tools::Confined
    }
    /// `None` when `audit_log = off`.
    pub fn audit_log(&self) -> Option<&Path> {
        self.audit_log.as_deref()
    }
    /// The advice sentence for refusals and hints.
    pub fn contact(&self) -> &str {
        &self.contact
    }
    /// Baseline first, then the file's rules in file order.
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }
    pub fn skipped(&self) -> &[Skipped] {
        &self.skipped
    }
}

/// Why a policy file was refused. `line` is 1-based; 0 = the whole file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyError {
    pub file: PathBuf,
    pub line: usize,
    pub reason: String,
    pub contact: String,
    /// The file has a `tools = confined` line (found leniently, like the
    /// contact): the session's in-process tools stay confined, with an empty
    /// write set, while the file is broken (fail closed).
    pub confined: bool,
}

impl PolicyError {
    fn new(file: &Path, line: usize, reason: impl Into<String>, contact: &str) -> Self {
        PolicyError { file: file.to_path_buf(), line, reason: reason.into(), contact: contact.to_string(), confined: false }
    }

    /// `<file>:<line>` (just `<file>` for a whole-file error).
    pub fn location(&self) -> String {
        if self.line == 0 {
            self.file.display().to_string()
        } else {
            format!("{}:{}", self.file.display(), self.line)
        }
    }

    /// The text a refused spawn returns to the agent.
    pub fn refusal(&self) -> String {
        format!(
            "sandbox policy invalid for this session ({}: {}); commands can't run until it's fixed. {}",
            self.location(),
            self.reason,
            self.contact
        )
    }

    /// The error without anything quoted from the file, for text that leaves
    /// the sandbox (the compaction carry's line and the compaction error). A
    /// line's reason can quote that line, so only its number is given, with
    /// the command that prints the reason. A whole-file reason (line 0) is
    /// the reader's or validator's own text about the file (its type, links,
    /// size, encoding, a missing `mode`, the base dir checks), never the
    /// file's content, so it is kept.
    pub fn summary(&self) -> String {
        if self.line == 0 {
            self.to_string()
        } else {
            format!(
                "{}: invalid line (`synaps sandbox check {}` gives the reason)",
                self.location(),
                self.file.display()
            )
        }
    }
}

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.location(), self.reason)
    }
}

impl std::error::Error for PolicyError {}

/// Why a valid policy couldn't become a Landlock ruleset at spawn time.
#[derive(Debug)]
pub enum BuildError {
    /// No usable Landlock on this host.
    Unavailable(Unavailable),
    /// The kernel refused one rule.
    Rule { path: PathBuf, error: io::Error },
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::Unavailable(u) => write!(f, "landlock unavailable: {u}"),
            BuildError::Rule { path, error } => write!(f, "rule for `{}` refused: {error}", path.display()),
        }
    }
}

impl std::error::Error for BuildError {}

#[cfg(target_os = "linux")]
impl Policy {
    /// The Landlock ruleset for this policy: one rule per [`Rule`], built
    /// from the fd validation checked (never by reopening the path).
    pub fn to_ruleset(&self) -> Result<Ruleset, BuildError> {
        let mut rs = Ruleset::new().map_err(BuildError::Unavailable)?;
        for r in &self.rules {
            rs.allow_fd(r.fd.as_fd(), r.access).map_err(|error| BuildError::Rule { path: r.path.clone(), error })?;
        }
        Ok(rs)
    }
}

/// The running ABI as the sandbox can use it: an ABI below
/// [`super::landlock::MIN_ABI`] is unavailable (`abi-too-old`).
pub fn usable_abi(abi: Result<u32, Unavailable>) -> Result<u32, Unavailable> {
    abi.and_then(|v| handled_for_abi(v).map(|_| v))
}

/// What validation protects: the canonical `$HOME` and base dir, and the
/// identities (device, inode) of both, of every ancestor of each, and of the
/// base dir's `sandbox/` and `sandbox/sessions/`.
struct Guard {
    home: PathBuf,
    base: PathBuf,
    protected: Vec<(u64, u64)>,
    /// Owner rule 27c's `$HOME` roots (`super::protected`): their names,
    /// and the identities of each existing root and every directory above it.
    owner: super::protected::GrantRoots,
}

impl Guard {
    /// Fails closed: if `$HOME`, the base dir or an ancestor of either can't
    /// be resolved and statted, there is no policy (the caller refuses it).
    fn new(env: &PolicyEnv) -> Result<Self, String> {
        let canonical = |p: &Path, what: &str| {
            std::fs::canonicalize(p).map_err(|e| format!("cannot resolve {what} `{}`: {e}", p.display()))
        };
        let home = canonical(env.home.as_path(), "$HOME")?;
        let base = canonical(env.base_dir.as_path(), "the Synaps base dir")?;
        if home == Path::new("/") {
            return Err("$HOME is `/`, so there is no home to protect".into());
        }
        let mut protected = Vec::new();
        for top in [&home, &base] {
            for p in top.ancestors() {
                let m = std::fs::metadata(p).map_err(|e| format!("cannot stat the protected `{}`: {e}", p.display()))?;
                protected.push((m.dev(), m.ino()));
            }
        }
        // The sandbox's own state, under any other name. Not there yet =
        // nothing to alias.
        for rel in [SANDBOX_DIR, SESSIONS_DIR] {
            let p = base.join(rel);
            match std::fs::metadata(&p) {
                Ok(m) => protected.push((m.dev(), m.ino())),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("cannot stat the protected `{}`: {e}", p.display())),
            }
        }
        // Owner rule 27c: whatever the base dir is.
        let owner = super::protected::grant_roots(&home)?;
        Ok(Guard { home, base, protected, owner })
    }

    /// The scope checks every rule passes: on `c`, the kernel's name for the
    /// opened object, and on `id`, that object's (device, inode). Tests call
    /// this form; validation calls [`Self::check_scope_as`].
    #[cfg(test)]
    fn check_scope(&self, c: &Path, id: (u64, u64)) -> Result<(), String> {
        self.check_scope_as(c, id, false)
    }

    /// The scope checks; for a `device` node (`dev_node`), `/dev/<name>`
    /// may have 2 components (its other checks keep it to one char device).
    fn check_scope_as(&self, c: &Path, id: (u64, u64), dev_node: bool) -> Result<(), String> {
        if c == Path::new("/") {
            return Err("refuses `/`".into());
        }
        let normal = c.components().filter(|k| matches!(k, Component::Normal(_))).count();
        let under_baseline_tmp = c.starts_with("/tmp") || c.starts_with("/dev/shm");
        let dev_name = dev_node && c.parent() == Some(Path::new("/dev"));
        if normal < 3 && !under_baseline_tmp && !dev_name {
            return Err(format!("`{}` has fewer than 3 path components", c.display()));
        }
        if self.home.starts_with(c) {
            return Err(format!("`{}` is $HOME ({}) or an ancestor of it", c.display(), self.home.display()));
        }
        if c.starts_with(&self.base) || self.base.starts_with(c) {
            return Err(format!(
                "`{}` overlaps the Synaps base dir {} (a sandboxed command must never be able to change its own policy)",
                c.display(),
                self.base.display()
            ));
        }
        if let Some(root) = self.owner.names.iter().find(|r| c.starts_with(r) || r.starts_with(c)) {
            return Err(format!(
                "`{}` is, is under or holds `{}`, which owner rule 27c leaves to JR's app and JR (no agent may write it)",
                c.display(),
                root.display()
            ));
        }
        // The same directory under another name (a bind mount).
        if self.protected.contains(&id) {
            return Err(format!(
                "`{}` is the same directory as $HOME, the Synaps base dir, one of their ancestors, or the base dir's sandbox/ or sandbox/sessions/",
                c.display()
            ));
        }
        if self.owner.ids.contains(&id) {
            return Err(format!(
                "`{}` is the same object as a path owner rule 27c protects, or as a directory above one",
                c.display()
            ));
        }
        Ok(())
    }

    /// A baseline directory gets no scope checks (it is granted whole), so it
    /// must contain neither `$HOME` nor the base dir, by name or by identity.
    fn check_baseline(&self, c: &Path, id: (u64, u64)) -> Result<(), String> {
        if self.home.starts_with(c) || self.base.starts_with(c) || self.protected.contains(&id) {
            return Err(format!(
                "the baseline directory `{}` (always writable while sandboxed) contains $HOME ({}) or the Synaps base dir ({}); \
                 the base dir and $HOME must not be under /tmp, /dev/shm, /dev/pts or /proc when sandboxing",
                c.display(),
                self.home.display(),
                self.base.display()
            ));
        }
        if self.owner.names.iter().any(|r| r.starts_with(c)) || self.owner.ids.contains(&id) {
            return Err(format!(
                "the baseline directory `{}` (always writable while sandboxed) holds a path owner rule 27c protects",
                c.display()
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Want {
    Dir,
    File,
    /// A character device directly under /dev (`device`).
    Device,
}

#[cfg(test)]
thread_local! {
    /// Tests: on this thread, the directory a `device` node must be in,
    /// instead of /dev, so the accept and refuse paths can be shown on
    /// scratch nodes no real /dev has. `None` = /dev.
    pub(crate) static TEST_DEV_ROOT: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// The directory a `device` node must be directly in: /dev.
fn dev_root() -> PathBuf {
    #[cfg(test)]
    if let Some(root) = TEST_DEV_ROOT.with(|r| r.borrow().clone()) {
        return root;
    }
    PathBuf::from("/dev")
}

/// `/dev/<name>` by its text alone, before anything is opened: exactly one
/// name directly in the dev root, split on `/` as written (`Path`'s own
/// components would hide `.` and `//`), so no `.`, `..`, empty component or
/// trailing `/`. Anything else is an error even when it doesn't exist.
fn is_dev_name(raw: &Path, root: &Path) -> bool {
    let mut prefix = root.as_os_str().as_bytes().to_vec();
    prefix.push(b'/');
    match raw.as_os_str().as_bytes().strip_prefix(prefix.as_slice()) {
        Some(name) => !name.is_empty() && name != b"." && name != b".." && !name.contains(&b'/'),
        None => false,
    }
}

/// The type and link checks on an opened `device` node, by the kernel's
/// name for it (`c`): a character device with exactly one name. A node
/// with none was unlinked after it was named; with more, its other names
/// may be protected ones, and a grant covers the inode.
fn device_node_ok(c: &Path, char_device: bool, nlink: u64) -> Result<(), String> {
    if !char_device {
        return Err(format!("`{}` is not a character device", c.display()));
    }
    if nlink != 1 {
        return Err(format!("`{}` has {nlink} hard links: a device node must have exactly one", c.display()));
    }
    Ok(())
}

/// What validation holds of a checked object: on Linux its `O_PATH` fd,
/// elsewhere the object as named and statted (`Named`).
#[cfg(target_os = "linux")]
type Handle = OwnedFd;
#[cfg(not(target_os = "linux"))]
type Handle = Named;

/// A checked object off Linux: its canonical name and its metadata, taken
/// by that name without opening it (an `O_PATH` stand-in that needs no
/// read permission and never runs a device's open: `/dev/tty` doesn't open
/// at all in a daemon without a terminal). Compiled for Linux tests too.
#[cfg(any(test, not(target_os = "linux")))]
#[derive(Debug)]
pub(crate) struct Named {
    path: PathBuf,
    meta: std::fs::Metadata,
}

/// `p` (following symlinks) as a [`Named`]: `canonicalize`, then
/// `symlink_metadata` of that name, which must not be a symlink (a canonical
/// name has none, unless the tree changed between the two calls).
#[cfg(any(test, not(target_os = "linux")))]
pub(crate) fn open_named(p: &Path) -> io::Result<Named> {
    let path = std::fs::canonicalize(p)?;
    let meta = std::fs::symlink_metadata(&path)?;
    if meta.file_type().is_symlink() || !path.is_absolute() {
        return Err(io::Error::other(format!("`{}` changed while it was checked", path.display())));
    }
    Ok(Named { path, meta })
}

#[cfg(not(target_os = "linux"))]
fn open_path(p: &Path) -> io::Result<Handle> {
    open_named(p)
}

#[cfg(not(target_os = "linux"))]
fn fd_path(h: &Handle) -> io::Result<PathBuf> {
    Ok(h.path.clone())
}

#[cfg(not(target_os = "linux"))]
fn fd_metadata(h: &Handle) -> io::Result<std::fs::Metadata> {
    Ok(h.meta.clone())
}

/// Open `p` as a handle only (`O_PATH`, following symlinks).
#[cfg(target_os = "linux")]
fn open_path(p: &Path) -> io::Result<OwnedFd> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
        .open(p)
        .map(OwnedFd::from)
}

/// The kernel's current absolute name for an open object.
#[cfg(target_os = "linux")]
fn fd_path(fd: &OwnedFd) -> io::Result<PathBuf> {
    let link = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))?;
    if !link.is_absolute() || link.as_os_str().as_bytes().ends_with(b" (deleted)") {
        return Err(io::Error::other(format!("no usable name ({})", link.display())));
    }
    Ok(link)
}

#[cfg(target_os = "linux")]
fn fd_metadata(fd: &OwnedFd) -> io::Result<std::fs::Metadata> {
    std::fs::File::from(fd.try_clone()?).metadata()
}

/// Open `raw`, then check the opened object: its name, scope and type.
/// `Ok(None)` = doesn't exist.
fn resolve(raw: &Path, want: Want, guard: &Guard) -> Result<Option<(PathBuf, Handle)>, String> {
    if !raw.is_absolute() {
        return Err(format!("`{}` is not absolute (write the full path; `~` and variables are not expanded)", raw.display()));
    }
    let root = dev_root();
    if want == Want::Device && !is_dev_name(raw, &root) {
        return Err(format!("`{}` is not a device directly under /dev (`device` takes /dev/<name>, one name)", raw.display()));
    }
    let fd = match open_path(raw) {
        Ok(fd) => fd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot open `{}`: {e}", raw.display())),
    };
    let c = fd_path(&fd).map_err(|e| format!("cannot name the opened `{}`: {e}", raw.display()))?;
    let meta = fd_metadata(&fd).map_err(|e| format!("cannot stat `{}`: {e}", c.display()))?;
    // Through a symlink, the opened object may be elsewhere (outside /dev, or
    // nested in it): checked before its type, so the message names where it is.
    if want == Want::Device && c.parent() != Some(root.as_path()) {
        return Err(format!("`{}` (opened as `{}`) is not a node directly under /dev", c.display(), raw.display()));
    }
    guard.check_scope_as(&c, (meta.dev(), meta.ino()), want == Want::Device)?;
    match want {
        Want::Dir if !meta.is_dir() => Err(format!("`{}` is not a directory", c.display())),
        Want::File if !meta.is_file() => Err(format!("`{}` is not a regular file", c.display())),
        Want::Device => device_node_ok(&c, meta.file_type().is_char_device(), meta.nlink()).map(|()| Some((c, fd))),
        // Its other names may be protected ones; a grant covers the inode.
        Want::File if meta.nlink() > 1 => Err(format!(
            "`{}` has {} hard links: a file with more than one name can't be granted",
            c.display(),
            meta.nlink()
        )),
        _ => Ok(Some((c, fd))),
    }
}

/// The `contact` value, found leniently (first well-formed `contact =` line)
/// so even a refusal for a broken file carries it.
fn scan_contact(text: &str) -> String {
    for line in text.lines() {
        let Some((k, v)) = line.trim().split_once('=') else { continue };
        if k.trim() == "contact" {
            let v = v.trim();
            if valid_contact(v) {
                return v.to_string();
            }
        }
    }
    DEFAULT_CONTACT.to_string()
}

/// Whether the text has a `tools = confined` line, found leniently (like the
/// contact), so a file that says so but can't be used keeps the session's
/// tools confined (fail closed). A `#` line doesn't count.
fn scan_confined(text: &str) -> bool {
    text.lines().any(|line| line.trim().split_once('=').is_some_and(|(k, v)| k.trim() == "tools" && v.trim() == "confined"))
}

/// A contact sentence: 1-200 characters, no control characters, and not the
/// word "password" (prompt-like text has no place in a refusal).
fn valid_contact(v: &str) -> bool {
    !v.is_empty()
        && v.chars().count() <= MAX_CONTACT_CHARS
        && !v.chars().any(|c| c.is_control())
        && !v.to_lowercase().contains("password")
}

fn valid_lane(v: &str) -> bool {
    !v.is_empty() && v.len() <= 64 && v.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// First 16 hex digits of SHA-256.
pub fn short_hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Parse and validate a policy file's text. `file` only labels errors.
pub fn parse(text: &str, file: &Path, env: &PolicyEnv) -> Result<Policy, PolicyError> {
    parse_with(text, file, env, BASELINE)
}

/// [`parse`] with a given baseline (tests use scratch stand-ins).
fn parse_with(text: &str, file: &Path, env: &PolicyEnv, baseline: &[(&str, u64, bool)]) -> Result<Policy, PolicyError> {
    if text.len() as u64 > MAX_POLICY_BYTES {
        return Err(PolicyError::new(file, 0, format!("larger than {MAX_POLICY_BYTES} bytes"), DEFAULT_CONTACT));
    }
    let contact = scan_contact(text);
    let confined = scan_confined(text);
    let err = |line: usize, reason: String| {
        let mut e = PolicyError::new(file, line, reason, &contact);
        e.confined = confined;
        e
    };

    let mut mode: Option<(Mode, usize)> = None;
    let mut required: Option<(bool, usize)> = None;
    let mut lane: Option<(String, usize)> = None;
    let mut subagents: Option<(Subagents, usize)> = None;
    let mut audit_log: Option<(Option<PathBuf>, usize)> = None;
    let mut tools: Option<(Tools, usize)> = None;
    let mut named: Vec<(String, usize)> = Vec::new();
    let mut contact_seen: Option<usize> = None;
    let mut paths: Vec<(&'static str, String, usize)> = Vec::new();

    fn once<T>(slot: &mut Option<(T, usize)>, v: T, key: &str, line: usize) -> Result<(), String> {
        if let Some((_, first)) = slot {
            return Err(format!("`{key}` set twice (first on line {first})"));
        }
        *slot = Some((v, line));
        Ok(())
    }

    for (i, raw) in text.lines().enumerate() {
        let n = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(err(n, format!("expected `key = value`, got `{line}`")));
        };
        let (key, value) = (key.trim(), value.trim());
        if value.is_empty() {
            return Err(err(n, format!("`{key}` has no value")));
        }
        if value.chars().any(|c| c.is_control() && c != '\t') {
            return Err(err(n, format!("`{key}` has a control character in its value")));
        }
        match key {
            "mode" => {
                let m = match value {
                    "enforce" => Mode::Enforce,
                    "off" => Mode::Off,
                    _ => return Err(err(n, format!("`mode` must be `enforce` or `off`, got `{value}`"))),
                };
                once(&mut mode, m, key, n).map_err(|r| err(n, r))?;
            }
            "required" => {
                let b = match value {
                    "true" => true,
                    "false" => false,
                    _ => return Err(err(n, format!("`required` must be `true` or `false`, got `{value}`"))),
                };
                once(&mut required, b, key, n).map_err(|r| err(n, r))?;
            }
            "subagents" => {
                let s = match value {
                    "enforce" => Subagents::Enforce,
                    "off" => Subagents::Off,
                    _ => return Err(err(n, format!("`subagents` must be `enforce` or `off`, got `{value}`"))),
                };
                once(&mut subagents, s, key, n).map_err(|r| err(n, r))?;
            }
            "lane" => {
                if !valid_lane(value) {
                    return Err(err(n, format!("`lane` must be 1-64 of A-Z a-z 0-9 . _ -, got `{value}`")));
                }
                once(&mut lane, value.to_string(), key, n).map_err(|r| err(n, r))?;
            }
            "audit_log" => {
                let v = match value {
                    "off" => None,
                    DEFAULT_AUDIT_LOG => Some(PathBuf::from(DEFAULT_AUDIT_LOG)),
                    _ => {
                        return Err(err(
                            n,
                            format!(
                                "`audit_log` must be `off` or `{DEFAULT_AUDIT_LOG}` (the daemon reads only that log), got `{value}`"
                            ),
                        ))
                    }
                };
                once(&mut audit_log, v, key, n).map_err(|r| err(n, r))?;
            }
            "contact" => {
                if !valid_contact(value) {
                    return Err(err(
                        n,
                        format!(
                            "`contact` must be one line of at most {MAX_CONTACT_CHARS} characters, without prompt words (docs/sandbox.md lists them)"
                        ),
                    ));
                }
                if let Some(first) = contact_seen {
                    return Err(err(n, format!("`contact` set twice (first on line {first})")));
                }
                contact_seen = Some(n);
            }
            "tools" => {
                let t = match value {
                    "open" => Tools::Open,
                    "confined" => Tools::Confined,
                    _ => return Err(err(n, format!("`tools` must be `open` or `confined`, got `{value}`"))),
                };
                once(&mut tools, t, key, n).map_err(|r| err(n, r))?;
            }
            "tool" => {
                if !super::confine::valid_tool_name(value) {
                    return Err(err(
                        n,
                        format!("`tool` must be 1-{} of A-Z a-z 0-9 _ . : -, got `{value}`", super::confine::MAX_TOOL_NAME),
                    ));
                }
                if super::confine::WORKER_TOOLS.contains(&value) {
                    return Err(err(n, format!("`tool = {value}` names a tool that starts or resumes a worker, which a confined session never runs")));
                }
                if let Some((_, first)) = named.iter().find(|(t, _)| t == value) {
                    return Err(err(n, format!("`tool = {value}` set twice (first on line {first})")));
                }
                if named.len() >= MAX_NAMED_TOOLS {
                    return Err(err(n, format!("more than {MAX_NAMED_TOOLS} `tool =` lines")));
                }
                named.push((value.to_string(), n));
            }
            "write" => paths.push(("write", value.to_string(), n)),
            "write_file" => paths.push(("write_file", value.to_string(), n)),
            "infra" => paths.push(("infra", value.to_string(), n)),
            "infra_file" => paths.push(("infra_file", value.to_string(), n)),
            "git_common" => paths.push(("git_common", value.to_string(), n)),
            "cargo_home" => paths.push(("cargo_home", value.to_string(), n)),
            "device" => paths.push(("device", value.to_string(), n)),
            _ => return Err(err(n, format!("unknown key `{key}`"))),
        }
    }

    let Some((mode, _)) = mode else {
        return Err(err(0, "missing `mode = enforce` or `mode = off`".into()));
    };
    let subagents = match subagents {
        Some((Subagents::Off, n)) if mode == Mode::Enforce => {
            return Err(err(n, "`subagents = off` needs `mode = off` (an enforcing session's subagents are always sandboxed)".into()));
        }
        Some((s, _)) => s,
        None => Subagents::Enforce,
    };
    let required = match required {
        Some((true, n)) if mode == Mode::Off => {
            return Err(err(n, "`required = true` needs `mode = enforce`".into()));
        }
        Some((b, _)) => b,
        None => false,
    };
    let tools = match tools {
        Some((Tools::Confined, n)) if mode == Mode::Off => {
            return Err(err(n, "`tools = confined` needs `mode = enforce` (a session that isn't sandboxed has no write set to hold its tools to)".into()));
        }
        Some((t, _)) => t,
        None => Tools::Open,
    };
    if let Some((_, n)) = named.first() {
        if tools != Tools::Confined {
            return Err(err(*n, "`tool` needs `tools = confined` (only a confined session has a tool list)".into()));
        }
    }

    let guard = Guard::new(env).map_err(|r| err(0, r))?;
    let mut rules: Vec<Rule> = Vec::new();
    let mut skipped = Vec::new();
    for (path, access, dir) in baseline {
        // Fixed system paths: opened and named like the rest. They get no
        // scope check, so a baseline directory must not hold what the scope
        // checks protect. Only "doesn't exist" skips one.
        let fd = match open_path(Path::new(path)) {
            Ok(fd) => fd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                skipped.push(Skipped { line: 0, key: "baseline", path: PathBuf::from(path) });
                continue;
            }
            Err(e) => return Err(err(0, format!("cannot open the baseline path `{path}`: {e}"))),
        };
        let c = fd_path(&fd).map_err(|e| err(0, format!("cannot name the baseline path `{path}`: {e}")))?;
        if *dir {
            let meta = fd_metadata(&fd).map_err(|e| err(0, format!("cannot stat the baseline path `{}`: {e}", c.display())))?;
            guard.check_baseline(&c, (meta.dev(), meta.ino())).map_err(|r| err(0, r))?;
        }
        rules.push(Rule { path: c, fd: Arc::new(fd), access: *access, dir: *dir, class: RuleClass::Baseline, key: "baseline", line: 0 });
    }

    for (key, value, n) in paths {
        let raw = PathBuf::from(&value);
        let (want, class, access) = match key {
            "write" => (Want::Dir, RuleClass::Work, WRITE_RIGHTS),
            "write_file" => (Want::File, RuleClass::Work, FILE_WRITE_RIGHTS),
            "infra" => (Want::Dir, RuleClass::Infra, WRITE_RIGHTS),
            "infra_file" => (Want::File, RuleClass::Infra, FILE_WRITE_RIGHTS),
            "device" => (Want::Device, RuleClass::Infra, FILE_WRITE_RIGHTS),
            _ => (Want::Dir, RuleClass::Infra, WRITE_RIGHTS),
        };
        let Some((c, fd)) = resolve(&raw, want, &guard).map_err(|r| err(n, r))? else {
            skipped.push(Skipped { line: n, key, path: raw });
            continue;
        };
        match key {
            "git_common" => {
                let looks_like_git = c.join("objects").is_dir() && c.join("refs").is_dir() && c.join("HEAD").is_file();
                if !looks_like_git {
                    return Err(err(
                        n,
                        format!(
                            "`{}` is not a git common dir (no objects/, refs/ and HEAD); give the repository's .git directory",
                            c.display()
                        ),
                    ));
                }
                expand(&c, GIT_COMMON_DIRS, Want::Dir, key, n, &guard, &mut rules, &mut skipped).map_err(|r| err(n, r))?;
            }
            "cargo_home" => {
                expand(&c, CARGO_HOME_DIRS, Want::Dir, key, n, &guard, &mut rules, &mut skipped).map_err(|r| err(n, r))?;
                expand(&c, CARGO_HOME_FILES, Want::File, key, n, &guard, &mut rules, &mut skipped).map_err(|r| err(n, r))?;
            }
            _ => rules.push(Rule { path: c, fd: Arc::new(fd), access, dir: want == Want::Dir, class, key, line: n }),
        }
        if rules.len() > MAX_RULES {
            return Err(err(n, format!("more than {MAX_RULES} rules (the baseline and every expansion count)")));
        }
    }

    Ok(Policy {
        file: file.to_path_buf(),
        hash: short_hash(text.as_bytes()),
        mode,
        required,
        lane: lane.map(|(l, _)| l),
        subagents,
        tools,
        named_tools: named.into_iter().map(|(t, _)| t).collect(),
        audit_log: match audit_log {
            Some((v, _)) => v,
            None => Some(PathBuf::from(DEFAULT_AUDIT_LOG)),
        },
        contact,
        rules,
        skipped,
    })
}

#[allow(clippy::too_many_arguments)]
fn expand(
    root: &Path,
    children: &[&str],
    want: Want,
    key: &'static str,
    line: usize,
    guard: &Guard,
    rules: &mut Vec<Rule>,
    skipped: &mut Vec<Skipped>,
) -> Result<(), String> {
    let access = if want == Want::Dir { WRITE_RIGHTS } else { FILE_WRITE_RIGHTS };
    for child in children {
        let raw = root.join(child);
        match resolve(&raw, want, guard)? {
            Some((c, fd)) => rules.push(Rule {
                path: c,
                fd: Arc::new(fd),
                access,
                dir: want == Want::Dir,
                class: RuleClass::Infra,
                key,
                line,
            }),
            None => skipped.push(Skipped { line, key, path: raw }),
        }
    }
    Ok(())
}

/// At most `cap` bytes of `r`; `None` if it holds more.
fn read_capped(r: impl io::Read, cap: u64) -> io::Result<Option<Vec<u8>>> {
    use std::io::Read as _;
    let mut buf = Vec::new();
    r.take(cap + 1).read_to_end(&mut buf)?;
    Ok((buf.len() as u64 <= cap).then_some(buf))
}

/// Read a policy file. `Ok(None)` = it doesn't exist. It is opened once
/// (no symlink, non-blocking, close-on-exec), checked through that handle
/// (a regular file with exactly one name, at most [`MAX_POLICY_BYTES`]) and
/// read through it, at most one byte past the cap.
pub fn read_policy_file(file: &Path) -> Result<Option<String>, PolicyError> {
    read_policy_file_meta(file).map(|r| r.map(|(text, _)| text))
}

/// [`read_policy_file`], also returning the metadata of the handle it read
/// (the compaction carry copies the mode).
#[cfg(unix)]
pub fn read_policy_file_meta(file: &Path) -> Result<Option<(String, std::fs::Metadata)>, PolicyError> {
    use std::os::unix::fs::OpenOptionsExt;
    let fail = |reason: String| PolicyError::new(file, 0, reason, DEFAULT_CONTACT);
    let f = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(file)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            return Err(fail("is a symlink (a policy file must be a regular file)".into()));
        }
        Err(e) => return Err(fail(format!("cannot read: {e}"))),
    };
    let meta = f.metadata().map_err(|e| fail(format!("cannot stat: {e}")))?;
    if !meta.is_file() {
        return Err(fail("not a regular file".into()));
    }
    if meta.nlink() != 1 {
        return Err(fail(format!("has {} hard links (a policy file must have exactly one name)", meta.nlink())));
    }
    if meta.len() > MAX_POLICY_BYTES {
        return Err(fail(format!("larger than {MAX_POLICY_BYTES} bytes")));
    }
    let bytes = read_capped(f, MAX_POLICY_BYTES)
        .map_err(|e| fail(format!("cannot read: {e}")))?
        .ok_or_else(|| fail(format!("larger than {MAX_POLICY_BYTES} bytes")))?;
    String::from_utf8(bytes).map(|text| Some((text, meta))).map_err(|e| {
        let mut f = fail("not valid UTF-8".into());
        f.confined = scan_confined(&String::from_utf8_lossy(e.as_bytes()));
        f
    })
}

/// [`read_policy_file`] off Unix (Windows): there is no `O_NOFOLLOW` open,
/// no link count to check and no sandbox to enforce a policy with, so an
/// existing policy file is refused unread and its session's spawns are
/// refused (fail closed). No file is still no policy.
#[cfg(not(unix))]
pub fn read_policy_file_meta(file: &Path) -> Result<Option<(String, std::fs::Metadata)>, PolicyError> {
    let fail = |reason: String| PolicyError::new(file, 0, reason, DEFAULT_CONTACT);
    match std::fs::symlink_metadata(file) {
        Ok(_) => Err(fail("cannot read: sandbox policy files need a Unix host".into())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(fail(format!("cannot read: {e}"))),
    }
}

/// Read, parse and validate `file`. `Ok(None)` = it doesn't exist.
pub fn load(file: &Path, env: &PolicyEnv) -> Result<Option<Policy>, PolicyError> {
    match read_policy_file(file)? {
        None => Ok(None),
        Some(text) => parse(&text, file, env).map(Some),
    }
}

/// What a spawn finds for its session.
#[derive(Debug)]
pub enum Lookup {
    /// No policy file for this session: no sandbox, nothing changes.
    None,
    /// `<base>/sandbox/OFF` exists: every sandbox is off.
    GlobalOff,
    /// A valid policy (`mode = enforce` or `mode = off`).
    Found(Policy),
    /// The file exists but can't be read, parsed or validated: refuse.
    Invalid(PolicyError),
}

/// Process-wide state of the kill switch, so its transitions are logged once
/// each, not once per spawn.
pub struct KillSwitch(Mutex<u8>);

const SWITCH_UNKNOWN: u8 = 0;
const SWITCH_ABSENT: u8 = 1;
const SWITCH_PRESENT: u8 = 2;

impl KillSwitch {
    pub const fn new() -> Self {
        KillSwitch(Mutex::new(SWITCH_UNKNOWN))
    }

    /// Sample the switch with `sample`, record the observation and log a
    /// transition with `log`, all under one lock: each change of the observed
    /// state is logged exactly once, in the order the observations were made.
    /// Returns whether the switch is present.
    pub fn observe_with(&self, sample: impl FnOnce() -> bool, log: &mut dyn FnMut(&'static str)) -> bool {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let present = sample();
        let new = if present { SWITCH_PRESENT } else { SWITCH_ABSENT };
        let line = match (*state, new) {
            (SWITCH_PRESENT, SWITCH_ABSENT) => Some(LINE_GLOBAL_ON),
            (SWITCH_UNKNOWN | SWITCH_ABSENT, SWITCH_PRESENT) => Some(LINE_GLOBAL_OFF),
            _ => None,
        };
        *state = new;
        if let Some(line) = line {
            log(line);
        }
        present
    }

    /// Record one observation; returns the log line for a transition.
    pub fn observe(&self, present: bool) -> Option<&'static str> {
        let mut line = None;
        self.observe_with(|| present, &mut |l| line = Some(l));
        line
    }
}

impl Default for KillSwitch {
    fn default() -> Self {
        Self::new()
    }
}

/// The daemon's kill-switch state.
pub static GLOBAL_KILL_SWITCH: KillSwitch = KillSwitch::new();

#[cfg(test)]
thread_local! {
    /// Tests: run on this thread as each [`lookup_with`] starts, before the
    /// kill switch or the policy file is read. A spawn must already hold its
    /// binding's lock there (`spawn::prepare`), so a test can observe it.
    pub(crate) static ON_LOOKUP: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
}

/// The kill switch is "present" if anything exists at its path; an error
/// other than not-found counts as absent (the sandbox stays as configured).
pub fn kill_switch_present(env: &PolicyEnv) -> bool {
    std::fs::symlink_metadata(env.kill_switch()).is_ok()
}

/// Look up `session_id`'s policy, kill switch first. A kill-switch
/// transition goes to `log`, from under the switch's lock.
pub fn lookup_with(env: &PolicyEnv, session_id: &str, switch: &KillSwitch, log: &mut dyn FnMut(&'static str)) -> Lookup {
    #[cfg(test)]
    ON_LOOKUP.with(|hook| {
        if let Some(f) = hook.borrow_mut().as_mut() {
            f();
        }
    });
    if switch.observe_with(|| kill_switch_present(env), log) {
        return Lookup::GlobalOff;
    }
    let Some(file) = env.policy_path(session_id) else {
        return Lookup::None;
    };
    match load(&file, env) {
        Ok(None) => Lookup::None,
        Ok(Some(p)) => Lookup::Found(p),
        Err(e) => Lookup::Invalid(e),
    }
}

/// [`lookup_with`] on the daemon's kill switch, logging its transitions.
pub fn lookup(env: &PolicyEnv, session_id: &str) -> Lookup {
    lookup_with(env, session_id, &GLOBAL_KILL_SWITCH, &mut |line| super::log_line(line))
}

/// Rights as names; `all-write` for the full handled set.
pub fn rights_names(access: u64) -> String {
    if access == WRITE_RIGHTS {
        return "all-write".into();
    }
    const NAMES: &[(u64, &str)] = &[
        (ACCESS_FS_WRITE_FILE, "write_file"),
        (ACCESS_FS_TRUNCATE, "truncate"),
        (ACCESS_FS_REMOVE_FILE, "remove_file"),
        (ACCESS_FS_REMOVE_DIR, "remove_dir"),
        (ACCESS_FS_MAKE_REG, "make_reg"),
        (ACCESS_FS_MAKE_DIR, "make_dir"),
        (ACCESS_FS_MAKE_SYM, "make_sym"),
        (ACCESS_FS_MAKE_SOCK, "make_sock"),
        (ACCESS_FS_MAKE_FIFO, "make_fifo"),
        (ACCESS_FS_MAKE_CHAR, "make_char"),
        (ACCESS_FS_MAKE_BLOCK, "make_block"),
        (ACCESS_FS_REFER, "refer"),
    ];
    let names: Vec<&str> = NAMES.iter().filter(|(bit, _)| access & bit != 0).map(|(_, n)| *n).collect();
    if names.is_empty() {
        "none".into()
    } else {
        names.join(",")
    }
}

/// `sandbox.policy …`: logged when a session's policy is first seen or
/// changes. `abi` goes through [`usable_abi`].
pub fn policy_line(session_id: &str, policy: &Policy, abi: Result<u32, Unavailable>) -> String {
    let abi = match usable_abi(abi) {
        Ok(v) => v.to_string(),
        Err(u) => format!("unavailable:{}", u.reason()),
    };
    let line = format!(
        "sandbox.policy session={} mode={} subagents={} abi={} rules={} skipped={} hash={}",
        session_id,
        policy.mode.as_str(),
        policy.subagents.as_str(),
        abi,
        policy.rules.len(),
        policy.skipped.len(),
        policy.hash
    );
    // Only a confined policy adds fields: every other file's line is
    // byte-identical to what it was before the key existed.
    if policy.tools == Tools::Confined {
        return format!("{line} tools=confined named={}", policy.named_tools.len());
    }
    line
}

/// `sandbox.skip …`, one per missing path.
pub fn skip_lines(session_id: &str, policy: &Policy) -> Vec<String> {
    policy
        .skipped
        .iter()
        .map(|s| format!("sandbox.skip session={} line={} key={} path={} why=missing", session_id, s.line, s.key, s.path.display()))
        .collect()
}

/// `sandbox.invalid …`, logged when a spawn is refused for a bad file.
pub fn invalid_line(session_id: &str, e: &PolicyError) -> String {
    format!("sandbox.invalid session={} file={} line={} reason={}", session_id, e.file.display(), e.line, e.reason)
}

/// `sandbox.off session=… reason=explicit`, logged once for a `mode = off` session.
pub fn explicit_off_line(session_id: &str, policy: &Policy) -> String {
    format!("sandbox.off session={} reason=explicit subagents={}", session_id, policy.subagents.as_str())
}

/// The `synaps sandbox check <file>` report: `(valid, text)`. Runs the same
/// read + parse + validation as a spawn, then prints every rule as it would
/// be applied. Writes nothing. `abi` goes through [`usable_abi`], so an ABI
/// below the minimum reports as unavailable.
pub fn check_report(file: &Path, env: &PolicyEnv, abi: Result<u32, Unavailable>) -> (bool, String) {
    use std::fmt::Write as _;
    let mut out = String::new();
    // The kill switch comes before any policy, so the report says so first.
    let kill = env.kill_switch();
    let kill_present = kill_switch_present(env);
    if kill_present {
        let _ = writeln!(
            out,
            "kill switch {}: PRESENT: every sandbox is off, so no policy file (this one included) is read or applied until it is removed",
            kill.display()
        );
    }
    let policy = match read_policy_file(file) {
        Ok(None) => {
            let _ = writeln!(
                out,
                "sandbox policy {}: INVALID\n{}: no such file\nagent sees: nothing (a session without a policy file runs unsandboxed)",
                file.display(),
                file.display()
            );
            return (false, out);
        }
        Ok(Some(text)) => parse(&text, file, env),
        Err(e) => Err(e),
    };
    let policy = match policy {
        Ok(p) => p,
        Err(e) => {
            let _ = writeln!(out, "sandbox policy {}: INVALID\n{e}\nagent sees: {}", file.display(), e.refusal());
            return (false, out);
        }
    };
    let _ = writeln!(out, "sandbox policy {}: OK (hash {})", file.display(), policy.hash);
    let _ = writeln!(
        out,
        "mode = {}, required = {}, subagents = {}, lane = {}, audit_log = {}",
        policy.mode.as_str(),
        policy.required,
        policy.subagents.as_str(),
        policy.lane.as_deref().unwrap_or("-"),
        policy.audit_log.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "off".into())
    );
    if policy.tools == Tools::Confined {
        let names = if policy.named_tools.is_empty() { "-".to_string() } else { policy.named_tools.join(", ") };
        let _ = writeln!(
            out,
            "tools = confined: write, edit and multi_edit write only in or below the [all-write] dirs below; worker tools are refused; other tools: the built-in list and {} named ({names})",
            policy.named_tools.len()
        );
    }
    let _ = writeln!(out, "contact: {}", policy.contact);
    match usable_abi(abi) {
        Ok(v) => {
            let _ = writeln!(out, "landlock: abi {v}");
        }
        Err(u @ Unavailable::Error(_)) if policy.mode == Mode::Enforce => {
            let _ = writeln!(
                out,
                "landlock: {u}: every spawn of this session is REFUSED (an error, unlike an unsupported kernel, never runs unsandboxed)"
            );
        }
        Err(u) if u.always_refuses() && policy.mode == Mode::Enforce => {
            let _ = writeln!(
                out,
                "landlock: unavailable ({u}): on this OS every spawn of this session is REFUSED (an enforcing policy never runs unsandboxed here, whatever `required` says)"
            );
        }
        Err(u) if policy.required && policy.mode == Mode::Enforce => {
            let _ = writeln!(out, "landlock: unavailable ({u}): on this host every spawn of this session is REFUSED (required = true)");
        }
        Err(u) => {
            let _ = writeln!(out, "landlock: unavailable ({u}): on this host this session runs unsandboxed");
        }
    }
    if !kill_present {
        let _ = writeln!(out, "kill switch {}: absent", kill.display());
    }
    let _ = writeln!(out, "rules ({}):", policy.rules.len());
    for r in &policy.rules {
        let origin = if r.line == 0 { r.key.to_string() } else { format!("{} (line {})", r.key, r.line) };
        let kind = if r.dir {
            "dir"
        } else if r.key == "device" {
            "dev"
        } else {
            "file"
        };
        let _ = writeln!(
            out,
            "  {:<8} {:<4} {:<22} {} [{}]",
            r.class.as_str(),
            kind,
            origin,
            r.path.display(),
            rights_names(r.access)
        );
    }
    let _ = writeln!(out, "skipped, missing ({}):", policy.skipped.len());
    for s in &policy.skipped {
        let origin = if s.line == 0 { s.key.to_string() } else { format!("{} (line {})", s.key, s.line) };
        let _ = writeln!(out, "  {:<22} {}", origin, s.path.display());
    }
    (true, out)
}

/// Tests: a parent for fixture trees that is not under a baseline directory
/// (validation refuses a base dir there): `$TMPDIR` unless it is under one,
/// else the test binary's own directory (inside the cargo target dir).
#[cfg(test)]
pub(crate) fn test_scratch_parent() -> PathBuf {
    let tmp = std::fs::canonicalize(std::env::temp_dir()).unwrap_or_else(|_| std::env::temp_dir());
    let under_baseline = BASELINE.iter().filter(|(_, _, dir)| *dir).any(|(p, _, _)| {
        let root = std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p));
        tmp.starts_with(root)
    });
    if !under_baseline {
        return tmp;
    }
    let exe = std::env::current_exe().expect("test binary path");
    exe.parent().expect("test binary dir").to_path_buf()
}
