//! Past-tab unit tests: catalog-backed filtering, facets, sort + day groups,
//! hidden toggle, Live exclusion, rename popup errors, copy / continue, the
//! deep-search state machine, esc layering, virtual scrolling, and render
//! sweeps (80/120/160 × every built-in theme).

use super::*;
use crate::tui::sessions::{draw, handle_event, InputOutcome, SessionsMsg, SessionsState, SwitchTarget, Tab};
use agent_core::session::Session;
use agent_core::session_catalog::{load_catalog, CatalogPaths, PreviewCache};
use agent_core::session_journal::{save_session_in_dir, SessionPersistence};
use agent_engine::session::{SessionId, SessionLifecycle, SessionMeta};
use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use serde_json::json;
use std::path::Path;

/// 2026-10-25 12:00Z, read in UTC.
fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 25, 12, 0, 0).unwrap()
}

fn clock() -> Clock {
    Clock::fixed(now(), FixedOffset::east_opt(0).unwrap())
}

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn type_str(st: &mut SessionsState, s: &str) {
    for c in s.chars() {
        handle_event(st, key(KeyCode::Char(c)));
    }
}

fn own_meta() -> SessionMeta {
    SessionMeta {
        id: SessionId("20261025-115900-0wn0".into()),
        name: Some("ember".into()),
        model: "anthropic/claude-opus-5-5".into(),
        cwd: None,
        created_at: now(),
        continued: false,
        continue_info: None,
        host_pid: 1,
        lifecycle: SessionLifecycle::Live,
        clients: 1,
        input_owner: None,
        awaiting_input: 0,
        roster: Vec::new(),
        client_input: None,
        journal_id: "20261025-115900-0wn0".into(),
        locked_by: None,
        pinned: false,
        busy: false,
        state_since: None,
        park_due: None,
        evict_due: None,
        title: None,
        cost_usd: 0.0,
        messages: 0,
    }
}

struct Fx {
    _tmp: tempfile::TempDir,
    dir: std::path::PathBuf,
    index: Vec<String>,
}

fn msgs(n: usize, first: &str, last: &str) -> Vec<serde_json::Value> {
    (0..n)
        .map(|i| {
            if i % 2 == 1 {
                json!({"role":"assistant","content":[{"type":"text","text":format!("answer {i} about tokio runtimes")}]})
            } else if i == 0 {
                json!({"role":"user","content":first})
            } else if i + 2 >= n {
                json!({"role":"user","content":last})
            } else {
                json!({"role":"user","content":format!("q{i}")})
            }
        })
        .collect()
}

impl Fx {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap().join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        Self { _tmp: tmp, dir, index: Vec::new() }
    }

    #[allow(clippy::too_many_arguments)]
    fn add(&mut self, id: &str, title: &str, model: &str, hours_ago: i64, n: usize, cost: f64, cwd: Option<&str>, name: Option<&str>) {
        let mut s = Session::new(model, "low", None);
        s.id = id.into();
        s.title = title.into();
        s.name = name.map(str::to_string);
        s.updated_at = now() - chrono::Duration::hours(hours_ago);
        s.created_at = s.updated_at - chrono::Duration::minutes(134);
        s.session_cost = cost;
        s.api_messages = msgs(n, &format!("first prompt of {title}"), &format!("last words of {title}"))
            .into_iter()
            .map(std::sync::Arc::new)
            .collect();
        save_session_in_dir(&self.dir, &s, SessionPersistence::Json).unwrap();
        if let Some(cwd) = cwd {
            self.index.push(
                json!({"schema_version":1,"session_id":id,"event":"start","timestamp":"2026-10-01T00:00:00Z","cwd":cwd})
                    .to_string(),
            );
        }
    }

    fn catalog(&self) -> Catalog {
        let index = self.dir.join("index.jsonl");
        std::fs::write(&index, self.index.join("\n") + "\n").unwrap();
        let paths = CatalogPaths {
            sessions_dir: self.dir.clone(),
            index_path: index,
            cache_path: self.dir.with_file_name("previews.json"),
        };
        let mut cat = load_catalog(&paths).unwrap();
        let mut cache = PreviewCache::default();
        cache.refresh(&self.dir, &cat.entries, &std::sync::atomic::AtomicBool::new(false), &mut |_, _| {});
        cat.apply_previews(&cache);
        cat
    }
}

