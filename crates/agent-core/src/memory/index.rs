//! Task 33 — staged in-repo lexical memory index (spec §9.6 bounds; SQLite
//! declined, see docs/decisions/T33-memory-index-no-sqlite.md).
//!
//! The index is fully DERIVED state over the append-only JSONL store:
//!
//! - immutable ts-desc-sorted segment files of doc summaries
//!   (`{"id","ts","tags","terms"}`). Lexical terms are CONTENT-DERIVED —
//!   they reveal the vocabulary of record bodies, so index files and
//!   directories are private-moded (0600/0700) like the store itself, and
//!   `secret`-sensitivity bodies contribute NO terms at all (metadata-only
//!   indexing: id, timestamp, tags);
//! - an atomically renamed `manifest.json` recording the consumed store
//!   offset, segment metadata (with min/max timestamps for range skipping),
//!   and the tombstone set;
//! - queries stream segment lines through a k-way ts-desc merge with a
//!   result-bounded collector — resident memory is proportional to the
//!   requested limit plus at most [`MAX_SEGMENTS`] buffered docs (staging
//!   compacts segments whenever they exceed that bound), never to the
//!   total match count, and streaming early-terminates once a page is
//!   full;
//! - crash safety + integrity: torn store tails are healed by the store's
//!   append path and skipped by the lenient reader; orphaned `*.tmp` files
//!   are ignored; the manifest records byte length AND sha256 per segment,
//!   and any unparseable manifest, offset mismatch, or missing/truncated/
//!   corrupted segment triggers a VERIFIED full rebuild from the store
//!   (the index can always be deleted safely).
//!
//! No network: this module performs filesystem I/O only. Embeddings are
//! DISABLED — no embedding hook exists here, so no implicit remote call is
//! possible (spec §9.6; local embeddings would be a separate ask-first
//! opt-in).

use std::collections::{BTreeSet, BinaryHeap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::store::{namespace_file, MemoryError, MemoryRecord, ProjectScope};
use super::store::{DEFAULT_SEARCH_LIMIT, MAX_SEARCH_LIMIT};

/// Docs per immutable segment file.
pub const SEGMENT_MAX_DOCS: usize = 1024;
/// Hard bound on staged segments (and therefore on open readers per
/// query). Staging compacts everything above this into fresh globally
/// ts-ordered segments.
pub const MAX_SEGMENTS: usize = 32;
/// Lexical terms retained per document (deduped, sorted).
pub const MAX_TERMS_PER_DOC: usize = 512;

const MANIFEST_VERSION: u32 = 2;

/// Per-project index directory: `<base>/memory/index/<namespace>/`.
pub fn index_dir_in(base: &Path, scope: &ProjectScope) -> PathBuf {
    super::store::memory_dir_in(base)
        .join("index")
        .join(scope.namespace())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SegmentMeta {
    file: String,
    docs: usize,
    min_ts: u64,
    max_ts: u64,
    /// Exact byte length of the segment file (truncation detection).
    bytes: u64,
    /// SHA-256 of the segment file (corruption detection).
    sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    /// Bytes of the store file consumed into segments.
    indexed_bytes: u64,
    segments: Vec<SegmentMeta>,
    tombstones: Vec<String>,
}

impl Manifest {
    fn empty() -> Self {
        Self {
            version: MANIFEST_VERSION,
            indexed_bytes: 0,
            segments: Vec::new(),
            tombstones: Vec::new(),
        }
    }
}

/// One indexed document summary — content-free (terms, not bodies).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SegmentDoc {
    id: String,
    ts: u64,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    terms: Vec<String>,
}

/// Exclusive pagination cursor: results strictly AFTER this position in
/// `(timestamp desc, id desc)` order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageCursor {
    pub before_ts: u64,
    pub before_id: String,
}

/// Index query. Terms use token-exact AND semantics over the lexical term
/// sets (see the T33 decision record for the trade-off vs FTS).
#[derive(Debug, Clone, Default)]
pub struct IndexQuery {
    pub terms: Vec<String>,
    pub tag_prefix: Option<String>,
    pub since_ms: Option<u64>,
    pub until_ms: Option<u64>,
    /// Clamped to [`MAX_SEARCH_LIMIT`].
    pub limit: Option<usize>,
    pub cursor: Option<PageCursor>,
}

/// A matching document reference (bodies stay in the store; fetch by id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexHit {
    pub id: String,
    pub timestamp_ms: u64,
    pub tags: Vec<String>,
}

/// Observability for the proportional-memory bound.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexSearchStats {
    /// Documents popped from the merge (early termination keeps this far
    /// below the corpus size for full pages).
    pub docs_scanned: usize,
    /// Peak retained hits — bounded by the requested limit by construction.
    pub max_resident_hits: usize,
    /// Segment readers opened for this query — bounded by [`MAX_SEGMENTS`].
    pub segments_open: usize,
}

