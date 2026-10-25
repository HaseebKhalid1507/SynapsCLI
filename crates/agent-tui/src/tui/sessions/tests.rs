//! Unit tests for the /sessions modal: row text per state, columns, input
//! (pin / close-confirm / own session), and render sweeps across widths and
//! built-in themes (rendered with an explicit theme — the process-global one
//! is never touched).

use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn now() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(1_800_000_000, 0).unwrap()
}

fn ago(secs: i64) -> Option<DateTime<Utc>> {
    Some(now() - chrono::Duration::seconds(secs))
}

fn ahead(secs: i64) -> Option<DateTime<Utc>> {
    Some(now() + chrono::Duration::seconds(secs))
}

pub(super) fn meta(id: &str, name: Option<&str>) -> SessionMeta {
    SessionMeta {
        id: SessionId(id.to_string()),
        name: name.map(str::to_string),
        model: "anthropic/claude-opus-5-5".to_string(),
        cwd: Some(PathBuf::from("/home/u/Projects/SynapsCLI")),
        created_at: now() - chrono::Duration::hours(3),
        continued: false,
        continue_info: None,
        host_pid: 1,
        lifecycle: SessionLifecycle::Live,
        clients: 1,
        input_owner: None,
        awaiting_input: 0,
        journal_id: id.to_string(),
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

fn streaming() -> SessionMeta {
    let mut m = meta("20261025-090000-0849", Some("ember"));
    m.busy = true;
    m.state_since = ago(125);
    m.cost_usd = 12.4;
    m
}

fn waiting() -> SessionMeta {
    let mut m = meta("20261025-090100-77aa", Some("muse"));
    m.busy = true;
    m.awaiting_input = 1;
    m
}

fn idle() -> SessionMeta {
    let mut m = meta("20261025-090200-49f6", Some("axon"));
    m.clients = 0;
    m.state_since = ago(4 * 60 + 10);
    m.park_due = ahead(42);
    m.cost_usd = 4.1;
    m
}

fn parked() -> SessionMeta {
    let mut m = meta("20261024-181829-08e3", None);
    m.lifecycle = SessionLifecycle::Parked;
    m.clients = 0;
    m.state_since = ago(12 * 60);
    m.evict_due = ahead(48 * 60);
    m.cost_usd = 1.03;
    m.title = Some("any ai events in nyc today?".into());
    m.messages = 58;
    m
}

fn pinned_idle() -> SessionMeta {
    let mut m = meta("20261025-090300-p1nd", Some("kite"));
    m.pinned = true;
    m.clients = 0;
    m.state_since = ago(30);
    m.park_due = None;
    m
}

/// A daemon-mode state: own = ember, plus the others.
fn state() -> SessionsState {
    let mut st = SessionsState::new(1, streaming(), true);
    st.now = now();
    st.home = Some(PathBuf::from("/home/u"));
    st.apply(
        SessionsMsg::Live { gen: 1, result: Ok(vec![parked(), idle(), streaming(), waiting(), pinned_idle()]) },
        streaming,
    );
    st.now = now();
    st
}

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::empty())
}

fn select(st: &mut SessionsState, id: &str) {
    st.selected = Some(id.to_string());
}

// ── row text ────────────────────────────────────────────────────────────────

#[test]
fn state_text_per_state() {
    assert_eq!(state_text(&streaming(), now()), "streaming 2m");
    assert_eq!(state_text(&waiting(), now()), "waiting on you");
    assert_eq!(state_text(&idle(), now()), "idle 4m · parks in 42s");
    assert_eq!(state_text(&parked(), now()), "parked 12m · evicts in 48m");
    // Pinned never parks: no countdown even if a stale due slipped through.
    let mut p = pinned_idle();
    p.park_due = ahead(10);
    assert_eq!(state_text(&p, now()), "idle 30s");
    // Overdue countdown.
    let mut i = idle();
    i.park_due = ago(1);
    assert_eq!(state_text(&i, now()), "idle 4m · parks now");
    // Older daemon: no state_since/due → bare state.
    let bare = meta("x", None);
    assert_eq!(state_text(&bare, now()), "idle");
}

