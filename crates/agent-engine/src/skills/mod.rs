//! Skills and plugins subsystem.
//!
//! Discovers plugins under `.synaps-cli/plugins/` (project-local) and
//! `~/.synaps-cli/plugins/` (global), registers each skill as a dynamic
//! slash command, and exposes the same skills to the model via the
//! `load_skill` tool. Submodules: `manifest` (plugin/marketplace JSON
//! parsing), `loader` (discovery walk + frontmatter parsing), `config`
//! (disable-list filtering), `registry` (command registry with collision
//! handling), `tool` (the `load_skill` tool implementation).

pub mod commands;
pub mod config;
pub mod install;
pub mod keybinds;
pub mod loader;
pub mod manifest;
pub mod marketplace;
pub mod plugin_index;
pub mod post_install;
pub mod registry;
pub mod state;
pub mod tool;
pub mod trust;
pub mod update_diff;

use std::path::PathBuf;
use std::sync::Arc;

use crate::extensions::manifest::ExtensionManifest;
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

// ── Stable skill identities (Task 17, spec §7.2/§7.6 boundary) ──────────────

/// Byte budget for one encoded skill-id segment.
const SKILL_ID_SEGMENT_MAX_BYTES: usize = 64;
/// Reserved marker for hex-encoded non-canonical segments.
const SKILL_ID_ENCODED_PREFIX: &str = "enc-";
/// Reserved marker for digest-compressed oversized segments.
const SKILL_ID_DIGEST_PREFIX: &str = "sha-";
/// Hex characters kept from the SHA-256 of an oversized segment (160 bits).
const SKILL_ID_DIGEST_HEX_LEN: usize = 40;

fn skill_segment_is_canonical(segment: &str) -> bool {
    if segment.is_empty() || segment.len() > SKILL_ID_SEGMENT_MAX_BYTES {
        return false;
    }
    let mut chars = segment.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    first_ok
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.'))
}

