//! Extension manifest model and validation.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::hooks::events::HookKind;
use super::permissions::PermissionSet;

/// Current extension protocol version supported by SynapsCLI.
pub const CURRENT_EXTENSION_PROTOCOL_VERSION: u32 = 1;

fn default_protocol_version() -> u32 {
    CURRENT_EXTENSION_PROTOCOL_VERSION
}

/// Extension declaration inside a plugin manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionManifest {
    /// Extension protocol version. Defaults to v1 for pre-versioned manifests.
    #[serde(default = "default_protocol_version")]
    pub protocol_version: u32,
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
    /// Non-secret config declarations resolved by Synaps and passed to initialize.
    #[serde(default)]
    pub config: Vec<ExtensionConfigEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExtensionConfigValueKind {
    String,
    Bool,
    Number,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtensionConfigEntry {
    pub key: String,
    #[serde(default, rename = "type")]
    pub value_type: Option<ExtensionConfigValueKind>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<Value>,
    #[serde(default)]
    pub secret_env: Option<String>,
}

/// A validated extension manifest prepared for loading.
#[derive(Debug, Clone)]
pub struct ValidatedExtensionManifest {
    pub permissions: PermissionSet,
    pub subscriptions: Vec<(HookKind, Option<String>, Option<HookMatcher>)>,
}

impl ExtensionManifest {
    /// Validate manifest fields and derive typed permissions/subscriptions.
    pub fn validate(&self, id: &str) -> Result<ValidatedExtensionManifest, String> {
        if self.protocol_version != CURRENT_EXTENSION_PROTOCOL_VERSION {
            return Err(format!(
                "Extension '{}' uses unsupported protocol_version {} (supported: {})",
                id, self.protocol_version, CURRENT_EXTENSION_PROTOCOL_VERSION,
            ));
        }

        if self.command.trim().is_empty() {
            return Err(format!("Extension '{}' has empty command", id));
        }

        let has_capability_permission = self.permissions.iter().any(|permission| {
            matches!(
                permission.as_str(),
                "tools.register" | "providers.register" | "memory.read" | "memory.write"
                    | "config.write" | "config.subscribe" | "audio.input" | "audio.output"
            )
        });
        if self.hooks.is_empty() && !has_capability_permission {
            return Err(format!("Extension '{}' must subscribe to at least one hook or request a registration permission", id));
        }

        let permissions = PermissionSet::try_from_strings(&self.permissions)?;
        let mut subscriptions = Vec::with_capacity(self.hooks.len());
        for sub in &self.hooks {
            let kind = HookKind::from_str(&sub.hook).ok_or_else(|| {
                format!("Unknown hook kind: '{}' in extension '{}'", sub.hook, id)
            })?;
            if !permissions.allows_hook(kind) {
                return Err(format!(
                    "Extension '{}' lacks permission '{}' required for hook '{}'",
                    id,
                    kind.required_permission().as_str(),
                    kind.as_str(),
                ));
            }
            if sub.tool.is_some() && !kind.allows_tool_filter() {
                return Err(format!(
                    "Extension '{}' hook '{}' does not allow a tool filter",
                    id,
                    kind.as_str(),
                ));
            }
            subscriptions.push((kind, sub.tool.clone(), sub.matcher.clone()));
        }

        Ok(ValidatedExtensionManifest {
            permissions,
            subscriptions,
        })
    }
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
    /// Optional simple matcher conditions.
    #[serde(default, rename = "match")]
    pub matcher: Option<HookMatcher>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HookMatcher {
    #[serde(default)]
    pub input_contains: Option<String>,
    #[serde(default)]
    pub input_equals: Option<serde_json::Value>,
}

impl HookMatcher {
    pub const SUPPORTED_KEYS: &'static [&'static str] = &["input_contains", "input_equals"];

    pub fn matches(&self, event: &crate::extensions::hooks::events::HookEvent) -> bool {
        let input = event.tool_input.as_ref().unwrap_or(&serde_json::Value::Null);
        if let Some(expected) = &self.input_equals {
            if input != expected {
                return false;
            }
        }
        if let Some(needle) = &self.input_contains {
            let haystack = serde_json::to_string(input).unwrap_or_default();
            if !haystack.contains(needle) {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Happy-path deserialisation ──────────────────────────────────────────

    #[test]
    fn deserialize_full_manifest() {
        let json = r#"{
            "protocol_version": 1,
            "runtime": "process",
            "command": "/usr/bin/my-ext",
            "args": ["--port", "0"],
            "permissions": ["tools.intercept", "session.lifecycle"],
            "hooks": [
                {"hook": "before_tool_call", "tool": "bash"},
                {"hook": "on_session_start"}
            ]
        }"#;

        let m: ExtensionManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.protocol_version, 1);
        assert_eq!(m.runtime, ExtensionRuntime::Process);
        assert_eq!(m.command, "/usr/bin/my-ext");
        assert_eq!(m.args, vec!["--port", "0"]);
        assert_eq!(m.permissions, vec!["tools.intercept", "session.lifecycle"]);
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
        assert_eq!(m.protocol_version, CURRENT_EXTENSION_PROTOCOL_VERSION);
        assert_eq!(m.runtime, ExtensionRuntime::Process);
        assert_eq!(m.command, "my-ext");
        assert!(m.args.is_empty(), "args should default to []");
        assert!(m.permissions.is_empty(), "permissions should default to []");
        assert!(m.hooks.is_empty(), "hooks should default to []");
    }

    #[test]
    fn extension_config_entry_deserializes_optional_type() {
        let json = r#"{
            "key": "backend",
            "type": "string",
            "description": "Backend selector",
            "default": "auto"
        }"#;

        let entry: ExtensionConfigEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.key, "backend");
        assert_eq!(entry.value_type, Some(ExtensionConfigValueKind::String));
        assert_eq!(entry.description.as_deref(), Some("Backend selector"));
        assert_eq!(entry.default, Some(serde_json::Value::String("auto".to_string())));
    }

    #[test]
    fn extension_config_entry_omitted_type_is_none() {
        let json = r#"{"key": "backend"}"#;

        let entry: ExtensionConfigEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.key, "backend");
        assert_eq!(entry.value_type, None);
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

    // ── Required fields ─────────────────────────────────────────────────────

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
        let json = r#"{"runtime": "Process", "command": "ext"}"#;
        let result: Result<ExtensionManifest, _> = serde_json::from_str(json);
        assert!(result.is_err(), "runtime matching is lowercase-only");
    }

    #[test]
    fn validate_rejects_unsupported_protocol_version() {
        let manifest = ExtensionManifest {
            protocol_version: 999,
            runtime: ExtensionRuntime::Process,
            command: "ext".to_string(),
            args: vec![],
            permissions: vec!["tools.intercept".to_string()],
            hooks: vec![HookSubscription {
                hook: "before_tool_call".to_string(),
                tool: None,
                matcher: None,
            }],
            config: vec![],
        };

        let err = manifest.validate("bad-version").unwrap_err();
        assert!(err.contains("unsupported protocol_version 999"));
    }

    #[test]
    fn validate_allows_hookless_provider_registration_extensions() {
        let manifest = ExtensionManifest {
            protocol_version: 1,
            runtime: ExtensionRuntime::Process,
            command: "ext".to_string(),
            args: vec![],
            permissions: vec!["providers.register".to_string()],
            hooks: vec![],
            config: vec![],
        };

        manifest.validate("provider-only").unwrap();
    }

    #[test]
    fn validate_rejects_tool_filter_on_non_tool_hook() {
        let manifest = ExtensionManifest {
            protocol_version: 1,
            runtime: ExtensionRuntime::Process,
            command: "ext".to_string(),
            args: vec![],
            permissions: vec!["session.lifecycle".to_string()],
            hooks: vec![HookSubscription {
                hook: "on_session_start".to_string(),
                tool: Some("bash".to_string()),
                matcher: None,
            }],
            config: vec![],
        };

        let err = manifest.validate("bad-filter").unwrap_err();
        assert!(err.contains("does not allow a tool filter"));
    }

    // ── Round-trip ─────────────────────────────────────────────────────────

    #[test]
    fn serialize_roundtrip() {
        let original = ExtensionManifest {
            protocol_version: 1,
            runtime: ExtensionRuntime::Process,
            command: "my-ext".to_string(),
            args: vec!["--verbose".to_string()],
            permissions: vec!["tools.intercept".to_string()],
            hooks: vec![HookSubscription {
                hook: "before_tool_call".to_string(),
                tool: Some("bash".to_string()),
                matcher: None,
            }],
            config: vec![],
        };

        let json = serde_json::to_string(&original).unwrap();
        let restored: ExtensionManifest = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.protocol_version, original.protocol_version);
        assert_eq!(restored.runtime, original.runtime);
        assert_eq!(restored.command, original.command);
        assert_eq!(restored.args, original.args);
        assert_eq!(restored.permissions, original.permissions);
        assert_eq!(restored.hooks[0].hook, original.hooks[0].hook);
        assert_eq!(restored.hooks[0].tool, original.hooks[0].tool);
    }

    // ── Runtime serialises as lowercase string ──────────────────────────────

    #[test]
    fn matcher_input_equals_requires_exact_tool_input() {
        let matcher = HookMatcher {
            input_contains: None,
            input_equals: Some(serde_json::json!({"command": "echo safe"})),
        };

        let matching = crate::extensions::hooks::events::HookEvent::before_tool_call(
            "bash",
            serde_json::json!({"command": "echo safe"}),
        );
        let different = crate::extensions::hooks::events::HookEvent::before_tool_call(
            "bash",
            serde_json::json!({"command": "echo safe", "extra": true}),
        );

        assert!(matcher.matches(&matching));
        assert!(!matcher.matches(&different));
    }

    #[test]
    fn matcher_conditions_are_combined_with_and() {
        let matcher = HookMatcher {
            input_contains: Some("safe".to_string()),
            input_equals: Some(serde_json::json!({"command": "echo safe"})),
        };

        let matching = crate::extensions::hooks::events::HookEvent::before_tool_call(
            "bash",
            serde_json::json!({"command": "echo safe"}),
        );
        let equals_but_missing_contains = crate::extensions::hooks::events::HookEvent::before_tool_call(
            "bash",
            serde_json::json!({"command": "echo ok"}),
        );

        assert!(matcher.matches(&matching));
        assert!(!matcher.matches(&equals_but_missing_contains));
    }

    #[test]
    fn runtime_serializes_as_lowercase() {
        let rt = ExtensionRuntime::Process;
        let json = serde_json::to_string(&rt).unwrap();
        assert_eq!(json, r#""process""#);
    }
}
