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
pub mod plugin_index;
pub mod update_diff;
pub mod install;
pub mod keybinds;
pub mod commands;
pub mod trust;
pub mod post_install;

use std::path::PathBuf;
use std::sync::Arc;

use crate::skills::registry::CommandRegistry;
use crate::skills::tool::LoadSkillTool;
use crate::extensions::manifest::ExtensionManifest;

/// A plugin discovered during skill loading.
#[derive(Debug, Clone)]
pub struct Plugin {
    pub name: String,
    pub root: PathBuf,
    pub marketplace: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub extension: Option<ExtensionManifest>,
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
    "clear", "compact", "chain", "model", "models", "system", "thinking", "sessions",
    "resume", "saveas", "theme", "gamba", "help", "quit", "exit",
    "settings", "plugins", "extensions", "status", "ping", "keybinds",
    "sidecar",
    // `voice` retained for one-release deprecation aliasing in
    // chatui::commands::handle_command. Will be removed once users
    // have migrated to `/sidecar` for toggle/status.
    "voice",
];

/// Load all skills, apply disable filters, build the command registry,
/// build the keybind registry, and register the `load_skill` tool.
/// Returns (command_registry, keybind_registry).
pub async fn register(
    tools: &Arc<tokio::sync::RwLock<crate::ToolRegistry>>,
    config: &crate::SynapsConfig,
) -> (Arc<CommandRegistry>, Arc<std::sync::RwLock<keybinds::KeybindRegistry>>) {
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

    // Synthesize the sidecar toggle keybind. The selected key in
    // `sidecar_toggle_key` is the *only* active sidecar toggle binding —
    // there's no plugin-level F8 anymore, so picking a value in
    // /settings → Sidecar fully replaces the previous chord. Defaults to
    // F8 when no value has been chosen.
    //
    // Back-compat: legacy `voice_toggle_key` is read if the new key is
    // absent. (Phase 7 deferred slice: replace `/voice toggle` literal
    // with the active sidecar plugin's declared toggle command.)
    let sidecar_key = crate::config::read_config_value("sidecar_toggle_key")
        .or_else(|| crate::config::read_config_value("voice_toggle_key"))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "F8".to_string());
    let mut overrides = std::collections::HashMap::new();
    overrides.insert(sidecar_key, "/sidecar toggle".to_string());
    kb_registry.register_user(&overrides);

    let registry = Arc::new(CommandRegistry::new_with_plugins(BUILTIN_COMMANDS, skills, plugins));
    let tool = LoadSkillTool::new(registry.clone());
    tools.write().await.register(Arc::new(tool));
    (registry, Arc::new(std::sync::RwLock::new(kb_registry)))
}

/// Re-walks discovery roots and swaps in the new skill set atomically.
/// Built-ins and the existing `load_skill` tool registration are unchanged.
pub fn reload_registry(registry: &CommandRegistry, config: &crate::SynapsConfig) {
    let (plugins, mut skills) = loader::load_all(&loader::default_roots());
    skills = config::filter_disabled(skills, &config.disabled_plugins, &config.disabled_skills);
    tracing::info!(skills = skills.len(), "reloaded skills");
    registry.rebuild_with_plugins(skills, plugins);
}
