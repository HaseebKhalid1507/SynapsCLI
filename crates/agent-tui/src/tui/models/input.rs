use crossterm::event::{KeyCode, KeyEvent};

use super::{
    build_sections, expanded_visible_models, model_id_for_runtime, normalize_favorite_id,
    remove_favorite_compat, selected_expanded_model, selected_model, selected_provider,
    visible_rows, ExpandedLoadState, ExpandedModelsState, ModelsModalState, ModelsView,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InputOutcome {
    None,
    Close,
    Apply(String),
    ExpandProvider(String),
}

pub(crate) fn handle_event(
    state: &mut ModelsModalState,
    key: KeyEvent,
    current_model: &str,
) -> InputOutcome {
    if state.expanded.is_some() {
        return handle_expanded_event(state, key);
    }

    let sections = build_sections(current_model, state);
    let row_count = visible_rows(&sections, state).len();
    match key.code {
        KeyCode::Esc => InputOutcome::Close,
        KeyCode::Up | KeyCode::Char('k') => {
            state.cursor = state.cursor.saturating_sub(1);
            InputOutcome::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if row_count > 0 {
                state.cursor = (state.cursor + 1).min(row_count - 1);
            }
            InputOutcome::None
        }
        KeyCode::Tab => {
            state.view = match state.view {
                ModelsView::All => ModelsView::Favorites,
                ModelsView::Favorites => ModelsView::All,
            };
            state.cursor = 0;
            InputOutcome::None
        }
        KeyCode::Char('c') => {
            let rows = visible_rows(&sections, state);
            if let Some(super::VisibleRow::Section { idx }) = rows.get(state.cursor) {
                if let Some(section) = sections.get(*idx) {
                    if !state.collapsed.remove(&section.provider_key) {
                        state.collapsed.insert(section.provider_key.clone());
                    }
                }
            }
            InputOutcome::None
        }
        KeyCode::Char('e') => {
            if let Some(provider) = selected_provider(&sections, state) {
                state.expanded = Some(ExpandedModelsState {
                    provider_key: provider.provider_key.clone(),
                    provider_name: provider.provider_name.clone(),
                    cursor: 0,
                    search: String::new(),
                    load_state: ExpandedLoadState::Loading,
                });
                return InputOutcome::ExpandProvider(provider.provider_key.clone());
            }
            InputOutcome::None
        }
        KeyCode::Char('f') => {
            if let Some(model) = selected_model(&sections, state) {
                if model.is_favorite {
                    remove_favorite_compat(&model.favorite_id);
                } else {
                    let _ = synaps_cli::config::add_favorite_model(&normalize_favorite_id(
                        &model.favorite_id,
                    ));
                }
                state.refresh_favorites();
                let new_len = visible_rows(&build_sections(current_model, state), state).len();
                if new_len == 0 {
                    state.cursor = 0;
                } else if state.cursor >= new_len {
                    state.cursor = new_len - 1;
                }
            }
            InputOutcome::None
        }
        KeyCode::Enter => {
            if let Some(model) = selected_model(&sections, state) {
                InputOutcome::Apply(model_id_for_runtime(&model.favorite_id))
            } else {
                InputOutcome::None
            }
        }
        KeyCode::Backspace => {
            state.search.pop();
            state.cursor = 0;
            InputOutcome::None
        }
        KeyCode::Char(ch) => {
            if !key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
            {
                state.search.push(ch);
                state.cursor = 0;
            }
            InputOutcome::None
        }
        _ => InputOutcome::None,
    }
}

fn handle_expanded_event(state: &mut ModelsModalState, key: KeyEvent) -> InputOutcome {
    match key.code {
        KeyCode::Esc => {
            state.expanded = None;
            InputOutcome::None
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(expanded) = state.expanded.as_mut() {
                expanded.cursor = expanded.cursor.saturating_sub(1);
            }
            InputOutcome::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let visible_len = expanded_visible_models(state).len();
            if let Some(expanded) = state.expanded.as_mut() {
                if visible_len > 0 {
                    expanded.cursor = (expanded.cursor + 1).min(visible_len - 1);
                }
            }
            InputOutcome::None
        }
        KeyCode::Enter => {
            if let Some(model) = selected_expanded_model(state) {
                InputOutcome::Apply(model.id)
            } else {
                InputOutcome::None
            }
        }
        KeyCode::Char('f') => {
            if let Some(model) = selected_expanded_model(state) {
                if model.is_favorite {
                    remove_favorite_compat(&model.id);
                } else {
                    let _ =
                        synaps_cli::config::add_favorite_model(&normalize_favorite_id(&model.id));
                }
                state.refresh_favorites();
            }
            InputOutcome::None
        }
        KeyCode::Backspace => {
            if let Some(expanded) = state.expanded.as_mut() {
                expanded.search.pop();
                expanded.cursor = 0;
            }
            InputOutcome::None
        }
        KeyCode::Char(ch) => {
            if !key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
            {
                if let Some(expanded) = state.expanded.as_mut() {
                    expanded.search.push(ch);
                    expanded.cursor = 0;
                }
            }
            InputOutcome::None
        }
        _ => InputOutcome::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::models::ExpandedModelEntry;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn every_copilot_curated_selection_emits_provider_qualified_routable_id() {
        for model in synaps_cli::runtime::openai::catalog::copilot_static_catalog_models() {
            let favorite_id = format!("github-copilot/{}", model.id);
            let emitted = model_id_for_runtime(&favorite_id);
            assert_eq!(emitted, favorite_id);
            let route = synaps_cli::runtime::openai::resolve_route(&emitted)
                .unwrap_or_else(|| panic!("Copilot model did not route: {emitted}"));
            assert_eq!(route.provider, "github-copilot");
        }
    }

    #[test]
    fn expanded_copilot_enter_emits_provider_qualified_claude_ids() {
        for wire_id in ["claude-fable-5", "claude-opus-4.8"] {
            let expected = format!("github-copilot/{wire_id}");
            let mut state = ModelsModalState::new();
            state.expanded = Some(ExpandedModelsState {
                provider_key: "github-copilot".to_string(),
                provider_name: "GitHub Copilot".to_string(),
                cursor: 0,
                search: String::new(),
                load_state: ExpandedLoadState::Ready(vec![ExpandedModelEntry::new(
                    expected.clone(),
                    wire_id.to_string(),
                    false,
                )]),
            });
            assert_eq!(
                handle_event(&mut state, key(KeyCode::Enter), "other"),
                InputOutcome::Apply(expected.clone())
            );
            assert_eq!(
                synaps_cli::runtime::openai::resolve_route(&expected)
                    .unwrap()
                    .provider,
                "github-copilot"
            );
        }
    }

    #[test]
    fn e_opens_expanded_provider_browser() {
        let mut state = ModelsModalState::new();
        state.view = ModelsView::All;
        let outcome = handle_event(&mut state, key(KeyCode::Char('e')), "claude-opus-4-7");
        assert_eq!(
            outcome,
            InputOutcome::ExpandProvider("anthropic".to_string())
        );
        let expanded = state.expanded.expect("expanded state");
        assert_eq!(expanded.provider_key, "anthropic");
        assert_eq!(expanded.search, "");
        assert_eq!(expanded.load_state, ExpandedLoadState::Loading);
    }

    #[test]
    fn expanded_typing_filters_and_enter_applies_selected_model() {
        let mut state = ModelsModalState::new();
        state.expanded = Some(ExpandedModelsState {
            provider_key: "openrouter".to_string(),
            provider_name: "OpenRouter".to_string(),
            cursor: 0,
            search: String::new(),
            load_state: ExpandedLoadState::Ready(vec![
                ExpandedModelEntry::new(
                    "openrouter/deepseek/deepseek-chat".to_string(),
                    "DeepSeek".to_string(),
                    false,
                ),
                ExpandedModelEntry::new(
                    "openrouter/qwen/qwen3-coder".to_string(),
                    "Qwen3 Coder".to_string(),
                    false,
                ),
            ]),
        });

        assert_eq!(
            handle_event(&mut state, key(KeyCode::Char('q')), "claude-opus-4-7"),
            InputOutcome::None
        );
        assert_eq!(
            handle_event(&mut state, key(KeyCode::Enter), "claude-opus-4-7"),
            InputOutcome::Apply("openrouter/qwen/qwen3-coder".to_string())
        );
    }

    #[test]
    fn esc_in_expanded_returns_to_curated_modal() {
        let mut state = ModelsModalState::new();
        state.expanded = Some(ExpandedModelsState {
            provider_key: "openrouter".to_string(),
            provider_name: "OpenRouter".to_string(),
            cursor: 0,
            search: String::new(),
            load_state: ExpandedLoadState::Loading,
        });

        assert_eq!(
            handle_event(&mut state, key(KeyCode::Esc), "claude-opus-4-7"),
            InputOutcome::None
        );
        assert!(state.expanded.is_none());
    }

    #[test]
    fn tab_toggles_all_and_favorites_view() {
        let mut state = ModelsModalState::new();
        state.view = ModelsView::All;
        assert_eq!(
            handle_event(&mut state, key(KeyCode::Tab), "claude-opus-4-7"),
            InputOutcome::None
        );
        assert_eq!(state.view, ModelsView::Favorites);
        assert_eq!(
            handle_event(&mut state, key(KeyCode::Tab), "claude-opus-4-7"),
            InputOutcome::None
        );
        assert_eq!(state.view, ModelsView::All);
    }

    #[test]
    fn typing_updates_search_and_backspace_removes() {
        let mut state = ModelsModalState::new();
        handle_event(&mut state, key(KeyCode::Char('q')), "claude-opus-4-7");
        handle_event(&mut state, key(KeyCode::Char('w')), "claude-opus-4-7");
        assert_eq!(state.search, "qw");
        handle_event(&mut state, key(KeyCode::Backspace), "claude-opus-4-7");
        assert_eq!(state.search, "q");
    }
}
