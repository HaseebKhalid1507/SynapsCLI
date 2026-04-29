//! Extension manifest — parsed from plugin.json's `extension` field.

use serde::{Deserialize, Serialize};

/// Extension declaration inside a plugin manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionManifest {
    /// Runtime type (only "process" in phase 1).
    pub runtime: ExtensionRuntime,
    /// Command to start the extension process.
    pub command: String,
    /// Arguments to pass to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Permissions requested by the extension.
    #[serde(default)]
    pub permissions: Vec<String>,
    /// Hooks the extension wants to subscribe to.
    #[serde(default)]
    pub hooks: Vec<HookSubscription>,
}

/// Supported extension runtime types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtensionRuntime {
    Process,
}

/// A hook subscription declared in the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookSubscription {
    /// Hook name (e.g. "before_tool_call", "on_session_start")
    pub hook: String,
    /// Optional tool filter (e.g. "bash" for tool-specific hooks)
    #[serde(default)]
    pub tool: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Happy-path deserialisation ──────────────────────────────────────────

    #[test]
    fn deserialize_full_manifest() {
        let json = r#"{
            "runtime": "process",
            "command": "/usr/bin/my-ext",
            "args": ["--port", "0"],
            "permissions": ["read_context", "write_output"],
            "hooks": [
                {"hook": "before_tool_call", "tool": "bash"},
                {"hook": "on_session_start"}
            ]
        }"#;

        let m: ExtensionManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.runtime, ExtensionRuntime::Process);
        assert_eq!(m.command, "/usr/bin/my-ext");
        assert_eq!(m.args, vec!["--port", "0"]);
        assert_eq!(m.permissions, vec!["read_context", "write_output"]);
        assert_eq!(m.hooks.len(), 2);
        assert_eq!(m.hooks[0].hook, "before_tool_call");
        assert_eq!(m.hooks[0].tool.as_deref(), Some("bash"));
        assert_eq!(m.hooks[1].hook, "on_session_start");
        assert_eq!(m.hooks[1].tool, None);
    }

    // ── Optional fields default correctly ──────────────────────────────────

    #[test]
    fn missing_optional_fields_get_defaults() {
        let json = r#"{
            "runtime": "process",
            "command": "my-ext"
        }"#;

        let m: ExtensionManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.runtime, ExtensionRuntime::Process);
        assert_eq!(m.command, "my-ext");
        assert!(m.args.is_empty(), "args should default to []");
        assert!(m.permissions.is_empty(), "permissions should default to []");
        assert!(m.hooks.is_empty(), "hooks should default to []");
    }

    #[test]
    fn hook_subscription_tool_defaults_to_none() {
        let json = r#"{
            "runtime": "process",
            "command": "ext",
            "hooks": [{"hook": "on_session_start"}]
        }"#;

        let m: ExtensionManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.hooks[0].tool, None);
    }

    // ── Required fields ────────────────────────────────────────────────────

    #[test]
    fn missing_command_fails() {
        let json = r#"{"runtime": "process"}"#;
        let result: Result<ExtensionManifest, _> = serde_json::from_str(json);
        assert!(result.is_err(), "command is required");
    }

    #[test]
    fn missing_runtime_fails() {
        let json = r#"{"command": "my-ext"}"#;
        let result: Result<ExtensionManifest, _> = serde_json::from_str(json);
        assert!(result.is_err(), "runtime is required");
    }

    // ── Unknown / invalid runtime type ─────────────────────────────────────

    #[test]
    fn unknown_runtime_type_errors() {
        let json = r#"{
            "runtime": "wasm",
            "command": "my-ext"
        }"#;
        let result: Result<ExtensionManifest, _> = serde_json::from_str(json);
        assert!(result.is_err(), "unknown runtime 'wasm' should be rejected");
    }

    #[test]
    fn runtime_is_case_sensitive() {
        // serde rename_all = "lowercase" means "Process" (capitalised) is invalid
        let json = r#"{"runtime": "Process", "command": "ext"}"#;
        let result: Result<ExtensionManifest, _> = serde_json::from_str(json);
        assert!(result.is_err(), "runtime matching is lowercase-only");
    }

    // ── Round-trip ─────────────────────────────────────────────────────────

    #[test]
    fn serialize_roundtrip() {
        let original = ExtensionManifest {
            runtime: ExtensionRuntime::Process,
            command: "my-ext".to_string(),
            args: vec!["--verbose".to_string()],
            permissions: vec!["read_context".to_string()],
            hooks: vec![HookSubscription {
                hook: "before_tool_call".to_string(),
                tool: Some("bash".to_string()),
            }],
        };

        let json = serde_json::to_string(&original).unwrap();
        let restored: ExtensionManifest = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.runtime, original.runtime);
        assert_eq!(restored.command, original.command);
        assert_eq!(restored.args, original.args);
        assert_eq!(restored.permissions, original.permissions);
        assert_eq!(restored.hooks[0].hook, original.hooks[0].hook);
        assert_eq!(restored.hooks[0].tool, original.hooks[0].tool);
    }

    // ── Runtime serialises as lowercase string ──────────────────────────────

    #[test]
    fn runtime_serializes_as_lowercase() {
        let rt = ExtensionRuntime::Process;
        let json = serde_json::to_string(&rt).unwrap();
        assert_eq!(json, r#""process""#);
    }
}
