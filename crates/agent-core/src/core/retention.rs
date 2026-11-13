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
//! - Sessions age by their EMBEDDED `updated_at` (mtime fallback);
//!   traces/logs age by file mtime. Memory records age by their embedded
//!   timestamp against their OWN retention class; the policy's global age
//!   reaches them only with `include_notes`.
//! - Memory files are compacted by atomic rewrite (expired + tombstoned
//!   records physically removed) rather than whole-file deletion; their
//!   derived index directories are dropped alongside (rebuildable state).
//! - The disk budget deletes oldest-first across whole-file artifacts
//!   (sessions/traces/logs/indexes), skipping protected chain heads. It
//!   evicts notes only with `include_notes`; without it an unmet budget is
//!   `BudgetUnmet` with every note untouched.
//! - A budget below what the sweep may not delete (protected chain heads,
//!   and notes without `include_notes`) can never be met: the sweep answers
//!   `BudgetUnmet` before its first deletion, with nothing deleted.

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
    /// The disk budget could not be met (typed instead of a silent
    /// overshoot): either it is below what the sweep may not delete, and
    /// then nothing was deleted (`nothing_deleted`), or it was still unmet
    /// after evicting every unprotected artifact.
    BudgetUnmet {
        budget: u64,
        remaining: u64,
        protected_chain_heads: usize,
        /// The policy did not say `include_notes`, so no note was evicted.
        notes_kept: bool,
        /// What this sweep may not delete, added up before its first
        /// deletion (the `protected_bytes` function).
        protected_bytes: u64,
        /// The budget is below `protected_bytes`: the sweep stopped before
        /// its first deletion, and every file is as it was.
        nothing_deleted: bool,
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
                notes_kept,
                protected_bytes,
                nothing_deleted,
            } => {
                if *nothing_deleted {
                    write!(
                        f,
                        "disk budget unmet: nothing was deleted, because the budget of \
                         {budget} bytes is below the {protected_bytes} bytes this sweep \
                         may not delete ({protected_chain_heads} protected chain head(s)"
                    )?;
                    if *notes_kept {
                        write!(f, " and every saved note")?;
                    }
                    write!(f, "); {remaining} bytes are in use")?;
                } else {
                    write!(
                        f,
                        "disk budget unmet: {remaining} bytes remain against a budget of \
                         {budget} after evicting every unprotected artifact \
                         ({protected_chain_heads} protected chain head(s) retained)"
                    )?;
                }
                if *notes_kept {
                    write!(
                        f,
                        "; no note was deleted: the budget evicts notes only with \
                         --include-notes"
                    )?;
                }
                Ok(())
            }
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
    /// Base dir: memory/ and spill/ (tool-output spill files) live here.
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
    fn spill_dir(&self) -> PathBuf {
        crate::core::tool_spill::spill_root(&self.base_dir)
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
    /// Sessions, traces, logs and spill folders older than this are
    /// deleted. Notes are not, unless `include_notes` is set.
    pub max_age_days: Option<u32>,
    /// Total disk budget. Notes are evicted for it only with `include_notes`.
    pub max_disk_bytes: Option<u64>,
    /// Let the sweep delete saved notes: apply `max_age_days` to notes, and
    /// let `max_disk_bytes` evict notes oldest first. Off by default: a note
    /// is something a person or the model chose to keep. A note with an
    /// expiry of its own (`max_age_days` in its retention class) expires by
    /// that on every sweep, whatever this says.
    pub include_notes: bool,
}

const DAY_MS: u64 = 86_400_000;

/// The end of a notes lock file's name (`memory/<ns>.jsonl.lock`).
#[cfg(unix)]
const NOTES_LOCK_SUFFIX: &str = ".jsonl.lock";

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

/// What a sweep with `policy` may not delete, in bytes as [`inspect`] counts
/// them: every file in `sessions/` that belongs to a protected chain head
/// (its snapshot, journal and turn draft), and, without `include_notes`,
/// what the notes files hold once this sweep has compacted them (forgotten
/// notes, notes past their own expiry and torn lines go on every sweep and
/// are not counted). With `include_notes` notes count as nothing, though
/// their tombstone lines stay: the sum is never more than what must remain,
/// so a budget below it cannot be met. Reads only.
fn protected_bytes(
    roots: &RetentionRoots,
    protected: &[String],
    policy: &RetentionPolicy,
    now_ms: u64,
) -> io::Result<u64> {
    let mut total = 0u64;
    for path in list_files(&roots.sessions_dir()) {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        if protected.iter().any(|id| id == stem) {
            total += fs::metadata(&path)?.len();
        }
    }
    if !policy.include_notes {
        for path in list_files(&roots.memory_dir()) {
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let raw = fs::read(&path)?;
            let (dropped, kept) = compacted_lines(&raw, now_ms, None);
            total += if dropped == 0 {
                raw.len() as u64 // not rewritten: the file as it is
            } else {
                kept.iter().map(|line| line.len() as u64 + 1).sum()
            };
        }
    }
    Ok(total)
}

