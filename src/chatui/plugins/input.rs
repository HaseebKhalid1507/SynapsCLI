use crossterm::event::{KeyCode, KeyEvent};
use super::PluginsModalState;
use super::state::{Focus, RightMode, LeftRow};

pub(crate) enum InputOutcome {
    None,
    Close,
    /// Caller should: fetch marketplace metadata and, on success, insert into state.
    AddMarketplace(String),
    /// Caller should: install the given plugin (from marketplace at index, plugin at index).
    Install { marketplace: String, plugin: String },
    Uninstall(String),
    Update(String),
    RefreshMarketplace(String),
    RemoveMarketplace(String),
    TrustAndInstall { plugin_name: String, host: String, source: String },
    /// Toggle disabled state of a plugin. enabled=true means "make it enabled".
    TogglePlugin { name: String, enabled: bool },
}

pub(crate) fn handle_event(state: &mut PluginsModalState, key: KeyEvent) -> InputOutcome {
    match &state.mode {
        RightMode::AddMarketplaceEditor { .. } => return editor_key(state, key),
        RightMode::TrustPrompt { .. } => return trust_key(state, key),
        RightMode::Confirm { .. } => return confirm_key(state, key),
        RightMode::Detail { .. } => return detail_key(state, key),
        RightMode::List => {}
    }

    match key.code {
        KeyCode::Esc => InputOutcome::Close,
        KeyCode::Tab => {
            state.focus = match state.focus { Focus::Left => Focus::Right, Focus::Right => Focus::Left };
            state.row_error = None;
            InputOutcome::None
        }
        KeyCode::Up => {
            match state.focus { Focus::Left => state.move_left_up(), Focus::Right => state.move_right_up() }
            state.row_error = None;
            InputOutcome::None
        }
        KeyCode::Down => {
            match state.focus { Focus::Left => state.move_left_down(), Focus::Right => state.move_right_down() }
            state.row_error = None;
            InputOutcome::None
        }
        KeyCode::Enter => list_enter(state),
        KeyCode::Char('i') if matches!(state.focus, Focus::Right) => install_on_row(state),
        KeyCode::Char('e') if matches!(state.focus, Focus::Right) => toggle_installed(state, true),
        KeyCode::Char('d') if matches!(state.focus, Focus::Right) => toggle_installed(state, false),
        KeyCode::Char('u') if matches!(state.focus, Focus::Right) => update_on_row(state),
        KeyCode::Char('U') if matches!(state.focus, Focus::Right) => {
            ask_uninstall(state)
        }
        KeyCode::Char('r') if matches!(state.focus, Focus::Left) => refresh_selected_marketplace(state),
        KeyCode::Char('R') if matches!(state.focus, Focus::Left) => ask_remove_marketplace(state),
        _ => InputOutcome::None,
    }
}

fn list_enter(state: &mut PluginsModalState) -> InputOutcome {
    let rows = state.left_rows();
    match rows.get(state.selected_left) {
        Some(LeftRow::AddMarketplace) if matches!(state.focus, Focus::Right | Focus::Left) => {
            state.mode = RightMode::AddMarketplaceEditor { buffer: String::new(), error: None };
            state.focus = Focus::Right;
            InputOutcome::None
        }
        Some(_) if matches!(state.focus, Focus::Right) => {
            if !state.right_rows().is_empty() {
                state.mode = RightMode::Detail { row_idx: state.selected_right };
            }
            InputOutcome::None
        }
        _ => InputOutcome::None,
    }
}

fn editor_key(state: &mut PluginsModalState, key: KeyEvent) -> InputOutcome {
    let RightMode::AddMarketplaceEditor { buffer, error } = &mut state.mode else { return InputOutcome::None };
    match key.code {
        KeyCode::Esc => { state.mode = RightMode::List; InputOutcome::None }
        KeyCode::Backspace => { buffer.pop(); *error = None; InputOutcome::None }
        KeyCode::Char(c) => { buffer.push(c); *error = None; InputOutcome::None }
        KeyCode::Enter => {
            let url = buffer.trim().to_string();
            if url.is_empty() { return InputOutcome::None; }
            InputOutcome::AddMarketplace(url)
        }
        _ => InputOutcome::None,
    }
}

fn trust_key(state: &mut PluginsModalState, key: KeyEvent) -> InputOutcome {
    let RightMode::TrustPrompt { plugin_name, host, pending_source } = &state.mode else {
        return InputOutcome::None;
    };
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            let out = InputOutcome::TrustAndInstall {
                plugin_name: plugin_name.clone(),
                host: host.clone(),
                source: pending_source.clone(),
            };
            state.mode = RightMode::List;
            out
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            state.mode = RightMode::List;
            InputOutcome::None
        }
        _ => InputOutcome::None,
    }
}

