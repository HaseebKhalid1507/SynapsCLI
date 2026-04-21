//! Slash command handling — dispatches /clear, /model, /system, etc.

use std::path::PathBuf;

use synaps_cli::{Runtime, Session, list_sessions, find_session};

use super::app::{App, ChatMessage};

/// All recognized built-in slash commands. Source of truth for the
/// built-in surface; the runtime merges this with discovered skills via
/// `CommandRegistry::all_commands()` for autocomplete and prefix resolution.
#[allow(dead_code)]

/// Commands that work while streaming.
pub(super) const STREAMING_COMMANDS: &[&str] = &["gamba", "theme", "quit", "exit"];

/// Merged list of built-ins + registered skill names (deduped, sorted).
/// Used for autocomplete and prefix resolution.
pub(super) fn all_commands_with_skills(
    registry: &synaps_cli::skills::registry::CommandRegistry,
) -> Vec<String> {
    registry.all_commands()
}

/// Convert a `&[&str]` slice into a `Vec<String>` for `resolve_prefix`.
pub(super) fn to_owned_commands(commands: &[&str]) -> Vec<String> {
    commands.iter().map(|s| s.to_string()).collect()
}

/// What the event loop should do after a command executes.
pub(super) enum CommandAction {
    /// Nothing special — continue the loop.
    None,
    /// Start a new stream with these API messages.
    #[allow(dead_code)] StartStream,
    /// Trigger the quit animation.
    Quit,
    /// Launch the casino (requires dropping/recreating EventStream).
    LaunchGamba,
    /// Open the /settings modal.
    OpenSettings,
    /// Open the /plugins modal.
    OpenPlugins,
    /// Force-reload registered plugins (for `/plugins reload`).
    ReloadPlugins,
    /// Synthesize load_skill tool-result + user message, then start stream.
    LoadSkill {
        skill: std::sync::Arc<synaps_cli::skills::LoadedSkill>,
        arg: String,
    },
    /// Compact the conversation history into a summary.
    Compact {
        custom_instructions: Option<String>,
    },
    /// Show the session compaction chain.
    Chain,
}

/// Resolve a partial command prefix to a full command name.
/// Returns the input unchanged if no unique match.
pub(super) fn resolve_prefix(raw: &str, commands: &[String]) -> String {
    if commands.iter().any(|c| c == raw) {
        return raw.to_string();
    }
    let matches: Vec<&String> = commands.iter().filter(|c| c.starts_with(raw)).collect();
    if matches.len() == 1 {
        matches[0].clone()
    } else {
        raw.to_string()
    }
}

