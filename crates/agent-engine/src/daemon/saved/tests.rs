use super::*;
use chrono::{Duration as Days, TimeZone};
use serde_json::json;
use std::path::Path;

/// A session header on disk (`api_messages` last: the header reader stops
/// at it).
#[allow(clippy::too_many_arguments)]
fn seed_full(
    dir: &Path,
    id: &str,
    name: Option<&str>,
    title: &str,
    updated: DateTime<Utc>,
    msgs: usize,
    into: Option<&str>,
    parent: Option<&str>,
) {
    let mut v = json!({
        "id": id, "title": title, "name": name, "model": "claude-sonnet-4-5",
        "thinking_level": "medium",
        "created_at": updated - Days::minutes(10), "updated_at": updated,
        "session_cost": 0.25, "message_count": msgs,
    });
    if let Some(s) = into {
        v["compacted_into"] = json!(s);
    }
    if let Some(s) = parent {
        v["parent_session"] = json!(s);
    }
    let head = serde_json::to_string(&v).unwrap();
    let file = format!("{},\"api_messages\":[]}}", head.strip_suffix('}').unwrap());
    std::fs::write(dir.join(format!("{id}.json")), file).unwrap();
}

/// A chain member: `into` its successor ([`link`] gives the successor its
/// `parent_session`, as a compaction writes it).
fn seed(dir: &Path, id: &str, name: Option<&str>, title: &str, updated: DateTime<Utc>, msgs: usize, into: Option<&str>) {
    seed_full(dir, id, name, title, updated, msgs, into, None);
}

/// Every seeded successor names its predecessor (`parent_session`).
fn link(dir: &Path) {
    let read = |id: &str| -> Option<serde_json::Value> {
        serde_json::from_str(&std::fs::read_to_string(dir.join(format!("{id}.json"))).ok()?).ok()
    };
    for e in std::fs::read_dir(dir).unwrap() {
        let name = e.unwrap().file_name().into_string().unwrap();
        let Some(id) = name.strip_suffix(".json") else { continue };
        let Some(next) = read(id).and_then(|v| v["compacted_into"].as_str().map(String::from)) else { continue };
        let Some(succ) = read(&next) else { continue };
        if succ.get("parent_session").is_none() {
            let path = dir.join(format!("{next}.json"));
            let text = std::fs::read_to_string(&path).unwrap();
            let text = text.replacen("\"api_messages\"", &format!("\"parent_session\":\"{id}\",\"api_messages\""), 1);
            std::fs::write(path, text).unwrap();
        }
    }
}

/// The folded catalog under `p`, chains linked.
fn facts_of(p: &CatalogPaths) -> Vec<SavedFacts> {
    if p.sessions_dir.exists() {
        link(&p.sessions_dir);
    }
    fold(&load(p).unwrap())
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

/// Every listable row, nothing held (one big page).
fn list(p: &CatalogPaths, named_only: bool) -> Vec<SessionMeta> {
    page(&facts_of(p), |_| false, None, 10_000, named_only).0
}

fn ids(rows: &[SessionMeta]) -> Vec<&str> {
    rows.iter().map(|r| r.journal_id.as_str()).collect()
}

/// `index.jsonl` start record: `id` ran in `cwd`.
fn started_in(p: &CatalogPaths, id: &str, cwd: &str) {
    use std::io::Write;
    let line = json!({"session_id": id, "event": "start", "cwd": cwd});
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&p.index_path).unwrap();
    writeln!(f, "{line}").unwrap();
}

