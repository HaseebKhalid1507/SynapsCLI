//! Sidecar preview cache: first + last user message and the true message
//! count per session, keyed by id and stamped with (mtime, bytes) so a
//! refresh only re-reads sessions whose files changed.
//!
//! User text goes through [`user_content_for_display`] — the same projection
//! titles use — so tool results and attachment bodies never leak into the
//! preview or the search haystack. Host-injected user-role text (event
//! deliveries, abort/interrupt markers) is skipped too: "last" means the
//! last thing the human typed.

use super::body::{is_context_note, is_host_injected, walk_session};
use super::CatalogEntry;
use crate::core::session::user_content_for_display;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Instant, SystemTime};

/// Preview text cap (characters, after whitespace collapse).
pub const PREVIEW_MAX_CHARS: usize = 200;
const CACHE_VERSION: u32 = 1;

/// Cached facts for one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preview {
    #[serde(rename = "f", default, skip_serializing_if = "Option::is_none")]
    pub first_user: Option<String>,
    #[serde(rename = "l", default, skip_serializing_if = "Option::is_none")]
    pub last_user: Option<String>,
    #[serde(rename = "n")]
    pub message_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Slot {
    /// Entry mtime in unix ms (0 = unknown).
    m: u64,
    /// Entry bytes (snapshot + journal).
    b: u64,
    #[serde(flatten)]
    p: Preview,
}

#[derive(Serialize, Deserialize)]
struct FileFormat {
    v: u32,
    entries: HashMap<String, Slot>,
}

/// What a [`PreviewCache::refresh`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RefreshStats {
    /// Sessions (re)read this refresh.
    pub scanned: usize,
    /// Sessions whose stamp still matched (no I/O).
    pub reused: usize,
    /// Cached ids no longer in the catalog, dropped.
    pub pruned: usize,
    /// Sessions that failed to read/parse (left uncached; retried next time).
    pub failed: usize,
    pub cancelled: bool,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Default)]
pub struct PreviewCache {
    entries: HashMap<String, Slot>,
}

fn stamp(e: &CatalogEntry) -> (u64, u64) {
    let m = e
        .mtime
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64);
    (m, e.bytes)
}

impl PreviewCache {
    /// Load a cache file. Missing, unreadable, corrupt or old-version files
    /// yield an empty cache (it is only ever a cache).
    pub fn load(path: &Path) -> Self {
        let entries = std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<FileFormat>(&b).ok())
            .filter(|f| f.v == CACHE_VERSION)
            .map(|f| f.entries)
            .unwrap_or_default();
        Self { entries }
    }

    /// Atomically write the cache (0600, parent created 0700).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            crate::core::private_fs::ensure_private_dir(parent).map_err(std::io::Error::other)?;
        }
        let body = serde_json::to_vec(&FileFormat {
            v: CACHE_VERSION,
            entries: self.entries.clone(),
        })
        .map_err(std::io::Error::other)?;
        crate::core::private_fs::write_atomic_private(path, &body).map_err(std::io::Error::other)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Cached preview if its stamp still matches `e`'s files.
    pub fn get_fresh(&self, e: &CatalogEntry) -> Option<&Preview> {
        let s = self.entries.get(&e.id)?;
        ((s.m, s.b) == stamp(e)).then_some(&s.p)
    }

    /// How many of `entries` need a (re)scan.
    pub fn stale_count(&self, entries: &[CatalogEntry]) -> usize {
        entries
            .iter()
            .filter(|e| self.get_fresh(e).is_none())
            .count()
    }

    /// Bring the cache in line with `entries` (pass the FULL catalog: ids not
    /// in it are pruned). Re-reads only sessions whose (mtime, bytes) changed,
    /// on up to 4 worker threads. `progress(done, total)` is called on the
    /// calling thread after each session; `cancel` stops between sessions,
    /// keeping everything finished so far. Blocking — run off the UI thread.
    pub fn refresh(
        &mut self,
        dir: &Path,
        entries: &[CatalogEntry],
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(usize, usize),
    ) -> RefreshStats {
        let started = Instant::now();
        let mut stats = RefreshStats::default();
        let live: std::collections::HashSet<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        let before = self.entries.len();
        self.entries.retain(|id, _| live.contains(id.as_str()));
        stats.pruned = before - self.entries.len();

        let todo: Vec<&CatalogEntry> = entries
            .iter()
            .filter(|e| self.get_fresh(e).is_none())
            .collect();
        stats.reused = entries.len() - todo.len();
        let total = todo.len();
        if total == 0 {
            stats.elapsed_ms = started.elapsed().as_millis() as u64;
            return stats;
        }

        let threads = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .clamp(1, 4)
            .min(total);
        let next = AtomicUsize::new(0);
        let (tx, rx) = std::sync::mpsc::channel::<(usize, Option<Preview>)>();
        std::thread::scope(|scope| {
            for _ in 0..threads {
                let tx = tx.clone();
                let (next, todo) = (&next, &todo);
                scope.spawn(move || loop {
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(e) = todo.get(i) else { break };
                    let p = scan_preview(dir, e).ok();
                    if tx.send((i, p)).is_err() {
                        break;
                    }
                });
            }
            drop(tx);
            let mut done = 0;
            for (i, p) in rx {
                done += 1;
                let e = todo[i];
                match p {
                    Some(p) => {
                        let (m, b) = stamp(e);
                        self.entries.insert(e.id.clone(), Slot { m, b, p });
                        stats.scanned += 1;
                    }
                    None => stats.failed += 1,
                }
                progress(done, total);
            }
        });
        stats.cancelled = cancel.load(Ordering::Relaxed) && stats.scanned + stats.failed < total;
        stats.elapsed_ms = started.elapsed().as_millis() as u64;
        stats
    }
}

/// Read one session body: first/last displayable user text + message count.
fn scan_preview(dir: &Path, e: &CatalogEntry) -> std::io::Result<Preview> {
    let mut first: Option<String> = None;
    let mut last: Option<String> = None;
    let (count, _) = walk_session(dir, &e.id, e.has_journal, &mut |m| {
        if m["role"] == "user" && !is_context_note(m) {
            if let Some(text) =
                user_content_for_display(&m["content"]).filter(|t| !is_host_injected(t))
            {
                let t = clip(&text);
                if !t.is_empty() {
                    if first.is_none() {
                        first = Some(t.clone());
                    }
                    last = Some(t);
                }
            }
        }
        true
    })?;
    Ok(Preview {
        first_user: first,
        last_user: last,
        message_count: count,
    })
}

/// Collapse whitespace runs to one space and cap at [`PREVIEW_MAX_CHARS`]
/// (ellipsis included). Control characters are dropped.
pub(crate) fn clip(s: &str) -> String {
    let mut out = String::new();
    let mut n = 0;
    for word in s.split_whitespace() {
        let sep = (n > 0).then_some(' ');
        for c in sep.into_iter().chain(word.chars()) {
            if c.is_control() {
                continue;
            }
            if n == PREVIEW_MAX_CHARS {
                // Over the cap: replace the last char with an ellipsis.
                out.pop();
                out.push('…');
                return out;
            }
            out.push(c);
            n += 1;
        }
    }
    out
}