fn confirm_key(state: &mut PluginsModalState, key: KeyEvent) -> InputOutcome {
    let RightMode::Confirm { on_yes, .. } = &state.mode else { return InputOutcome::None };
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            let action = match on_yes {
                crate::chatui::plugins::state::ConfirmAction::Uninstall(n) => InputOutcome::Uninstall(n.clone()),
                crate::chatui::plugins::state::ConfirmAction::RemoveMarketplace(n) => InputOutcome::RemoveMarketplace(n.clone()),
            };
            state.mode = RightMode::List;
            action
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            state.mode = RightMode::List;
            InputOutcome::None
        }
        _ => InputOutcome::None,
    }
}

fn detail_key(state: &mut PluginsModalState, key: KeyEvent) -> InputOutcome {
    match key.code {
        KeyCode::Esc => { state.mode = RightMode::List; InputOutcome::None }
        _ => InputOutcome::None,
    }
}

fn install_on_row(state: &mut PluginsModalState) -> InputOutcome {
    use super::state::{LeftRow, RightRow};
    let left = state.left_rows();
    let Some(LeftRow::Marketplace(mname)) = left.get(state.selected_left) else {
        return InputOutcome::None;
    };
    let rows = state.right_rows();
    match rows.get(state.selected_right) {
        Some(RightRow::Browseable { plugin, installed: false }) => {
            // TOFU check done by the main loop; we just emit the intent.
            InputOutcome::Install { marketplace: mname.clone(), plugin: plugin.name.clone() }
        }
        Some(RightRow::Browseable { installed: true, .. }) => {
            state.row_error = Some("already installed".into());
            InputOutcome::None
        }
        _ => InputOutcome::None,
    }
}

fn toggle_installed(state: &mut PluginsModalState, enabled: bool) -> InputOutcome {
    let rows = state.right_rows();
    if let Some(super::state::RightRow::Installed(p)) = rows.get(state.selected_right) {
        return InputOutcome::TogglePlugin { name: p.name.clone(), enabled };
    }
    InputOutcome::None
}

fn update_on_row(state: &mut PluginsModalState) -> InputOutcome {
    let rows = state.right_rows();
    if let Some(super::state::RightRow::Installed(p)) = rows.get(state.selected_right) {
        return InputOutcome::Update(p.name.clone());
    }
    InputOutcome::None
}

fn ask_uninstall(state: &mut PluginsModalState) -> InputOutcome {
    let rows = state.right_rows();
    if let Some(super::state::RightRow::Installed(p)) = rows.get(state.selected_right) {
        state.mode = RightMode::Confirm {
            prompt: format!("Uninstall '{}'? y/n", p.name),
            on_yes: crate::chatui::plugins::state::ConfirmAction::Uninstall(p.name.clone()),
        };
    }
    InputOutcome::None
}

fn refresh_selected_marketplace(state: &mut PluginsModalState) -> InputOutcome {
    if let Some(LeftRow::Marketplace(n)) = state.left_rows().get(state.selected_left) {
        return InputOutcome::RefreshMarketplace(n.clone());
    }
    InputOutcome::None
}

fn ask_remove_marketplace(state: &mut PluginsModalState) -> InputOutcome {
    let name = match state.left_rows().get(state.selected_left) {
        Some(LeftRow::Marketplace(n)) => n.clone(),
        _ => return InputOutcome::None,
    };
    let cascade = state
        .file
        .installed
        .iter()
        .filter(|p| p.marketplace.as_deref() == Some(name.as_str()))
        .count();
    let prompt = if cascade > 0 {
        format!(
            "Remove marketplace '{}' and uninstall {} plugin(s) from it? y/n",
            name, cascade
        )
    } else {
        format!("Remove marketplace '{}'? y/n", name)
    };
    state.mode = RightMode::Confirm {
        prompt,
        on_yes: crate::chatui::plugins::state::ConfirmAction::RemoveMarketplace(name),
    };
    InputOutcome::None
}

#[cfg(test)]
mod tests {
    use super::*;
    use synaps_cli::skills::state::PluginsState;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(c: KeyCode) -> KeyEvent { KeyEvent::new(c, KeyModifiers::NONE) }

    #[test]
    fn esc_in_list_closes() {
        let mut s = crate::chatui::plugins::PluginsModalState::new(PluginsState::default());
        assert!(matches!(handle_event(&mut s, key(KeyCode::Esc)), InputOutcome::Close));
    }

