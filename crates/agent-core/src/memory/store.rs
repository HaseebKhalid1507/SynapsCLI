use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryRecord {
    /// Caller-supplied namespace, e.g. "session-notes" or "<plugin-id>".
    pub namespace: String,
    /// Unix epoch milliseconds.
    pub timestamp_ms: u64,
    /// Free-form text content.
    pub content: String,
    /// Optional tag list (e.g. ["@user", "preference"]).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Optional structured metadata. Validated as JSON on read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
    /// Stable record id (`mem-<uuid>`), assigned by [`store_record_in`].
    /// Absent on pre-T32 legacy lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Canonical project-scope key. Absent on legacy lines — project-less
    /// records never match a project scope (fail closed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Who/what produced this record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<MemoryProvenance>,
    /// Sensitivity class (spec §9.5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensitivity: Option<MemorySensitivity>,
    /// Retention class (consumed by the unified retention sweep).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<MemoryRetention>,
}

/// Provenance of a memory record (spec §9.5).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryProvenance {
    /// Producer: "user", "model", "tool:<name>", "extension:<id>", …
    pub source: String,
    /// Session the record was produced in, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// Sensitivity class of a memory record (spec §9.5). Enforcement at the
/// model-visibility boundary lives in the memory tools: `Secret` bodies are
/// never returned to model context.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemorySensitivity {
    Normal,
    Sensitive,
    Secret,
}

/// Retention class of a memory record (consumed by spec §9.7 retention).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryRetention {
    /// Kept until explicitly forgotten or swept by disk budget.
    Standard,
    /// Expires after the given number of days.
    MaxAgeDays(u32),
}

#[derive(Debug, Clone, Default)]
pub struct MemoryQuery {
    /// Optional substring match against `content` (case-insensitive).
    pub content_contains: Option<String>,
    /// Optional tag prefix; record matches if ANY of its tags has this prefix.
    pub tag_prefix: Option<String>,
    /// Inclusive lower bound on `timestamp_ms`.
    pub since_ms: Option<u64>,
    /// Inclusive upper bound on `timestamp_ms`.
    pub until_ms: Option<u64>,
    /// Maximum number of records to return (most recent first). Default: 50.
    pub limit: Option<usize>,
}

/// Default per-query record cap.
pub const DEFAULT_LIMIT: usize = 50;

/// Maximum content length per record (UTF-8 byte length).
pub const MAX_CONTENT_BYTES: usize = 16 * 1024;

#[derive(Debug)]
pub enum MemoryError {
    InvalidNamespace(String),
    ContentTooLarge {
        len: usize,
        max: usize,
    },
    Io(String),
    Serde(String),
    /// The record id does not exist IN THIS PROJECT SCOPE (also returned
    /// for other projects' ids — fail closed without existence disclosure).
    NotFound(String),
    /// The project root could not be canonicalized.
    InvalidProjectRoot(String),
}

impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoryError::InvalidNamespace(s) => write!(f, "invalid namespace: {s:?}"),
            MemoryError::ContentTooLarge { len, max } => {
                write!(f, "content too large: {len} bytes (max {max})")
            }
            MemoryError::Io(s) => write!(f, "memory io error: {s}"),
            MemoryError::Serde(s) => write!(f, "memory serde error: {s}"),
            MemoryError::NotFound(id) => {
                write!(f, "memory record not found in this project scope: {id}")
            }
            MemoryError::InvalidProjectRoot(s) => write!(f, "invalid project root: {s}"),
        }
    }
}

impl std::error::Error for MemoryError {}

impl From<std::io::Error> for MemoryError {
    fn from(e: std::io::Error) -> Self {
        MemoryError::Io(e.to_string())
    }
}

impl From<serde_json::Error> for MemoryError {
    fn from(e: serde_json::Error) -> Self {
        MemoryError::Serde(e.to_string())
    }
}

/// Current Unix epoch in milliseconds.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn validate_namespace(ns: &str) -> Result<(), MemoryError> {
    if ns.is_empty()
        || ns.len() > 64
        || ns.contains('/')
        || ns.contains('\\')
        || ns.contains("..")
        || ns.chars().any(|c| c.is_whitespace())
    {
        return Err(MemoryError::InvalidNamespace(ns.to_string()));
    }
    Ok(())
}

/// Path to the memory directory under base. Caller creates dirs lazily.
pub fn memory_dir() -> PathBuf {
    crate::config::base_dir().join("memory")
}

/// Memory directory under an explicit base (test/tool seam).
pub fn memory_dir_in(base: &Path) -> PathBuf {
    base.join("memory")
}

fn namespace_path(dir: &Path, ns: &str) -> PathBuf {
    dir.join(format!("{ns}.jsonl"))
}

/// Append one record. Validates namespace, content size. One line is one
/// `write` to a file opened for append, and the file is synced before this
/// returns (see `append_jsonl_line`).
pub fn append(record: &MemoryRecord) -> Result<(), MemoryError> {
    append_to(&crate::config::base_dir(), record)
}

pub fn append_to(base: &Path, record: &MemoryRecord) -> Result<(), MemoryError> {
    append_record(base, record, true)
}

/// [`append_to`]; with `sync_now` false the sync is left to the caller (an
/// import, which syncs each file once at the end of its run).
fn append_record(base: &Path, record: &MemoryRecord, sync_now: bool) -> Result<(), MemoryError> {
    validate_namespace(&record.namespace)?;
    if record.content.len() > MAX_CONTENT_BYTES {
        return Err(MemoryError::ContentTooLarge {
            len: record.content.len(),
            max: MAX_CONTENT_BYTES,
        });
    }
    let dir = memory_dir_in(base);
    crate::core::private_fs::ensure_private_dir(&dir).map_err(std::io::Error::from)?;
    let path = namespace_path(&dir, &record.namespace);
    let line = serde_json::to_string(record)?;
    append_jsonl_line(&path, &line, sync_now)
}

/// Append one JSONL line and make it durable before returning: the line goes
/// to the file whole ([`write_whole_line`]), the file is synced, and when
/// this append found no file or an empty one, its directory is synced too,
/// because nothing else puts the new file's name on disk.
///
/// With `sync_now` false nothing is synced here: the caller is an import
/// run, which syncs each file and its directory once, at its end
/// ([`ImportRun::sync`]).
///
/// An error means the save is NOT acknowledged. It does not mean nothing was
/// written: after a short write or a failed sync, part or all of the line can
/// be in the file.
///
/// Torn-tail healing: if a previous writer died mid-line (the file does not
/// end in a newline), a leading newline isolates the torn fragment on its
/// own line. Lenient readers skip the fragment and every later record stays
/// parseable. Nothing is ever cut off the file, so healing cannot drop a
/// line, and a line that only lacked its newline becomes a good line.
///
/// There is no lock. Appenders in other processes are kept apart by the
/// append mode alone, which is enough for whole lines: each is one `write`.
/// What it does not cover is the look at the last byte, which is a separate
/// step from the write. If another writer's line is torn between the two (it
/// died, or its write was short), this line lands on that fragment and is
/// lost with it. Looking and finding a line that another writer has only
/// half written costs nothing: the newline it adds makes an empty line.
fn append_jsonl_line(path: &Path, line: &str, sync_now: bool) -> Result<(), MemoryError> {
    let (had_bytes, needs_heal) = match fs::metadata(path) {
        Ok(meta) if meta.len() > 0 => {
            use std::io::{Read, Seek, SeekFrom};
            let mut f = fs::File::open(path)?;
            f.seek(SeekFrom::End(-1))?;
            let mut last = [0u8; 1];
            f.read_exact(&mut last)?;
            (true, last[0] != b'\n')
        }
        _ => (false, false),
    };
    let mut f = crate::core::private_fs::open_private_append(path).map_err(std::io::Error::from)?;
    write_whole_line(&mut f, line, needs_heal)?;
    if sync_now {
        sync_file(&f)?;
        if !had_bytes {
            if let Some(dir) = path.parent() {
                sync_dir(dir)?;
            }
        }
    }
    Ok(())
}

/// How many `write` calls one line may take before the append gives up.
const LINE_WRITE_ATTEMPTS: usize = 3;

/// Write `line` and its newline with ONE `write` call, so that no other
/// appender's bytes can land inside a line that was acknowledged.
///
/// `write_all` is not used: after a short write it sends the REST of the
/// line in a second call, and whatever another appender wrote in between
/// would sit in the middle of this line. Here a short write is abandoned
/// where it is (bytes are never taken back out of the file) and the whole
/// line is written again, led by a newline that ends the fragment, which a
/// reader then skips like any torn line. If the fragment was the whole
/// record short of its newline, the record is in the file twice, same id and
/// same bytes: a duplicate rather than a line this call cannot vouch for.
/// A regular file takes a short write when the disk or a quota is full, so
/// the retry normally fails and the caller gets that error.
fn write_whole_line<W: Write>(
    file: &mut W,
    line: &str,
    mut lead_newline: bool,
) -> std::io::Result<()> {
    let mut attempts = 0;
    while attempts < LINE_WRITE_ATTEMPTS {
        let mut out = Vec::with_capacity(line.len() + 2);
        if lead_newline {
            out.push(b'\n');
        }
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
        step("write");
        match file.write(&out) {
            Ok(n) if n == out.len() => return Ok(()),
            Ok(n) => {
                attempts += 1;
                lead_newline |= n > 0;
            }
            // Nothing was written; the same call again.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::WriteZero,
        "a line could not be appended with one write",
    ))
}

/// Sync a notes file: its bytes and its length.
fn sync_file(file: &fs::File) -> std::io::Result<()> {
    step("sync file");
    file.sync_all()
}

