//! Parse .synaps-plugin/plugin.json and .synaps-plugin/marketplace.json.

use serde::Deserialize;

use super::keybinds::ManifestKeybind;
use super::plugin_index::PluginIndexEntry;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PluginCompatibility {
    #[serde(default)]
    pub synaps: Option<String>,
    #[serde(default)]
    pub extension_protocol: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum ManifestCommand {
    Shell(ManifestShellCommand),
    ExtensionTool(ManifestExtensionToolCommand),
    SkillPrompt(ManifestSkillPromptCommand),
    Interactive(ManifestInteractiveCommand),
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ManifestShellCommand {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ManifestExtensionToolCommand {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub tool: String,
    #[serde(default)]
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ManifestSkillPromptCommand {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub skill: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ManifestInteractiveCommand {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Route this slash command to the plugin extension's `command.invoke` RPC.
    pub interactive: bool,
    #[serde(default)]
    pub subcommands: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub keybinds: Vec<ManifestKeybind>,
    #[serde(default)]
    pub compatibility: Option<PluginCompatibility>,
    #[serde(default)]
    pub commands: Vec<ManifestCommand>,
    #[serde(default)]
    pub extension: Option<crate::extensions::manifest::ExtensionManifest>,
    #[serde(default, alias = "help")]
    pub help_entries: Vec<crate::help::HelpEntry>,
    #[serde(default)]
    pub provides: Option<PluginProvides>,
    /// Plugin-declared Settings categories (Path B Phase 4). Each plugin
    /// may contribute one or more categories to the `/settings` modal,
    /// each with declarative `text`/`cycler`/`picker` fields or a
    /// plugin-rendered `custom` editor (JSON-RPC `settings.editor.*`).
    #[serde(default)]
    pub settings: Option<ManifestSettings>,
}

/// Container for plugin-declared settings categories.
///
/// JSON shape:
/// ```jsonc
/// "settings": {
///   "category": [
///     { "id": "voice", "label": "Voice", "fields": [ ... ] }
///   ]
/// }
/// ```
/// The TOML equivalent (`[[settings.category]]`) deserializes through the
/// `category` alias. The plural Rust field name is preferred internally.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct ManifestSettings {
    #[serde(default, alias = "category")]
    pub categories: Vec<ManifestSettingsCategory>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ManifestSettingsCategory {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub fields: Vec<ManifestSettingsField>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ManifestSettingsField {
    pub key: String,
    pub label: String,
    pub editor: ManifestEditorKind,
    /// Discrete options for `cycler` editors. Ignored otherwise.
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub help: Option<String>,
    /// Optional default value seeded into the plugin's config namespace
    /// when the field is first read. Type-erased JSON; consumer decides
    /// how to interpret based on `editor`.
    #[serde(default)]
    pub default: Option<serde_json::Value>,
    /// `true` for fields whose editor is `text` and accepts only numeric
    /// input. Mirrors `EditorKind::Text { numeric }` in the core schema.
    #[serde(default)]
    pub numeric: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ManifestEditorKind {
    /// Free-text input (optionally numeric — see `numeric`).
    Text,
    /// Discrete-option cycler — uses `options`.
    Cycler,
    /// Generic picker. Options are supplied by the plugin at editor-open
    /// time via the `settings.editor.*` JSON-RPC contract.
    Picker,
    /// Plugin-rendered overlay using `settings.editor.open` /
    /// `settings.editor.render` / `settings.editor.key` /
    /// `settings.editor.commit`. See
    /// `src/extensions/settings_editor.rs` for the typed payloads.
    Custom,
}

/// Plugin-provided capabilities consumed by Synaps CLI core.
///
/// Currently only one slot is recognised: `sidecar`. A plugin
/// advertises a long-running sidecar binary by setting
/// `provides.sidecar.command`; the integration layer in
/// `src/sidecar/` discovers and supervises it.
///
/// ## Wire compatibility
///
/// Older plugin manifests use the field name `voice_sidecar`. That
/// spelling is still accepted via a serde alias for one release so
/// existing plugins keep working unchanged. New plugins should use
/// `sidecar`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct PluginProvides {
    #[serde(default, alias = "voice_sidecar")]
    pub sidecar: Option<SidecarManifest>,
}

/// Sidecar binary that Synaps CLI launches as a long-running plugin
/// process. Modality-agnostic: a plugin can use this for voice STT,
/// OCR, agent runners, EEG dictation, or anything that fits the
/// "trigger-driven streaming source" shape.
///
/// `command` is resolved relative to the plugin root unless absolute.
/// `protocol_version` is matched against the line-JSON protocol version
/// understood by `src/sidecar/protocol.rs` (currently `1`).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SidecarManifest {
    pub command: String,
    #[serde(default)]
    pub setup: Option<String>,
    #[serde(default = "default_sidecar_protocol_version")]
    pub protocol_version: u16,
    #[serde(default)]
    pub model: Option<SidecarModel>,
    /// Optional plugin-claimed lifecycle UX. When set, core
    /// auto-registers `<command> toggle` and `<command> status` and
    /// uses `display_name` for the pill / status / errors. When
    /// unset, the plugin is reachable via the generic `/sidecar`
    /// fallback (ambiguity-aware: errors when 2+ unclaimed plugins
    /// are loaded).
    #[serde(default)]
    pub lifecycle: Option<SidecarLifecycle>,
}

/// Plugin-claimed lifecycle UX for a sidecar. See [`SidecarManifest::lifecycle`].
///
/// The plugin chooses how its lifecycle commands and settings appear
/// to the user. Core uses `display_name` for the pill, status line,
/// and error messages; auto-registers `<command> toggle/status` as
/// addressable slash commands; injects a virtual toggle-key field
/// into `settings_category` (when given).
///
/// `importance` controls pill ordering when multiple sidecars are
/// loaded simultaneously: higher = leftmost. Defaults to `0`. Cap at
/// `-100..=100`; values outside that range are clamped at parse time.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SidecarLifecycle {
    /// Slash-command name that owns this sidecar's lifecycle.
    /// Together with `toggle`/`status` subcommands forms e.g.
    /// `/voice toggle`.
    pub command: String,
    /// Settings category id (matches a `settings.categories[].id` in
    /// the plugin manifest) that should host the virtual toggle-key
    /// field. When `None`, no settings injection happens.
    #[serde(default)]
    pub settings_category: Option<String>,
    /// Display name shown in the pill, status line, and `/extensions`
    /// (e.g. "Voice", "OCR"). Defaults to `command` when `None`.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Pill-ordering hint (-100..=100, default 0). Higher = leftmost.
    #[serde(default, deserialize_with = "deserialize_clamped_importance")]
    pub importance: i32,
}

impl SidecarLifecycle {
    /// Resolved display name: `display_name` if set, else `command`.
    pub fn effective_display_name(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.command)
    }
}

/// Clamp `importance` to the documented range `-100..=100`.
fn deserialize_clamped_importance<'de, D>(d: D) -> Result<i32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = i32::deserialize(d)?;
    Ok(raw.clamp(-100, 100))
}

/// Backwards-compat type alias. New code should use [`SidecarManifest`].
#[deprecated(
    since = "0.1.0-phase7",
    note = "use SidecarManifest; the voice-prefixed alias will be removed in a future release"
)]
pub type VoiceSidecarManifest = SidecarManifest;

