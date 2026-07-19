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

/// Typed retention failure (CP-13 fix1 I4/M2).
#[derive(Debug)]
pub enum RetentionError {
    Io(io::Error),
    /// A named-chain file could not be read or parsed: its head is UNKNOWN,
    /// so every destructive session operation fails closed.
    UnreadableChain(PathBuf),
    /// The disk budget could not be met after evicting every unprotected
    /// artifact (typed instead of a silent overshoot).
    BudgetUnmet {
        budget: u64,
        remaining: u64,
        protected_chain_heads: usize,
    },
}

impl std::fmt::Display for RetentionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RetentionError::Io(e) => write!(f, "retention io error: {e}"),
            RetentionError::UnreadableChain(path) => write!(
                f,
                "unreadable named-chain file {} — refusing every destructive session \
                 operation while chain protection is unknown",
                path.display()
            ),
            RetentionError::BudgetUnmet {
                budget,
                remaining,
                protected_chain_heads,
            } => write!(
                f,
                "disk budget unmet: {remaining} bytes remain against a budget of \
                 {budget} after evicting every unprotected artifact \
                 ({protected_chain_heads} protected chain head(s) retained)"
            ),
        }
    }
}

impl std::error::Error for RetentionError {}

impl From<io::Error> for RetentionError {
    fn from(e: io::Error) -> Self {
        RetentionError::Io(e)
    }
}

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

/// Session ids protected by named chains (heads must never dangle). Any
/// unreadable or unparseable chain file fails CLOSED (CP-13 fix1 M2): with
/// an unknown head, no destructive session operation may proceed.
fn chain_heads(roots: &RetentionRoots) -> Result<Vec<String>, RetentionError> {
    let mut heads = Vec::new();
    for path in list_files(&roots.chains_dir()) {
        let raw =
            fs::read_to_string(&path).map_err(|_| RetentionError::UnreadableChain(path.clone()))?;
        let value: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|_| RetentionError::UnreadableChain(path.clone()))?;
        match value["head"].as_str() {
            Some(head) => heads.push(head.to_string()),
            None => return Err(RetentionError::UnreadableChain(path.clone())),
        }
    }
    Ok(heads)
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
    let traces = cache_files(&roots.cache_dir);
    let logs = log_files(&roots.config_dir);
    let index_files = walk_regular_files(&roots.index_dir());

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
            files: index_files.len(),
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
) -> Result<SweepOutcome, RetentionError> {
    let mut outcome = SweepOutcome::default();
    // Chain protection resolves FIRST and fails closed — nothing is
    // deleted while any chain head is unknown.
    let protected: Vec<String> = chain_heads(roots)?;
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

        // ── Traces + logs: mtime-aged whole files (traces recursive,
        // symlink-confined) ──
        for path in cache_files(&roots.cache_dir)
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

    // ── Disk budget (CP-13 fix1 I4): staged eviction until the final
    // total actually fits, or a typed unmet error. Stage order minimizes
    // data loss: (1) derived index dirs (rebuildable — free win), (2)
    // whole-file artifacts oldest-first (never protected chain heads),
    // (3) memory records oldest-first via atomic rewrite. ──
    if let Some(budget) = policy.max_disk_bytes {
        let total_now = |roots: &RetentionRoots| -> u64 {
            inspect(roots).map(|r| r.total_bytes).unwrap_or(u64::MAX)
        };

        // Stage 1: derived index directories.
        if total_now(roots) > budget {
            if let Ok(entries) = fs::read_dir(roots.index_dir()) {
                for entry in entries.flatten() {
                    if total_now(roots) <= budget {
                        break;
                    }
                    let dir = entry.path();
                    if dir.is_dir() {
                        outcome.freed_bytes += dir_bytes(&dir);
                        fs::remove_dir_all(&dir).map_err(RetentionError::Io)?;
                        outcome.deleted_files += 1;
                    }
                }
            }
        }

        // Stage 2: whole-file artifacts, oldest-first, chain heads immune.
        if total_now(roots) > budget {
            let mut candidates: Vec<(u64, PathBuf, bool)> = Vec::new();
            for path in list_files(&roots.sessions_dir()) {
                let id = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string();
                let age = session_age_ms(&path).unwrap_or(u64::MAX);
                candidates.push((age, path, protected.contains(&id)));
            }
            for path in cache_files(&roots.cache_dir)
                .into_iter()
                .chain(log_files(&roots.config_dir))
            {
                let age = mtime_ms(&path).unwrap_or(u64::MAX);
                candidates.push((age, path, false));
            }
            candidates.sort_by(|a, b| a.0.cmp(&b.0));
            for (_, path, is_protected) in candidates {
                if total_now(roots) <= budget {
                    break;
                }
                if is_protected {
                    outcome.protected_chain_heads += 1;
                    continue;
                }
                delete_file(&path, &mut outcome).map_err(RetentionError::Io)?;
            }
        }

        // Stage 3: memory records, globally oldest-first.
        if total_now(roots) > budget {
            let over = total_now(roots) - budget;
            let dropped = evict_oldest_memory_records(roots, over)?;
            outcome.memory_records_dropped += dropped;
        }

        // Final enforcement: fit or typed error.
        let remaining = total_now(roots);
        if remaining > budget {
            return Err(RetentionError::BudgetUnmet {
                budget,
                remaining,
                protected_chain_heads: protected.len(),
            });
        }
    }

    Ok(outcome)
}

