use synaps_cli::help::{builtin_entries, render_entry, render_help, HelpEntry, HelpRegistry, HelpTopicKind};

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
fn base_help_uses_polished_intro_copy() {
    let registry = HelpRegistry::new(builtin_entries(), Vec::new());
    let rendered = render_help(&registry, None).expect("base help should render");
    let root = registry.entry_by_command("/help").expect("root help entry exists");

    assert_eq!(root.title, "SynapsCLI Help");
    assert_eq!(root.summary, "Fast paths for finding commands, settings, plugins, models, and diagnostics.");
    assert!(rendered.contains("Start here. Pick a path or search everything."), "missing polished intro:\n{}", rendered);
    assert!(!rendered.contains("Beautiful brief guide"), "placeholder copy leaked into rendered help:\n{}", rendered);
    assert!(!root.summary.contains("Beautiful brief guide"), "placeholder summary leaked into metadata");
}

#[test]
fn root_help_uses_common_paths_and_guides() {
    let registry = HelpRegistry::new(builtin_entries(), Vec::new());
    let rendered = render_help(&registry, None).expect("base help should render");

    assert!(rendered.contains("Common paths"), "root should group fast paths:\n{}", rendered);
    assert!(rendered.contains("Guides"), "root should include guide links:\n{}", rendered);
    assert!(rendered.contains("/doctor"), "root should include diagnostics path:\n{}", rendered);
    assert!(rendered.contains("/help sessions"), "root should link sessions guide:\n{}", rendered);
    assert!(!rendered.contains("Common commands"), "phase 1 root should use common paths wording:\n{}", rendered);
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
fn phase_one_topics_render_from_help() {
    let registry = HelpRegistry::new(builtin_entries(), Vec::new());

    for topic in ["sessions", "extensions", "trust", "compact", "chain"] {
        let rendered = render_help(&registry, Some(topic))
            .unwrap_or_else(|| panic!("{topic} help should render"));
        assert!(
            !rendered.contains("No help topic"),
            "{topic} should be a concrete help entry:\n{}",
            rendered
        );
        assert!(rendered.lines().count() >= 3, "{topic} help should have useful detail:\n{}", rendered);
    }
}

#[test]
fn phase_one_topics_are_searchable_by_keywords() {
    let registry = HelpRegistry::new(builtin_entries(), Vec::new());

    for (query, command) in [
        ("resume", "/help sessions"),
        ("extension audit", "/help extensions"),
        ("protected namespace", "/help trust"),
        ("summarize history", "/compact"),
        ("compaction history", "/chain"),
    ] {
        let matches = registry.search(query);
        assert!(
            matches.iter().any(|entry| entry.command == command),
            "query {query:?} should find {command}; got {:?}",
            matches.iter().map(|entry| entry.command.as_str()).collect::<Vec<_>>()
        );
    }
}

#[test]
fn phase_one_command_entries_render_directly() {
    let registry = HelpRegistry::new(builtin_entries(), Vec::new());

    for command in ["/compact", "/chain"] {
        let entry = registry.entry_by_command(command).unwrap_or_else(|| panic!("{command} entry exists"));
        let rendered = render_entry(entry);
        assert!(rendered.contains(command), "{command} detail should show command usage:\n{}", rendered);
    }
}

#[test]
fn unknown_branch_suggests_help_find() {
    let registry = HelpRegistry::new(builtin_entries(), Vec::new());
    let rendered = render_help(&registry, Some("wat")).expect("unknown help should render fallback");

    assert!(rendered.contains("No help topic"), "missing unknown-topic message:\n{}", rendered);
    assert!(rendered.contains("/help find"), "missing find suggestion:\n{}", rendered);
}

#[test]
fn plugin_entries_cannot_shadow_protected_namespace_by_alias() {
    let plugin_entries = vec![HelpEntry {
        id: "safe-id".to_string(),
        command: "/plugin:settings-help".to_string(),
        title: "Alias Hijack".to_string(),
        summary: "bad".to_string(),
        category: "Plugin".to_string(),
        topic: HelpTopicKind::Command,
        protected: false,
        common: false,
        aliases: vec!["/settings".to_string()],
        keywords: vec![],
        lines: vec![],
        related: vec![],
        source: Some("evil".to_string()),
    }];

    let registry = HelpRegistry::new(builtin_entries(), plugin_entries);

    assert!(registry.entry_by_command("/plugin:settings-help").is_none());
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
