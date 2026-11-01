//! Skills and plugins subsystem.
//!
//! Discovers plugins under `.synaps-cli/plugins/` (project-local),
//! `skills.extra_root` (one configured path) and `~/.synaps-cli/plugins/`
//! (global), registers each skill as a dynamic
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
pub mod status;
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

/// A skill discovered during loading. Boot discovery holds ONLY bounded
/// metadata — the body stays on disk behind an immutable fingerprint and
/// is read, verified, substituted, and returned exclusively by
/// [`LoadedSkill::load_body`] at exact selection time.
#[derive(Clone)]
pub struct LoadedSkill {
    pub name: String,
    pub description: String,
    /// Body source — NEVER the body itself at boot. Private: all access
    /// flows through [`Self::load_body`].
    pub(crate) source: SkillSource,
    pub plugin: Option<String>, // None for loose skills
    pub base_dir: PathBuf,      // absolute
    pub source_path: PathBuf,   // absolute path to SKILL.md
}

/// Redacted Debug (Task 21 security review): bounded identity only — no
/// source paths, no base dir, no body, no fingerprint material — so a
/// stray debug/log rendering of boot metadata can never leak filesystem
/// layout or content.
impl std::fmt::Debug for LoadedSkill {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedSkill")
            .field("name", &agent_core::BoundedText::new(&self.name, 128).text)
            .field(
                "plugin",
                &self
                    .plugin
                    .as_deref()
                    .map(|p| agent_core::BoundedText::new(p, 128).text),
            )
            .field("description_len", &self.description.len())
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

/// Where a skill body comes from. Debug is redacted: no body content, no
/// fingerprint material.
#[derive(Clone)]
pub(crate) enum SkillSource {
    /// Body held in memory (test constructors; no verification needed).
    Inline(String),
    /// Lazy on-disk body: re-read + fingerprint-verified at selection.
    Lazy {
        fingerprint: loader::SkillFingerprint,
        /// Plugin root for ${CLAUDE_PLUGIN_ROOT} substitution.
        plugin_root: Option<PathBuf>,
    },
}

impl std::fmt::Debug for SkillSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Inline(_) => f.debug_struct("Inline").finish_non_exhaustive(),
            Self::Lazy { .. } => f.debug_struct("Lazy").finish_non_exhaustive(),
        }
    }
}

impl LoadedSkill {
    /// Test/compat constructor with an in-memory body.
    pub fn new_inline(
        name: &str,
        description: &str,
        body: &str,
        plugin: Option<&str>,
        base_dir: PathBuf,
        source_path: PathBuf,
    ) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            source: SkillSource::Inline(body.to_string()),
            plugin: plugin.map(str::to_string),
            base_dir,
            source_path,
        }
    }
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
    "close",
    "settings",
    "plugins",
    "extensions",
    "status",
    "stats",
    "context",
    "budget",
    "workers",
    "confirms",
    "keepwarm",
    // The daemon-session pin; `keep-warm` is its older name, kept as an
    // alias. Registered so `/keep-warm` never fuzzy-resolves to `/keepwarm`
    // (the prompt-cache pings, which cost money).
    "pin",
    "keep-warm",
    "clients",
    // Take input from the daemon client that owns it (`--takeover`, in place).
    "takeover",
    "trace",
    "memory",
    "auth",
    "ping",
    "keybinds",
    "sidecar",
    "watch",
];

/// One filtered discovery pass over [`loader::discovery_roots`].
pub struct Discovered {
    /// The pass, with the disable filters applied to its `plugins` and
    /// `skills` (its per-root counts are from before them).
    pub pass: loader::DiscoveryPass,
    /// Skills the disable filters removed.
    pub disabled: usize,
}

/// Walk the five discovery roots for `keys` and apply its disable lists to
/// the skills and to the plugins (commands, keybinds, help).
pub fn discover(keys: &agent_core::config::SkillsKeys) -> Discovered {
    let roots = loader::discovery_roots(keys.extra_root.as_deref());
    let mut pass = loader::load_all_reported(&roots);
    let found = pass.skills.len();
    pass.skills = config::filter_disabled(
        std::mem::take(&mut pass.skills),
        &keys.disabled_plugins,
        &keys.disabled_skills,
    );
    let disabled = found - pass.skills.len();
    if !keys.disabled_plugins.is_empty() {
        pass.plugins
            .retain(|p| !keys.disabled_plugins.iter().any(|d| d == &p.name));
    }
    Discovered { pass, disabled }
}

