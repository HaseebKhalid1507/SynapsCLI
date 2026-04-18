use crossterm::event::KeyEvent;
use super::PluginsModalState;

pub(crate) enum InputOutcome {
    None,
    Close,
}

pub(crate) fn handle_event(_state: &mut PluginsModalState, _key: KeyEvent) -> InputOutcome {
    InputOutcome::None
}