#[test]
fn clients_model_cwd_cost() {
    let m = meta("abc", None);
    assert_eq!(clients_text(&m, true), "you");
    let mut two = m.clone();
    two.clients = 2;
    assert_eq!(clients_text(&two, true), "you +1");
    assert_eq!(clients_text(&two, false), "2 clients");
    assert_eq!(clients_text(&idle(), false), "\u{2014}");
    assert_eq!(short_model("anthropic/claude-opus-5-5"), "opus-5-5");
    assert_eq!(short_model("gpt-5"), "gpt-5");
    let home = PathBuf::from("/home/u");
    assert_eq!(short_cwd(Some(&home), Some(&home)), "~/");
    assert_eq!(short_cwd(Some(&home.join("x/y")), Some(&home)), "~/x/y");
    assert_eq!(short_cwd(Some(std::path::Path::new("/tmp/a")), Some(&home)), "/tmp/a");
    assert_eq!(cost_text(0.0), "");
    assert_eq!(cost_text(1.034), "$1.03");
    assert_eq!(cost_text(0.0001), "<$0.01");
    assert_eq!(cost_text(123.4), "$123");
    assert_eq!(label(&parked()), "\u{2026}08e3");
    assert_eq!(label(&idle()), "axon");
}

#[test]
fn rows_sorted_live_then_parked_and_counted() {
    let st = state();
    let order: Vec<RowState> = st.live.iter().map(RowState::of).collect();
    assert_eq!(
        order,
        vec![RowState::Streaming, RowState::Waiting, RowState::Idle, RowState::Idle, RowState::Parked]
    );
    assert_eq!(st.counts(), (1, 1, 2, 1));
    // Selection starts on our own session.
    assert_eq!(st.selected.as_deref(), Some("20261025-090000-0849"));
}

#[test]
fn refresh_keeps_selection_and_falls_back_when_gone() {
    let mut st = state();
    select(&mut st, "20261025-090200-49f6");
    st.apply(SessionsMsg::Live { gen: 1, result: Ok(vec![idle(), streaming()]) }, streaming);
    assert_eq!(st.selected.as_deref(), Some("20261025-090200-49f6"));
    st.apply(SessionsMsg::Live { gen: 1, result: Ok(vec![streaming()]) }, streaming);
    assert_eq!(st.selected.as_deref(), Some("20261025-090000-0849"), "falls back to own");
}

#[test]
fn stale_generation_is_dropped() {
    let mut st = state();
    let n = st.live.len();
    assert!(!st.apply(SessionsMsg::Live { gen: 99, result: Ok(vec![]) }, streaming));
    assert_eq!(st.live.len(), n);
}

#[test]
fn daemon_down_falls_back_to_own_row() {
    let mut st = state();
    st.apply(SessionsMsg::Live { gen: 1, result: Err("daemon unreachable: refused".into()) }, streaming);
    assert!(matches!(st.source, LiveSource::DaemonDown(_)));
    assert_eq!(st.live.len(), 1);
    assert_eq!(st.live[0].id.0, st.own_id);
}

// ── input ───────────────────────────────────────────────────────────────────

#[test]
fn own_session_cannot_be_closed() {
    let mut st = state();
    assert_eq!(handle_event(&mut st, key(KeyCode::Char('x'))), InputOutcome::None);
    assert!(st.confirm.is_none());
    assert!(st.notice.as_ref().unwrap().text.contains("this session"));
    assert!(!input::hint(&st).contains("x close"));
}

