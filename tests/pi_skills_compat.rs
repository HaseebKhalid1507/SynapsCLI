//! Manual/optional compat check — run with:
//!   HOME=/tmp/synaps-pi-home cargo test --test pi_skills_compat -- --ignored --nocapture
//! Verifies the pi-skills repo (with `.claude-plugin` renamed to `.synaps-plugin`)
//! is discovered by the current loader.

use synaps_cli::skills::{loader, registry::{CommandRegistry, Resolution}, BUILTIN_COMMANDS};

#[test]
#[ignore]
fn pi_skills_discovery() {
    let roots = loader::default_roots();
    let (plugins, skills) = loader::load_all(&roots);

    println!("\n── pi-skills compat check ──");
    println!("roots: {:?}", roots);
    println!("plugins: {}", plugins.len());
    for p in &plugins {
        println!("  {} @ {}", p.name, p.root.display());
    }
    println!("skills: {}", skills.len());
    for s in &skills {
        println!(
            "  {}{} — {}",
            s.plugin.as_deref().map(|p| format!("{}:", p)).unwrap_or_default(),
            s.name,
            s.description
        );
    }

    assert!(!plugins.is_empty(), "expected at least one plugin discovered");
    assert!(!skills.is_empty(), "expected at least one skill discovered");

    let registry = CommandRegistry::new(BUILTIN_COMMANDS, skills);
    let expected = ["exa-search", "browser-tools", "youtube", "transcribe", "workflow"];
    for name in expected {
        match registry.resolve(name) {
            Resolution::Skill(s) => println!("  resolve /{} → {:?}:{}", name, s.plugin, s.name),
            Resolution::PluginCommand(c) => println!("  resolve /{} → PluginCommand {:?}:{}", name, c.plugin, c.name),
            Resolution::Builtin => println!("  resolve /{} → Builtin (collision)", name),
            Resolution::Ambiguous(v) => println!("  resolve /{} → Ambiguous {:?}", name, v),
            Resolution::Unknown => println!("  resolve /{} → Unknown (MISSING)", name),
        }
    }
}
