//! `ClientFrame::SavedSessions`: the saved sessions on disk, for clients
//! that cannot read the sessions dir themselves (a thin or remote client).
//!
//! Same source and filter as the TUI's `/sessions` Past tab
//! (`session_catalog::load_catalog` + its noise rules), with differences
//! that suit a session picker: a compacted predecessor folds into its chain
//! head (one row per conversation), sessions this daemon holds stay listed
//! (flagged `live`), and the noise rules never hide a named conversation
//! (names are unique; a hidden one would block its name while invisible).
//!
//! The catalog read is blocking disk I/O. [`SavedLoader`] runs it on
//! `spawn_blocking`, one load at a time daemon-wide, on that one thread
//! (no header-reading pool): requests that arrive during a load share it.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use agent_core::session_catalog::{
    is_sandbox_cwd, load_catalog_with, noise_reasons, CatalogEntry, CatalogPaths, PreviewCache,
};
use tokio::sync::{Mutex, OwnedMutexGuard};

use super::DaemonState;
use crate::session::types::SessionMeta;
use crate::session::wire::{
    DaemonFrame, SavedSessionInfo, SAVED_SESSIONS_DEFAULT_LIMIT, SAVED_SESSIONS_MAX_LIMIT, SAVED_SESSION_TITLE_CHARS,
};

/// Serve `SavedSessions{named_only, limit}`. Always one frame:
/// `SavedSessionList`, or `Error` when the sessions dir cannot be read.
/// Cancel-safe: dropping it leaves a running load to finish (and serve the
/// requests waiting on it).
pub async fn saved_sessions(state: &Arc<DaemonState>, named_only: bool, limit: Option<u32>) -> DaemonFrame {
    let entries = match state.saved.entries(CatalogPaths::resolve()).await {
        Ok(e) => e,
        Err(e) => return DaemonFrame::Error { session_id: None, message: format!("saved sessions: {e}") },
    };
    // After the load: a session the daemon took up meanwhile is `live`.
    let live = live_ids(&state.session_metas());
    let (sessions, truncated) = rows(&entries, &live, named_only, effective_limit(limit));
    DaemonFrame::SavedSessionList { sessions, truncated }
}

/// Held sessions by journal id (≠ actor id after a LinkedSuccessor
/// compaction) and by actor id (the successor may not be on disk yet).
pub fn live_ids(metas: &[SessionMeta]) -> HashSet<String> {
    metas
        .iter()
        .flat_map(|m| [m.journal_id.clone(), m.id.as_str().to_string()])
        .filter(|id| !id.is_empty())
        .collect()
}

/// Absent → 200; capped at 1000.
pub fn effective_limit(limit: Option<u32>) -> usize {
    limit.unwrap_or(SAVED_SESSIONS_DEFAULT_LIMIT).min(SAVED_SESSIONS_MAX_LIMIT) as usize
}

/// The catalog, loaded at most once at a time daemon-wide. A request that
/// arrives while a load runs waits for it and shares its result; one that
/// arrives after starts a fresh load, so no reply predates its request by
/// more than the load in flight when it came. The blocking job owns the
/// lock, so a client that gives up never lets a second load start beside it.
#[derive(Default)]
pub struct SavedLoader {
    slot: Arc<Mutex<Option<Loaded>>>,
    requests: AtomicUsize,
    loads: Arc<AtomicUsize>,
}

struct Loaded {
    done: Instant,
    entries: Arc<Vec<CatalogEntry>>,
}

