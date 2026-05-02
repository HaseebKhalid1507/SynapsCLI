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
}

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
    fn marketplace_entry_missing_source_is_allowed_for_index_backed_entries() {
        let json = r#"{"name":"p","plugins":[{"name":"x"}]}"#;
        let m: MarketplaceManifest = serde_json::from_str(json).unwrap();
        assert!(m.plugins[0].source.is_none());
    }
}