/// One bounded result page.
#[derive(Debug, Clone)]
pub struct IndexPage {
    pub hits: Vec<IndexHit>,
    pub next: Option<PageCursor>,
    pub stats: IndexSearchStats,
}

/// Status returned by [`ensure_index_in`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexStatus {
    pub segments: usize,
    pub indexed_docs: usize,
    pub tombstones: usize,
}

/// Normalize content into deduped lexical terms.
fn terms_of(content: &str) -> Vec<String> {
    let mut set = BTreeSet::new();
    for token in content
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 2)
    {
        set.insert(token.to_lowercase());
        if set.len() >= MAX_TERMS_PER_DOC {
            break;
        }
    }
    set.into_iter().collect()
}

fn manifest_path(dir: &Path) -> PathBuf {
    dir.join("manifest.json")
}

fn load_manifest(dir: &Path) -> Option<Manifest> {
    let raw = fs::read_to_string(manifest_path(dir)).ok()?;
    let manifest: Manifest = serde_json::from_str(&raw).ok()?;
    if manifest.version != MANIFEST_VERSION {
        return None;
    }
    Some(manifest)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), MemoryError> {
    crate::core::private_fs::write_atomic_private(path, bytes)
        .map_err(std::io::Error::from)
        .map_err(MemoryError::from)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Verify every manifest-referenced segment: present, exact byte length,
/// exact sha256 (CP-13 fix1 I8). Any deviation invalidates the manifest.
fn segments_verified(dir: &Path, manifest: &Manifest) -> bool {
    for seg in &manifest.segments {
        let path = dir.join(&seg.file);
        let Ok(bytes) = fs::read(&path) else {
            return false; // missing/unreadable
        };
        if bytes.len() as u64 != seg.bytes {
            return false; // truncated/grown
        }
        if sha256_hex(&bytes) != seg.sha256 {
            return false; // corrupted
        }
    }
    true
}

/// Wipe every index artifact (derived state — always safe) for a rebuild.
fn reset_index_dir(dir: &Path) -> Result<(), MemoryError> {
    if dir.exists() {
        fs::remove_dir_all(dir)?;
    }
    Ok(())
}

/// Stage the index up to the current end of the store: consume unindexed
/// store bytes into new immutable segments and an atomically replaced
/// manifest. Crash-safe at every step; rebuilds from scratch when the
/// manifest is unparseable or the store shrank (rewritten by retention).
pub fn ensure_index_in(base: &Path, scope: &ProjectScope) -> Result<IndexStatus, MemoryError> {
    let dir = index_dir_in(base, scope);
    let store_path = namespace_file(base, &scope.namespace());
    let store_len = fs::metadata(&store_path).map(|m| m.len()).unwrap_or(0);

    let mut manifest = match load_manifest(&dir) {
        Some(m) if m.indexed_bytes <= store_len && segments_verified(&dir, &m) => m,
        Some(_) | None => {
            // Corrupt/mismatched manifest, rewritten store, or a
            // missing/truncated/corrupted segment: verified full rebuild.
            reset_index_dir(&dir)?;
            Manifest::empty()
        }
    };

    if store_len > manifest.indexed_bytes {
        crate::core::private_fs::ensure_private_dir(&dir).map_err(std::io::Error::from)?;
        let mut tombstones: HashSet<String> = manifest.tombstones.iter().cloned().collect();
        let mut pending: Vec<SegmentDoc> = Vec::new();

        let file = fs::File::open(&store_path)?;
        let mut reader = BufReader::new(file);
        // Skip already-consumed bytes.
        {
            use std::io::Read;
            std::io::copy(
                &mut Read::take(&mut reader, manifest.indexed_bytes),
                &mut std::io::sink(),
            )?;
        }
        let mut consumed = manifest.indexed_bytes;
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line)?;
            if n == 0 {
                break;
            }
            if !line.ends_with('\n') {
                // Torn tail: not consumed; the store's append path heals it
                // and a later pass picks up everything after the heal.
                break;
            }
            consumed += n as u64;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
                if let Some(id) = value.get("tombstone").and_then(|v| v.as_str()) {
                    tombstones.insert(id.to_string());
                    pending.retain(|doc| doc.id != id);
                    continue;
                }
            }
            let record: MemoryRecord = match serde_json::from_str(trimmed) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if record.project.as_deref() != Some(scope.key()) {
                continue;
            }
            let Some(id) = record.id.clone() else {
                continue;
            };
            if tombstones.contains(&id) {
                continue;
            }
            // CP-13 fix1 I7: secret bodies contribute NO terms — the
            // persisted index carries only metadata for them.
            let terms = if record.sensitivity == Some(super::store::MemorySensitivity::Secret) {
                Vec::new()
            } else {
                terms_of(&record.content)
            };
            pending.push(SegmentDoc {
                id,
                ts: record.timestamp_ms,
                tags: record.tags,
                terms,
            });
        }

        let mut next_seg = manifest
            .segments
            .iter()
            .filter_map(|s| {
                s.file
                    .strip_prefix("seg-")
                    .and_then(|rest| rest.strip_suffix(".jsonl"))
                    .and_then(|n| n.parse::<u64>().ok())
            })
            .max()
            .map(|n| n + 1)
            .unwrap_or(0);
        for chunk in pending.chunks(SEGMENT_MAX_DOCS) {
            let mut docs = chunk.to_vec();
            docs.sort_by(|a, b| b.ts.cmp(&a.ts).then_with(|| b.id.cmp(&a.id)));
            manifest
                .segments
                .push(write_segment(&dir, next_seg, &docs)?);
            next_seg += 1;
        }

        manifest.indexed_bytes = consumed;
        manifest.tombstones = tombstones.into_iter().collect();
        manifest.tombstones.sort();

        // CP-13 fix1 M1: bound the staged segment count. Above
        // MAX_SEGMENTS, merge every live doc into fresh globally
        // ts-ordered segments (tombstones applied and cleared). This is
        // maintenance-time memory (proportional to indexed docs), traded
        // for a hard bound on QUERY-time open readers and buffered docs.
        if manifest.segments.len() > MAX_SEGMENTS {
            let tombstoned: HashSet<String> = manifest.tombstones.iter().cloned().collect();
            let mut all_docs: Vec<SegmentDoc> = Vec::new();
            for seg in &manifest.segments {
                let mut stream = SegmentStream::open(&dir.join(&seg.file))?;
                while let Some(doc) = stream.current.take() {
                    if !tombstoned.contains(&doc.id) {
                        all_docs.push(doc);
                    }
                    stream.advance()?;
                }
            }
            all_docs.sort_by(|a, b| b.ts.cmp(&a.ts).then_with(|| b.id.cmp(&a.id)));

            let old_files: Vec<String> = manifest.segments.iter().map(|s| s.file.clone()).collect();
            let target_docs = all_docs
                .len()
                .div_ceil(MAX_SEGMENTS / 2)
                .max(SEGMENT_MAX_DOCS);
            let mut compacted: Vec<SegmentMeta> = Vec::new();
            for chunk in all_docs.chunks(target_docs) {
                compacted.push(write_segment(&dir, next_seg, chunk)?);
                next_seg += 1;
            }
            manifest.segments = compacted;
            manifest.tombstones.clear();
            // New manifest FIRST (atomic), then the orphaned old segment
            // files can be removed; a crash in between leaves ignorable
            // orphans, never a broken index.
            write_atomic(
                &manifest_path(&dir),
                serde_json::to_string_pretty(&manifest)?.as_bytes(),
            )?;
            for file in old_files {
                let _ = fs::remove_file(dir.join(file));
            }
        } else {
            write_atomic(
                &manifest_path(&dir),
                serde_json::to_string_pretty(&manifest)?.as_bytes(),
            )?;
        }
    }

    Ok(IndexStatus {
        segments: manifest.segments.len(),
        indexed_docs: manifest.segments.iter().map(|s| s.docs).sum(),
        tombstones: manifest.tombstones.len(),
    })
}