/// Handle a slash command when NOT streaming.
pub(super) async fn handle_command(
    cmd: &str,
    arg: &str,
    app: &mut App,
    runtime: &mut Runtime,
    system_prompt_path: &PathBuf,
    registry: &std::sync::Arc<synaps_cli::skills::registry::CommandRegistry>,
) -> CommandAction {
    use synaps_cli::skills::registry::Resolution;
    match cmd {
        "clear" => {
            app.save_session().await;
            app.messages.clear();
            app.invalidate();
            app.api_messages.clear();
            app.total_input_tokens = 0;
            app.total_output_tokens = 0;
            app.total_cache_read_tokens = 0;
            app.total_cache_creation_tokens = 0;
            app.session_cost = 0.0;
            app.input_tokens = 0;
            app.output_tokens = 0;
            app.session = Session::new(runtime.model(), runtime.thinking_level(), runtime.system_prompt());
            app.push_msg(ChatMessage::System("new session started".to_string()));
        }
        "model" => {
            if arg.is_empty() {
                app.push_msg(ChatMessage::System(
                    format!("current model: {}", runtime.model())
                ));
            } else {
                runtime.set_model(arg.to_string());
                app.push_msg(ChatMessage::System(
                    format!("model set to: {}", arg)
                ));
            }
        }
        "system" => {
            if arg.is_empty() {
                app.push_msg(ChatMessage::System(
                    "usage: /system <prompt>  |  /system save  |  /system show".to_string()
                ));
            } else if arg == "save" {
                let _ = std::fs::create_dir_all(synaps_cli::config::get_active_config_dir());
                match std::fs::write(system_prompt_path, runtime.system_prompt().unwrap_or("")) {
                    Ok(_) => app.push_msg(ChatMessage::System(
                        format!("saved to {}", system_prompt_path.display())
                    )),
                    Err(e) => app.push_msg(ChatMessage::Error(
                        format!("failed to save: {}", e)
                    )),
                }
            } else if arg == "show" {
                let prompt = runtime.system_prompt().unwrap_or("(none)");
                app.push_msg(ChatMessage::System(prompt.to_string()));
            } else {
                runtime.set_system_prompt(arg.to_string());
                app.push_msg(ChatMessage::System(
                    "system prompt updated".to_string()
                ));
            }
        }
        "thinking" => {
            match arg {
                "low" => { runtime.set_thinking_budget(2048); }
                "medium" | "med" => { runtime.set_thinking_budget(4096); }
                "high" => { runtime.set_thinking_budget(16384); }
                "xhigh" => { runtime.set_thinking_budget(32768); }
                "" => {
                    app.push_msg(ChatMessage::System(
                        format!("thinking: {} ({})", runtime.thinking_level(), runtime.thinking_budget())
                    ));
                }
                _ => {
                    app.push_msg(ChatMessage::Error(
                        "usage: /thinking low|medium|high|xhigh".to_string()
                    ));
                }
            }
            if !arg.is_empty() && ["low", "medium", "med", "high", "xhigh"].contains(&arg) {
                app.push_msg(ChatMessage::System(
                    format!("thinking set to: {}", runtime.thinking_level())
                ));
            }
        }
        "sessions" => {
            match list_sessions() {
                Ok(sessions) if sessions.is_empty() => {
                    app.push_msg(ChatMessage::System("no saved sessions".to_string()));
                }
                Ok(sessions) => {
                    app.push_msg(ChatMessage::System(format!("{} session(s):", sessions.len())));
                    for s in sessions.iter().take(20) {
                        let title = if s.title.is_empty() { "(untitled)" } else { &s.title };
                        let active = if s.id == app.session.id { " *" } else { "" };
                        app.push_msg(ChatMessage::System(format!(
                            "  {} — {} [{}] ${:.4}{}",
                            &s.id, title, s.model, s.session_cost, active
                        )));
                    }
                }
                Err(e) => {
                    app.push_msg(ChatMessage::Error(format!("failed to list sessions: {}", e)));
                }
            }
        }
        "resume" => {
            if arg.is_empty() {
                app.push_msg(ChatMessage::System("usage: /resume <session_id>".to_string()));
            } else {
                match find_session(arg) {
                    Ok(session) => {
                        runtime.set_model(session.model.clone());
                        if let Some(ref sp) = session.system_prompt {
                            runtime.set_system_prompt(sp.clone());
                        }
                        app.save_session().await;
                        let old_id = app.session.id.clone();
                        app.messages.clear();
                        app.invalidate();
                        app.api_messages = session.api_messages.clone();
                        app.total_input_tokens = session.total_input_tokens;
                        app.total_output_tokens = session.total_output_tokens;
                        app.session_cost = session.session_cost;
                        super::rebuild_display_messages(&session.api_messages, app);
                        app.session = session;
                        app.push_msg(ChatMessage::System(
                            format!("switched from {} to {}", old_id, app.session.id)
                        ));
                    }
                    Err(e) => {
                        app.push_msg(ChatMessage::Error(format!("failed to load session: {}", e)));
                    }
                }
            }
        }
        "help" => {
            let help_lines = [
                "/clear — reset conversation",
                "/compact [focus] — summarize & compact conversation history",
                "/chain — show session compaction history",
                "/model [name] — show or set model",
                "/system <prompt|show|save> — system prompt",
                "/thinking [low|medium|high|xhigh] — thinking budget",
                "/sessions — list saved sessions",
                "/resume <id> — switch to a different session",
                "/help — show this",
                "/theme — list available themes",
                "/settings — open the settings menu",
                "/plugins — manage marketplaces and installed plugins",
                "/gamba — open the casino 🎰",
            ];
            for line in help_lines {
                app.push_msg(ChatMessage::System(line.to_string()));
            }
            let skills = registry.all_skills();
            if !skills.is_empty() {
                app.push_msg(ChatMessage::System(String::new()));
                app.push_msg(ChatMessage::System("## Skills".to_string()));
                let mut sorted = skills.clone();
                sorted.sort_by(|a, b| a.name.cmp(&b.name));
                for s in sorted {
                    let display = match &s.plugin {
                        Some(p) => format!("/{} ({}:{}) — {}", s.name, p, s.name, s.description),
                        None => format!("/{} — {}", s.name, s.description),
                    };
                    app.push_msg(ChatMessage::System(display));
                }
            }
        }
        "quit" | "exit" => {
            return CommandAction::Quit;
        }
        "theme" => {
            app.handle_theme_command(arg);
        }
        "gamba" => {
            return CommandAction::LaunchGamba;
        }
        "settings" => {
            return CommandAction::OpenSettings;
        }
        "plugins" => {
            if arg.trim() == "reload" {
                return CommandAction::ReloadPlugins;
            }
            return CommandAction::OpenPlugins;
        }
        "compact" => {
            return CommandAction::Compact {
                custom_instructions: if arg.is_empty() { None } else { Some(arg.to_string()) },
            };
        }
        "chain" => {
            return CommandAction::Chain;
        }
        _ => {
            match registry.resolve(cmd) {
                Resolution::Skill(skill) => {
                    return CommandAction::LoadSkill { skill, arg: arg.to_string() };
                }
                Resolution::Ambiguous(opts) => {
                    app.push_msg(ChatMessage::Error(format!(
                        "ambiguous command /{}; try one of: {}",
                        cmd,
                        opts.iter().map(|o| format!("/{}", o)).collect::<Vec<_>>().join(", ")
                    )));
                }
                Resolution::Builtin | Resolution::Unknown => {
                    app.push_msg(ChatMessage::Error(format!("unknown command: /{}", cmd)));
                }
            }
        }
    }
    CommandAction::None
}

/// Handle a slash command while streaming (limited set).
pub(super) fn handle_streaming_command(
    cmd: &str,
    full_input: &str,
    app: &mut App,
) -> CommandAction {
    match cmd {
        "gamba" => CommandAction::LaunchGamba,
        "theme" => {
            let arg = full_input[1..].split_once(' ').map(|x| x.1).unwrap_or("").trim();
            app.handle_theme_command(arg);
            CommandAction::None
        }
        "quit" | "exit" => CommandAction::Quit,
        _ => CommandAction::None, // unknown — handled by caller as steer/queue
    }
}

#[cfg(test)]
mod tests {
    use synaps_cli::skills::BUILTIN_COMMANDS as ALL_COMMANDS;

    #[test]
    fn plugins_is_in_all_commands() {
        assert!(ALL_COMMANDS.contains(&"plugins"));
    }
}
