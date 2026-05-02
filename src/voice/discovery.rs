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

/// Build-info reported by the sidecar via `--print-build-info`.
///
/// One JSON line on stdout looking like:
/// ```json
/// {"backend":"cpu","features":["local-stt"],"version":"0.1.0"}
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarBuildInfo {
    /// "cpu" | "cuda" | "metal" | "vulkan" | "openblas" (best-effort).
    pub backend: String,
    pub features: Vec<String>,
    pub version: String,
}

#[derive(serde::Deserialize)]
struct RawBuildInfo {
    backend: String,
    #[serde(default)]
    features: Vec<String>,
    #[serde(default)]
    version: String,
}

/// Spawn the sidecar binary with `--print-build-info`, parse its single-line
/// JSON response, and return the compiled backend info. Returns `None` if
/// the binary is missing, exits nonzero, or emits unparseable output. Logs a
/// warning via `tracing::warn!` on failure but never panics.
pub fn read_build_info(binary: &Path) -> Option<SidecarBuildInfo> {
    let output = match std::process::Command::new(binary)
        .arg("--print-build-info")
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(
                "voice: read_build_info: failed to spawn {}: {}",
                binary.display(),
                e
            );
            return None;
        }
    };
    if !output.status.success() {
        tracing::warn!(
            "voice: read_build_info: {} exited with {:?}",
            binary.display(),
            output.status.code()
        );
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().find(|l| !l.trim().is_empty())?;
    match serde_json::from_str::<RawBuildInfo>(line.trim()) {
        Ok(raw) => Some(SidecarBuildInfo {
            backend: raw.backend,
            features: raw.features,
            version: raw.version,
        }),
        Err(e) => {
            tracing::warn!(
                "voice: read_build_info: failed to parse JSON from {}: {} (line was {:?})",
                binary.display(),
                e,
                line
            );
            None
        }
    }
}

/// Best-effort detection of the most-capable accelerator the host could
/// support. Order: cuda > metal > vulkan > openblas > cpu.
///
/// All probes are non-panicking and short-circuit fast.
pub fn detect_host_backend() -> &'static str {
    if probe_cuda() {
        return "cuda";
    }
    if cfg!(target_os = "macos") {
        return "metal";
    }
    if probe_command_ok("vulkaninfo", &[]) {
        return "vulkan";
    }
    if probe_command_ok("pkg-config", &["--exists", "openblas"]) {
        return "openblas";
    }
    "cpu"
}

fn probe_cuda() -> bool {
    if Path::new("/usr/local/cuda").exists() {
        return true;
    }
    if probe_command_ok("nvcc", &["--version"]) {
        return true;
    }
    if probe_command_ok("nvidia-smi", &[]) {
        return true;
    }
    false
}

fn probe_command_ok(cmd: &str, args: &[&str]) -> bool {
    let mut command = std::process::Command::new(cmd);
    command.args(args);
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null());
    command.stdin(std::process::Stdio::null());
    match command.status() {
        Ok(s) => s.success(),
        Err(_) => false,
    }
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
        let model = sidecar.model.as_ref().unwrap();
        assert!(model.required_for_real_stt);
        assert_eq!(
            model.default_path.as_deref(),
            Some("~/.synaps-cli/models/whisper/ggml-base.en.bin")
        );
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

    #[test]
    fn detect_host_backend_returns_known_string() {
        let b = detect_host_backend();
        assert!(
            matches!(b, "cpu" | "cuda" | "metal" | "vulkan" | "openblas"),
            "unexpected backend: {}",
            b
        );
    }

    #[test]
    fn read_build_info_returns_none_on_missing_binary() {
        let p = PathBuf::from("/nonexistent/path/to/synaps-voice-plugin-xyz");
        assert!(read_build_info(&p).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn read_build_info_parses_single_line_json() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            f,
            "#!/bin/sh\necho '{{\"backend\":\"cuda\",\"features\":[\"cuda\"],\"version\":\"0.1.0\"}}'"
        )
        .unwrap();
        let path = f.into_temp_path();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        let info = read_build_info(path.as_ref()).expect("should parse");
        assert_eq!(info.backend, "cuda");
        assert_eq!(info.features, vec!["cuda".to_string()]);
        assert_eq!(info.version, "0.1.0");
    }

    #[cfg(unix)]
    #[test]
    fn read_build_info_returns_none_on_garbage_output() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "#!/bin/sh\necho 'not json at all'").unwrap();
        let path = f.into_temp_path();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        assert!(read_build_info(path.as_ref()).is_none());
    }
}