/// Sync a directory, so that the name of a file just created in it is on
/// disk.
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    step("sync dir");
    #[cfg(unix)]
    {
        fs::File::open(dir)?.sync_all()
    }
    // Windows cannot open a directory to flush it, and NTFS journals the new
    // name itself: the choice `session_journal::sync_sessions_dir` makes.
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    /// The write and sync calls this thread's appends made, in order.
    static IO_STEPS: std::cell::RefCell<Vec<&'static str>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Notes a write or a sync for the tests that check their order.
#[cfg(test)]
fn step(name: &'static str) {
    IO_STEPS.with(|steps| steps.borrow_mut().push(name));
}

#[cfg(not(test))]
fn step(_name: &'static str) {}

/// Query records in a namespace, applying filters, returning most-recent-first up to limit.
pub fn query(namespace: &str, q: &MemoryQuery) -> Result<Vec<MemoryRecord>, MemoryError> {
    query_in(&crate::config::base_dir(), namespace, q)
}

pub fn query_in(
    base: &Path,
    namespace: &str,
    q: &MemoryQuery,
) -> Result<Vec<MemoryRecord>, MemoryError> {
    validate_namespace(namespace)?;
    let path = namespace_path(&memory_dir_in(base), namespace);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let f = fs::File::open(&path)?;
    let reader = BufReader::new(f);
    let needle = q.content_contains.as_ref().map(|s| s.to_lowercase());
    let mut out: Vec<MemoryRecord> = Vec::new();
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        let rec: MemoryRecord = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if let Some(since) = q.since_ms {
            if rec.timestamp_ms < since {
                continue;
            }
        }
        if let Some(until) = q.until_ms {
            if rec.timestamp_ms > until {
                continue;
            }
        }
        if let Some(needle) = &needle {
            if !rec.content.to_lowercase().contains(needle) {
                continue;
            }
        }
        if let Some(prefix) = &q.tag_prefix {
            if !rec.tags.iter().any(|t| t.starts_with(prefix)) {
                continue;
            }
        }
        out.push(rec);
    }
    out.sort_by_key(|b| std::cmp::Reverse(b.timestamp_ms));
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT);
    out.truncate(limit);
    Ok(out)
}

/// List existing namespaces under the memory dir.
pub fn list_namespaces() -> Result<Vec<String>, MemoryError> {
    list_namespaces_in(&crate::config::base_dir())
}

pub(crate) fn list_namespaces_in(base: &Path) -> Result<Vec<String>, MemoryError> {
    let dir = memory_dir_in(base);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                out.push(stem.to_string());
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Build a record with `now_ms()` timestamp.
pub fn new_record(
    namespace: impl Into<String>,
    content: impl Into<String>,
    tags: Vec<String>,
    meta: Option<serde_json::Value>,
) -> MemoryRecord {
    MemoryRecord {
        namespace: namespace.into(),
        timestamp_ms: now_ms(),
        content: content.into(),
        tags,
        meta,
        id: None,
        project: None,
        provenance: None,
        sensitivity: None,
        retention: None,
    }
}

// ─── Task 32: project-scoped progressive memory (spec §9.5) ─────────────────

/// Host-resolved project identity: the canonical workspace root, digested
/// into a stable scope key. The HOST constructs this from trusted execution
/// context — it is never accepted solely from model-authored arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectScope {
    key: String,
    root: PathBuf,
}

impl ProjectScope {
    /// Resolve a scope from a workspace root. The path is canonicalized so
    /// every spelling of the same directory yields one project identity.
    pub fn for_root(root: &Path) -> Result<Self, MemoryError> {
        let canonical = root
            .canonicalize()
            .map_err(|e| MemoryError::InvalidProjectRoot(format!("{}: {e}", root.display())))?;
        Ok(Self::for_canonical_root(canonical))
    }

    // Internal seam for historical, already-verified canonical paths retained
    // in the private repository marker. Unlike for_root this does not re-resolve
    // an old path that may have moved or been reused by another repository.
    pub(super) fn for_canonical_root(canonical: PathBuf) -> Self {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(canonical.as_os_str().as_encoded_bytes());
        let digest = hasher.finalize();
        let mut key = String::with_capacity(17);
        key.push('p');
        for byte in digest.iter().take(8) {
            use std::fmt::Write;
            let _ = write!(key, "{byte:02x}");
        }
        Self {
            key,
            root: canonical,
        }
    }

    /// Construct an explicit user-wide scope at a canonical host base path.
    /// This is the ONLY constructor accepting the reserved all-zero key.
    /// Never derive this scope from model-authored project arguments.
    pub fn user_scope(root: &Path) -> Result<Self, MemoryError> {
        let mut scope = Self::for_root(root)?;
        scope.require_directory()?;
        scope.key = "p0000000000000000".into();
        Ok(scope)
    }

    /// Restore a host-verified key (e.g. an operator migration manifest).
    /// Syntax validation is NOT authorization: callers must establish the key's
    /// trusted provenance, never accept a model-supplied key as a scope grant.
    /// Accepts exactly `p[0-9a-f]{16}`, excluding the user-wide reserved key.
    pub fn from_key(root: &Path, key: &str) -> Result<Self, MemoryError> {
        Self::validate_repository_key(key)?;
        let mut scope = Self::for_root(root)?;
        scope.require_directory()?;
        scope.key = key.to_owned();
        Ok(scope)
    }

