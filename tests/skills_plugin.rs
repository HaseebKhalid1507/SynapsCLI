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

#[test]
fn end_to_end_nested_marketplace_discovery() {
    // Real-world install layout: a marketplace repo cloned into the discovery
    // root as a subdirectory (e.g. ~/.synaps-cli/plugins/pi-skills/...).
    let tmp = std::env::temp_dir().join(format!(
        "synaps-int-nested-{}-{}",
        std::process::id(),
        synaps_cli::epoch_millis()
    ));
    fs::create_dir_all(&tmp).unwrap();

    // Nested marketplace under {tmp}/pkg/.synaps-plugin/marketplace.json
    write(
        &tmp.join("pkg/.synaps-plugin/marketplace.json"),
        r#"{"name":"nested","plugins":[{"name":"web","source":"./web"}]}"#,
    );
    write(
        &tmp.join("pkg/web/.synaps-plugin/plugin.json"),
        r#"{"name":"web"}"#,
    );
    write(
        &tmp.join("pkg/web/skills/search/SKILL.md"),
        "---\nname: search\ndescription: Web search\n---\nBody",
    );

    let (plugins, skills) = loader::load_all(std::slice::from_ref(&tmp));
    assert_eq!(plugins.len(), 1, "nested marketplace should yield 1 plugin");
    assert_eq!(plugins[0].name, "web");
    assert_eq!(plugins[0].marketplace.as_deref(), Some("nested"));
    assert_eq!(skills.len(), 1, "nested marketplace should yield 1 skill");
    assert_eq!(skills[0].name, "search");
    assert_eq!(skills[0].plugin.as_deref(), Some("web"));

    fs::remove_dir_all(&tmp).ok();
}

#[tokio::test]
async fn reload_picks_up_new_skill() {
    use synaps_cli::{ToolRegistry, SynapsConfig};
    use synaps_cli::skills::{register, reload_registry};
    use std::sync::Arc;
    use tokio::sync::RwLock;

    let dir = tempfile::tempdir().unwrap();
    let plugins_root = dir.path().join(".synaps-cli").join("plugins");
    std::fs::create_dir_all(&plugins_root).unwrap();

    // Save HOME so we can restore it at the end (avoid leaking into other tests).
    let prev_home = std::env::var_os("HOME");
    // Point HOME so default_roots() picks up our dir.
    std::env::set_var("HOME", dir.path());

    let tools = Arc::new(RwLock::new(ToolRegistry::new()));
    let config = SynapsConfig::default();
    let registry = register(&tools, &config).await;

    // No skill yet.
    assert!(matches!(registry.resolve("fresh"), synaps_cli::skills::registry::Resolution::Unknown));

    // Drop a new skill on disk.
    let skill_dir = plugins_root.join("freshplug").join("skills").join("fresh");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::create_dir_all(plugins_root.join("freshplug").join(".synaps-plugin")).unwrap();
    std::fs::write(
        plugins_root.join("freshplug").join(".synaps-plugin").join("plugin.json"),
        r#"{"name":"freshplug"}"#,
    ).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"),
        "---\nname: fresh\ndescription: d\n---\nbody").unwrap();

    reload_registry(&registry, &config);

    assert!(matches!(registry.resolve("fresh"), synaps_cli::skills::registry::Resolution::Skill(_)));

    // Restore HOME so we don't leak into other tests in this binary.
    match prev_home {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
}
