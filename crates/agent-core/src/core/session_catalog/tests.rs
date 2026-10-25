use super::*;
use crate::core::session::Session;
use crate::core::session_journal::{save_session_in_dir, SessionPersistence};
use chrono::{FixedOffset, TimeZone};
use serde_json::json;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

// ─── fixtures ────────────────────────────────────────────────────────────────

fn edt() -> FixedOffset {
    FixedOffset::west_opt(4 * 3600).unwrap()
}

/// Sun 2026-10-25 08:00 local (12:00Z).
fn clock() -> Clock {
    Clock::fixed(Utc.with_ymd_and_hms(2026, 10, 25, 12, 0, 0).unwrap(), edt())
}

fn at(y: i32, mo: u32, d: u32, h: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, mo, d, h, 0, 0).unwrap()
}

struct Fx {
    _tmp: tempfile::TempDir,
    paths: CatalogPaths,
    index: Vec<String>,
}

impl Fx {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let paths = CatalogPaths {
            sessions_dir: root.join("sessions"),
            index_path: root.join("sessions").join("index.jsonl"),
            cache_path: root.join("catalog").join("previews.json"),
        };
        std::fs::create_dir_all(&paths.sessions_dir).unwrap();
        Self {
            _tmp: tmp,
            paths,
            index: Vec::new(),
        }
    }

    fn dir(&self) -> &Path {
        &self.paths.sessions_dir
    }

    /// Save a real session (current format, has message_count).
    fn session(
        &self,
        id: &str,
        title: &str,
        model: &str,
        updated: DateTime<Utc>,
        msgs: Vec<serde_json::Value>,
    ) -> Session {
        let mut s = Session::new(model, "low", None);
        s.id = id.into();
        s.title = title.into();
        s.created_at = updated - chrono::Duration::hours(1);
        s.updated_at = updated;
        s.api_messages = msgs.into_iter().map(Arc::new).collect();
        save_session_in_dir(self.dir(), &s, SessionPersistence::Json).unwrap();
        s
    }

    fn index_start(&mut self, id: &str, cwd: &str) {
        self.index.push(
            json!({"schema_version":1,"session_id":id,"event":"start",
                   "timestamp":"2026-10-01T00:00:00Z","cwd":cwd,"model":"m"})
            .to_string(),
        );
    }

    fn index_end(&mut self, id: &str, turns: usize) {
        self.index.push(
            json!({"schema_version":1,"session_id":id,"event":"end",
                   "timestamp":"2026-10-01T01:00:00Z","turns":turns})
            .to_string(),
        );
    }

    fn write_index(&self) {
        std::fs::write(&self.paths.index_path, self.index.join("\n") + "\n").unwrap();
    }

    fn load(&self) -> Catalog {
        load_catalog(&self.paths).unwrap()
    }
}

/// Legacy snapshot: no `message_count`, `api_messages` LAST (as on disk;
/// `json!` would sort keys and put it first).
fn write_legacy(dir: &Path, id: &str, title: &str, msgs: Vec<serde_json::Value>) {
    let body = format!(
        r#"{{"id":"{id}","title":"{title}","model":"claude-opus-4-8","created_at":"2026-10-01T10:00:00Z","updated_at":"2026-10-01T11:00:00Z","session_cost":1.5,"api_messages":{}}}"#,
        serde_json::Value::Array(msgs)
    );
    std::fs::write(dir.join(format!("{id}.json")), body).unwrap();
}

fn user(t: &str) -> serde_json::Value {
    json!({"role":"user","content":t})
}
fn asst(t: &str) -> serde_json::Value {
    json!({"role":"assistant","content":[{"type":"thinking","thinking":"secret plan"},{"type":"text","text":t}]})
}
fn tool_result(t: &str) -> serde_json::Value {
    json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"x","content":t}]})
}
fn chat(n: usize) -> Vec<serde_json::Value> {
    (0..n)
        .map(|i| {
            if i % 2 == 0 {
                user(&format!("q{i}"))
            } else {
                asst(&format!("a{i}"))
            }
        })
        .collect()
}

const A: &str = "20261025-070000-aaaa";
const B: &str = "20261024-070000-bbbb";
const C: &str = "20261001-070000-cccc";

fn ids(cat: &Catalog, v: &View) -> Vec<String> {
    v.rows.iter().map(|&i| cat.entries[i].id.clone()).collect()
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap()
}

// ─── load / join ─────────────────────────────────────────────────────────────

