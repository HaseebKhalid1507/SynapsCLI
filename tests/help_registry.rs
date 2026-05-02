use synaps_cli::help::{builtin_entries, render_entry, render_help, source_display, HelpEntry, HelpExample, HelpRegistry, HelpTopicKind};

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
fn help_topics_lists_branch_help_and_excludes_command_entries() {
    let registry = HelpRegistry::new(builtin_entries(), Vec::new());
    let rendered = render_help(&registry, Some("topics")).expect("topics help should render");

    assert!(rendered.starts_with("Help topics"), "topics heading missing:\n{}", rendered);
    assert!(rendered.contains("/help plugins"), "topics should include branch help:\n{}", rendered);
    assert!(rendered.contains("/help sessions"), "topics should include conceptual branch help:\n{}", rendered);
    assert!(rendered.contains("/help trust"), "topics should include conceptual trust branch help:\n{}", rendered);
    assert!(!rendered.contains("/compact"), "topics should exclude command entries:\n{}", rendered);
}

#[test]
fn help_reference_groups_all_entries_including_commands_and_plugins() {
    let plugin_entries = vec![HelpEntry {
        id: "acme-sync".to_string(),
        command: "/acme:sync".to_string(),
        title: "Acme Sync".to_string(),
        summary: "Sync Acme workspace state.".to_string(),
        category: "Plugin".to_string(),
        topic: HelpTopicKind::Command,
        protected: false,
        common: false,
        aliases: vec![],
        keywords: vec![],
        lines: vec![],
        usage: None,
        examples: vec![],
        related: vec![],
        source: Some("acme-tools".to_string()),
    }];
    let registry = HelpRegistry::new(builtin_entries(), plugin_entries);
    let rendered = render_help(&registry, Some("reference")).expect("reference help should render");

    assert!(rendered.starts_with("Help reference"), "reference heading missing:\n{}", rendered);
    assert!(rendered.contains("Core"), "reference should group by category:\n{}", rendered);
    assert!(rendered.contains("  /help — Fast paths for finding commands"), "reference should include root summary:\n{}", rendered);
    assert!(rendered.contains("  /compact —"), "reference should include command entries:\n{}", rendered);
    assert!(rendered.contains("Plugin"), "reference should include plugin category:\n{}", rendered);
    assert!(rendered.contains("  /acme:sync — Sync Acme workspace state."), "reference should include plugin entries:\n{}", rendered);
}

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
fn help_entry_deserializes_when_usage_and_examples_are_omitted() {
    let json = r#"{
        "id": "minimal",
        "command": "/minimal",
        "title": "Minimal",
        "summary": "A minimal legacy help entry.",
        "category": "Core",
        "topic": "Command",
        "protected": false,
        "common": false
    }"#;

    let entry: HelpEntry = serde_json::from_str(json).expect("legacy help JSON should still deserialize");

    assert_eq!(entry.usage, None);
    assert!(entry.examples.is_empty());
}

#[test]
fn command_categories_group_common_core_and_session_commands() {
    let registry = HelpRegistry::new(builtin_entries(), Vec::new());

    assert_eq!(registry.entry_by_command("/model").map(|entry| entry.category.as_str()), Some("Core"));
    assert_eq!(registry.entry_by_command("/settings").map(|entry| entry.category.as_str()), Some("Core"));
    assert_eq!(registry.entry_by_command("/clear").map(|entry| entry.category.as_str()), Some("Core"));
    assert_eq!(registry.entry_by_command("/sessions").map(|entry| entry.category.as_str()), Some("Sessions"));
}

#[test]
fn render_entry_does_not_duplicate_related_from_body_lines() {
    let entry = HelpEntry {
        id: "settings".to_string(),
        command: "/help settings".to_string(),
        title: "Settings".to_string(),
        summary: "Configure SynapsCLI.".to_string(),
        category: "Settings".to_string(),
        topic: HelpTopicKind::Branch,
        protected: true,
        common: true,
        aliases: vec![],
        keywords: vec![],
        lines: vec![
            "Use settings for preferences.".to_string(),
            "".to_string(),
            "Related: /help models, /help plugins".to_string(),
        ],
        usage: None,
        examples: vec![],
        related: vec!["/help models".to_string(), "/help plugins".to_string()],
        source: None,
    };

    let rendered = render_entry(&entry);

    assert_eq!(rendered.matches("Related:").count(), 1);
}

