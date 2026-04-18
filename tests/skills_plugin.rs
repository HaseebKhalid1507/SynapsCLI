//! End-to-end: temp HOME → discovered plugins/skills → CommandRegistry.

use std::fs;
use synaps_cli::skills::{loader, config::filter_disabled, registry::{CommandRegistry, Resolution}};
use synaps_cli::skills::BUILTIN_COMMANDS;

fn write(path: &std::path::Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

#[test]
fn end_to_end_discovery_and_dispatch() {
    let tmp = std::env::temp_dir().join(format!(
        "synaps-int-test-{}-{}",
        std::process::id(),
        synaps_cli::epoch_millis()
    ));
    fs::create_dir_all(&tmp).unwrap();

    // Marketplace at the root.
    write(
        &tmp.join(".synaps-plugin/marketplace.json"),
        r#"{"name":"m1","plugins":[{"name":"web","source":"./web"}]}"#,
    );
    // Plugin "web" with two skills.
    write(
        &tmp.join("web/.synaps-plugin/plugin.json"),
        r#"{"name":"web"}"#,
    );
    write(
        &tmp.join("web/skills/search/SKILL.md"),
        "---\nname: search\ndescription: Web search\n---\nBody",
    );
    // Collides with built-in "clear".
    write(
        &tmp.join("web/skills/clear/SKILL.md"),
        "---\nname: clear\ndescription: unrelated\n---\nBody",
    );
    // Loose skill.
    write(
        &tmp.join("skills/unique/SKILL.md"),
        "---\nname: unique\ndescription: Loose\n---\nBody",
    );

    let (plugins, skills) = loader::load_all(std::slice::from_ref(&tmp));
    assert_eq!(plugins.len(), 1);
    assert_eq!(skills.len(), 3);

    let registry = CommandRegistry::new(BUILTIN_COMMANDS, skills);

    // Built-in wins over the skill named "clear"...
    assert!(matches!(registry.resolve("clear"), Resolution::Builtin));
    // ...but qualified form reaches the skill.
    assert!(matches!(registry.resolve("web:clear"), Resolution::Skill(_)));
    // Unique skill resolves.
    assert!(matches!(registry.resolve("search"), Resolution::Skill(_)));
    // Loose skill resolves.
    assert!(matches!(registry.resolve("unique"), Resolution::Skill(_)));

    // Disable filter removes the plugin-qualified skill.
    let (_p, skills) = loader::load_all(std::slice::from_ref(&tmp));
    let filtered = filter_disabled(skills, &[], &["web:search".to_string()]);
    let registry = CommandRegistry::new(BUILTIN_COMMANDS, filtered);
    assert!(matches!(registry.resolve("search"), Resolution::Unknown));
    assert!(matches!(registry.resolve("unique"), Resolution::Skill(_)));

    fs::remove_dir_all(&tmp).ok();
}
