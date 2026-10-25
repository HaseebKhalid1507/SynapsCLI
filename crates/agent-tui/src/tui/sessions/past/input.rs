//! Past-tab keys.
//!
//! - list: ↑↓/jk move · pgup/pgdn · home/end · enter continue · r rename ·
//!   y copy id · s sort · h show/hide hidden · / search · ctrl+f deep search
//! - ← sidebar / → list. Sidebar: ↑↓ move · enter/space toggle · c clear
//! - search line: type to filter live; ↑↓ still move the list; enter done.
//!
//! Esc unwinds one layer at a time: rename popup → a running deep scan →
//! search editing → deep mode → the query text → (not consumed) the modal.
//! Tab is never consumed here: it stays the Live/Past switch.

use super::{Focus, PastCmd, RenameEdit};
use crate::tui::sessions::{InputOutcome, Notice, SessionsOp, SessionsState, SwitchTarget};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// `None` = not consumed (the shell handles it: esc closes, tab switches).
pub(crate) fn handle_key(st: &mut SessionsState, key: KeyEvent) -> Option<InputOutcome> {
    if st.past.rename.is_some() {
        return Some(rename_key(st, key));
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if key.code == KeyCode::Tab || key.code == KeyCode::BackTab {
        return None;
    }
    if key.code == KeyCode::Esc {
        return esc(st).then_some(InputOutcome::None);
    }
    if ctrl && key.code == KeyCode::Char('f') {
        st.past.enter_deep();
        return Some(InputOutcome::None);
    }
    if st.past.editing {
        return Some(search_key(st, key));
    }
    let p = &mut st.past;
    match (key.code, p.focus) {
        (KeyCode::Char('/'), _) => {
            p.editing = true;
            p.focus = Focus::List;
        }
        (KeyCode::Char('s'), _) => {
            p.sort = p.sort.next();
            p.recompute();
        }
        (KeyCode::Char('h'), _) => {
            p.show_hidden = !p.show_hidden;
            p.recompute();
        }
        (KeyCode::Left, _) => p.focus = Focus::Sidebar,
        (KeyCode::Right, _) => p.focus = Focus::List,
        (KeyCode::Up | KeyCode::Char('k'), Focus::Sidebar) => p.move_facet(-1),
        (KeyCode::Down | KeyCode::Char('j'), Focus::Sidebar) => p.move_facet(1),
        (KeyCode::Home, Focus::Sidebar) => p.move_facet(isize::MIN / 2),
        (KeyCode::End, Focus::Sidebar) => p.move_facet(isize::MAX / 2),
        (KeyCode::Enter | KeyCode::Char(' '), Focus::Sidebar) => p.toggle_facet(),
        (KeyCode::Char('c'), Focus::Sidebar) => p.clear_facets(),
        (KeyCode::Enter, Focus::List) => return Some(continue_selected(st)),
        (KeyCode::Char('r'), Focus::List) => open_rename(st),
        (KeyCode::Char('y'), Focus::List) => return Some(copy_id(st)),
        (KeyCode::Char('p'), Focus::List) => return Some(pin_selected(st)),
        _ => {
            list_nav(st, key);
        }
    }
    Some(InputOutcome::None)
}

/// Navigation keys shared by the list and the search line. `true` = moved.
fn list_nav(st: &mut SessionsState, key: KeyEvent) -> bool {
    let p = &mut st.past;
    let page = p.page() as isize;
    match key.code {
        KeyCode::Up => p.move_cursor(-1),
        KeyCode::Down => p.move_cursor(1),
        KeyCode::Char('k') if !p.editing => p.move_cursor(-1),
        KeyCode::Char('j') if !p.editing => p.move_cursor(1),
        KeyCode::PageUp => p.move_cursor(-page),
        KeyCode::PageDown => p.move_cursor(page),
        KeyCode::Home if !p.editing => p.move_cursor(isize::MIN / 2),
        KeyCode::End if !p.editing => p.move_cursor(isize::MAX / 2),
        _ => return false,
    }
    true
}

fn esc(st: &mut SessionsState) -> bool {
    let p = &mut st.past;
    if let Some(d) = p.deep.as_mut().filter(|d| d.running) {
        d.cancel_running();
        return true;
    }
    if p.editing {
        p.editing = false;
        return true;
    }
    if p.deep.is_some() {
        p.leave_deep();
        return true;
    }
    if !p.query_text.is_empty() {
        p.set_query(String::new());
        return true;
    }
    false
}

fn search_key(st: &mut SessionsState, key: KeyEvent) -> InputOutcome {
    if list_nav(st, key) {
        return InputOutcome::None;
    }
    let p = &mut st.past;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let mut text = p.search_text().to_string();
    match key.code {
        KeyCode::Enter => {
            p.editing = false;
            if p.deep.is_some() {
                return match p.start_deep() {
                    Some(cmd) => InputOutcome::Past(cmd),
                    None => {
                        p.editing = true;
                        InputOutcome::None
                    }
                };
            }
            return InputOutcome::None;
        }
        KeyCode::Backspace => {
            text.pop();
        }
        KeyCode::Char('u') if ctrl => text.clear(),
        KeyCode::Char('w') if ctrl => {
            let trimmed = text.trim_end().len();
            text.truncate(trimmed);
            let cut = text.rfind(' ').map_or(0, |i| i + 1);
            text.truncate(cut);
        }
        KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => text.push(c),
        _ => return InputOutcome::None,
    }
    p.set_query(text);
    InputOutcome::None
}

/// Enter on a Past row (or a deep hit): continue that journal. The dispatch
/// loop drains `pending_switch` and does the swap (PR 4).
fn continue_selected(st: &mut SessionsState) -> InputOutcome {
    if let Some(e) = st.past.selected_entry() {
        st.pending_switch = Some(SwitchTarget::Continue { journal_id: e.id.clone(), cwd: e.cwd.clone() });
    }
    InputOutcome::None
}

/// `p` on a Past row: a pinned one is unpinned (it won't come back); any
/// other is continued into the daemon and pinned, without switching to it.
fn pin_selected(st: &mut SessionsState) -> InputOutcome {
    if crate::tui::sessions::input::no_daemon(st) || crate::tui::sessions::input::busy_notice(st) {
        return InputOutcome::None;
    }
    let Some(e) = st.past.selected_entry() else { return InputOutcome::None };
    let op = if st.past.is_pinned(&e.id) {
        SessionsOp::ClearPin { id: e.id.clone() }
    } else {
        SessionsOp::PinPast { journal_id: e.id.clone(), cwd: e.cwd.clone() }
    };
    InputOutcome::Op(op)
}

fn copy_id(st: &mut SessionsState) -> InputOutcome {
    let Some(id) = st.past.selected_entry().map(|e| e.id.clone()) else {
        return InputOutcome::None;
    };
    st.notice = Some(Notice { text: format!("copied {id}"), error: false });
    InputOutcome::Copy(id)
}

fn open_rename(st: &mut SessionsState) {
    let Some(e) = st.past.selected_entry() else { return };
    st.notice = None;
    st.past.rename = Some(RenameEdit {
        id: e.id.clone(),
        buffer: e.name.clone().unwrap_or_default(),
        error: None,
        pending: false,
    });
}

fn rename_key(st: &mut SessionsState, key: KeyEvent) -> InputOutcome {
    let Some(r) = st.past.rename.as_mut() else { return InputOutcome::None };
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => {
            st.past.rename = None;
            return InputOutcome::None;
        }
        _ if r.pending => return InputOutcome::None,
        KeyCode::Enter => {
            let name = r.buffer.trim().to_string();
            let name = (!name.is_empty()).then_some(name);
            if let Some(n) = &name {
                if let Err(e) = agent_core::session::validate_name(n) {
                    r.error = Some(e);
                    return InputOutcome::None;
                }
            }
            r.pending = true;
            r.error = None;
            return InputOutcome::Past(PastCmd::Rename { id: r.id.clone(), name });
        }
        KeyCode::Backspace => {
            r.buffer.pop();
        }
        KeyCode::Char('u') if ctrl => r.buffer.clear(),
        KeyCode::Char(c) if !ctrl && r.buffer.chars().count() < 40 => r.buffer.push(c),
        _ => {}
    }
    r.error = None;
    InputOutcome::None
}