#[test]
fn render_entry_includes_usage_and_examples_when_present() {
    let entry = HelpEntry {
        id: "example-rich".to_string(),
        command: "/example".to_string(),
        title: "/example".to_string(),
        summary: "Run the example command.".to_string(),
        category: "Core".to_string(),
        topic: HelpTopicKind::Command,
        protected: false,
        common: false,
        aliases: vec![],
        keywords: vec![],
        lines: vec!["Extra detail.".to_string()],
        usage: Some("/example [name]".to_string()),
        examples: vec![synaps_cli::help::HelpExample {
            command: "/example demo".to_string(),
            description: "Run with a demo name.".to_string(),
        }],
        related: vec!["/help find".to_string()],
        source: None,
    };

    let rendered = render_entry(&entry);

    assert!(rendered.contains("Usage\n  /example [name]"), "usage section missing:\n{}", rendered);
    assert!(rendered.contains("Examples\n  /example demo    Run with a demo name."), "examples section missing:\n{}", rendered);
    assert!(rendered.contains("Related: /help find"), "related section missing:\n{}", rendered);
}

#[test]
fn builtin_complex_commands_have_usage_and_examples() {
    let registry = HelpRegistry::new(builtin_entries(), Vec::new());

    for command in ["/compact", "/chain", "/model", "/plugins"] {
        let entry = registry.entry_by_command(command).unwrap_or_else(|| panic!("{command} entry exists"));
        assert!(entry.usage.is_some(), "{command} should define usage");
        assert!(!entry.examples.is_empty(), "{command} should define examples");
    }
}

#[test]
fn help_drill_down_normalizes_command_topics_before_branch_fallback() {
    let registry = HelpRegistry::new(builtin_entries(), Vec::new());

    for topic in ["model", "/model"] {
        let rendered = render_help(&registry, Some(topic))
            .unwrap_or_else(|| panic!("{topic} help should render"));
        assert!(rendered.starts_with("Model router"), "{topic} should render /model command help:\n{}", rendered);
        assert!(rendered.contains("Usage\n  /model [name]"), "{topic} should include /model usage:\n{}", rendered);
        assert!(!rendered.starts_with("Models"), "{topic} should prefer exact command over models branch:\n{}", rendered);
    }
}

#[test]
fn help_drill_down_prefers_exact_subcommand_before_branch_fallback() {
    let registry = HelpRegistry::new(builtin_entries(), Vec::new());
    let rendered = render_help(&registry, Some("extensions audit"))
        .expect("extensions audit help should render");

    assert!(rendered.starts_with("/extensions audit"), "should render exact command entry:\n{}", rendered);
    assert!(rendered.contains("Usage\n  /extensions audit [limit]"), "should include audit usage:\n{}", rendered);
    assert!(!rendered.starts_with("Extensions"), "should not fall back to extensions branch:\n{}", rendered);
}