#[test]
fn load_joins_headers_with_index() {
    let mut fx = Fx::new();
    fx.session(
        A,
        "alpha",
        "anthropic/claude-opus-4-6",
        at(2026, 10, 25, 11),
        chat(4),
    );
    fx.index_start(A, "/old/cwd");
    fx.index_end(A, 2);
    fx.index_start(A, "/work/proj");
    fx.index_end(A, 3);
    fx.index.push("{not json".into());
    fx.write_index();
    // Junk: directory, mismatched stem, non-session id, unreadable file.
    std::fs::create_dir(fx.dir().join("sess_abc")).unwrap();
    let s = fx.session(B, "beta", "claude-opus-4-6", at(2026, 10, 24, 11), chat(3));
    let mut wrong = std::fs::read_to_string(fx.dir().join(format!("{B}.json"))).unwrap();
    wrong = wrong.replacen(B, "20261024-070000-ffff", 1);
    std::fs::write(fx.dir().join("copy.json"), wrong).unwrap();
    let mut junk = s.clone();
    junk.id = "sess_main".into();
    save_session_in_dir(fx.dir(), &junk, SessionPersistence::Json).unwrap();
    std::fs::write(fx.dir().join("broken.json"), b"\x00garbage").unwrap();

    let cat = fx.load();
    assert_eq!(cat.unreadable, 1);
    assert_eq!(cat.entries.len(), 4);
    let a = cat.get(A).unwrap();
    assert_eq!(a.title, "alpha");
    assert_eq!(a.message_count, Some(4));
    assert_eq!(
        a.cwd.as_deref(),
        Some(Path::new("/work/proj")),
        "latest start cwd wins"
    );
    assert_eq!(a.turns, Some(5), "end turns summed");
    assert_eq!(a.model_key(), "claude-opus-4-6");
    assert_eq!(a.model_label(), "opus-4-6");
    assert_eq!(a.short_id(), "aaaa");
    assert!(!a.junk_id && a.bytes > 0 && a.mtime.is_some());
    assert!(cat.get(B).unwrap().cwd.is_none(), "no index record");
    assert!(cat.get("sess_main").unwrap().junk_id);
    assert!(
        cat.get("20261024-070000-ffff").unwrap().junk_id,
        "stem/id mismatch"
    );
    assert_eq!(cat.clone().without_ids([A]).entries.len(), 3);
}

#[test]
fn load_missing_dir_and_index_is_empty() {
    let fx = Fx::new();
    let mut paths = fx.paths.clone();
    paths.sessions_dir = paths.sessions_dir.join("nope");
    paths.index_path = paths.index_path.with_file_name("nope.jsonl");
    let cat = load_catalog(&paths).unwrap();
    assert!(cat.entries.is_empty());
}

#[test]
fn load_legacy_header_without_count_and_journal_overlay() {
    let fx = Fx::new();
    // Legacy: no message_count in header.
    write_legacy(fx.dir(), C, "old", vec![user("hello"), asst("hi")]);
    // Journal mode: second save appends a delta + meta tail.
    let mut s = fx.session(A, "j", "m", at(2026, 10, 25, 1), chat(2));
    save_session_in_dir(fx.dir(), &s, SessionPersistence::Journal).unwrap();
    s.api_messages.push(Arc::new(user("later")));
    s.updated_at = at(2026, 10, 25, 9);
    s.session_cost = 2.0;
    save_session_in_dir(fx.dir(), &s, SessionPersistence::Journal).unwrap();
    assert!(fx.dir().join(format!("{A}.journal")).exists());

    let cat = fx.load();
    let c = cat.get(C).unwrap();
    assert_eq!(c.message_count, None, "legacy count unknown, not 0");
    assert_eq!(c.cost, 1.5);
    let a = cat.get(A).unwrap();
    assert!(a.has_journal);
    assert_eq!(a.updated_at, at(2026, 10, 25, 9));
    assert_eq!(a.message_count, Some(3));
    assert_eq!(a.cost, 2.0);
}

// ─── preview cache ───────────────────────────────────────────────────────────