fn default_sidecar_protocol_version() -> u16 {
    1
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct SidecarModel {
    #[serde(default)]
    pub default_path: Option<String>,
    #[serde(default)]
    pub required_for_real_stt: bool,
}

/// Backwards-compat type alias. New code should use [`SidecarModel`].
#[deprecated(
    since = "0.1.0-phase7",
    note = "use SidecarModel; the voice-prefixed alias will be removed in a future release"
)]
pub type VoiceSidecarModel = SidecarModel;

#[derive(Debug, Clone, Deserialize)]
pub struct MarketplaceManifest {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub trust: Option<MarketplaceTrust>,
    pub plugins: Vec<MarketplacePluginEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarketplaceTrust {
    #[serde(default)]
    pub publisher: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarketplacePluginEntry {
    pub name: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub index: Option<PluginIndexEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sibling-repo manifest pin: the live `local-voice` plugin manifest
    /// in `synaps-skills` must round-trip through this crate's parser
    /// with the Phase 8 lifecycle block + keybinds wired in. If this
    /// test fails because the file moved, update or delete the path —
    /// don't loosen the assertions.
    #[test]
    fn local_voice_plugin_json_parses_with_phase8_lifecycle_and_keybinds() {
        let path = "/home/jr/Projects/Maha-Media/.worktrees/\
            synaps-skills-local-voice-plugin-commands-tasks/local-voice-plugin/\
            .synaps-plugin/plugin.json";
        let Ok(json) = std::fs::read_to_string(path) else {
            // Sibling worktree absent — skip rather than fail in CI/other
            // environments. The pin is best-effort local-dev guard.
            eprintln!("skip: {path} not found");
            return;
        };
        let m: PluginManifest =
            serde_json::from_str(&json).expect("local-voice manifest must deserialize");
        assert_eq!(m.name, "local-voice");

        let provides = m.provides.expect("provides present");
        let sidecar = provides.sidecar.expect("sidecar present");
        assert_eq!(sidecar.command, "bin/synaps-voice-plugin");
        let lc = sidecar.lifecycle.expect("lifecycle present");
        assert_eq!(lc.command, "voice");
        assert_eq!(lc.settings_category.as_deref(), Some("voice"));
        assert_eq!(lc.effective_display_name(), "Voice");
        assert_eq!(lc.importance, 50);

        assert_eq!(m.keybinds.len(), 1);
        let kb = &m.keybinds[0];
        assert_eq!(kb.key, "C-Space");
        assert_eq!(kb.action, "slash_command");
        assert_eq!(kb.command.as_deref(), Some("voice toggle"));
    }

    #[test]
    fn plugin_manifest_minimal() {
        let json = r#"{"name":"web-tools"}"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.name, "web-tools");
        assert_eq!(m.version, None);
        assert_eq!(m.description, None);
        assert!(m.commands.is_empty());
        assert!(m.help_entries.is_empty());
        assert!(m.compatibility.is_none());
    }

    #[test]
    fn plugin_manifest_full_with_extras() {
        let json = r#"{
            "name": "web-tools",
            "version": "1.0.0",
            "description": "Web tools",
            "author": {"name": "x"},
            "repository": "https://...",
            "license": "MIT",
            "compatibility": {
                "synaps": ">=0.1.0",
                "extension_protocol": "1"
            },
            "unknown_field": 42
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.name, "web-tools");
        assert_eq!(m.version.as_deref(), Some("1.0.0"));
        assert_eq!(m.description.as_deref(), Some("Web tools"));
        assert_eq!(m.compatibility.as_ref().unwrap().synaps.as_deref(), Some(">=0.1.0"));
        assert_eq!(m.compatibility.as_ref().unwrap().extension_protocol.as_deref(), Some("1"));
    }

    #[test]
    fn plugin_manifest_parses_help_entries_with_usage_examples() {
        let json = r#"{
            "name": "web-tools",
            "help_entries": [
                {
                    "id": "web-search-help",
                    "command": "/web:search",
                    "title": "Web Search",
                    "summary": "Search the web from a plugin.",
                    "category": "Plugin",
                    "topic": "Command",
                    "protected": false,
                    "common": false,
                    "keywords": ["web", "search"],
                    "usage": "/web:search <query>",
                    "examples": [
                        {
                            "command": "/web:search rust serde",
                            "description": "Search for Rust serde resources."
                        }
                    ]
                }
            ]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.help_entries.len(), 1);
        assert_eq!(m.help_entries[0].command, "/web:search");
        assert_eq!(m.help_entries[0].usage.as_deref(), Some("/web:search <query>"));
        assert_eq!(m.help_entries[0].examples[0].command, "/web:search rust serde");
    }

