//! Past-sessions catalog: the UI-free engine behind the `/sessions` Past tab.
//!
//! Everything here is synchronous and pure over injected paths, so callers
//! run it on `spawn_blocking` (or a plain thread) and never on a draw loop:
//!
//! * [`load_catalog`] — every session header joined with `index.jsonl`
//!   (cwd / profile / turns / duration). Headers only, never message history.
//! * [`PreviewCache`] — sidecar cache of first/last user message + true
//!   message count, keyed by id + (mtime, bytes), refreshed incrementally.
//! * [`Query`] / [`parse_query`] — free text + power tokens.
//! * [`build_view`] — noise hiding, facets with counts, sorting; plus
//!   [`day_groups`] for the day headers.
//! * [`deep_search`] — cancellable streaming full-text scan of bodies.
//! * [`rename_past_session`] — rename under the session's own `.lock`.
//!
//! Nothing in this module ever deletes a session (owner decision).

mod body;
mod deep;
mod preview;
mod query;
mod rename;
mod time;
mod view;

#[cfg(test)]
mod tests;

pub use deep::{
    deep_search, DeepEvent, DeepHit, DeepRole, DeepSearchOptions, DeepSummary, DEEP_HIT_CAP,
};
pub use preview::{Preview, PreviewCache, RefreshStats, PREVIEW_MAX_CHARS};
pub use query::{parse_query, DateBound, Query};
pub use rename::{rename_past_session, rename_past_session_if, RenameError, SessionMtimes};
pub use time::{Clock, When};
pub use view::{
    build_view, day_groups, noise_reasons, DateKey, DayGroup, FacetCount, FacetSelection, Facets,
    NoiseCounts, NoiseReasons, Sort, View, MIN_MESSAGES,
};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Where the catalog reads and writes. Every path is injectable so tests and
/// benches never touch the real `~/.synaps-cli`.
#[derive(Debug, Clone)]
pub struct CatalogPaths {
    /// Sessions directory (`<config_dir>/sessions`, profile-aware).
    pub sessions_dir: PathBuf,
    /// `index.jsonl` (always under the BASE dir, shared by profiles).
    pub index_path: PathBuf,
    /// Preview sidecar cache file (written by [`PreviewCache::save`]).
    pub cache_path: PathBuf,
}

impl CatalogPaths {
    /// Production paths for the active profile. The cache lives outside the
    /// sessions dir and outside `~/.cache/synaps` (retention prunes that).
    pub fn resolve() -> Self {
        let config_dir = crate::config::get_active_config_dir();
        Self {
            sessions_dir: config_dir.join("sessions"),
            index_path: crate::core::session_index::index_path(),
            cache_path: config_dir.join("catalog").join("session-previews.json"),
        }
    }
}

/// One past session as the Past tab sees it.
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    pub id: String,
    pub title: String,
    pub name: Option<String>,
    /// Raw model id as stored (`anthropic/claude-opus-4-6`).
    pub model: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub cost: f64,
    /// Header `message_count` (journal-overlaid). `None` for legacy files
    /// that never persisted it — filled from the preview cache when known.
    pub message_count: Option<usize>,
    /// Successor id when this session was compacted into another.
    pub compacted_into: Option<String>,
    // ── index.jsonl join ──
    /// Most recent `start` cwd.
    pub cwd: Option<PathBuf>,
    pub profile: Option<String>,
    /// Sum of `end` record turns, when any were recorded.
    pub turns: Option<usize>,
    /// Sum of `duration_ms` (no writer emits it today — see [`Self::span`]).
    pub duration: Option<std::time::Duration>,
    // ── preview cache ──
    pub first_user: Option<String>,
    pub last_user: Option<String>,
    // ── file facts ──
    /// Newest of snapshot / journal mtime.
    pub mtime: Option<SystemTime>,
    /// Snapshot + journal bytes.
    pub bytes: u64,
    pub has_journal: bool,
    /// File stem differs from header id, or the id is not `YYYYMMDD-HHMMSS-xxxx`.
    pub junk_id: bool,
    /// Lower-cased search haystack (title, name, id, cwd, first/last).
    search_blob: String,
}

impl CatalogEntry {
    /// Wall-clock span created → updated (what the preview strip shows).
    pub fn span(&self) -> chrono::Duration {
        self.updated_at - self.created_at
    }

    /// Normalised model key: provider prefix stripped (`claude-opus-4-6`).
    pub fn model_key(&self) -> &str {
        model_key(&self.model)
    }

    /// Short model label (`opus-4-6`).
    pub fn model_label(&self) -> &str {
        model_label(&self.model)
    }

    /// cwd with `$HOME` shown as `~` (`~/Projects/x`, `~` for home).
    pub fn cwd_display(&self) -> Option<String> {
        self.cwd.as_deref().map(tilde)
    }

    /// Last four id characters (`…ab12` style row label).
    pub fn short_id(&self) -> &str {
        let n = self.id.len();
        self.id.get(n.saturating_sub(4)..).unwrap_or(&self.id)
    }