#[test]
fn preview_cache_is_incremental_and_uses_display_projection() {
    let fx = Fx::new();
    let msgs = vec![
        json!({"role":"user","_synaps_context":{},"content":"CONTEXT NOTE"}),
        user("  first\n\n  prompt  "),
        asst("answer"),
        tool_result("TOOL OUTPUT"),
        user("last one"),
        tool_result("TOOL OUTPUT 2"),
        user("<event id=\"e1\" type=\"subagent_completion\">done</event>"),
        user("[Request interrupted by user]"),
    ];
    fx.session(A, "a", "m", at(2026, 10, 25, 1), msgs);
    write_legacy(
        fx.dir(),
        C,
        "old",
        vec![user("x"), asst("y"), user("z"), asst("w")],
    );

    let mut cat = fx.load();
    let mut cache = PreviewCache::load(&fx.paths.cache_path);
    assert!(cache.is_empty());
    let no = AtomicBool::new(false);
    let mut calls = 0;
    let st = cache.refresh(fx.dir(), &cat.entries, &no, &mut |_, _| calls += 1);
    assert_eq!((st.scanned, st.reused, st.failed), (2, 0, 0));
    assert_eq!(calls, 2);
    cache.save(&fx.paths.cache_path).unwrap();

    cat.apply_previews(&cache);
    let a = cat.get(A).unwrap();
    assert_eq!(a.first_user.as_deref(), Some("first prompt"));
    assert_eq!(a.last_user.as_deref(), Some("last one"));
    assert_eq!(
        cat.get(C).unwrap().message_count,
        Some(4),
        "legacy count filled"
    );
    let q = parse_query("last one", &clock());
    assert!(q.matches(cat.get(A).unwrap()));
    assert!(!parse_query("tool output", &clock()).matches(cat.get(A).unwrap()));

    // Reload from disk: everything reused.
    let mut cache = PreviewCache::load(&fx.paths.cache_path);
    let st = cache.refresh(fx.dir(), &cat.entries, &no, &mut |_, _| {});
    assert_eq!((st.scanned, st.reused), (0, 2));

    // Touch one session: only it is rescanned.
    fx.session(A, "a", "m", at(2026, 10, 25, 2), chat(6));
    let cat = fx.load();
    assert_eq!(cache.stale_count(&cat.entries), 1);
    let st = cache.refresh(fx.dir(), &cat.entries, &no, &mut |_, _| {});
    assert_eq!((st.scanned, st.reused), (1, 1));
    assert_eq!(
        cache
            .get_fresh(cat.get(A).unwrap())
            .unwrap()
            .last_user
            .as_deref(),
        Some("q4")
    );

    // Prune ids no longer present.
    let only_c: Vec<_> = cat.entries.iter().filter(|e| e.id == C).cloned().collect();
    let st = cache.refresh(fx.dir(), &only_c, &no, &mut |_, _| {});
    assert_eq!(st.pruned, 1);
    assert_eq!(cache.len(), 1);
}

#[test]
fn preview_cache_corrupt_file_is_empty_and_cancel_keeps_progress() {
    let fx = Fx::new();
    std::fs::create_dir_all(fx.paths.cache_path.parent().unwrap()).unwrap();
    std::fs::write(&fx.paths.cache_path, b"{garbage").unwrap();
    assert!(PreviewCache::load(&fx.paths.cache_path).is_empty());
    for i in 0..20 {
        fx.session(
            &format!("20261025-0700{i:02}-aaaa"),
            "t",
            "m",
            at(2026, 10, 25, 1),
            chat(3),
        );
    }
    let cat = fx.load();
    let mut cache = PreviewCache::default();
    let cancel = AtomicBool::new(true);
    let st = cache.refresh(fx.dir(), &cat.entries, &cancel, &mut |_, _| {});
    assert!(st.cancelled);
    assert!(st.scanned < 20);
}

#[test]
fn clip_collapses_and_caps() {
    assert_eq!(preview::clip("  a \n\t b  "), "a b");
    let long = "x".repeat(500);
    let c = preview::clip(&long);
    assert_eq!(c.chars().count(), PREVIEW_MAX_CHARS);
    assert!(c.ends_with('…'));
    let exact = "y".repeat(PREVIEW_MAX_CHARS);
    assert_eq!(preview::clip(&exact), exact);
}

// ─── query parser ────────────────────────────────────────────────────────────

#[test]
fn parse_query_tokens() {
    let c = clock();
    let q = parse_query(
        r#"Fix "Header Gradient" cwd:Synaps model:5-5 name:kite is:named $>5 $<10.5 msgs>20 msgs<500"#,
        &c,
    );
    assert_eq!(q.terms, vec!["fix", "header gradient"]);
    assert_eq!(q.cwd, vec!["synaps"]);
    assert_eq!(q.model, vec!["5-5"]);
    assert_eq!(q.name, vec!["kite"]);
    assert!(q.named);
    assert_eq!((q.cost_gt, q.cost_lt), (Some(5.0), Some(10.5)));
    assert_eq!((q.msgs_gt, q.msgs_lt), (Some(20), Some(500)));
    assert!(parse_query("   ", &c).is_empty());
}

