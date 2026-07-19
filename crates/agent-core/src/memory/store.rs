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

/// Append one record. Validates namespace, content size. Atomic per-line via O_APPEND.
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
    let mut f =
        crate::core::private_fs::open_private_append(&path).map_err(std::io::Error::from)?;
    let mut line = serde_json::to_string(record)?;
    line.push('\n');
    f.write_all(line.as_bytes())?;
    Ok(())
}

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
        Ok(Self {
            key,
            root: canonical,
        })
    }

    /// Stable scope key (`p<16 hex>`).
    pub fn key(&self) -> &str {
        &self.key
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
            if !rec.content.to_lowercase().contains(needle) {
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
            let bounded = crate::text::BoundedText::new(&rec.content, snippet_bytes);
            MemoryDescriptor {
                id: rec.id.clone().unwrap_or_default(),
                project: scope.key().to_string(),
                timestamp_ms: rec.timestamp_ms,
                tags: rec.tags,
                snippet: bounded.text,
                truncated: bounded.truncated,
                content_bytes: rec.content.len(),
                sensitivity: rec.sensitivity.unwrap_or(MemorySensitivity::Normal),
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
    let mut f =
        crate::core::private_fs::open_private_append(&path).map_err(std::io::Error::from)?;
    let mut line = serde_json::to_string(&TombstoneLine {
        tombstone: id.to_string(),
        timestamp_ms: now_ms(),
    })?;
    line.push('\n');
    f.write_all(line.as_bytes())?;
    Ok(())
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
