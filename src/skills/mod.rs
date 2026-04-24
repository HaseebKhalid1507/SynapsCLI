//! Skills and plugins subsystem.
//!
//! Discovers plugins under `.synaps-cli/plugins/` (project-local) and
//! `~/.synaps-cli/plugins/` (global), registers each skill as a dynamic
//! slash command, and exposes the same skills to the model via the
//! `load_skill` tool. Submodules: `manifest` (plugin/marketplace JSON
//! parsing), `loader` (discovery walk + frontmatter parsing), `config`
//! (disable-list filtering), `registry` (command registry with collision
//! handling), `tool` (the `load_skill` tool implementation).

pub mod manifest;
pub mod loader;
pub mod config;
pub mod registry;
pub mod tool;
pub mod state;
pub mod marketplace;
pub mod install;
pub mod keybinds;

use std::path::PathBuf;
use std::sync::Arc;

use crate::skills::registry::CommandRegistry;
use crate::skills::tool::LoadSkillTool;

/// A plugin discovered during skill loading.
#[derive(Debug, Clone)]
pub struct Plugin {
    pub name: String,
    pub root: PathBuf,
    pub marketplace: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub manifest: Option<manifest::PluginManifest>,
}

/// A skill discovered during loading.
#[derive(Debug, Clone)]
pub struct LoadedSkill {
    pub name: String,
    pub description: String,
    pub body: String,           // post-{baseDir} substitution
    pub plugin: Option<String>, // None for loose skills
    pub base_dir: PathBuf,      // absolute
    pub source_path: PathBuf,   // absolute path to SKILL.md
}

/// Built-in command names. Keep in sync with the match in
/// `src/chatui/commands.rs::handle_command`.
pub const BUILTIN_COMMANDS: &[&str] = &[
    "clear", "compact", "chain", "model", "system", "thinking", "sessions",
    "resume", "saveas", "theme", "gamba", "help", "quit", "exit",
    "settings", "plugins", "status", "ping", "keybinds",
];

/// Load all skills, apply disable filters, build the command registry,
/// build the keybind registry, and register the `load_skill` tool.
/// Returns (command_registry, keybind_registry).
pub async fn register(
    tools: &Arc<tokio::sync::RwLock<crate::ToolRegistry>>,
    config: &crate::SynapsConfig,
) -> (Arc<CommandRegistry>, Arc<keybinds::KeybindRegistry>) {
    let (plugins, mut skills) = loader::load_all(&loader::default_roots());
    skills = config::filter_disabled(skills, &config.disabled_plugins, &config.disabled_skills);

    tracing::info!(
        plugins = plugins.len(),
        skills = skills.len(),
        "loaded plugins and skills"
    );

    // Build keybind registry from plugin manifests
    let mut kb_registry = keybinds::KeybindRegistry::new();
    for plugin in &plugins {
        if let Some(ref manifest) = plugin.manifest {
            if !manifest.keybinds.is_empty() {
                kb_registry.register_plugin(&manifest.name, &manifest.keybinds, &plugin.root);
                tracing::info!(
                    plugin = manifest.name.as_str(),
                    count = manifest.keybinds.len(),
                    "registered plugin keybinds"
                );
            }
        }
    }

    // Apply user keybind overrides from config
    if !config.keybinds.is_empty() {
        kb_registry.register_user(&config.keybinds);
    }

    let registry = Arc::new(CommandRegistry::new(BUILTIN_COMMANDS, skills));
    let tool = LoadSkillTool::new(registry.clone());
    tools.write().await.register(Arc::new(tool));
    (registry, Arc::new(kb_registry))
}

/// Re-walks discovery roots and swaps in the new skill set atomically.
/// Built-ins and the existing `load_skill` tool registration are unchanged.
pub fn reload_registry(registry: &CommandRegistry, config: &crate::SynapsConfig) {
    let (_plugins, mut skills) = loader::load_all(&loader::default_roots());
    skills = config::filter_disabled(skills, &config.disabled_plugins, &config.disabled_skills);
    tracing::info!(skills = skills.len(), "reloaded skills");
    registry.rebuild_with(skills);
}