    #[test]
    fn tab_toggles_focus() {
        let mut s = crate::chatui::plugins::PluginsModalState::new(PluginsState::default());
        handle_event(&mut s, key(KeyCode::Tab));
        assert!(matches!(s.focus, crate::chatui::plugins::state::Focus::Right));
    }

    #[test]
    fn enter_on_add_marketplace_opens_editor() {
        let mut s = crate::chatui::plugins::PluginsModalState::new(PluginsState::default());
        s.selected_left = s.left_rows().len() - 1; // AddMarketplace
        s.focus = crate::chatui::plugins::state::Focus::Right;
        handle_event(&mut s, key(KeyCode::Enter));
        assert!(matches!(s.mode, crate::chatui::plugins::state::RightMode::AddMarketplaceEditor { .. }));
    }

    #[test]
    fn esc_in_add_marketplace_editor_returns_to_list() {
        let mut s = crate::chatui::plugins::PluginsModalState::new(PluginsState::default());
        s.mode = crate::chatui::plugins::state::RightMode::AddMarketplaceEditor {
            buffer: "x".into(), error: None,
        };
        handle_event(&mut s, key(KeyCode::Esc));
        assert!(matches!(s.mode, crate::chatui::plugins::state::RightMode::List));
    }

    #[test]
    fn y_in_confirm_uninstall_emits_uninstall_and_returns_to_list() {
        use crate::chatui::plugins::state::{RightMode, ConfirmAction};
        let mut s = crate::chatui::plugins::PluginsModalState::new(PluginsState::default());
        s.mode = RightMode::Confirm {
            prompt: "Uninstall 'x'? y/n".into(),
            on_yes: ConfirmAction::Uninstall("x".into()),
        };
        let out = handle_event(&mut s, key(KeyCode::Char('y')));
        assert!(matches!(out, InputOutcome::Uninstall(ref n) if n == "x"));
        assert!(matches!(s.mode, RightMode::List));
    }

    #[test]
    fn n_in_confirm_returns_to_list_without_action() {
        use crate::chatui::plugins::state::{RightMode, ConfirmAction};
        let mut s = crate::chatui::plugins::PluginsModalState::new(PluginsState::default());
        s.mode = RightMode::Confirm {
            prompt: "x".into(),
            on_yes: ConfirmAction::RemoveMarketplace("m".into()),
        };
        let out = handle_event(&mut s, key(KeyCode::Char('n')));
        assert!(matches!(out, InputOutcome::None));
        assert!(matches!(s.mode, RightMode::List));
    }

    #[test]
    fn y_in_trust_prompt_emits_trust_and_install() {
        use crate::chatui::plugins::state::RightMode;
        let mut s = crate::chatui::plugins::PluginsModalState::new(PluginsState::default());
        s.mode = RightMode::TrustPrompt {
            plugin_name: "p".into(),
            host: "github.com".into(),
            pending_source: "https://github.com/u/r".into(),
        };
        let out = handle_event(&mut s, key(KeyCode::Char('y')));
        assert!(matches!(
            out,
            InputOutcome::TrustAndInstall { ref plugin_name, ref host, ref source }
                if plugin_name == "p" && host == "github.com" && source == "https://github.com/u/r"
        ));
        assert!(matches!(s.mode, RightMode::List));
    }

    #[test]
    fn esc_in_detail_returns_to_list() {
        use crate::chatui::plugins::state::RightMode;
        let mut s = crate::chatui::plugins::PluginsModalState::new(PluginsState::default());
        s.mode = RightMode::Detail { row_idx: 0 };
        handle_event(&mut s, key(KeyCode::Esc));
        assert!(matches!(s.mode, RightMode::List));
    }

    #[test]
    fn capital_r_on_marketplace_warns_about_cascade_uninstall() {
        use synaps_cli::skills::state::{Marketplace, InstalledPlugin};
        let mut file = PluginsState::default();
        file.marketplaces.push(Marketplace {
            name: "mp".into(),
            url: "https://example/mp".into(),
            description: None,
            last_refreshed: None,
            cached_plugins: vec![],
            repo_url: None,
        });
        for plugin in ["a", "b"] {
            file.installed.push(InstalledPlugin {
                name: plugin.into(),
                marketplace: Some("mp".into()),
                source_url: "https://github.com/u/r".into(),
                installed_commit: "abc".into(),
                latest_commit: None,
                installed_at: "now".into(),
                source_subdir: None,
            });
        }
        let mut s = crate::chatui::plugins::PluginsModalState::new(file);
        s.selected_left = 1; // first marketplace row
        handle_event(&mut s, KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT));
        match &s.mode {
            RightMode::Confirm { prompt, .. } => {
                assert!(prompt.contains("2 plugin"));
            }
            other => panic!("expected Confirm, got {:?}", other),
        }
    }
}