/// Deterministic, injective, bounded encoding of one raw identity segment
/// (mirrors the `ToolId` segment strategy): canonical lowercase segments
/// pass through verbatim; anything else becomes `enc-<hex>`; oversized
/// encodings compress to `sha-<truncated sha256>`. Two distinct raw
/// spellings can never collapse into one encoded segment.
fn encode_skill_segment(raw: &str) -> String {
    use sha2::{Digest as _, Sha256};
    if skill_segment_is_canonical(raw)
        && !raw.starts_with(SKILL_ID_ENCODED_PREFIX)
        && !raw.starts_with(SKILL_ID_DIGEST_PREFIX)
    {
        return raw.to_string();
    }
    let mut hex = String::with_capacity(raw.len() * 2);
    for byte in raw.as_bytes() {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    let encoded = format!("{SKILL_ID_ENCODED_PREFIX}{hex}");
    if encoded.len() <= SKILL_ID_SEGMENT_MAX_BYTES {
        return encoded;
    }
    let digest = Sha256::digest(raw.as_bytes());
    let mut digest_hex = String::with_capacity(SKILL_ID_DIGEST_HEX_LEN);
    for byte in digest.iter().take(SKILL_ID_DIGEST_HEX_LEN / 2) {
        use std::fmt::Write as _;
        let _ = write!(digest_hex, "{byte:02x}");
    }
    format!("{SKILL_ID_DIGEST_PREFIX}{digest_hex}")
}

/// Stable model-facing identity of one skill (Task 17): deterministic and
/// alias-safe per exact (plugin, name) pair, bounded, and free of source
/// paths. Plugin skills use the `skill.<plugin>:<name>` namespace; loose
/// skills use `skill:<name>` — the namespaces cannot collide, so a loose
/// skill spelling a qualified name can never impersonate a plugin skill.
pub fn stable_skill_id(skill: &LoadedSkill) -> String {
    match &skill.plugin {
        Some(plugin) => format!(
            "skill.{}:{}",
            encode_skill_segment(plugin),
            encode_skill_segment(&skill.name)
        ),
        None => format!("skill:{}", encode_skill_segment(&skill.name)),
    }
}

/// True when a `load_skill` input is spelled in the stable skill-id
/// namespace (and should therefore resolve by exact id, not legacy names).
pub(crate) fn looks_like_stable_skill_id(raw: &str) -> bool {
    raw.strip_prefix("skill")
        .is_some_and(|rest| rest.starts_with(':') || rest.starts_with('.'))
}

/// Built-in command names. Keep in sync with the match in
/// `src/chatui/commands.rs::handle_command`.
pub const BUILTIN_COMMANDS: &[&str] = &[
    "clear",
    "compact",
    "chain",
    "model",
    "models",
    "system",
    "thinking",
    "effort",
    "sessions",
    "resume",
    "saveas",
    "theme",
    "gamba",
    "help",
    "quit",
    "exit",
    "settings",
    "plugins",
    "extensions",
    "status",
    "stats",
    "context",
    "trace",
    "ping",
    "keybinds",
    "sidecar",
];

/// Load all skills, apply disable filters, build the command registry,
/// build the keybind registry, and register the `load_skill` tool.
/// Returns (command_registry, keybind_registry).
pub async fn register(
    tools: &Arc<tokio::sync::RwLock<crate::ToolRegistry>>,
    config: &crate::SynapsConfig,
) -> (
    Arc<CommandRegistry>,
    Arc<std::sync::RwLock<keybinds::KeybindRegistry>>,
) {
    // The fs-walk (read_dir + read_to_string + canonicalize across multiple roots)
    // is fully synchronous std::fs; do it on a blocking pool so we don't park a
    // tokio worker during boot. Behavior is identical — same inputs, same output.
    let (mut plugins, mut skills) =
        tokio::task::spawn_blocking(|| loader::load_all(&loader::default_roots()))
            .await
            .expect("skills::loader::load_all panicked");
    skills = config::filter_disabled(skills, &config.disabled_plugins, &config.disabled_skills);

    // Filter disabled plugins from commands, keybinds, and help too — not just skills
    if !config.disabled_plugins.is_empty() {
        plugins.retain(|p| !config.disabled_plugins.iter().any(|d| d == &p.name));
    }

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
    let sidecar_key = crate::config::read_config_value("sidecar_toggle_key")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "F8".to_string());
    let mut overrides = std::collections::HashMap::new();
    overrides.insert(sidecar_key, "/sidecar toggle".to_string());
    kb_registry.register_user(&overrides);

    let registry = Arc::new(CommandRegistry::new_with_plugins(
        BUILTIN_COMMANDS,
        skills,
        plugins,
    ));
    let tool = LoadSkillTool::new(registry.clone());
    {
        let mut tools = tools.write().await;
        tools.register(Arc::new(tool));
        // Task 17: bounded skill discovery beside load_skill (stable IDs +
        // compact descriptions only; no bodies, no source paths).
        tools.register(Arc::new(tool::SearchSkillsTool::new(registry.clone())));
    }
    (registry, Arc::new(std::sync::RwLock::new(kb_registry)))
}

/// Re-walks discovery roots and swaps in the new skill set atomically.
/// Built-ins and the existing `load_skill` tool registration are unchanged.
pub fn reload_registry(registry: &CommandRegistry, config: &crate::SynapsConfig) {
    let (mut plugins, mut skills) = loader::load_all(&loader::default_roots());
    skills = config::filter_disabled(skills, &config.disabled_plugins, &config.disabled_skills);
    if !config.disabled_plugins.is_empty() {
        plugins.retain(|p| !config.disabled_plugins.iter().any(|d| d == &p.name));
    }
    tracing::info!(skills = skills.len(), "reloaded skills");
    registry.rebuild_with_plugins(skills, plugins);
}
