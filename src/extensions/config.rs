//! Config redaction helpers and diagnostics types for extensions.
//!
//! These helpers are used by `/extensions status` and similar UX to display
//! extension config without leaking secret values, and to mirror the
//! resolution order implemented in `manager::resolve_config`.

use super::manifest::ExtensionConfigEntry;

/// Redact a secret value for display. Always cosmetic; never returns the full value.
pub fn redact_secret_value(value: &str) -> String {
    let len = value.chars().count();
    if len == 0 {
        return String::new();
    }
    if len <= 3 {
        return "***".to_string();
    }
    let tail_len = if len <= 7 { 2 } else { 4 };
    let tail: String = value.chars().skip(len - tail_len).collect();
    format!("***{}", tail)
}

/// Compute the env override variable name for a given extension id + config key.
pub fn extension_env_var(extension_id: &str, key: &str) -> String {
    let id_upper = extension_id.replace('-', "_").to_ascii_uppercase();
    let key_upper = key.replace('-', "_").to_ascii_uppercase();
    format!("SYNAPS_EXTENSION_{}_{}", id_upper, key_upper)
}

/// Where a resolved config value originated from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// Resolved from the `SYNAPS_EXTENSION_<ID>_<KEY>` env override.
    EnvOverride(String),
    /// Resolved from the manifest-declared `secret_env` variable.
    SecretEnv(String),
    /// Resolved from the plugin-owned config file `plugins/<id>/config`.
    PluginConfig,
    /// Resolved from the deprecated persisted config key `extension.<id>.<key>`.
    LegacyConfigKey(String),
    /// Resolved from the manifest-declared default value.
    Default,
    /// No value available from any source.
    Missing,
}

/// Diagnostic status for a single config entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigEntryStatus {
    pub key: String,
    pub description: Option<String>,
    pub required: bool,
    pub source: ConfigSource,
    pub has_value: bool,
}

/// Classify a config entry against the same resolution order as
/// `manager::resolve_config`, using injected lookup closures so callers
/// (and tests) need not touch real `std::env` or persisted config.
pub fn classify_config_entry(
    extension_id: &str,
    entry: &ExtensionConfigEntry,
    env_lookup: &impl Fn(&str) -> Option<String>,
    plugin_config_lookup: &impl Fn(&str) -> Option<String>,
    legacy_config_lookup: &impl Fn(&str) -> Option<String>,
) -> ConfigEntryStatus {
    let env_var = extension_env_var(extension_id, &entry.key);
    let legacy_config_key = format!("extension.{}.{}", extension_id, entry.key);

    let source = if env_lookup(&env_var).is_some() {
        ConfigSource::EnvOverride(env_var)
    } else if let Some(secret_env) = entry.secret_env.as_ref() {
        if env_lookup(secret_env).is_some() {
            ConfigSource::SecretEnv(secret_env.clone())
        } else if plugin_config_lookup(&entry.key).is_some() {
            ConfigSource::PluginConfig
        } else if legacy_config_lookup(&legacy_config_key).is_some() {
            ConfigSource::LegacyConfigKey(legacy_config_key)
        } else if entry.default.is_some() {
            ConfigSource::Default
        } else {
            ConfigSource::Missing
        }
    } else if plugin_config_lookup(&entry.key).is_some() {
        ConfigSource::PluginConfig
    } else if legacy_config_lookup(&legacy_config_key).is_some() {
        ConfigSource::LegacyConfigKey(legacy_config_key)
    } else if entry.default.is_some() {
        ConfigSource::Default
    } else {
        ConfigSource::Missing
    };

    let has_value = !matches!(source, ConfigSource::Missing);

    ConfigEntryStatus {
        key: entry.key.clone(),
        description: entry.description.clone(),
        required: entry.required,
        source,
        has_value,
    }
}

/// Aggregated config diagnostics for a single extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionConfigDiagnostics {
    pub extension_id: String,
    pub entries: Vec<ConfigEntryStatus>,
    /// Provider-level required keys that aren't satisfied by any entry above.
    /// Each item is `(provider_id, missing_key)`.
    pub provider_missing: Vec<(String, String)>,
}

