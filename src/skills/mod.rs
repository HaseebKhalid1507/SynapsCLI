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