/// What [`register_reported`] built.
pub struct Registered {
    pub commands: Arc<CommandRegistry>,
    pub keybinds: Arc<std::sync::RwLock<keybinds::KeybindRegistry>>,
    /// The boot pass's report (generation 1), for `skills.status` and
    /// `skills.reload`.
    pub state: status::SkillsState,
}

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
    let Registered {
        commands, keybinds, ..
    } = register_reported(tools, config).await;
    (commands, keybinds)
}

/// [`register`], keeping the boot pass's report and the reload state (the
/// daemon host's `skills.status` / `skills.reload`).
pub async fn register_reported(
    tools: &Arc<tokio::sync::RwLock<crate::ToolRegistry>>,
    config: &crate::SynapsConfig,
) -> Registered {
    // The fs-walk (read_dir + read_to_string + canonicalize across multiple roots)
    // is fully synchronous std::fs; do it on a blocking pool so we don't park a
    // tokio worker during boot. Behavior is identical — same inputs, same output.
    let keys = config.skills_keys();
    let found = tokio::task::spawn_blocking(move || discover(&keys))
        .await
        .expect("skills::loader::load_all panicked");
    let state = status::SkillsState::new(status::SkillsReport::of(1, &found));
    let loader::DiscoveryPass {
        plugins, skills, ..
    } = found.pass;
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
    Registered {
        commands: registry,
        keybinds: Arc::new(std::sync::RwLock::new(kb_registry)),
        state,
    }
}

/// Re-walks discovery roots and swaps in the new skill set atomically.
/// Built-ins and the existing `load_skill` tool registration are unchanged.
pub fn reload_registry(registry: &CommandRegistry, config: &crate::SynapsConfig) {
    let loader::DiscoveryPass {
        plugins, skills, ..
    } = discover(&config.skills_keys()).pass;
    tracing::info!(skills = skills.len(), "reloaded skills");
    registry.rebuild_with_plugins(skills, plugins);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(dir: &std::path::Path, name: &str) {
        std::fs::create_dir_all(dir.join(name)).unwrap();
        std::fs::write(
            dir.join(name).join("SKILL.md"),
            format!("---\nname: {name}\ndescription: d\n---\nBody"),
        )
        .unwrap();
    }

    /// Pin 3 (skills v1): `disabled_plugins` (and `disabled_skills`) apply
    /// to the plugins and skills of `skills.extra_root` like any other root.
    #[test]
    #[serial_test::serial(synaps_base_dir)]
    fn disable_lists_apply_to_the_extra_root() {
        let _base = crate::test_env::BaseDirGuard::new();
        let extra = tempfile::TempDir::new().unwrap();
        let plugin = extra.path().join("pria-skills");
        std::fs::create_dir_all(plugin.join(".synaps-plugin")).unwrap();
        std::fs::write(
            plugin.join(".synaps-plugin").join("plugin.json"),
            r#"{"name":"pria-skills"}"#,
        )
        .unwrap();
        write_skill(&plugin.join("skills"), "a");
        write_skill(&plugin.join("skills"), "b");
        write_skill(extra.path(), "loose");
        write_skill(extra.path(), "other");
        let mut keys = agent_core::config::SkillsKeys {
            extra_root: Some(extra.path().to_path_buf()),
            ..Default::default()
        };
        let all = discover(&keys);
        assert_eq!(all.pass.skills.len(), 4);
        assert_eq!(all.disabled, 0);
        assert_eq!(all.pass.plugins.len(), 1);

        keys.disabled_plugins = vec!["pria-skills".into()];
        keys.disabled_skills = vec!["other".into()];
        let found = discover(&keys);
        let names: Vec<&str> = found.pass.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["loose"]);
        assert!(
            found.pass.plugins.is_empty(),
            "the disabled plugin is gone too"
        );
        assert_eq!(found.disabled, 3);
        let extra_root = &found.pass.roots[2];
        assert_eq!(extra_root.kind, loader::RootKind::Extra);
        assert_eq!(
            (extra_root.plugins, extra_root.skills),
            (1, 4),
            "counted before the filters"
        );
    }
}