    fn require_directory(&self) -> Result<(), MemoryError> {
        if !self.root.is_dir() {
            return Err(MemoryError::InvalidProjectRoot(
                "host scope root must be a directory".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn validate_repository_key(key: &str) -> Result<(), MemoryError> {
        if key.len() != 17
            || !key.starts_with('p')
            || !key.as_bytes()[1..]
                .iter()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
            || key == "p0000000000000000"
        {
            return Err(MemoryError::InvalidProjectRoot(
                "invalid or reserved repository scope key".into(),
            ));
        }
        Ok(())
    }

    /// Stable scope key (`p<16 hex>`).
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Discover the stable project root for a starting directory (CP-13
    /// fix1 I2): the `SYNAPS_PROJECT_ROOT` env override wins; otherwise a
    /// bounded upward walk finds the nearest directory containing a `.git`
    /// entry (DIRECTORY or FILE — worktree-safe, no process spawning, no
    /// gitdir parsing) or a `.synaps-project` marker; with no marker the
    /// start directory itself is the root.
    pub fn discover(start: &Path) -> Result<Self, MemoryError> {
        let override_root = std::env::var_os("SYNAPS_PROJECT_ROOT").map(std::path::PathBuf::from);
        Self::discover_with_override(start, override_root.as_deref())
    }

    /// [`Self::discover`] with an explicit override (env-free seam).
    pub fn discover_with_override(
        start: &Path,
        override_root: Option<&Path>,
    ) -> Result<Self, MemoryError> {
        if let Some(root) = override_root {
            return Self::for_root(root);
        }
        const MARKERS: [&str; 2] = [".git", ".synaps-project"];
        const MAX_ASCENT: usize = 64;
        let canonical_start = start
            .canonicalize()
            .map_err(|e| MemoryError::InvalidProjectRoot(format!("{}: {e}", start.display())))?;
        let mut dir = canonical_start.as_path();
        for _ in 0..MAX_ASCENT {
            if MARKERS
                .iter()
                .any(|marker| dir.join(marker).symlink_metadata().is_ok())
            {
                return Self::for_root(dir);
            }
            match dir.parent() {
                Some(parent) => dir = parent,
                None => break,
            }
        }
        Self::for_root(&canonical_start)
    }

    /// The canonical workspace root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The JSONL namespace holding this project's records.
    pub fn namespace(&self) -> String {
        project_namespace(&self.key)
    }
}

/// The JSONL namespace of the project with this key.
fn project_namespace(key: &str) -> String {
    format!("project-{key}")
}

/// Input for [`store_record_in`] — explicit provenance, sensitivity, and
/// retention (spec §9.5: nothing is stored with implicit metadata).
#[derive(Debug, Clone)]
pub struct NewMemoryRecord {
    pub content: String,
    pub tags: Vec<String>,
    pub provenance: MemoryProvenance,
    pub sensitivity: MemorySensitivity,
    pub retention: MemoryRetention,
}

/// Bounded search result: descriptor + snippet, NEVER the full body (full
/// content requires an exact [`fetch_exact_in`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryDescriptor {
    pub id: String,
    pub project: String,
    pub timestamp_ms: u64,
    pub tags: Vec<String>,
    /// UTF-8-boundary-bounded head of the content.
    pub snippet: String,
    /// Whether the snippet was cut.
    pub truncated: bool,
    /// Full content length in bytes (size disclosure only).
    pub content_bytes: usize,
    pub sensitivity: MemorySensitivity,
    pub retention: MemoryRetention,
}

/// Hard cap on search results per call.
pub const MAX_SEARCH_LIMIT: usize = 25;
/// Default search result count.
pub const DEFAULT_SEARCH_LIMIT: usize = 8;
/// Hard cap on snippet bytes per descriptor.
pub const MAX_SNIPPET_BYTES: usize = 400;
/// Default snippet bytes per descriptor.
pub const DEFAULT_SNIPPET_BYTES: usize = 160;

/// Project-scoped search query. Limits clamp to the hard caps.
#[derive(Debug, Clone, Default)]
pub struct ProjectMemoryQuery {
    pub content_contains: Option<String>,
    pub tag_prefix: Option<String>,
    pub since_ms: Option<u64>,
    pub until_ms: Option<u64>,
    pub limit: Option<usize>,
    pub snippet_bytes: Option<usize>,
}

/// One parsed line of a project namespace file: a record or a tombstone.
/// Tombstones are append-only (`{"tombstone":"<id>","timestamp_ms":…}`);
/// physical deletion belongs to the retention sweep. Legacy readers skip
/// tombstone lines as malformed records — forward compatible.
#[derive(Debug, Serialize, Deserialize)]
struct TombstoneLine {
    tombstone: String,
    timestamp_ms: u64,
}

/// Store one record in a project scope: assigns a stable `mem-<uuid>` id,
/// binds the scope key, and appends to the project namespace.
pub fn store_record_in(
    base: &Path,
    scope: &ProjectScope,
    new: NewMemoryRecord,
) -> Result<MemoryRecord, MemoryError> {
    let record = MemoryRecord {
        namespace: scope.namespace(),
        timestamp_ms: now_ms(),
        content: new.content,
        tags: new.tags,
        meta: None,
        id: Some(format!("mem-{}", uuid::Uuid::new_v4().simple())),
        project: Some(scope.key().to_string()),
        provenance: Some(new.provenance),
        sensitivity: Some(new.sensitivity),
        retention: Some(new.retention),
    };
    append_to(base, &record)?;
    Ok(record)
}

/// What the lines of one project's file amount to: the records that carry
/// the project's key and an id, in file order, and every id a tombstone line
/// names. A record whose id is tombstoned is not live, wherever in the file
/// the tombstone stands.
struct ProjectLines {
    records: Vec<MemoryRecord>,
    tombstoned: std::collections::HashSet<String>,
}

/// Scan `<base>/memory/project-<key>.jsonl` by a bare project key (a
/// [`ProjectScope`] needs the project's folder to exist; an import has only
/// the key). No file is an empty project. Records whose inner `project` key
/// is not `key` are excluded — fail closed even if a foreign file was copied
/// into the namespace.
///
/// A line that does not parse is skipped, and so is one that is not UTF-8 (a
/// torn tail can end inside a character). Any other read error is returned:
/// a scan that carried on past it would answer from part of the file.
fn scan_project_file(base: &Path, key: &str) -> Result<ProjectLines, MemoryError> {
    let mut lines = ProjectLines {
        records: Vec::new(),
        tombstoned: std::collections::HashSet::new(),
    };
    let path = namespace_path(&memory_dir_in(base), &project_namespace(key));
    let f = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(lines),
        Err(e) => return Err(e.into()),
    };
    for line in BufReader::new(f).lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => continue,
            Err(e) => return Err(e.into()),
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(tomb) = serde_json::from_str::<TombstoneLine>(&line) {
            lines.tombstoned.insert(tomb.tombstone);
            continue;
        }
        let rec: MemoryRecord = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => continue,
        };
        // Fail closed: only records carrying THIS project's key participate.
        if rec.project.as_deref() != Some(key) || rec.id.is_none() {
            continue;
        }
        lines.records.push(rec);
    }
    Ok(lines)
}

/// Load the LIVE (non-tombstoned, scope-matching) records of a project.
fn load_live_project_records(
    base: &Path,
    scope: &ProjectScope,
) -> Result<Vec<MemoryRecord>, MemoryError> {
    let ProjectLines {
        mut records,
        tombstoned,
    } = scan_project_file(base, scope.key())?;
    records.retain(|r| r.id.as_ref().is_some_and(|id| !tombstoned.contains(id)));
    Ok(records)
}

/// Project-scoped bounded search: descriptors + snippets, most recent
/// first, hard-capped result count and snippet bytes.
pub fn search_project_in(
    base: &Path,
    scope: &ProjectScope,
    q: &ProjectMemoryQuery,
) -> Result<Vec<MemoryDescriptor>, MemoryError> {
    let needle = q.content_contains.as_ref().map(|s| s.to_lowercase());
    let snippet_bytes = q
        .snippet_bytes
        .unwrap_or(DEFAULT_SNIPPET_BYTES)
        .min(MAX_SNIPPET_BYTES);
    let limit = q
        .limit
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .min(MAX_SEARCH_LIMIT);

    let mut records = load_live_project_records(base, scope)?;
    records.retain(|rec| {
        let secret = rec.sensitivity == Some(MemorySensitivity::Secret);
        if let Some(since) = q.since_ms {
            if rec.timestamp_ms < since {
                return false;
            }
        }
        if let Some(until) = q.until_ms {
            if rec.timestamp_ms > until {
                return false;
            }
        }
        if let Some(needle) = &needle {
            // Secret bodies never participate in content matching — a
            // substring probe must not confirm secret content.
            if secret || !rec.content.to_lowercase().contains(needle) {
                return false;
            }
        }
        if let Some(prefix) = &q.tag_prefix {
            if !rec.tags.iter().any(|t| t.starts_with(prefix)) {
                return false;
            }
        }
        true
    });
    records.sort_by_key(|r| std::cmp::Reverse(r.timestamp_ms));
    records.truncate(limit);

    Ok(records
        .into_iter()
        .map(|rec| {
            let sensitivity = rec.sensitivity.unwrap_or(MemorySensitivity::Normal);
            // Secret descriptors carry NO body-derived snippet text (spec
            // §9.5: secret bodies are visible locally only).
            let (snippet, truncated) = if sensitivity == MemorySensitivity::Secret {
                (String::new(), false)
            } else {
                let bounded = crate::text::BoundedText::new(&rec.content, snippet_bytes);
                (bounded.text, bounded.truncated)
            };
            MemoryDescriptor {
                id: rec.id.clone().unwrap_or_default(),
                project: scope.key().to_string(),
                timestamp_ms: rec.timestamp_ms,
                tags: rec.tags,
                snippet,
                truncated,
                content_bytes: rec.content.len(),
                sensitivity,
                retention: rec.retention.unwrap_or(MemoryRetention::Standard),
            }
        })
        .collect())
}

/// Fetch exact record ids within a project scope. EVERY id must resolve in
/// this scope or the whole call fails closed with [`MemoryError::NotFound`]
/// — other projects' ids are indistinguishable from unknown ids.
pub fn fetch_exact_in(
    base: &Path,
    scope: &ProjectScope,
    ids: &[&str],
) -> Result<Vec<MemoryRecord>, MemoryError> {
    let live = load_live_project_records(base, scope)?;
    let mut out = Vec::with_capacity(ids.len());
    for wanted in ids {
        match live.iter().find(|r| r.id.as_deref() == Some(*wanted)) {
            Some(rec) => out.push(rec.clone()),
            None => return Err(MemoryError::NotFound((*wanted).to_string())),
        }
    }
    Ok(out)
}

/// Tombstone one record in a project scope (append-only forget). Fails
/// closed with [`MemoryError::NotFound`] for unknown, already-tombstoned,
/// or other-project ids.
pub fn forget_in(base: &Path, scope: &ProjectScope, id: &str) -> Result<(), MemoryError> {
    let live = load_live_project_records(base, scope)?;
    if !live.iter().any(|r| r.id.as_deref() == Some(id)) {
        return Err(MemoryError::NotFound(id.to_string()));
    }
    let tombstone = TombstoneLine {
        tombstone: id.to_string(),
        timestamp_ms: now_ms(),
    };
    append_tombstone(base, &scope.namespace(), &tombstone, true)
}

/// Append a tombstone line to a project namespace's file.
fn append_tombstone(
    base: &Path,
    namespace: &str,
    tombstone: &TombstoneLine,
    sync_now: bool,
) -> Result<(), MemoryError> {
    let dir = memory_dir_in(base);
    crate::core::private_fs::ensure_private_dir(&dir).map_err(std::io::Error::from)?;
    let path = namespace_path(&dir, namespace);
    let line = serde_json::to_string(tombstone)?;
    append_jsonl_line(&path, &line, sync_now)
}

/// Path of a project namespace's JSONL store file (index-module seam).
pub(crate) fn namespace_file(base: &Path, namespace: &str) -> PathBuf {
    namespace_path(&memory_dir_in(base), namespace)
}

// ─── Import from another store ──────────────────────────────────────────────

/// The reserved key of user-wide notes, which have no project file to go to.
const USER_WIDE_KEY: &str = "p0000000000000000";

/// The fields a note line may have: those of [`MemoryRecord`].
const RECORD_FIELDS: [&str; 10] = [
    "namespace",
    "timestamp_ms",
    "content",
    "tags",
    "meta",
    "id",
    "project",
    "provenance",
    "sensitivity",
    "retention",
];

/// Where [`import_line_in`] files a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportTarget<'a> {
    /// Under the project the line names in its own `project`. Nothing in the
    /// line changes. A tombstone line names no project and is refused.
    AsRecorded,
    /// Under this project key (`p` + 16 hex, not the user-wide key). A
    /// note's `project` and `namespace` are set to it.
    Project(&'a str),
}

/// What [`import_line_in`] did with one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportOutcome {
    /// Appended: a note, or a tombstone, whose id the file did not have.
    Stored,
    /// Nothing written: the note is live in the file as given, or (for a
    /// tombstone line) the id is already tombstoned there.
    AlreadyPresent,
    /// Nothing written: a note whose id is tombstoned in the file. An import
    /// does not undo a forget.
    KeptForgotten,
    /// A tombstone appended for an id that was live.
    Forgotten,
    /// Nothing written. The reason names fields and positions and never
    /// quotes a value of the line.
    Refused(String),
}

impl ImportOutcome {
    /// The outcome's name in an import report.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Stored => "stored",
            Self::AlreadyPresent => "already_present",
            Self::KeptForgotten => "kept_forgotten",
            Self::Forgotten => "forgotten",
            Self::Refused(_) => "refused",
        }
    }
}

/// [`import_line_in`]'s answer for one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedLine {
    pub outcome: ImportOutcome,
    /// The line's id, if the line was read far enough to have one.
    pub id: Option<String>,
    /// The key of the project whose file the line was judged against, if it
    /// got that far.
    pub project: Option<String>,
    /// Whether the line is a tombstone line rather than a note.
    pub tombstone: bool,
}

/// What one import run knows: each notes file it has touched, read from disk
/// once and kept in step with what the run has stored since, in a dry run
/// too. That is what makes the lines of one run be judged against each
/// other, and a dry run report what the real run then does.
#[derive(Debug, Default)]
pub struct ImportRun {
    files: std::collections::HashMap<PathBuf, ImportFile>,
}