    /// Set the name (after a rename) and keep the search haystack in sync.
    pub fn set_name(&mut self, name: Option<String>) {
        self.name = name;
        self.rebuild_search_blob();
    }

    pub(crate) fn search_blob(&self) -> &str {
        &self.search_blob
    }

    fn rebuild_search_blob(&mut self) {
        let mut s = String::with_capacity(256);
        for part in [
            Some(self.title.as_str()),
            self.name.as_deref(),
            Some(self.id.as_str()),
            self.first_user.as_deref(),
            self.last_user.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            s.push_str(part);
            s.push('\u{1f}');
        }
        if let Some(cwd) = &self.cwd {
            s.push_str(&cwd.to_string_lossy());
            s.push('\u{1f}');
            s.push_str(&tilde(cwd));
        }
        self.search_blob = s.to_lowercase();
    }
}

/// The loaded catalog. `entries` is in no particular order — use
/// [`build_view`] for display order.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub entries: Vec<CatalogEntry>,
    /// Snapshot files whose header could not be parsed (skipped).
    pub unreadable: usize,
}

impl Catalog {
    /// Fill first/last user text and true message counts from the cache.
    /// Header counts win when present (they are journal-overlaid and fresh).
    pub fn apply_previews(&mut self, cache: &PreviewCache) {
        for e in &mut self.entries {
            if let Some(p) = cache.get_fresh(e) {
                e.first_user = p.first_user.clone();
                e.last_user = p.last_user.clone();
                if e.message_count.is_none() {
                    e.message_count = Some(p.message_count);
                }
                e.rebuild_search_blob();
            }
        }
    }

    /// Drop ids that are live in the daemon (Past = disk minus daemon ids).
    pub fn without_ids<'a>(mut self, live: impl IntoIterator<Item = &'a str>) -> Self {
        let live: std::collections::HashSet<&str> = live.into_iter().collect();
        self.entries.retain(|e| !live.contains(e.id.as_str()));
        self
    }

    pub fn get(&self, id: &str) -> Option<&CatalogEntry> {
        self.entries.iter().find(|e| e.id == id)
    }
}

/// Load every past session header in `paths.sessions_dir`, joined with
/// `paths.index_path`. Pure + blocking: run it on `spawn_blocking`.
/// A missing dir or index is empty, not an error.
pub fn load_catalog(paths: &CatalogPaths) -> std::io::Result<Catalog> {
    let index = read_index(&paths.index_path);
    let dir = &paths.sessions_dir;
    let entries = crate::core::session_journal::session_dir_entries(dir)?;

    let mut journals: HashMap<&str, &crate::core::session_journal::SessionDirEntry> =
        HashMap::new();
    for e in &entries {
        if let Some(id) = e.name.strip_suffix(".journal") {
            journals.insert(id, e);
        }
    }

    let snapshots: Vec<(&crate::core::session_journal::SessionDirEntry, &str)> = entries
        .iter()
        .filter_map(|f| f.name.strip_suffix(".json").map(|stem| (f, stem)))
        .collect();
    // Header reads are tiny but latency-bound on a cold page cache: a few
    // threads keep the disk queue busy (≈4× faster cold, same warm).
    let headers = parallel_map(&snapshots, 8, |(file, _)| read_header(dir, &file.name));

    let mut out = Catalog::default();
    for ((file, stem), h) in snapshots.iter().zip(headers) {
        let Some(h) = h else {
            out.unreadable += 1;
            continue;
        };
        let journal = journals.get(stem);
        let mtime = match (file.mtime, journal.and_then(|j| j.mtime)) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        let junk_id = h.id != *stem || !is_session_id(&h.id);
        let joined = index.get(h.id.as_str());
        let mut entry = CatalogEntry {
            id: h.id,
            title: h.title,
            name: h.name,
            model: h.model,
            created_at: h.created_at,
            updated_at: h.updated_at,
            cost: h.session_cost,
            message_count: h.message_count,
            compacted_into: h.compacted_into,
            cwd: joined.and_then(|j| j.cwd.clone()),
            profile: joined.and_then(|j| j.profile.clone()),
            turns: joined.and_then(|j| j.turns),
            duration: joined
                .and_then(|j| j.duration_ms)
                .map(std::time::Duration::from_millis),
            first_user: None,
            last_user: None,
            mtime,
            bytes: file.byte_len + journal.map_or(0, |j| j.byte_len),
            has_journal: journal.is_some(),
            junk_id,
            search_blob: String::new(),
        };
        entry.rebuild_search_blob();
        out.entries.push(entry);
    }
    Ok(out)
}

