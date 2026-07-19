//! Task 34 — unified retention (spec §9.7): one sweep across sessions
//! (including compaction parents), memory records, derived memory indexes,
//! traces, and logs, with max-age and disk budgets and headless
//! inspect/export/forget operations.
//!
//! Invariants:
//!
//! - CHAIN INTEGRITY: a session that is the head of a named chain is never
//!   deleted by any sweep or forget — retention cannot leave a named chain
//!   pointing at a deleted session.
//! - Sessions age by their EMBEDDED `updated_at` (mtime fallback); memory
//!   records age by their embedded timestamps plus per-record retention;
//!   traces/logs age by file mtime.
//! - Memory files are compacted by atomic rewrite (expired + tombstoned
//!   records physically removed) rather than whole-file deletion; their
//!   derived index directories are dropped alongside (rebuildable state).
//! - The disk budget deletes oldest-first across whole-file artifacts
//!   (sessions/traces/logs/indexes), skipping protected chain heads.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// Filesystem roots one retention pass operates over. Production callers
/// use [`RetentionRoots::resolve`]; tests inject temp roots.
#[derive(Debug, Clone)]
pub struct RetentionRoots {
    /// Active config dir: sessions/, chains/, synaps.log* live here.
    pub config_dir: PathBuf,
    /// Base dir: memory/ lives here.
    pub base_dir: PathBuf,
    /// Cache dir: trace/telemetry JSONL files live here.
    pub cache_dir: PathBuf,
}

impl RetentionRoots {
    /// Resolve the production roots.
    pub fn resolve() -> Self {
        let cache_dir = std::env::var("HOME")
            .map(|home| PathBuf::from(home).join(".cache/synaps"))
            .unwrap_or_else(|_| crate::config::base_dir().join("cache"));
        Self {
            config_dir: crate::config::get_active_config_dir(),
            base_dir: crate::config::base_dir(),
            cache_dir,
        }
    }

    fn sessions_dir(&self) -> PathBuf {
        self.config_dir.join("sessions")
    }
    fn chains_dir(&self) -> PathBuf {
        self.config_dir.join("chains")
    }
    fn memory_dir(&self) -> PathBuf {
        self.base_dir.join("memory")
    }
    fn index_dir(&self) -> PathBuf {
        self.memory_dir().join("index")
    }
}

/// Artifact domains under unified retention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionDomain {
    Sessions,
    Memory,
    MemoryIndex,
    Traces,
    Logs,
}

/// Unified policy: maximum artifact age and total disk budget.
#[derive(Debug, Clone, Copy, Default)]
pub struct RetentionPolicy {
    pub max_age_days: Option<u32>,
    pub max_disk_bytes: Option<u64>,
}

const DAY_MS: u64 = 86_400_000;

#[derive(Debug, Clone, Serialize)]
pub struct DomainReport {
    pub domain: RetentionDomain,
    pub files: usize,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RetentionReport {
    pub domains: Vec<DomainReport>,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct SweepOutcome {
    pub deleted_files: usize,
    pub freed_bytes: u64,
    pub protected_chain_heads: usize,
    pub memory_records_dropped: usize,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ExportSummary {
    pub files: usize,
    pub bytes: u64,
}

fn list_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.path().is_file() {
                out.push(entry.path());
            }
        }
    }
    out.sort();
    out
}

fn log_files(config_dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = list_files(config_dir)
        .into_iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("synaps.log"))
        })
        .collect();
    out.sort();
    out
}

fn dir_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                total += dir_bytes(&path);
            } else if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

/// Session ids protected by named chains (heads must never dangle).
fn chain_heads(roots: &RetentionRoots) -> Vec<String> {
    let mut heads = Vec::new();
    for path in list_files(&roots.chains_dir()) {
        if let Ok(raw) = fs::read_to_string(&path) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
                if let Some(head) = value["head"].as_str() {
                    heads.push(head.to_string());
                }
            }
        }
    }
    heads
}

