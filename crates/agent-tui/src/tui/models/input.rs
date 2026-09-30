use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Keys: every printable character types into the search (the picker says
/// "type to search", so no letter may be stolen by a command). Commands are
/// arrows, Tab, Enter, Esc and Ctrl chords: ↑↓ move, ←/→ collapse / open a
/// provider section, Ctrl+E expand the provider's live catalog, Ctrl+F
/// favorite.
fn ctrl(key: &KeyEvent, ch: char) -> bool {
    key.code == KeyCode::Char(ch) && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// A character to type into a search field: no Ctrl/Alt chord.
fn typed_char(key: &KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(ch)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            Some(ch)
        }
        _ => None,
    }
}

use super::{
    build_sections, expanded_visible_models, normalize_favorite_id, remove_favorite_compat,
    selected_expanded_model, selected_model, selected_provider, visible_rows, ExpandedLoadState,
    ExpandedModelsState, ModelsModalState, ModelsView,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InputOutcome {
    None,
    Close,
    Apply(String),
    ExpandProvider(String),
    /// The user explicitly trusted a model mid-session (favorited it). The
    /// caller must propagate this grant into the live delegation policy so
    /// subagent dispatch honors it immediately — not only after a restart.
    Trusted(String),
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
    if ctrl(&key, 'e') {
        if let Some(provider) = selected_provider(&sections, state) {
            let (provider_key, provider_name) = (
                provider.provider_key.clone(),
                provider.provider_name.clone(),
            );
            return open_expanded_provider(state, provider_key, provider_name);
        }
        return InputOutcome::None;
    }
    if ctrl(&key, 'f') {
        if let Some(model) = selected_model(&sections, state) {
            let trusted = if model.is_favorite {
                remove_favorite_compat(&model.favorite_id);
                None
            } else {
                let _ = synaps_cli::config::add_favorite_model(&normalize_favorite_id(
                    &model.favorite_id,
                ));
                // Runtime-qualified identity of the visible row — the same
                // exact ID that Apply uses — for the live policy grant.
                Some(model.id.clone())
            };
            state.refresh_favorites();
            let new_len = visible_rows(&build_sections(current_model, state), state).len();
            if new_len == 0 {
                state.cursor = 0;
            } else if state.cursor >= new_len {
                state.cursor = new_len - 1;
            }
            if let Some(model_id) = trusted {
                return InputOutcome::Trusted(model_id);
            }
        }
        return InputOutcome::None;
    }
    if let Some(ch) = typed_char(&key) {
        state.search.push(ch);
        state.cursor = 0;
        return InputOutcome::None;
    }
    match key.code {
        KeyCode::Esc => InputOutcome::Close,
        KeyCode::Up => {
            state.cursor = state.cursor.saturating_sub(1);
            InputOutcome::None
        }
        KeyCode::Down => {
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
        // ← collapses the provider section under the cursor, → opens it.
        KeyCode::Left | KeyCode::Right => {
            let rows = visible_rows(&sections, state);
            if let Some(super::VisibleRow::Section { idx }) = rows.get(state.cursor) {
                if let Some(section) = sections.get(*idx) {
                    if key.code == KeyCode::Left {
                        state.collapsed.insert(section.provider_key.clone());
                    } else {
                        state.collapsed.remove(&section.provider_key);
                    }
                }
            }
            InputOutcome::None
        }
        KeyCode::Enter => {
            if let Some(model) = selected_model(&sections, state) {
                // Apply the provider-qualified identity of the visible row directly.
                // Favorite normalization is compatibility-only and must not reroute it.
                InputOutcome::Apply(model.id.clone())
            } else {
                InputOutcome::None
            }
        }
        KeyCode::Backspace => {
            state.search.pop();
            state.cursor = 0;
            InputOutcome::None
        }
        _ => InputOutcome::None,
    }
}

/// Open the expanded provider browser for `provider_key` (the 'e' key).
///
/// Source-controlled providers (openai-codex) resolve immediately to their
/// static entries — no `ExpandProvider` action is emitted, so no network
/// catalog fetch is ever initiated. All other providers (Anthropic included)
/// enter `Loading` and request an async live catalog fetch as before.
pub(crate) fn open_expanded_provider(
    state: &mut ModelsModalState,
    provider_key: String,
    provider_name: String,
) -> InputOutcome {
    state.expanded = Some(ExpandedModelsState {
        provider_key: provider_key.clone(),
        provider_name,
        cursor: 0,
        search: String::new(),
        load_state: ExpandedLoadState::Loading,
    });
    if provider_key == "openai-codex" {
        // Canonicalization resolves this to the eight static OAuth entries
        // (and marks favorites) — same central invariant as live results.
        super::apply_model_list_result(state, &provider_key, Ok(Vec::new()));
        return InputOutcome::None;
    }
    InputOutcome::ExpandProvider(provider_key)
}

fn handle_expanded_event(state: &mut ModelsModalState, key: KeyEvent) -> InputOutcome {
    if ctrl(&key, 'f') {
        if let Some(model) = selected_expanded_model(state) {
            let trusted = if model.is_favorite {
                remove_favorite_compat(&model.id);
                None
            } else {
                let _ = synaps_cli::config::add_favorite_model(&normalize_favorite_id(&model.id));
                Some(model.id.clone())
            };
            state.refresh_favorites();
            if let Some(model_id) = trusted {
                return InputOutcome::Trusted(model_id);
            }
        }
        return InputOutcome::None;
    }
    if let Some(ch) = typed_char(&key) {
        if let Some(expanded) = state.expanded.as_mut() {
            expanded.search.push(ch);
            expanded.cursor = 0;
        }
        return InputOutcome::None;
    }
    match key.code {
        KeyCode::Esc => {
            state.expanded = None;
            InputOutcome::None
        }
        KeyCode::Up => {
            if let Some(expanded) = state.expanded.as_mut() {
                expanded.cursor = expanded.cursor.saturating_sub(1);
            }
            InputOutcome::None
        }
        KeyCode::Down => {
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
        KeyCode::Backspace => {
            if let Some(expanded) = state.expanded.as_mut() {
                expanded.search.pop();
                expanded.cursor = 0;
            }
            InputOutcome::None
        }
        _ => InputOutcome::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::models::{model_id_for_runtime, ExpandedModelEntry};
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
    fn expanding_openai_codex_bypasses_network_and_shows_eight_static_rows() {
        let mut state = ModelsModalState::new();
        let outcome = open_expanded_provider(
            &mut state,
            "openai-codex".to_string(),
            "OpenAI Codex".to_string(),
        );
        // No ExpandProvider action → no catalog fetch is ever initiated.
        assert_eq!(
            outcome,
            InputOutcome::None,
            "openai-codex expansion must not request a live catalog fetch"
        );
        let expanded = state.expanded.expect("expanded state");
        assert_eq!(expanded.provider_key, "openai-codex");
        match expanded.load_state {
            ExpandedLoadState::Ready(models) => {
                let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
                assert_eq!(
                    ids,
                    vec![
                        "openai-codex/gpt-6-astra",
                        "openai-codex/gpt-5.6-sol",
                        "openai-codex/gpt-5.6-terra",
                        "openai-codex/gpt-5.6-luna",
                        "openai-codex/gpt-5.5",
                        "openai-codex/gpt-5.4",
                        "openai-codex/gpt-5.4-mini",
                        "openai-codex/gpt-5.3-codex-spark",
                    ],
                    "expanded rows must be exactly the eight static OAuth models, in order"
                );
            }
            other => panic!("expected static Ready rows without fetching, got {other:?}"),
        }
    }

    #[test]
    fn expanding_anthropic_still_requests_live_catalog() {
        let mut state = ModelsModalState::new();
        let outcome =
            open_expanded_provider(&mut state, "anthropic".to_string(), "Anthropic".to_string());
        assert_eq!(
            outcome,
            InputOutcome::ExpandProvider("anthropic".to_string())
        );
        let expanded = state.expanded.expect("expanded state");
        assert_eq!(expanded.load_state, ExpandedLoadState::Loading);
    }

    /// `e` expands whichever provider is under the cursor.
    ///
    /// The expectation is DERIVED from `build_sections`, not hardcoded: that
    /// function reads real host state (`configured_static_provider_keys()` and
    /// `logged_in_oauth_providers()`), so which provider sorts first differs
    /// per machine. Asserting "anthropic" passed only on hosts holding
    /// Anthropic credentials and failed everywhere else — on a clean CI runner
    /// the first row is `azure-openai`.
    ///
    /// Load semantics stay out of scope here because they are provider-class
    /// dependent (live-catalog providers go to `Loading`, static-OAuth ones to
    /// a `Ready` row set without fetching). Those are covered exactly by
    /// `expanding_anthropic_still_requests_live_catalog` and the static-OAuth
    /// test above; this one pins the routing.
    #[test]
    fn ctrl_e_opens_expanded_provider_browser() {
        // Host state is read TWICE (the expectation below, then again inside
        // handle_event). Config-env tests swap SYNAPS_BASE_DIR to an empty
        // tempdir under CONFIG_ENV_TEST_LOCK; without the lock one of those
        // can land between the reads, and a logged-in provider (anthropic)
        // vanishes from the second read only.
        let _guard = crate::tui::CONFIG_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut state = ModelsModalState::new();
        state.view = ModelsView::All;
        let sections = crate::tui::models::build_sections("claude-opus-4-7", &state);
        let expected = crate::tui::models::selected_provider(&sections, &state)
            .expect("a provider row is selected at cursor 0")
            .provider_key
            .clone();

        let outcome = handle_event(
            &mut state,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
            "claude-opus-4-7",
        );
        assert_eq!(outcome, InputOutcome::ExpandProvider(expected.clone()));
        let expanded = state.expanded.expect("expanded state");
        assert_eq!(expanded.provider_key, expected);
        assert_eq!(expanded.search, "");
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

    /// Regression: the picker says "type to search", so every letter —
    /// including the old single-key commands c / e / f / j / k — types.
    #[test]
    fn every_letter_types_into_the_search() {
        let _guard = crate::tui::CONFIG_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut state = ModelsModalState::new();
        for ch in "codex fjk".chars() {
            let outcome = handle_event(&mut state, key(KeyCode::Char(ch)), "claude-opus-4-7");
            assert_eq!(outcome, InputOutcome::None, "{ch:?} is not a command");
        }
        assert_eq!(state.search, "codex fjk");
        assert!(state.expanded.is_none(), "e no longer opens the catalog");
        assert!(state.collapsed.is_empty(), "c no longer folds a section");

        // Same inside the expanded catalog.
        state.expanded = Some(ExpandedModelsState {
            provider_key: "openai-codex".into(),
            provider_name: "OpenAI Codex".into(),
            cursor: 0,
            search: String::new(),
            load_state: ExpandedLoadState::Loading,
        });
        for ch in "fjk".chars() {
            handle_event(&mut state, key(KeyCode::Char(ch)), "claude-opus-4-7");
        }
        assert_eq!(state.expanded.as_ref().unwrap().search, "fjk");
    }

    #[test]
    fn left_folds_and_right_opens_a_provider_section() {
        let _guard = crate::tui::CONFIG_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut state = ModelsModalState::new();
        state.view = ModelsView::All;
        state.cursor = 0; // the first row is a provider section
        let sections = build_sections("claude-opus-4-7", &state);
        let Some(super::super::VisibleRow::Section { idx }) =
            visible_rows(&sections, &state).first().cloned()
        else {
            panic!("first row should be a section");
        };
        let provider = sections[idx].provider_key.clone();
        handle_event(&mut state, key(KeyCode::Left), "claude-opus-4-7");
        assert!(state.collapsed.contains(&provider), "← folds");
        handle_event(&mut state, key(KeyCode::Right), "claude-opus-4-7");
        assert!(!state.collapsed.contains(&provider), "→ opens");
    }
}