/// Evict the globally oldest live memory records until at least
/// `bytes_to_free` of line bytes are removed (atomic per-file rewrites).
/// Returns the number of records dropped.
fn evict_oldest_memory_records(
    roots: &RetentionRoots,
    bytes_to_free: u64,
) -> Result<usize, RetentionError> {
    // Collect (ts, file, id, line_len) for every live record.
    let mut live: Vec<(u64, PathBuf, String, usize)> = Vec::new();
    for path in list_files(&roots.memory_dir()) {
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let raw = fs::read_to_string(&path).map_err(RetentionError::Io)?;
        let mut tombstoned: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut records: Vec<(u64, String, usize)> = Vec::new();
        for line in raw.lines() {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                continue;
            };
            if let Some(id) = value["tombstone"].as_str() {
                tombstoned.insert(id.to_string());
                continue;
            }
            if let Some(id) = value["id"].as_str() {
                records.push((
                    value["timestamp_ms"].as_u64().unwrap_or(0),
                    id.to_string(),
                    line.len() + 1,
                ));
            }
        }
        for (ts, id, len) in records {
            if !tombstoned.contains(&id) {
                live.push((ts, path.clone(), id, len));
            }
        }
    }
    live.sort_by(|a, b| a.0.cmp(&b.0));

    let mut freed: u64 = 0;
    let mut victims: std::collections::HashMap<PathBuf, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    let mut dropped = 0usize;
    for (_, path, id, len) in live {
        if freed >= bytes_to_free {
            break;
        }
        victims.entry(path).or_default().insert(id);
        freed += len as u64;
        dropped += 1;
    }

    for (path, ids) in victims {
        let raw = fs::read_to_string(&path).map_err(RetentionError::Io)?;
        let kept: Vec<&str> = raw
            .lines()
            .filter(|line| {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                    return false; // garbage compacts away
                };
                if let Some(id) = value["id"].as_str() {
                    return !ids.contains(id);
                }
                // Tombstone lines for evicted ids are no longer needed;
                // keep other tombstones.
                if let Some(id) = value["tombstone"].as_str() {
                    return !ids.contains(id);
                }
                true
            })
            .collect();
        let mut body = kept.join("\n");
        if !body.is_empty() {
            body.push('\n');
        }
        crate::core::private_fs::write_atomic_private(&path, body.as_bytes())
            .map_err(io::Error::from)
            .map_err(RetentionError::Io)?;
    }
    Ok(dropped)
}

/// Recursively list REGULAR files under a confined root. Symlinks (files
/// or directories) are never followed — `DirEntry::file_type` does not
/// follow, so symlinked escapes are simply skipped. Bounded depth.
fn walk_regular_files(root: &Path) -> Vec<PathBuf> {
    fn recurse(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if depth > 32 {
            return;
        }
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_symlink() {
                    continue; // confined: never follow
                }
                let path = entry.path();
                if file_type.is_dir() {
                    recurse(&path, depth + 1, out);
                } else if file_type.is_file() {
                    out.push(path);
                }
            }
        }
    }
    let mut out = Vec::new();
    recurse(root, 0, &mut out);
    out.sort();
    out
}

