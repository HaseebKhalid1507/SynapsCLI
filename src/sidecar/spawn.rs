//! Plugin-supplied sidecar spawn parameters.
//!
//! This RPC keeps bootstrap ownership inside the selected plugin. Core asks
//! `sidecar.spawn_args` and the plugin replies with the args to pass to its
//! binary plus optional metadata. The plugin is free to source those values
//! from anywhere it likes (its own config namespace, environment, hardware,
//! or generated defaults). Core never sees plugin-specific keys.
//!
//! ## Wire shape (`sidecar.spawn_args` response)
//!
//! ```json
//! {
//!   "args": ["--model-path", "/abs/path/to/model.bin", "--language", "en"],
//!   "language": "en"
//! }
//! ```
//!
//! Both fields are optional. A plugin that has no overrides at all
//! can simply return `{}` — core then falls back to manifest defaults
//! (the `provides.sidecar.model.default_path`, if any).

use serde::{Deserialize, Serialize};

/// Sidecar spawn parameters returned by the plugin's `sidecar.spawn_args` RPC.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct SidecarSpawnArgs {
    /// CLI arguments to pass to the sidecar binary.
    ///
    /// The plugin is responsible for resolving any tilde-expansion,
    /// path validation, and modality-specific knobs. Core treats this
    /// as opaque.
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional plugin-owned language hint. Core stores and forwards this
    /// value only where a plugin explicitly asks for it; the sidecar
    /// protocol does not prescribe language semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_full_payload() {
        let payload = r#"{
            "args": ["--model-path", "/m.bin", "--language", "en"],
            "language": "en"
        }"#;
        let parsed: SidecarSpawnArgs = serde_json::from_str(payload).unwrap();
        assert_eq!(parsed.args, vec!["--model-path", "/m.bin", "--language", "en"]);
        assert_eq!(parsed.language.as_deref(), Some("en"));
    }

    #[test]
    fn deserializes_empty_object_as_defaults() {
        let parsed: SidecarSpawnArgs = serde_json::from_str("{}").unwrap();
        assert!(parsed.args.is_empty());
        assert_eq!(parsed.language, None);
    }

    #[test]
    fn deserializes_args_only() {
        let parsed: SidecarSpawnArgs =
            serde_json::from_str(r#"{"args":["--mute"]}"#).unwrap();
        assert_eq!(parsed.args, vec!["--mute"]);
        assert_eq!(parsed.language, None);
    }

    #[test]
    fn round_trips_through_serde() {
        let original = SidecarSpawnArgs {
            args: vec!["--foo".into(), "bar".into()],
            language: Some("fr".into()),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: SidecarSpawnArgs = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn skip_serializing_none_language() {
        let original = SidecarSpawnArgs {
            args: vec![],
            language: None,
        };
        let json = serde_json::to_string(&original).unwrap();
        assert!(!json.contains("language"), "`language: None` should be omitted, got: {json}");
    }
}
