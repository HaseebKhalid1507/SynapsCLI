//! Discover a sidecar from loaded plugin manifests.
//!
//! Walks the loaded plugin set and returns the first plugin that
//! declares a sidecar binary in its manifest. Synaps CLI today supports
//! at most one active sidecar per session.
//!
//! The `command` field from the manifest is resolved to an absolute
//! path: relative paths are joined to the plugin root.
//!
//! ## Manifest schema
//!
//! Plugins declare a sidecar via `provides.sidecar` in their plugin
//! manifest.

use std::path::{Path, PathBuf};

use crate::skills::manifest::{SidecarLifecycle, SidecarManifest, SidecarModel};
use crate::skills::Plugin;

/// A discovered sidecar, resolved against its plugin root and ready to
/// be spawned by [`crate::sidecar::manager::SidecarManager`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredSidecar {
    /// Plugin name from the manifest.
    pub plugin_name: String,
    /// Absolute path to the plugin's root directory.
    pub plugin_root: PathBuf,
    /// Absolute path to the sidecar binary.
    pub binary: PathBuf,
    /// Sidecar wire-protocol version declared by the plugin.
    pub protocol_version: u16,
    /// Optional setup script path (resolved against plugin root).
    pub setup_script: Option<PathBuf>,
    /// Optional model metadata (modality-specific; opaque to core).
    pub model: Option<SidecarModel>,
    /// Optional plugin-claimed lifecycle UX (Phase 8). When `Some`,
    /// core auto-registers `<lifecycle.command> toggle/status` and
    /// uses `display_name` for the pill / status / errors. When
    /// `None`, the plugin is reachable via the generic `/sidecar`
    /// fallback only.
    pub lifecycle: Option<SidecarLifecycle>,
}

impl DiscoveredSidecar {
    fn from_plugin(plugin: &Plugin, sidecar: &SidecarManifest) -> Self {
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
            lifecycle: sidecar.lifecycle.clone(),
        }
    }
}

/// Discover the first sidecar declared by any plugin in `plugins`.
///
/// Phase 8 transition: this is a thin wrapper over [`discover_all_in`]
/// that returns the first result. New code should prefer
/// [`discover_all_in`] / [`discover_all`] which return every sidecar.
pub fn discover_in(plugins: &[Plugin]) -> Option<DiscoveredSidecar> {
    discover_all_in(plugins).into_iter().next()
}

/// Discover every sidecar declared by any plugin in `plugins`.
///
/// Order matches the input plugin order — caller is responsible for
/// sorting (e.g. by `lifecycle.importance`) if a deterministic display
/// order is needed.
pub fn discover_all_in(plugins: &[Plugin]) -> Vec<DiscoveredSidecar> {
    let mut out = Vec::new();
    for plugin in plugins {
        let Some(manifest) = plugin.manifest.as_ref() else {
            continue;
        };
        let Some(provides) = manifest.provides.as_ref() else {
            continue;
        };
        let Some(sidecar) = provides.sidecar.as_ref() else {
            continue;
        };
        out.push(DiscoveredSidecar::from_plugin(plugin, sidecar));
    }
    out
}

/// Discover by walking the default plugin roots — a thin wrapper
/// around [`crate::skills::loader::load_all`] for callers that don't
/// already hold the plugin set.
pub fn discover() -> Option<DiscoveredSidecar> {
    discover_all().into_iter().next()
}