#[test]
fn parse_query_dates() {
    let c = clock();
    let d = |s: &str| parse_query(s, &c).after.map(|b| b.date.to_string());
    assert_eq!(d("after:2026-10-20").as_deref(), Some("2026-10-20"));
    assert_eq!(d("after:oct-20").as_deref(), Some("2026-10-20"));
    assert_eq!(d("after:Oct-20").as_deref(), Some("2026-10-20"));
    assert_eq!(
        d("after:dec-01").as_deref(),
        Some("2025-12-01"),
        "future month-day = last year"
    );
    assert_eq!(d("after:3d").as_deref(), Some("2026-10-22"));
    assert_eq!(d("after:1w").as_deref(), Some("2026-10-18"));
    assert_eq!(d("after:today").as_deref(), Some("2026-10-25"));
    assert_eq!(d("after:yesterday").as_deref(), Some("2026-10-24"));
    let b = parse_query("before:today", &c).before.unwrap();
    assert_eq!(b.start, at(2026, 10, 25, 4), "local midnight EDT = 04:00Z");
}

#[test]
fn parse_query_bad_tokens_degrade_to_text() {
    let c = clock();
    let q = parse_query(
        "$>abc msgs>x before:never after: is:weird foo:bar $ msgs> cwd: $>inf",
        &c,
    );
    assert_eq!(
        q.terms,
        vec![
            "$>abc",
            "msgs>x",
            "before:never",
            "after:",
            "is:weird",
            "foo:bar",
            "$",
            "msgs>",
            "cwd:",
            "$>inf"
        ]
    );
    assert!(q.after.is_none() && q.before.is_none() && q.cost_gt.is_none());
    // Unterminated quote runs to the end.
    assert_eq!(parse_query(r#"a "b c"#, &c).terms, vec!["a", "b c"]);
    assert_eq!(parse_query("after:feb-30", &c).terms, vec!["after:feb-30"]);
}

#[test]
fn query_matches_filters() {
    let mut fx = Fx::new();
    let mut s = fx.session(
        A,
        "Fix the header",
        "anthropic/claude-opus-5-5",
        at(2026, 10, 25, 11),
        chat(30),
    );
    s.name = Some("kite".into());
    s.session_cost = 7.0;
    save_session_in_dir(fx.dir(), &s, SessionPersistence::Json).unwrap();
    fx.session(
        B,
        "midterm",
        "claude-opus-4-6",
        at(2026, 10, 10, 11),
        chat(4),
    );
    fx.index_start(A, "/home/x/Projects/SynapsCLI");
    fx.write_index();
    let cat = fx.load();
    let c = clock();
    let hit = |s: &str| {
        let q = parse_query(s, &c);
        let mut v: Vec<&str> = cat
            .entries
            .iter()
            .filter(|e| q.matches(e))
            .map(|e| e.id.as_str())
            .collect();
        v.sort();
        v
    };
    assert_eq!(hit("HEADER fix"), vec![A]);
    assert_eq!(
        hit("header midterm"),
        Vec::<&str>::new(),
        "all words must match"
    );
    assert_eq!(hit("bbbb"), vec![B], "id is searchable");
    assert_eq!(hit("synapscli"), vec![A], "cwd is searchable");
    assert_eq!(hit("cwd:synaps"), vec![A]);
    assert_eq!(hit("model:opus-5-5"), vec![A]);
    assert_eq!(
        hit("model:anthropic/claude-opus-4-6"),
        vec![B],
        "provider prefix ignored"
    );
    assert_eq!(hit("is:named"), vec![A]);
    assert_eq!(hit("name:ki"), vec![A]);
    assert_eq!(hit("$>5"), vec![A]);
    assert_eq!(hit("$<1"), vec![B]);
    assert_eq!(hit("msgs>20"), vec![A]);
    assert_eq!(hit("msgs<5"), vec![B]);
    assert_eq!(hit("after:oct-20"), vec![A]);
    assert_eq!(hit("before:oct-20"), vec![B]);
    assert_eq!(hit("after:oct-01 before:oct-11"), vec![B]);
}

// ─── noise / facets / sort / groups ──────────────────────────────────────────

fn facet_fixture() -> (Fx, Catalog) {
    let mut fx = Fx::new();
    let h = home();
    fx.session(
        A,
        "today one",
        "anthropic/claude-opus-4-6",
        at(2026, 10, 25, 11),
        chat(10),
    );
    fx.session(
        B,
        "yesterday",
        "claude-opus-4-6",
        at(2026, 10, 24, 11),
        chat(50),
    );
    fx.session(C, "older", "openai/gpt-5", at(2026, 9, 1, 11), chat(5));
    fx.session(
        "20261020-070000-dddd",
        "this week",
        "claude-opus-5-5",
        at(2026, 10, 20, 11),
        chat(3),
    );
    fx.session(
        "20261010-070000-eeee",
        "month",
        "claude-opus-5-5",
        at(2026, 10, 10, 11),
        chat(8),
    );
    // Noise.
    fx.session(
        "20261025-080000-0001",
        "sandbox",
        "m",
        at(2026, 10, 25, 11),
        chat(10),
    );
    fx.session(
        "20261025-080000-0002",
        "tiny",
        "m",
        at(2026, 10, 25, 11),
        chat(2),
    );
    fx.session("sess_abc", "junk", "m", at(2026, 10, 25, 11), chat(10));
    fx.index_start(A, &h.to_string_lossy());
    fx.index_start(B, &h.join("Projects/agent-runtime").to_string_lossy());
    fx.index_start(C, &h.to_string_lossy());
    fx.index_start("20261025-080000-0001", "/tmp/synaps-web-sandbox/x");
    fx.write_index();
    let cat = fx.load();
    (fx, cat)
}

#[test]
fn noise_rules_hide_by_default_with_counts() {
    let (_fx, cat) = facet_fixture();
    let c = clock();
    let v = build_view(
        &cat.entries,
        &Query::default(),
        &FacetSelection::default(),
        false,
        Sort::Recent,
        &c,
    );
    assert_eq!(v.rows.len(), 5);
    assert_eq!(
        v.hidden,
        NoiseCounts {
            sandbox: 1,
            few_messages: 1,
            junk_id: 1,
            total: 3
        }
    );
    let v2 = build_view(
        &cat.entries,
        &Query::default(),
        &FacetSelection::default(),
        true,
        Sort::Recent,
        &c,
    );
    assert_eq!(v2.rows.len(), 8);
    assert_eq!(v2.hidden.total, 3, "still reported while shown");
    // Hidden counts follow the query.
    let q = parse_query("tiny", &c);
    let v3 = build_view(
        &cat.entries,
        &q,
        &FacetSelection::default(),
        false,
        Sort::Recent,
        &c,
    );
    assert!(v3.rows.is_empty());
    assert_eq!(v3.hidden.total, 1);
}

#[test]
fn unknown_message_count_is_not_noise() {
    let fx = Fx::new();
    write_legacy(fx.dir(), C, "old", vec![]);
    let cat = fx.load();
    assert!(!noise_reasons(&cat.entries[0]).any());
}

#[test]
fn facets_count_with_query_and_other_selections() {
    let (_fx, cat) = facet_fixture();
    let c = clock();
    let none = FacetSelection::default();
    let v = build_view(
        &cat.entries,
        &Query::default(),
        &none,
        false,
        Sort::Recent,
        &c,
    );
    assert_eq!(
        v.facets.when,
        vec![
            (When::Today, 1),
            (When::Last7Days, 2),
            (When::Last30Days, 1),
            (When::Older, 1)
        ]
    );
    let models: Vec<(&str, &str, usize)> = v
        .facets
        .models
        .iter()
        .map(|f| (f.key.as_str(), f.label.as_str(), f.count))
        .collect();
    assert_eq!(
        models,
        vec![
            ("claude-opus-4-6", "opus-4-6", 2),
            ("claude-opus-5-5", "opus-5-5", 2),
            ("gpt-5", "gpt-5", 1)
        ],
        "anthropic/claude-opus-4-6 and claude-opus-4-6 are one model"
    );
    let projects: Vec<(&str, usize)> = v
        .facets
        .projects
        .iter()
        .map(|f| (f.label.as_str(), f.count))
        .collect();
    assert_eq!(
        projects,
        vec![("(unknown)", 2), ("~/", 2), ("agent-runtime", 1)]
    );

    // Selecting a model filters rows and the OTHER facets, not its own.
    let sel = FacetSelection {
        model: Some("claude-opus-4-6".into()),
        ..Default::default()
    };
    let v = build_view(
        &cat.entries,
        &Query::default(),
        &sel,
        false,
        Sort::Recent,
        &c,
    );
    assert_eq!(ids(&cat, &v), vec![A, B]);
    assert_eq!(v.facets.models.len(), 3, "own group unaffected");
    assert_eq!(v.facets.when[0], (When::Today, 1));
    assert_eq!(v.facets.when[1], (When::Last7Days, 1));
    assert_eq!(v.facets.projects.len(), 2);

    // Query narrows counts.
    let q = parse_query("yesterday", &c);
    let v = build_view(&cat.entries, &q, &none, false, Sort::Recent, &c);
    assert_eq!(v.facets.when.iter().map(|w| w.1).sum::<usize>(), 1);
    assert_eq!(
        v.facets.models,
        vec![FacetCount {
            key: "claude-opus-4-6".into(),
            label: "opus-4-6".into(),
            count: 1
        }]
    );

    // When + project selection.
    let sel = FacetSelection {
        when: Some(When::Older),
        project: Some(home().to_string_lossy().into_owned()),
        ..Default::default()
    };
    let v = build_view(
        &cat.entries,
        &Query::default(),
        &sel,
        false,
        Sort::Recent,
        &c,
    );
    assert_eq!(ids(&cat, &v), vec![C]);
    let unknown = FacetSelection {
        project: Some(String::new()),
        ..Default::default()
    };
    assert_eq!(
        build_view(
            &cat.entries,
            &Query::default(),
            &unknown,
            false,
            Sort::Recent,
            &c
        )
        .rows
        .len(),
        2
    );
    let named = FacetSelection {
        named_only: true,
        ..Default::default()
    };
    let v = build_view(
        &cat.entries,
        &Query::default(),
        &named,
        false,
        Sort::Recent,
        &c,
    );
    assert!(v.rows.is_empty());
    assert_eq!(v.facets.named, 0);
}

#[test]
fn sort_orders_and_day_groups() {
    let (fx, _) = facet_fixture();
    let mut s = Session::load_from_dir(fx.dir(), C).unwrap();
    s.session_cost = 99.0;
    s.created_at = at(2026, 10, 25, 5);
    save_session_in_dir(fx.dir(), &s, SessionPersistence::Json).unwrap();
    let cat = fx.load();
    let c = clock();
    let q = Query::default();
    let none = FacetSelection::default();
    let order = |sort| ids(&cat, &build_view(&cat.entries, &q, &none, false, sort, &c));
    assert_eq!(
        order(Sort::Recent),
        vec![A, B, "20261020-070000-dddd", "20261010-070000-eeee", C]
    );
    assert_eq!(order(Sort::Cost)[0], C);
    assert_eq!(
        order(Sort::Length),
        vec![B, A, "20261010-070000-eeee", C, "20261020-070000-dddd"]
    );
    assert_eq!(order(Sort::Created)[..2], [A, C]);

    let v = build_view(&cat.entries, &q, &none, false, Sort::Recent, &c);
    let groups = day_groups(&cat.entries, &v.rows, Sort::Recent.date_key().unwrap(), &c);
    let labels: Vec<(&str, usize, usize)> = groups
        .iter()
        .map(|g| (g.label.as_str(), g.start, g.len))
        .collect();
    assert_eq!(
        labels,
        vec![
            ("Today", 0, 1),
            ("Yesterday", 1, 1),
            ("Oct 20", 2, 1),
            ("Oct 10", 3, 1),
            ("Sep 1", 4, 1)
        ]
    );
    assert!(Sort::Cost.date_key().is_none());
    assert_eq!(Sort::Recent.next(), Sort::Cost);
}

#[test]
fn clock_buckets_and_labels_use_local_time() {
    let c = clock();
    // 03:00Z on the 25th is still the 24th in EDT.
    assert_eq!(c.when(at(2026, 10, 25, 3)), When::Last7Days);
    assert_eq!(c.when(at(2026, 10, 25, 5)), When::Today);
    assert_eq!(c.when(at(2026, 10, 19, 12)), When::Last7Days);
    assert_eq!(c.when(at(2026, 10, 18, 12)), When::Last30Days);
    assert_eq!(c.when(at(2026, 9, 25, 12)), When::Older);
    assert_eq!(
        c.when(at(2027, 1, 1, 0)),
        When::Today,
        "future skew = today"
    );
    assert_eq!(
        c.day_label(chrono::NaiveDate::from_ymd_opt(2025, 10, 21).unwrap()),
        "Oct 21 2025"
    );
}

// ─── deep search ─────────────────────────────────────────────────────────────

fn deep_fixture(n: usize) -> (Fx, Catalog) {
    let fx = Fx::new();
    for i in 0..n {
        let msgs = vec![
            user(&format!("please find the Needle number {i}")),
            asst("I see a needle\nin   the haystack"),
            tool_result("needle in a tool dump"),
            user("<event id=\"x\">host event payload</event>"),
            json!({"role":"assistant","content":[{"type":"thinking","thinking":"needle thoughts"}]}),
        ];
        fx.session(
            &format!("20261025-0700{i:02}-abcd"),
            "t",
            "m",
            at(2026, 10, 25, 1),
            msgs,
        );
    }
    fx.session(
        "20261025-080000-0000",
        "t",
        "m",
        at(2026, 10, 25, 1),
        chat(4),
    );
    let cat = fx.load();
    (fx, cat)
}

fn run_deep(
    fx: &Fx,
    cat: &Catalog,
    phrase: &str,
    opts: &DeepSearchOptions,
    cancel: &AtomicBool,
) -> (Vec<DeepHit>, DeepSummary, usize) {
    let targets: Vec<&CatalogEntry> = cat.entries.iter().collect();
    let mut hits = Vec::new();
    let mut progress = 0;
    let sum = deep_search(
        fx.dir(),
        &targets,
        phrase,
        opts,
        cancel,
        &mut |ev| match ev {
            DeepEvent::Hit(h) => hits.push(h),
            DeepEvent::Progress { .. } => progress += 1,
        },
    );
    (hits, sum, progress)
}

#[test]
fn deep_search_roles_snippets_and_prefilter() {
    let (fx, cat) = deep_fixture(3);
    let no = AtomicBool::new(false);
    let (hits, sum, progress) = run_deep(&fx, &cat, "NEEDLE", &DeepSearchOptions::default(), &no);
    assert_eq!(sum.files_total, 4);
    assert_eq!(progress, 4);
    assert_eq!(
        sum.files_prefiltered, 1,
        "the chat-only file is never parsed"
    );
    assert_eq!(
        hits.len(),
        6,
        "user + assistant per session; no tool/thinking"
    );
    assert!(hits.iter().all(|h| h.role != DeepRole::ToolResult));
    let u = hits.iter().find(|h| h.role == DeepRole::User).unwrap();
    assert_eq!(&u.snippet[u.matched.clone()], "Needle");
    assert_eq!(u.message_index, 0);

    // Phrase across collapsed whitespace.
    let (hits, ..) = run_deep(
        &fx,
        &cat,
        "needle in the",
        &DeepSearchOptions::default(),
        &no,
    );
    assert_eq!(hits.len(), 3);
    let h = &hits[0];
    assert_eq!(h.role, DeepRole::Assistant);
    assert_eq!(&h.snippet[h.matched.clone()], "needle in the");

    let opts = DeepSearchOptions {
        include_tool_results: true,
        ..Default::default()
    };
    let (hits, ..) = run_deep(&fx, &cat, "tool dump", &opts, &no);
    assert_eq!(hits.len(), 3);
    assert!(hits.iter().all(|h| h.role == DeepRole::ToolResult));
    let (hits, ..) = run_deep(&fx, &cat, "host event", &DeepSearchOptions::default(), &no);
    assert!(hits.is_empty(), "host-injected text is opt-in");
    let (hits, ..) = run_deep(&fx, &cat, "host event", &opts, &no);
    assert_eq!(hits.len(), 3);
    assert!(hits.iter().all(|h| h.role == DeepRole::Host));
    let (hits, ..) = run_deep(&fx, &cat, "thoughts", &opts, &no);
    assert!(hits.is_empty(), "thinking is never searched");
    let (hits, sum, _) = run_deep(&fx, &cat, "   ", &opts, &no);
    assert!(hits.is_empty() && sum.files_done == 0);
}

#[test]
fn deep_search_stops_at_cap() {
    let (fx, cat) = deep_fixture(30);
    let no = AtomicBool::new(false);
    let opts = DeepSearchOptions {
        max_hits: 7,
        threads: 2,
        ..Default::default()
    };
    let (hits, sum, _) = run_deep(&fx, &cat, "needle", &opts, &no);
    assert_eq!(hits.len(), 7);
    assert!(sum.capped && !sum.cancelled);
    assert!(sum.files_done < sum.files_total);
    let per = DeepSearchOptions {
        max_hits_per_session: 1,
        ..Default::default()
    };
    let (hits, ..) = run_deep(&fx, &cat, "needle", &per, &no);
    assert_eq!(hits.len(), 30);
}

#[test]
fn deep_search_cancels() {
    let (fx, cat) = deep_fixture(30);
    let cancel = AtomicBool::new(false);
    let targets: Vec<&CatalogEntry> = cat.entries.iter().collect();
    let opts = DeepSearchOptions {
        threads: 1,
        ..Default::default()
    };
    let mut hits = 0;
    let sum = deep_search(fx.dir(), &targets, "needle", &opts, &cancel, &mut |ev| {
        if let DeepEvent::Hit(_) = ev {
            hits += 1;
        }
        if let DeepEvent::Progress { done: 2, .. } = ev {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    });
    assert!(sum.cancelled);
    assert!(sum.files_done < 30, "stopped early: {}", sum.files_done);
    assert!(hits >= 4);
}

#[test]
fn deep_search_non_ascii_and_journal() {
    let fx = Fx::new();
    let mut s = fx.session(
        A,
        "t",
        "m",
        at(2026, 10, 25, 1),
        vec![user("Ünïcode Straße")],
    );
    save_session_in_dir(fx.dir(), &s, SessionPersistence::Journal).unwrap();
    s.api_messages.push(Arc::new(asst("journal-only phrase")));
    save_session_in_dir(fx.dir(), &s, SessionPersistence::Journal).unwrap();
    let cat = fx.load();
    assert!(cat.entries[0].has_journal);
    let no = AtomicBool::new(false);
    let (hits, ..) = run_deep(&fx, &cat, "üNÏCODE", &DeepSearchOptions::default(), &no);
    assert_eq!(hits.len(), 1);
    assert_eq!(&hits[0].snippet[hits[0].matched.clone()], "Ünïcode");
    let (hits, ..) = run_deep(
        &fx,
        &cat,
        "journal-only",
        &DeepSearchOptions::default(),
        &no,
    );
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].message_index, 1);
}

// ─── rename ──────────────────────────────────────────────────────────────────

#[test]
fn rename_sets_and_clears_name_without_touching_recency() {
    let fx = Fx::new();
    fx.session(A, "t", "m", at(2026, 10, 25, 1), chat(3));
    let path = fx.dir().join(format!("{A}.json"));
    let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(old)
        .unwrap();

    rename_past_session(fx.dir(), A, Some("kite")).unwrap();
    let s = Session::load_from_dir(fx.dir(), A).unwrap();
    assert_eq!(s.name.as_deref(), Some("kite"));
    assert_eq!(s.updated_at, at(2026, 10, 25, 1));
    assert_eq!(s.api_messages.len(), 3);
    assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), old);
    assert!(
        !fx.dir().join(format!("{A}.lock")).exists(),
        "no lock litter"
    );
    rename_past_session(fx.dir(), A, Some("kite")).unwrap(); // idempotent
    rename_past_session(fx.dir(), A, None).unwrap();
    assert_eq!(Session::load_from_dir(fx.dir(), A).unwrap().name, None);
}

