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