/// Discover every sidecar by walking the default plugin roots.
pub fn discover_all() -> Vec<DiscoveredSidecar> {
    let (plugins, _) = crate::skills::loader::load_all(&crate::skills::loader::default_roots());
    discover_all_in(&plugins)
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

    fn sidecar_plugin() -> Plugin {
        // Canonical `provides.sidecar` fixture.
        let manifest_json = r#"{
            "name": "sample-sidecar",
            "provides": {
                "sidecar": {
                    "command": "bin/sample-sidecar",
                    "setup": "scripts/setup.sh",
                    "protocol_version": 1,
                    "model": {
                        "default_path": "~/.synaps-cli/models/sample/model.bin",
                        "required": true
                    }
                }
            }
        }"#;
        let manifest: PluginManifest = serde_json::from_str(manifest_json).unwrap();
        Plugin {
            name: "sample-sidecar".into(),
            root: PathBuf::from("/opt/synaps-skills/sample-sidecar"),
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
    fn discover_returns_none_when_no_plugin_provides_a_sidecar() {
        let plugins = vec![plain_plugin("a"), plain_plugin("b")];
        assert_eq!(discover_in(&plugins), None);
    }

    #[test]
    fn discover_resolves_relative_binary_under_plugin_root() {
        let plugins = vec![sidecar_plugin()];
        let sidecar = discover_in(&plugins).expect("sidecar plugin should be discovered");
        assert_eq!(sidecar.plugin_name, "sample-sidecar");
        assert_eq!(
            sidecar.binary,
            PathBuf::from("/opt/synaps-skills/sample-sidecar/bin/sample-sidecar")
        );
        assert_eq!(
            sidecar.setup_script.as_deref(),
            Some(PathBuf::from(
                "/opt/synaps-skills/sample-sidecar/scripts/setup.sh"
            ))
            .as_deref()
        );
        assert_eq!(sidecar.protocol_version, 1);
    }

    #[test]
    fn discover_keeps_absolute_binary_path_unchanged() {
        let plugin_json = r#"{
            "name": "abs-sidecar",
            "provides": {
                "sidecar": {
                    "command": "/usr/local/bin/sidecar",
                    "protocol_version": 1
                }
            }
        }"#;
        let manifest: PluginManifest = serde_json::from_str(plugin_json).unwrap();
        let plugin = Plugin {
            name: "abs-sidecar".into(),
            root: PathBuf::from("/opt/abs-sidecar"),
            marketplace: None,
            version: None,
            description: None,
            extension: None,
            manifest: Some(manifest),
        };
        let sidecar = discover_in(&[plugin]).expect("absolute path should be discovered");
        assert_eq!(sidecar.binary, PathBuf::from("/usr/local/bin/sidecar"));
    }

    #[test]
    fn discover_picks_first_plugin_with_a_sidecar() {
        let plugins = vec![plain_plugin("zzz"), sidecar_plugin(), plain_plugin("aaa")];
        let sidecar = discover_in(&plugins).expect("should find sidecar plugin");
        assert_eq!(sidecar.plugin_name, "sample-sidecar");
    }

    #[test]
    fn discover_accepts_canonical_sidecar_field() {
        // Phase 7 slice G: new plugins should declare `provides.sidecar`
        // This test guards the canonical manifest shape.
        let plugin_json = r#"{
            "name": "modality-neutral",
            "provides": {
                "sidecar": {
                    "command": "bin/sidecar",
                    "protocol_version": 1
                }
            }
        }"#;
        let manifest: PluginManifest = serde_json::from_str(plugin_json).unwrap();
        let plugin = Plugin {
            name: "modality-neutral".into(),
            root: PathBuf::from("/opt/modality-neutral"),
            marketplace: None,
            version: None,
            description: None,
            extension: None,
            manifest: Some(manifest),
        };
        let sidecar = discover_in(&[plugin]).expect("canonical field should be discovered");
        assert_eq!(sidecar.plugin_name, "modality-neutral");
        assert_eq!(sidecar.binary, PathBuf::from("/opt/modality-neutral/bin/sidecar"));
    }

    // ---- Phase 8 slice 8A: lifecycle propagation + discover_all -------------

    fn plugin_with_lifecycle(name: &str, lifecycle_command: &str, importance: i32) -> Plugin {
        let manifest_json = format!(
            r#"{{
                "name": "{name}",
                "provides": {{
                    "sidecar": {{
                        "command": "bin/{name}-sidecar",
                        "protocol_version": 1,
                        "lifecycle": {{
                            "command": "{lifecycle_command}",
                            "settings_category": "{lifecycle_command}",
                            "display_name": "{lifecycle_command}-display",
                            "importance": {importance}
                        }}
                    }}
                }}
            }}"#
        );
        let manifest: PluginManifest = serde_json::from_str(&manifest_json).unwrap();
        Plugin {
            name: name.into(),
            root: PathBuf::from(format!("/opt/{name}")),
            marketplace: None,
            version: None,
            description: None,
            extension: None,
            manifest: Some(manifest),
        }
    }

    #[test]
    fn discovered_propagates_lifecycle_when_present() {
        let plugin = plugin_with_lifecycle("p", "sensor", 50);
        let s = discover_in(&[plugin]).expect("should discover");
        let lc = s.lifecycle.expect("lifecycle should propagate");
        assert_eq!(lc.command, "sensor");
        assert_eq!(lc.importance, 50);
        assert_eq!(lc.effective_display_name(), "sensor-display");
    }

    #[test]
    fn discovered_lifecycle_is_none_when_absent() {
        let plugins = vec![sidecar_plugin()];
        let s = discover_in(&plugins).unwrap();
        assert!(s.lifecycle.is_none(), "no lifecycle declared → should be None");
    }

    #[test]
    fn discover_all_returns_every_sidecar_in_input_order() {
        let plugins = vec![
            plugin_with_lifecycle("a", "alpha", 10),
            plain_plugin("no-sidecar-here"),
            plugin_with_lifecycle("b", "beta", 90),
            plugin_with_lifecycle("c", "gamma", -5),
        ];
        let all = discover_all_in(&plugins);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].plugin_name, "a");
        assert_eq!(all[1].plugin_name, "b");
        assert_eq!(all[2].plugin_name, "c");
    }

    #[test]
    fn discover_all_returns_empty_when_no_sidecars() {
        let plugins = vec![plain_plugin("x"), plain_plugin("y")];
        assert!(discover_all_in(&plugins).is_empty());
    }

    #[test]
    fn discover_in_matches_discover_all_in_first_for_compatibility() {
        let plugins = vec![
            plain_plugin("a"),
            sidecar_plugin(),
            plugin_with_lifecycle("b", "beta", 0),
        ];
        let single = discover_in(&plugins).unwrap();
        let multi = discover_all_in(&plugins);
        assert_eq!(single, multi[0]);
    }
}
