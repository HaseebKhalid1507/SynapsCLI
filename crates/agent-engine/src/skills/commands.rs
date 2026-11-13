//! Execution helper for plugin manifest commands.

use std::path::{Component, Path};
use std::process::Stdio;
use std::sync::Arc;

use serde_json::Value;

use crate::memory_backend::MemoryBinding;
use crate::session::types::ToolEnv;
use crate::skills::registry::{RegisteredPluginCommand, RegisteredPluginCommandBackend};
use crate::tools::{ToolCapabilities, ToolChannels, ToolLimits};
use crate::{ToolContext, ToolRegistry};

/// The chat a slash command is run for: whose command it is, and that
/// chat's own folder, memory binding and environment. A runtime hands out
/// its own ([`crate::Runtime::command_chat`]); under the daemon the
/// process's folder, project and environment are the daemon's, never a
/// chat's.
#[derive(Clone, Copy)]
pub struct CommandChat<'a> {
    /// The chat the command was typed in. `None` = no conversation.
    pub session_id: Option<&'a str>,
    /// The folder that chat runs in. `None` = the process folder.
    pub cwd: Option<&'a Path>,
    /// The chat's memory binding. Required: a tool the command runs is
    /// never left to work one out from the process folder.
    pub memory_backend: &'a MemoryBinding,
    /// The environment the chat's tools spawn with. `None` = the process
    /// environment (in-process hosts, where that is the chat's).
    pub env: Option<&'a ToolEnv>,
}

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
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|v| interpolate_slash_args(v, slash_args))
                .collect(),
        ),
        Value::Object(obj) => Value::Object(
            obj.into_iter()
                .map(|(k, v)| (k, interpolate_slash_args(v, slash_args)))
                .collect(),
        ),
        other => other,
    }
}

/// Run a plugin command that needs none of the chat's tools. `env` is the
/// environment a shell-backed command is spawned with: the session's, as
/// the chat's `bash` gets it; `None` = inherit the process environment. The
/// command still runs from the plugin's own folder.
pub async fn execute_plugin_command(
    command: &RegisteredPluginCommand,
    slash_args: &str,
    env: Option<&ToolEnv>,
) -> crate::Result<PluginCommandOutput> {
    match &command.backend {
        RegisteredPluginCommandBackend::Shell {
            command: executable,
            args,
        } => {
            validate_command_path(executable)?;

            let mut cmd = tokio::process::Command::new(executable);
            cmd.current_dir(&command.plugin_root)
                .args(args)
                .args(slash_words(slash_args))
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if let Some(env) = env {
                cmd.env_clear().envs(env.spawn_vars());
            }

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
                stdout: format!(
                    "Skill prompt for /{}:{} (skill: {})\n{}",
                    command.plugin, command.name, skill, text
                ),
                stderr: String::new(),
            })
        }
        RegisteredPluginCommandBackend::ExtensionTool { .. } => Err(crate::RuntimeError::Tool(
            "extension-backed plugin command requires execute_plugin_command_with_tools"
                .to_string(),
        )),
        RegisteredPluginCommandBackend::Interactive { .. } => Err(crate::RuntimeError::Tool(
            "interactive plugin command requires ExtensionManager::invoke_command".to_string(),
        )),
    }
}

/// [`execute_plugin_command`], plus the backend that needs the chat's tools:
/// an extension tool. `chat` is the chat the command was typed in: the
/// tool's `tool.call` carries its id and folder, the tool's context holds
/// its memory binding and environment, and a shell-backed command is
/// spawned with that environment.
pub async fn execute_plugin_command_with_tools(
    command: &RegisteredPluginCommand,
    slash_args: &str,
    tools: Arc<tokio::sync::RwLock<ToolRegistry>>,
    chat: CommandChat<'_>,
) -> crate::Result<PluginCommandOutput> {
    if let RegisteredPluginCommandBackend::ExtensionTool { tool, input } = &command.backend {
        let runtime_tool_name = format!("{}:{}", command.plugin, tool);
        let params = interpolate_slash_args(input.clone(), slash_args);
        let registry = tools.read().await;
        let tool = registry
            .get(&runtime_tool_name)
            .ok_or_else(|| {
                crate::RuntimeError::Tool(format!(
                    "extension tool '{}' is not registered",
                    runtime_tool_name
                ))
            })?
            .clone();
        drop(registry);
        let stdout = tool.execute(params, command_tool_context(chat)).await?;
        Ok(PluginCommandOutput {
            status: Some(0),
            stdout,
            stderr: String::new(),
        })
    } else {
        execute_plugin_command(command, slash_args, chat.env).await
    }
}

/// The context a slash command's tool runs in: whose call it is, with the
/// chat's memory binding and environment, and no other capability of the
/// chat's.
fn command_tool_context(chat: CommandChat<'_>) -> ToolContext {
    ToolContext {
        channels: ToolChannels {
            tx_delta: None,
            tx_events: None,
        },
        capabilities: ToolCapabilities {
            session_allow_all: None,
            launch_cancel: None,
            memory_backend: Some(chat.memory_backend.clone()),
            telemetry: None,
            tool_register_tx: None,
            session_manager: None,
            subagent_registry: None,
            event_queue: None,
            delegation_parent: None,
            codex_parent_plan: None,
            secret_prompt: None,
            orchestration: None,
            tool_activation: None,
            mcp_leases: None,
            extension_leases: None,
            cwd: chat.cwd.map(Path::to_path_buf),
            env: chat.env.cloned(),
            env_stripped: Vec::new(),
            env_warned: Default::default(),
            session_id: chat.session_id.map(str::to_string),
            hook_bus: None,
            continuation: None,
        },
        limits: ToolLimits {
            max_tool_output: 30_000,
            max_tool_buffer: 256 * 1024,
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
    use crate::Tool;
    use async_trait::async_trait;
    use std::path::PathBuf;

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

        let output = execute_plugin_command(&command, "extra", None).await.unwrap();

        assert_eq!(output.status, Some(0));
        assert_eq!(output.stdout, "cwd=. arg=extra");
    }

    #[tokio::test]
    async fn execute_plugin_command_rejects_parent_dir_command_path() {
        let root = tempdir();
        let command = cmd("../evil", vec![], root);

        let result = execute_plugin_command(&command, "", None).await;

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

        let output = execute_plugin_command(&command, "diff", None).await.unwrap();
        assert_eq!(output.status, Some(0));
        assert!(output.stdout.contains("Review: diff"));
    }

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "p:echo"
        }
        fn description(&self) -> &str {
            "echo"
        }
        fn parameters(&self) -> Value {
            serde_json::json!({"type":"object"})
        }
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
        let runtime = crate::Runtime::new_headless();

        let output =
            execute_plugin_command_with_tools(&command, "hello", registry, runtime.command_chat())
                .await
                .unwrap();

        assert_eq!(output.status, Some(0));
        assert_eq!(output.stdout, "echo hello");
    }
}
