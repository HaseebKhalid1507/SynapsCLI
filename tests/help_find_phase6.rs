use synaps_cli::help::{HelpEntry, HelpFindState, HelpRegistry, HelpTopicKind};

fn test_entry(command: &str, title: &str, category: &str, common: bool) -> HelpEntry {
    HelpEntry {
        id: command.trim_start_matches('/').replace(' ', "-").to_string(),
        command: command.to_string(),
        title: title.to_string(),
        summary: format!("{} summary", title),
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
fn help_find_places_help_commands_category_last_with_parent_command_first() {
    let registry = HelpRegistry::new(
        vec![
            test_entry("/help find", "Find Help", "Core", true),
            test_entry("/help", "Help", "Core", true),
            test_entry("/plugins", "Plugins Modal", "Plugins", true),
            test_entry("/model", "Model", "Models", true),
        ],
        Vec::new(),
    );
    let state = HelpFindState::new(registry.entries().to_vec(), "");

    let rows = state.filtered_rows();
    let help_header_index = rows.iter().position(|row| row.category() == Some("Help commands")).unwrap();
    assert_eq!(help_header_index, rows.iter().rposition(|row| row.category().is_some()).unwrap());
    assert_eq!(rows[help_header_index + 1].entry().map(|entry| entry.command.as_str()), Some("/help"));
    assert_eq!(rows[help_header_index + 2].entry().map(|entry| entry.command.as_str()), Some("/help find"));
}

#[test]
fn help_find_default_state_includes_help_topics_and_real_commands() {
    let registry = HelpRegistry::new(
        vec![
            test_entry("/help", "Help", "Core", true),
            test_entry("/help plugins", "Plugins Help", "Plugins", true),
            test_entry("/plugins", "Plugins Modal", "Plugins", true),
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

    assert!(commands.contains(&"/help"));
    assert!(commands.contains(&"/help plugins"));
    assert!(commands.contains(&"/plugins"));
    assert!(commands.contains(&"/model"));
}

#[test]
fn help_find_groups_help_topics_under_help_commands_and_real_commands_under_their_theme() {
    let registry = HelpRegistry::new(
        vec![
            test_entry("/help plugins", "Plugins Help", "Plugins", true),
            test_entry("/plugins", "Plugins Modal", "Plugins", true),
            test_entry("/model", "Model", "Models", true),
        ],
        Vec::new(),
    );
    let state = HelpFindState::new(registry.entries().to_vec(), "");

    let rows = state.filtered_rows();
    let help_header_index = rows.iter().position(|row| row.category() == Some("Help commands")).unwrap();
    let plugins_header_index = rows.iter().position(|row| row.category() == Some("Plugins")).unwrap();

    assert!(rows[help_header_index + 1].entry().is_some_and(|entry| entry.command == "/help plugins"));
    assert!(rows[plugins_header_index + 1].entry().is_some_and(|entry| entry.command == "/plugins"));
}

#[test]
fn help_find_empty_query_groups_each_category_once_even_when_ranking_interleaves_categories() {
    let registry = HelpRegistry::new(
        vec![
            test_entry("/core-common", "Core Common", "Core", true),
            test_entry("/plugin-common", "Plugin Common", "Plugins", true),
            test_entry("/core-rare", "Core Rare", "Core", false),
            test_entry("/plugin-rare", "Plugin Rare", "Plugins", false),
        ],
        Vec::new(),
    );
    let state = HelpFindState::new(registry.entries().to_vec(), "");

    let categories = state
        .filtered_rows()
        .into_iter()
        .filter_map(|row| row.category().map(str::to_string))
        .collect::<Vec<_>>();

    assert_eq!(categories, vec!["Core", "Plugins"]);
}

#[test]
fn help_find_can_scope_to_help_commands_for_explicit_help_only_views() {
    let registry = HelpRegistry::new(
        vec![
            test_entry("/help", "Help", "Core", true),
            test_entry("/help plugins", "Plugins", "Plugins", true),
            test_entry("/plugins", "Plugins Modal", "Plugins", true),
            test_entry("/model", "Model", "Models", true),
        ],
        Vec::new(),
    );
    let state = HelpFindState::new_help_commands(registry.entries().to_vec(), "");

    let commands = state
        .filtered_entries()
        .into_iter()
        .map(|entry| entry.command.as_str())
        .collect::<Vec<_>>();

    assert_eq!(commands, vec!["/help", "/help plugins"]);
}

#[test]
fn help_find_sections_group_empty_query_by_category_with_header_rows() {
    let registry = HelpRegistry::new(
        vec![
            test_entry("/zeta", "Zeta", "Advanced", false),
            test_entry("/settings", "Settings", "Settings", true),
            test_entry("/model", "Model", "Models", true),
        ],
        Vec::new(),
    );
    let state = HelpFindState::new(registry.entries().to_vec(), "");

    let rows = state.filtered_rows();

    assert_eq!(rows[0].category(), Some("Models"));
    assert_eq!(rows[1].entry().map(|entry| entry.command.as_str()), Some("/model"));
    assert_eq!(rows[2].category(), Some("Settings"));
    assert_eq!(rows[3].entry().map(|entry| entry.command.as_str()), Some("/settings"));
    assert_eq!(rows[4].category(), Some("Advanced"));
    assert_eq!(rows[5].entry().map(|entry| entry.command.as_str()), Some("/zeta"));
}

#[test]
fn help_find_navigation_skips_category_headers() {
    let registry = HelpRegistry::new(
        vec![
            test_entry("/alpha", "Alpha", "Core", true),
            test_entry("/beta", "Beta", "Core", false),
        ],
        Vec::new(),
    );
    let mut state = HelpFindState::new(registry.entries().to_vec(), "");

    assert_eq!(state.cursor(), 1, "initial cursor should select first entry, not category header");
    assert_eq!(state.selected().map(|entry| entry.command.as_str()), Some("/alpha"));

    state.move_up();
    assert_eq!(state.selected().map(|entry| entry.command.as_str()), Some("/alpha"));

    state.move_down();
    assert_eq!(state.selected().map(|entry| entry.command.as_str()), Some("/beta"));
}

#[test]
fn help_find_section_headers_are_hidden_when_query_filters() {
    let registry = HelpRegistry::new(
        vec![
            test_entry("/model", "Model", "Models", true),
            test_entry("/settings", "Settings", "Settings", true),
        ],
        Vec::new(),
    );
    let state = HelpFindState::new(registry.entries().to_vec(), "model");

    let rows = state.filtered_rows();

    assert!(rows.iter().all(|row| row.entry().is_some()));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].entry().map(|entry| entry.command.as_str()), Some("/model"));
}

#[test]
fn help_find_highlight_spans_mark_query_matches_in_command_and_summary() {
    let mut entry = test_entry("/model", "Model", "Models", true);
    entry.summary = "Choose a model provider".to_string();

    let command_spans = synaps_cli::help::highlight_segments(&entry.command, "mod");
    assert_eq!(command_spans.len(), 3);
    assert_eq!(command_spans[0].text, "/");
    assert!(!command_spans[0].matched);
    assert_eq!(command_spans[1].text, "mod");
    assert!(command_spans[1].matched);
    assert_eq!(command_spans[2].text, "el");
    assert!(!command_spans[2].matched);

    let summary_spans = synaps_cli::help::highlight_segments(&entry.summary, "model");
    assert!(summary_spans.iter().any(|span| span.text == "model" && span.matched));
}

#[test]
fn help_find_mru_boosts_recently_opened_entry_without_beating_exact_command() {
    let registry = HelpRegistry::new(
        vec![
            test_entry("/model", "Model", "Models", false),
            test_entry("/modelist", "Modelist", "Advanced", false),
        ],
        Vec::new(),
    );
    let mut state = HelpFindState::new(registry.entries().to_vec(), "model");

    while state.selected().map(|entry| entry.command.as_str()) != Some("/modelist") {
        state.move_down();
    }
    state.open_selected();
    state.close_detail();

    let commands = state
        .filtered_entries()
        .into_iter()
        .map(|entry| entry.command.as_str())
        .collect::<Vec<_>>();

    assert_eq!(commands[..2], ["/model", "/modelist"], "exact command stays first");

    state.clear_filter();
    state.push_char('m');
    state.push_char('o');
    state.push_char('d');
    state.push_char('e');
    state.push_char('l');
    state.push_char('i');

    let commands = state
        .filtered_entries()
        .into_iter()
        .map(|entry| entry.command.as_str())
        .collect::<Vec<_>>();
    assert_eq!(commands.first().copied(), Some("/modelist"));
}