#[test]
fn close_needs_confirm_and_cancel_sends_nothing() {
    let mut st = state();
    select(&mut st, "20261025-090200-49f6");
    assert!(input::hint(&st).contains("x close"));
    // x only opens the popup — no op.
    assert_eq!(handle_event(&mut st, key(KeyCode::Char('x'))), InputOutcome::None);
    assert_eq!(st.confirm.as_ref().map(|c| c.label.as_str()), Some("axon"));
    // Other keys while the popup is up do nothing (no pin, no close).
    assert_eq!(handle_event(&mut st, key(KeyCode::Char('p'))), InputOutcome::None);
    assert_eq!(handle_event(&mut st, key(KeyCode::Down)), InputOutcome::None);
    assert_eq!(st.selected.as_deref(), Some("20261025-090200-49f6"));
    // n cancels; esc on the popup cancels the popup, not the modal.
    assert_eq!(handle_event(&mut st, key(KeyCode::Char('n'))), InputOutcome::None);
    assert!(st.confirm.is_none());
    handle_event(&mut st, key(KeyCode::Char('x')));
    assert_eq!(handle_event(&mut st, key(KeyCode::Esc)), InputOutcome::None);
    assert!(st.confirm.is_none());
    // y confirms → the op.
    handle_event(&mut st, key(KeyCode::Char('x')));
    assert_eq!(
        handle_event(&mut st, key(KeyCode::Char('y'))),
        InputOutcome::Op(SessionsOp::Close { id: "20261025-090200-49f6".into(), label: "axon".into() })
    );
    assert!(st.confirm.is_none());
}

#[test]
fn streaming_session_close_refused_locally() {
    let mut st = state();
    let mut other = streaming();
    other.id = SessionId("other-busy".into());
    other.name = Some("busy".into());
    st.apply(SessionsMsg::Live { gen: 1, result: Ok(vec![streaming(), other]) }, streaming);
    select(&mut st, "other-busy");
    assert_eq!(handle_event(&mut st, key(KeyCode::Char('x'))), InputOutcome::None);
    assert!(st.confirm.is_none());
    assert!(st.notice.as_ref().unwrap().text.contains("streaming"));
    assert!(!input::hint(&st).contains("x close"), "no close hint on a streaming row");
}

#[test]
fn pin_toggles_and_waits_for_pending() {
    let mut st = state();
    assert_eq!(
        handle_event(&mut st, key(KeyCode::Char('p'))),
        InputOutcome::Op(SessionsOp::Pin { id: "20261025-090000-0849".into(), on: true })
    );
    select(&mut st, "20261025-090300-p1nd");
    assert!(input::hint(&st).contains("p unpin"));
    assert_eq!(
        handle_event(&mut st, key(KeyCode::Char('p'))),
        InputOutcome::Op(SessionsOp::Pin { id: "20261025-090300-p1nd".into(), on: false })
    );
    st.pending = Some(SessionsOp::Pin { id: "x".into(), on: true });
    assert_eq!(handle_event(&mut st, key(KeyCode::Char('p'))), InputOutcome::None);
    // The answer clears pending and leaves a notice.
    st.apply(
        SessionsMsg::OpDone { gen: 1, op: SessionsOp::Pin { id: "x".into(), on: true }, result: Ok("pinned".into()) },
        || unreachable!(),
    );
    assert!(st.pending.is_none());
    assert!(st.notice.as_ref().unwrap().text.starts_with("pinned"));
}

#[test]
fn close_refusal_shows_inline() {
    let mut st = state();
    st.apply(
        SessionsMsg::OpDone {
            gen: 1,
            op: SessionsOp::Close { id: "a".into(), label: "axon".into() },
            result: Err("session axon has 1 client attached".into()),
        },
        || unreachable!(),
    );
    let n = st.notice.clone().unwrap();
    assert!(n.error);
    assert_eq!(n.text, "axon not closed: session axon has 1 client attached");
}

