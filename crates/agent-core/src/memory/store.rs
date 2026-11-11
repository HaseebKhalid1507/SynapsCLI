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
    append_jsonl_line(&path, &line)
}

/// Append one JSONL line and make it durable before returning: the line goes
/// to the file whole ([`write_whole_line`]), the file is synced, and when
/// this append found no file or an empty one, its directory is synced too,
/// because nothing else puts the new file's name on disk.
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
fn append_jsonl_line(path: &Path, line: &str) -> Result<(), MemoryError> {
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
    sync_file(&f)?;
    if !had_bytes {
        if let Some(dir) = path.parent() {
            sync_dir(dir)?;
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
        format!("project-{}", self.key)
    }
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

/// Load the LIVE (non-tombstoned, scope-matching) records of a project.
/// Records whose inner `project` key does not match the scope are excluded
/// — fail closed even if a foreign file was copied into the namespace.
fn load_live_project_records(
    base: &Path,
    scope: &ProjectScope,
) -> Result<Vec<MemoryRecord>, MemoryError> {
    let path = namespace_path(&memory_dir_in(base), &scope.namespace());
    if !path.exists() {
        return Ok(Vec::new());
    }
    let f = fs::File::open(&path)?;
    let reader = BufReader::new(f);
    let mut live: Vec<MemoryRecord> = Vec::new();
    let mut tombstoned: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(tomb) = serde_json::from_str::<TombstoneLine>(&line) {
            tombstoned.insert(tomb.tombstone);
            continue;
        }
        let rec: MemoryRecord = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => continue,
        };
        // Fail closed: only records carrying THIS scope's key participate.
        if rec.project.as_deref() != Some(scope.key()) || rec.id.is_none() {
            continue;
        }
        live.push(rec);
    }
    live.retain(|r| {
        r.id.as_ref()
            .map(|id| !tombstoned.contains(id))
            .unwrap_or(false)
    });
    Ok(live)
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
    let dir = memory_dir_in(base);
    crate::core::private_fs::ensure_private_dir(&dir).map_err(std::io::Error::from)?;
    let path = namespace_path(&dir, &scope.namespace());
    let line = serde_json::to_string(&TombstoneLine {
        tombstone: id.to_string(),
        timestamp_ms: now_ms(),
    })?;
    append_jsonl_line(&path, &line)
}

/// Path of a project namespace's JSONL store file (index-module seam).
pub(crate) fn namespace_file(base: &Path, namespace: &str) -> PathBuf {
    namespace_path(&memory_dir_in(base), namespace)
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