/// A session's age reference: embedded `updated_at`, mtime fallback.
fn session_age_ms(path: &Path) -> Option<u64> {
    if let Ok(raw) = fs::read_to_string(path) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(updated) = value["updated_at"].as_str() {
                if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(updated) {
                    return Some(ts.timestamp_millis().max(0) as u64);
                }
            }
        }
    }
    mtime_ms(path)
}

fn mtime_ms(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
}

/// Headless inspect: per-domain file counts and byte totals.
pub fn inspect(roots: &RetentionRoots) -> io::Result<RetentionReport> {
    let sessions = list_files(&roots.sessions_dir());
    let memory: Vec<PathBuf> = list_files(&roots.memory_dir())
        .into_iter()
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .collect();
    let traces = list_files(&roots.cache_dir);
    let logs = log_files(&roots.config_dir);

    let sum = |files: &[PathBuf]| -> u64 {
        files
            .iter()
            .filter_map(|p| fs::metadata(p).ok().map(|m| m.len()))
            .sum()
    };
    let index_bytes = dir_bytes(&roots.index_dir());

    let domains = vec![
        DomainReport {
            domain: RetentionDomain::Sessions,
            files: sessions.len(),
            bytes: sum(&sessions),
        },
        DomainReport {
            domain: RetentionDomain::Memory,
            files: memory.len(),
            bytes: sum(&memory),
        },
        DomainReport {
            domain: RetentionDomain::MemoryIndex,
            files: 0,
            bytes: index_bytes,
        },
        DomainReport {
            domain: RetentionDomain::Traces,
            files: traces.len(),
            bytes: sum(&traces),
        },
        DomainReport {
            domain: RetentionDomain::Logs,
            files: logs.len(),
            bytes: sum(&logs),
        },
    ];
    let total_bytes = domains.iter().map(|d| d.bytes).sum();
    Ok(RetentionReport {
        domains,
        total_bytes,
    })
}

/// Unified sweep at an explicit clock (tests inject `now_ms`; production
/// uses [`sweep`]). Applies max-age per domain, compacts memory files, then
/// enforces the disk budget oldest-first — never touching chain heads.
pub fn sweep_at(
    roots: &RetentionRoots,
    policy: &RetentionPolicy,
    now_ms: u64,
) -> io::Result<SweepOutcome> {
    let mut outcome = SweepOutcome::default();
    let protected: Vec<String> = chain_heads(roots);
    let age_cutoff = policy
        .max_age_days
        .map(|days| now_ms.saturating_sub(days as u64 * DAY_MS));

    // ── Age pass: sessions (embedded timestamps; compaction parents are
    // ordinary sessions here) ──
    if let Some(cutoff) = age_cutoff {
        for path in list_files(&roots.sessions_dir()) {
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            if protected.contains(&id) {
                outcome.protected_chain_heads += 1;
                continue;
            }
            let Some(age_ref) = session_age_ms(&path) else {
                continue; // unknown age: fail safe, keep
            };
            if age_ref < cutoff {
                delete_file(&path, &mut outcome)?;
            }
        }

        // ── Traces + logs: mtime-aged whole files ──
        for path in list_files(&roots.cache_dir)
            .into_iter()
            .chain(log_files(&roots.config_dir))
        {
            if mtime_ms(&path).is_some_and(|m| m < cutoff) {
                delete_file(&path, &mut outcome)?;
            }
        }
    }

    // ── Memory: atomic rewrite dropping tombstoned records, records past
    // their OWN retention class, and (when a global age policy is set)
    // records past the global cutoff. Runs on every sweep so per-record
    // retention holds even without a global max age. ──
    for path in list_files(&roots.memory_dir()) {
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let dropped = compact_memory_file(&path, now_ms, age_cutoff)?;
        if dropped > 0 {
            outcome.memory_records_dropped += dropped;
            // The derived index for a rewritten namespace is stale by
            // construction — drop it; it rebuilds from the store.
            if let Some(ns) = path.file_stem().and_then(|s| s.to_str()) {
                let index_dir = roots.index_dir().join(ns);
                if index_dir.exists() {
                    outcome.freed_bytes += dir_bytes(&index_dir);
                    fs::remove_dir_all(&index_dir)?;
                }
            }
        }
    }

    // ── Disk budget: oldest-first across whole-file artifacts ──
    if let Some(budget) = policy.max_disk_bytes {
        let mut candidates: Vec<(u64, PathBuf, bool)> = Vec::new(); // (age_ref, path, protected)
        for path in list_files(&roots.sessions_dir()) {
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let age = session_age_ms(&path).unwrap_or(u64::MAX);
            candidates.push((age, path, protected.contains(&id)));
        }
        for path in list_files(&roots.cache_dir)
            .into_iter()
            .chain(log_files(&roots.config_dir))
        {
            let age = mtime_ms(&path).unwrap_or(u64::MAX);
            candidates.push((age, path, false));
        }
        candidates.sort_by(|a, b| a.0.cmp(&b.0));

        let mut total: u64 = candidates
            .iter()
            .filter_map(|(_, p, _)| fs::metadata(p).ok().map(|m| m.len()))
            .sum::<u64>()
            + dir_bytes(&roots.index_dir())
            + list_files(&roots.memory_dir())
                .iter()
                .filter_map(|p| fs::metadata(p).ok().map(|m| m.len()))
                .sum::<u64>();
        for (_, path, is_protected) in candidates {
            if total <= budget {
                break;
            }
            if is_protected {
                outcome.protected_chain_heads += 1;
                continue;
            }
            let len = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            delete_file(&path, &mut outcome)?;
            total = total.saturating_sub(len);
        }
    }

    Ok(outcome)
}