/// Footer hint for the Past tab.
pub(crate) fn hint(st: &SessionsState) -> String {
    let p = &st.past;
    if p.rename.is_some() {
        return "enter save (empty clears)  esc cancel".into();
    }
    if let Some(d) = &p.deep {
        if d.running {
            return "\u{2191}\u{2193} navigate  enter continue  esc stop".into();
        }
        if p.editing {
            return "enter search bodies  \u{2191}\u{2193} navigate  esc done".into();
        }
        return "\u{2191}\u{2193} navigate  enter continue  / edit  y copy id  esc back".into();
    }
    if p.editing {
        return "enter done  \u{2191}\u{2193} navigate  ctrl+f deep search  esc done".into();
    }
    if p.focus == Focus::Sidebar {
        return "\u{2191}\u{2193} move  enter toggle  c clear  \u{2192} list  / search  tab live  esc close".into();
    }
    let pin = match p.selected_entry() {
        Some(e) if p.is_pinned(&e.id) => "  p unpin",
        Some(_) => "  p pin",
        None => "",
    };
    format!(
        "enter continue{pin}  / search  ctrl+f deep  s sort  h {}  r rename  y copy id  \u{2190} filters  esc {}",
        if p.show_hidden { "hide noise" } else { "show hidden" },
        if p.query_text.is_empty() { "close" } else { "clear" },
    )
}
