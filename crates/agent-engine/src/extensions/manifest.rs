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
    /// Optional path to a post-install setup script (relative to plugin
    /// root). When present, the marketplace install flow runs this
    /// script after the plugin source is in place — used by source-shipped
    /// extensions (e.g. Rust binaries) that need to compile a binary
    /// before [`Self::command`] resolves. Same security model as
    /// `provides.sidecar.setup` (path must stay inside the plugin dir;
    /// see [`crate::skills::post_install`] for the runner).
    #[serde(default)]
    pub setup: Option<String>,
    /// Optional per-host-triple prebuilt asset map. When the installer
    /// can't find [`Self::command`] on disk after the source clone, it
    /// looks up the current host's triple (e.g. `linux-x86_64`,
    /// `darwin-arm64`, `windows-x86_64` — see
    /// [`crate::skills::post_install::host_triple`]) in this map and,
    /// if a matching [`PrebuiltAsset`] exists, downloads and extracts
    /// it into the plugin dir as a fast path that skips
    /// [`Self::setup`]. Empty by default.
    #[serde(default)]
    pub prebuilt: std::collections::HashMap<String, PrebuiltAsset>,
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
    /// OPTIONAL (additive, protocol v1-compatible per docs/STABILITY.md §1):
    /// theme tokens this extension declares, merged by the TUI into the
    /// active theme under the `ext.<plugin-id>.<token>` namespace at load.
    /// Keys are token names (no dots/slashes/spaces); values are hex colors
    /// (`#rgb` or `#rrggbb`). A user theme-TOML key `ext.<plugin-id>.<token>`
    /// always overrides the manifest value. Absent => empty map => behavior
    /// identical to pre-P19.2 manifests.
    #[serde(default)]
    pub theme_tokens: std::collections::BTreeMap<String, String>,
    /// OPTIONAL (additive, protocol v1-compatible): passive `deferred`
    /// lifecycle declarations (Task 20, spec 7.5). Absent => legacy eager
    /// lifecycle, byte-for-byte compatible. Present declarations are
    /// trusted local manifest expectations validated BEFORE any spawn and
    /// matched EXACTLY against runtime initialize declarations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<super::lifecycle::DeferredDeclarations>,
}

