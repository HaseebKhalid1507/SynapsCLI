//! Parse .synaps-plugin/plugin.json and .synaps-plugin/marketplace.json.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    pub name: String,
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
    fn plugin_manifest_missing_name_fails() {
        let json = r#"{"version":"1.0.0"}"#;
        let result: Result<PluginManifest, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }
}