#[test]
fn chains_fold_into_their_head_and_noise_and_junk_stay_out() {
    let tmp = tempfile::TempDir::new().unwrap();
    let p = paths(tmp.path());
    let d = &p.sessions_dir;
    let t = Utc::now();
    seed(d, "20260101-000001-aaaa", Some("alpha"), "Alpha", t - Days::hours(1), 6, None);
    seed(d, "20260101-000002-bbbb", None, "", t - Days::hours(2), 6, None);
    // chain c1 → c2 → c3; the name moved to the head; the head holds only
    // the summary (2 messages) and is still listed (chained).
    seed(d, "20260101-000003-cc01", None, "Gamma", t - Days::hours(9), 40, Some("20260101-000003-cc02"));
    seed(d, "20260101-000003-cc02", None, "↳ Gamma", t - Days::hours(6), 30, Some("20260101-000003-cc03"));
    seed(d, "20260101-000003-cc03", Some("gamma"), "↳ ↳ Gamma", t - Days::hours(3), 2, None);
    // few messages, not chained: hidden
    seed(d, "20260101-000004-dddd", None, "short", t - Days::hours(4), 1, None);
    // junk id (stem/id not a session id), even though named
    seed(d, "scratch", Some("junk"), "junk", t, 9, None);
    // a two-session cycle cannot be resumed: never listed
    seed_full(d, "20260101-000005-ee01", None, "x", t, 9, Some("20260101-000005-ee02"), None);
    seed_full(d, "20260101-000005-ee02", None, "y", t, 9, Some("20260101-000005-ee01"), None);

    let rows = list(&p, false);
    assert_eq!(ids(&rows), ["20260101-000001-aaaa", "20260101-000002-bbbb", "20260101-000003-cc03"]);
    let head = &rows[2];
    assert_eq!(head.name.as_deref(), Some("gamma"));
    assert_eq!(head.created_at, t - Days::hours(9) - Days::minutes(10), "the conversation's start");
    assert_eq!(head.messages, 2);
    assert!(rows.iter().all(|r| r.lifecycle == SessionLifecycle::Saved && r.host_pid == 0));
    assert!(rows.iter().all(|r| r.id.as_str() == r.journal_id), "a saved row's id is its journal id");

    let named = list(&p, true);
    assert_eq!(ids(&named), ["20260101-000001-aaaa", "20260101-000003-cc03"]);
}

#[test]
fn a_predecessors_name_carries_to_a_head_without_one() {
    let tmp = tempfile::TempDir::new().unwrap();
    let p = paths(tmp.path());
    let t = Utc::now();
    seed(&p.sessions_dir, "20260101-000001-aa01", Some("old"), "A", t - Days::hours(2), 9, Some("20260101-000001-aa02"));
    seed(&p.sessions_dir, "20260101-000001-aa02", None, "↳ A", t - Days::hours(1), 9, None);
    let rows = list(&p, true);
    assert_eq!(ids(&rows), ["20260101-000001-aa02"]);
    assert_eq!(rows[0].name.as_deref(), Some("old"));
}

#[test]
fn titles_fall_back_to_the_first_message_on_one_line_and_are_clipped() {
    let tmp = tempfile::TempDir::new().unwrap();
    let p = paths(tmp.path());
    seed(&p.sessions_dir, "20260101-000001-aaaa", None, &"word ".repeat(100), Utc::now(), 9, None);
    let rows = list(&p, false);
    let title = rows[0].title.as_deref().unwrap();
    assert_eq!(title.chars().count(), agent_core::session_catalog::LISTING_TITLE_CHARS);
    assert!(title.ends_with('…'));
    let mut f = facts_of(&p).remove(0);
    f.header_title = "  ".into();
    f.first_user = Some("fix the\n  build".into());
    assert_eq!(saved_row(&f, false).unwrap().title.as_deref(), Some("fix the build"));
    f.first_user = None;
    assert_eq!(saved_row(&f, false).unwrap().title, None, "no title at all");
    f.header_title = "a\u{1b}[31mred\u{7}\u{9b}x".into();
    assert_eq!(saved_row(&f, false).unwrap().title.as_deref(), Some("a[31mredx"));
}

#[test]
fn limits_default_to_50_and_cap_at_200() {
    assert_eq!(effective_limit(None), 50);
    assert_eq!(effective_limit(Some(5)), 5);
    assert_eq!(effective_limit(Some(0)), 1);
    assert_eq!(effective_limit(Some(50_000)), 200);
}

