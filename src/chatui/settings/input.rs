use crossterm::event::KeyEvent;
use super::SettingsState;

pub(crate) enum InputOutcome {
    None,
    Close,
}

pub(crate) fn handle_event(_state: &mut SettingsState, _key: KeyEvent) -> InputOutcome {
    // Implemented in Task 11.
    InputOutcome::None
}
