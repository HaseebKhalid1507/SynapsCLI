use synaps_cli::help::{builtin_entries, render_help, HelpEntry, HelpRegistry, HelpTopicKind};

#[test]
fn base_help_is_brief_and_points_to_find() {
    let registry = HelpRegistry::new(builtin_entries(), Vec::new());
    let rendered = render_help(&registry, None).expect("base help should render");

    assert!(rendered.lines().count() <= 20, "base help should stay brief:\n{}", rendered);
    assert!(rendered.contains("/help find"), "base help should point to search:\n{}", rendered);
    assert!(rendered.contains("/settings"), "base help should include settings:\n{}", rendered);
    assert!(rendered.contains("/plugins"), "base help should include plugins:\n{}", rendered);
    assert!(!rendered.contains("/extensions audit"), "base help should not dump advanced commands:\n{}", rendered);
}

#[test]
fn branch_help_renders_specific_topic() {
    let registry = HelpRegistry::new(builtin_entries(), Vec::new());
    let rendered = render_help(&registry, Some("plugins")).expect("plugins help should render");

    assert!(rendered.contains("Plugins"), "plugins title missing:\n{}", rendered);
    assert!(rendered.contains("/plugins"), "plugins command missing:\n{}", rendered);
    assert!(rendered.contains("/help find"), "related discovery missing:\n{}", rendered);
    assert!(!rendered.contains("What would you like to do?"), "branch should not duplicate base help:\n{}", rendered);
}

#[test]
fn unknown_branch_suggests_help_find() {
    let registry = HelpRegistry::new(builtin_entries(), Vec::new());
    let rendered = render_help(&registry, Some("wat")).expect("unknown help should render fallback");

    assert!(rendered.contains("No help topic"), "missing unknown-topic message:\n{}", rendered);
    assert!(rendered.contains("/help find"), "missing find suggestion:\n{}", rendered);
}

#[test]
fn plugin_entries_cannot_override_protected_help_namespace() {
    let mut plugin_entries = vec![
        HelpEntry {
            id: "evil-help".to_string(),
            command: "/help".to_string(),
            title: "Hijacked".to_string(),
            summary: "bad".to_string(),
            category: "Plugin".to_string(),
            topic: HelpTopicKind::Command,
            protected: false,
            common: false,
            aliases: vec![],
            keywords: vec![],
            lines: vec!["bad".to_string()],
            related: vec![],
            source: Some("evil".to_string()),
        },
        HelpEntry {
            id: "good-plugin".to_string(),
            command: "/evil:search".to_string(),
            title: "Plugin Search".to_string(),
            summary: "Search from a plugin.".to_string(),
            category: "Plugin".to_string(),
            topic: HelpTopicKind::Command,
            protected: false,
            common: false,
            aliases: vec![],
            keywords: vec!["plugin".to_string()],
            lines: vec!["Search from a plugin.".to_string()],
            related: vec![],
            source: Some("evil".to_string()),
        },
    ];

    let registry = HelpRegistry::new(builtin_entries(), std::mem::take(&mut plugin_entries));

    assert_ne!(registry.entry_by_command("/help").unwrap().title, "Hijacked");
    assert!(registry.entry_by_command("/evil:search").is_some());
}

#[test]
fn help_find_filters_in_memory_by_command_summary_keywords_and_aliases() {
    let registry = HelpRegistry::new(builtin_entries(), Vec::new());

    let model_matches = registry.search("provider");
    assert!(model_matches.iter().any(|entry| entry.command == "/help models"), "provider should find models help");

    let plugin_matches = registry.search("PLUGIN");
    assert!(plugin_matches.iter().any(|entry| entry.command == "/help plugins"), "filter should be case-insensitive");
}
