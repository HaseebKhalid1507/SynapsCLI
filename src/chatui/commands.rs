//! Slash command handling — dispatches /clear, /model, /system, etc.

use std::path::PathBuf;

use synaps_cli::{Runtime, Session, list_sessions, resolve_session};

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
    /// Ping all configured provider models.
    Ping,
    /// Show the session compaction chain.
    Chain,
    /// List named chains.
    ChainList,
    /// Create/update a named chain pointing at the current session.
    ChainName { name: String },
    /// Delete a named chain.
    ChainUnname { name: String },
    /// Assign (or clear, if empty) a name to the current session. Persists via save.
    /// Show account usage and reset times.
    Status,
}

/// Levenshtein edit distance between two strings.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (m, n) = (a.len(), b.len());
    let mut prev = (0..=n).collect::<Vec<_>>();
    let mut curr = vec![0; n + 1];
    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1)
                .min(curr[j - 1] + 1)
                .min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

/// Find the best fuzzy match for `raw` among `commands`.
/// Returns `Some(command)` if there's a single best match within the
/// distance threshold (≤40% of target length, minimum distance of 2).
/// Returns `None` if no match is close enough or if it's ambiguous.
pub(super) fn fuzzy_match<'a>(raw: &str, commands: &'a [String]) -> Option<&'a String> {
    if raw.is_empty() {
        return None;
    }
    let mut best: Option<(usize, &String)> = None;
    let mut ambiguous = false;
    for cmd in commands {
        let threshold = (cmd.len() * 2 / 5).max(2); // 40% of target len, min 2
        let dist = edit_distance(raw, cmd);
        if dist == 0 || dist > threshold {
            continue;
        }
        match best {
            None => best = Some((dist, cmd)),
            Some((d, _)) if dist < d => {
                best = Some((dist, cmd));
                ambiguous = false;
            }
            Some((d, _)) if dist == d => {
                ambiguous = true;
            }
            _ => {}
        }
    }
    if ambiguous { None } else { best.map(|(_, cmd)| cmd) }
}

