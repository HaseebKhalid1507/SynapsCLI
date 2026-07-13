use synaps_cli::help::HelpEntry;

#[test]
fn slash_help_palette_query_uses_partial_command_without_slash() {
    assert_eq!(synaps_cli::help::prefilter_query_for_slash_command("/mod"), Some("mod".to_string()));
    assert_eq!(synaps_cli::help::prefilter_query_for_slash_command("  /extensions au"), Some("extensions au".to_string()));
}

#[test]
fn slash_help_palette_query_requires_ambiguous_incomplete_command() {
    assert_eq!(synaps_cli::help::prefilter_query_for_slash_command("/"), None);
    assert_eq!(synaps_cli::help::prefilter_query_for_slash_command("hello"), None);
}

#[test]
fn ambiguous_prefix_match_count_counts_commands_and_aliases_once() {
    let registry = synaps_cli::help::HelpRegistry::new(synaps_cli::help::builtin_entries(), Vec::<HelpEntry>::new());

    let count = registry.command_prefix_match_count("mod");

    assert!(count >= 1);
    assert_eq!(registry.command_prefix_match_count("model"), 2);
}
