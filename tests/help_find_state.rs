use synaps_cli::help::{builtin_entries, HelpEntry, HelpFindState, HelpRegistry, HelpTopicKind};

fn test_entry(command: &str, title: &str, category: &str, common: bool) -> HelpEntry {
    HelpEntry {
        id: command.trim_start_matches('/').replace(' ', "-").to_string(),
        command: command.to_string(),
        title: title.to_string(),
        summary: String::new(),
        category: category.to_string(),
        topic: HelpTopicKind::Command,
        protected: false,
        common,
        aliases: vec![],
        keywords: vec![],
        lines: vec![],
        usage: None,
        examples: vec![],
        related: vec![],
        source: None,
    }
}

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

#[test]
fn find_state_ranks_exact_command_title_then_prefix_then_lower_quality_matches() {
    let mut exact_command = test_entry("/model", "Switch Model", "Models", false);
    exact_command.lines = vec!["Body mentions model palette.".to_string()];
    let mut exact_title = test_entry("/zzz", "model", "Advanced", false);
    exact_title.lines = vec!["Body mentions model palette.".to_string()];
    let prefix = test_entry("/modelist", "Modelist", "Advanced", false);
    let mut alias = test_entry("/alias-hit", "Alias Hit", "Advanced", false);
    alias.aliases = vec!["/model-alias".to_string()];
    let mut body = test_entry("/alpha", "Alpha", "Advanced", false);
    body.lines = vec!["Only the body mentions model.".to_string()];
    let registry = HelpRegistry::new(vec![body, alias, prefix, exact_title, exact_command], Vec::new());

    let state = HelpFindState::new(registry.entries().to_vec(), "model");
    let commands = state
        .filtered_entries()
        .into_iter()
        .map(|entry| entry.command.as_str())
        .collect::<Vec<_>>();

    assert_eq!(commands[..5], ["/model", "/zzz", "/modelist", "/alias-hit", "/alpha"]);
}

#[test]
fn find_state_empty_query_orders_common_core_category_command() {
    let registry = HelpRegistry::new(
        vec![
            test_entry("/zeta", "Zeta", "Advanced", false),
            test_entry("/beta", "Beta", "Core", false),
            test_entry("/alpha", "Alpha", "Core", false),
            test_entry("/settings", "Settings", "Settings", true),
            test_entry("/model", "Model", "Models", true),
        ],
        Vec::new(),
    );
    let state = HelpFindState::new(registry.entries().to_vec(), "");

    let commands = state
        .filtered_entries()
        .into_iter()
        .map(|entry| entry.command.as_str())
        .collect::<Vec<_>>();

    assert_eq!(commands, ["/model", "/settings", "/alpha", "/beta", "/zeta"]);
}

#[test]
fn find_state_no_results_message_suggests_stable_queries() {
    let registry = HelpRegistry::new(builtin_entries(), Vec::new());
    let state = HelpFindState::new(registry.entries().to_vec(), "zzzz-no-match");

    assert!(state.filtered_entries().is_empty());
    let message = state.no_results_message();
    assert!(message.contains("No help matches for 'zzzz-no-match'."));
    assert!(message.contains("Try: model, settings, plugins, sessions, doctor"));
}
