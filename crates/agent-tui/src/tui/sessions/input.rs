//! Keys for the `/sessions` modal.
//!
//! `tab` switches Live/Past, `esc` closes (or cancels the confirm popup),
//! `p` pins/unpins the selected session, `x` asks to close another one —
//! always through the confirm popup, never for this TUI's own session.
//! `enter` (switch to that session) lands in PR 4; `/` is reserved for the
//! filter.

use super::{ConfirmClose, Notice, RowState, SessionsOp, SessionsState, Tab};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InputOutcome {
    None,
    Close,
    /// Run this against the daemon (dispatch spawns it).
    Op(SessionsOp),
}

pub(crate) fn handle_event(st: &mut SessionsState, key: KeyEvent) -> InputOutcome {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return InputOutcome::Close;
    }
    if st.confirm.is_some() {
        return confirm_key(st, key);
    }
    match key.code {
        KeyCode::Esc => return InputOutcome::Close,
        KeyCode::Tab | KeyCode::BackTab => {
            st.tab = match st.tab {
                Tab::Live => Tab::Past,
                Tab::Past => Tab::Live,
            };
            return InputOutcome::None;
        }
        // Reserved: filter (PR 3). Swallowed so it never leaks anywhere.
        KeyCode::Char('/') => return InputOutcome::None,
        _ => {}
    }
    match st.tab {
        Tab::Past => {
            st.past.handle_key(key);
            InputOutcome::None
        }
        Tab::Live => live_key(st, key),
    }
}

fn live_key(st: &mut SessionsState, key: KeyEvent) -> InputOutcome {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => st.move_selection(-1),
        KeyCode::Down | KeyCode::Char('j') => st.move_selection(1),
        KeyCode::Home => st.move_selection(isize::MIN / 2),
        KeyCode::End => st.move_selection(isize::MAX / 2),
        KeyCode::Char('p') => return pin(st),
        KeyCode::Char('x') => ask_close(st),
        _ => {}
    }
    InputOutcome::None
}

fn busy_notice(st: &mut SessionsState) -> bool {
    if st.pending.is_some() {
        st.notice = Some(Notice { text: "still waiting on the daemon\u{2026}".into(), error: false });
        return true;
    }
    false
}

/// No daemon to ask (in-process TUI, or it is down): say so, emit nothing.
fn no_daemon(st: &mut SessionsState) -> bool {
    let why = match &st.source {
        super::LiveSource::InProcess => "no daemon \u{2014} pin and close need `synaps --attach`",
        super::LiveSource::DaemonDown(_) => "the daemon is unreachable \u{2014} try again when it is back",
        _ => return false,
    };
    st.notice = Some(Notice { text: why.into(), error: true });
    true
}

fn pin(st: &mut SessionsState) -> InputOutcome {
    if no_daemon(st) || busy_notice(st) {
        return InputOutcome::None;
    }
    let Some(m) = st.selected_meta() else { return InputOutcome::None };
    InputOutcome::Op(SessionsOp::Pin { id: m.id.0.clone(), on: !m.pinned })
}

/// `x`: never your own session; refused locally while it streams (the
/// daemon would refuse too); otherwise open the confirm popup. No daemon
/// call happens here.
fn ask_close(st: &mut SessionsState) {
    if no_daemon(st) || busy_notice(st) {
        return;
    }
    let Some(m) = st.selected_meta() else { return };
    let label = super::label(m);
    if st.is_own(m) {
        st.notice = Some(Notice {
            text: "that's this session — /quit or /detach from here".into(),
            error: true,
        });
        return;
    }
    if RowState::of(m) == RowState::Streaming {
        st.notice = Some(Notice {
            text: format!("{label} is streaming — close it once the turn ends"),
            error: true,
        });
        return;
    }
    let (id, clients) = (m.id.0.clone(), m.clients);
    st.notice = None;
    st.confirm = Some(ConfirmClose { id, label, clients });
}

fn confirm_key(st: &mut SessionsState, key: KeyEvent) -> InputOutcome {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
            let Some(c) = st.confirm.take() else { return InputOutcome::None };
            InputOutcome::Op(SessionsOp::Close { id: c.id, label: c.label })
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            st.confirm = None;
            InputOutcome::None
        }
        _ => InputOutcome::None,
    }
}

/// The footer hint for the current selection (no `enter` until PR 4).
pub(crate) fn hint(st: &SessionsState) -> String {
    if st.confirm.is_some() {
        return "y close  n cancel".into();
    }
    if st.tab == Tab::Past {
        return st.past.hint().into();
    }
    let mut h = String::from("\u{2191}\u{2193} navigate");
    let daemon = !matches!(st.source, super::LiveSource::InProcess | super::LiveSource::DaemonDown(_));
    if let Some(m) = st.selected_meta().filter(|_| daemon) {
        h.push_str(if m.pinned { "  p unpin" } else { "  p pin" });
        if !st.is_own(m) {
            h.push_str("  x close");
        }
    }
    h.push_str("  tab past  esc close");
    h
}