#[derive(Debug)]
struct ImportFile {
    /// The file's records by id, by the reader's rule. A file can hold an id
    /// more than once.
    notes: std::collections::HashMap<String, Vec<MemoryRecord>>,
    tombstoned: std::collections::HashSet<String>,
    /// Whether this run appended to the file.
    appended: bool,
}

impl ImportRun {
    pub fn new() -> Self {
        Self::default()
    }

    /// The file of project `key`, read the first time the run needs it.
    fn file(&mut self, base: &Path, key: &str) -> Result<&mut ImportFile, MemoryError> {
        use std::collections::hash_map::Entry;
        match self
            .files
            .entry(namespace_file(base, &project_namespace(key)))
        {
            Entry::Occupied(known) => Ok(known.into_mut()),
            Entry::Vacant(vacant) => {
                let lines = scan_project_file(base, key)?;
                let mut notes: std::collections::HashMap<String, Vec<MemoryRecord>> =
                    std::collections::HashMap::new();
                for record in lines.records {
                    if let Some(id) = record.id.clone() {
                        notes.entry(id).or_default().push(record);
                    }
                }
                Ok(vacant.insert(ImportFile {
                    notes,
                    tombstoned: lines.tombstoned,
                    appended: false,
                }))
            }
        }
    }

    /// Sync every file this run appended to, then the directory of each: the
    /// end of a run. An import's appends are not synced one by one, so
    /// nothing the run stored is durable until this returns `Ok`; call it
    /// when the run stops early too. Returns the files synced, sorted. A dry
    /// run has none.
    pub fn sync(&self) -> Result<Vec<PathBuf>, MemoryError> {
        let mut synced: Vec<PathBuf> = self
            .files
            .iter()
            .filter(|(_, file)| file.appended)
            .map(|(path, _)| path.clone())
            .collect();
        synced.sort();
        let mut dirs: std::collections::BTreeSet<&Path> = std::collections::BTreeSet::new();
        for path in &synced {
            // Opened for append, not for reading: Windows flushes only a
            // handle that may write.
            sync_file(&fs::OpenOptions::new().append(true).open(path)?)?;
            dirs.extend(path.parent());
        }
        for dir in dirs {
            sync_dir(dir)?;
        }
        Ok(synced)
    }
}

/// Import one JSONL line of another store's notes into the plain-file store
/// under `base`: a note (a [`MemoryRecord`] line) or a tombstone line
/// (`{"tombstone":"<id>","timestamp_ms":…}`).
///
/// A note must be a JSON object with no field but [`MemoryRecord`]'s, an
/// `id` of 1 to 128 bytes without whitespace or control characters, a
/// `project` that is a project key, `namespace == "project-" + project`, a
/// `timestamp_ms`, and at most [`MAX_CONTENT_BYTES`] of content. It is
/// stored as given, except that [`ImportTarget::Project`] sets `project` and
/// `namespace`. No field is dropped and no time is made up: a line that
/// could only be stored without something it says is refused.
///
/// Against the target file: a note whose id is tombstoned there is
/// [`ImportOutcome::KeptForgotten`]; whose id is live there with the same
/// content, tags, time, provenance, sensitivity, retention and meta is
/// [`ImportOutcome::AlreadyPresent`], and with anything different is
/// refused; otherwise it is appended. A tombstone line is appended unless
/// the id is already tombstoned: [`ImportOutcome::Forgotten`] if the id was
/// live, [`ImportOutcome::Stored`] if the file did not have it (a later
/// import of that note then stays forgotten).
///
/// With `dry_run` nothing is written, created or synced, and the outcomes
/// are the ones the real run gives from the same files. `run` is the state
/// of one run; finish the run with [`ImportRun::sync`].
///
/// `Err` is for what is not the line's fault and should stop the run: an
/// I/O error, or a target that is not a project key (the user-wide key is
/// not one).
pub fn import_line_in(
    base: &Path,
    line: &str,
    target: ImportTarget<'_>,
    dry_run: bool,
    run: &mut ImportRun,
) -> Result<ImportedLine, MemoryError> {
    if let ImportTarget::Project(key) = target {
        ProjectScope::validate_repository_key(key)?;
    }
    let mut about = ImportedLine {
        outcome: ImportOutcome::Refused(String::new()),
        id: None,
        project: None,
        tombstone: false,
    };
    let outcome = match parse_import_line(line) {
        Err(reason) => ImportOutcome::Refused(reason),
        Ok(ImportLine::Tombstone(tombstone)) => {
            about.tombstone = true;
            import_tombstone(base, tombstone, target, dry_run, run, &mut about)?
        }
        Ok(ImportLine::Note(note)) => import_note(base, *note, target, dry_run, run, &mut about)?,
    };
    about.outcome = outcome;
    Ok(about)
}

fn refused(reason: &str) -> Result<ImportOutcome, MemoryError> {
    Ok(ImportOutcome::Refused(reason.to_string()))
}

fn import_note(
    base: &Path,
    mut note: MemoryRecord,
    target: ImportTarget<'_>,
    dry_run: bool,
    run: &mut ImportRun,
    about: &mut ImportedLine,
) -> Result<ImportOutcome, MemoryError> {
    let Some(id) = note.id.clone() else {
        return refused("missing field id");
    };
    if !valid_import_id(&id) {
        return refused("invalid id");
    }
    about.id = Some(id.clone());
    let Some(recorded) = note.project.clone() else {
        return refused("missing field project");
    };
    if recorded != USER_WIDE_KEY && ProjectScope::validate_repository_key(&recorded).is_err() {
        return refused("invalid project");
    }
    if note.namespace != project_namespace(&recorded) {
        return refused("namespace is not project-<project>");
    }
    if note.content.len() > MAX_CONTENT_BYTES {
        return refused(&format!("content is over {MAX_CONTENT_BYTES} bytes"));
    }
    let key = match target {
        ImportTarget::Project(key) => {
            note.project = Some(key.to_string());
            note.namespace = project_namespace(key);
            key.to_string()
        }
        ImportTarget::AsRecorded if recorded == USER_WIDE_KEY => {
            return refused("user-wide note; pass --cwd to file it under a project");
        }
        ImportTarget::AsRecorded => recorded,
    };
    about.project = Some(key.clone());
    let file = run.file(base, &key)?;
    if file.tombstoned.contains(&id) {
        return Ok(ImportOutcome::KeptForgotten);
    }
    if let Some(held) = file.notes.get(&id) {
        if held.iter().any(|other| same_note(other, &note)) {
            return Ok(ImportOutcome::AlreadyPresent);
        }
        return refused("id already holds a different note");
    }
    if !dry_run {
        append_record(base, &note, false)?;
        file.appended = true;
    }
    file.notes.insert(id, vec![note]);
    Ok(ImportOutcome::Stored)
}

fn import_tombstone(
    base: &Path,
    tombstone: TombstoneLine,
    target: ImportTarget<'_>,
    dry_run: bool,
    run: &mut ImportRun,
    about: &mut ImportedLine,
) -> Result<ImportOutcome, MemoryError> {
    if !valid_import_id(&tombstone.tombstone) {
        return refused("invalid id");
    }
    about.id = Some(tombstone.tombstone.clone());
    let ImportTarget::Project(key) = target else {
        return refused("a tombstone line names no project; pass --cwd or --project-key");
    };
    about.project = Some(key.to_string());
    let file = run.file(base, key)?;
    if file.tombstoned.contains(&tombstone.tombstone) {
        return Ok(ImportOutcome::AlreadyPresent);
    }
    if !dry_run {
        append_tombstone(base, &project_namespace(key), &tombstone, false)?;
        file.appended = true;
    }
    let was_live = file.notes.contains_key(&tombstone.tombstone);
    file.tombstoned.insert(tombstone.tombstone);
    Ok(if was_live {
        ImportOutcome::Forgotten
    } else {
        ImportOutcome::Stored
    })
}