#[test]
fn a_missing_sessions_dir_is_an_empty_list() {
    let tmp = tempfile::TempDir::new().unwrap();
    let p = CatalogPaths {
        sessions_dir: tmp.path().join("nope"),
        index_path: tmp.path().join("index.jsonl"),
        cache_path: tmp.path().join("cache.json"),
    };
    assert!(list(&p, false).is_empty());
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
    seed(d, "20260101-000001-aaaa", Some("tiny"), "named, 1 message", t - Days::hours(1), 1, None);
    seed(d, "20260101-000002-bbbb", Some("tmpwork"), "named, in /tmp", t - Days::hours(2), 9, None);
    started_in(&p, "20260101-000002-bbbb", sandbox);
    seed(d, "20260101-000003-cccc", None, "unnamed, in /tmp", t - Days::hours(3), 9, None);
    started_in(&p, "20260101-000003-cccc", sandbox);
    // An unnamed chain begun in /tmp: the head has no start record.
    seed(d, "20260101-000004-dd01", None, "x", t - Days::hours(5), 9, Some("20260101-000004-dd02"));
    seed(d, "20260101-000004-dd02", None, "↳ x", t - Days::hours(4), 2, None);
    started_in(&p, "20260101-000004-dd01", sandbox);
    seed(d, "20260101-000005-eeee", None, "unnamed, 1 message", t - Days::hours(6), 1, None);

    let rows = list(&p, false);
    assert_eq!(ids(&rows), ["20260101-000001-aaaa", "20260101-000002-bbbb"]);
    assert_eq!(rows[1].cwd.as_deref(), Some(Path::new(sandbox)));
    assert_eq!(ids(&list(&p, true)), ids(&rows));
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
        seed(d, &id(i), name, "L", t - Days::hours((n - i) as i64), 9, next.as_deref());
    }
    seed_full(d, "20260101-000002-bb01", None, "tail", t, 9, Some("20260101-000002-bb02"), None);
    seed_full(d, "20260101-000002-bb02", None, "c1", t, 9, Some("20260101-000002-bb03"), None);
    seed_full(d, "20260101-000002-bb03", None, "c2", t, 9, Some("20260101-000002-bb02"), None);

    let rows = list(&p, false);
    assert_eq!(ids(&rows), [id(n - 1).as_str()]);
    assert_eq!(rows[0].name.as_deref(), Some("long"), "the first session's name, 59 hops back");
    assert_eq!(rows[0].created_at, t - Days::hours(n as i64) - Days::minutes(10));
}

// ── held ─────────────────────────────────────────────────────────────────────

fn handle(actor: &str, journal: &str, supersedes: &[&str]) -> (SessionHandle, tokio::task::JoinHandle<()>) {
    let (h, task) = SessionHandle::echo_for_test(SessionId::from(actor));
    let mut cell = crate::session::handle::JournalCell::new(journal.into(), Utc::now());
    cell.supersedes = supersedes.iter().map(|s| s.to_string()).collect();
    h.set_journal_for_test(cell);
    (h, task)
}

/// The one rule: a head is out of the pages iff a live session holds it.
/// After a LinkedSuccessor compaction whose successor is not on disk yet,
/// the disk chain ends at the predecessor, which the actor still holds.
/// After `/clear` the actor keeps its id but no longer holds its old
/// conversation: that one is saved and listed.
#[tokio::test]
async fn held_excludes_a_compacted_predecessor_but_never_a_cleared_id() {
    let tmp = tempfile::TempDir::new().unwrap();
    let p = paths(tmp.path());
    let d = &p.sessions_dir;
    let t = Utc::now();
    const PRED: &str = "20260101-000001-aa01"; // compacted into AA02, not on disk yet
    const CLEARED: &str = "20260101-000002-bbbb"; // actor BBBB, cleared to NNNN
    const HELD: &str = "20260101-000003-cccc";
    const FREE: &str = "20260101-000004-dddd";
    seed_full(d, PRED, Some("pred"), "P", t - Days::hours(1), 9, Some("20260101-000001-aa02"), None);
    seed(d, CLEARED, Some("cleared"), "B", t - Days::hours(2), 9, None);
    seed(d, HELD, None, "C", t - Days::hours(3), 9, None);
    seed(d, FREE, None, "D", t - Days::hours(4), 9, None);
    let facts = facts_of(&p);
    assert_eq!(facts.iter().map(|f| f.journal_id.as_str()).collect::<Vec<_>>(), [PRED, CLEARED, HELD, FREE]);

    let (compacted, _a) = handle(PRED, "20260101-000001-aa02", &[PRED]);
    let (cleared, _b) = handle(CLEARED, "20260101-000009-nnnn", &[]);
    let (plain, _c) = handle(HELD, HELD, &[]);
    let handles = [compacted, cleared, plain];
    assert!(held(&handles, PRED), "the actor holds what it compacted away from");
    assert!(!held(&handles, CLEARED), "an actor id is not a conversation");
    assert!(held(&handles, HELD));
    let (rows, next) = page(&facts, |c| held(&handles, c), None, 50, false);
    assert_eq!(ids(&rows), [CLEARED, FREE]);
    assert_eq!(next, None);
}

