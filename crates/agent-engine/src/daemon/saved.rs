//! `ClientFrame::CatalogPage`: the saved sessions, one page at a time, for
//! clients that cannot read the sessions dir themselves (a thin or remote
//! client).
//!
//! Held and saved are disjoint and together complete, by one rule
//! ([`held`]): a chain head on disk is saved iff it is listable
//! ([`saved_row`]) and no live session holds it. A compacted predecessor
//! folds into its chain head (one row per conversation), and the noise
//! rules never hide a named conversation (names are unique; a hidden one
//! would block its name while invisible).
//!
//! The catalog read is blocking disk I/O. [`SavedLoader`] runs it on
//! `spawn_blocking`, one load at a time daemon-wide, on that one thread (no
//! header-reading pool). A first page always gets a load that started
//! after it was asked; a continuation page may reuse one at most
//! [`CATALOG_REUSE`] old, so paging through is one disk scan.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_core::session_catalog::{
    is_sandbox_cwd, listing_title, load_catalog_with, CatalogEntry, CatalogPaths, PreviewCache, MIN_MESSAGES,
};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use tokio::sync::{Mutex, OwnedMutexGuard};

use super::DaemonState;
use crate::session::types::{SessionId, SessionLifecycle, SessionMeta};
use crate::session::wire::{CatalogCursor, DaemonFrame, CATALOG_PAGE_DEFAULT_LIMIT, CATALOG_PAGE_MAX_LIMIT};
use crate::session::SessionHandle;

/// `CatalogPage` requests one connection may have in flight; one more is
/// answered at once with `error`.
pub const CATALOG_PAGES_IN_FLIGHT: usize = 4;

/// How old a load a continuation page (`before` set) may reuse.
pub const CATALOG_REUSE: Duration = Duration::from_secs(30);

/// What a saved row is built from. The disk driver ([`fold`]) fills it
/// from the catalog; an actor can fill it from its conversation (PR 3:
/// the actor driver, for rows pushed when a session ends), and
/// [`saved_row`] builds the same row from either.
#[derive(Clone, Debug, PartialEq)]
pub struct SavedFacts {
    /// The chain head's journal id.
    pub journal_id: String,
    /// The journal is on disk (a session that never saved is not).
    pub on_disk: bool,
    /// The file stem differs from the id, or it is not a session id.
    pub junk_id: bool,
    /// The head's name, else the newest named predecessor's.
    pub name: Option<String>,
    pub header_title: String,
    pub first_user: Option<String>,
    pub model: String,
    /// The head's cwd, else the newest predecessor's that has one.
    pub cwd: Option<PathBuf>,
    pub cost_usd: f64,
    /// The chain's start.
    pub created_at: DateTime<Utc>,
    /// The head journal's.
    pub updated_at: DateTime<Utc>,
    /// The head's message count; `None` from a legacy file.
    pub messages: Option<usize>,
    /// The head's `parent_session`: it is a compaction's successor. An
    /// actor that has written the successor but not yet moved onto it
    /// still holds this one, so the head is held too ([`page`]).
    pub parent: Option<String>,
}

/// The one place the listing rules live. PR 3: the hub's retire builds the
/// pushed row (`Watch{saved: true}`) with this too. `None`: not listable — not on
/// disk, a junk id, or unnamed and either in a sandbox cwd or with few
/// messages and not chained (a fresh successor holds just the summary).
/// A named conversation is never hidden; `named_only` lists only those.
pub fn saved_row(f: &SavedFacts, named_only: bool) -> Option<SessionMeta> {
    if !f.on_disk || f.junk_id || (named_only && f.name.is_none()) {
        return None;
    }
    if f.name.is_none() {
        let sandbox = f.cwd.as_deref().is_some_and(is_sandbox_cwd);
        let few = f.messages.is_some_and(|n| n < MIN_MESSAGES) && f.parent.is_none();
        if sandbox || few {
            return None;
        }
    }
    let title = listing_title(&f.header_title, f.first_user.as_deref());
    Some(SessionMeta {
        id: SessionId::from(f.journal_id.clone()),
        name: f.name.clone(),
        model: f.model.clone(),
        cwd: f.cwd.clone(),
        created_at: f.created_at,
        continued: false,
        continue_info: None,
        host_pid: 0,
        lifecycle: SessionLifecycle::Saved,
        clients: 0,
        input_owner: None,
        awaiting_input: 0,
        roster: Vec::new(),
        client_input: None,
        journal_id: f.journal_id.clone(),
        locked_by: None,
        pinned: false,
        busy: false,
        state_since: None,
        park_due: None,
        evict_due: None,
        title: Some(title).filter(|t| !t.is_empty()),
        cost_usd: f.cost_usd,
        messages: f.messages.unwrap_or(0),
        workers: Default::default(),
        updated_at: Some(f.updated_at),
        surfaces_waiting: 0,
        surfaces_open: 0,
    })
}