    #[test]
    fn plugin_manifest_accepts_help_alias_for_help_entries() {
        let json = r#"{
            "name": "web-tools",
            "help": [
                {
                    "id": "web-help",
                    "command": "/help web",
                    "title": "Web Tools",
                    "summary": "Use web tools from the plugin.",
                    "category": "Plugin",
                    "topic": "Branch",
                    "protected": false,
                    "common": false
                }
            ]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.help_entries.len(), 1);
        assert_eq!(m.help_entries[0].command, "/help web");
        assert_eq!(m.help_entries[0].topic, crate::help::HelpTopicKind::Branch);
    }

    #[test]
    fn plugin_manifest_can_add_command_and_matching_help_entries_together() {
        let json = r#"{
            "name": "dev-tools",
            "commands": [
                {
                    "name": "lint",
                    "description": "Run lint",
                    "command": "bash",
                    "args": ["scripts/lint.sh"]
                }
            ],
            "help_entries": [
                {
                    "id": "dev-lint-help",
                    "command": "/dev-tools:lint",
                    "title": "Lint",
                    "summary": "Run plugin lint checks.",
                    "category": "Plugin",
                    "topic": "Command",
                    "protected": false,
                    "common": false,
                    "usage": "/dev-tools:lint"
                }
            ]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.commands.len(), 1);
        assert_eq!(m.help_entries.len(), 1);
        assert_eq!(m.help_entries[0].command, "/dev-tools:lint");
    }

    #[test]
    fn plugin_manifest_help_entries_default_boilerplate_fields() {
        let json = r#"{
            "name": "dev-tools",
            "help": [
                {
                    "id": "dev-lint-help",
                    "command": "/dev-tools:lint",
                    "title": "Lint",
                    "summary": "Run plugin lint checks."
                }
            ]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.help_entries.len(), 1);
        assert_eq!(m.help_entries[0].category, "Plugin");
        assert_eq!(m.help_entries[0].topic, crate::help::HelpTopicKind::Command);
        assert!(!m.help_entries[0].protected);
        assert!(!m.help_entries[0].common);
    }

    #[test]
    fn plugin_manifest_parses_provides_voice_sidecar() {
        let json = r#"{
            "name": "local-voice",
            "provides": {
                "voice_sidecar": {
                    "command": "bin/synaps-voice-plugin",
                    "setup": "scripts/setup.sh",
                    "protocol_version": 1,
                    "model": {
                        "default_path": "~/.synaps-cli/models/whisper/ggml-base.en.bin",
                        "required_for_real_stt": true
                    }
                }
            }
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        let provides = m.provides.expect("provides should deserialize");
        let sidecar = provides.sidecar.expect("sidecar should deserialize via voice_sidecar alias");
        assert_eq!(sidecar.command, "bin/synaps-voice-plugin");
        assert_eq!(sidecar.setup.as_deref(), Some("scripts/setup.sh"));
        assert_eq!(sidecar.protocol_version, 1);
        let model = sidecar.model.expect("model should deserialize");
        assert_eq!(
            model.default_path.as_deref(),
            Some("~/.synaps-cli/models/whisper/ggml-base.en.bin")
        );
        assert!(model.required_for_real_stt);
    }

    #[test]
    fn plugin_manifest_parses_provides_sidecar_canonical() {
        // Phase 7 slice G: the canonical field name is `sidecar`. Older
        // plugins keep working via the `voice_sidecar` serde alias (above);
        // new plugins should write `sidecar` directly.
        let json = r#"{
            "name": "local-ocr",
            "provides": {
                "sidecar": {
                    "command": "bin/ocr-sidecar",
                    "protocol_version": 1
                }
            }
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        let provides = m.provides.expect("provides should deserialize");
        let sidecar = provides.sidecar.expect("canonical `sidecar` field should deserialize");
        assert_eq!(sidecar.command, "bin/ocr-sidecar");
        assert_eq!(sidecar.protocol_version, 1);
    }

    #[test]
    fn plugin_manifest_rejects_both_sidecar_fields_present() {
        // serde treats fields and their aliases as the same slot, so
        // declaring both `sidecar` and `voice_sidecar` is a duplicate-
        // field error. This is *safer* than last-wins because it
        // catches accidental double-declaration during the migration
        // window. Pinned to notice future serde-version regressions.
        let json = r#"{
            "name": "x",
            "provides": {
                "voice_sidecar": {"command": "old", "protocol_version": 1},
                "sidecar":       {"command": "new", "protocol_version": 1}
            }
        }"#;
        let err = serde_json::from_str::<PluginManifest>(json).unwrap_err();
        assert!(
            err.to_string().contains("duplicate field"),
            "expected duplicate-field error, got: {err}"
        );
    }

    // ---- Phase 8 slice 8A: sidecar lifecycle parsing ----------------------

    #[test]
    fn sidecar_lifecycle_parses_full_block() {
        let json = r#"{
            "name": "p",
            "provides": {
                "sidecar": {
                    "command": "bin/sidecar",
                    "protocol_version": 1,
                    "lifecycle": {
                        "command": "voice",
                        "settings_category": "voice",
                        "display_name": "Voice",
                        "importance": 50
                    }
                }
            }
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        let lc = m
            .provides
            .unwrap()
            .sidecar
            .unwrap()
            .lifecycle
            .expect("lifecycle should deserialize");
        assert_eq!(lc.command, "voice");
        assert_eq!(lc.settings_category.as_deref(), Some("voice"));
        assert_eq!(lc.display_name.as_deref(), Some("Voice"));
        assert_eq!(lc.importance, 50);
        assert_eq!(lc.effective_display_name(), "Voice");
    }

    #[test]
    fn sidecar_lifecycle_is_optional() {
        let json = r#"{
            "name": "p",
            "provides": {
                "sidecar": { "command": "bin/sidecar", "protocol_version": 1 }
            }
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert!(m.provides.unwrap().sidecar.unwrap().lifecycle.is_none());
    }

    #[test]
    fn sidecar_lifecycle_minimal_only_command_required() {
        let json = r#"{
            "name": "p",
            "provides": {
                "sidecar": {
                    "command": "bin/sidecar",
                    "protocol_version": 1,
                    "lifecycle": { "command": "voice" }
                }
            }
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        let lc = m
            .provides
            .unwrap()
            .sidecar
            .unwrap()
            .lifecycle
            .unwrap();
        assert_eq!(lc.command, "voice");
        assert!(lc.settings_category.is_none());
        assert!(lc.display_name.is_none());
        assert_eq!(lc.importance, 0);
        // effective_display_name falls back to `command` when display_name absent.
        assert_eq!(lc.effective_display_name(), "voice");
    }

    #[test]
    fn sidecar_lifecycle_clamps_importance_above_100() {
        let json = r#"{
            "name": "p",
            "provides": {
                "sidecar": {
                    "command": "bin/sidecar",
                    "protocol_version": 1,
                    "lifecycle": { "command": "v", "importance": 9999 }
                }
            }
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        let lc = m.provides.unwrap().sidecar.unwrap().lifecycle.unwrap();
        assert_eq!(lc.importance, 100);
    }

    #[test]
    fn sidecar_lifecycle_clamps_importance_below_negative_100() {
        let json = r#"{
            "name": "p",
            "provides": {
                "sidecar": {
                    "command": "bin/sidecar",
                    "protocol_version": 1,
                    "lifecycle": { "command": "v", "importance": -9999 }
                }
            }
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        let lc = m.provides.unwrap().sidecar.unwrap().lifecycle.unwrap();
        assert_eq!(lc.importance, -100);
    }

    #[test]
    fn sidecar_lifecycle_missing_command_fails() {
        let json = r#"{
            "name": "p",
            "provides": {
                "sidecar": {
                    "command": "bin/sidecar",
                    "protocol_version": 1,
                    "lifecycle": { "display_name": "no command" }
                }
            }
        }"#;
        let err = serde_json::from_str::<PluginManifest>(json).unwrap_err();
        assert!(
            err.to_string().contains("missing field `command`"),
            "expected missing `command` error, got: {err}"
        );
    }

    #[test]
    fn plugin_manifest_without_provides_is_ok() {
        let json = r#"{"name":"plain"}"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert!(m.provides.is_none());
    }


    #[test]
    fn plugin_manifest_parses_interactive_command() {
        let json = r#"{
            "name": "demo-plugin",
            "commands": [
                {
                    "name": "demo",
                    "description": "Run interactive demo",
                    "interactive": true,
                    "subcommands": ["models", "download"]
                }
            ]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        match &m.commands[0] {
            ManifestCommand::Interactive(cmd) => {
                assert_eq!(cmd.name, "demo");
                assert_eq!(cmd.description.as_deref(), Some("Run interactive demo"));
                assert_eq!(cmd.subcommands, vec!["models", "download"]);
            }
            other => panic!("expected interactive command, got {other:?}"),
        }
    }

    #[test]
    fn plugin_manifest_parses_commands() {
        let json = r#"{
            "name": "dev-tools",
            "commands": [
                {
                    "name": "lint",
                    "description": "Run lint",
                    "command": "bash",
                    "args": ["scripts/lint.sh"]
                }
            ]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.commands.len(), 1);
        match &m.commands[0] {
            ManifestCommand::Shell(cmd) => {
                assert_eq!(cmd.name, "lint");
                assert_eq!(cmd.description.as_deref(), Some("Run lint"));
                assert_eq!(cmd.command, "bash");
                assert_eq!(cmd.args, vec!["scripts/lint.sh"]);
            }
            other => panic!("expected shell command, got {other:?}"),
        }
    }

    #[test]
    fn plugin_manifest_parses_extension_tool_command() {
        let json = r#"{
            "name": "dev-tools",
            "commands": [
                {
                    "name": "echo",
                    "description": "Echo via extension tool",
                    "tool": "echo",
                    "input": {"text": "hello"}
                }
            ]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        match &m.commands[0] {
            ManifestCommand::ExtensionTool(cmd) => {
                assert_eq!(cmd.name, "echo");
                assert_eq!(cmd.tool, "echo");
                assert_eq!(cmd.input["text"], "hello");
            }
            other => panic!("expected extension tool command, got {other:?}"),
        }
    }

    #[test]
    fn plugin_manifest_parses_skill_prompt_command() {
        let json = r#"{
            "name": "dev-tools",
            "commands": [
                {
                    "name": "review",
                    "description": "Run review skill",
                    "skill": "reviewer",
                    "prompt": "Review this diff"
                }
            ]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        match &m.commands[0] {
            ManifestCommand::SkillPrompt(cmd) => {
                assert_eq!(cmd.name, "review");
                assert_eq!(cmd.skill, "reviewer");
                assert_eq!(cmd.prompt, "Review this diff");
            }
            other => panic!("expected skill prompt command, got {other:?}"),
        }
    }

    #[test]
    fn plugin_manifest_command_missing_command_fails() {
        let json = r#"{"name":"p","commands":[{"name":"x"}]}"#;
        let result: Result<PluginManifest, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn plugin_manifest_missing_name_fails() {
        let json = r#"{"version":"1.0.0"}"#;
        let result: Result<PluginManifest, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn marketplace_manifest_basic() {
        let json = r#"{
            "name": "pi-skills",
            "version": "1.0.0",
            "description": "Plugin index",
            "categories": ["productivity"],
            "keywords": ["local-first"],
            "trust": {"publisher":"Maha Media","homepage":"https://example.com"},
            "plugins": [
                {"name": "web-tools", "source": "./web-tools-plugin", "category":"research", "keywords":["web"]},
                {"name": "dev-tools", "source": "./dev-tools", "version": "2.0.0", "license":"MIT"}
            ]
        }"#;
        let m: MarketplaceManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.name, "pi-skills");
        assert_eq!(m.categories, vec!["productivity"]);
        assert_eq!(m.keywords, vec!["local-first"]);
        assert_eq!(m.trust.as_ref().unwrap().publisher.as_deref(), Some("Maha Media"));
        assert_eq!(m.plugins.len(), 2);
        assert_eq!(m.plugins[0].name, "web-tools");
        assert_eq!(m.plugins[0].source.as_deref(), Some("./web-tools-plugin"));
        assert_eq!(m.plugins[0].category.as_deref(), Some("research"));
        assert_eq!(m.plugins[0].keywords, vec!["web"]);
    }

    #[test]
    fn marketplace_manifest_missing_plugins_fails() {
        let json = r#"{"name":"empty"}"#;
        let result: Result<MarketplaceManifest, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn plugin_manifest_parses_settings_categories_with_declarative_fields() {
        let json = r#"{
            "name": "demo",
            "settings": {
                "category": [
                    {
                        "id": "demo",
                        "label": "Demo",
                        "fields": [
                            {
                                "key": "backend",
                                "label": "Backend",
                                "editor": "cycler",
                                "options": ["auto", "cpu", "cuda"]
                            },
                            {
                                "key": "endpoint",
                                "label": "API endpoint",
                                "editor": "text",
                                "help": "Base URL"
                            },
                            {
                                "key": "max_tokens",
                                "label": "Max tokens",
                                "editor": "text",
                                "numeric": true,
                                "default": 2048
                            },
                            {
                                "key": "model_path",
                                "label": "Model",
                                "editor": "custom"
                            },
                            {
                                "key": "preset",
                                "label": "Preset",
                                "editor": "picker"
                            }
                        ]
                    }
                ]
            }
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        let s = m.settings.expect("settings should deserialize");
        assert_eq!(s.categories.len(), 1);
        let cat = &s.categories[0];
        assert_eq!(cat.id, "demo");
        assert_eq!(cat.label, "Demo");
        assert_eq!(cat.fields.len(), 5);

        assert_eq!(cat.fields[0].key, "backend");
        assert_eq!(cat.fields[0].editor, ManifestEditorKind::Cycler);
        assert_eq!(cat.fields[0].options, vec!["auto", "cpu", "cuda"]);

        assert_eq!(cat.fields[1].editor, ManifestEditorKind::Text);
        assert!(!cat.fields[1].numeric);
        assert_eq!(cat.fields[1].help.as_deref(), Some("Base URL"));

        assert_eq!(cat.fields[2].editor, ManifestEditorKind::Text);
        assert!(cat.fields[2].numeric);
        assert_eq!(cat.fields[2].default, Some(serde_json::json!(2048)));

        assert_eq!(cat.fields[3].editor, ManifestEditorKind::Custom);
        assert_eq!(cat.fields[4].editor, ManifestEditorKind::Picker);
    }

    #[test]
    fn plugin_manifest_settings_default_to_none() {
        let json = r#"{"name":"plain"}"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert!(m.settings.is_none());
    }

    #[test]
    fn plugin_manifest_settings_unknown_editor_kind_fails() {
        let json = r#"{
            "name": "demo",
            "settings": {
                "category": [
                    { "id": "x", "label": "X", "fields": [
                        { "key": "k", "label": "L", "editor": "bogus" }
                    ] }
                ]
            }
        }"#;
        let result: Result<PluginManifest, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn plugin_manifest_settings_additive_with_help_entries_field() {
        // Verifies the `settings` field (Phase 4) and the `help_entries`
        // field (help-command series) coexist on PluginManifest.
        let json = r#"{
            "name": "merge-friendly",
            "settings": {
                "category": [
                    { "id": "x", "label": "X", "fields": [] }
                ]
            },
            "help_entries": [
                {
                    "id": "x-do",
                    "command": "/x:do",
                    "title": "Do",
                    "summary": "do a thing"
                }
            ]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert!(m.settings.is_some());
        assert_eq!(m.settings.unwrap().categories[0].id, "x");
        assert_eq!(m.help_entries.len(), 1);
        assert_eq!(m.help_entries[0].command, "/x:do");
    }

    #[test]
    fn marketplace_entry_missing_source_is_allowed_for_index_backed_entries() {
        let json = r#"{"name":"p","plugins":[{"name":"x"}]}"#;
        let m: MarketplaceManifest = serde_json::from_str(json).unwrap();
        assert!(m.plugins[0].source.is_none());
    }
}