// ── paging ───────────────────────────────────────────────────────────────────

/// Keyset paging: every listable row exactly once, in order, whatever the
/// page size, including runs of equal `updated_at` that straddle a page
/// boundary, with held and unlisted rows in between.
#[test]
fn cursor_paging_lists_every_row_once_across_equal_updated_at() {
    let tmp = tempfile::TempDir::new().unwrap();
    let p = paths(tmp.path());
    let d = &p.sessions_dir;
    let t0 = Utc.with_ymd_and_hms(2026, 11, 1, 12, 0, 0).unwrap();
    let mut n = 0;
    for group in 0..7i64 {
        // groups of 1..=7 sessions sharing one updated_at
        for k in 0..=group {
            let id = format!("20261101-{:06}-{:04x}", group * 100 + k, n);
            let msgs = if n % 5 == 4 { 1 } else { 9 }; // every fifth is noise
            seed(d, &id, None, "x", t0 - Days::hours(group), msgs, None);
            n += 1;
        }
    }
    let facts = facts_of(&p);
    assert_eq!(facts.len(), n);
    let held_ids: HashSet<String> = facts.iter().step_by(6).map(|f| f.journal_id.clone()).collect();
    let is_held = |c: &str| held_ids.contains(c);
    let (want, end) = page(&facts, is_held, None, 10_000, false);
    assert_eq!(end, None);
    assert!(want.len() > 10 && want.len() < n, "{} of {n}", want.len());
    assert!(want.iter().all(|r| !held_ids.contains(&r.journal_id)));
    for limit in [1, 2, 3, 4, 5, 7, 50] {
        let (mut got, mut before) = (Vec::new(), None::<CatalogCursor>);
        loop {
            let (rows, next) = page(&facts, is_held, before.as_ref(), limit, false);
            assert!(rows.len() <= limit);
            got.extend(rows);
            match next {
                Some(c) => {
                    assert_eq!(c.id, got.last().unwrap().journal_id, "next is the last row's cursor");
                    before = Some(c);
                }
                None => break,
            }
        }
        assert_eq!(ids(&got), ids(&want), "limit {limit}");
    }
    // A cursor that names no row (it left the catalog) still pages on.
    let mid = &want[want.len() / 2];
    let gone = CatalogCursor { updated_at: mid.updated_at.unwrap(), id: format!("{}z", mid.journal_id) };
    let (rows, _) = page(&facts, is_held, Some(&gone), 10_000, false);
    assert_eq!(ids(&rows), ids(&want[want.len() / 2..]));
}

// ── the property, disk driver ────────────────────────────────────────────────

/// One conversation as its actor knows it, written to disk; the disk
/// driver must recover exactly these facts. PR 3: run the same cases
/// through the actor driver (`Presence.saved`) and assert
/// `saved_row(disk) == saved_row(actor)`.
struct Case {
    what: &'static str,
    /// Oldest first; the last is the head.
    chain: Vec<Member>,
    cwd: Option<&'static str>,
    listed: bool,
}