/// Unified sweep at an explicit clock (tests inject `now_ms`; production
/// uses [`sweep`]). Applies max-age per domain, compacts memory files, then
/// enforces the disk budget oldest-first — never touching chain heads. A
/// budget below what the sweep may not delete ([`protected_bytes`]) is
/// refused before anything is deleted, the age pass included.
pub fn sweep_at(
    roots: &RetentionRoots,
    policy: &RetentionPolicy,
    now_ms: u64,
) -> Result<SweepOutcome, RetentionError> {
    let mut outcome = SweepOutcome::default();
    // Chain protection resolves FIRST and fails closed — nothing is
    // deleted while any chain head is unknown.
    let protected: Vec<String> = chain_heads(roots)?;
    // A budget that cannot be met is refused HERE, before the first
    // deletion: evicting every unprotected session, trace and log on the
    // way to an error would destroy them for nothing.
    let mut may_not_delete = 0;
    if let Some(budget) = policy.max_disk_bytes {
        may_not_delete = protected_bytes(roots, &protected, policy, now_ms)?;
        if budget < may_not_delete {
            return Err(RetentionError::BudgetUnmet {
                budget,
                remaining: inspect(roots)?.total_bytes,
                protected_chain_heads: protected.len(),
                notes_kept: !policy.include_notes,
                protected_bytes: may_not_delete,
                nothing_deleted: true,
            });
        }
    }
    let age_cutoff = policy
        .max_age_days
        .map(|days| now_ms.saturating_sub(days as u64 * DAY_MS));

    // ── Orphan journals (Task 35): a `<id>.journal` whose snapshot is gone
    // is unusable regardless of age — swept on every pass, unless a chain
    // head claims the id (fail-safe keep). ──
    for path in list_files(&roots.sessions_dir()) {
        if path.extension().and_then(|e| e.to_str()) != Some("journal")
            || path.with_extension("json").exists()
        {
            continue;
        }
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        if protected.contains(&id) {
            outcome.protected_chain_heads += 1;
            continue;
        }
        delete_file(&path, &mut outcome)?;
    }

    // ── Age pass: sessions (embedded timestamps; compaction parents are
    // ordinary sessions here) ──
    if let Some(cutoff) = age_cutoff {
        for path in list_files(&roots.sessions_dir()) {
            // Paired journals age WITH their snapshot (Task 35) — never as
            // independent candidates.
            if is_paired_journal(&path) {
                continue;
            }
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
                delete_session_artifact(&path, &mut outcome)?;
                // Its tool-output spill files die with it.
                remove_session_spill(roots, &id, &mut outcome)?;
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

        // ── Tool-output spill dirs past the same cutoff (worker dirs have
        // no session file, so this is what ages them out) ──
        let age = std::time::Duration::from_millis(now_ms.saturating_sub(cutoff));
        let now = std::time::UNIX_EPOCH + std::time::Duration::from_millis(now_ms);
        let swept = crate::core::tool_spill::sweep_stale_spill(&roots.spill_dir(), age, now);
        outcome.deleted_files += swept.files;
        outcome.freed_bytes += swept.bytes;
    }

    // ── Memory: atomic rewrite dropping tombstoned records, records past
    // their OWN retention class, and (only when the policy says
    // `include_notes`) records past the global cutoff. Runs on every sweep
    // so per-record retention holds even without a global max age. ──
    let notes_cutoff = age_cutoff.filter(|_| policy.include_notes);
    for path in list_files(&roots.memory_dir()) {
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let dropped = compact_memory_file(&path, now_ms, notes_cutoff)?;
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
    // (3) memory records oldest-first via atomic rewrite, only with
    // `include_notes`. ──
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
                // Paired journals evict WITH their snapshot (Task 35).
                if is_paired_journal(&path) {
                    continue;
                }
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
            candidates.sort_by_key(|item| item.0);
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

        // Stage 3: memory records, globally oldest-first. Only when asked:
        // without `include_notes` the budget never deletes a note.
        if policy.include_notes && total_now(roots) > budget {
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
                notes_kept: !policy.include_notes,
                protected_bytes: may_not_delete,
                nothing_deleted: false,
            });
        }
    }

    Ok(outcome)
}

