use crossterm::event::KeyEvent;
use super::PluginsModalState;

pub(crate) enum InputOutcome {
    None,
    // Task 15 will construct this when Esc is pressed; matched exhaustively in input.rs dispatcher.
    #[allow(dead_code)]
    Close,
}

pub(crate) fn handle_event(_state: &mut PluginsModalState, _key: KeyEvent) -> InputOutcome {
    InputOutcome::None
}