/// The conversations live sessions hold ([`SessionHandle::holds`]: each
/// one's journal, and those it compacted away from, whose successor may
/// not be on disk yet), read once per page. Never an actor id: after
/// `/clear` the actor keeps its id while its old conversation is saved.
pub fn held(handles: &[SessionHandle]) -> HashSet<String> {
    let mut set = HashSet::new();
    for h in handles {
        let cell = h.journal();
        set.insert(cell.journal_id.clone());
        set.extend(cell.supersedes.iter().cloned());
    }
    set
}

/// Absent → 50; at least 1, at most 200.
pub fn effective_limit(limit: Option<u32>) -> usize {
    limit.unwrap_or(CATALOG_PAGE_DEFAULT_LIMIT).clamp(1, CATALOG_PAGE_MAX_LIMIT) as usize
}

/// Serve `CatalogPage`. Always one frame, `CatalogPage{req}`, with `error`
/// when the sessions dir cannot be read. Cancel-safe: dropping it leaves
/// a running load to finish (and serve the requests waiting on it).
pub async fn catalog_page(
    state: &Arc<DaemonState>,
    req: u64,
    before: Option<CatalogCursor>,
    limit: Option<u32>,
    named_only: bool,
) -> DaemonFrame {
    let facts = match state.saved.facts(CatalogPaths::resolve(), before.is_none()).await {
        Ok(f) => f,
        Err(e) => return page_error(req, format!("saved sessions: {e}")),
    };
    // After the load: a session resumed meanwhile is held, not saved.
    let held = held(&state.host.session_handles());
    let (sessions, next) = page(&facts, |c| held.contains(c), before.as_ref(), effective_limit(limit), named_only);
    DaemonFrame::CatalogPage { req, sessions, next, error: None }
}

/// A `CatalogPage` reply carrying only `error`.
pub fn page_error(req: u64, error: String) -> DaemonFrame {
    DaemonFrame::CatalogPage { req, sessions: Vec::new(), next: None, error: Some(error) }
}

/// One page of `facts` (sorted by [`fold`]): rows strictly after `before`,
/// listable and not held, at most `limit`; `next` is the last row's cursor
/// when another row follows. A head is held when it or its parent is: a
/// compaction writes the successor before the actor moves onto it, and in
/// that window the actor holds only the parent. No actor holds any other
/// non-head member (resume follows the chain).
pub fn page(
    facts: &[SavedFacts],
    held: impl Fn(&str) -> bool,
    before: Option<&CatalogCursor>,
    limit: usize,
    named_only: bool,
) -> (Vec<SessionMeta>, Option<CatalogCursor>) {
    let start = before.map_or(0, |c| facts.partition_point(|f| !after(f, c)));
    let mut rows = facts[start..]
        .iter()
        .filter(|f| !held(&f.journal_id) && !f.parent.as_deref().is_some_and(&held))
        .filter_map(|f| saved_row(f, named_only));
    let out: Vec<SessionMeta> = rows.by_ref().take(limit).collect();
    let next = match (out.last(), rows.next()) {
        (Some(last), Some(_)) => Some(CatalogCursor {
            updated_at: last.updated_at.unwrap_or_default(),
            id: last.journal_id.clone(),
        }),
        _ => None,
    };
    (out, next)
}

/// `f` sorts after cursor `c` (newest `updated_at` first, then `id`
/// descending).
fn after(f: &SavedFacts, c: &CatalogCursor) -> bool {
    (f.updated_at, f.journal_id.as_str()) < (c.updated_at, c.id.as_str())
}

/// The catalog, loaded at most once at a time daemon-wide. A first page
/// waits for a load that started after it was asked (requests that queue
/// behind a running load share the next one); a continuation page may
/// reuse a load at most [`CATALOG_REUSE`] old. The blocking job owns the
/// lock, so a client that gives up never lets a second load start beside
/// it.
pub struct SavedLoader {
    slot: Arc<Mutex<Option<Loaded>>>,
    requests: AtomicUsize,
    loads: Arc<AtomicUsize>,
    reuse: Duration,
}

impl Default for SavedLoader {
    fn default() -> Self {
        Self::with_reuse(CATALOG_REUSE)
    }
}

struct Loaded {
    started: Instant,
    facts: Arc<Vec<SavedFacts>>,
}