/// Evict the globally oldest live memory records until at least
/// `bytes_to_free` of line bytes are removed (atomic per-file rewrites).
/// Returns the number of records dropped. Tombstone lines are kept: they are
/// about 70 bytes each and the budget cannot take them back.
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
        let raw = fs::read(&path).map_err(RetentionError::Io)?;
        let mut tombstoned: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut records: Vec<(u64, String, usize)> = Vec::new();
        for line in notes_lines(&raw).flatten() {
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
    live.sort_by_key(|item| item.0);

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
        // Held from this read through the rename. The pass above looked
        // without it: this one decides by id on what it reads here.
        let _lock = lock_notes(&path)?;
        let raw = fs::read(&path).map_err(RetentionError::Io)?;
        rewrite_read_done();
        let kept: Vec<&str> = notes_lines(&raw)
            // A line that is not UTF-8 goes the way of any line that does
            // not parse.
            .flatten()
            .filter(|line| {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                    return false; // garbage compacts away
                };
                if let Some(id) = value["id"].as_str() {
                    return !ids.contains(id);
                }
                // Every tombstone line stays, whatever id it names: it is
                // what keeps a forgotten note from being imported again.
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

/// The lines of a notes file, read as bytes: `None` for a line that is not
/// UTF-8. A tail torn inside a multi-byte character stays in the file as
/// such a line (a later save fences it off with a newline, nothing removes
/// it), and `read_to_string` would fail on the whole file because of it, on
/// every run. Like `str::lines`, without the line ends and without an empty
/// piece after the last newline.
fn notes_lines(raw: &[u8]) -> impl Iterator<Item = Option<&str>> {
    let empty = raw.is_empty();
    let raw = raw.strip_suffix(b"\n").unwrap_or(raw);
    raw.split(|byte| *byte == b'\n')
        .filter(move |_| !empty)
        .map(|line| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            std::str::from_utf8(line).ok()
        })
}

/// The lock every appender and rewriter of a notes file holds
/// (`memory::store::lock_notes_file`). An error when it is not had in time:
/// the caller does not rewrite.
fn lock_notes(path: &Path) -> io::Result<crate::core::lock_file::ExclusiveLock> {
    crate::memory::store::lock_notes_file(path)
}

#[cfg(test)]
thread_local! {
    /// Run by this thread's rewriters once they have read the file, lock
    /// held, before they write: where a test puts a save in their way.
    static AFTER_REWRITE_READ: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn rewrite_read_done() {
    let hook = AFTER_REWRITE_READ.with(|hook| hook.borrow_mut().take());
    if let Some(mut hook) = hook {
        hook();
    }
}

#[cfg(not(test))]
fn rewrite_read_done() {}

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

/// Files that belong to a session snapshot `<id>.json` and live and die
/// with it: the Task 35 journal and the in-flight turn draft
/// (`session_draft`, `<id>.turn`).
const PAIRED_SESSION_EXTENSIONS: [&str; 2] = ["journal", "turn"];

/// True when `path` is a journal or turn draft whose snapshot sibling still
/// exists — such a file is handled WITH its `.json` artifact, never as an
/// independent retention candidate.
fn is_paired_journal(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| PAIRED_SESSION_EXTENSIONS.contains(&e))
        && path.with_extension("json").exists()
}

/// Remove session `id`'s tool-output spill dir (`<base>/spill/<id>/`).
fn remove_session_spill(
    roots: &RetentionRoots,
    id: &str,
    outcome: &mut SweepOutcome,
) -> io::Result<()> {
    let freed = crate::core::tool_spill::remove_session_spill(&roots.spill_dir(), id)?;
    outcome.deleted_files += freed.files;
    outcome.freed_bytes += freed.bytes;
    Ok(())
}

/// Delete a session artifact: the file plus, for `.json` snapshots, any
/// sibling journal and turn draft (Task 35 — a session and its journal die
/// together, so a sweep can never orphan journal state).
fn delete_session_artifact(path: &Path, outcome: &mut SweepOutcome) -> io::Result<()> {
    delete_file(path, outcome)?;
    if path.extension().and_then(|e| e.to_str()) == Some("json") {
        for ext in PAIRED_SESSION_EXTENSIONS {
            let paired = path.with_extension(ext);
            if paired.exists() {
                delete_file(&paired, outcome)?;
            }
        }
    }
    Ok(())
}

/// Rewrite one memory JSONL file, dropping tombstoned records, records past
/// their own retention class (`ts + max_age_days < now`), and — when a
/// global cutoff is set — records older than it. Atomic rewrite; returns
/// the number of records physically removed. Tombstone lines are kept, in
/// place, on every rewrite.
///
/// A line that does not parse is dropped and counted too, and so is a line
/// that is not UTF-8 (a tail torn inside a character). The file's lock is
/// held here, so such a line is never a save caught half written: it is a
/// fragment a writer left behind (a torn tail that a later save fenced off).
fn compact_memory_file(
    path: &Path,
    now_ms: u64,
    global_cutoff_ms: Option<u64>,
) -> io::Result<usize> {
    // Held from the read through the rename, so no save lands in the file
    // this replaces, and no line is read half written.
    let _lock = lock_notes(path)?;
    let raw = fs::read(path)?;
    rewrite_read_done();
    let (dropped, kept) = compacted_lines(&raw, now_ms, global_cutoff_ms);
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

/// What [`compact_memory_file`] makes of a notes file's bytes: how many
/// lines it drops, and the lines it keeps, in order. The one rule of what a
/// compaction keeps, for the rewrite and for [`protected_bytes`].
fn compacted_lines(
    raw: &[u8],
    now_ms: u64,
    global_cutoff_ms: Option<u64>,
) -> (usize, Vec<String>) {
    /// A line of the file: a tombstone, or a record with what decides
    /// whether it stays.
    enum Line {
        Tombstone(String),
        Record(Option<String>, u64, Option<u64>, String),
    }
    let mut tombstoned: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut lines: Vec<Line> = Vec::new();
    let mut dropped = 0usize;

    for line in notes_lines(raw) {
        let Some(line) = line else {
            dropped += 1; // not UTF-8: a torn line, dropped like one
            continue;
        };
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
            lines.push(Line::Tombstone(trimmed.to_string()));
            continue;
        }
        let ts = value["timestamp_ms"].as_u64().unwrap_or(0);
        let per_record_span_ms = value["retention"]["max_age_days"]
            .as_u64()
            .map(|days| days * DAY_MS);
        lines.push(Line::Record(
            value["id"].as_str().map(String::from),
            ts,
            per_record_span_ms,
            trimmed.to_string(),
        ));
    }

    let mut kept: Vec<String> = Vec::new();
    for line in lines {
        let (id, ts, per_record_span_ms, line) = match line {
            // A tombstone line is never dropped and never counted: it stays
            // where it was, so a forgotten note stays forgotten when its
            // line is gone (an import checks for the tombstone), and a file
            // with nothing else to drop is not rewritten.
            Line::Tombstone(line) => {
                kept.push(line);
                continue;
            }
            Line::Record(id, ts, span, line) => (id, ts, span, line),
        };
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
    (dropped, kept)
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

    // Flat domains: discovery AND authoritative opens are relative to the
    // same held trusted source-root handle. There is no full-path source
    // reopen after discovery. A planted/swap-raced source entry is refused
    // by open_file; a destination collision remains a hard error.
    let flat = [
        (roots.sessions_dir(), "sessions"),
        (roots.memory_dir(), "memory"),
    ];
    for (src, sub) in flat {
        let Ok(src_root) = ConfinedDir::open_root(&src) else {
            continue; // absent domain — nothing to export
        };
        let out = root.child_dir(sub)?;
        for entry in src_root.entries()? {
            if !entry.is_file {
                continue; // source symlink/non-regular entries are omitted
            }
            if sub == "memory" && entry.name.ends_with(NOTES_LOCK_SUFFIX) {
                continue; // a notes file's lock, not data
            }
            let components = [entry.name.clone()];
            match src_root.open_file(&components) {
                Ok(source) => {
                    let bytes = copy_opened_into_confined(source, &out, &entry.name)?;
                    summary.bytes += bytes;
                    summary.files += 1;
                }
                // Entry changed after enumeration (including a swap to a
                // symlink): refuse/skip it, never follow it.
                Err(_) => continue,
            }
        }
    }

    // Traces: recursively walk entirely through source directory handles.
    if let Ok(cache_root) = ConfinedDir::open_root(&roots.cache_dir) {
        let traces_root = root.child_dir("traces")?;
        export_tree_from_handles(&cache_root, &traces_root, &mut summary, 0)?;
    }

    // Logs: enumerate the config root through its handle, filter the
    // handle-returned names, and open relative to the same handle.
    if let Ok(logs_root) = ConfinedDir::open_root(&roots.config_dir) {
        let logs_out = root.child_dir("logs")?;
        for entry in logs_root.entries()? {
            if !entry.is_file || !entry.name.starts_with("synaps.log") {
                continue;
            }
            match logs_root.open_file(std::slice::from_ref(&entry.name)) {
                Ok(source) => {
                    let bytes = copy_opened_into_confined(source, &logs_out, &entry.name)?;
                    summary.bytes += bytes;
                    summary.files += 1;
                }
                Err(_) => continue,
            }
        }
    }
    Ok(summary)
}

/// Copy an ALREADY-CONFINED opened regular source handle into a destination
/// created relative to a held destination directory handle.
#[cfg(unix)]
fn copy_opened_into_confined(
    mut source: fs::File,
    dest_dir: &crate::private_fs::ConfinedDir,
    name: &str,
) -> io::Result<u64> {
    use std::io::{Read, Write};
    if !source.metadata()?.is_file() {
        return Err(io::Error::other("confined export source is not regular"));
    }
    let mut out = dest_dir.create_file(name)?;
    let mut buf = [0u8; 64 * 1024];
    let mut written = 0u64;
    loop {
        let n = source.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        written += n as u64;
    }
    Ok(written)
}

/// Recursive source traversal using held source and destination directory
/// handles only. Symlink entries are never opened; a directory swapped after
/// enumeration fails child `open_dirs` and is skipped. Bounded depth.
#[cfg(unix)]
fn export_tree_from_handles(
    source: &crate::private_fs::ConfinedDir,
    destination: &crate::private_fs::ConfinedDir,
    summary: &mut ExportSummary,
    depth: usize,
) -> io::Result<()> {
    if depth > 32 {
        return Err(io::Error::other("confined export source depth exceeds 32"));
    }
    for entry in source.entries()? {
        if entry.is_file {
            match source.open_file(std::slice::from_ref(&entry.name)) {
                Ok(opened) => {
                    let bytes = copy_opened_into_confined(opened, destination, &entry.name)?;
                    summary.bytes += bytes;
                    summary.files += 1;
                }
                Err(_) => continue,
            }
        } else if entry.is_dir {
            let source_child = match source.open_dirs(std::slice::from_ref(&entry.name)) {
                Ok(child) => child,
                Err(_) => continue,
            };
            let dest_child = destination.child_dir(&entry.name)?;
            export_tree_from_handles(&source_child, &dest_child, summary, depth + 1)?;
        }
    }
    Ok(())
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
            fs::remove_file(path)?;
            // Pair-delete the opt-in journal (Task 35) and any turn draft —
            // forgetting a session must not leave replayable state behind.
            for ext in PAIRED_SESSION_EXTENSIONS {
                let paired = roots
                    .sessions_dir()
                    .join(format!("{}.{ext}", file.display()));
                match fs::remove_file(paired) {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    other => other?,
                }
            }
            // And its tool-output spill files.
            crate::core::tool_spill::remove_session_spill(&roots.spill_dir(), id)?;
            Ok(())
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
            // The store's own check of an id, not the shape of the ids it
            // makes: an imported note's id need not start with `mem-`.
            if !crate::memory::store::valid_import_id(record_id) {
                return Err(io::Error::other(format!(
                    "invalid memory record id: {record_id:?}"
                )));
            }
            let path = roots
                .memory_dir()
                .join(format!("{}.jsonl", namespace.display()));
            // No file, no lock file: fail before one is created.
            fs::metadata(&path)?;
            // Held from the read through the rename.
            let _lock = lock_notes(&path)?;
            let raw = fs::read(&path)?;
            rewrite_read_done();

            // EXACT live-id match (CP-13 fix1 M4): parse every line;
            // substring/prefix probes and already-tombstoned ids fail
            // closed.
            let mut live = false;
            let mut tombstoned = false;
            for line in notes_lines(&raw).flatten() {
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
            // Every byte that was read is written back, a line that is not
            // UTF-8 included: a forget removes nothing.
            let mut body = raw;
            if !body.ends_with(b"\n") && !body.is_empty() {
                body.push(b'\n');
            }
            body.extend_from_slice(line.to_string().as_bytes());
            body.push(b'\n');
            crate::core::private_fs::write_atomic_private(&path, &body)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::store::{self, MemoryRecord};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;
    use std::time::Duration;
    use tempfile::TempDir;

    fn roots(tmp: &TempDir) -> RetentionRoots {
        RetentionRoots {
            config_dir: tmp.path().join("config"),
            base_dir: tmp.path().join("base"),
            cache_dir: tmp.path().join("cache"),
        }
    }

    fn note(ns: &str, id: &str, timestamp_ms: u64) -> MemoryRecord {
        let mut record = store::new_record(ns, format!("content of {id}"), vec![], None);
        record.id = Some(id.to_string());
        record.timestamp_ms = timestamp_ms;
        record
    }

    fn save(roots: &RetentionRoots, ns: &str, id: &str, timestamp_ms: u64) {
        store::append_to(&roots.base_dir, &note(ns, id, timestamp_ms)).unwrap();
    }

    fn notes_file(roots: &RetentionRoots, ns: &str) -> PathBuf {
        roots.memory_dir().join(format!("{ns}.jsonl"))
    }

    /// The ids of the note lines in a file, in order (tombstones left out).
    fn ids(path: &Path) -> Vec<String> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter_map(|value| value["id"].as_str().map(String::from))
            .collect()
    }

    /// An age sweep that takes notes too.
    const AGE: RetentionPolicy = RetentionPolicy {
        max_age_days: Some(1),
        max_disk_bytes: None,
        include_notes: true,
    };

    /// Put a save of note `id` in the way of this thread's next rewrite: it
    /// starts once the rewriter has read the file and before it writes. The
    /// rewriter is held there for a moment, and the save must still be
    /// waiting. Returns where the save's thread is left.
    fn save_during_next_rewrite(
        roots: &RetentionRoots,
        ns: &'static str,
        id: &'static str,
    ) -> Arc<Mutex<Option<JoinHandle<()>>>> {
        let slot = Arc::new(Mutex::new(None));
        let (left, roots, path) = (slot.clone(), roots.clone(), notes_file(roots, ns));
        AFTER_REWRITE_READ.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                let saver = {
                    let roots = roots.clone();
                    std::thread::spawn(move || save(&roots, ns, id, crate::epoch_millis()))
                };
                std::thread::sleep(Duration::from_millis(150));
                assert!(!saver.is_finished(), "the save waits for the rewrite");
                assert!(!ids(&path).contains(&id.to_string()));
                *left.lock().unwrap() = Some(saver);
            }));
        });
        slot
    }

    fn join_save(slot: &Arc<Mutex<Option<JoinHandle<()>>>>) {
        let saver = slot.lock().unwrap().take();
        saver.expect("the rewriter read the file").join().unwrap();
    }

    /// Each of the three rewriters holds the lock from its read through its
    /// rename: a save that arrives in between waits, and its note is in the
    /// file the rewrite left. Without the lock the save would go into the
    /// file the rename replaces, and be lost after it returned Ok.
    #[test]
    fn a_save_waits_for_a_rewrite_and_its_note_is_in_the_file_after_it() {
        let now = crate::epoch_millis();

        // The sweep.
        let tmp = TempDir::new().unwrap();
        let roots = roots(&tmp);
        save(&roots, "ns", "mem-old", 1);
        save(&roots, "ns", "mem-new", now);
        let late = save_during_next_rewrite(&roots, "ns", "mem-late");
        let outcome = sweep_at(&roots, &AGE, now).unwrap();
        join_save(&late);
        assert_eq!(outcome.memory_records_dropped, 1);
        assert_eq!(ids(&notes_file(&roots, "ns")), ["mem-new", "mem-late"]);

        // Eviction.
        let tmp = TempDir::new().unwrap();
        let roots = self::roots(&tmp);
        save(&roots, "ns", "mem-old", 1);
        save(&roots, "ns", "mem-new", 2);
        let late = save_during_next_rewrite(&roots, "ns", "mem-late");
        assert_eq!(evict_oldest_memory_records(&roots, 1).unwrap(), 1);
        join_save(&late);
        assert_eq!(ids(&notes_file(&roots, "ns")), ["mem-new", "mem-late"]);

        // `retention forget memory`.
        let tmp = TempDir::new().unwrap();
        let roots = self::roots(&tmp);
        save(&roots, "ns", "mem-a", now);
        let late = save_during_next_rewrite(&roots, "ns", "mem-late");
        forget(&roots, RetentionDomain::Memory, "ns:mem-a").unwrap();
        join_save(&late);
        let path = notes_file(&roots, "ns");
        assert_eq!(ids(&path), ["mem-a", "mem-late"]);
        assert!(fs::read_to_string(&path)
            .unwrap()
            .contains(r#""tombstone":"mem-a""#));
    }

    /// Savers on four threads against a thread that rewrites the same file
    /// over and over (a forget, then a sweep that has something to drop):
    /// every save that returned Ok is in the file at the end, once.
    #[test]
    fn saves_beside_repeated_rewrites_lose_no_note() {
        const SAVERS: usize = 4;
        const SAVES: usize = 40;
        let tmp = TempDir::new().unwrap();
        let roots = roots(&tmp);
        save(&roots, "ns", "mem-first", crate::epoch_millis());
        let done = Arc::new(AtomicBool::new(false));

        let rewriter = {
            let (roots, done) = (roots.clone(), done.clone());
            std::thread::spawn(move || {
                let mut rewrites = 0usize;
                let mut round = 0usize;
                while !done.load(Ordering::Acquire) || round < 3 {
                    let now = crate::epoch_millis();
                    let victim = format!("mem-victim-{round}");
                    save(&roots, "ns", &victim, now);
                    save(&roots, "ns", &format!("mem-aged-{round}"), 1);
                    forget(&roots, RetentionDomain::Memory, &format!("ns:{victim}")).unwrap();
                    let outcome = sweep_at(&roots, &AGE, now).unwrap();
                    assert_eq!(outcome.memory_records_dropped, 2, "round {round}");
                    rewrites += 2;
                    round += 1;
                }
                rewrites
            })
        };
        let savers: Vec<_> = (0..SAVERS)
            .map(|saver| {
                let roots = roots.clone();
                std::thread::spawn(move || {
                    for n in 0..SAVES {
                        save(&roots, "ns", &format!("mem-{saver}-{n}"), crate::epoch_millis());
                        std::thread::sleep(Duration::from_millis(1));
                    }
                })
            })
            .collect();
        for saver in savers {
            saver.join().unwrap();
        }
        done.store(true, Ordering::Release);
        assert!(rewriter.join().unwrap() >= 6);

        let mut found = ids(&notes_file(&roots, "ns"));
        found.sort();
        let mut expected: Vec<String> = (0..SAVERS)
            .flat_map(|saver| (0..SAVES).map(move |n| format!("mem-{saver}-{n}")))
            .collect();
        expected.push("mem-first".into());
        expected.sort();
        assert_eq!(found, expected);
    }

    /// A rewriter that cannot get the lock in time fails and leaves the file
    /// as it was.
    #[test]
    fn a_rewrite_that_cannot_get_the_lock_fails_and_rewrites_nothing() {
        let tmp = TempDir::new().unwrap();
        let roots = roots(&tmp);
        let now = crate::epoch_millis();
        save(&roots, "ns", "mem-old", 1);
        save(&roots, "ns", "mem-new", now);
        let path = notes_file(&roots, "ns");
        let before = fs::read(&path).unwrap();

        let held = store::lock_notes_file(&path).unwrap();
        store::set_notes_lock_wait(Some(Duration::from_millis(60)));
        let swept = sweep_at(&roots, &AGE, now).unwrap_err();
        let evicted = evict_oldest_memory_records(&roots, 1).unwrap_err();
        let forgotten = forget(&roots, RetentionDomain::Memory, "ns:mem-new").unwrap_err();
        store::set_notes_lock_wait(None);
        for message in [swept.to_string(), evicted.to_string(), forgotten.to_string()] {
            assert!(message.contains("ns.jsonl is locked by another writer"), "{message}");
        }
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(!path.with_extension("jsonl.tmp").exists());

        // Released, each of them goes through.
        drop(held);
        assert_eq!(sweep_at(&roots, &AGE, now).unwrap().memory_records_dropped, 1);
        forget(&roots, RetentionDomain::Memory, "ns:mem-new").unwrap();
    }

    /// A forgotten note stays forgotten across sweep, eviction and forget:
    /// each of the three rewrites the file, the tombstone line is still in
    /// it, and an import of the note's own line is turned away by it.
    #[test]
    fn a_tombstone_outlives_every_rewrite() {
        use crate::memory::store::{
            import_line_in, ImportOutcome, ImportRun, ImportTarget, MemoryProvenance,
            MemoryRetention, MemorySensitivity, NewMemoryRecord, ProjectScope,
        };
        let tmp = TempDir::new().unwrap();
        let roots = roots(&tmp);
        let project = tmp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let scope = ProjectScope::for_root(&project).unwrap();
        let ns = scope.namespace();
        let path = notes_file(&roots, &ns);
        let stored = |content: &str| {
            let new = NewMemoryRecord {
                content: content.to_string(),
                tags: vec![],
                provenance: MemoryProvenance {
                    source: "model".into(),
                    session: None,
                },
                sensitivity: MemorySensitivity::Normal,
                retention: MemoryRetention::Standard,
            };
            let record = store::store_record_in(&roots.base_dir, &scope, new).unwrap();
            std::thread::sleep(Duration::from_millis(2)); // distinct times
            record.id.unwrap()
        };
        let inode = || {
            #[cfg(unix)]
            {
                std::os::unix::fs::MetadataExt::ino(&fs::metadata(&path).unwrap())
            }
            #[cfg(not(unix))]
            {
                0u64
            }
        };

        let forgotten = stored("forgotten");
        let exported = fs::read_to_string(&path).unwrap().trim().to_string();
        let (second, third, fourth) = (stored("second"), stored("third"), stored("fourth"));
        store::forget_in(&roots.base_dir, &scope, &forgotten).unwrap();
        let tombstone = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .last()
            .unwrap()
            .to_string();
        assert!(tombstone.contains(&format!(r#""tombstone":"{forgotten}""#)));

        // What the import's check (`import_note`, "tombstoned" in the file)
        // says about the forgotten note's own line, and what is in the file.
        let check = |live: &[&String]| {
            let text = fs::read_to_string(&path).unwrap();
            assert_eq!(text.lines().filter(|line| *line == tombstone).count(), 1);
            let live: Vec<String> = live.iter().map(|id| id.to_string()).collect();
            assert_eq!(ids(&path), live);
            let imported = import_line_in(
                &roots.base_dir,
                &exported,
                ImportTarget::AsRecorded,
                &mut ImportRun::new(false),
            )
            .unwrap();
            assert_eq!(imported.outcome.name(), ImportOutcome::KeptForgotten.name());
            assert_eq!(fs::read_to_string(&path).unwrap(), text, "nothing imported");
        };

        // The sweep: it drops the forgotten note's line, so it rewrites.
        // (Inode checks are unix-only: elsewhere `inode()` is always 0.)
        #[cfg(unix)]
        let before = inode();
        let now = crate::epoch_millis();
        let none = RetentionPolicy::default();
        assert_eq!(sweep_at(&roots, &none, now).unwrap().memory_records_dropped, 1);
        #[cfg(unix)]
        assert_ne!(inode(), before, "the sweep rewrote the file");
        check(&[&second, &third, &fourth]);

        // A second sweep: tombstones and live notes only, nothing to drop,
        // so the file is left as it is.
        let before = inode();
        assert_eq!(sweep_at(&roots, &none, now).unwrap().memory_records_dropped, 0);
        assert_eq!(inode(), before, "a sweep with nothing to drop does not rewrite");
        check(&[&second, &third, &fourth]);

        // Eviction of the oldest live note.
        assert_eq!(evict_oldest_memory_records(&roots, 1).unwrap(), 1);
        #[cfg(unix)]
        assert_ne!(inode(), before, "the eviction rewrote the file");
        check(&[&third, &fourth]);

        // The store's forget (an append), then `retention forget memory`
        // (a rewrite): the file now has three tombstones.
        store::forget_in(&roots.base_dir, &scope, &third).unwrap();
        check(&[&third, &fourth]);
        #[cfg(unix)]
        let before = inode();
        forget(&roots, RetentionDomain::Memory, &format!("{ns}:{fourth}")).unwrap();
        #[cfg(unix)]
        assert_ne!(inode(), before, "the forget rewrote the file");
        check(&[&third, &fourth]);
        let text = fs::read_to_string(&path).unwrap();
        for id in [&forgotten, &third, &fourth] {
            assert!(text.contains(&format!(r#""tombstone":"{id}""#)), "{id}");
        }

        // And the sweep after that drops both forgotten notes and keeps all
        // three tombstones.
        assert_eq!(sweep_at(&roots, &none, now).unwrap().memory_records_dropped, 2);
        check(&[]);
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert_eq!(text.matches(r#""tombstone":"#).count(), 3);
    }

    /// A notes file with a line that is not UTF-8 (a tail torn inside a
    /// character, fenced off by the next save) is still read by the sweep,
    /// the eviction and `retention forget memory`. The forget keeps the
    /// line's bytes; the two that compact drop it as they drop any line
    /// that does not parse, and the sweep counts it.
    #[test]
    fn a_line_that_is_not_utf8_does_not_stop_a_rewriter() {
        let torn = b"{\"namespace\":\"ns\",\"content\":\"caf\xc3";
        let file_with_fragment = |roots: &RetentionRoots| {
            save(roots, "ns", "mem-a", 10);
            let path = notes_file(roots, "ns");
            let mut bytes = fs::read(&path).unwrap();
            bytes.extend_from_slice(torn);
            fs::write(&path, bytes).unwrap();
            // The next saves fence the fragment off on a line of its own.
            save(roots, "ns", "mem-b", 20);
            save(roots, "ns", "mem-c", crate::epoch_millis());
            assert!(fs::read_to_string(&path).is_err(), "the file is not UTF-8");
            path
        };
        let has_fragment = |path: &Path| {
            let bytes = fs::read(path).unwrap();
            bytes.windows(torn.len()).any(|window| window == torn)
        };

        // `retention forget memory`: the note is forgotten, no byte is lost.
        let tmp = TempDir::new().unwrap();
        let roots = roots(&tmp);
        let path = file_with_fragment(&roots);
        let before = fs::read(&path).unwrap();
        forget(&roots, RetentionDomain::Memory, "ns:mem-b").unwrap();
        let after = fs::read(&path).unwrap();
        assert!(after.starts_with(&before));
        assert!(has_fragment(&path));
        let added = std::str::from_utf8(&after[before.len()..]).unwrap();
        assert!(added.contains(r#""tombstone":"mem-b""#) && added.ends_with('\n'));
        assert_eq!(added.lines().count(), 1);

        // The sweep: it reads the file, drops the forgotten note and the
        // fragment, counts both, and leaves a file that is UTF-8.
        let outcome = sweep_at(&roots, &RetentionPolicy::default(), 30).unwrap();
        assert_eq!(outcome.memory_records_dropped, 2);
        assert!(!has_fragment(&path));
        assert_eq!(ids(&path), ["mem-a", "mem-c"]);
        assert!(fs::read_to_string(&path).unwrap().contains(r#""tombstone":"mem-b""#));

        // Eviction: both passes read the file; the rewrite drops the oldest
        // note and the fragment.
        let tmp = TempDir::new().unwrap();
        let roots = self::roots(&tmp);
        let path = file_with_fragment(&roots);
        assert_eq!(evict_oldest_memory_records(&roots, 1).unwrap(), 1);
        assert!(!has_fragment(&path));
        assert_eq!(ids(&path), ["mem-b", "mem-c"]);

        // Lines as `str::lines` gives them, but for the one that is not text.
        let lines: Vec<_> = notes_lines(b"a\r\n\n\xff\nb").collect();
        assert_eq!(lines, [Some("a"), Some(""), None, Some("b")]);
        assert_eq!(notes_lines(b"").count(), 0);
        assert_eq!(notes_lines(b"a\n").collect::<Vec<_>>(), [Some("a")]);
        assert_eq!(notes_lines(b"\n").collect::<Vec<_>>(), [Some("")]);
    }

    /// An age sweep is for sessions, traces and logs. A `Standard` note
    /// survives it however old it is; a note with an expiry of its own goes
    /// when that is past; `include_notes` makes the age apply to notes; and
    /// the disk budget evicts notes only with `include_notes`.
    #[test]
    fn an_age_sweep_keeps_saved_notes_unless_it_is_told_to_include_them() {
        use crate::memory::store::MemoryRetention;
        let tmp = TempDir::new().unwrap();
        let roots = roots(&tmp);
        let now = 1_000 * DAY_MS;
        let with_class = |id: &str, timestamp_ms: u64, class: MemoryRetention| {
            let mut record = note("ns", id, timestamp_ms);
            record.retention = Some(class);
            store::append_to(&roots.base_dir, &record).unwrap();
        };
        with_class("mem-standard-old", DAY_MS, MemoryRetention::Standard);
        save(&roots, "ns", "mem-classless-old", 2 * DAY_MS);
        with_class("mem-expired", 900 * DAY_MS, MemoryRetention::MaxAgeDays(30));
        with_class("mem-not-expired", 990 * DAY_MS, MemoryRetention::MaxAgeDays(30));
        with_class("mem-standard-new", 999 * DAY_MS, MemoryRetention::Standard);
        let path = notes_file(&roots, "ns");
        let age = |include_notes| RetentionPolicy {
            max_age_days: Some(5),
            max_disk_bytes: None,
            include_notes,
        };

        let outcome = sweep_at(&roots, &age(false), now).unwrap();
        assert_eq!(outcome.memory_records_dropped, 1, "the note past its own expiry");
        assert_eq!(
            ids(&path),
            ["mem-standard-old", "mem-classless-old", "mem-not-expired", "mem-standard-new"]
        );
        // Again: nothing to drop.
        assert_eq!(sweep_at(&roots, &age(false), now).unwrap().memory_records_dropped, 0);

        // `include_notes`: every note older than the age goes, whatever its
        // class (mem-not-expired is 10 days old, inside its own 30).
        let outcome = sweep_at(&roots, &age(true), now).unwrap();
        assert_eq!(outcome.memory_records_dropped, 3);
        assert_eq!(ids(&path), ["mem-standard-new"]);

        // `include_notes` without an age is no cutoff.
        let none = RetentionPolicy {
            include_notes: true,
            ..Default::default()
        };
        assert_eq!(sweep_at(&roots, &none, now).unwrap().memory_records_dropped, 0);

        // The disk budget evicts notes, oldest first, only with `include_notes`.
        with_class("mem-standard-newer", now, MemoryRetention::Standard);
        let total = inspect(&roots).unwrap().total_bytes;
        let budget = |include_notes| RetentionPolicy {
            max_disk_bytes: Some(total - 1),
            include_notes,
            ..Default::default()
        };
        assert!(matches!(
            sweep_at(&roots, &budget(false), now),
            Err(RetentionError::BudgetUnmet { notes_kept: true, .. })
        ));
        assert_eq!(ids(&path), ["mem-standard-new", "mem-standard-newer"]);
        assert_eq!(sweep_at(&roots, &budget(true), now).unwrap().memory_records_dropped, 1);
        assert_eq!(ids(&path), ["mem-standard-newer"]);
    }

    /// `retention forget memory` takes any id the store would take, not only
    /// one that starts with `mem-`: an imported note keeps the id it came
    /// with. The match is still exact, and an id the store would not take is
    /// still turned away before the file is read.
    #[test]
    fn a_note_with_an_imported_id_can_be_forgotten_from_the_command_line() {
        let tmp = TempDir::new().unwrap();
        let roots = roots(&tmp);
        for id in ["note:2024/a", "note:2024/ab", "mem-x"] {
            save(&roots, "ns", id, 5);
        }
        let path = notes_file(&roots, "ns");
        forget(&roots, RetentionDomain::Memory, "ns:note:2024/a").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("tombstone").count(), 1);
        assert!(text.contains(r#""tombstone":"note:2024/a""#));
        let again = forget(&roots, RetentionDomain::Memory, "ns:note:2024/a").unwrap_err();
        assert!(again.to_string().contains("not present as a live record"), "{again}");
        let part = forget(&roots, RetentionDomain::Memory, "ns:note:2024").unwrap_err();
        assert!(part.to_string().contains("not present as a live record"), "{part}");

        let before = fs::read(&path).unwrap();
        let long = format!("ns:{}", "i".repeat(129));
        for bad in ["ns:", "ns:has space", "ns:tab\there", long.as_str()] {
            let error = forget(&roots, RetentionDomain::Memory, bad).unwrap_err();
            assert!(error.to_string().starts_with("invalid memory record id"), "{error}");
        }
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    /// The lock file sits in `memory/` beside the notes file and is not one:
    /// it is not listed as a namespace, not counted, not swept and not
    /// exported, and a sweep leaves it where it is.
    #[test]
    fn the_lock_file_is_not_a_notes_file() {
        let tmp = TempDir::new().unwrap();
        let roots = roots(&tmp);
        let now = crate::epoch_millis();
        save(&roots, "ns", "mem-old", 1);
        save(&roots, "ns", "mem-new", now);
        let path = notes_file(&roots, "ns");
        let lock = roots.memory_dir().join("ns.jsonl.lock");
        assert_eq!(store::notes_lock_path(&path), lock);
        assert!(lock.is_file());

        assert_eq!(store::list_namespaces_in(&roots.base_dir).unwrap(), ["ns"]);
        let counted = |roots: &RetentionRoots| {
            let report = inspect(roots).unwrap();
            let memory = report
                .domains
                .iter()
                .find(|d| d.domain == RetentionDomain::Memory)
                .unwrap();
            (memory.files, memory.bytes)
        };
        assert_eq!(counted(&roots), (1, fs::metadata(&path).unwrap().len()));

        // A sweep that rewrites the notes file, and an eviction after it.
        assert_eq!(sweep_at(&roots, &AGE, now).unwrap().memory_records_dropped, 1);
        assert_eq!(evict_oldest_memory_records(&roots, 1).unwrap(), 1);
        assert!(lock.is_file(), "the lock file is never removed or renamed");
        assert_eq!(fs::metadata(&lock).unwrap().len(), 0);
        assert_eq!(counted(&roots), (1, 0));
        assert_eq!(store::list_namespaces_in(&roots.base_dir).unwrap(), ["ns"]);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&lock).unwrap().permissions().mode() & 0o777, 0o600);
            let dest = tmp.path().join("export");
            let summary = export(&roots, &dest).unwrap();
            assert_eq!(summary.files, 1);
            let exported: Vec<_> = fs::read_dir(dest.join("memory"))
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(exported, ["ns.jsonl"]);
        }
    }
}
