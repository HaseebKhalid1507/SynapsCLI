//! Skills and plugins subsystem.
//!
//! Legacy flat-.md loader currently lives in `legacy`; new plugin-based
//! submodules will be built in `manifest`, `loader`, `config`, `registry`,
//! `tool` and eventually supersede it.

mod legacy;
pub mod manifest;
pub mod loader;
pub mod config;
pub mod registry;
pub mod tool;

// Re-export legacy API so existing callers (chatui/main.rs) keep compiling.
pub use legacy::{Skill, load_skills, format_skills_for_prompt, parse_skills_config, setup_skill_tool};

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
}

/// A skill discovered during loading. Renamed `LoadedSkill` temporarily
/// to avoid clashing with the re-exported `legacy::Skill`; the legacy
/// alias will be removed in the final migration task.
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
    "clear", "model", "system", "thinking", "sessions",
    "resume", "theme", "gamba", "help", "quit", "exit",
];

/// Load all skills, apply disable filters, build the command registry,
/// and register the `load_skill` tool. Returns the registry for chatui wiring.
pub async fn register(
    tools: &Arc<tokio::sync::RwLock<crate::ToolRegistry>>,
    config: &crate::SynapsConfig,
) -> Arc<CommandRegistry> {
    let (plugins, mut skills) = loader::load_all(&loader::default_roots());
    skills = config::filter_disabled(skills, &config.disabled_plugins, &config.disabled_skills);

    tracing::info!(
        plugins = plugins.len(),
        skills = skills.len(),
        "loaded plugins and skills"
    );

    let registry = Arc::new(CommandRegistry::new(BUILTIN_COMMANDS, skills));
    let tool = LoadSkillTool::new(registry.clone());
    tools.write().await.register(Arc::new(tool));
    registry
}
