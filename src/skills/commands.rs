//! Execution helper for plugin manifest commands.

use std::path::{Component, Path};
use std::process::Stdio;
use std::sync::Arc;

use serde_json::Value;

use crate::skills::registry::{RegisteredPluginCommand, RegisteredPluginCommandBackend};
use crate::tools::{ToolCapabilities, ToolChannels, ToolLimits};
use crate::{ToolContext, ToolRegistry};

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

fn interpolate_slash_args(value: Value, slash_args: &str) -> Value {
    match value {
        Value::String(s) if s == "${args}" => Value::String(slash_args.to_string()),
        Value::String(s) => Value::String(s.replace("${args}", slash_args)),
        Value::Array(items) => Value::Array(items.into_iter().map(|v| interpolate_slash_args(v, slash_args)).collect()),
        Value::Object(obj) => Value::Object(
            obj.into_iter()
                .map(|(k, v)| (k, interpolate_slash_args(v, slash_args)))
                .collect(),
        ),
        other => other,
    }
}

pub async fn execute_plugin_command(
    command: &RegisteredPluginCommand,
    slash_args: &str,
) -> crate::Result<PluginCommandOutput> {
    match &command.backend {
        RegisteredPluginCommandBackend::Shell { command: executable, args } => {
            validate_command_path(executable)?;

            let mut cmd = tokio::process::Command::new(executable);
            cmd.current_dir(&command.plugin_root)
                .args(args)
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
        RegisteredPluginCommandBackend::SkillPrompt { skill, prompt } => {
            let mut text = prompt.replace("${args}", slash_args);
            if !slash_args.trim().is_empty() && !prompt.contains("${args}") {
                text.push('\n');
                text.push_str(slash_args);
            }
            Ok(PluginCommandOutput {
                status: Some(0),
                stdout: format!("Skill prompt for /{}:{} (skill: {})\n{}", command.plugin, command.name, skill, text),
                stderr: String::new(),
            })
        }
        RegisteredPluginCommandBackend::ExtensionTool { .. } => Err(crate::RuntimeError::Tool(
            "extension-backed plugin command requires execute_plugin_command_with_tools".to_string(),
        )),
        RegisteredPluginCommandBackend::Interactive { .. } => Err(crate::RuntimeError::Tool(
            "interactive plugin command requires ExtensionManager::invoke_command".to_string(),
        )),
    }
}

pub async fn execute_plugin_command_with_tools(
    command: &RegisteredPluginCommand,
    slash_args: &str,
    tools: Arc<tokio::sync::RwLock<ToolRegistry>>,
) -> crate::Result<PluginCommandOutput> {
    if let RegisteredPluginCommandBackend::ExtensionTool { tool, input } = &command.backend {
        let runtime_tool_name = format!("{}:{}", command.plugin, tool);
        let params = interpolate_slash_args(input.clone(), slash_args);
        let registry = tools.read().await;
        let tool = registry.get(&runtime_tool_name).ok_or_else(|| {
            crate::RuntimeError::Tool(format!("extension tool '{}' is not registered", runtime_tool_name))
        })?.clone();
        drop(registry);
        let stdout = tool.execute(params, empty_tool_context()).await?;
        Ok(PluginCommandOutput { status: Some(0), stdout, stderr: String::new() })
    } else {
        execute_plugin_command(command, slash_args).await
    }
}

fn empty_tool_context() -> ToolContext {
    ToolContext {
        channels: ToolChannels { tx_delta: None, tx_events: None },
        capabilities: ToolCapabilities {
            watcher_exit_path: None,
            tool_register_tx: None,
            session_manager: None,
            subagent_registry: None,
            event_queue: None,
            secret_prompt: None,
        },
        limits: ToolLimits {
            max_tool_output: 30_000,
            bash_timeout: 30,
            bash_max_timeout: 300,
            subagent_timeout: 300,
        },
    }
}

fn slash_words(input: &str) -> Vec<String> {
    input.split_whitespace().map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::path::PathBuf;
    use crate::Tool;

    fn cmd(command: &str, args: Vec<&str>, root: PathBuf) -> RegisteredPluginCommand {
        RegisteredPluginCommand {
            plugin: "p".to_string(),
            name: "hello".to_string(),
            description: None,
            backend: RegisteredPluginCommandBackend::Shell {
                command: command.to_string(),
                args: args.into_iter().map(str::to_string).collect(),
            },
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

    #[tokio::test]
    async fn skill_prompt_command_outputs_prompt_with_args() {
        let command = RegisteredPluginCommand {
            plugin: "p".to_string(),
            name: "review".to_string(),
            description: None,
            backend: RegisteredPluginCommandBackend::SkillPrompt {
                skill: "reviewer".to_string(),
                prompt: "Review: ${args}".to_string(),
            },
            plugin_root: PathBuf::from("/tmp/p"),
        };

        let output = execute_plugin_command(&command, "diff").await.unwrap();
        assert_eq!(output.status, Some(0));
        assert!(output.stdout.contains("Review: diff"));
    }

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str { "p:echo" }
        fn description(&self) -> &str { "echo" }
        fn parameters(&self) -> Value { serde_json::json!({"type":"object"}) }
        async fn execute(&self, params: Value, _ctx: ToolContext) -> crate::Result<String> {
            Ok(format!("echo {}", params["text"].as_str().unwrap()))
        }
    }

    #[tokio::test]
    async fn extension_tool_command_executes_namespaced_registered_tool() {
        let command = RegisteredPluginCommand {
            plugin: "p".to_string(),
            name: "echo".to_string(),
            description: None,
            backend: RegisteredPluginCommandBackend::ExtensionTool {
                tool: "echo".to_string(),
                input: serde_json::json!({"text":"${args}"}),
            },
            plugin_root: PathBuf::from("/tmp/p"),
        };
        let registry = Arc::new(tokio::sync::RwLock::new(ToolRegistry::without_subagent()));
        registry.write().await.register(Arc::new(EchoTool));

        let output = execute_plugin_command_with_tools(&command, "hello", registry).await.unwrap();

        assert_eq!(output.status, Some(0));
        assert_eq!(output.stdout, "echo hello");
    }
}
