//! Extension-provided capability and build information returned by `info.get`.

use serde::{Deserialize, Serialize};

/// Best-effort plugin information advertised after `initialize()`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct PluginInfo {
    /// Optional build metadata for sidecars/backends owned by the plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<PluginBuildInfo>,
    /// Human-readable capability inventory.
    #[serde(default)]
    pub capabilities: Vec<PluginCapabilityInfo>,
    /// Optional model inventory advertised by the plugin.
    #[serde(default)]
    pub models: Vec<PluginModelInfo>,
}

/// Build metadata for a plugin-owned backend or sidecar.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PluginBuildInfo {
    /// Active backend, e.g. `cpu`, `cuda`, `metal`, `vulkan`, `openblas`.
    pub backend: String,
    /// Compile/runtime feature flags reported by the plugin.
    #[serde(default)]
    pub features: Vec<String>,
    /// Plugin or backend version string.
    #[serde(default)]
    pub version: String,
}

/// One advertised capability.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PluginCapabilityInfo {
    /// Capability class, e.g. `voice`, `models`, `tasks`, `settings`.
    pub kind: String,
    /// Display name.
    pub name: String,
    /// Optional supported modes for this capability.
    #[serde(default)]
    pub modes: Vec<String>,
}

/// One model known to the plugin.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PluginModelInfo {
    /// Stable model id.
    pub id: String,
    /// Optional display label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Whether the model is available locally.
    #[serde(default)]
    pub installed: bool,
}
