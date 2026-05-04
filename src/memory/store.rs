use std::fs::{self, OpenOptions};
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
    ContentTooLarge { len: usize, max: usize },
    Io(String),
    Serde(String),
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

pub(crate) fn memory_dir_in(base: &Path) -> PathBuf {
    base.join("memory")
}

fn namespace_path(dir: &Path, ns: &str) -> PathBuf {
    dir.join(format!("{ns}.jsonl"))
}

/// Append one record. Validates namespace, content size. Atomic per-line via O_APPEND.
pub fn append(record: &MemoryRecord) -> Result<(), MemoryError> {
    append_to(&crate::config::base_dir(), record)
}

pub(crate) fn append_to(base: &Path, record: &MemoryRecord) -> Result<(), MemoryError> {
    validate_namespace(&record.namespace)?;
    if record.content.len() > MAX_CONTENT_BYTES {
        return Err(MemoryError::ContentTooLarge {
            len: record.content.len(),
            max: MAX_CONTENT_BYTES,
        });
    }
    let dir = memory_dir_in(base);
    fs::create_dir_all(&dir)?;
    let path = namespace_path(&dir, &record.namespace);
    let mut f = OpenOptions::new().append(true).create(true).open(&path)?;
    let mut line = serde_json::to_string(record)?;
    line.push('\n');
    f.write_all(line.as_bytes())?;
    Ok(())
}

/// Query records in a namespace, applying filters, returning most-recent-first up to limit.
pub fn query(namespace: &str, q: &MemoryQuery) -> Result<Vec<MemoryRecord>, MemoryError> {
    query_in(&crate::config::base_dir(), namespace, q)
}

pub(crate) fn query_in(
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
    out.sort_by(|a, b| b.timestamp_ms.cmp(&a.timestamp_ms));
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
}