/// Cache files under the confined cache root (recursive, symlink-free).
fn cache_files(cache_dir: &Path) -> Vec<PathBuf> {
    walk_regular_files(cache_dir)
}

/// Copy one artifact with no symlink surface on either side and no
/// check/use window (CP-13 fix2): the SOURCE is opened `O_NOFOLLOW` and
/// must be a regular file by opened-handle metadata; the DESTINATION file
/// is created `O_CREAT|O_EXCL|O_NOFOLLOW` 0600 RELATIVE to a held
/// [`ConfinedDir`] handle, so neither ancestor symlinks nor concurrently
/// swapped components can redirect the write.
#[cfg(unix)]
fn copy_into_confined(
    src: &Path,
    dest_dir: &crate::private_fs::ConfinedDir,
    name: &str,
) -> io::Result<u64> {
    use std::io::Read;

    let mut options = fs::OpenOptions::new();
    options.read(true);
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc_nofollow());
    }
    let mut source = options.open(src)?;
    let meta = source.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::other(format!(
            "refusing to export non-regular file {}",
            src.display()
        )));
    }

    let mut out = dest_dir.create_file(name)?;
    let mut buf = [0u8; 64 * 1024];
    let mut written: u64 = 0;
    loop {
        let n = source.read(&mut buf)?;
        if n == 0 {
            break;
        }
        use std::io::Write;
        out.write_all(&buf[..n])?;
        written += n as u64;
    }
    Ok(written)
}

#[cfg(unix)]
fn libc_nofollow() -> i32 {
    // O_NOFOLLOW without a libc dependency: the constant is stable ABI on
    // the supported unix targets.
    #[cfg(target_os = "linux")]
    {
        0o400000
    }
    #[cfg(not(target_os = "linux"))]
    {
        0x0100
    }
}

