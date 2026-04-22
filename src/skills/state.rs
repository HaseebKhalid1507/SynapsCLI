//! Persisted plugin management state: ~/.synaps-cli/plugins.json.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginsState {
    #[serde(default)]
    pub marketplaces: Vec<Marketplace>,
    #[serde(default)]
    pub installed: Vec<InstalledPlugin>,
    #[serde(default)]
    pub trusted_hosts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Marketplace {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub last_refreshed: Option<String>,
    #[serde(default)]
    pub cached_plugins: Vec<CachedPlugin>,
    /// Git clone URL for the marketplace repo. Set when the marketplace
    /// hosts Claude-Code-style plugins whose `source` is `./<subdir>`.
    #[serde(default)]
    pub repo_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedPlugin {
    pub name: String,
    pub source: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPlugin {
    pub name: String,
    #[serde(default)]
    pub marketplace: Option<String>,
    pub source_url: String,
    pub installed_commit: String,
    #[serde(default)]
    pub latest_commit: Option<String>,
    pub installed_at: String,
    /// When the plugin was installed from a subdir of a marketplace repo
    /// (Claude-Code-style layout), this is the subdir name. `source_url`
    /// then refers to the marketplace repo, not a standalone plugin repo.
    #[serde(default)]
    pub source_subdir: Option<String>,
}

impl PluginsState {
    pub fn load_from(path: &Path) -> std::io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(c) => serde_json::from_str(&c)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        // atomic write via temp + rename
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }

    /// Resolve the on-disk path for the current profile.
    pub fn default_path() -> std::path::PathBuf {
        crate::config::resolve_write_path("plugins.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugins_state_round_trip() {
        let s = PluginsState {
            marketplaces: vec![Marketplace {
                name: "pi-skills".into(),
                url: "https://github.com/maha-media/pi-skills".into(),
                description: Some("…".into()),
                last_refreshed: Some("2026-04-18T12:00:00Z".into()),
                cached_plugins: vec![CachedPlugin {
                    name: "web".into(),
                    source: "https://github.com/maha-media/pi-web.git".into(),
                    version: Some("1.0".into()),
                    description: Some("Web tools".into()),
                }],
                repo_url: Some("https://github.com/maha-media/pi-skills.git".into()),
            }],
            installed: vec![InstalledPlugin {
                name: "web".into(),
                marketplace: Some("pi-skills".into()),
                source_url: "https://github.com/maha-media/pi-web.git".into(),
                installed_commit: "abc123".into(),
                latest_commit: Some("abc123".into()),
                installed_at: "2026-04-18T12:01:00Z".into(),
                source_subdir: None,
            }],
            trusted_hosts: vec!["github.com/maha-media".into()],
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: PluginsState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.marketplaces.len(), 1);
        assert_eq!(back.installed.len(), 1);
        assert_eq!(back.trusted_hosts, vec!["github.com/maha-media"]);
    }

    #[test]
    fn plugins_state_defaults_to_empty() {
        let empty: PluginsState = serde_json::from_str("{}").unwrap();
        assert!(empty.marketplaces.is_empty());
        assert!(empty.installed.is_empty());
        assert!(empty.trusted_hosts.is_empty());
    }

    #[test]
    fn plugins_state_load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugins.json");
        let loaded = PluginsState::load_from(&path).unwrap();
        assert!(loaded.marketplaces.is_empty());
    }

    #[test]
    fn plugins_state_save_and_load_round_trip_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugins.json");
        let mut s = PluginsState::default();
        s.trusted_hosts.push("github.com/x".into());
        s.save_to(&path).unwrap();
        let back = PluginsState::load_from(&path).unwrap();
        assert_eq!(back.trusted_hosts, vec!["github.com/x"]);
    }

    #[test]
    fn plugins_state_load_malformed_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugins.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(PluginsState::load_from(&path).is_err());
    }
}