struct Member {
    id: &'static str,
    name: Option<&'static str>,
    title: &'static str,
    created: DateTime<Utc>,
    updated: DateTime<Utc>,
    messages: usize,
    cost: f64,
}

fn write_case(p: &CatalogPaths, c: &Case) {
    use agent_core::core::session_journal::{save_session_in_dir, SessionPersistence};
    for (i, m) in c.chain.iter().enumerate() {
        let mut s = agent_core::session::Session::new("claude-opus-4-6", "medium", None);
        s.id = m.id.into();
        s.name = m.name.map(Into::into);
        s.title = m.title.into();
        s.created_at = m.created;
        s.updated_at = m.updated;
        s.session_cost = m.cost;
        s.api_messages = (0..m.messages)
            .map(|k| {
                let (role, text) = if k % 2 == 0 { ("user", format!("ask {} #{k}", m.id)) } else { ("assistant", "ok".into()) };
                Arc::new(json!({"role": role, "content": text}))
            })
            .collect();
        s.parent_session = (i > 0).then(|| c.chain[i - 1].id.to_string());
        s.compacted_into = c.chain.get(i + 1).map(|n| n.id.to_string());
        save_session_in_dir(&p.sessions_dir, &s, SessionPersistence::Json).unwrap();
    }
    if let Some(cwd) = c.cwd {
        started_in(p, c.chain[0].id, cwd);
    }
}

/// The facts the conversation's actor holds (what PR 3's actor driver
/// will fill), independent of the disk.
fn actor_facts(c: &Case) -> SavedFacts {
    let head = c.chain.last().unwrap();
    SavedFacts {
        journal_id: head.id.into(),
        on_disk: true,
        junk_id: false,
        name: c.chain.iter().rev().find_map(|m| m.name.map(String::from)),
        header_title: head.title.into(),
        first_user: (head.messages > 0).then(|| format!("ask {} #0", head.id)),
        model: "claude-opus-4-6".into(),
        cwd: c.cwd.map(PathBuf::from),
        cost_usd: head.cost,
        created_at: c.chain[0].created,
        updated_at: head.updated,
        messages: Some(head.messages),
        chained: c.chain.len() > 1,
    }
}

#[test]
fn the_disk_driver_recovers_the_conversations_facts_and_row() {
    let t = Utc.with_ymd_and_hms(2026, 11, 10, 12, 0, 0).unwrap();
    let h = |n: i64| t - Days::hours(n);
    let m = |id, name, title, created, updated, messages| Member { id, name, title, created, updated, messages, cost: 1.25 };
    let home = "/home/u/Projects/x";
    let sandbox = "/tmp/synaps-saved-property/x";
    let cases = vec![
        Case { what: "named", chain: vec![m("20261110-000001-aaaa", Some("alpha"), "Alpha", h(9), h(8), 6)], cwd: Some(home), listed: true },
        Case { what: "unnamed", chain: vec![m("20261110-000002-bbbb", None, "", h(9), h(7), 6)], cwd: Some(home), listed: true },
        Case { what: "unnamed, sandbox cwd", chain: vec![m("20261110-000003-cccc", None, "C", h(9), h(6), 6)], cwd: Some(sandbox), listed: false },
        Case { what: "named, sandbox cwd", chain: vec![m("20261110-000004-dddd", Some("tmpwork"), "D", h(9), h(5), 6)], cwd: Some(sandbox), listed: true },
        Case { what: "unnamed, few messages", chain: vec![m("20261110-000005-eeee", None, "E", h(9), h(4), 1)], cwd: Some(home), listed: false },
        Case { what: "named, few messages", chain: vec![m("20261110-000006-ffff", Some("tiny"), "F", h(9), h(3), 1)], cwd: None, listed: true },
        Case {
            what: "chained, one message on the head",
            chain: vec![
                m("20261110-000007-aa01", None, "G", h(30), h(20), 40),
                m("20261110-000007-aa02", None, "↳ G", h(20), h(2), 1),
            ],
            cwd: Some(home),
            listed: true,
        },
        Case {
            what: "resumed head: renamed after a resume, saved again days later",
            chain: vec![
                m("20261110-000008-bb01", Some("old"), "H", h(90), h(80), 30),
                m("20261110-000008-bb02", Some("new"), "↳ H", h(80), h(1), 12),
            ],
            cwd: Some(home),
            listed: true,
        },
    ];
    let tmp = tempfile::TempDir::new().unwrap();
    let p = paths(tmp.path());
    for c in &cases {
        write_case(&p, c);
    }
    // The previews a client built (first user message).
    let entries = load_catalog_with(&p, 1).unwrap().entries;
    let mut cache = PreviewCache::load(&p.cache_path);
    cache.refresh(&p.sessions_dir, &entries, &std::sync::atomic::AtomicBool::new(false), &mut |_, _| {});
    cache.save(&p.cache_path).unwrap();

    let disk = facts_of(&p);
    assert_eq!(disk.len(), cases.len());
    for c in &cases {
        let want = actor_facts(c);
        let got = disk.iter().find(|f| f.journal_id == want.journal_id).unwrap_or_else(|| panic!("{}: not folded", c.what));
        assert_eq!(got, &want, "{}", c.what);
        let row = saved_row(got, false);
        assert_eq!(row.is_some(), c.listed, "{}", c.what);
        assert_eq!(row, saved_row(&want, false), "{}", c.what);
        if let Some(row) = row {
            assert_eq!(row.title.as_deref(), Some(listing_title(c.chain.last().unwrap().title, want.first_user.as_deref()).as_str()).filter(|t| !t.is_empty()));
            assert_eq!(saved_row(got, true).is_some(), want.name.is_some(), "{}: named_only", c.what);
        }
    }
    // Not on disk (a zero-turn session) and a junk id are never listed.
    let mut f = actor_facts(&cases[0]);
    f.on_disk = false;
    assert_eq!(saved_row(&f, false), None);
    let mut f = actor_facts(&cases[0]);
    f.junk_id = true;
    assert_eq!(saved_row(&f, false), None);
}