/// Serialize one ts-desc-sorted doc chunk into an immutable segment file
/// and return its verified metadata (length + sha256).
fn write_segment(dir: &Path, seq: u64, docs: &[SegmentDoc]) -> Result<SegmentMeta, MemoryError> {
    let mut body = String::new();
    for doc in docs {
        body.push_str(&serde_json::to_string(doc)?);
        body.push('\n');
    }
    let file = format!("seg-{seq:06}.jsonl");
    write_atomic(&dir.join(&file), body.as_bytes())?;
    Ok(SegmentMeta {
        file,
        docs: docs.len(),
        min_ts: docs.iter().map(|d| d.ts).min().unwrap_or(0),
        max_ts: docs.iter().map(|d| d.ts).max().unwrap_or(0),
        bytes: body.len() as u64,
        sha256: sha256_hex(body.as_bytes()),
    })
}

/// Streaming ts-desc segment reader: one buffered doc at a time.
struct SegmentStream {
    reader: BufReader<fs::File>,
    current: Option<SegmentDoc>,
}

impl SegmentStream {
    fn open(path: &Path) -> Result<Self, MemoryError> {
        let reader = BufReader::new(fs::File::open(path)?);
        let mut stream = Self {
            reader,
            current: None,
        };
        stream.advance()?;
        Ok(stream)
    }