/// Production sweep at the current clock.
pub fn sweep(roots: &RetentionRoots, policy: &RetentionPolicy) -> io::Result<SweepOutcome> {
    sweep_at(roots, policy, crate::epoch_millis())
}

fn delete_file(path: &Path, outcome: &mut SweepOutcome) -> io::Result<()> {
    let len = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    fs::remove_file(path)?;
    outcome.deleted_files += 1;
    outcome.freed_bytes += len;
    Ok(())
}

/// Rewrite one memory JSONL file, dropping tombstoned records, records past
/// their own retention class (`ts + max_age_days < now`), and — when a
/// global cutoff is set — records older than it. Atomic rewrite; returns
/// the number of records physically removed.
fn compact_memory_file(
    path: &Path,
    now_ms: u64,
    global_cutoff_ms: Option<u64>,
) -> io::Result<usize> {
    let raw = fs::read_to_string(path)?;
    let mut tombstoned: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut records: Vec<(Option<String>, u64, Option<u64>, String)> = Vec::new();
    let mut dropped = 0usize;

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            dropped += 1; // torn/garbage line — compacted away
            continue;
        };
        if let Some(id) = value["tombstone"].as_str() {
            tombstoned.insert(id.to_string());
            continue;
        }
        let ts = value["timestamp_ms"].as_u64().unwrap_or(0);
        let per_record_span_ms = value["retention"]["max_age_days"]
            .as_u64()
            .map(|days| days * DAY_MS);
        records.push((
            value["id"].as_str().map(String::from),
            ts,
            per_record_span_ms,
            trimmed.to_string(),
        ));
    }

    let mut kept: Vec<String> = Vec::new();
    for (id, ts, per_record_span_ms, line) in records {
        if id.as_ref().is_some_and(|id| tombstoned.contains(id)) {
            dropped += 1;
            continue;
        }
        if per_record_span_ms.is_some_and(|span| ts.saturating_add(span) < now_ms) {
            dropped += 1;
            continue;
        }
        if global_cutoff_ms.is_some_and(|cutoff| ts < cutoff) {
            dropped += 1;
            continue;
        }
        kept.push(line);
    }

    // Tombstones fully applied: kept lines only.
    if dropped > 0 {
        let mut body = kept.join("\n");
        if !body.is_empty() {
            body.push('\n');
        }
        crate::core::private_fs::write_atomic_private(path, body.as_bytes())
            .map_err(io::Error::from)?;
    }
    Ok(dropped)
}