/// [`SavedLoader::hold`]: the loader's lock, held by a test.
#[cfg(any(test, feature = "testing"))]
pub struct HeldLoader(#[allow(dead_code)] OwnedMutexGuard<Option<Loaded>>);

impl SavedLoader {
    pub async fn entries(&self, paths: CatalogPaths) -> std::io::Result<Arc<Vec<CatalogEntry>>> {
        let asked = Instant::now();
        self.requests.fetch_add(1, Ordering::SeqCst);
        let slot: OwnedMutexGuard<Option<Loaded>> = Arc::clone(&self.slot).lock_owned().await;
        if let Some(l) = slot.as_ref().filter(|l| l.done >= asked) {
            return Ok(Arc::clone(&l.entries));
        }
        let loads = Arc::clone(&self.loads);
        let job = tokio::task::spawn_blocking(move || {
            let mut slot = slot;
            loads.fetch_add(1, Ordering::SeqCst);
            let r = load(&paths).map(Arc::new);
            if let Ok(entries) = &r {
                *slot = Some(Loaded { done: Instant::now(), entries: Arc::clone(entries) });
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

    /// Hold the loader's lock: every listing waits until the guard drops.
    #[cfg(any(test, feature = "testing"))]
    pub async fn hold(&self) -> HeldLoader {
        HeldLoader(Arc::clone(&self.slot).lock_owned().await)
    }
}

/// Headers (one thread, no pool) + the preview cache, read only. Blocking.
fn load(paths: &CatalogPaths) -> std::io::Result<Vec<CatalogEntry>> {
    let mut cat = load_catalog_with(paths, 1)?;
    cat.apply_previews(&PreviewCache::load(&paths.cache_path));
    Ok(cat.entries)
}

/// Load the catalog under `paths` and list it: `(rows, truncated)`.
/// Blocking.
pub fn list_saved(
    paths: &CatalogPaths,
    live: &HashSet<String>,
    named_only: bool,
    limit: usize,
) -> std::io::Result<(Vec<SavedSessionInfo>, bool)> {
    Ok(rows(&load(paths)?, live, named_only, limit))
}

/// Junk ids out; each chain folded into its head; the Past tab's noise
/// rules on the conversation, except that a named one is never hidden, and
/// a compacted chain or a held session is never hidden for having few
/// messages (a fresh successor holds just the summary); newest
/// `updated_at` first.
fn rows(
    entries: &[CatalogEntry],
    live: &HashSet<String>,
    named_only: bool,
    limit: usize,
) -> (Vec<SavedSessionInfo>, bool) {
    let entries: Vec<&CatalogEntry> = entries.iter().filter(|e| !e.junk_id).collect();
    let by_id: HashMap<&str, usize> = entries.iter().enumerate().map(|(i, e)| (e.id.as_str(), i)).collect();
    let mut chains: HashMap<usize, Vec<usize>> = HashMap::new();
    for (i, head) in chain_heads(&entries, &by_id).into_iter().enumerate() {
        if let Some(head) = head {
            chains.entry(head).or_default().push(i);
        }
    }

    let mut out = Vec::new();
    for (head, members) in chains {
        let h = entries[head];
        let is_live = live.contains(&h.id);
        // Newest member first: a name/cwd the head lacks comes from the
        // most recent predecessor that has one.
        let mut chain: Vec<&CatalogEntry> = members.iter().map(|&i| entries[i]).collect();
        chain.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| b.id.cmp(&a.id)));
        let name = h.name.clone().or_else(|| chain.iter().find_map(|e| e.name.clone()));
        if named_only && name.is_none() {
            continue;
        }
        let cwd = h.cwd.clone().or_else(|| chain.iter().find_map(|e| e.cwd.clone()));
        if name.is_none() {
            // The cwd shown is the one judged (a successor has no start
            // record of its own).
            let sandbox = cwd.as_deref().is_some_and(is_sandbox_cwd);
            let few = noise_reasons(h).few_messages && members.len() == 1 && !is_live;
            if sandbox || few {
                continue;
            }
        }
        out.push(SavedSessionInfo {
            id: h.id.clone(),
            name,
            title: title(h),
            model: h.model.clone(),
            cwd,
            created_at: chain.iter().map(|e| e.created_at).min().unwrap_or(h.created_at),
            updated_at: h.updated_at,
            message_count: h.message_count,
            live: is_live,
        });
    }
    out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| b.id.cmp(&a.id)));
    let truncated = out.len() > limit;
    out.truncate(limit);
    (out, truncated)
}

/// Each entry's chain head: `compacted_into` followed to the last session
/// on disk, however long the chain. `None` for a cycle and anything that
/// leads into one: it cannot be resumed, so it is not listed. A successor
/// missing from disk ends the walk there (resuming that row reports the
/// missing successor). O(n): every entry is walked once.
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

/// Header title, else the first user message; one line, at most
/// [`SAVED_SESSION_TITLE_CHARS`] characters.
fn title(e: &CatalogEntry) -> String {
    let src = Some(e.title.as_str())
        .filter(|t| !t.trim().is_empty())
        .or(e.first_user.as_deref())
        .unwrap_or("");
    // One line, no control characters (a terminal client prints it).
    let line = src.split_whitespace().collect::<Vec<_>>().join(" ");
    let line: String = line.chars().filter(|c| !c.is_control()).collect();
    if line.chars().count() <= SAVED_SESSION_TITLE_CHARS {
        return line;
    }
    let mut cut: String = line.chars().take(SAVED_SESSION_TITLE_CHARS - 1).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Duration, Utc};
    use serde_json::json;
    use std::path::Path;

    fn seed(dir: &Path, id: &str, name: Option<&str>, title: &str, updated: DateTime<Utc>, msgs: usize, into: Option<&str>) {
        let mut v = json!({
            "id": id, "title": title, "name": name, "model": "claude-sonnet-4-5",
            "thinking_level": "medium",
            "created_at": updated - Duration::minutes(10), "updated_at": updated,
            "message_count": msgs,
        });
        if let Some(s) = into {
            v["compacted_into"] = json!(s);
        }
        // `api_messages` last: the header reader stops at it.
        let head = serde_json::to_string(&v).unwrap();
        let file = format!("{},\"api_messages\":[]}}", head.strip_suffix('}').unwrap());
        std::fs::write(dir.join(format!("{id}.json")), file).unwrap();
    }

    fn paths(root: &Path) -> CatalogPaths {
        // Session reads refuse a symlinked ancestor (macOS's /var → /private/var).
        let root = root.canonicalize().unwrap();
        let sessions_dir = root.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        CatalogPaths {
            index_path: sessions_dir.join("index.jsonl"),
            cache_path: root.join("catalog").join("session-previews.json"),
            sessions_dir,
        }
    }

    fn ids(rows: &[SavedSessionInfo]) -> Vec<&str> {
        rows.iter().map(|r| r.id.as_str()).collect()
    }

    #[test]
    fn chains_fold_into_their_head_and_noise_and_junk_stay_out() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = paths(tmp.path());
        let d = &p.sessions_dir;
        let t = Utc::now();
        seed(d, "20260101-000001-aaaa", Some("alpha"), "Alpha", t - Duration::hours(1), 6, None);
        seed(d, "20260101-000002-bbbb", None, "", t - Duration::hours(2), 6, None);
        // chain c1 → c2 → c3; the name moved to the head; the head holds
        // only the summary (2 messages) and is still listed.
        seed(d, "20260101-000003-cc01", None, "Gamma", t - Duration::hours(9), 40, Some("20260101-000003-cc02"));
        seed(d, "20260101-000003-cc02", None, "↳ Gamma", t - Duration::hours(6), 30, Some("20260101-000003-cc03"));
        seed(d, "20260101-000003-cc03", Some("gamma"), "↳ ↳ Gamma", t - Duration::hours(3), 2, None);
        // few messages: hidden unless live
        seed(d, "20260101-000004-dddd", None, "short", t - Duration::hours(4), 1, None);
        // junk id (stem/id not a session id), even though named
        seed(d, "scratch", Some("junk"), "junk", t, 9, None);
        // a two-session cycle cannot be resumed: never listed
        seed(d, "20260101-000005-ee01", None, "x", t, 9, Some("20260101-000005-ee02"));
        seed(d, "20260101-000005-ee02", None, "y", t, 9, Some("20260101-000005-ee01"));

        let none = HashSet::new();
        let (rows, truncated) = list_saved(&p, &none, false, 200).unwrap();
        assert!(!truncated);
        assert_eq!(ids(&rows), ["20260101-000001-aaaa", "20260101-000002-bbbb", "20260101-000003-cc03"]);
        let head = &rows[2];
        assert_eq!(head.name.as_deref(), Some("gamma"));
        assert_eq!(head.created_at, t - Duration::hours(9) - Duration::minutes(10), "the conversation's start");
        assert_eq!(head.message_count, Some(2));
        assert!(rows.iter().all(|r| !r.live));

        let (named, _) = list_saved(&p, &none, true, 200).unwrap();
        assert_eq!(ids(&named), ["20260101-000001-aaaa", "20260101-000003-cc03"]);

        let (two, truncated) = list_saved(&p, &none, false, 2).unwrap();
        assert!(truncated);
        assert_eq!(ids(&two), ["20260101-000001-aaaa", "20260101-000002-bbbb"]);
        let (_, truncated) = list_saved(&p, &none, false, 3).unwrap();
        assert!(!truncated, "exactly `limit` matched");

        let live: HashSet<String> = ["20260101-000004-dddd".to_string()].into();
        let (rows, _) = list_saved(&p, &live, false, 200).unwrap();
        let d = rows.iter().find(|r| r.id == "20260101-000004-dddd").expect("a held session is listed");
        assert!(d.live);
    }

    #[test]
    fn a_predecessors_name_carries_to_a_head_without_one() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = paths(tmp.path());
        let t = Utc::now();
        seed(&p.sessions_dir, "20260101-000001-aa01", Some("old"), "A", t - Duration::hours(2), 9, Some("20260101-000001-aa02"));
        seed(&p.sessions_dir, "20260101-000001-aa02", None, "↳ A", t - Duration::hours(1), 9, None);
        let (rows, _) = list_saved(&p, &HashSet::new(), true, 200).unwrap();
        assert_eq!(ids(&rows), ["20260101-000001-aa02"]);
        assert_eq!(rows[0].name.as_deref(), Some("old"));
    }

    #[test]
    fn titles_fall_back_to_the_first_message_on_one_line_and_are_clipped() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = paths(tmp.path());
        seed(&p.sessions_dir, "20260101-000001-aaaa", None, &"word ".repeat(100), Utc::now(), 9, None);
        let (rows, _) = list_saved(&p, &HashSet::new(), false, 200).unwrap();
        assert_eq!(rows[0].title.chars().count(), SAVED_SESSION_TITLE_CHARS);
        assert!(rows[0].title.ends_with('…'));
        let mut e = agent_core::session_catalog::load_catalog(&p).unwrap().entries.remove(0);
        e.title = "  ".into();
        e.first_user = Some("fix the\n  build".into());
        assert_eq!(title(&e), "fix the build");
    }

    #[test]
    fn limits_default_to_200_and_cap_at_1000() {
        assert_eq!(effective_limit(None), 200);
        assert_eq!(effective_limit(Some(5)), 5);
        assert_eq!(effective_limit(Some(50_000)), 1000);
    }

    #[test]
    fn a_missing_sessions_dir_is_an_empty_list() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = CatalogPaths {
            sessions_dir: tmp.path().join("nope"),
            index_path: tmp.path().join("index.jsonl"),
            cache_path: tmp.path().join("cache.json"),
        };
        let (rows, truncated) = list_saved(&p, &HashSet::new(), false, 200).unwrap();
        assert!(rows.is_empty() && !truncated);
    }

    /// `index.jsonl` start record: `id` ran in `cwd`.
    fn started_in(p: &CatalogPaths, id: &str, cwd: &str) {
        use std::io::Write;
        let line = json!({"session_id": id, "event": "start", "cwd": cwd});
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&p.index_path).unwrap();
        writeln!(f, "{line}").unwrap();
    }

    /// A name is the user saying "keep this", and names are unique: a named
    /// conversation is listed whatever its size or cwd. Unnamed noise stays
    /// out, judged on the cwd the row would show (a successor has none).
    #[test]
    fn named_sessions_are_never_noise_and_the_shown_cwd_is_the_one_judged() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = paths(tmp.path());
        let d = &p.sessions_dir;
        let t = Utc::now();
        let sandbox = "/tmp/synaps-saved-sessions-test/scratch";
        seed(d, "20260101-000001-aaaa", Some("tiny"), "named, 1 message", t - Duration::hours(1), 1, None);
        seed(d, "20260101-000002-bbbb", Some("tmpwork"), "named, in /tmp", t - Duration::hours(2), 9, None);
        started_in(&p, "20260101-000002-bbbb", sandbox);
        seed(d, "20260101-000003-cccc", None, "unnamed, in /tmp", t - Duration::hours(3), 9, None);
        started_in(&p, "20260101-000003-cccc", sandbox);
        // An unnamed chain begun in /tmp: the head has no start record.
        seed(d, "20260101-000004-dd01", None, "x", t - Duration::hours(5), 9, Some("20260101-000004-dd02"));
        seed(d, "20260101-000004-dd02", None, "↳ x", t - Duration::hours(4), 2, None);
        started_in(&p, "20260101-000004-dd01", sandbox);
        seed(d, "20260101-000005-eeee", None, "unnamed, 1 message", t - Duration::hours(6), 1, None);

        let (rows, _) = list_saved(&p, &HashSet::new(), false, 200).unwrap();
        assert_eq!(ids(&rows), ["20260101-000001-aaaa", "20260101-000002-bbbb"]);
        assert_eq!(rows[1].cwd.as_deref(), Some(Path::new(sandbox)));
        let (named, _) = list_saved(&p, &HashSet::new(), true, 200).unwrap();
        assert_eq!(ids(&named), ids(&rows));
    }

    /// A long-lived conversation compacts many times: the whole chain folds
    /// into one row, `created_at` its first session's. A cycle, and a tail
    /// leading into one, is never listed.
    #[test]
    fn a_chain_of_any_length_is_one_row_and_a_cycle_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = paths(tmp.path());
        let d = &p.sessions_dir;
        let t = Utc::now();
        let id = |i: usize| format!("20260101-000001-a{i:03}");
        let n = 60;
        for i in 0..n {
            let next = (i + 1 < n).then(|| id(i + 1));
            let name = (i == 0).then_some("long");
            seed(d, &id(i), name, "L", t - Duration::hours((n - i) as i64), 9, next.as_deref());
        }
        seed(d, "20260101-000002-bb01", None, "tail", t, 9, Some("20260101-000002-bb02"));
        seed(d, "20260101-000002-bb02", None, "c1", t, 9, Some("20260101-000002-bb03"));
        seed(d, "20260101-000002-bb03", None, "c2", t, 9, Some("20260101-000002-bb02"));

        let (rows, _) = list_saved(&p, &HashSet::new(), false, 200).unwrap();
        assert_eq!(ids(&rows), [id(n - 1).as_str()]);
        assert_eq!(rows[0].name.as_deref(), Some("long"), "the first session's name, 59 hops back");
        assert_eq!(rows[0].created_at, t - Duration::hours(n as i64) - Duration::minutes(10));
    }

    /// After a LinkedSuccessor compaction the held session keeps its actor
    /// id while its journal is the successor's: the row for the journal is
    /// `live`.
    #[test]
    fn a_held_session_is_live_by_its_journal_id_after_compaction() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = paths(tmp.path());
        let d = &p.sessions_dir;
        let t = Utc::now();
        seed(d, "20260101-000001-aa01", Some("g"), "G", t - Duration::hours(2), 40, Some("20260101-000001-aa02"));
        seed(d, "20260101-000001-aa02", None, "↳ G", t - Duration::hours(1), 2, None);
        seed(d, "20260101-000002-bbbb", None, "other", t, 9, None);
        let mut held = crate::session::handle::echo::meta_for(&crate::session::SessionId::from("20260101-000001-aa01"));
        held.journal_id = "20260101-000001-aa02".into();
        let live = live_ids(&[held]);
        assert!(live.contains("20260101-000001-aa02") && live.contains("20260101-000001-aa01"));
        let (rows, _) = list_saved(&p, &live, false, 200).unwrap();
        assert_eq!(ids(&rows), ["20260101-000002-bbbb", "20260101-000001-aa02"]);
        assert!(!rows[0].live);
        assert!(rows[1].live, "matched on the journal id, not the actor id");
    }

    #[test]
    fn titles_carry_no_control_characters() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = paths(tmp.path());
        seed(&p.sessions_dir, "20260101-000001-aaaa", None, "a\u{1b}[31mred\u{7}\u{9b}x", Utc::now(), 9, None);
        let (rows, _) = list_saved(&p, &HashSet::new(), false, 200).unwrap();
        assert_eq!(rows[0].title, "a[31mredx");
    }

    /// Many concurrent listings run ONE catalog load: the ones that arrive
    /// while it runs share it. A later request loads afresh.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_requests_share_one_load_and_a_later_one_reloads() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = paths(tmp.path());
        seed(&p.sessions_dir, "20260101-000001-aaaa", Some("a"), "A", Utc::now(), 9, None);
        let loader = Arc::new(SavedLoader::default());
        let held = loader.hold().await;
        let n = 24;
        let jobs: Vec<_> = (0..n)
            .map(|_| {
                let (loader, p) = (Arc::clone(&loader), p.clone());
                tokio::spawn(async move { loader.entries(p).await })
            })
            .collect();
        let t0 = Instant::now();
        while loader.requests() < n {
            assert!(t0.elapsed() < std::time::Duration::from_secs(10), "requests never arrived");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(loader.loads(), 0, "nothing loads while the lock is held");
        drop(held);
        for j in jobs {
            assert_eq!(j.await.unwrap().unwrap().len(), 1);
        }
        assert_eq!(loader.loads(), 1, "{n} requests, one load");
        loader.entries(p.clone()).await.unwrap();
        assert_eq!(loader.loads(), 2, "a request after the load reads the disk again");
    }
}