#[test]
fn rename_refuses_live_locked_session() {
    let fx = Fx::new();
    fx.session(A, "t", "m", at(2026, 10, 25, 1), chat(3));
    let lock = crate::core::session_lock::SessionLock::try_acquire(
        fx.dir(),
        A,
        crate::core::session_lock::LockHolder {
            pid: 4242,
            kind: "tui".into(),
        },
    )
    .unwrap();
    match rename_past_session(fx.dir(), A, Some("kite")) {
        Err(RenameError::Live { holder: Some(h) }) => assert_eq!(h.pid, 4242),
        other => panic!("expected Live, got {other:?}"),
    }
    assert_eq!(Session::load_from_dir(fx.dir(), A).unwrap().name, None);
    drop(lock);
    // Stale lock file (holder gone): allowed, and the file is kept.
    rename_past_session(fx.dir(), A, Some("kite")).unwrap();
    assert!(fx.dir().join(format!("{A}.lock")).exists());
}

#[test]
fn rename_validation_errors() {
    let fx = Fx::new();
    let mut b = fx.session(B, "t", "m", at(2026, 10, 24, 1), chat(3));
    b.name = Some("taken".into());
    save_session_in_dir(fx.dir(), &b, SessionPersistence::Json).unwrap();
    fx.session(A, "t", "m", at(2026, 10, 25, 1), chat(3));
    let mut c = fx.session(C, "t", "m", at(2026, 10, 1, 1), chat(3));
    c.compacted_into = Some(A.into());
    save_session_in_dir(fx.dir(), &c, SessionPersistence::Json).unwrap();

    assert!(matches!(
        rename_past_session(fx.dir(), A, Some("Bad Name")),
        Err(RenameError::InvalidName(_))
    ));
    assert!(matches!(
        rename_past_session(fx.dir(), "../x", Some("ok")),
        Err(RenameError::InvalidId)
    ));
    assert!(matches!(
        rename_past_session(fx.dir(), A, Some("taken")),
        Err(RenameError::NameTaken { other_id }) if other_id == B
    ));
    assert!(matches!(
        rename_past_session(fx.dir(), C, Some("free-name-zq9")),
        Err(RenameError::CompactedInto { successor }) if successor == A
    ));
    let missing = "20200101-000000-0000";
    assert!(matches!(
        rename_past_session(fx.dir(), missing, Some("x")),
        Err(RenameError::NotFound)
    ));
    assert!(!fx.dir().join(format!("{missing}.lock")).exists());
}