/// Headless export: copy sessions, memory, traces, and logs to `dest`.
pub fn export(roots: &RetentionRoots, dest: &Path) -> io::Result<ExportSummary> {
    let mut summary = ExportSummary { files: 0, bytes: 0 };
    let plans = [
        (roots.sessions_dir(), dest.join("sessions")),
        (roots.memory_dir(), dest.join("memory")),
        (roots.cache_dir.clone(), dest.join("traces")),
    ];
    for (src, out) in plans {
        for file in list_files(&src) {
            fs::create_dir_all(&out)?;
            let name = file.file_name().unwrap_or_default();
            let target = out.join(name);
            fs::copy(&file, &target)?;
            summary.files += 1;
            summary.bytes += fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
        }
    }
    for file in log_files(&roots.config_dir) {
        let out = dest.join("logs");
        fs::create_dir_all(&out)?;
        let target = out.join(file.file_name().unwrap_or_default());
        fs::copy(&file, &target)?;
        summary.files += 1;
        summary.bytes += fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
    }
    Ok(summary)
}

/// Headless forget: delete one artifact by domain and id. Session chain
/// heads fail closed; memory ids use `"<namespace>:<record-id>"` and
/// tombstone through the store.
pub fn forget(roots: &RetentionRoots, domain: RetentionDomain, id: &str) -> io::Result<()> {
    match domain {
        RetentionDomain::Sessions => {
            if chain_heads(roots).contains(&id.to_string()) {
                return Err(io::Error::other(format!(
                    "session {id} is the head of a named chain — delete or repoint the \
                     chain first (retention never leaves chains dangling)"
                )));
            }
            let path = roots.sessions_dir().join(format!("{id}.json"));
            fs::remove_file(path)
        }
        RetentionDomain::Traces => fs::remove_file(roots.cache_dir.join(sanitize_file_id(id)?)),
        RetentionDomain::Logs => {
            if !id.starts_with("synaps.log") {
                return Err(io::Error::other("log ids start with synaps.log"));
            }
            fs::remove_file(roots.config_dir.join(sanitize_file_id(id)?))
        }
        RetentionDomain::MemoryIndex => {
            let dir = roots.index_dir().join(sanitize_file_id(id)?);
            fs::remove_dir_all(dir)
        }
        RetentionDomain::Memory => {
            let (namespace, record_id) = id
                .split_once(':')
                .ok_or_else(|| io::Error::other("memory ids use \"<namespace>:<record-id>\""))?;
            let path = roots
                .memory_dir()
                .join(format!("{}.jsonl", sanitize_file_id(namespace)?.display()));
            let raw = fs::read_to_string(&path)?;
            if !raw.contains(record_id) {
                return Err(io::Error::other(format!(
                    "record {record_id} not present in namespace {namespace}"
                )));
            }
            let line = serde_json::json!({
                "tombstone": record_id,
                "timestamp_ms": crate::epoch_millis(),
            });
            let mut body = raw;
            if !body.ends_with('\n') && !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&line.to_string());
            body.push('\n');
            crate::core::private_fs::write_atomic_private(&path, body.as_bytes())
                .map_err(io::Error::from)
        }
    }
}

/// Reject path-traversal in file-name ids.
fn sanitize_file_id(id: &str) -> io::Result<PathBuf> {
    if id.is_empty() || id.contains('/') || id.contains('\\') || id.contains("..") {
        return Err(io::Error::other(format!("invalid artifact id: {id:?}")));
    }
    Ok(PathBuf::from(id))
}
