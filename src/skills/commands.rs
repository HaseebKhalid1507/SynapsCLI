//! Execution helper for plugin manifest commands.

use std::path::{Component, Path};
use std::process::Stdio;

use crate::skills::registry::RegisteredPluginCommand;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginCommandOutput {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

fn validate_command_path(command: &str) -> crate::Result<()> {
    let path = Path::new(command);
    if path.is_absolute() {
        return Err(crate::RuntimeError::Tool(
            "plugin command must be relative to plugin root or resolved from PATH".to_string(),
        ));
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(crate::RuntimeError::Tool(
            "plugin command may not contain '..' path components".to_string(),
        ));
    }
    Ok(())
}

pub async fn execute_plugin_command(
    command: &RegisteredPluginCommand,
    slash_args: &str,
) -> crate::Result<PluginCommandOutput> {
    validate_command_path(&command.command)?;

    let mut cmd = tokio::process::Command::new(&command.command);
    cmd.current_dir(&command.plugin_root)
        .args(&command.args)
        .args(slash_words(slash_args))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = cmd.output().await.map_err(|e| {
        crate::RuntimeError::Tool(format!(
            "failed to run plugin command /{}:{}: {}",
            command.plugin, command.name, e
        ))
    })?;

    Ok(PluginCommandOutput {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

fn slash_words(input: &str) -> Vec<String> {
    input.split_whitespace().map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cmd(command: &str, args: Vec<&str>, root: PathBuf) -> RegisteredPluginCommand {
        RegisteredPluginCommand {
            plugin: "p".to_string(),
            name: "hello".to_string(),
            description: None,
            command: command.to_string(),
            args: args.into_iter().map(str::to_string).collect(),
            plugin_root: root,
        }
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "synaps-plugin-cmd-test-{}-{}",
            std::process::id(),
            crate::epoch_millis()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn execute_plugin_command_runs_from_plugin_root_and_appends_slash_args() {
        let root = tempdir();
        let command = cmd("printf", vec!["cwd=%s arg=%s", "."], root);

        let output = execute_plugin_command(&command, "extra").await.unwrap();

        assert_eq!(output.status, Some(0));
        assert_eq!(output.stdout, "cwd=. arg=extra");
    }

    #[tokio::test]
    async fn execute_plugin_command_rejects_parent_dir_command_path() {
        let root = tempdir();
        let command = cmd("../evil", vec![], root);

        let result = execute_plugin_command(&command, "").await;

        assert!(result.is_err());
    }
}