#[test]
fn unknown_help_topic_can_include_closest_suggestions() {
    let registry = HelpRegistry::new(builtin_entries(), Vec::new());
    let rendered = render_help(&registry, Some("modle")).expect("unknown help should render fallback");

    assert!(rendered.contains("No help topic"), "missing unknown-topic message:\n{}", rendered);
    assert!(rendered.contains("/help find"), "missing find suggestion:\n{}", rendered);
    assert!(rendered.contains("Closest matches"), "missing closest suggestions:\n{}", rendered);
    assert!(rendered.contains("/model"), "missing likely /model suggestion:\n{}", rendered);
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
        usage: None,
        examples: vec![],
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
            usage: None,
            examples: vec![],
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
            usage: None,
            examples: vec![],
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

#[test]
fn search_ranking_exact_command_and_title_outrank_prefix_and_body() {
    let mut exact_command = test_entry("/model", "Switch Model", "Models", false);
    exact_command.lines = vec!["Body mentions model palette.".to_string()];
    let mut exact_title = test_entry("/zzz", "model", "Advanced", false);
    exact_title.lines = vec!["Body mentions model palette.".to_string()];
    let mut prefix = test_entry("/modelist", "Modelist", "Advanced", false);
    prefix.summary = "Prefix command.".to_string();
    let mut body = test_entry("/alpha", "Alpha", "Advanced", false);
    body.lines = vec!["Only the body mentions model.".to_string()];
    let registry = HelpRegistry::new(vec![body, prefix, exact_title, exact_command], Vec::new());

    let commands = registry
        .search("model")
        .into_iter()
        .map(|entry| entry.command.as_str())
        .collect::<Vec<_>>();

    assert_eq!(commands[..4], ["/model", "/zzz", "/modelist", "/alpha"]);
}

#[test]
fn search_ranking_prefix_outranks_alias_keyword_summary_and_body() {
    let prefix = test_entry("/chain", "Chain", "Sessions", false);
    let mut alias = test_entry("/alias-hit", "Alias Hit", "Advanced", false);
    alias.aliases = vec!["/chain-alias".to_string()];
    let mut keyword = test_entry("/keyword-hit", "Keyword Hit", "Advanced", false);
    keyword.keywords = vec!["chain".to_string()];
    let mut summary = test_entry("/summary-hit", "Summary Hit", "Advanced", false);
    summary.summary = "Summary mentions chain.".to_string();
    let mut body = test_entry("/body-hit", "Body Hit", "Advanced", false);
    body.lines = vec!["Body mentions chain.".to_string()];
    let registry = HelpRegistry::new(vec![body, summary, keyword, alias, prefix], Vec::new());

    let commands = registry
        .search("chain")
        .into_iter()
        .map(|entry| entry.command.as_str())
        .collect::<Vec<_>>();

    assert_eq!(commands[0], "/chain");
    assert_eq!(commands[1], "/alias-hit");
    assert!(commands.iter().position(|command| *command == "/summary-hit").unwrap()
        < commands.iter().position(|command| *command == "/body-hit").unwrap());
}

#[test]
fn search_alias_match_returns_canonical_entry() {
    let mut entry = test_entry("/canonical", "Canonical", "Core", false);
    entry.aliases = vec!["/shortcut".to_string()];
    let registry = HelpRegistry::new(vec![entry], Vec::new());

    let matches = registry.search("shortcut");

    assert_eq!(matches.first().map(|entry| entry.command.as_str()), Some("/canonical"));
}

#[test]
fn empty_search_orders_common_then_core_then_category_and_command() {
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

    let commands = registry
        .search("")
        .into_iter()
        .map(|entry| entry.command.as_str())
        .collect::<Vec<_>>();

    assert_eq!(commands, ["/model", "/settings", "/alpha", "/beta", "/zeta"]);
}

#[test]
fn plugin_help_entry_with_usage_examples_loads_searches_and_forces_plugin_source() {
    let plugin_entries = vec![HelpEntry {
        id: "acme-sync".to_string(),
        command: "/acme:sync".to_string(),
        title: "Acme Sync".to_string(),
        summary: "Sync Acme workspace state.".to_string(),
        category: "Plugin".to_string(),
        topic: HelpTopicKind::Command,
        protected: true,
        common: false,
        aliases: vec!["/acme:pull".to_string()],
        keywords: vec!["workspace".to_string(), "sync".to_string()],
        lines: vec!["Keeps the local Acme cache up to date.".to_string()],
        usage: Some("/acme:sync [workspace]".to_string()),
        examples: vec![HelpExample {
            command: "/acme:sync docs".to_string(),
            description: "Sync the docs workspace.".to_string(),
        }],
        related: vec!["/help plugins".to_string()],
        source: Some("acme-tools".to_string()),
    }];

    let registry = HelpRegistry::new(builtin_entries(), plugin_entries);
    let entry = registry.entry_by_command("/acme:sync").expect("plugin help entry should load");

    assert!(!entry.protected, "plugin help entries must not remain protected");
    assert_eq!(entry.usage.as_deref(), Some("/acme:sync [workspace]"));
    assert_eq!(entry.examples[0].command, "/acme:sync docs");
    assert_eq!(entry.source.as_deref(), Some("plugin acme-tools"));
    assert_eq!(source_display(entry).as_deref(), Some("plugin acme-tools"));

    let matches = registry.search("docs workspace");
    assert!(
        matches.iter().any(|entry| entry.command == "/acme:sync"),
        "plugin help should be searchable by examples/keywords; got {:?}",
        matches.iter().map(|entry| entry.command.as_str()).collect::<Vec<_>>()
    );
}

#[test]
fn plugin_help_entry_without_source_displays_generic_plugin_source() {
    let plugin_entries = vec![HelpEntry {
        id: "acme-status".to_string(),
        command: "/acme:status".to_string(),
        title: "Acme Status".to_string(),
        summary: "Show Acme status.".to_string(),
        category: "Plugin".to_string(),
        topic: HelpTopicKind::Command,
        protected: false,
        common: false,
        aliases: vec![],
        keywords: vec!["acme".to_string()],
        lines: vec![],
        usage: None,
        examples: vec![],
        related: vec![],
        source: None,
    }];

    let registry = HelpRegistry::new(builtin_entries(), plugin_entries);
    let entry = registry.entry_by_command("/acme:status").expect("plugin help entry should load");

    assert_eq!(entry.source.as_deref(), Some("plugin"));
    assert_eq!(source_display(entry).as_deref(), Some("plugin"));
}
