//! Slash command handling — dispatches /clear, /model, /system, etc.

use std::path::PathBuf;

use tokio::sync::mpsc;

use synaps_cli::{Session, list_sessions, find_session};
use synaps_cli::transport::Inbound;

use super::app::{App, ChatMessage};

/// All recognized slash commands.
pub(super) const ALL_COMMANDS: &[&str] = &[
    "clear", "model", "system", "thinking", "sessions",
    "resume", "theme", "gamba", "help", "quit", "exit",
];

/// Commands that work while streaming.
pub(super) const STREAMING_COMMANDS: &[&str] = &["gamba", "theme", "quit", "exit"];

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
}

/// Resolve a partial command prefix to a full command name.
/// Returns the input unchanged if no unique match.
pub(super) fn resolve_prefix(raw: &str, commands: &[&str]) -> String {
    if commands.contains(&raw) {
        return raw.to_string();
    }
    let matches: Vec<&&str> = commands.iter().filter(|c| c.starts_with(raw)).collect();
    if matches.len() == 1 {
        matches[0].to_string()
    } else {
        raw.to_string()
    }
}

/// Handle a slash command when NOT streaming.
pub(super) async fn handle_command(
    cmd: &str,
    arg: &str,
    app: &mut App,
    inbound_tx: &mpsc::UnboundedSender<Inbound>,
    system_prompt_path: Option<&PathBuf>,
) -> CommandAction {
    let _ = system_prompt_path; // reserved for future use
    match cmd {
        "clear" => {
            app.save_session().await;
            app.messages.clear();
            app.dirty = true;
            app.api_messages.clear();
            app.total_input_tokens = 0;
            app.total_output_tokens = 0;
            app.total_cache_read_tokens = 0;
            app.total_cache_creation_tokens = 0;
            app.session_cost = 0.0;
            app.input_tokens = 0;
            app.output_tokens = 0;
            app.session = Session::new(&app.model_name, &app.thinking_level, None);
            // Tell the driver to clear too
            let _ = inbound_tx.send(Inbound::Command { name: "clear".into(), args: String::new() });
            app.push_msg(ChatMessage::System("new session started".to_string()));
        }
        "model" => {
            if arg.is_empty() {
                app.push_msg(ChatMessage::System(
                    format!("current model: {}", app.model_name)
                ));
            } else {
                app.model_name = arg.to_string();
                let _ = inbound_tx.send(Inbound::Command { name: "model".into(), args: arg.to_string() });
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
                // System prompt save is local — we don't have the prompt text anymore.
                // Just inform the user to use /system show to see the current prompt.
                app.push_msg(ChatMessage::System(
                    "system prompt save not available in bus mode — prompt is managed by the driver".to_string()
                ));
            } else if arg == "show" {
                // Ask the driver, but for now just show what we know
                let _ = inbound_tx.send(Inbound::Command { name: "system".into(), args: "show".to_string() });
                app.push_msg(ChatMessage::System("(queried driver for system prompt)".to_string()));
            } else {
                let _ = inbound_tx.send(Inbound::Command { name: "system".into(), args: arg.to_string() });
                app.push_msg(ChatMessage::System(
                    "system prompt updated".to_string()
                ));
            }
        }
        "thinking" => {
            match arg {
                "low" | "medium" | "med" | "high" | "xhigh" => {
                    let _ = inbound_tx.send(Inbound::Command { name: "thinking".into(), args: arg.to_string() });
                    // Update local display
                    let level = match arg {
                        "low" => "low",
                        "medium" | "med" => "medium",
                        "high" => "high",
                        "xhigh" => "xhigh",
                        _ => arg,
                    };
                    app.thinking_level = level.to_string();
                    app.push_msg(ChatMessage::System(
                        format!("thinking set to: {}", level)
                    ));
                }
                "" => {
                    app.push_msg(ChatMessage::System(
                        format!("thinking: {}", app.thinking_level)
                    ));
                }
                _ => {
                    app.push_msg(ChatMessage::Error(
                        "usage: /thinking low|medium|high|xhigh".to_string()
                    ));
                }
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
                        // Update driver's model/system
                        let _ = inbound_tx.send(Inbound::Command { name: "model".into(), args: session.model.clone() });
                        if let Some(ref sp) = session.system_prompt {
                            let _ = inbound_tx.send(Inbound::Command { name: "system".into(), args: sp.clone() });
                        }
                        app.model_name = session.model.clone();
                        app.save_session().await;
                        let old_id = app.session.id.clone();
                        app.messages.clear();
                        app.dirty = true;
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
                "/model [name] — show or set model",
                "/system <prompt|show|save> — system prompt",
                "/thinking [low|medium|high|xhigh] — thinking budget",
                "/sessions — list saved sessions",
                "/resume <id> — switch to a different session",
                "/help — show this",
                "/theme — list available themes",
                "/gamba — open the casino 🎰",
            ];
            for line in help_lines {
                app.push_msg(ChatMessage::System(line.to_string()));
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
        _ => {
            app.push_msg(ChatMessage::Error(
                format!("unknown command: /{}", cmd)
            ));
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
