//! Discover a voice sidecar from loaded plugin manifests.
//!
//! Walks the loaded plugin set and returns the first plugin that
//! declares `provides.voice_sidecar` in its manifest. Synaps CLI today
//! supports at most one active voice sidecar per session.
//!
//! The `command` field from the manifest is resolved to an absolute
//! path: relative paths are joined to the plugin root.

use std::path::{Path, PathBuf};

use crate::skills::manifest::{VoiceSidecarManifest, VoiceSidecarModel};
use crate::skills::Plugin;

/// A discovered voice sidecar, resolved against its plugin root and
/// ready to be spawned by `crate::voice::manager::VoiceManager`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredVoiceSidecar {
    /// Plugin name from the manifest (e.g. "local-voice").
    pub plugin_name: String,
    /// Absolute path to the plugin's root directory.
    pub plugin_root: PathBuf,
    /// Absolute path to the sidecar binary.
    pub binary: PathBuf,
    /// Sidecar wire-protocol version declared by the plugin.
    pub protocol_version: u16,
    /// Optional setup script path (relative to plugin root, if relative).
    pub setup_script: Option<PathBuf>,
    /// Optional STT model metadata.
    pub model: Option<VoiceSidecarModel>,
}

impl DiscoveredVoiceSidecar {
    fn from_plugin(plugin: &Plugin, sidecar: &VoiceSidecarManifest) -> Self {
        let binary = resolve_relative(&plugin.root, &sidecar.command);
        let setup_script = sidecar
            .setup
            .as_deref()
            .map(|s| resolve_relative(&plugin.root, s));
        Self {
            plugin_name: plugin.name.clone(),
            plugin_root: plugin.root.clone(),
            binary,
            protocol_version: sidecar.protocol_version,
            setup_script,
            model: sidecar.model.clone(),
        }
    }
}

/// Discover the first voice sidecar declared by any plugin in `plugins`.
pub fn discover_in(plugins: &[Plugin]) -> Option<DiscoveredVoiceSidecar> {
    for plugin in plugins {
        let Some(manifest) = plugin.manifest.as_ref() else {
            continue;
        };
        let Some(provides) = manifest.provides.as_ref() else {
            continue;
        };
        let Some(sidecar) = provides.voice_sidecar.as_ref() else {
            continue;
        };
        return Some(DiscoveredVoiceSidecar::from_plugin(plugin, sidecar));
    }
    None
}

/// Discover by walking the default plugin roots — a thin wrapper
/// around [`crate::skills::loader::load_all`] for callers that don't
/// already hold the plugin set.
pub fn discover() -> Option<DiscoveredVoiceSidecar> {
    let (plugins, _) = crate::skills::loader::load_all(&crate::skills::loader::default_roots());
    discover_in(&plugins)
}

fn resolve_relative(root: &Path, candidate: &str) -> PathBuf {
    let path = PathBuf::from(candidate);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::manifest::PluginManifest;
    use std::path::PathBuf;

    fn voice_plugin() -> Plugin {
        let manifest_json = r#"{
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
        let manifest: PluginManifest = serde_json::from_str(manifest_json).unwrap();
        Plugin {
            name: "local-voice".into(),
            root: PathBuf::from("/opt/synaps-skills/local-voice-plugin"),
            marketplace: None,
            version: None,
            description: None,
            extension: None,
            manifest: Some(manifest),
        }
    }

    fn plain_plugin(name: &str) -> Plugin {
        let manifest_json = format!(r#"{{"name":"{}"}}"#, name);
        let manifest: PluginManifest = serde_json::from_str(&manifest_json).unwrap();
        Plugin {
            name: name.into(),
            root: PathBuf::from(format!("/opt/synaps-skills/{}", name)),
            marketplace: None,
            version: None,
            description: None,
            extension: None,
            manifest: Some(manifest),
        }
    }

    #[test]
    fn discover_returns_none_when_no_plugin_provides_voice() {
        let plugins = vec![plain_plugin("a"), plain_plugin("b")];
        assert_eq!(discover_in(&plugins), None);
    }

    #[test]
    fn discover_resolves_relative_binary_under_plugin_root() {
        let plugins = vec![voice_plugin()];
        let sidecar = discover_in(&plugins).expect("voice plugin should be discovered");
        assert_eq!(sidecar.plugin_name, "local-voice");
        assert_eq!(
            sidecar.binary,
            PathBuf::from("/opt/synaps-skills/local-voice-plugin/bin/synaps-voice-plugin")
        );
        assert_eq!(
            sidecar.setup_script.as_deref(),
            Some(PathBuf::from(
                "/opt/synaps-skills/local-voice-plugin/scripts/setup.sh"
            ))
            .as_deref()
        );
        assert_eq!(sidecar.protocol_version, 1);
    }

    #[test]
    fn discover_keeps_absolute_binary_path_unchanged() {
        let plugin_json = r#"{
            "name": "abs-voice",
            "provides": {
                "voice_sidecar": {
                    "command": "/usr/local/bin/voice",
                    "protocol_version": 1
                }
            }
        }"#;
        let manifest: PluginManifest = serde_json::from_str(plugin_json).unwrap();
        let plugin = Plugin {
            name: "abs-voice".into(),
            root: PathBuf::from("/opt/abs-voice"),
            marketplace: None,
            version: None,
            description: None,
            extension: None,
            manifest: Some(manifest),
        };
        let sidecar = discover_in(&[plugin]).expect("absolute path should be discovered");
        assert_eq!(sidecar.binary, PathBuf::from("/usr/local/bin/voice"));
    }

    #[test]
    fn discover_picks_first_plugin_with_voice_sidecar() {
        let plugins = vec![plain_plugin("zzz"), voice_plugin(), plain_plugin("aaa")];
        let sidecar = discover_in(&plugins).expect("should find voice plugin");
        assert_eq!(sidecar.plugin_name, "local-voice");
    }
}
