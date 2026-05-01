//! Plugin update manifest diff helpers.

use crate::skills::manifest::{ManifestCommand, PluginManifest};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PluginUpdateDiff {
    pub version_change: Option<(Option<String>, Option<String>)>,
    pub added_permissions: Vec<String>,
    pub removed_permissions: Vec<String>,
    pub added_hooks: Vec<String>,
    pub removed_hooks: Vec<String>,
    pub extension_command_change: Option<(Option<String>, Option<String>)>,
    pub added_config_keys: Vec<String>,
    pub removed_config_keys: Vec<String>,
    pub added_commands: Vec<String>,
    pub removed_commands: Vec<String>,
}

impl PluginUpdateDiff {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some((old, new)) = &self.version_change {
            lines.push(format!("version: {} -> {}", old.as_deref().unwrap_or("unspecified"), new.as_deref().unwrap_or("unspecified")));
        }
        push_list(&mut lines, "added permissions", &self.added_permissions);
        push_list(&mut lines, "removed permissions", &self.removed_permissions);
        push_list(&mut lines, "added hooks", &self.added_hooks);
        push_list(&mut lines, "removed hooks", &self.removed_hooks);
        if let Some((old, new)) = &self.extension_command_change {
            lines.push(format!("extension command: {} -> {}", old.as_deref().unwrap_or("none"), new.as_deref().unwrap_or("none")));
        }
        push_list(&mut lines, "added config keys", &self.added_config_keys);
        push_list(&mut lines, "removed config keys", &self.removed_config_keys);
        push_list(&mut lines, "added commands", &self.added_commands);
        push_list(&mut lines, "removed commands", &self.removed_commands);
        if lines.is_empty() {
            lines.push("no manifest capability changes detected".to_string());
        }
        lines
    }
}

fn push_list(lines: &mut Vec<String>, label: &str, values: &[String]) {
    if !values.is_empty() {
        lines.push(format!("{}: {}", label, values.join(", ")));
    }
}

pub fn diff_plugin_manifests(old: &PluginManifest, new: &PluginManifest) -> PluginUpdateDiff {
    let mut diff = PluginUpdateDiff::default();
    if old.version != new.version {
        diff.version_change = Some((old.version.clone(), new.version.clone()));
    }

    let old_permissions = old.extension.as_ref().map(|e| e.permissions.clone()).unwrap_or_default();
    let new_permissions = new.extension.as_ref().map(|e| e.permissions.clone()).unwrap_or_default();
    diff.added_permissions = added(&old_permissions, &new_permissions);
    diff.removed_permissions = added(&new_permissions, &old_permissions);

    let old_hooks = old.extension.as_ref().map(hook_names).unwrap_or_default();
    let new_hooks = new.extension.as_ref().map(hook_names).unwrap_or_default();
    diff.added_hooks = added(&old_hooks, &new_hooks);
    diff.removed_hooks = added(&new_hooks, &old_hooks);

    let old_ext_cmd = old.extension.as_ref().map(extension_command);
    let new_ext_cmd = new.extension.as_ref().map(extension_command);
    if old_ext_cmd != new_ext_cmd {
        diff.extension_command_change = Some((old_ext_cmd, new_ext_cmd));
    }

    let old_config = old.extension.as_ref().map(config_keys).unwrap_or_default();
    let new_config = new.extension.as_ref().map(config_keys).unwrap_or_default();
    diff.added_config_keys = added(&old_config, &new_config);
    diff.removed_config_keys = added(&new_config, &old_config);

    let old_commands = command_names(&old.commands);
    let new_commands = command_names(&new.commands);
    diff.added_commands = added(&old_commands, &new_commands);
    diff.removed_commands = added(&new_commands, &old_commands);

    diff
}

fn added(old: &[String], new: &[String]) -> Vec<String> {
    let mut out: Vec<String> = new.iter().filter(|v| !old.contains(v)).cloned().collect();
    out.sort();
    out
}

fn hook_names(ext: &crate::extensions::manifest::ExtensionManifest) -> Vec<String> {
    let mut names: Vec<String> = ext.hooks.iter().map(|h| h.hook.clone()).collect();
    names.sort();
    names.dedup();
    names
}

fn config_keys(ext: &crate::extensions::manifest::ExtensionManifest) -> Vec<String> {
    let mut keys: Vec<String> = ext.config.iter().map(|c| c.key.clone()).collect();
    keys.sort();
    keys.dedup();
    keys
}

fn extension_command(ext: &crate::extensions::manifest::ExtensionManifest) -> String {
    if ext.args.is_empty() {
        ext.command.clone()
    } else {
        format!("{} {}", ext.command, ext.args.join(" "))
    }
}

fn command_names(commands: &[ManifestCommand]) -> Vec<String> {
    let mut names: Vec<String> = commands
        .iter()
        .map(|command| match command {
            ManifestCommand::Shell(c) => c.name.clone(),
            ManifestCommand::ExtensionTool(c) => c.name.clone(),
            ManifestCommand::SkillPrompt(c) => c.name.clone(),
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(json: &str) -> PluginManifest {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn reports_permission_hook_command_config_and_version_changes() {
        let old = manifest(r#"{
          "name":"policy-test",
          "version":"0.1.0",
          "commands":[{"name":"old-cmd","command":"printf"}],
          "extension":{
            "protocol_version":1,
            "runtime":"process",
            "command":"python3",
            "args":["old.py"],
            "permissions":["tools.intercept"],
            "hooks":[{"hook":"before_tool_call"}],
            "config":[{"key":"endpoint"}]
          }
        }"#);
        let new = manifest(r#"{
          "name":"policy-test",
          "version":"0.2.0",
          "commands":[{"name":"new-cmd","command":"printf"}],
          "extension":{
            "protocol_version":1,
            "runtime":"process",
            "command":"python3",
            "args":["new.py"],
            "permissions":["tools.intercept","privacy.llm_content"],
            "hooks":[{"hook":"before_tool_call"},{"hook":"on_message_complete"}],
            "config":[{"key":"api_key"}]
          }
        }"#);

        let diff = diff_plugin_manifests(&old, &new);
        assert_eq!(diff.version_change, Some((Some("0.1.0".into()), Some("0.2.0".into()))));
        assert_eq!(diff.added_permissions, vec!["privacy.llm_content"]);
        assert_eq!(diff.added_hooks, vec!["on_message_complete"]);
        assert_eq!(diff.extension_command_change, Some((Some("python3 old.py".into()), Some("python3 new.py".into()))));
        assert_eq!(diff.added_config_keys, vec!["api_key"]);
        assert_eq!(diff.removed_config_keys, vec!["endpoint"]);
        assert_eq!(diff.added_commands, vec!["new-cmd"]);
        assert_eq!(diff.removed_commands, vec!["old-cmd"]);
    }
}