#[test]
fn tab_esc_and_slash() {
    let mut st = state();
    assert_eq!(handle_event(&mut st, key(KeyCode::Tab)), InputOutcome::None);
    assert_eq!(st.tab, Tab::Past);
    // p/x do nothing on Past.
    assert_eq!(handle_event(&mut st, key(KeyCode::Char('p'))), InputOutcome::None);
    assert_eq!(handle_event(&mut st, key(KeyCode::Char('/'))), InputOutcome::None);
    assert_eq!(handle_event(&mut st, key(KeyCode::BackTab)), InputOutcome::None);
    assert_eq!(st.tab, Tab::Live);
    // Enter on our own row: nothing to switch to.
    assert_eq!(handle_event(&mut st, key(KeyCode::Enter)), InputOutcome::None);
    assert!(st.pending_switch.is_none());
    assert!(!input::hint(&st).contains("enter"), "no enter hint on our own row");
    assert_eq!(handle_event(&mut st, key(KeyCode::Esc)), InputOutcome::Close);
}

#[test]
fn enter_on_another_row_requests_a_switch() {
    let mut st = state();
    select(&mut st, "20261024-181829-08e3"); // parked: attaching unparks it
    assert!(input::hint(&st).contains("enter switch"));
    assert_eq!(handle_event(&mut st, key(KeyCode::Enter)), InputOutcome::None);
    assert_eq!(st.pending_switch, Some(SwitchTarget::Attach { id: "20261024-181829-08e3".into() }));
    // In-process: no daemon to switch through.
    let mut st = SessionsState::new(1, streaming(), false);
    handle_event(&mut st, key(KeyCode::Enter));
    assert!(st.pending_switch.is_none());
    assert!(!input::hint(&st).contains("enter"));
}

// ── rendering ───────────────────────────────────────────────────────────────

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

#[test]
fn renders_every_width_and_builtin_theme() {
    let mut confirm = state();
    select(&mut confirm, "20261025-090200-49f6");
    handle_event(&mut confirm, key(KeyCode::Char('x')));
    let mut past = state();
    past.tab = Tab::Past;
    past.past.set(Ok(vec![]));
    let mut down = state();
    down.apply(SessionsMsg::Live { gen: 1, result: Err("daemon unreachable".into()) }, streaming);
    let mut long = state();
    long.notice = Some(Notice {
        text: "axon not closed: a turn is running. Closing the session would stop it. Wait, or use /close --force from that session's own terminal to stop it anyway".into(),
        error: true,
    });
    let states = [state(), confirm, past, down, long];
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
            }
        }
    }
}

#[test]
fn live_rows_at_120_cols() {
    let theme = crate::tui::theme::Theme::builtin_for_test("default");
    let text = render_text(&state(), &theme, 120, 30);
    for want in [
        "Sessions",
        "Live 5",
        "LIVE",
        "PARKED",
        "streaming 2m",
        "waiting on you",
        "idle 4m · parks in 42s",
        "parked 12m · evicts in 48m",
        "\u{2691}", // pinned marker (kite)
        "you",
        "opus-5-5",
        "$12.40",
        "~/Projects/SynapsCLI",
        "20261025-090000-0849", // detail strip: full id of the selection
        "this TUI",
    ] {
        assert!(text.contains(want), "missing {want:?} in:\n{text}");
    }
    assert!(!text.contains("x close"), "own session selected: no close hint\n{text}");
    assert!(!text.contains("enter switch"), "own row selected: no switch hint\n{text}");
    // The pinned marker sits on kite's row.
    let kite = text.lines().find(|l| l.contains("kite")).unwrap();
    assert!(kite.contains('\u{2691}'), "{kite}");
}

#[test]
fn narrow_drops_columns_not_state() {
    let c = draw::Columns::for_width(65);
    assert!(c.state >= 16);
    let wide = draw::Columns::for_width(150);
    assert!(wide.clients && wide.cost && wide.model && wide.id && wide.cwd > 0);
    let theme = crate::tui::theme::Theme::builtin_for_test("default");
    let text = render_text(&state(), &theme, 80, 24);
    assert!(text.contains("parks in 42s"), "{text}");
}