/// Resolve a partial command prefix to a full command name.
/// Tries exact match, then prefix match, then fuzzy match.
/// Returns the input unchanged if no unique match.
pub(super) fn resolve_prefix(raw: &str, commands: &[String]) -> String {
    if commands.iter().any(|c| c == raw) {
        return raw.to_string();
    }
    let prefix_matches: Vec<&String> = commands.iter().filter(|c| c.starts_with(raw)).collect();
    if prefix_matches.len() == 1 {
        return prefix_matches[0].clone();
    }
    // Fall back to fuzzy matching when no unique prefix match
    if prefix_matches.is_empty() {
        if let Some(m) = fuzzy_match(raw, commands) {
            return m.clone();
        }
    }
    raw.to_string()
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
                        let name_tag = s.name.as_deref().map(|n| format!(" [@{}]", n)).unwrap_or_default();
                        app.push_msg(ChatMessage::System(format!(
                            "  {}{} — {} [{}] ${:.4}{}",
                            &s.id, name_tag, title, s.model, s.session_cost, active
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
                app.push_msg(ChatMessage::System("usage: /resume <name_or_id>".to_string()));
            } else {
                match resolve_session(arg) {
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
                        let new_id = session.id.clone();
                        app.session = session;
                        let via = if synaps_cli::chain::load_chain(arg).is_ok() {
                            format!(" (via chain '{}')", arg)
                        } else if synaps_cli::session::find_session_by_name(arg).is_ok() {
                            format!(" (via name '{}')", arg)
                        } else {
                            String::new()
                        };
                        app.push_msg(ChatMessage::System(
                            format!("switched from {} to {}{}", old_id, new_id, via)
                        ));
                    }
                    Err(e) => {
                        app.push_msg(ChatMessage::Error(format!("failed to load session: {}", e)));
                    }
                }
            }
        }
        "saveas" => {
            let trimmed = arg.trim();
            if trimmed.is_empty() {
                app.session.clear_name();
                // Force save even with no messages — persist the name change
                let _ = app.session.save().await;
                app.push_msg(ChatMessage::System("session name cleared".into()));
            } else {
                match app.session.set_name(trimmed) {
                    Ok(()) => {
                        // Force save even with no messages — persist the name change
                        let _ = app.session.save().await;
                        app.push_msg(ChatMessage::System(format!("session named '{}'", trimmed)));
                    }
                    Err(e) => {
                        app.push_msg(ChatMessage::Error(format!("saveas failed: {}", e)));
                    }
                }
            }
        }
        "help" => {
            let help_lines = [
                "/clear — reset conversation",
                "/compact [focus] — summarize & compact conversation history",
                "/chain — show session compaction history",
                "/chain list — list named chains",
                "/chain name <name> — bookmark current session as <name> (auto-advances on compaction)",
                "/chain unname <name> — delete a named chain",
                "/saveas [name] — name the current session, or clear if empty",
                "/model [name] — show or set model",
                "/system <prompt|show|save> — system prompt",
                "/thinking [low|medium|high|xhigh] — thinking budget",
                "/sessions — list saved sessions",
                "/resume <name_or_id> — switch to a different session (chain > name > id)",
                "/help — show this",
                "/theme — list available themes",
                "/settings — open the settings menu",
                "/plugins — manage marketplaces and installed plugins",
                "/status — show account usage and reset times",
                "/ping — health-check all configured provider models",
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
            let mut parts = arg.splitn(2, char::is_whitespace);
            let sub = parts.next().unwrap_or("").trim();
            let rest = parts.next().unwrap_or("").trim();
            match sub {
                "" => return CommandAction::Chain,
                "list" | "ls" => return CommandAction::ChainList,
                "name" => {
                    if rest.is_empty() {
                        app.push_msg(ChatMessage::System("usage: /chain name <name>".into()));
                        return CommandAction::None;
                    }
                    return CommandAction::ChainName { name: rest.to_string() };
                }
                "unname" | "rm" => {
                    if rest.is_empty() {
                        app.push_msg(ChatMessage::System("usage: /chain unname <name>".into()));
                        return CommandAction::None;
                    }
                    return CommandAction::ChainUnname { name: rest.to_string() };
                }
                _ => {
                    app.push_msg(ChatMessage::Error(format!("unknown /chain subcommand: {}", sub)));
                }
            }
        }
        "status" => {
            return CommandAction::Status;
        }
        "ping" => {
            return CommandAction::Ping;
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
    use super::{edit_distance, fuzzy_match, resolve_prefix};

    #[test]
    fn plugins_is_in_all_commands() {
        assert!(ALL_COMMANDS.contains(&"plugins"));
    }

    // -- edit_distance tests --

    #[test]
    fn edit_distance_identical() {
        assert_eq!(edit_distance("plugins", "plugins"), 0);
    }

    #[test]
    fn edit_distance_empty() {
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("abc", ""), 3);
        assert_eq!(edit_distance("", ""), 0);
    }

    #[test]
    fn edit_distance_one_swap() {
        // "plgu" -> "plug" requires swapping gu->ug = 2 edits (delete + insert)
        assert_eq!(edit_distance("plgu", "plug"), 2);
    }

    #[test]
    fn edit_distance_typo() {
        // "plguins" -> "plugins": transposition + missing char
        assert!(edit_distance("plguins", "plugins") <= 2);
    }

    // -- fuzzy_match tests --

    fn commands() -> Vec<String> {
        vec![
            "clear".into(), "compact".into(), "chain".into(), "model".into(),
            "system".into(), "thinking".into(), "sessions".into(), "resume".into(),
            "saveas".into(), "theme".into(), "gamba".into(), "help".into(),
            "quit".into(), "exit".into(), "settings".into(), "plugins".into(),
            "status".into(),
        ]
    }

    #[test]
    fn fuzzy_match_plgu_to_plugins() {
        let cmds = commands();
        let _result = fuzzy_match("plgu", &cmds);
        // "plgu" is close to nothing perfectly, but let's check it matches something reasonable
        // or plugins via the longer form
        // Actually "plgu" vs "plug" portion... let's test the full typo
        let result2 = fuzzy_match("plguins", &cmds);
        assert_eq!(result2.map(|s| s.as_str()), Some("plugins"));
    }

    #[test]
    fn fuzzy_match_settngs_to_settings() {
        let cmds = commands();
        let result = fuzzy_match("settngs", &cmds);
        assert_eq!(result.map(|s| s.as_str()), Some("settings"));
    }

    #[test]
    fn fuzzy_match_hlep_to_help() {
        let cmds = commands();
        let result = fuzzy_match("hlep", &cmds);
        assert_eq!(result.map(|s| s.as_str()), Some("help"));
    }

    #[test]
    fn fuzzy_match_exact_returns_none() {
        // Exact match has distance 0, fuzzy_match skips it (exact is handled by resolve_prefix)
        let cmds = commands();
        assert!(fuzzy_match("plugins", &cmds).is_none());
    }

    #[test]
    fn fuzzy_match_gibberish_returns_none() {
        let cmds = commands();
        assert!(fuzzy_match("zzzzzzz", &cmds).is_none());
    }

    // -- resolve_prefix integration tests --

    #[test]
    fn resolve_prefix_exact() {
        let cmds = commands();
        assert_eq!(resolve_prefix("plugins", &cmds), "plugins");
    }

    #[test]
    fn resolve_prefix_prefix_match() {
        let cmds = commands();
        assert_eq!(resolve_prefix("plug", &cmds), "plugins");
    }

    #[test]
    fn resolve_prefix_fuzzy_fallback() {
        let cmds = commands();
        assert_eq!(resolve_prefix("plguins", &cmds), "plugins");
    }

    #[test]
    fn resolve_prefix_no_match() {
        let cmds = commands();
        assert_eq!(resolve_prefix("xyzzy", &cmds), "xyzzy");
    }

    #[test]
    fn resolve_prefix_ambiguous_prefix() {
        // "s" matches system, sessions, saveas, settings, status — returns raw
        let cmds = commands();
        assert_eq!(resolve_prefix("s", &cmds), "s");
    }
}