/// Per-host-triple prebuilt distribution asset for an extension. Lives
/// inside [`ExtensionManifest::prebuilt`]. When a matching entry exists
/// for the current host, the installer fetches `url`, verifies its
/// SHA-256 against `sha256`, and extracts it into the plugin install
/// directory — letting users skip a (potentially slow) source build.
///
/// The archive is expected to lay out files relative to the plugin root
/// such that [`ExtensionManifest::command`] resolves after extraction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrebuiltAsset {
    /// HTTPS URL of the archive (`.tar.gz` or `.zip`). The installer
    /// refuses non-`https://` schemes and `file://` (except in tests
    /// gated by `cfg(test)`).
    pub url: String,
    /// Hex-encoded SHA-256 of the archive bytes; **required**. The
    /// installer aborts and surfaces an error if the downloaded bytes
    /// don't match — same model as the existing marketplace
    /// `checksum_value` for plugin sources.
    pub sha256: String,
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
        // Review fix A1/A2: full deferred policy — block bounds PLUS
        // permission coupling (deferred tools => 'tools.register',
        // deferred providers => 'providers.register') and hook-lifecycle
        // coupling (lifecycle 'hook' => at least one hook subscription).
        // Runs before any spawn or catalog registration.
        super::lifecycle::validate_manifest_deferred(self).map_err(|reason| {
            format!("Extension '{id}' deferred declarations invalid: {reason}")
        })?;
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
                "tools.register"
                    | "providers.register"
                    | "memory.read"
                    | "memory.write"
                    | "config.write"
                    | "config.subscribe"
                    | "audio.input"
                    | "audio.output"
            )
        });
        if self.hooks.is_empty() && !has_capability_permission {
            return Err(format!("Extension '{}' must subscribe to at least one hook or request a registration permission", id));
        }

        for (token, value) in &self.theme_tokens {
            if token.trim().is_empty()
                || token.contains('.')
                || token.contains('/')
                || token.contains('\\')
                || token.contains(char::is_whitespace)
            {
                return Err(format!(
                    "Extension '{}' theme token '{}' is invalid: token names must be non-empty and must not contain dots, slashes, or spaces",
                    id, token,
                ));
            }
            if !is_hex_color(value) {
                return Err(format!(
                    "Extension '{}' theme token '{}' has invalid color '{}': expected '#rgb' or '#rrggbb' hex",
                    id, token, value,
                ));
            }
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

/// True when `value` is a `#rgb` or `#rrggbb` hex color literal. Used to
/// fail-closed on malformed `theme_tokens` values at manifest validation
/// (before the extension process is spawned) instead of silently rendering
/// nothing.
fn is_hex_color(value: &str) -> bool {
    let hex = match value.trim().strip_prefix('#') {
        Some(h) => h,
        None => return false,
    };
    matches!(hex.len(), 3 | 6) && hex.chars().all(|c| c.is_ascii_hexdigit())
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
        let input = event
            .tool_input
            .as_ref()
            .unwrap_or(&serde_json::Value::Null);
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
        assert_eq!(
            entry.default,
            Some(serde_json::Value::String("auto".to_string()))
        );
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
            theme_tokens: Default::default(),
            deferred: None,
            protocol_version: 999,
            runtime: ExtensionRuntime::Process,
            command: "ext".to_string(),
            setup: None,
            prebuilt: ::std::collections::HashMap::new(),
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
            theme_tokens: Default::default(),
            deferred: None,
            protocol_version: 1,
            runtime: ExtensionRuntime::Process,
            command: "ext".to_string(),
            setup: None,
            prebuilt: ::std::collections::HashMap::new(),
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
            theme_tokens: Default::default(),
            deferred: None,
            protocol_version: 1,
            runtime: ExtensionRuntime::Process,
            command: "ext".to_string(),
            setup: None,
            prebuilt: ::std::collections::HashMap::new(),
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
            theme_tokens: Default::default(),
            deferred: None,
            protocol_version: 1,
            runtime: ExtensionRuntime::Process,
            command: "my-ext".to_string(),
            setup: None,
            prebuilt: ::std::collections::HashMap::new(),
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
        let equals_but_missing_contains =
            crate::extensions::hooks::events::HookEvent::before_tool_call(
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

    #[test]
    fn extension_manifest_defaults_prebuilt_to_empty_when_absent() {
        // Older manifests without `prebuilt` must still parse cleanly.
        let json = r#"{
            "runtime": "process",
            "command": "bin/ext"
        }"#;
        let m: ExtensionManifest = serde_json::from_str(json).unwrap();
        assert!(m.prebuilt.is_empty());
        assert!(m.setup.is_none());
    }

    #[test]
    fn extension_manifest_round_trips_prebuilt_assets() {
        let json = r#"{
            "runtime": "process",
            "command": "bin/ext",
            "prebuilt": {
                "linux-x86_64": {
                    "url": "https://example.com/ext-linux-x86_64.tar.gz",
                    "sha256": "abc123"
                },
                "darwin-arm64": {
                    "url": "https://example.com/ext-darwin-arm64.tar.gz",
                    "sha256": "def456"
                }
            }
        }"#;
        let m: ExtensionManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.prebuilt.len(), 2);
        let linux = m.prebuilt.get("linux-x86_64").expect("linux entry");
        assert_eq!(linux.url, "https://example.com/ext-linux-x86_64.tar.gz");
        assert_eq!(linux.sha256, "abc123");
        // Round-trip
        let back = serde_json::to_value(&m).unwrap();
        assert_eq!(
            back["prebuilt"]["darwin-arm64"]["sha256"],
            serde_json::Value::String("def456".to_string())
        );
    }

    #[test]
    fn prebuilt_asset_requires_both_url_and_sha256() {
        // Missing sha256 must error — no silent acceptance of unverified assets.
        let json = r#"{ "url": "https://example.com/x.tar.gz" }"#;
        let res: Result<PrebuiltAsset, _> = serde_json::from_str(json);
        assert!(
            res.is_err(),
            "PrebuiltAsset without sha256 must fail to parse"
        );
    }

    // ── Theme tokens (P19.2, additive-optional per STABILITY.md §1) ─────────

    #[test]
    fn theme_tokens_absent_defaults_to_empty_and_validates() {
        // The stability guarantee in one test: a pre-P19.2 manifest (no
        // theme_tokens key) parses to an empty map and passes validation
        // exactly as before — additive optional, no protocol bump.
        let json = r#"{
            "runtime": "process",
            "command": "my-ext",
            "permissions": ["tools.register"]
        }"#;
        let m: ExtensionManifest = serde_json::from_str(json).unwrap();
        assert!(
            m.theme_tokens.is_empty(),
            "absent theme_tokens must default to empty"
        );
        assert!(
            m.validate("legacy-ext").is_ok(),
            "legacy manifests must validate unchanged"
        );
    }

    #[test]
    fn theme_tokens_deserialize_and_validate() {
        let json = r##"{
            "runtime": "process",
            "command": "my-ext",
            "permissions": ["tools.register"],
            "theme_tokens": { "accent": "#22d3ee", "warn": "#fa0" }
        }"##;
        let m: ExtensionManifest = serde_json::from_str(json).unwrap();
        assert_eq!(
            m.theme_tokens.get("accent").map(String::as_str),
            Some("#22d3ee")
        );
        assert_eq!(m.theme_tokens.get("warn").map(String::as_str), Some("#fa0"));
        assert!(m.validate("themed-ext").is_ok());
    }

    #[test]
    fn theme_tokens_reject_bad_token_names() {
        for bad in ["with.dot", "with/slash", "with space", ""] {
            let json = format!(
                r##"{{
                    "runtime": "process",
                    "command": "my-ext",
                    "permissions": ["tools.register"],
                    "theme_tokens": {{ "{bad}": "#ffffff" }}
                }}"##
            );
            let m: ExtensionManifest = serde_json::from_str(&json).unwrap();
            let err = m.validate("bad-ext").unwrap_err();
            assert!(
                err.contains("theme token"),
                "'{bad}' must be rejected: {err}"
            );
        }
    }

    #[test]
    fn theme_tokens_reject_non_hex_colors() {
        for bad in ["red", "#12345", "#gggggg", "22d3ee", "#"] {
            let json = format!(
                r#"{{
                    "runtime": "process",
                    "command": "my-ext",
                    "permissions": ["tools.register"],
                    "theme_tokens": {{ "accent": "{bad}" }}
                }}"#
            );
            let m: ExtensionManifest = serde_json::from_str(&json).unwrap();
            let err = m.validate("bad-ext").unwrap_err();
            assert!(
                err.contains("invalid color"),
                "'{bad}' must be rejected: {err}"
            );
        }
    }

    #[test]
    fn is_hex_color_accepts_short_and_long_forms() {
        assert!(is_hex_color("#22d3ee"));
        assert!(is_hex_color("#fa0"));
        assert!(is_hex_color(" #ffffff "));
        assert!(!is_hex_color("#22d3e"));
        assert!(!is_hex_color("blue"));
    }
}