/// [`SavedLoader::hold`]: the loader's lock, held by a test.
#[cfg(any(test, feature = "testing"))]
pub struct HeldLoader(#[allow(dead_code)] OwnedMutexGuard<Option<Loaded>>);

impl SavedLoader {
    /// A loader whose continuation pages reuse a load at most `reuse` old.
    pub fn with_reuse(reuse: Duration) -> Self {
        Self { slot: Arc::default(), requests: AtomicUsize::new(0), loads: Arc::default(), reuse }
    }

    /// The folded catalog: a load that started after this call when
    /// `fresh`, else one at most the reuse window old.
    pub async fn facts(&self, paths: CatalogPaths, fresh: bool) -> std::io::Result<Arc<Vec<SavedFacts>>> {
        let asked = Instant::now();
        self.requests.fetch_add(1, Ordering::SeqCst);
        let slot: OwnedMutexGuard<Option<Loaded>> = Arc::clone(&self.slot).lock_owned().await;
        let usable = |l: &&Loaded| if fresh { l.started >= asked } else { l.started.elapsed() <= self.reuse };
        if let Some(l) = slot.as_ref().filter(usable) {
            return Ok(Arc::clone(&l.facts));
        }
        let loads = Arc::clone(&self.loads);
        let job = tokio::task::spawn_blocking(move || {
            let mut slot = slot;
            loads.fetch_add(1, Ordering::SeqCst);
            let started = Instant::now();
            let r = load(&paths).map(|e| Arc::new(fold(&e)));
            if let Ok(facts) = &r {
                *slot = Some(Loaded { started, facts: Arc::clone(facts) });
            }
            r
        });
        // `panic = "abort"` in release: a JoinError here is only the
        // runtime shutting down.
        job.await.unwrap_or_else(|e| Err(std::io::Error::other(format!("listing interrupted: {e}"))))
    }

    /// Requests received (diagnostics, tests).
    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    /// Catalog loads run (diagnostics, tests).
    pub fn loads(&self) -> usize {
        self.loads.load(Ordering::SeqCst)
    }

    /// Hold the loader's lock: every page waits until the guard drops.
    #[cfg(any(test, feature = "testing"))]
    pub async fn hold(&self) -> HeldLoader {
        HeldLoader(Arc::clone(&self.slot).lock_owned().await)
    }
}

/// Headers (one thread, no pool) + the preview cache, read only. Blocking.
pub fn load(paths: &CatalogPaths) -> std::io::Result<Vec<CatalogEntry>> {
    let mut cat = load_catalog_with(paths, 1)?;
    cat.apply_previews(&PreviewCache::load(&paths.cache_path));
    Ok(cat.entries)
}

/// The disk driver: junk ids out; each chain folded into its head (name
/// and cwd from the newest predecessor that has one when the head lacks
/// them, `created_at` the chain's start); sorted newest `updated_at`
/// first, then `id` descending. Listability is [`saved_row`]'s.
pub fn fold(entries: &[CatalogEntry]) -> Vec<SavedFacts> {
    let entries: Vec<&CatalogEntry> = entries.iter().filter(|e| !e.junk_id).collect();
    let by_id: HashMap<&str, usize> = entries.iter().enumerate().map(|(i, e)| (e.id.as_str(), i)).collect();
    let mut chains: HashMap<usize, Vec<usize>> = HashMap::new();
    for (i, head) in chain_heads(&entries, &by_id).into_iter().enumerate() {
        if let Some(head) = head {
            chains.entry(head).or_default().push(i);
        }
    }

    let mut out: Vec<SavedFacts> = chains
        .into_iter()
        .map(|(head, members)| {
            let h = entries[head];
            // Newest member first: a name/cwd the head lacks comes from the
            // most recent predecessor that has one.
            let mut chain: Vec<&CatalogEntry> = members.iter().map(|&i| entries[i]).collect();
            chain.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| b.id.cmp(&a.id)));
            SavedFacts {
                journal_id: h.id.clone(),
                on_disk: true,
                junk_id: h.junk_id,
                name: h.name.clone().or_else(|| chain.iter().find_map(|e| e.name.clone())),
                header_title: h.title.clone(),
                first_user: h.first_user.clone(),
                model: h.model.clone(),
                cwd: h.cwd.clone().or_else(|| chain.iter().find_map(|e| e.cwd.clone())),
                cost_usd: h.cost,
                created_at: chain.iter().map(|e| e.created_at).min().unwrap_or(h.created_at),
                updated_at: h.updated_at,
                messages: h.message_count,
                parent: h.parent_session.clone(),
            }
        })
        .collect();
    out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| b.journal_id.cmp(&a.journal_id)));
    out
}

/// Each entry's chain head: `compacted_into` followed to the last session
/// on disk, however long the chain. `None` for a cycle and anything that
/// leads into one: it cannot be resumed, so it is not listed. A successor
/// missing from disk ends the walk there (a live session that compacted
/// into it holds the predecessor, [`held`]). O(n): every entry is walked
/// once.
fn chain_heads(entries: &[&CatalogEntry], by_id: &HashMap<&str, usize>) -> Vec<Option<usize>> {
    let mut head: Vec<Option<Option<usize>>> = vec![None; entries.len()];
    for start in 0..entries.len() {
        let mut path = Vec::new();
        let mut on_path = HashSet::new();
        let mut cur = start;
        let found = loop {
            if let Some(known) = head[cur] {
                break known;
            }
            if !on_path.insert(cur) {
                break None;
            }
            path.push(cur);
            match entries[cur].compacted_into.as_deref().and_then(|next| by_id.get(next)) {
                Some(&next) => cur = next,
                None => break Some(cur),
            }
        };
        for i in path {
            head[i] = Some(found);
        }
    }
    head.into_iter().map(Option::flatten).collect()
}

#[cfg(test)]
mod tests;
