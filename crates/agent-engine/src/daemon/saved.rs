//! `ClientFrame::SavedSessions`: the saved sessions on disk, for clients
//! that cannot read the sessions dir themselves (a thin or remote client).
//!
//! Same source and filter as the TUI's `/sessions` Past tab
//! (`session_catalog::load_catalog` + its noise rules), with two
//! differences that suit a session picker: a compacted predecessor folds
//! into its chain head (one row per conversation), and sessions this
//! daemon holds stay listed, flagged `live`. The catalog read is blocking
//! disk I/O, so it runs on `spawn_blocking`; the connection task awaits it
//! and no other connection waits.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use agent_core::session_catalog::{load_catalog, noise_reasons, CatalogEntry, CatalogPaths, PreviewCache};

use super::DaemonState;
use crate::session::wire::{
    DaemonFrame, SavedSessionInfo, SAVED_SESSIONS_DEFAULT_LIMIT, SAVED_SESSIONS_MAX_LIMIT, SAVED_SESSION_TITLE_CHARS,
};

/// `compacted_into` hops followed before a chain counts as broken (cycle).
const MAX_CHAIN_HOPS: usize = 32;

/// Serve `SavedSessions{named_only, limit}`. Always one frame:
/// `SavedSessionList`, or `Error` when the sessions dir cannot be read.
pub async fn saved_sessions(state: &Arc<DaemonState>, named_only: bool, limit: Option<u32>) -> DaemonFrame {
    // Held sessions by journal id (≠ actor id after a LinkedSuccessor
    // compaction) and by actor id.
    let live: HashSet<String> = state
        .session_metas()
        .into_iter()
        .flat_map(|m| [m.journal_id, m.id.as_str().to_string()])
        .filter(|id| !id.is_empty())
        .collect();
    let paths = CatalogPaths::resolve();
    let limit = effective_limit(limit);
    match tokio::task::spawn_blocking(move || list_saved(&paths, &live, named_only, limit)).await {
        Ok(Ok((sessions, truncated))) => DaemonFrame::SavedSessionList { sessions, truncated },
        Ok(Err(e)) => DaemonFrame::Error { session_id: None, message: format!("saved sessions: {e}") },
        Err(e) => DaemonFrame::Error { session_id: None, message: format!("saved sessions: listing failed: {e}") },
    }
}

/// Absent → 200; capped at 1000.
pub fn effective_limit(limit: Option<u32>) -> usize {
    limit.unwrap_or(SAVED_SESSIONS_DEFAULT_LIMIT).min(SAVED_SESSIONS_MAX_LIMIT) as usize
}

/// Load the catalog under `paths` (headers + the preview cache, read only)
/// and list it: `(rows, truncated)`. Blocking.
pub fn list_saved(
    paths: &CatalogPaths,
    live: &HashSet<String>,
    named_only: bool,
    limit: usize,
) -> std::io::Result<(Vec<SavedSessionInfo>, bool)> {
    let mut cat = load_catalog(paths)?;
    cat.apply_previews(&PreviewCache::load(&paths.cache_path));
    Ok(rows(cat.entries, live, named_only, limit))
}

/// Junk ids out; each chain folded into its head; the Past tab's noise
/// rules on the head (a chain head or a held session is never hidden for
/// having few messages: a fresh successor holds just the summary); newest
/// `updated_at` first.
fn rows(
    entries: Vec<CatalogEntry>,
    live: &HashSet<String>,
    named_only: bool,
    limit: usize,
) -> (Vec<SavedSessionInfo>, bool) {
    let entries: Vec<CatalogEntry> = entries.into_iter().filter(|e| !e.junk_id).collect();
    let by_id: HashMap<&str, usize> = entries.iter().enumerate().map(|(i, e)| (e.id.as_str(), i)).collect();
    let mut chains: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..entries.len() {
        if let Some(head) = chain_head(&entries, &by_id, i) {
            chains.entry(head).or_default().push(i);
        }
    }

    let mut out = Vec::new();
    for (head, members) in chains {
        let h = &entries[head];
        let is_live = live.contains(&h.id);
        let noise = noise_reasons(h);
        if noise.sandbox || (noise.few_messages && members.len() == 1 && !is_live) {
            continue;
        }
        // Newest member first: a name/cwd the head lacks comes from the
        // most recent predecessor that has one.
        let mut chain: Vec<&CatalogEntry> = members.iter().map(|&i| &entries[i]).collect();
        chain.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| b.id.cmp(&a.id)));
        let name = h.name.clone().or_else(|| chain.iter().find_map(|e| e.name.clone()));
        if named_only && name.is_none() {
            continue;
        }
        out.push(SavedSessionInfo {
            id: h.id.clone(),
            name,
            title: title(h),
            model: h.model.clone(),
            cwd: h.cwd.clone().or_else(|| chain.iter().find_map(|e| e.cwd.clone())),
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

/// Follow `compacted_into` from `i` to the last session on disk. `None` on
/// a cycle (or a chain longer than [`MAX_CHAIN_HOPS`]): it cannot be
/// resumed, so it is not listed. A successor missing from disk ends the
/// walk there.
fn chain_head(entries: &[CatalogEntry], by_id: &HashMap<&str, usize>, i: usize) -> Option<usize> {
    let mut cur = i;
    for _ in 0..=MAX_CHAIN_HOPS {
        match entries[cur].compacted_into.as_deref().and_then(|next| by_id.get(next)) {
            Some(&next) => cur = next,
            None => return Some(cur),
        }
    }
    None
}

/// Header title, else the first user message; one line, at most
/// [`SAVED_SESSION_TITLE_CHARS`] characters.
fn title(e: &CatalogEntry) -> String {
    let src = Some(e.title.as_str())
        .filter(|t| !t.trim().is_empty())
        .or(e.first_user.as_deref())
        .unwrap_or("");
    let line = src.split_whitespace().collect::<Vec<_>>().join(" ");
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
        let mut e = load_catalog(&p).unwrap().entries.remove(0);
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
}