    fn advance(&mut self) -> Result<(), MemoryError> {
        let mut line = String::new();
        loop {
            line.clear();
            if self.reader.read_line(&mut line)? == 0 {
                self.current = None;
                return Ok(());
            }
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<SegmentDoc>(line.trim()) {
                Ok(doc) => {
                    self.current = Some(doc);
                    return Ok(());
                }
                Err(_) => continue, // torn/foreign line — skip
            }
        }
    }
}

/// Search the staged index (auto-staging the tail first). Bounded page,
/// newest-first, resident memory proportional to the limit.
pub fn search_index_in(
    base: &Path,
    scope: &ProjectScope,
    query: &IndexQuery,
) -> Result<IndexPage, MemoryError> {
    ensure_index_in(base, scope)?;
    let dir = index_dir_in(base, scope);
    let manifest = load_manifest(&dir).unwrap_or_else(Manifest::empty);
    let tombstones: HashSet<&String> = manifest.tombstones.iter().collect();
    let limit = query
        .limit
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .min(MAX_SEARCH_LIMIT)
        .max(1);
    let wanted_terms: Vec<String> = query.terms.iter().flat_map(|t| terms_of(t)).collect();

    // Open streams for segments that can overlap the time range.
    let mut streams: Vec<SegmentStream> = Vec::new();
    for seg in &manifest.segments {
        if let Some(since) = query.since_ms {
            if seg.max_ts < since {
                continue;
            }
        }
        if let Some(until) = query.until_ms {
            if seg.min_ts > until {
                continue;
            }
        }
        if let Some(cursor) = &query.cursor {
            if seg.min_ts > cursor.before_ts {
                continue; // entirely newer than the cursor position
            }
        }
        streams.push(SegmentStream::open(&dir.join(&seg.file))?);
    }

    // K-way merge on (ts desc, id desc).
    #[derive(PartialEq, Eq)]
    struct HeapKey {
        ts: u64,
        id: String,
        stream: usize,
    }
    impl Ord for HeapKey {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            self.ts.cmp(&other.ts).then_with(|| self.id.cmp(&other.id))
        }
    }
    impl PartialOrd for HeapKey {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }
    let mut heap: BinaryHeap<HeapKey> = BinaryHeap::new();
    for (i, stream) in streams.iter().enumerate() {
        if let Some(doc) = &stream.current {
            heap.push(HeapKey {
                ts: doc.ts,
                id: doc.id.clone(),
                stream: i,
            });
        }
    }

    let mut hits: Vec<IndexHit> = Vec::new();
    let mut stats = IndexSearchStats {
        segments_open: streams.len(),
        ..Default::default()
    };
    let mut next: Option<PageCursor> = None;

    while let Some(key) = heap.pop() {
        let stream = &mut streams[key.stream];
        let doc = stream
            .current
            .take()
            .expect("heap keys always reference a buffered doc");
        stream.advance()?;
        if let Some(following) = &stream.current {
            heap.push(HeapKey {
                ts: following.ts,
                id: following.id.clone(),
                stream: key.stream,
            });
        }
        stats.docs_scanned += 1;

        // Cursor: only positions strictly after (older than) the cursor.
        if let Some(cursor) = &query.cursor {
            let after = doc.ts < cursor.before_ts
                || (doc.ts == cursor.before_ts && doc.id < cursor.before_id);
            if !after {
                continue;
            }
        }
        if tombstones.contains(&doc.id) {
            continue;
        }
        if let Some(since) = query.since_ms {
            if doc.ts < since {
                continue;
            }
        }
        if let Some(until) = query.until_ms {
            if doc.ts > until {
                continue;
            }
        }
        if let Some(prefix) = &query.tag_prefix {
            if !doc.tags.iter().any(|t| t.starts_with(prefix)) {
                continue;
            }
        }
        if !wanted_terms.is_empty() {
            let have: HashSet<&String> = doc.terms.iter().collect();
            if !wanted_terms.iter().all(|t| have.contains(t)) {
                continue;
            }
        }

        if hits.len() == limit {
            // One more qualifying doc exists → expose the cursor and stop.
            let last = hits.last().expect("limit >= 1");
            next = Some(PageCursor {
                before_ts: last.timestamp_ms,
                before_id: last.id.clone(),
            });
            break;
        }
        hits.push(IndexHit {
            id: doc.id,
            timestamp_ms: doc.ts,
            tags: doc.tags,
        });
        stats.max_resident_hits = stats.max_resident_hits.max(hits.len());
    }

    Ok(IndexPage { hits, next, stats })
}
