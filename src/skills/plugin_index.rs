//! Plugin index schema support.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PluginIndex {
    pub schema_version: u32,
    #[serde(default)]
    pub generated_at: Option<String>,
    pub plugins: Vec<PluginIndexEntry>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PluginIndexEntry {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub repository: String,
    #[serde(default)]
    pub subdir: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    pub checksum: PluginIndexChecksum,
    pub compatibility: PluginIndexCompatibility,
    pub capabilities: PluginIndexCapabilities,
    #[serde(default)]
    pub trust: Option<PluginIndexTrust>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PluginIndexChecksum {
    pub algorithm: String,
    pub value: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PluginIndexCompatibility {
    #[serde(default)]
    pub synaps: Option<String>,
    #[serde(default)]
    pub extension_protocol: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PluginIndexCapabilities {
    #[serde(default)]
    pub skills: Vec<String>,
    pub has_extension: bool,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub hooks: Vec<String>,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub providers: Vec<PluginIndexProviderCapability>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginIndexProviderCapability {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PluginIndexTrust {
    #[serde(default)]
    pub publisher: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
}

pub fn validate_plugin_index(index: &PluginIndex) -> Result<(), String> {
    if index.schema_version != 1 {
        return Err(format!("plugin index schema_version must be 1, got {}", index.schema_version));
    }
    for (idx, plugin) in index.plugins.iter().enumerate() {
        if plugin.id.is_empty() || !plugin.id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            return Err(format!("plugins[{idx}].id must be lower-kebab-case"));
        }
        if plugin.name.trim().is_empty() {
            return Err(format!("plugins[{idx}].name is required"));
        }
        if !is_semver(&plugin.version) {
            return Err(format!("plugins[{idx}].version must be semver"));
        }
        if plugin.description.trim().is_empty() {
            return Err(format!("plugins[{idx}].description is required"));
        }
        if !(plugin.repository.starts_with("https://") || plugin.repository.starts_with("file://")) {
            return Err(format!("plugins[{idx}].repository must be https:// or file://"));
        }
        if plugin.checksum.algorithm != "sha256" {
            return Err(format!("plugins[{idx}].checksum.algorithm must be sha256"));
        }
        if plugin.checksum.value.len() != 64 || !plugin.checksum.value.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) {
            return Err(format!("plugins[{idx}].checksum.value must be 64 lowercase hex characters"));
        }
        if let Some(trust) = &plugin.trust {
            if let Some(homepage) = &trust.homepage {
                if !homepage.starts_with("https://") {
                    return Err(format!("plugins[{idx}].trust.homepage must be https://"));
                }
            }
        }
        for (provider_idx, provider) in plugin.capabilities.providers.iter().enumerate() {
            if provider.id.trim().is_empty() {
                return Err(format!("plugins[{idx}].capabilities.providers[{provider_idx}].id is required"));
            }
            if provider.id.contains(':') {
                return Err(format!("plugins[{idx}].capabilities.providers[{provider_idx}].id must not contain ':'"));
            }
            for (model_idx, model) in provider.models.iter().enumerate() {
                if model.trim().is_empty() || model.contains(':') {
                    return Err(format!("plugins[{idx}].capabilities.providers[{provider_idx}].models[{model_idx}] must be non-empty and must not contain ':'"));
                }
            }
        }
    }
    Ok(())
}

fn is_semver(value: &str) -> bool {
    let mut parts = value.splitn(2, '-');
    let core = parts.next().unwrap_or_default();
    let nums: Vec<&str> = core.split('.').collect();
    nums.len() == 3
        && nums
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_index_json() -> &'static str {
        r#"{
          "schema_version": 1,
          "generated_at": "2026-05-01T12:00:00Z",
          "plugins": [{
            "id": "session-memory",
            "name": "session-memory",
            "version": "0.1.0",
            "description": "Extracts local session notes.",
            "repository": "https://github.com/example/synaps-skills.git",
            "subdir": "session-memory-plugin",
            "checksum": {"algorithm": "sha256", "value": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"},
            "compatibility": {"synaps": ">=0.1.0", "extension_protocol": "1"},
            "capabilities": {
              "skills": ["session-memory"],
              "has_extension": true,
              "permissions": ["session.lifecycle"],
              "hooks": ["on_session_end"],
              "commands": []
            },
            "trust": {"publisher": "Maha Media", "homepage": "https://example.com"}
          }]
        }"#
    }

    #[test]
    fn parses_and_validates_v1_plugin_index() {
        let index: PluginIndex = serde_json::from_str(sample_index_json()).unwrap();
        validate_plugin_index(&index).unwrap();
        assert_eq!(index.plugins[0].id, "session-memory");
        assert!(index.plugins[0].capabilities.has_extension);
        assert_eq!(index.plugins[0].capabilities.permissions, vec!["session.lifecycle"]);
    }

    #[test]
    fn rejects_unsupported_schema_version() {
        let mut index: PluginIndex = serde_json::from_str(sample_index_json()).unwrap();
        index.schema_version = 2;
        assert!(validate_plugin_index(&index).unwrap_err().contains("schema_version"));
    }

    #[test]
    fn rejects_bad_checksum_algorithm() {
        let mut index: PluginIndex = serde_json::from_str(sample_index_json()).unwrap();
        index.plugins[0].checksum.algorithm = "md5".into();
        assert!(validate_plugin_index(&index).unwrap_err().contains("checksum.algorithm"));
    }

    #[test]
    fn rejects_bad_checksum_shape() {
        let mut index: PluginIndex = serde_json::from_str(sample_index_json()).unwrap();
        index.plugins[0].checksum.value = "abc123".into();
        assert!(validate_plugin_index(&index).unwrap_err().contains("checksum.value"));
    }
}