/// Production sweep at the current clock.
pub fn sweep(
    roots: &RetentionRoots,
    policy: &RetentionPolicy,
) -> Result<SweepOutcome, RetentionError> {
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

/// Headless export: copy sessions, memory, traces (recursive), and logs
/// to `dest` with no symlink surface, no check/use races, and private
/// modes throughout (CP-13 fix2): every destination directory and file is
/// created RELATIVE to held directory handles beneath the trusted export
/// root — a planted or concurrently swapped ancestor symlink fails the
/// export closed instead of being written through. Unix-only; other
/// platforms fail closed.
#[cfg(unix)]
pub fn export(roots: &RetentionRoots, dest: &Path) -> io::Result<ExportSummary> {
    use crate::private_fs::ConfinedDir;
    let mut summary = ExportSummary { files: 0, bytes: 0 };
    let root = ConfinedDir::create_root(dest)?;

    let flat = [
        (roots.sessions_dir(), "sessions"),
        (roots.memory_dir(), "memory"),
    ];
    for (src, sub) in flat {
        let out = root.child_dir(sub)?;
        for file in list_files(&src) {
            if file
                .symlink_metadata()
                .map(|m| m.is_symlink())
                .unwrap_or(true)
            {
                continue; // source symlinks are never followed
            }
            let Some(name) = file.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            summary.bytes += copy_into_confined(&file, &out, name)?;
            summary.files += 1;
        }
    }

    // Traces: recursive under the confined cache root, preserving relative
    // subpaths through validated components and handle-relative descents.
    let traces_root = root.child_dir("traces")?;
    for file in walk_regular_files(&roots.cache_dir) {
        let rel = file
            .strip_prefix(&roots.cache_dir)
            .map_err(|_| io::Error::other("cache walk escaped its root"))?;
        let rel_str = rel
            .to_str()
            .ok_or_else(|| io::Error::other("non-UTF8 cache path"))?;
        let mut components = crate::private_fs::validated_relative_components(rel_str)?;
        let name = components
            .pop()
            .ok_or_else(|| io::Error::other("empty cache relative path"))?;
        let dir = traces_root.create_dirs(&components)?;
        summary.bytes += copy_into_confined(&file, &dir, &name)?;
        summary.files += 1;
    }

    let logs_out = root.child_dir("logs")?;
    for file in log_files(&roots.config_dir) {
        if file
            .symlink_metadata()
            .map(|m| m.is_symlink())
            .unwrap_or(true)
        {
            continue;
        }
        let Some(name) = file.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        summary.bytes += copy_into_confined(&file, &logs_out, name)?;
        summary.files += 1;
    }
    Ok(summary)
}

/// Non-unix: confined export is unavailable — fail closed.
#[cfg(not(unix))]
pub fn export(_roots: &RetentionRoots, _dest: &Path) -> io::Result<ExportSummary> {
    Err(io::Error::other(
        "confined export requires a unix platform (directory-handle-relative creation)",
    ))
}

/// Headless forget: delete one artifact by domain and id. Session chain
/// heads fail closed; memory ids use `"<namespace>:<record-id>"` and
/// tombstone through the store.
pub fn forget(roots: &RetentionRoots, domain: RetentionDomain, id: &str) -> io::Result<()> {
    match domain {
        RetentionDomain::Sessions => {
            // Sanitize BEFORE any path construction (CP-13 fix1 I5).
            let file = sanitize_file_id(id)?;
            let heads = chain_heads(roots).map_err(|e| io::Error::other(e.to_string()))?;
            if heads.contains(&id.to_string()) {
                return Err(io::Error::other(format!(
                    "session {id} is the head of a named chain — delete or repoint the \
                     chain first (retention never leaves chains dangling)"
                )));
            }
            let path = roots
                .sessions_dir()
                .join(format!("{}.json", file.display()));
            fs::remove_file(path)
        }
        RetentionDomain::Traces => {
            // Nested relative addressing with validated components and
            // dir-handle confinement (CP-13 fix2 moderate): symlinked
            // ancestors inside the cache are refused at open time.
            #[cfg(unix)]
            {
                let mut components = crate::private_fs::validated_relative_components(id)
                    .map_err(|_| io::Error::other(format!("invalid artifact id: {id:?}")))?;
                let name = components
                    .pop()
                    .ok_or_else(|| io::Error::other(format!("invalid artifact id: {id:?}")))?;
                let root = crate::private_fs::ConfinedDir::open_root(&roots.cache_dir)?;
                let dir = root.open_dirs(&components)?;
                dir.remove_file(&name)
            }
            #[cfg(not(unix))]
            {
                fs::remove_file(roots.cache_dir.join(sanitize_file_id(id)?))
            }
        }
        RetentionDomain::Logs => {
            let file = sanitize_file_id(id)?;
            if !id.starts_with("synaps.log") {
                return Err(io::Error::other(
                    "invalid log id: must start with synaps.log",
                ));
            }
            fs::remove_file(roots.config_dir.join(file))
        }
        RetentionDomain::MemoryIndex => {
            let dir = roots.index_dir().join(sanitize_file_id(id)?);
            fs::remove_dir_all(dir)
        }
        RetentionDomain::Memory => {
            let (namespace, record_id) = id.split_once(':').ok_or_else(|| {
                io::Error::other("invalid memory id: use \"<namespace>:<record-id>\"")
            })?;
            let namespace = sanitize_file_id(namespace)?;
            if record_id.is_empty() || !record_id.starts_with("mem-") {
                return Err(io::Error::other(format!(
                    "invalid memory record id: {record_id:?}"
                )));
            }
            let path = roots
                .memory_dir()
                .join(format!("{}.jsonl", namespace.display()));
            let raw = fs::read_to_string(&path)?;

            // EXACT live-id match (CP-13 fix1 M4): parse every line;
            // substring/prefix probes and already-tombstoned ids fail
            // closed.
            let mut live = false;
            let mut tombstoned = false;
            for line in raw.lines() {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                    continue;
                };
                if value["tombstone"].as_str() == Some(record_id) {
                    tombstoned = true;
                } else if value["id"].as_str() == Some(record_id) {
                    live = true;
                }
            }
            if !live || tombstoned {
                return Err(io::Error::other(format!(
                    "record {record_id} not present as a live record in namespace {}",
                    namespace.display()
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