/// Order-preserving map over `items` on up to `threads` scoped threads.
fn parallel_map<T: Sync, R: Send>(
    items: &[T],
    threads: usize,
    f: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    let threads = threads.min(items.len()).max(1);
    if threads == 1 {
        return items.iter().map(f).collect();
    }
    let chunk = items.len().div_ceil(threads);
    std::thread::scope(|scope| {
        let handles: Vec<_> = items
            .chunks(chunk)
            .map(|part| {
                let f = &f;
                scope.spawn(move || part.iter().map(f).collect::<Vec<R>>())
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("catalog worker panicked"))
            .collect()
    })
}

// ─── header ──────────────────────────────────────────────────────────────────

struct Header {
    id: String,
    title: String,
    name: Option<String>,
    model: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    session_cost: f64,
    message_count: Option<usize>,
    compacted_into: Option<String>,
}

/// Same header read + journal overlay as `session::parse_session_header`,
/// but keeps `message_count` optional (legacy files lack it).
fn read_header(dir: &Path, file_name: &str) -> Option<Header> {
    #[derive(Deserialize)]
    struct Raw {
        #[serde(default)]
        _journal_generation: Option<String>,
        id: String,
        #[serde(default)]
        title: String,
        #[serde(default)]
        name: Option<String>,
        model: String,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
        #[serde(default)]
        session_cost: f64,
        #[serde(default)]
        message_count: Option<usize>,
        #[serde(default)]
        compacted_into: Option<String>,
    }
    let file = crate::core::session_journal::confined_open(dir, file_name).ok()??;
    let text = crate::core::session::read_session_header_from_file(file)?;
    let raw: Raw = serde_json::from_str(&text).ok()?;
    let mut h = Header {
        id: raw.id,
        title: raw.title,
        name: raw.name,
        model: raw.model,
        created_at: raw.created_at,
        updated_at: raw.updated_at,
        session_cost: raw.session_cost,
        message_count: raw.message_count,
        compacted_into: raw.compacted_into,
    };
    if let Some(tail) = crate::core::session_journal::journal_meta_tail_for_generation(
        dir,
        &h.id,
        raw._journal_generation.as_deref(),
    ) {
        if tail.updated_at > h.updated_at {
            h.updated_at = tail.updated_at;
            h.session_cost = tail.session_cost;
            if tail.message_count.is_some() {
                h.message_count = tail.message_count;
            }
        }
    }
    Some(h)
}

// ─── index.jsonl join ────────────────────────────────────────────────────────

#[derive(Default)]
struct IndexJoin {
    cwd: Option<PathBuf>,
    profile: Option<String>,
    turns: Option<usize>,
    duration_ms: Option<u64>,
}

/// Fold `index.jsonl` per session id. Lenient: malformed lines are skipped;
/// only the handful of fields we need are deserialised.
fn read_index(path: &Path) -> HashMap<String, IndexJoin> {
    #[derive(Deserialize)]
    struct Line {
        session_id: String,
        #[serde(default)]
        event: Option<String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
        #[serde(default)]
        profile: Option<String>,
        #[serde(default)]
        turns: Option<usize>,
        #[serde(default)]
        duration_ms: Option<u64>,
    }
    let mut out: HashMap<String, IndexJoin> = HashMap::new();
    let Ok(bytes) = std::fs::read(path) else {
        return out;
    };
    // The index is append-only: later lines are newer.
    for line in bytes.split(|b| *b == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(rec) = serde_json::from_slice::<Line>(line) else {
            continue;
        };
        let slot = out.entry(rec.session_id).or_default();
        if rec.event.as_deref() == Some("start") || rec.cwd.is_some() {
            if rec.cwd.is_some() {
                slot.cwd = rec.cwd;
            }
            if rec.profile.is_some() {
                slot.profile = rec.profile;
            }
        }
        if let Some(t) = rec.turns {
            slot.turns = Some(slot.turns.unwrap_or(0) + t);
        }
        if let Some(d) = rec.duration_ms {
            slot.duration_ms = Some(slot.duration_ms.unwrap_or(0) + d);
        }
    }
    out
}

// ─── small helpers ───────────────────────────────────────────────────────────

/// `YYYYMMDD-HHMMSS-<hex>` (4+ hex chars) — the shape `Session::new` mints.
pub fn is_session_id(id: &str) -> bool {
    let b = id.as_bytes();
    b.len() >= 20
        && b[..8].iter().all(u8::is_ascii_digit)
        && b[8] == b'-'
        && b[9..15].iter().all(u8::is_ascii_digit)
        && b[15] == b'-'
        && b[16..]
            .iter()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
}

/// Strip a `provider/` prefix: `anthropic/claude-opus-4-6` → `claude-opus-4-6`.
pub fn model_key(model: &str) -> &str {
    model.rsplit_once('/').map_or(model, |(_, m)| m)
}

/// Short label: `claude-opus-4-6` → `opus-4-6`; others unchanged.
pub fn model_label(model: &str) -> &str {
    let k = model_key(model);
    k.strip_prefix("claude-").unwrap_or(k)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// `$HOME` → `~`, `$HOME/x` → `~/x`.
pub fn tilde(path: &Path) -> String {
    tilde_with(path, home_dir().as_deref())
}

pub(crate) fn tilde_with(path: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home {
        if path == home {
            return "~".into();
        }
        if let Ok(rest) = path.strip_prefix(home) {
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}
