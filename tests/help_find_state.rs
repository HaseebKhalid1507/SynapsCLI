use synaps_cli::help::{builtin_entries, HelpFindState, HelpRegistry};

#[test]
fn find_state_initial_query_filters_results() {
    let registry = HelpRegistry::new(builtin_entries(), Vec::new());
    let state = HelpFindState::new(registry.entries().to_vec(), "plugin");

    assert_eq!(state.filter(), "plugin");
    assert!(state.filtered_entries().iter().any(|entry| entry.command == "/help plugins"));
    assert!(state.filtered_entries().iter().all(|entry| {
        let text = format!(
            "{} {} {} {:?} {:?} {:?}",
            entry.command,
            entry.title,
            entry.summary,
            entry.keywords,
            entry.aliases,
            entry.lines
        )
        .to_ascii_lowercase();
        text.contains("plugin")
    }));
}

#[test]
fn find_state_navigation_clamps_and_scrolls() {
    let registry = HelpRegistry::new(builtin_entries(), Vec::new());
    let mut state = HelpFindState::new(registry.entries().to_vec(), "");
    state.set_visible_height(3);

    state.move_down();
    state.move_down();
    state.move_down();
    state.move_down();

    assert_eq!(state.cursor(), 4);
    assert_eq!(state.scroll(), 2);

    for _ in 0..1000 {
        state.move_down();
    }
    assert_eq!(state.cursor(), state.filtered_entries().len() - 1);

    for _ in 0..1000 {
        state.move_up();
    }
    assert_eq!(state.cursor(), 0);
    assert_eq!(state.scroll(), 0);
}

#[test]
fn find_state_enter_opens_detail_and_escape_returns_to_list() {
    let registry = HelpRegistry::new(builtin_entries(), Vec::new());
    let mut state = HelpFindState::new(registry.entries().to_vec(), "plugins");

    assert!(state.detail_entry().is_none());
    while state.selected().map(|entry| entry.command.as_str()) != Some("/help plugins") {
        state.move_down();
    }
    state.open_selected();
    let detail = state.detail_entry().expect("plugins detail should open");
    assert_eq!(detail.command.as_str(), "/help plugins");
    assert!(detail.examples.iter().any(|example| example.command == "/plugins"));
    state.close_detail();
    assert!(state.detail_entry().is_none());
}

#[test]
fn find_state_enter_selects_current_entry_and_esc_closes() {
    let registry = HelpRegistry::new(builtin_entries(), Vec::new());
    let mut state = HelpFindState::new(registry.entries().to_vec(), "models");

    let selected = state.selected().expect("selection");
    let selected_text = format!(
        "{} {} {} {:?} {:?} {:?}",
        selected.command,
        selected.title,
        selected.summary,
        selected.keywords,
        selected.aliases,
        selected.lines
    )
    .to_ascii_lowercase();
    assert!(selected_text.contains("model"));

    state.push_char('x');
    assert_eq!(state.cursor(), 0);
    assert_eq!(state.scroll(), 0);
    state.backspace();
    assert_eq!(state.filter(), "models");
}