#[test]
fn in_process_notice_and_daemon_down_notice() {
    let theme = crate::tui::theme::Theme::builtin_for_test("default");
    let mut st = SessionsState::new(1, streaming(), false);
    st.now = now();
    let text = render_text(&st, &theme, 120, 30);
    assert!(text.contains("no daemon (in-process)"), "{text}");
    assert!(text.contains("ember"));
    let mut down = state();
    down.apply(SessionsMsg::Live { gen: 1, result: Err("daemon unreachable: refused".into()) }, streaming);
    let text = render_text(&down, &theme, 120, 30);
    assert!(text.contains("daemon unreachable"), "{text}");
    assert!(!text.contains("axon"), "{text}");
}

#[test]
fn confirm_popup_text() {
    let theme = crate::tui::theme::Theme::builtin_for_test("default");
    let mut st = state();
    select(&mut st, "20261025-090200-49f6");
    handle_event(&mut st, key(KeyCode::Char('x')));
    let text = render_text(&st, &theme, 120, 30);
    assert!(text.contains("close axon?"), "{text}");
    assert!(text.contains("y close"), "{text}");
    // With a client attached the popup says what the daemon will do.
    let mut st = state();
    let mut att = idle();
    att.clients = 1;
    st.apply(SessionsMsg::Live { gen: 1, result: Ok(vec![streaming(), att]) }, streaming);
    select(&mut st, "20261025-090200-49f6");
    handle_event(&mut st, key(KeyCode::Char('x')));
    let text = render_text(&st, &theme, 120, 30);
    assert!(text.contains("its TUI client will exit (1 attached)"), "{text}");
    const { assert!(CLOSE_ATTACHED, "owner decision: confirm closes even with clients attached") };
}

#[test]
fn past_tab_lists_newest_first() {
    let theme = crate::tui::theme::Theme::builtin_for_test("default");
    let mut st = state();
    st.tab = Tab::Past;
    let info = |id: &str, title: &str, h: i64| synaps_cli::SessionInfo {
        id: id.into(),
        title: title.into(),
        name: None,
        model: "m".into(),
        created_at: now() - chrono::Duration::hours(h + 1),
        updated_at: now() - chrono::Duration::hours(h),
        session_cost: 1.0,
        message_count: 10,
    };
    st.past.set(Ok(vec![info("old-aaaa", "older one", 30), info("new-bbbb", "newer one", 2)]));
    let text = render_text(&st, &theme, 120, 30);
    assert!(text.contains("enter continue"), "{text}");
    // Enter on the newest row asks to continue its journal.
    handle_event(&mut st, key(KeyCode::Enter));
    assert_eq!(st.pending_switch, Some(SwitchTarget::Continue { journal_id: "new-bbbb".into(), cwd: None }));
    assert!(text.contains("Past 2"), "{text}");
    assert!(text.contains("search & filters coming"), "{text}");
    let newer = text.find("newer one").unwrap();
    let older = text.find("older one").unwrap();
    assert!(newer < older, "{text}");
    assert!(text.contains("2h ago"));
}

/// Dumps the Live tab at 120 cols (`cargo test -p synaps-tui dump_live_120 -- --nocapture`).
#[test]
fn dump_live_120() {
    let theme = crate::tui::theme::Theme::builtin_for_test("default");
    let mut st = state();
    st.past.set(Ok(vec![]));
    println!("{}", render_text(&st, &theme, 120, 30));
}

/// An older daemon omits `busy`: this TUI's own running turn still shows.
#[test]
fn own_busy_overlays_an_old_daemon_row() {
    let mut st = state();
    let mut own = streaming();
    own.busy = false;
    st.apply(SessionsMsg::Live { gen: 1, result: Ok(vec![own.clone(), idle()]) }, streaming);
    assert_eq!(RowState::of(&st.live[0]), RowState::Streaming);
    // …and never invents busy for a quiet TUI.
    let quiet = || {
        let mut m = streaming();
        m.busy = false;
        m
    };
    st.apply(SessionsMsg::Live { gen: 1, result: Ok(vec![own, idle()]) }, quiet);
    assert_eq!(RowState::of(st.selected_meta().unwrap()), RowState::Idle);
}