const KITE: &str = "20261025-090000-08e3";
const AB12: &str = "20261025-020000-ab12";
const PETRI: &str = "20261024-080000-9f01";
const BOOT: &str = "20261021-070000-0849";
const OLD: &str = "20260901-070000-0001";
const SANDBOX: &str = "20261025-100000-5a5a";
const TINY: &str = "20261025-100500-7777";

fn fixture() -> (Fx, Catalog) {
    let mut fx = Fx::new();
    fx.add(KITE, "any ai events in nyc today?", "anthropic/claude-opus-5-5", 3, 58, 1.03, Some("/home/u"), Some("kite"));
    fx.add(AB12, "fix the header gradient", "claude-opus-5-5", 10, 210, 8.4, Some("/home/u/Projects/SynapsCLI"), None);
    fx.add(PETRI, "midterm petri nets", "claude-opus-4-8", 28, 96, 3.1, Some("/home/u"), None);
    fx.add(BOOT, "boot", "anthropic/claude-opus-4-8", 4 * 24 + 5, 644, 41.2, Some("/home/u/Projects/agent-runtime"), None);
    fx.add(OLD, "an old one", "openai-codex/gpt-5.6-sol", 50 * 24, 12, 0.4, None, None);
    fx.add(SANDBOX, "web sandbox run", "claude-opus-5-5", 2, 20, 0.2, Some("/tmp/synaps-web-sandbox/x"), None);
    fx.add(TINY, "hi", "claude-opus-5-5", 1, 2, 0.01, Some("/home/u"), None);
    let cat = fx.catalog();
    (fx, cat)
}

fn state_with(cat: Catalog) -> SessionsState {
    let mut st = SessionsState::new(1, own_meta(), true);
    st.now = now();
    st.home = Some("/home/u".into());
    st.tab = Tab::Past;
    st.past.clock = Some(clock());
    st.apply(SessionsMsg::Past { gen: 1, msg: PastMsg::Catalog(Ok(Arc::new(cat))) }, own_meta);
    st
}

fn rows(st: &SessionsState) -> Vec<String> {
    let p = &st.past;
    match &p.deep {
        Some(d) => d.hits.iter().map(|h| h.id.clone()).collect(),
        None => p.derived.view.rows.iter().map(|&i| p.entries[i].id.clone()).collect(),
    }
}