// ── loader ───────────────────────────────────────────────────────────────────

/// Many concurrent first pages run ONE catalog load: the ones that queue
/// before it starts share it. A later first page loads afresh.
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
            tokio::spawn(async move { loader.facts(p, true).await })
        })
        .collect();
    let t0 = Instant::now();
    while loader.requests() < n {
        assert!(t0.elapsed() < Duration::from_secs(10), "requests never arrived");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(loader.loads(), 0, "nothing loads while the lock is held");
    drop(held);
    for j in jobs {
        assert_eq!(j.await.unwrap().unwrap().len(), 1);
    }
    assert_eq!(loader.loads(), 1, "{n} requests, one load");
    loader.facts(p.clone(), true).await.unwrap();
    assert_eq!(loader.loads(), 2, "a first page after the load reads the disk again");
}

/// A continuation page reuses a load inside the reuse window; a first page
/// never does; past the window a continuation reloads too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continuation_pages_reuse_a_recent_load_and_first_pages_never_do() {
    let tmp = tempfile::TempDir::new().unwrap();
    let p = paths(tmp.path());
    seed(&p.sessions_dir, "20260101-000001-aaaa", Some("a"), "A", Utc::now(), 9, None);
    let loader = SavedLoader::default();
    loader.facts(p.clone(), true).await.unwrap();
    for _ in 0..10 {
        loader.facts(p.clone(), false).await.unwrap();
    }
    assert_eq!(loader.loads(), 1, "ten continuation pages, one disk scan");
    // A session saved after the load: only a first page sees it.
    seed(&p.sessions_dir, "20260101-000002-bbbb", Some("b"), "B", Utc::now(), 9, None);
    assert_eq!(loader.facts(p.clone(), false).await.unwrap().len(), 1);
    assert_eq!(loader.facts(p.clone(), true).await.unwrap().len(), 2);
    assert_eq!(loader.loads(), 2);

    let short = SavedLoader::with_reuse(Duration::from_millis(50));
    short.facts(p.clone(), true).await.unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    short.facts(p.clone(), false).await.unwrap();
    assert_eq!(short.loads(), 2, "a load past the window is not reused");
}
