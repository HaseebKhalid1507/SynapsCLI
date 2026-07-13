use synaps_cli::help::{builtin_entries, render_help, HelpEntry, HelpFindState, HelpRegistry, HelpTopicKind};

fn plugin_entry(command: &str, title: &str) -> HelpEntry {
    HelpEntry {
        id: command.trim_start_matches('/').replace([' ', ':'], "-").to_string(),
        command: command.to_string(),
        title: title.to_string(),
        summary: format!("Help for {title}."),
        category: "Plugin".to_string(),
        topic: HelpTopicKind::Command,
        protected: false,
        common: false,
        aliases: vec![],
        keywords: vec!["acme".to_string()],
        lines: vec!["Plugin-provided help detail.".to_string()],
        usage: Some(format!("{command} [options]")),
        examples: vec![],
        related: vec!["/help find".to_string()],
        source: Some("acme-tools".to_string()),
    }
}

#[test]
fn plugin_can_add_namespaced_help_topic_under_help() {
    let mut entry = plugin_entry("/help acme", "Acme Help");
    entry.topic = HelpTopicKind::Branch;
    entry.category = "Acme".to_string();
    entry.aliases = vec!["acme".to_string(), "/acme:help".to_string()];

    let registry = HelpRegistry::new(builtin_entries(), vec![entry]);

    let rendered = render_help(&registry, Some("acme")).expect("plugin help topic should render");
    assert!(rendered.starts_with("Acme Help"), "plugin /help namespace entry should render:\n{rendered}");
    assert!(rendered.contains("Plugin-provided help detail."));
}

#[test]
fn plugin_help_topic_appears_in_find_menu() {
    let mut entry = plugin_entry("/help acme", "Acme Help");
    entry.topic = HelpTopicKind::Branch;
    entry.category = "Acme".to_string();
    entry.summary = "Operate the Acme extension.".to_string();

    let registry = HelpRegistry::new(builtin_entries(), vec![entry]);
    let state = HelpFindState::new(registry.entries().to_vec(), "acme");

    let commands = state
        .filtered_entries()
        .into_iter()
        .map(|entry| entry.command.as_str())
        .collect::<Vec<_>>();

    assert!(commands.contains(&"/help acme"), "plugin help topic should be searchable in /help find; got {commands:?}");
}

#[test]
fn plugin_command_help_entry_appears_in_find_menu() {
    let entry = plugin_entry("/acme:sync", "Acme Sync");

    let registry = HelpRegistry::new(builtin_entries(), vec![entry]);
    let state = HelpFindState::new(registry.entries().to_vec(), "sync");

    let commands = state
        .filtered_entries()
        .into_iter()
        .map(|entry| entry.command.as_str())
        .collect::<Vec<_>>();

    assert!(commands.contains(&"/acme:sync"), "plugin command help should be searchable in /help find; got {commands:?}");
}

#[test]
fn plugin_cannot_hijack_builtin_help_topics_but_can_add_new_help_topics() {
    let mut hijack = plugin_entry("/help settings", "Hijacked Settings");
    hijack.id = "settings".to_string();
    hijack.topic = HelpTopicKind::Branch;

    let mut allowed = plugin_entry("/help acme", "Acme Help");
    allowed.topic = HelpTopicKind::Branch;

    let registry = HelpRegistry::new(builtin_entries(), vec![hijack, allowed]);

    let settings = render_help(&registry, Some("settings")).expect("builtin settings should render");
    assert!(!settings.starts_with("Hijacked Settings"), "protected builtin help topic was hijacked:\n{settings}");
    assert!(registry.entry_by_command("/help acme").is_some(), "non-conflicting plugin help topic should load");
}