/// 1 to 128 bytes, no whitespace, no control characters.
fn valid_import_id(id: &str) -> bool {
    (1..=128).contains(&id.len()) && !id.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Whether two notes under one id are the same note: every field but the id
/// and the project they are filed under.
fn same_note(a: &MemoryRecord, b: &MemoryRecord) -> bool {
    a.content == b.content
        && a.tags == b.tags
        && a.timestamp_ms == b.timestamp_ms
        && a.provenance == b.provenance
        && a.sensitivity == b.sensitivity
        && a.retention == b.retention
        && a.meta == b.meta
}

enum ImportLine {
    Note(Box<MemoryRecord>),
    Tombstone(TombstoneLine),
}

/// Read one line as a note or a tombstone line, or say why it is neither.
fn parse_import_line(line: &str) -> Result<ImportLine, String> {
    let object: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(line).map_err(|e| match e.classify() {
            serde_json::error::Category::Data => "not a JSON object".to_string(),
            _ => format!("not JSON at column {}", e.column()),
        })?;
    if object.contains_key("tombstone") {
        if let Some(extra) = object
            .keys()
            .find(|name| !["tombstone", "timestamp_ms"].contains(&name.as_str()))
        {
            return Err(unknown_field(extra));
        }
        return serde_json::from_str(line)
            .map(ImportLine::Tombstone)
            .map_err(|e| unread("tombstone line", &e));
    }
    if let Some(extra) = object
        .keys()
        .find(|name| !RECORD_FIELDS.contains(&name.as_str()))
    {
        return Err(unknown_field(extra));
    }
    // From the text, not from `object`: the record's own reader refuses a
    // field given twice, where a JSON map keeps the last and says nothing.
    let note: MemoryRecord = serde_json::from_str(line).map_err(|e| unread("note", &e))?;
    // Nothing the line says may be missing from what would be written. This
    // is what catches a field inside `provenance` that the record has no
    // place for. A `null` says nothing and may go.
    let kept = serde_json::to_value(&note)
        .map(|kept| without_nulls(&kept))
        .map_err(|_| "not a note".to_string())?;
    for (name, value) in &object {
        if !value.is_null() && kept.get(name) != Some(&without_nulls(value)) {
            return Err(format!("{name} holds something a note cannot keep"));
        }
    }
    Ok(ImportLine::Note(Box::new(note)))
}

fn unknown_field(name: &str) -> String {
    let shown: String = name.chars().take(64).collect();
    format!("unknown field {}", shown.escape_debug())
}

/// Why serde did not read a line, without quoting the line. Its "missing
/// field" and "duplicate field" messages name a field of the record and
/// nothing else. Any other message can quote a value, so only the place is
/// passed on.
fn unread(what: &str, e: &serde_json::Error) -> String {
    let message = e.to_string();
    for kind in ["missing field", "duplicate field"] {
        let name = message
            .strip_prefix(kind)
            .and_then(|rest| rest.split('`').nth(1));
        if let Some(name) = name {
            return format!("{kind} {name}");
        }
    }
    format!("not a {what}: wrong value at column {}", e.column())
}

/// `value` without the object members that are `null`, at any depth.
fn without_nulls(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Object(members) => Value::Object(
            members
                .iter()
                .filter(|(_, member)| !member.is_null())
                .map(|(name, member)| (name.clone(), without_nulls(member)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_nulls).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn rec(ns: &str, ts: u64, content: &str, tags: Vec<&str>) -> MemoryRecord {
        MemoryRecord {
            namespace: ns.to_string(),
            timestamp_ms: ts,
            content: content.to_string(),
            tags: tags.into_iter().map(String::from).collect(),
            meta: None,
            id: None,
            project: None,
            provenance: None,
            sensitivity: None,
            retention: None,
        }
    }

    #[test]
    fn append_then_query_returns_record() {
        let tmp = TempDir::new().unwrap();
        let r = rec("ns", 100, "hello world", vec!["@user"]);
        append_to(tmp.path(), &r).unwrap();
        let got = query_in(tmp.path(), "ns", &MemoryQuery::default()).unwrap();
        assert_eq!(got, vec![r]);
    }

    #[test]
    fn query_filters_by_content_contains() {
        let tmp = TempDir::new().unwrap();
        append_to(tmp.path(), &rec("ns", 100, "Hello World", vec![])).unwrap();
        append_to(tmp.path(), &rec("ns", 200, "goodbye", vec![])).unwrap();
        let q = MemoryQuery {
            content_contains: Some("hello".to_string()),
            ..Default::default()
        };
        let got = query_in(tmp.path(), "ns", &q).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].content, "Hello World");
    }

    #[test]
    fn query_filters_by_tag_prefix() {
        let tmp = TempDir::new().unwrap();
        append_to(
            tmp.path(),
            &rec("ns", 100, "x", vec!["@user", "preference"]),
        )
        .unwrap();
        let got = query_in(
            tmp.path(),
            "ns",
            &MemoryQuery {
                tag_prefix: Some("@u".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(got.len(), 1);
        let got = query_in(
            tmp.path(),
            "ns",
            &MemoryQuery {
                tag_prefix: Some("@x".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn query_filters_by_time_range() {
        let tmp = TempDir::new().unwrap();
        append_to(tmp.path(), &rec("ns", 100, "a", vec![])).unwrap();
        append_to(tmp.path(), &rec("ns", 200, "b", vec![])).unwrap();
        append_to(tmp.path(), &rec("ns", 300, "c", vec![])).unwrap();
        let got = query_in(
            tmp.path(),
            "ns",
            &MemoryQuery {
                since_ms: Some(150),
                until_ms: Some(250),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].timestamp_ms, 200);
    }

    #[test]
    fn query_returns_most_recent_first() {
        let tmp = TempDir::new().unwrap();
        append_to(tmp.path(), &rec("ns", 100, "a", vec![])).unwrap();
        append_to(tmp.path(), &rec("ns", 300, "c", vec![])).unwrap();
        append_to(tmp.path(), &rec("ns", 200, "b", vec![])).unwrap();
        let got = query_in(tmp.path(), "ns", &MemoryQuery::default()).unwrap();
        let ts: Vec<u64> = got.iter().map(|r| r.timestamp_ms).collect();
        assert_eq!(ts, vec![300, 200, 100]);
    }

    #[test]
    fn query_respects_limit() {
        let tmp = TempDir::new().unwrap();
        for i in 1..=5 {
            append_to(tmp.path(), &rec("ns", i * 100, "x", vec![])).unwrap();
        }
        let got = query_in(
            tmp.path(),
            "ns",
            &MemoryQuery {
                limit: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].timestamp_ms, 500);
        assert_eq!(got[1].timestamp_ms, 400);
    }

    #[test]
    fn query_skips_malformed_lines() {
        let tmp = TempDir::new().unwrap();
        let dir = memory_dir_in(tmp.path());
        fs::create_dir_all(&dir).unwrap();
        let path = namespace_path(&dir, "ns");
        let v1 = serde_json::to_string(&rec("ns", 100, "a", vec![])).unwrap();
        let v2 = serde_json::to_string(&rec("ns", 200, "b", vec![])).unwrap();
        let body = format!("invalid json\n{v1}\nnot json either\n{v2}\n");
        fs::write(&path, body).unwrap();
        let got = query_in(tmp.path(), "ns", &MemoryQuery::default()).unwrap();
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn append_rejects_oversized_content() {
        let tmp = TempDir::new().unwrap();
        let big = "x".repeat(MAX_CONTENT_BYTES + 1);
        let r = rec("ns", 1, &big, vec![]);
        match append_to(tmp.path(), &r) {
            Err(MemoryError::ContentTooLarge { .. }) => {}
            other => panic!("expected ContentTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn append_rejects_invalid_namespace() {
        let tmp = TempDir::new().unwrap();
        let cases = [
            "",
            "a/b",
            "a\\b",
            "..",
            "a..b",
            "has space",
            "tab\there",
            &"x".repeat(65),
        ];
        for ns in cases {
            let r = rec(ns, 1, "x", vec![]);
            match append_to(tmp.path(), &r) {
                Err(MemoryError::InvalidNamespace(_)) => {}
                other => panic!("expected InvalidNamespace for {ns:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn list_namespaces_returns_existing_files() {
        let tmp = TempDir::new().unwrap();
        append_to(tmp.path(), &rec("alpha", 1, "x", vec![])).unwrap();
        append_to(tmp.path(), &rec("beta", 1, "x", vec![])).unwrap();
        let got = list_namespaces_in(tmp.path()).unwrap();
        assert_eq!(got, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn list_namespaces_on_missing_dir_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let got = list_namespaces_in(tmp.path()).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn query_on_missing_namespace_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let got = query_in(tmp.path(), "nope", &MemoryQuery::default()).unwrap();
        assert!(got.is_empty());
    }

    // ─── Durable, whole-line appends ────────────────────────────────────────

    fn project_scope(tmp: &TempDir) -> ProjectScope {
        let root = tmp.path().join("project");
        fs::create_dir_all(&root).unwrap();
        ProjectScope::for_root(&root).unwrap()
    }

    fn note(content: &str) -> NewMemoryRecord {
        NewMemoryRecord {
            content: content.to_string(),
            tags: vec!["t".into()],
            provenance: MemoryProvenance {
                source: "model".into(),
                session: None,
            },
            sensitivity: MemorySensitivity::Normal,
            retention: MemoryRetention::Standard,
        }
    }

    /// The write and sync calls this thread's appends made since the last
    /// call, in order.
    fn take_steps() -> Vec<&'static str> {
        IO_STEPS.with(|steps| std::mem::take(&mut *steps.borrow_mut()))
    }

    /// What this shows: the ORDER of the calls a save makes. One write of the
    /// line, then a sync of the file, then (only when the file was new) a
    /// sync of its directory, all before the call returns, and the same for
    /// the line a forget appends. What it cannot show: that the bytes are on
    /// the platter. A test cannot cut the power, and whether a sync reaches
    /// the medium is up to the system and the disk.
    #[test]
    fn a_save_and_a_forget_are_synced_before_they_return() {
        let tmp = TempDir::new().unwrap();
        let scope = project_scope(&tmp);
        take_steps();

        let first = store_record_in(tmp.path(), &scope, note("first")).unwrap();
        assert_eq!(
            take_steps(),
            ["write", "sync file", "sync dir"],
            "the save that creates the file syncs the file, then its directory"
        );

        store_record_in(tmp.path(), &scope, note("second")).unwrap();
        assert_eq!(
            take_steps(),
            ["write", "sync file"],
            "a later save syncs the file; its name is already on disk"
        );

        forget_in(tmp.path(), &scope, first.id.as_deref().unwrap()).unwrap();
        assert_eq!(
            take_steps(),
            ["write", "sync file"],
            "a tombstone line is synced like a note"
        );

        // The plain namespace path (the extension `memory.append` call).
        append_to(tmp.path(), &rec("ns", 1, "x", vec![])).unwrap();
        assert_eq!(take_steps(), ["write", "sync file", "sync dir"]);

        // A file that is there but empty has no line whose save synced the
        // directory, so the first line does.
        fs::write(memory_dir_in(tmp.path()).join("empty.jsonl"), "").unwrap();
        append_to(tmp.path(), &rec("empty", 1, "x", vec![])).unwrap();
        assert_eq!(take_steps(), ["write", "sync file", "sync dir"]);
    }

    /// A file opened for append that another writer shares, as far as a
    /// writer can tell: every `write` lands at the end, whole. It takes at
    /// most the scripted number of bytes per call, and after its first short
    /// write the other writer appends `interloper`.
    struct SharedAppendFile {
        bytes: Vec<u8>,
        accept: std::collections::VecDeque<usize>,
        interloper: Option<String>,
        calls: Vec<Vec<u8>>,
    }

    impl SharedAppendFile {
        fn taking(accept: &[usize], interloper: Option<String>) -> Self {
            Self {
                bytes: Vec::new(),
                accept: accept.iter().copied().collect(),
                interloper,
                calls: Vec::new(),
            }
        }

        fn lines(&self) -> Vec<&str> {
            std::str::from_utf8(&self.bytes).unwrap().lines().collect()
        }
    }

    impl Write for SharedAppendFile {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let n = self.accept.pop_front().unwrap_or(buf.len()).min(buf.len());
            self.calls.push(buf.to_vec());
            self.bytes.extend_from_slice(&buf[..n]);
            if n < buf.len() {
                if let Some(other) = self.interloper.take() {
                    self.bytes.extend_from_slice(other.as_bytes());
                }
            }
            Ok(n)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    const LINE: &str = r#"{"namespace":"ns","timestamp_ms":1,"content":"mine","tags":[]}"#;
    const OTHER: &str = r#"{"namespace":"ns","timestamp_ms":2,"content":"theirs","tags":[]}"#;

    #[test]
    fn a_line_is_one_write_call() {
        let mut file = SharedAppendFile::taking(&[], None);
        write_whole_line(&mut file, LINE, false).unwrap();
        assert_eq!(file.calls, [format!("{LINE}\n").into_bytes()]);

        // The newline that ends a torn tail goes in the same call.
        let mut file = SharedAppendFile::taking(&[], None);
        write_whole_line(&mut file, LINE, true).unwrap();
        assert_eq!(file.calls, [format!("\n{LINE}\n").into_bytes()]);
    }

    /// What this shows: after a short write the rest of the line is never
    /// sent as a second call, so a line another writer appends in between
    /// cannot end up inside this one. The writer is a script, not a file: a
    /// real file gives a short write only when the disk or a quota is full,
    /// which a hermetic test cannot arrange. What it cannot show: how a given
    /// file system behaves at that point.
    #[test]
    fn a_short_write_is_abandoned_and_the_whole_line_written_again() {
        // The other writer saw the torn tail and led with a newline.
        let mut file = SharedAppendFile::taking(&[20], Some(format!("\n{OTHER}\n")));
        write_whole_line(&mut file, LINE, false).unwrap();
        assert_eq!(file.lines(), [&LINE[..20], OTHER, "", LINE]);
        assert_eq!(
            file.calls,
            [format!("{LINE}\n").into_bytes(), format!("\n{LINE}\n").into_bytes()],
            "each call carries the whole line; none carries a remainder"
        );

        // The other writer looked before the fragment was there and did not
        // lead with a newline. Its line is lost on the fragment, which no
        // writer can prevent without a lock; this writer's line is whole.
        let mut file = SharedAppendFile::taking(&[20], Some(format!("{OTHER}\n")));
        write_whole_line(&mut file, LINE, false).unwrap();
        let merged = format!("{}{OTHER}", &LINE[..20]);
        assert_eq!(file.lines(), [merged.as_str(), "", LINE]);

        // Every reader keeps the good lines and skips the fragment.
        for line in file.lines() {
            let parsed = serde_json::from_str::<MemoryRecord>(line);
            assert_eq!(parsed.is_ok(), line == LINE, "{line:?}");
        }
    }

    #[test]
    fn a_line_that_cannot_go_in_one_write_is_an_error_not_a_split_line() {
        // Every call is short: the save fails; it is never reported as done.
        let mut file = SharedAppendFile::taking(&[5, 5, 5, 5, 5, 5], None);
        let err = write_whole_line(&mut file, LINE, false).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::WriteZero);
        assert_eq!(file.calls.len(), LINE_WRITE_ATTEMPTS);
        assert!(
            file.lines().iter().all(|line| line.len() <= 5),
            "only fragments, each on a line of its own: {:?}",
            file.lines()
        );

        // A writer that takes nothing fails the same way.
        let mut file = SharedAppendFile::taking(&[0, 0, 0, 0], None);
        assert!(write_whole_line(&mut file, LINE, false).is_err());
        assert!(file.bytes.is_empty());
    }

    /// What this shows: on the machine and file system the test runs on,
    /// writers that each open the file for append and start together do not
    /// tear or lose each other's lines, for lines from a few bytes to several
    /// pages. Threads with a handle each stand in for processes: the kernel
    /// orders appends by file, not by who holds the handle. What it cannot
    /// show: the same for every file system (a network one may not keep
    /// append writes whole), or what happens when a write is short; the
    /// scripted-writer tests above cover that.
    #[test]
    fn concurrent_appends_keep_every_line_whole_and_lose_none() {
        const WRITERS: usize = 8;
        const ROUNDS: usize = 24;
        let tmp = TempDir::new().unwrap();
        let scope = project_scope(&tmp);
        let start = std::sync::Barrier::new(WRITERS);
        let mut forgotten: Vec<String> = Vec::new();
        let mut failures: Vec<String> = Vec::new();
        std::thread::scope(|threads| {
            let handles: Vec<_> = (0..WRITERS)
                .map(|writer| {
                    let (base, scope, start) = (tmp.path(), &scope, &start);
                    threads.spawn(move || {
                        let (mut ids, mut errors) = (Vec::new(), Vec::new());
                        for round in 0..ROUNDS {
                            // 9 bytes to about 15 KiB, so some lines span
                            // several pages.
                            let pad = "x".repeat((round % 6) * 3_000);
                            let content = format!("w{writer}-r{round}-{pad}");
                            // Every writer comes to every round and none
                            // panics in between: one that stopped early would
                            // leave the others waiting here for good, and a
                            // torn line would hang the test, not fail it.
                            start.wait();
                            let stored = match store_record_in(base, scope, note(&content)) {
                                Ok(stored) => stored,
                                Err(e) => {
                                    errors.push(format!("save w{writer}-r{round}: {e}"));
                                    continue;
                                }
                            };
                            if round % 8 == 7 {
                                let id = stored.id.unwrap_or_default();
                                match forget_in(base, scope, &id) {
                                    Ok(()) => ids.push(id),
                                    Err(e) => errors.push(format!("forget w{writer}-r{round}: {e}")),
                                }
                            }
                        }
                        (ids, errors)
                    })
                })
                .collect();
            for handle in handles {
                let (ids, errors) = handle.join().unwrap();
                forgotten.extend(ids);
                failures.extend(errors);
            }
        });
        assert!(
            failures.is_empty(),
            "a save or a forget failed among concurrent writers: {failures:?}"
        );

        let raw = fs::read_to_string(namespace_file(tmp.path(), &scope.namespace())).unwrap();
        assert!(raw.ends_with('\n'));
        let mut contents = std::collections::HashSet::new();
        let mut tombstones = std::collections::HashSet::new();
        for line in raw.lines().filter(|line| !line.is_empty()) {
            if let Ok(tomb) = serde_json::from_str::<TombstoneLine>(line) {
                assert!(tombstones.insert(tomb.tombstone), "a tombstone twice");
                continue;
            }
            let record: MemoryRecord = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("a line does not parse ({e}): {:.80}", line));
            let head: String = record.content.chars().take(12).collect();
            assert!(contents.insert(record.content), "a note twice: {head}");
        }
        assert_eq!(contents.len(), WRITERS * ROUNDS, "no note is lost");
        for writer in 0..WRITERS {
            for round in 0..ROUNDS {
                let pad = "x".repeat((round % 6) * 3_000);
                assert!(contents.contains(&format!("w{writer}-r{round}-{pad}")));
            }
        }
        assert_eq!(forgotten.len(), WRITERS * ROUNDS / 8);
        assert_eq!(
            tombstones,
            forgotten.iter().cloned().collect(),
            "no tombstone is lost"
        );
        let live = load_live_project_records(tmp.path(), &scope).unwrap();
        assert_eq!(live.len(), WRITERS * ROUNDS - forgotten.len());
    }

    // ─── The torn last line ─────────────────────────────────────────────────

    /// What this shows: what the next append does with a last line that a
    /// dead writer left unfinished, cut inside a multi-byte character. No
    /// byte before the tear changes, the fragment ends up alone on a line
    /// every reader skips, and the new note is whole. The tear is made by
    /// writing the file, not by killing a process mid-write.
    #[test]
    fn a_torn_last_line_is_fenced_off_and_no_good_line_is_touched() {
        let tmp = TempDir::new().unwrap();
        let scope = project_scope(&tmp);
        let kept = store_record_in(tmp.path(), &scope, note("kept")).unwrap();
        let path = namespace_file(tmp.path(), &scope.namespace());
        let before = fs::read(&path).unwrap();

        let mut whole = MemoryRecord {
            content: "caf\u{e9}".into(),
            ..kept.clone()
        };
        whole.id = Some("mem-torn".into());
        let torn_line = serde_json::to_string(&whole).unwrap();
        let cut = torn_line.find('\u{e9}').unwrap() + 1;
        let fragment = &torn_line.as_bytes()[..cut];
        assert!(std::str::from_utf8(fragment).is_err(), "cut inside a character");
        let mut torn = before.clone();
        torn.extend_from_slice(fragment);
        fs::write(&path, &torn).unwrap();

        let after = store_record_in(tmp.path(), &scope, note("after the tear")).unwrap();

        let bytes = fs::read(&path).unwrap();
        assert!(bytes.starts_with(&torn), "nothing before the append changed");
        let mut expected = torn.clone();
        expected.push(b'\n');
        expected.extend_from_slice(serde_json::to_string(&after).unwrap().as_bytes());
        expected.push(b'\n');
        assert_eq!(bytes, expected, "fragment, newline, the new line, newline");

        let live = load_live_project_records(tmp.path(), &scope).unwrap();
        assert_eq!(live, [kept.clone(), after.clone()]);
        let ids = [kept.id.as_deref().unwrap(), after.id.as_deref().unwrap()];
        assert_eq!(fetch_exact_in(tmp.path(), &scope, &ids).unwrap().len(), 2);
        assert_eq!(
            query_in(tmp.path(), &scope.namespace(), &MemoryQuery::default())
                .unwrap()
                .len(),
            2
        );
    }

    /// A last line that is complete but for its newline is a good line. The
    /// next append ends it; it is neither dropped nor run into the new one.
    #[test]
    fn a_last_line_missing_only_its_newline_is_kept() {
        let tmp = TempDir::new().unwrap();
        let scope = project_scope(&tmp);
        let first = store_record_in(tmp.path(), &scope, note("first")).unwrap();
        let path = namespace_file(tmp.path(), &scope.namespace());
        let mut bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.pop(), Some(b'\n'));
        fs::write(&path, bytes).unwrap();

        let second = store_record_in(tmp.path(), &scope, note("second")).unwrap();

        let live = load_live_project_records(tmp.path(), &scope).unwrap();
        assert_eq!(live, [first, second]);
        let raw = fs::read_to_string(&path).unwrap();
        assert_eq!(raw.lines().count(), 2, "no empty line, nothing merged");
    }

    // ─── Import from another store (cut-spec A3) ───────────────────────────────────

    /// A note line as another store exports it, with all ten fields.
    fn exported(id: &str, project: &str, timestamp_ms: u64, content: &str) -> String {
        serde_json::json!({
            "namespace": format!("project-{project}"),
            "timestamp_ms": timestamp_ms,
            "content": content,
            "tags": ["imported", "@user"],
            "meta": {"_synaps_repository": {"source_worktree_root": "/elsewhere"}, "blank": null},
            "id": id,
            "project": project,
            "provenance": {"source": "model", "session": "sess-old"},
            "sensitivity": "sensitive",
            "retention": {"max_age_days": 30},
        })
        .to_string()
    }

    fn import(
        base: &Path,
        line: &str,
        target: ImportTarget<'_>,
        run: &mut ImportRun,
    ) -> ImportedLine {
        import_line_in(base, line, target, false, run).unwrap()
    }

    fn refusal(imported: &ImportedLine) -> &str {
        match &imported.outcome {
            ImportOutcome::Refused(reason) => reason,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// cut-spec T-A10.
    #[test]
    fn import_notes_t_a10() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let scope = project_scope(&tmp);
        let key = scope.key().to_string();
        let here = ImportTarget::Project(&key);
        let path = namespace_file(base, &scope.namespace());

        // A file with 3 notes; import 8 more.
        let old: Vec<String> = (0..3)
            .map(|i| {
                let stored = store_record_in(base, &scope, note(&format!("old note {i}")));
                stored.unwrap().id.unwrap()
            })
            .collect();
        let lines: Vec<String> = (0..8u64)
            .map(|i| exported(&format!("mem-new-{i}"), &key, 1_000 + i, &format!("new note {i}")))
            .collect();
        let mut run = ImportRun::new();
        for (i, line) in lines.iter().enumerate() {
            assert_eq!(
                import(base, line, ImportTarget::AsRecorded, &mut run),
                ImportedLine {
                    outcome: ImportOutcome::Stored,
                    id: Some(format!("mem-new-{i}")),
                    project: Some(key.clone()),
                    tombstone: false,
                }
            );
        }
        assert_eq!(run.sync().unwrap(), std::slice::from_ref(&path));

        // 11 live; search and fetch find old and new.
        assert_eq!(load_live_project_records(base, &scope).unwrap().len(), 11);
        let search = |needle: &str| {
            let query = ProjectMemoryQuery {
                content_contains: Some(needle.into()),
                limit: Some(MAX_SEARCH_LIMIT),
                ..Default::default()
            };
            search_project_in(base, &scope, &query).unwrap().len()
        };
        assert_eq!(
            (search("note"), search("old note"), search("new note")),
            (11, 3, 8)
        );
        let mut ids = old.clone();
        ids.extend((0..8).map(|i| format!("mem-new-{i}")));
        let wanted: Vec<&str> = ids.iter().map(String::as_str).collect();
        let fetched = fetch_exact_in(base, &scope, &wanted).unwrap();
        assert_eq!(fetched.len(), 11);
        // An imported note is its line: every field, and the time it came with.
        for (line, stored) in lines.iter().zip(&fetched[3..]) {
            assert_eq!(
                serde_json::to_value(stored).unwrap(),
                serde_json::from_str::<serde_json::Value>(line).unwrap()
            );
        }

        // Import again: 8 already_present, file bytes unchanged.
        let before = fs::read(&path).unwrap();
        let mut again = ImportRun::new();
        for line in &lines {
            assert_eq!(
                import(base, line, ImportTarget::AsRecorded, &mut again).outcome,
                ImportOutcome::AlreadyPresent
            );
        }
        assert!(again.sync().unwrap().is_empty());
        assert_eq!(fs::read(&path).unwrap(), before);

        // A line with a known id and other content is refused.
        let changed = exported("mem-new-3", &key, 1_003, "another text");
        assert_eq!(
            refusal(&import(base, &changed, here, &mut again)),
            "id already holds a different note"
        );

        // A tombstoned id is kept_forgotten.
        forget_in(base, &scope, "mem-new-5").unwrap();
        let before = fs::read(&path).unwrap();
        let mut run = ImportRun::new();
        assert_eq!(
            import(base, &lines[5], here, &mut run).outcome,
            ImportOutcome::KeptForgotten
        );

        // A line with an extra field is refused.
        let extra = lines[0].replacen('{', r#"{"colour":"red","#, 1);
        assert_eq!(
            refusal(&import(base, &extra, here, &mut run)),
            "unknown field colour"
        );

        // The user-wide key is refused: as a line's own project, and as the
        // project to file under.
        let user_wide = exported("mem-user", USER_WIDE_KEY, 3_000, "a user-wide note");
        assert_eq!(
            refusal(&import(base, &user_wide, ImportTarget::AsRecorded, &mut run)),
            "user-wide note; pass --cwd to file it under a project"
        );
        let to_user_wide = ImportTarget::Project(USER_WIDE_KEY);
        let err = import_line_in(base, &lines[0], to_user_wide, false, &mut run).unwrap_err();
        assert!(matches!(err, MemoryError::InvalidProjectRoot(_)), "{err}");

        assert!(run.sync().unwrap().is_empty());
        assert_eq!(fs::read(&path).unwrap(), before, "none of these wrote");
        assert_eq!(list_namespaces_in(base).unwrap(), [scope.namespace()]);

        // A tombstone line before its note keeps the note out.
        let tombstone = r#"{"tombstone":"mem-late","timestamp_ms":77}"#;
        let late = exported("mem-late", &key, 2_000, "forgotten before it came");
        let first = import(base, tombstone, here, &mut run);
        assert_eq!(
            (first.outcome, first.tombstone),
            (ImportOutcome::Stored, true)
        );
        assert_eq!(
            import(base, &late, here, &mut run).outcome,
            ImportOutcome::KeptForgotten
        );
        run.sync().unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap().lines().last(),
            Some(tombstone),
            "the tombstone is stored with the time it came with"
        );
        assert!(matches!(
            fetch_exact_in(base, &scope, &["mem-late"]),
            Err(MemoryError::NotFound(_))
        ));
        // In a later run too.
        assert_eq!(
            import(base, &late, here, &mut ImportRun::new()).outcome,
            ImportOutcome::KeptForgotten
        );

        // Re-homing rewrites `project` and `namespace`, and nothing else.
        let elsewhere = "pc4d9f8d15dc3bd8f";
        let moved = exported("mem-moved", elsewhere, 4_000, "from another project");
        let imported = import(base, &moved, here, &mut run);
        assert_eq!(
            (imported.outcome, imported.project),
            (ImportOutcome::Stored, Some(key.clone()))
        );
        let stored = fetch_exact_in(base, &scope, &["mem-moved"]).unwrap().remove(0);
        let mut expected: MemoryRecord = serde_json::from_str(&moved).unwrap();
        expected.project = Some(key.clone());
        expected.namespace = scope.namespace();
        assert_eq!(stored, expected);
        assert!(!namespace_file(base, &project_namespace(elsewhere)).exists());

        // A user-wide note is filed under a project when one is named.
        assert_eq!(
            import(base, &user_wide, here, &mut run).outcome,
            ImportOutcome::Stored
        );
        let filed = fetch_exact_in(base, &scope, &["mem-user"]).unwrap().remove(0);
        assert_eq!(filed.project.as_deref(), Some(key.as_str()));
        assert_eq!(filed.namespace, scope.namespace());
    }

    /// What this shows: with `dry_run` no byte of the store changes and no
    /// file or directory appears, and the outcomes are, line for line, those
    /// of the real run made next from the same files. It cannot show the
    /// same for a store that another process changes between the two runs.
    #[test]
    fn a_dry_run_changes_no_byte_and_says_what_the_real_run_then_does() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let scope = project_scope(&tmp);
        let key = scope.key().to_string();
        let here = ImportTarget::Project(&key);
        let path = namespace_file(base, &scope.namespace());

        // In the file: a live note and a forgotten one.
        let mut setup = ImportRun::new();
        for id in ["mem-live", "mem-gone"] {
            import(base, &exported(id, &key, 10, "there before"), here, &mut setup);
        }
        setup.sync().unwrap();
        forget_in(base, &scope, "mem-gone").unwrap();
        let before = fs::read(&path).unwrap();

        let lines = [
            exported("mem-a", &key, 20, "new"),
            exported("mem-a", &key, 20, "new"),
            exported("mem-a", &key, 20, "not the same"),
            exported("mem-live", &key, 10, "there before"),
            exported("mem-gone", &key, 10, "there before"),
            r#"{"tombstone":"mem-live","timestamp_ms":30}"#.to_string(),
            r#"{"tombstone":"mem-live","timestamp_ms":31}"#.to_string(),
            exported("mem-live", &key, 10, "there before"),
            r#"{"tombstone":"mem-never","timestamp_ms":32}"#.to_string(),
            exported("mem-never", &key, 40, "comes after its tombstone"),
            "not json".to_string(),
        ];
        let expected = [
            "stored",
            "already_present",
            "refused",
            "already_present",
            "kept_forgotten",
            "forgotten",
            "already_present",
            "kept_forgotten",
            "stored",
            "kept_forgotten",
            "refused",
        ];
        let run_all = |dry_run: bool| -> Vec<ImportedLine> {
            let mut run = ImportRun::new();
            let all: Vec<ImportedLine> = lines
                .iter()
                .map(|line| import_line_in(base, line, here, dry_run, &mut run).unwrap())
                .collect();
            assert_eq!(run.sync().unwrap().is_empty(), dry_run);
            all
        };

        take_steps();
        let dry = run_all(true);
        assert!(take_steps().is_empty(), "a dry run writes and syncs nothing");
        assert_eq!(fs::read(&path).unwrap(), before, "byte for byte");
        assert_eq!(list_namespaces_in(base).unwrap(), [scope.namespace()]);
        let names: Vec<&str> = dry.iter().map(|line| line.outcome.name()).collect();
        assert_eq!(names, expected);

        let real = run_all(false);
        assert_eq!(real, dry, "the real run does what the dry run said");
        let live = load_live_project_records(base, &scope).unwrap();
        let ids: Vec<&str> = live.iter().filter_map(|r| r.id.as_deref()).collect();
        assert_eq!(ids, ["mem-a"], "and the file agrees");

        // Where there is no store yet, a dry run makes none.
        let empty = TempDir::new().unwrap();
        let mut run = ImportRun::new();
        let imported = import_line_in(empty.path(), &lines[0], here, true, &mut run).unwrap();
        assert_eq!(imported.outcome, ImportOutcome::Stored);
        assert!(run.sync().unwrap().is_empty());
        assert_eq!(fs::read_dir(empty.path()).unwrap().count(), 0);
    }

    /// What this shows: the order of calls. An import's lines are written
    /// without a sync each, and `ImportRun::sync` then syncs every file the
    /// run appended to, once, and their directory after them. As for a save,
    /// it cannot show that the bytes reached the disk.
    #[test]
    fn an_import_syncs_each_file_once_at_the_end_of_the_run() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let (a, b) = ("p1111111111111111", "p2222222222222222");
        let mut run = ImportRun::new();
        take_steps();
        for (i, project) in [a, a, b, a].into_iter().enumerate() {
            let line = exported(&format!("mem-{i}"), project, i as u64, "x");
            assert_eq!(
                import(base, &line, ImportTarget::AsRecorded, &mut run).outcome,
                ImportOutcome::Stored
            );
        }
        let tombstone = r#"{"tombstone":"mem-0","timestamp_ms":9}"#;
        assert_eq!(
            import(base, tombstone, ImportTarget::Project(a), &mut run).outcome,
            ImportOutcome::Forgotten
        );
        assert_eq!(take_steps(), ["write"; 5], "five lines, no sync yet");

        let files = [a, b].map(|key| namespace_file(base, &project_namespace(key)));
        assert_eq!(run.sync().unwrap(), files);
        assert_eq!(take_steps(), ["sync file", "sync file", "sync dir"]);
    }

    #[test]
    fn a_line_that_could_not_be_kept_whole_is_refused_and_writes_nothing() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let key = "p1111111111111111";
        let good = exported("mem-x", key, 5, "secret words");
        let with = |from: &str, to: &str| {
            assert!(good.contains(from), "{from}");
            good.replacen(from, to, 1)
        };
        let long_id = format!(r#""id":"{}""#, "i".repeat(129));
        let big = format!(r#""content":"{}""#, "c".repeat(MAX_CONTENT_BYTES + 1));
        let wrong_value = "not a note: wrong value at column ";
        let cases: Vec<(String, &str)> = vec![
            // A time is never made up.
            (with(r#","timestamp_ms":5"#, ""), "missing field timestamp_ms"),
            (with(r#""timestamp_ms":5"#, r#""timestamp_ms":"5""#), wrong_value),
            // Nothing is dropped: not a field, not a part of one, not a
            // second copy of one.
            (with(r#"{"content""#, r#"{"extra":1,"content""#), "unknown field extra"),
            (
                with(r#""source":"model""#, r#""source":"model","host":"x""#),
                "provenance holds something a note cannot keep",
            ),
            (
                with(r#"{"content""#, r#"{"content":"first","content""#),
                "duplicate field content",
            ),
            (with(r#""retention":{"max_age_days":30}"#, r#""retention":"forever""#), wrong_value),
            (with(r#""id":"mem-x","#, ""), "missing field id"),
            (with(r#""id":"mem-x""#, r#""id":"mem x""#), "invalid id"),
            (with(r#""id":"mem-x""#, r#""id":"""#), "invalid id"),
            (with(r#""id":"mem-x""#, &long_id), "invalid id"),
            (with(r#""project":"p1111111111111111","#, ""), "missing field project"),
            (
                with(r#""project":"p1111111111111111""#, r#""project":"P1111111111111111""#),
                "invalid project",
            ),
            (
                with(r#""namespace":"project-p1111111111111111""#, r#""namespace":"notes""#),
                "namespace is not project-<project>",
            ),
            (with(r#""content":"secret words""#, &big), "content is over 16384 bytes"),
            (r#"{"content":"secret words""#.to_string(), "not JSON at column "),
            (r#"["secret words"]"#.to_string(), "not a JSON object"),
            (r#""secret words""#.to_string(), "not a JSON object"),
            // Tombstone lines.
            (
                r#"{"tombstone":"mem-x","timestamp_ms":1,"content":"secret words"}"#.to_string(),
                "unknown field content",
            ),
            (r#"{"tombstone":"mem-x"}"#.to_string(), "missing field timestamp_ms"),
            (
                r#"{"tombstone":"mem-x","timestamp_ms":1}"#.to_string(),
                "a tombstone line names no project; pass --cwd or --project-key",
            ),
        ];
        let mut run = ImportRun::new();
        for (line, expected) in &cases {
            let imported = import(base, line, ImportTarget::AsRecorded, &mut run);
            let reason = refusal(&imported);
            assert_eq!(
                reason.trim_end_matches(|c: char| c.is_ascii_digit()),
                *expected,
                "{line:.70}"
            );
            assert!(!reason.contains("secret"), "a reason quotes nothing: {reason}");
        }
        assert!(run.sync().unwrap().is_empty());
        assert!(!memory_dir_in(base).exists(), "a refused line writes nothing");

        // The line they were all made from is a good one.
        assert_eq!(
            import(base, &good, ImportTarget::AsRecorded, &mut run).outcome,
            ImportOutcome::Stored
        );
    }

    /// A `null` and an absent field say the same, and absent `tags` are no
    /// tags: such a line is kept, as the record the store's reader makes of
    /// it.
    #[test]
    fn a_null_field_and_absent_tags_are_not_a_loss() {
        let tmp = TempDir::new().unwrap();
        let scope = project_scope(&tmp);
        let key = scope.key();
        let line = format!(
            r#"{{"namespace":"project-{key}","timestamp_ms":7,"content":"c","id":"mem-n","project":"{key}","meta":null,"provenance":{{"source":"model","session":null}}}}"#
        );
        let mut run = ImportRun::new();
        assert_eq!(
            import(tmp.path(), &line, ImportTarget::AsRecorded, &mut run).outcome,
            ImportOutcome::Stored
        );
        let stored = fetch_exact_in(tmp.path(), &scope, &["mem-n"]).unwrap().remove(0);
        assert_eq!(stored, serde_json::from_str::<MemoryRecord>(&line).unwrap());
        assert_eq!(stored.timestamp_ms, 7);
    }

    /// Private-mode tests (spec §5.4). Umask isolation: see
    /// `private_fs::test_support::UmaskGuard` — `#[serial(umask)]` serializes
    /// every umask-mutating test in the crate, and the guard restores the old
    /// mask on drop (panic-safe).
    #[cfg(unix)]
    mod private_modes {
        use super::*;
        use crate::core::private_fs::test_support::UmaskGuard;
        use serial_test::serial;
        use std::os::unix::fs::PermissionsExt;

        fn mode_of(path: &std::path::Path) -> u32 {
            fs::metadata(path).unwrap().permissions().mode() & 0o777
        }

        #[test]
        #[serial(umask)]
        fn append_creates_0600_file_and_0700_dir_under_permissive_umask() {
            let _umask = UmaskGuard::set(0);
            let tmp = TempDir::new().unwrap();
            append_to(tmp.path(), &rec("ns", 1, "x", vec![])).unwrap();
            let dir = memory_dir_in(tmp.path());
            assert_eq!(mode_of(&dir), 0o700, "memory dir must be 0700");
            assert_eq!(
                mode_of(&dir.join("ns.jsonl")),
                0o600,
                "memory file must be 0600"
            );
        }

        #[test]
        fn append_refuses_symlink_target() {
            let tmp = TempDir::new().unwrap();
            let dir = memory_dir_in(tmp.path());
            fs::create_dir_all(&dir).unwrap();
            let victim = tmp.path().join("victim.txt");
            fs::write(&victim, "").unwrap();
            std::os::unix::fs::symlink(&victim, dir.join("ns.jsonl")).unwrap();
            let res = append_to(tmp.path(), &rec("ns", 1, "secret", vec![]));
            assert!(res.is_err(), "append through a symlink must fail");
            assert_eq!(
                fs::read_to_string(&victim).unwrap(),
                "",
                "no bytes may be written through the planted symlink"
            );
        }

        #[test]
        fn append_repairs_preexisting_broad_mode() {
            let tmp = TempDir::new().unwrap();
            let dir = memory_dir_in(tmp.path());
            fs::create_dir_all(&dir).unwrap();
            let file = dir.join("ns.jsonl");
            fs::write(&file, "").unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o666)).unwrap();
            append_to(tmp.path(), &rec("ns", 1, "x", vec![])).unwrap();
            assert_eq!(
                mode_of(&file),
                0o600,
                "broad pre-existing mode must be repaired"
            );
        }
    }
}