/// Compute diagnostics for an extension by combining manifest config classification
/// with provider-level `config_schema.required` checks. Lookups are injected so
/// the function is fully testable without touching real env or persisted config.
pub fn diagnose_extension_config(
    extension_id: &str,
    manifest_config: &[ExtensionConfigEntry],
    provider_required: &[(String, Vec<String>)],
    env_lookup: &impl Fn(&str) -> Option<String>,
    plugin_config_lookup: &impl Fn(&str) -> Option<String>,
    legacy_config_lookup: &impl Fn(&str) -> Option<String>,
) -> ExtensionConfigDiagnostics {
    let entries: Vec<ConfigEntryStatus> = manifest_config
        .iter()
        .map(|entry| {
            classify_config_entry(
                extension_id,
                entry,
                env_lookup,
                plugin_config_lookup,
                legacy_config_lookup,
            )
        })
        .collect();

    let mut provider_missing: Vec<(String, String)> = Vec::new();
    for (provider_id, required_keys) in provider_required {
        for key in required_keys {
            let satisfied = entries
                .iter()
                .any(|status| status.key == *key && status.has_value);
            if !satisfied {
                provider_missing.push((provider_id.clone(), key.clone()));
            }
        }
    }

    ExtensionConfigDiagnostics {
        extension_id: extension_id.to_string(),
        entries,
        provider_missing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn empty_lookup(_: &str) -> Option<String> {
        None
    }

    fn entry(key: &str) -> ExtensionConfigEntry {
        ExtensionConfigEntry {
            key: key.to_string(),
            description: None,
            required: false,
            default: None,
            secret_env: None,
        }
    }

    #[test]
    fn redact_empty() {
        assert_eq!(redact_secret_value(""), "");
    }

    #[test]
    fn redact_short() {
        assert_eq!(redact_secret_value("a"), "***");
        assert_eq!(redact_secret_value("abc"), "***");
    }

    #[test]
    fn redact_medium() {
        assert_eq!(redact_secret_value("abcd"), "***cd");
        assert_eq!(redact_secret_value("abc1234"), "***34");
    }

    #[test]
    fn redact_long() {
        assert_eq!(redact_secret_value("abc12345"), "***2345");
        assert_eq!(
            redact_secret_value("abcdefghijklmnopqrst"),
            "***qrst"
        );
        // sanity: never contains the full value beyond the tail
        let s = redact_secret_value("supersecretvalue1234");
        assert!(s.starts_with("***"));
        assert!(!s.contains("supersecret"));
    }

    #[test]
    fn env_var_uppercases_and_replaces_dashes() {
        assert_eq!(
            extension_env_var("my-ext", "api-key"),
            "SYNAPS_EXTENSION_MY_EXT_API_KEY"
        );
    }

    #[test]
    fn classify_env_override() {
        let e = entry("api-key");
        let env = |k: &str| {
            if k == "SYNAPS_EXTENSION_MY_EXT_API_KEY" {
                Some("v".to_string())
            } else {
                None
            }
        };
        let status = classify_config_entry("my-ext", &e, &env, &empty_lookup, &empty_lookup);
        assert_eq!(
            status.source,
            ConfigSource::EnvOverride("SYNAPS_EXTENSION_MY_EXT_API_KEY".to_string())
        );
        assert!(status.has_value);
    }

    #[test]
    fn classify_secret_env() {
        let mut e = entry("api-key");
        e.secret_env = Some("MY_PROVIDER_KEY".to_string());
        let env = |k: &str| {
            if k == "MY_PROVIDER_KEY" {
                Some("v".to_string())
            } else {
                None
            }
        };
        let status = classify_config_entry("my-ext", &e, &env, &empty_lookup, &empty_lookup);
        assert_eq!(
            status.source,
            ConfigSource::SecretEnv("MY_PROVIDER_KEY".to_string())
        );
        assert!(status.has_value);
    }

    #[test]
    fn classify_plugin_config() {
        let e = entry("api-key");
        let plugin = |k: &str| {
            if k == "api-key" { Some("v".to_string()) } else { None }
        };
        let status = classify_config_entry("my-ext", &e, &empty_lookup, &plugin, &empty_lookup);
        assert_eq!(status.source, ConfigSource::PluginConfig);
        assert!(status.has_value);
    }

    #[test]
    fn classify_config_key() {
        let e = entry("api-key");
        let cfg = |k: &str| {
            if k == "extension.my-ext.api-key" {
                Some("v".to_string())
            } else {
                None
            }
        };
        let status = classify_config_entry("my-ext", &e, &empty_lookup, &empty_lookup, &cfg);
        assert_eq!(
            status.source,
            ConfigSource::LegacyConfigKey("extension.my-ext.api-key".to_string())
        );
        assert!(status.has_value);
    }

    #[test]
    fn classify_default() {
        let mut e = entry("region");
        e.default = Some(Value::String("us-east-1".to_string()));
        let status = classify_config_entry("my-ext", &e, &empty_lookup, &empty_lookup, &empty_lookup);
        assert_eq!(status.source, ConfigSource::Default);
        assert!(status.has_value);
    }

    #[test]
    fn classify_missing() {
        let mut e = entry("api-key");
        e.required = true;
        let status = classify_config_entry("my-ext", &e, &empty_lookup, &empty_lookup, &empty_lookup);
        assert_eq!(status.source, ConfigSource::Missing);
        assert!(!status.has_value);
        assert!(status.required);
    }

    #[test]
    fn env_override_wins_over_all() {
        let mut e = entry("api-key");
        e.secret_env = Some("MY_PROVIDER_KEY".to_string());
        e.default = Some(Value::String("d".to_string()));
        let env = |k: &str| Some(format!("env-{}", k));
        let cfg = |_: &str| Some("cfg".to_string());
        let status = classify_config_entry("my-ext", &e, &env, &empty_lookup, &cfg);
        assert!(matches!(status.source, ConfigSource::EnvOverride(_)));
    }

    #[test]
    fn secret_env_wins_over_config_and_default() {
        let mut e = entry("api-key");
        e.secret_env = Some("MY_PROVIDER_KEY".to_string());
        e.default = Some(Value::String("d".to_string()));
        let env = |k: &str| {
            if k == "MY_PROVIDER_KEY" {
                Some("s".to_string())
            } else {
                None
            }
        };
        let cfg = |_: &str| Some("cfg".to_string());
        let status = classify_config_entry("my-ext", &e, &env, &empty_lookup, &cfg);
        assert_eq!(
            status.source,
            ConfigSource::SecretEnv("MY_PROVIDER_KEY".to_string())
        );
    }

    #[test]
    fn config_key_wins_over_default() {
        let mut e = entry("region");
        e.default = Some(Value::String("us-east-1".to_string()));
        let cfg = |k: &str| {
            if k == "extension.my-ext.region" {
                Some("eu-west-1".to_string())
            } else {
                None
            }
        };
        let status = classify_config_entry("my-ext", &e, &empty_lookup, &empty_lookup, &cfg);
        assert!(matches!(status.source, ConfigSource::LegacyConfigKey(_)));
    }

    #[test]
    fn default_only_when_no_env_or_config() {
        let mut e = entry("region");
        e.default = Some(Value::String("us-east-1".to_string()));
        let status = classify_config_entry("my-ext", &e, &empty_lookup, &empty_lookup, &empty_lookup);
        assert_eq!(status.source, ConfigSource::Default);
    }

    #[test]
    fn diagnose_empty_manifest_no_providers() {
        let diag = diagnose_extension_config(
            "my-ext",
            &[],
            &[],
            &empty_lookup,
            &empty_lookup,
            &empty_lookup,
        );
        assert_eq!(diag.extension_id, "my-ext");
        assert!(diag.entries.is_empty());
        assert!(diag.provider_missing.is_empty());
    }

    #[test]
    fn diagnose_entry_with_default_resolves() {
        let mut e = entry("region");
        e.default = Some(Value::String("us-east-1".to_string()));
        let diag = diagnose_extension_config(
            "my-ext",
            std::slice::from_ref(&e),
            &[],
            &empty_lookup,
            &empty_lookup,
            &empty_lookup,
        );
        assert_eq!(diag.entries.len(), 1);
        assert_eq!(diag.entries[0].source, ConfigSource::Default);
        assert!(diag.entries[0].has_value);
        assert!(diag.provider_missing.is_empty());
    }

    #[test]
    fn diagnose_provider_requires_undeclared_key() {
        let diag = diagnose_extension_config(
            "my-ext",
            &[],
            &[("p".to_string(), vec!["api-key".to_string()])],
            &empty_lookup,
            &empty_lookup,
            &empty_lookup,
        );
        assert_eq!(
            diag.provider_missing,
            vec![("p".to_string(), "api-key".to_string())]
        );
    }

    #[test]
    fn diagnose_provider_required_key_resolved_via_env() {
        let mut e = entry("api-key");
        e.required = true;
        let env = |k: &str| {
            if k == "SYNAPS_EXTENSION_MY_EXT_API_KEY" {
                Some("v".to_string())
            } else {
                None
            }
        };
        let diag = diagnose_extension_config(
            "my-ext",
            std::slice::from_ref(&e),
            &[("p".to_string(), vec!["api-key".to_string()])],
            &env,
            &empty_lookup,
            &empty_lookup,
        );
        assert!(diag.entries[0].has_value);
        assert!(diag.provider_missing.is_empty());
    }

    #[test]
    fn diagnose_provider_required_key_declared_but_missing() {
        let mut e = entry("api-key");
        e.required = true;
        let diag = diagnose_extension_config(
            "my-ext",
            std::slice::from_ref(&e),
            &[("p".to_string(), vec!["api-key".to_string()])],
            &empty_lookup,
            &empty_lookup,
            &empty_lookup,
        );
        assert!(!diag.entries[0].has_value);
        assert_eq!(
            diag.provider_missing,
            vec![("p".to_string(), "api-key".to_string())]
        );
    }
}
