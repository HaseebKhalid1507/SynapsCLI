//! Plugin permission/trust inspection helpers.

use crate::skills::Plugin;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginPermissionSummary {
    pub has_executable_extension: bool,
    pub has_setup_script: bool,
    pub permissions: Vec<String>,
    pub hooks: Vec<String>,
    pub config_keys: Vec<String>,
    pub command: Option<String>,
}

impl PluginPermissionSummary {
    pub fn is_empty(&self) -> bool {
        !self.has_executable_extension
            && !self.has_setup_script
            && self.permissions.is_empty()
            && self.hooks.is_empty()
            && self.config_keys.is_empty()
            && self.command.is_none()
    }

    pub fn lines(&self) -> Vec<String> {
        if self.is_empty() {
            return vec!["no executable extension or extension permissions declared".to_string()];
        }

        let mut lines = Vec::new();
        if self.has_executable_extension {
            lines.push("executable extension: yes".to_string());
        }
        if self.has_setup_script {
            lines.push("⚠ runs setup script on install".to_string());
        }
        if let Some(command) = &self.command {
            lines.push(format!("command: {}", command));
        }
        if self.permissions.is_empty() {
            lines.push("permissions: <none>".to_string());
        } else {
            lines.push(format!("permissions: {}", self.permissions.join(", ")));
        }
        if !self.hooks.is_empty() {
            lines.push(format!("hooks: {}", self.hooks.join(", ")));
        }
        if !self.config_keys.is_empty() {
            lines.push(format!("config: {}", self.config_keys.join(", ")));
        }
        lines
    }
}

pub fn summarize_plugin_permissions(plugin: &Plugin) -> PluginPermissionSummary {
    let Some(extension) = &plugin.extension else {
        return PluginPermissionSummary {
            has_executable_extension: false,
            has_setup_script: false,
            permissions: Vec::new(),
            hooks: Vec::new(),
            config_keys: Vec::new(),
            command: None,
        };
    };

    let mut permissions = extension.permissions.clone();
    permissions.sort();
    permissions.dedup();

    let hooks = extension
        .hooks
        .iter()
        .map(|hook| match &hook.tool {
            Some(tool) => format!("{}({})", hook.hook, tool),
            None => hook.hook.clone(),
        })
        .collect();

    let config_keys = extension
        .config
        .iter()
        .map(|entry| {
            if entry.required {
                format!("{} [required]", entry.key)
            } else {
                entry.key.clone()
            }
        })
        .collect();

    PluginPermissionSummary {
        has_executable_extension: true,
        has_setup_script: extension.setup.is_some(),
        permissions,
        hooks,
        config_keys,
        command: Some(
            format!("{} {}", extension.command, extension.args.join(" "))
                .trim()
                .to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::manifest::{
        ExtensionConfigEntry, ExtensionManifest, ExtensionRuntime, HookSubscription,
    };
    use std::path::PathBuf;

    fn plugin(extension: Option<ExtensionManifest>) -> Plugin {
        Plugin {
            name: "policy".to_string(),
            root: PathBuf::from("/tmp/policy"),
            marketplace: None,
            version: None,
            description: None,
            extension,
            manifest: None,
        }
    }

    /// Load one plugin from a `plugin.json` body, the way the install
    /// trust prompt does (`load_all` over the parent folder).
    fn loaded(name: &str, manifest_json: &str) -> Plugin {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(name).join(".synaps-plugin");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.json"), manifest_json).unwrap();
        let (mut plugins, _) = crate::skills::loader::load_all(&[root.path().to_path_buf()]);
        assert_eq!(plugins.len(), 1, "{name} must load");
        plugins.remove(0)
    }

    const SETUP_LINE: &str = "⚠ runs setup script on install";

    #[test]
    fn extension_setup_shows_the_setup_script_warning() {
        let p = loaded(
            "with-setup",
            r#"{"name":"with-setup","extension":{"protocol_version":1,"runtime":"process",
               "command":"bin/x","setup":"scripts/setup.sh","permissions":["config.subscribe"]}}"#,
        );
        let summary = summarize_plugin_permissions(&p);
        assert!(summary.has_setup_script);
        assert!(summary.lines().iter().any(|l| l == SETUP_LINE), "{:?}", summary.lines());
    }

    /// `provides.sidecar.setup` was the old slot; the block is ignored now,
    /// so a plugin with only that and no extension has no setup script.
    #[test]
    fn legacy_sidecar_setup_alone_shows_no_setup_script_warning() {
        let p = loaded(
            "legacy-setup",
            r#"{"name":"legacy-setup","provides":{"sidecar":{"command":"bin/x",
               "setup":"scripts/setup.sh","protocol_version":1}}}"#,
        );
        let summary = summarize_plugin_permissions(&p);
        assert!(!summary.has_setup_script);
        assert!(summary.lines().iter().all(|l| l != SETUP_LINE), "{:?}", summary.lines());
        assert!(summary.is_empty());
    }

    #[test]
    fn summary_is_empty_without_extension() {
        let summary = summarize_plugin_permissions(&plugin(None));
        assert!(summary.is_empty());
        assert_eq!(
            summary.lines(),
            vec!["no executable extension or extension permissions declared"]
        );
    }

    #[test]
    fn summary_lists_permissions_hooks_and_required_config() {
        let summary = summarize_plugin_permissions(&plugin(Some(ExtensionManifest {
            theme_tokens: Default::default(),
            deferred: None,
            protocol_version: 1,
            runtime: ExtensionRuntime::Process,
            command: "python3".to_string(),
            setup: None,
            prebuilt: ::std::collections::HashMap::new(),
            args: vec!["ext.py".to_string()],
            permissions: vec![
                "tools.intercept".to_string(),
                "privacy.llm_content".to_string(),
            ],
            hooks: vec![HookSubscription {
                hook: "before_tool_call".to_string(),
                tool: Some("bash".to_string()),
                matcher: None,
            }],
            config: vec![ExtensionConfigEntry {
                key: "api_key".to_string(),
                value_type: None,
                description: None,
                required: true,
                default: None,
                secret_env: Some("API_KEY".to_string()),
                host_context: None,
            }],
        })));

        assert!(summary.has_executable_extension);
        assert_eq!(
            summary.permissions,
            vec!["privacy.llm_content", "tools.intercept"]
        );
        assert_eq!(summary.hooks, vec!["before_tool_call(bash)"]);
        assert_eq!(summary.config_keys, vec!["api_key [required]"]);
        assert!(summary
            .lines()
            .iter()
            .any(|line| line.contains("permissions: privacy.llm_content, tools.intercept")));
    }
}
