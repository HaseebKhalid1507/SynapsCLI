//! Parse .synaps-plugin/plugin.json and .synaps-plugin/marketplace.json.

use serde::Deserialize;

use super::keybinds::ManifestKeybind;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ManifestCommand {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
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
    pub commands: Vec<ManifestCommand>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarketplaceManifest {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    pub plugins: Vec<MarketplacePluginEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarketplacePluginEntry {
    pub name: String,
    pub source: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
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
            "unknown_field": 42
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.name, "web-tools");
        assert_eq!(m.version.as_deref(), Some("1.0.0"));
        assert_eq!(m.description.as_deref(), Some("Web tools"));
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
        assert_eq!(m.commands[0].name, "lint");
        assert_eq!(m.commands[0].description.as_deref(), Some("Run lint"));
        assert_eq!(m.commands[0].command, "bash");
        assert_eq!(m.commands[0].args, vec!["scripts/lint.sh"]);
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
            "plugins": [
                {"name": "web-tools", "source": "./web-tools-plugin"},
                {"name": "dev-tools", "source": "./dev-tools", "version": "2.0.0"}
            ]
        }"#;
        let m: MarketplaceManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.name, "pi-skills");
        assert_eq!(m.plugins.len(), 2);
        assert_eq!(m.plugins[0].name, "web-tools");
        assert_eq!(m.plugins[0].source, "./web-tools-plugin");
    }

    #[test]
    fn marketplace_manifest_missing_plugins_fails() {
        let json = r#"{"name":"empty"}"#;
        let result: Result<MarketplaceManifest, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn marketplace_entry_missing_source_fails() {
        let json = r#"{"name":"p","plugins":[{"name":"x"}]}"#;
        let result: Result<MarketplaceManifest, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }
}