fn render_text(st: &SessionsState, theme: &crate::tui::theme::Theme, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| draw::render_with(f, f.area(), st, theme)).unwrap();
    let buf = term.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..h {
        for x in 0..w {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

fn default_theme() -> crate::tui::theme::Theme {
    crate::tui::theme::Theme::builtin_for_test("default")
}

// ── data / filtering ────────────────────────────────────────────────────────

#[test]
fn loads_hides_noise_and_sorts_recent() {
    let (_fx, cat) = fixture();
    let st = state_with(cat);
    assert_eq!(st.past.count(), Some(7));
    assert_eq!(rows(&st), vec![KITE, AB12, PETRI, BOOT, OLD], "sandbox + tiny hidden");
    assert_eq!(st.past.hiding_text(), "hiding 2 (sandbox, <3 msgs)");
}

#[test]
fn hidden_toggle_shows_noise() {
    let (_fx, cat) = fixture();
    let mut st = state_with(cat);
    handle_event(&mut st, key(KeyCode::Char('h')));
    assert_eq!(rows(&st).len(), 7);
    assert_eq!(st.past.hiding_text(), "showing 2 hidden");
    assert!(input::hint(&st).contains("h hide noise"));
    handle_event(&mut st, key(KeyCode::Char('h')));
    assert_eq!(rows(&st).len(), 5);
}

#[test]
fn search_filters_live_with_tokens() {
    let (_fx, cat) = fixture();
    let mut st = state_with(cat);
    handle_event(&mut st, key(KeyCode::Char('/')));
    assert!(st.past.editing);
    type_str(&mut st, "petri");
    assert_eq!(rows(&st), vec![PETRI], "title");
    for _ in 0..5 {
        handle_event(&mut st, key(KeyCode::Backspace));
    }
    type_str(&mut st, "last words of boot");
    assert_eq!(rows(&st), vec![BOOT], "last user message is searchable");
    handle_event(&mut st, ctrl('u'));
    type_str(&mut st, "msgs>100 $>5");
    assert_eq!(rows(&st), vec![AB12, BOOT]);
    handle_event(&mut st, ctrl('w'));
    assert_eq!(st.past.query_text, "msgs>100 ");
    handle_event(&mut st, ctrl('u'));
    type_str(&mut st, "cwd:synaps");
    assert_eq!(rows(&st), vec![AB12]);
    // j/k/s/h are text while editing.
    handle_event(&mut st, ctrl('u'));
    type_str(&mut st, "sh");
    assert_eq!(st.past.query_text, "sh");
    assert_eq!(st.past.sort, Sort::Recent);
    assert!(!st.past.show_hidden);
}

#[test]
fn live_ids_are_excluded_from_past() {
    let (_fx, cat) = fixture();
    let mut st = state_with(cat);
    let mut live = own_meta();
    live.id = SessionId("daemon-side-id".into());
    live.journal_id = KITE.into();
    st.apply(SessionsMsg::Live { gen: 1, result: Ok(vec![own_meta(), live]) }, own_meta);
    assert_eq!(st.past.count(), Some(6));
    assert!(!rows(&st).contains(&KITE.to_string()));
    // Gone from the daemon → back in Past.
    st.apply(SessionsMsg::Live { gen: 1, result: Ok(vec![own_meta()]) }, own_meta);
    assert!(rows(&st).contains(&KITE.to_string()));
}

// ── facets ──────────────────────────────────────────────────────────────────

#[track_caller]
fn facet_index(st: &SessionsState, pred: impl Fn(&FacetItem) -> bool) -> usize {
    let items = &st.past.derived.sidebar;
    items.iter().position(pred).unwrap_or_else(|| panic!("facet not listed in {items:#?}"))
}

#[track_caller]
fn select_facet(st: &mut SessionsState, pred: impl Fn(&FacetItem) -> bool) {
    handle_event(st, key(KeyCode::Left));
    assert_eq!(st.past.focus, Focus::Sidebar);
    let target = facet_index(st, pred);
    handle_event(st, key(KeyCode::Home));
    while st.past.facet_cursor < target {
        handle_event(st, key(KeyCode::Down));
    }
    assert_eq!(st.past.facet_cursor, target);
    handle_event(st, key(KeyCode::Enter));
    handle_event(st, key(KeyCode::Right));
}

#[test]
fn facets_combine_and_counts_follow_the_query() {
    let (_fx, cat) = fixture();
    let mut st = state_with(cat);
    let when: Vec<(When, usize)> = st
        .past
        .derived
        .sidebar
        .iter()
        .filter_map(|f| match f {
            FacetItem::When(w, n) => Some((*w, *n)),
            _ => None,
        })
        .collect();
    assert_eq!(when, vec![(When::Today, 2), (When::Last7Days, 2), (When::Last30Days, 0), (When::Older, 1)]);

    select_facet(&mut st, |f| matches!(f, FacetItem::Model(m) if m.label == "opus-4-8"));
    assert_eq!(rows(&st), vec![PETRI, BOOT], "anthropic/ and bare ids are one model");
    select_facet(&mut st, |f| matches!(f, FacetItem::When(When::Last7Days, _)));
    assert_eq!(rows(&st), vec![PETRI, BOOT]);
    select_facet(&mut st, |f| matches!(f, FacetItem::Project(p) if p.key == "/home/u"));
    assert_eq!(rows(&st), vec![PETRI], "three groups combine");

    // Counts follow the query, and a selected value stays listed at 0.
    handle_event(&mut st, key(KeyCode::Char('/')));
    type_str(&mut st, "zzz-nothing");
    assert!(rows(&st).is_empty());
    let i = facet_index(&st, |f| matches!(f, FacetItem::Model(m) if m.label == "opus-4-8"));
    assert!(matches!(&st.past.derived.sidebar[i], FacetItem::Model(m) if m.count == 0));
    assert!(st.past.facet_selected(&st.past.derived.sidebar[i].clone()));
    handle_event(&mut st, key(KeyCode::Esc)); // stop editing
    handle_event(&mut st, key(KeyCode::Esc)); // clear query

    // Toggle off, then clear all.
    select_facet(&mut st, |f| matches!(f, FacetItem::Project(p) if p.key == "/home/u"));
    assert_eq!(rows(&st), vec![PETRI, BOOT]);
    handle_event(&mut st, key(KeyCode::Left));
    handle_event(&mut st, key(KeyCode::Char('c')));
    assert_eq!(rows(&st).len(), 5);

    // Named only.
    select_facet(&mut st, |f| matches!(f, FacetItem::Named(_)));
    assert_eq!(rows(&st), vec![KITE]);
}

#[test]
fn sort_cycles_and_day_groups_follow() {
    let (_fx, cat) = fixture();
    let mut st = state_with(cat);
    let labels: Vec<&str> = st.past.derived.groups.iter().map(|g| g.label.as_str()).collect();
    assert_eq!(labels, vec!["Today", "Yesterday", "Oct 21", "Sep 5"]);
    handle_event(&mut st, key(KeyCode::Char('s')));
    assert_eq!(st.past.sort, Sort::Cost);
    assert_eq!(rows(&st)[0], BOOT);
    assert!(st.past.derived.groups.is_empty(), "no day headers for cost");
    handle_event(&mut st, key(KeyCode::Char('s')));
    assert_eq!(rows(&st), vec![BOOT, AB12, PETRI, KITE, OLD], "length");
    handle_event(&mut st, key(KeyCode::Char('s')));
    assert_eq!(st.past.sort, Sort::Created);
    assert!(!st.past.derived.groups.is_empty());
    handle_event(&mut st, key(KeyCode::Char('s')));
    assert_eq!(st.past.sort, Sort::Recent);
    let text = render_text(&st, &default_theme(), 120, 34);
    assert!(text.contains("sort recent"), "{text}");
}

// ── actions ─────────────────────────────────────────────────────────────────

#[test]
fn enter_sets_continue_and_y_copies() {
    let (_fx, cat) = fixture();
    let mut st = state_with(cat);
    handle_event(&mut st, key(KeyCode::Down));
    assert_eq!(handle_event(&mut st, key(KeyCode::Enter)), InputOutcome::None);
    assert_eq!(
        st.pending_switch,
        Some(SwitchTarget::Continue {
            journal_id: AB12.into(),
            cwd: Some("/home/u/Projects/SynapsCLI".into())
        })
    );
    assert_eq!(handle_event(&mut st, key(KeyCode::Char('y'))), InputOutcome::Copy(AB12.into()));
    // Unknown cwd (no index record) → None.
    handle_event(&mut st, key(KeyCode::End));
    handle_event(&mut st, key(KeyCode::Enter));
    assert_eq!(st.pending_switch, Some(SwitchTarget::Continue { journal_id: OLD.into(), cwd: None }));
    assert_eq!(st.notice.as_ref().unwrap().text, format!("copied {AB12}"));
}

#[test]
fn rename_popup_validates_and_shows_errors_inline() {
    let (_fx, cat) = fixture();
    let mut st = state_with(cat);
    handle_event(&mut st, key(KeyCode::Down));
    handle_event(&mut st, key(KeyCode::Char('r')));
    assert_eq!(st.past.rename.as_ref().unwrap().id, AB12);
    type_str(&mut st, "Bad Name");
    assert_eq!(handle_event(&mut st, key(KeyCode::Enter)), InputOutcome::None, "local validation");
    assert!(st.past.rename.as_ref().unwrap().error.as_ref().unwrap().contains("allowed characters"));
    handle_event(&mut st, ctrl('u'));
    type_str(&mut st, "kite");
    assert_eq!(
        handle_event(&mut st, key(KeyCode::Enter)),
        InputOutcome::Past(PastCmd::Rename { id: AB12.into(), name: Some("kite".into()) })
    );
    assert!(st.past.rename.as_ref().unwrap().pending);
    // The engine refuses: shown inline in the popup.
    st.apply(
        SessionsMsg::Past {
            gen: 1,
            msg: PastMsg::Renamed {
                id: AB12.into(),
                name: Some("kite".into()),
                result: Err(format!("name already used by session {KITE}")),
            },
        },
        own_meta,
    );
    let text = render_text(&st, &default_theme(), 120, 34);
    assert!(text.contains("name already used by session"), "{text}");
    assert!(text.contains("rename …ab12"), "{text}");
    // Esc closes the popup only.
    assert_eq!(handle_event(&mut st, key(KeyCode::Esc)), InputOutcome::None);
    assert!(st.past.rename.is_none());
    // Success: popup closes, row label + search follow.
    handle_event(&mut st, key(KeyCode::Char('r')));
    type_str(&mut st, "gradient");
    handle_event(&mut st, key(KeyCode::Enter));
    st.apply(
        SessionsMsg::Past { gen: 1, msg: PastMsg::Renamed { id: AB12.into(), name: Some("gradient".into()), result: Ok(()) } },
        own_meta,
    );
    assert!(st.past.rename.is_none());
    let e = st.past.entries.iter().find(|e| e.id == AB12).unwrap();
    assert_eq!(e.name.as_deref(), Some("gradient"));
    handle_event(&mut st, key(KeyCode::Char('/')));
    type_str(&mut st, "name:grad");
    assert_eq!(rows(&st), vec![AB12]);
}

// ── deep search ─────────────────────────────────────────────────────────────

fn hit(id: &str) -> DeepHit {
    DeepHit {
        id: id.into(),
        role: agent_core::session_catalog::DeepRole::Assistant,
        message_index: 1,
        snippet: "answer 1 about tokio runtimes".into(),
        matched: 15..20,
    }
}

fn deep_msg(st: &mut SessionsState, search: u64, ev: DeepEvent) -> bool {
    st.apply(SessionsMsg::Past { gen: 1, msg: PastMsg::Deep { search, ev } }, own_meta)
}

#[test]
fn deep_search_state_machine() {
    let (_fx, cat) = fixture();
    let mut st = state_with(cat);
    handle_event(&mut st, key(KeyCode::Char('/')));
    type_str(&mut st, "tokio cwd:x");
    handle_event(&mut st, key(KeyCode::Enter));
    handle_event(&mut st, ctrl('f'));
    let d = st.past.deep.as_ref().unwrap();
    assert_eq!(d.phrase, "tokio", "seeded from free words");
    assert!(st.past.editing && !d.running);
    assert_eq!(rows(&st).len(), 0, "no hits yet");
    assert_eq!(st.past.derived.view.rows.len(), 5, "targets ignore the text query");

    // Blank phrase: enter does nothing.
    handle_event(&mut st, ctrl('u'));
    assert_eq!(handle_event(&mut st, key(KeyCode::Enter)), InputOutcome::None);
    type_str(&mut st, "tokio");
    let InputOutcome::Past(PastCmd::Deep { search }) = handle_event(&mut st, key(KeyCode::Enter)) else {
        panic!("deep start")
    };
    assert!(st.past.deep.as_ref().unwrap().running);
    assert!(deep_msg(&mut st, search, DeepEvent::Hit(hit(BOOT))));
    assert!(deep_msg(&mut st, search, DeepEvent::Progress { done: 2, total: 5 }));
    assert!(!deep_msg(&mut st, search + 99, DeepEvent::Hit(hit(KITE))), "stale search dropped");
    assert_eq!(rows(&st), vec![BOOT]);
    let text = render_text(&st, &default_theme(), 120, 34);
    assert!(text.contains("1 hit · 2/5 files"), "{text}");
    assert!(text.contains("tokio"), "{text}");
    assert_eq!(st.past.selected_entry().unwrap().id, BOOT, "preview follows the hit");

    // Editing while running cancels.
    let cancel = st.past.deep.as_ref().unwrap().cancel.clone();
    handle_event(&mut st, key(KeyCode::Char('/')));
    handle_event(&mut st, key(KeyCode::Char('s')));
    assert!(cancel.load(std::sync::atomic::Ordering::Relaxed));
    assert!(!st.past.deep.as_ref().unwrap().running);
    assert!(!deep_msg(&mut st, search, DeepEvent::Hit(hit(KITE))), "cancelled search dropped");

    // Restart; esc while running stops (keeps hits); esc again leaves deep.
    let InputOutcome::Past(PastCmd::Deep { search: s2 }) = handle_event(&mut st, key(KeyCode::Enter)) else {
        panic!("restart")
    };
    assert!(s2 > search);
    deep_msg(&mut st, s2, DeepEvent::Hit(hit(KITE)));
    let cancel2 = st.past.deep.as_ref().unwrap().cancel.clone();
    handle_event(&mut st, key(KeyCode::Esc));
    assert!(cancel2.load(std::sync::atomic::Ordering::Relaxed));
    assert_eq!(rows(&st), vec![KITE]);
    assert!(render_text(&st, &default_theme(), 120, 34).contains("stopped at"));
    // enter on a hit continues that session.
    handle_event(&mut st, key(KeyCode::Enter));
    assert_eq!(st.pending_switch, Some(SwitchTarget::Continue { journal_id: KITE.into(), cwd: Some("/home/u".into()) }));
    // Done summary.
    st.apply(
        SessionsMsg::Past { gen: 1, msg: PastMsg::DeepDone { search: s2, summary: DeepSummary::default() } },
        own_meta,
    );
    handle_event(&mut st, key(KeyCode::Esc));
    assert!(st.past.deep.is_none());
    assert_eq!(rows(&st).len(), 0, "the text query is back");
    assert_eq!(st.past.query_text, "tokio cwd:x");
}

#[test]
fn deep_done_finishes_and_close_cancels() {
    let (_fx, cat) = fixture();
    let mut st = state_with(cat);
    handle_event(&mut st, ctrl('f'));
    type_str(&mut st, "tokio");
    let InputOutcome::Past(PastCmd::Deep { search }) = handle_event(&mut st, key(KeyCode::Enter)) else {
        panic!()
    };
    let summary = DeepSummary { files_total: 5, files_done: 5, hits: 0, ..Default::default() };
    st.apply(SessionsMsg::Past { gen: 1, msg: PastMsg::DeepDone { search, summary } }, own_meta);
    let d = st.past.deep.as_ref().unwrap();
    assert!(!d.running && !d.cancelled);
    let text = render_text(&st, &default_theme(), 120, 34);
    assert!(text.contains("no matches in conversation text"), "{text}");
    assert!(text.contains("0 hits · 5 files"), "{text}");
    // shutdown (modal close) cancels a running scan + the preview refresh.
    handle_event(&mut st, key(KeyCode::Char('/')));
    handle_event(&mut st, key(KeyCode::Enter));
    let cancel = st.past.deep.as_ref().unwrap().cancel.clone();
    st.past.shutdown();
    assert!(cancel.load(std::sync::atomic::Ordering::Relaxed));
    assert!(st.past.stop.load(std::sync::atomic::Ordering::Relaxed));
}

#[test]
fn esc_unwinds_one_layer_at_a_time() {
    let (_fx, cat) = fixture();
    let mut st = state_with(cat);
    handle_event(&mut st, key(KeyCode::Char('/')));
    type_str(&mut st, "boot");
    assert_eq!(handle_event(&mut st, key(KeyCode::Esc)), InputOutcome::None);
    assert!(!st.past.editing);
    assert_eq!(rows(&st), vec![BOOT], "esc keeps the filter");
    assert!(input::hint(&st).contains("esc clear"));
    assert_eq!(handle_event(&mut st, key(KeyCode::Esc)), InputOutcome::None);
    assert!(st.past.query_text.is_empty());
    assert_eq!(handle_event(&mut st, key(KeyCode::Esc)), InputOutcome::Close);
    // Tab still switches while editing.
    let (_fx, cat) = fixture();
    let mut st = state_with(cat);
    handle_event(&mut st, key(KeyCode::Char('/')));
    handle_event(&mut st, key(KeyCode::Tab));
    assert_eq!(st.tab, Tab::Live);
}

// ── virtual list ────────────────────────────────────────────────────────────

#[test]
fn large_catalog_scrolls_virtually() {
    let mut fx = Fx::new();
    for i in 0..300 {
        let id = format!("2026{:02}{:02}-070000-{i:04x}", 10 - (i / 28) as u32 % 10, 1 + (i % 28) as u32);
        fx.add(&id, &format!("session number {i}"), "claude-opus-5-5", i as i64 * 7, 6, 0.5, Some("/home/u"), None);
    }
    let mut st = state_with(fx.catalog());
    assert_eq!(st.past.derived.view.rows.len(), 300);
    let theme = default_theme();
    let text = render_text(&st, &theme, 120, 34);
    assert!(text.contains("session number 0"));
    handle_event(&mut st, key(KeyCode::End));
    let text = render_text(&st, &theme, 120, 34);
    let last = rows(&st).last().unwrap().clone();
    let label = &last[last.len() - 4..];
    assert!(text.contains(&format!("\u{2026}{label}")), "{text}");
    assert!(!text.contains("session number 0 "), "top scrolled away");
    // Page up moves by the drawn viewport height.
    let page = st.past.page();
    assert!(page > 5 && page < 30, "viewport {page}");
    handle_event(&mut st, key(KeyCode::PageUp));
    assert_eq!(st.past.cursor, 299 - page);
    handle_event(&mut st, key(KeyCode::Home));
    assert_eq!((st.past.cursor, st.past.scroll), (0, 0));
}

// ── rendering ───────────────────────────────────────────────────────────────

#[test]
fn past_renders_every_width_and_builtin_theme() {
    let (_fx, cat) = fixture();
    let base = state_with(cat.clone());
    let mut sidebar = base.clone();
    handle_event(&mut sidebar, key(KeyCode::Left));
    handle_event(&mut sidebar, key(KeyCode::Down));
    handle_event(&mut sidebar, key(KeyCode::Enter));
    let mut search = base.clone();
    handle_event(&mut search, key(KeyCode::Char('/')));
    type_str(&mut search, "a very long search phrase that will not fit in an eighty column terminal at all msgs>3");
    let mut deep = base.clone();
    handle_event(&mut deep, ctrl('f'));
    type_str(&mut deep, "tokio");
    if let InputOutcome::Past(PastCmd::Deep { search }) = handle_event(&mut deep, key(KeyCode::Enter)) {
        let mut h = hit(BOOT);
        h.snippet = format!("{} tokio {}", "lead ".repeat(30), "tail ".repeat(30));
        h.matched = 151..156;
        deep_msg(&mut deep, search, DeepEvent::Hit(h));
    }
    let mut rename = base.clone();
    handle_event(&mut rename, key(KeyCode::Char('r')));
    type_str(&mut rename, "Nope");
    handle_event(&mut rename, key(KeyCode::Enter));
    let mut loading = SessionsState::new(1, own_meta(), true);
    loading.tab = Tab::Past;
    let mut empty = base.clone();
    handle_event(&mut empty, key(KeyCode::Char('/')));
    type_str(&mut empty, "zzzz");
    let states = [base, sidebar, search, deep, rename, loading, empty];
    for name in crate::tui::settings::BUILTIN_THEMES {
        let theme = crate::tui::theme::Theme::builtin_for_test(name);
        for (w, h) in [(80u16, 24u16), (120, 30), (160, 40), (60, 18), (40, 10)] {
            for st in &states {
                let text = render_text(st, &theme, w, h);
                for line in text.lines() {
                    assert!(
                        crate::tui::text_metrics::width(line) <= usize::from(w),
                        "{name} {w}x{h}: overflow: {line:?}"
                    );
                }
                if w >= 80 && st.past.catalog.is_some() {
                    assert!(text.contains("Past 7"), "{name} {w}x{h}:\n{text}");
                }
            }
        }
    }
}

#[test]
fn past_layout_at_120() {
    let (_fx, cat) = fixture();
    let st = state_with(cat);
    let text = render_text(&st, &default_theme(), 120, 34);
    for want in [
        "Past 7",
        "hiding 2 (sandbox, <3 msgs)",
        "WHEN",
        "Today",
        "Last 7 days",
        "PROJECT",
        "~/ ",
        "agent-runtime",
        "MODEL",
        "opus-4-8",
        "TODAY",
        "YESTERDAY",
        "kite",
        "any ai events in nyc today?",
        "58 msgs",
        "$1.03",
        "first: “first prompt of any ai events in nyc today?”",
        "last:  “last words of any ai events in nyc today?”",
        "kite · 20261025-090000-08e3 · opus-5-5 · ~/",
        "2h14m",
        "Oct 25 06:46 → 09:00",
        "enter continue",
        "ctrl+f deep",
    ] {
        assert!(text.contains(want), "missing {want:?}:\n{text}");
    }
    // Rows sit under their day headers.
    let today = text.find("TODAY").unwrap();
    let kite = text.find("any ai events").unwrap();
    let yesterday = text.find("YESTERDAY").unwrap();
    let petri = text.find("midterm petri").unwrap();
    assert!(today < kite && kite < yesterday && yesterday < petri);
    let _ = Path::new("");
}

/// `cargo test -p synaps-tui dump_past_120 -- --nocapture`
#[test]
fn dump_past_120() {
    let (_fx, cat) = fixture();
    let mut st = state_with(cat);
    handle_event(&mut st, key(KeyCode::Char('/')));
    type_str(&mut st, "a");
    println!("{}", render_text(&st, &default_theme(), 120, 34));
}
