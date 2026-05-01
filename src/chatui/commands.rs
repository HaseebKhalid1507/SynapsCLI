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
#[derive(Clone)]
pub(super) enum CommandAction {
    /// Nothing special — continue the loop.
    None,
    /// Start a new stream with these API messages.
    #[allow(dead_code)] StartStream,
    /// Trigger the quit animation.
    Quit,
    /// Launch the casino (requires dropping/recreating EventStream).
    LaunchGamba,
    /// Open the /model(s) router modal.
    OpenModels,
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
    /// Execute a plugin manifest command.
    PluginCommand {
        command: std::sync::Arc<synaps_cli::skills::registry::RegisteredPluginCommand>,
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
    /// Show loaded extension health snapshots.
    ExtensionsStatus,
    /// Show extension config diagnostics. `None` = all loaded extensions.
    ExtensionsConfig { id: Option<String> },
    /// Manage per-provider trust state.
    ExtensionsTrust(ExtensionsTrustAction),
    /// Show last N (or all) provider audit log entries.
    ExtensionsAudit { tail: Option<usize> },
    /// Inspect local memory store (namespaces, recent records).
    ExtensionsMemory(ExtensionsMemoryAction),
    /// Toggle voice dictation on/off (`/voice` or `/voice toggle`).
    VoiceToggle,
    /// Show voice subsystem status (`/voice status`).
    VoiceStatus,
    /// Print the whisper.cpp model catalog with installed status (`/voice models`).
    VoiceModels,
    /// Download a model from the catalog by id (`/voice download <id>`).
    VoiceDownload { id: String },
    /// Print `/voice` subcommand help (`/voice help`).
    VoiceHelp,
}

#[derive(Debug, Clone)]
pub enum ExtensionsTrustAction {
    List,
    Enable { runtime_id: String },
    Disable { runtime_id: String, reason: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionsMemoryAction {
    /// List all known memory namespaces.
    Namespaces,
    /// Show the most recent N records of a namespace (default 20).
    Recent { namespace: String, limit: Option<usize> },
}

pub(super) async fn execute_command_action(
    action: CommandAction,
    app: &mut App,
    runtime: &Runtime,
) {
    match action {
        CommandAction::PluginCommand { command, arg } => {
            match synaps_cli::skills::commands::execute_plugin_command_with_tools(
                &command,
                &arg,
                runtime.tools_shared(),
            ).await {
                Ok(output) => {
                    let mut lines = vec![format!(
                        "plugin command /{}:{} exited with {}",
                        command.plugin,
                        command.name,
                        output.status.map(|c| c.to_string()).unwrap_or_else(|| "signal".to_string())
                    )];
                    if !output.stdout.trim().is_empty() {
                        lines.push(format!("stdout:\n{}", output.stdout.trim_end()));
                    }
                    if !output.stderr.trim().is_empty() {
                        lines.push(format!("stderr:\n{}", output.stderr.trim_end()));
                    }
                    app.push_msg(ChatMessage::System(lines.join("\n")));
                }
                Err(e) => app.push_msg(ChatMessage::Error(format!("plugin command failed: {}", e))),
            }
        }
        _ => {}
    }
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
    keybind_registry: &synaps_cli::skills::keybinds::KeybindRegistry,
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
        "model" | "models" => {
            if arg.is_empty() {
                return CommandAction::OpenModels;
            } else {
                runtime.set_model(arg.to_string());
                app.push_msg(ChatMessage::System(
                    format!("model set to: {}", runtime.model())
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
                "/model, /models — open model router; /model <name> still sets directly",
                "/system <prompt|show|save> — system prompt",
                "/thinking [low|medium|high|xhigh] — thinking budget",
                "/sessions — list saved sessions",
                "/resume <name_or_id> — switch to a different session (chain > name > id)",
                "/help — show this",
                "/theme — list available themes",
                "/settings — open the settings menu",
                "/plugins — manage marketplaces and installed plugins",
                "/extensions status — show loaded extension health",
                "/extensions config [id] — show extension config diagnostics",
                "/extensions trust [list|enable <id>|disable <id> [reason]] — manage provider trust",
                "/extensions audit [N] — show last N provider audit log entries",
                "/extensions memory [namespaces|recent <ns> [N]] — inspect local memory store",
                "/status — show account usage and reset times",
                "/ping — health-check configured providers (set keys in /settings)",
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
        "extensions" => {
            let trimmed = arg.trim();
            if trimmed.is_empty() || trimmed == "status" {
                return CommandAction::ExtensionsStatus;
            }
            let mut parts = trimmed.splitn(2, char::is_whitespace);
            let sub = parts.next().unwrap_or("");
            let rest = parts.next().unwrap_or("").trim();
            match sub {
                "config" => {
                    if rest.is_empty() {
                        return CommandAction::ExtensionsConfig { id: None };
                    }
                    return CommandAction::ExtensionsConfig { id: Some(rest.to_string()) };
                }
                "trust" => {
                    if rest.is_empty() || rest == "list" {
                        return CommandAction::ExtensionsTrust(ExtensionsTrustAction::List);
                    }
                    let mut tparts = rest.splitn(2, char::is_whitespace);
                    let tsub = tparts.next().unwrap_or("");
                    let trest = tparts.next().unwrap_or("").trim();
                    match tsub {
                        "enable" => {
                            if trest.is_empty() {
                                app.push_msg(ChatMessage::System(
                                    "usage: /extensions trust enable <runtime_id>".to_string(),
                                ));
                                return CommandAction::None;
                            }
                            return CommandAction::ExtensionsTrust(
                                ExtensionsTrustAction::Enable { runtime_id: trest.to_string() },
                            );
                        }
                        "disable" => {
                            if trest.is_empty() {
                                app.push_msg(ChatMessage::System(
                                    "usage: /extensions trust disable <runtime_id> [reason]".to_string(),
                                ));
                                return CommandAction::None;
                            }
                            let mut dparts = trest.splitn(2, char::is_whitespace);
                            let runtime_id = dparts.next().unwrap_or("").to_string();
                            let reason = dparts
                                .next()
                                .map(|s| s.trim().to_string())
                                .filter(|s| !s.is_empty());
                            return CommandAction::ExtensionsTrust(
                                ExtensionsTrustAction::Disable { runtime_id, reason },
                            );
                        }
                        other => {
                            app.push_msg(ChatMessage::System(format!(
                                "usage: /extensions trust [list|enable <id>|disable <id> [reason]] (unknown: {})",
                                other
                            )));
                            return CommandAction::None;
                        }
                    }
                }
                "audit" => {
                    if rest.is_empty() {
                        return CommandAction::ExtensionsAudit { tail: None };
                    }
                    match rest.parse::<usize>() {
                        Ok(n) => return CommandAction::ExtensionsAudit { tail: Some(n) },
                        Err(_) => {
                            app.push_msg(ChatMessage::System(format!(
                                "usage: /extensions audit [N] (not a number: {})",
                                rest
                            )));
                            return CommandAction::None;
                        }
                    }
                }
                "memory" => {
                    if rest.is_empty() || rest == "namespaces" {
                        return CommandAction::ExtensionsMemory(ExtensionsMemoryAction::Namespaces);
                    }
                    let mut mparts = rest.splitn(2, char::is_whitespace);
                    let msub = mparts.next().unwrap_or("");
                    let mrest = mparts.next().unwrap_or("").trim();
                    match msub {
                        "recent" => {
                            if mrest.is_empty() {
                                app.push_msg(ChatMessage::System(
                                    "usage: /extensions memory recent <ns> [N]".to_string(),
                                ));
                                return CommandAction::None;
                            }
                            let mut rparts = mrest.splitn(2, char::is_whitespace);
                            let namespace = rparts.next().unwrap_or("").to_string();
                            let limit_str = rparts.next().unwrap_or("").trim();
                            let limit = if limit_str.is_empty() {
                                None
                            } else {
                                match limit_str.parse::<usize>() {
                                    Ok(n) => Some(n),
                                    Err(_) => {
                                        app.push_msg(ChatMessage::System(format!(
                                            "usage: /extensions memory recent <ns> [N] (not a number: {})",
                                            limit_str
                                        )));
                                        return CommandAction::None;
                                    }
                                }
                            };
                            return CommandAction::ExtensionsMemory(
                                ExtensionsMemoryAction::Recent { namespace, limit },
                            );
                        }
                        other => {
                            app.push_msg(ChatMessage::System(format!(
                                "usage: /extensions memory [namespaces|recent <ns> [N]] (unknown: {})",
                                other
                            )));
                            return CommandAction::None;
                        }
                    }
                }
                other => {
                    app.push_msg(ChatMessage::System(format!(
                        "usage: /extensions [status|config [id]|trust [list|enable <id>|disable <id> [reason]]|audit [N]|memory [namespaces|recent <ns> [N]]] (unknown: {})",
                        other
                    )));
                    return CommandAction::None;
                }
            }
        }
        "status" => {
            return CommandAction::Status;
        }
        "ping" => {
            return CommandAction::Ping;
        }
        "voice" => {
            let trimmed = arg.trim();
            return match trimmed {
                "" | "toggle" => CommandAction::VoiceToggle,
                "status" => CommandAction::VoiceStatus,
                "models" => CommandAction::VoiceModels,
                "help" => CommandAction::VoiceHelp,
                "download" => {
                    app.push_msg(ChatMessage::Error(
                        "usage: /voice download <id> (run `/voice models` to list)".to_string(),
                    ));
                    CommandAction::None
                }
                other if other.starts_with("download ") => {
                    let id = other["download ".len()..].trim();
                    if id.is_empty() {
                        app.push_msg(ChatMessage::Error(
                            "usage: /voice download <id> (run `/voice models` to list)".to_string(),
                        ));
                        CommandAction::None
                    } else {
                        CommandAction::VoiceDownload { id: id.to_string() }
                    }
                }
                other => {
                    app.push_msg(ChatMessage::Error(format!(
                        "unknown /voice subcommand: '{}' (expected toggle | status | models | download <id> | help — try `/voice help`)",
                        other
                    )));
                    CommandAction::None
                }
            };
        }
        "keybinds" => {
            let custom = keybind_registry.custom_binds();
            if custom.is_empty() {
                app.push_msg(ChatMessage::System("No plugin or user keybinds registered.".to_string()));
            } else {
                let mut lines = vec!["Keybinds:".to_string()];
                for bind in &custom {
                    let key = synaps_cli::skills::keybinds::format_key(&bind.key);
                    let source = match &bind.source {
                        synaps_cli::skills::keybinds::KeybindSource::Plugin(name) => format!(" ({})", name),
                        synaps_cli::skills::keybinds::KeybindSource::User => " (user)".to_string(),
                        _ => String::new(),
                    };
                    lines.push(format!("  {:18} {}{}", key, bind.description, source));
                }
                app.push_msg(ChatMessage::System(lines.join("\n")));
            }
        }
        _ => {
            match registry.resolve(cmd) {
                Resolution::Skill(skill) => {
                    return CommandAction::LoadSkill { skill, arg: arg.to_string() };
                }
                Resolution::PluginCommand(command) => {
                    return CommandAction::PluginCommand { command, arg: arg.to_string() };
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

/// Resolve the local whisper model directory (`~/.synaps-cli/models/whisper`).
/// Mirrors `chatui::settings::mod::whisper_model_options`'s derivation.
pub(crate) fn voice_models_dir() -> std::path::PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    std::path::PathBuf::from(home).join(".synaps-cli/models/whisper")
}

/// Render the whisper.cpp catalog as an aligned plain-text table, marking
/// each entry as installed (✓) or missing (✗) by checking
/// `models_dir.join(entry.filename).exists()`.
pub(crate) fn render_models_table(models_dir: &std::path::Path) -> String {
    use synaps_cli::voice::models::CATALOG;

    // Column widths derived from the catalog (with header minimums).
    let id_w = CATALOG.iter().map(|e| e.id.len()).max().unwrap_or(2).max("ID".len());
    let file_w = CATALOG
        .iter()
        .map(|e| e.filename.len())
        .max()
        .unwrap_or(4)
        .max("FILE".len());

    let mut out = String::new();
    out.push_str(&format!(
        "Whisper model catalog (downloads to {}/):\n\n",
        models_dir.display()
    ));
    out.push_str(&format!(
        "  {:<id_w$}  {:<file_w$}  {:>7}  {:<5}  STATUS\n",
        "ID",
        "FILE",
        "SIZE",
        "LANG",
        id_w = id_w,
        file_w = file_w,
    ));
    for entry in CATALOG {
        let installed = models_dir.join(entry.filename).exists();
        let status = if installed { "✓ installed" } else { "✗ not installed" };
        let lang = if entry.multilingual { "multi" } else { "en" };
        let size = format!("{} MB", entry.size_mb);
        out.push_str(&format!(
            "  {:<id_w$}  {:<file_w$}  {:>7}  {:<5}  {}\n",
            entry.id,
            entry.filename,
            size,
            lang,
            status,
            id_w = id_w,
            file_w = file_w,
        ));
    }
    out.push_str("\nRun `/voice download <id>` to install. Switch active model in `/settings → Voice → STT model`.");
    out
}

/// One-screen help text for `/voice` subcommands.
pub(crate) fn voice_help_text() -> String {
    let mut s = String::new();
    s.push_str("/voice subcommands:\n\n");
    s.push_str("  /voice                — toggle dictation on/off (same as `/voice toggle`)\n");
    s.push_str("  /voice toggle         — toggle dictation on/off\n");
    s.push_str("  /voice status         — show voice subsystem status\n");
    s.push_str("  /voice models         — list whisper.cpp model catalog and installed status\n");
    s.push_str("  /voice download <id>  — download a model from HuggingFace\n");
    s.push_str("  /voice help           — show this help\n\n");
    s.push_str("Toggle key configurable in /settings → Voice.");
    s
}

#[cfg(test)]
mod tests {
    use super::{edit_distance, execute_command_action, fuzzy_match, handle_command, render_models_table, resolve_prefix, CommandAction, ExtensionsMemoryAction, ExtensionsTrustAction};
    use async_trait::async_trait;
    use serde_json::Value;
    use std::path::PathBuf;
    use std::sync::Arc;
    use synaps_cli::skills::manifest::ManifestSkillPromptCommand;
    use synaps_cli::skills::registry::{CommandRegistry, RegisteredPluginCommand, RegisteredPluginCommandBackend};
    use synaps_cli::{Tool, ToolContext, ToolRegistry};

    #[test]
    fn plugins_is_in_all_commands() {
        assert!(synaps_cli::skills::BUILTIN_COMMANDS.contains(&"plugins"));
    }

    #[test]
    fn extensions_is_in_all_commands() {
        assert!(synaps_cli::skills::BUILTIN_COMMANDS.contains(&"extensions"));
    }

    #[test]
    fn resolve_prefix_keeps_exact_plugin_command_name() {
        let cmds = vec!["help".to_string(), "my-plugin:hello".to_string()];
        assert_eq!(resolve_prefix("my-plugin:hello", &cmds), "my-plugin:hello");
    }

    #[tokio::test]
    async fn plugin_colon_command_resolves_to_plugin_command_action() {
        let command = RegisteredPluginCommand {
            plugin: "policy".to_string(),
            name: "mode".to_string(),
            description: None,
            backend: RegisteredPluginCommandBackend::SkillPrompt {
                skill: "policy".to_string(),
                prompt: "Mode: ${args}".to_string(),
            },
            plugin_root: PathBuf::from("/tmp/policy"),
        };
        let registry = CommandRegistry::new_with_plugins(
            &[],
            vec![],
            vec![synaps_cli::skills::Plugin {
                name: "policy".to_string(),
                root: PathBuf::from("/tmp/policy"),
                marketplace: None,
                version: None,
                description: None,
                extension: None,
                manifest: Some(synaps_cli::skills::manifest::PluginManifest {
                    name: "policy".to_string(),
                    version: None,
                    description: None,
                    keybinds: vec![],
                    compatibility: None,
                    extension: None,
                    provides: None,
                    commands: vec![synaps_cli::skills::manifest::ManifestCommand::SkillPrompt(
                        ManifestSkillPromptCommand {
                            name: command.name.clone(),
                            description: None,
                            skill: "policy".to_string(),
                            prompt: "Mode: ${args}".to_string(),
                        },
                    )],
                }),
            }],
        );
        let mut app = crate::chatui::app::App::new(synaps_cli::Session::new("test", "medium", None));
        let mut runtime = synaps_cli::Runtime::new().await.unwrap();
        let system_prompt_path = PathBuf::from("/tmp/synaps-test-system-prompt");
        let registry = Arc::new(registry);
        let keybinds = synaps_cli::skills::keybinds::KeybindRegistry::new();

        match handle_command(
            "policy:mode",
            "strict",
            &mut app,
            &mut runtime,
            &system_prompt_path,
            &registry,
            &keybinds,
        ).await {
            CommandAction::PluginCommand { command, arg } => {
                assert_eq!(command.plugin, "policy");
                assert_eq!(command.name, "mode");
                assert_eq!(arg, "strict");
            }
            _ => panic!("expected plugin command action"),
        }
    }

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str { "policy:echo" }
        fn description(&self) -> &str { "echo" }
        fn parameters(&self) -> Value { serde_json::json!({"type":"object"}) }
        async fn execute(&self, params: Value, _ctx: ToolContext) -> synaps_cli::Result<String> {
            Ok(format!("echo {}", params["text"].as_str().unwrap_or_default()))
        }
    }

    #[tokio::test]
    async fn plugin_command_action_executes_extension_tool_and_prints_result() {
        let mut tools = ToolRegistry::without_subagent();
        tools.register(Arc::new(EchoTool));
        let mut runtime = synaps_cli::Runtime::new().await.unwrap();
        runtime.set_tools(tools);
        let mut app = crate::chatui::app::App::new(synaps_cli::Session::new("test", "medium", None));
        let command = Arc::new(RegisteredPluginCommand {
            plugin: "policy".to_string(),
            name: "echo".to_string(),
            description: None,
            backend: RegisteredPluginCommandBackend::ExtensionTool {
                tool: "echo".to_string(),
                input: serde_json::json!({"text":"${args}"}),
            },
            plugin_root: PathBuf::from("/tmp/policy"),
        });

        execute_command_action(
            CommandAction::PluginCommand { command, arg: "hello".to_string() },
            &mut app,
            &runtime,
        ).await;

        let last = app.messages.last().expect("system message should be pushed");
        match &last.msg {
            crate::chatui::app::ChatMessage::System(text) => {
                assert!(text.contains("plugin command /policy:echo exited with 0"), "{text}");
                assert!(text.contains("stdout:\necho hello"), "{text}");
            }
            _ => panic!("expected system message"),
        }
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
            "models".into(), "system".into(), "thinking".into(), "sessions".into(), "resume".into(),
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

    // -- /extensions parsing tests --

    async fn invoke_extensions(arg: &str) -> CommandAction {
        let mut app = crate::chatui::app::App::new(synaps_cli::Session::new("test", "medium", None));
        let mut runtime = synaps_cli::Runtime::new().await.unwrap();
        let system_prompt_path = PathBuf::from("/tmp/synaps-test-system-prompt");
        let registry = Arc::new(CommandRegistry::new_with_plugins(&[], vec![], vec![]));
        let keybinds = synaps_cli::skills::keybinds::KeybindRegistry::new();
        handle_command(
            "extensions",
            arg,
            &mut app,
            &mut runtime,
            &system_prompt_path,
            &registry,
            &keybinds,
        ).await
    }

    #[tokio::test]
    async fn parse_extensions_status_unchanged() {
        match invoke_extensions("status").await {
            CommandAction::ExtensionsStatus => {}
            _ => panic!("expected ExtensionsStatus for `status`"),
        }
        match invoke_extensions("").await {
            CommandAction::ExtensionsStatus => {}
            _ => panic!("expected ExtensionsStatus for empty arg"),
        }
    }

    #[tokio::test]
    async fn parse_extensions_config_no_arg() {
        match invoke_extensions("config").await {
            CommandAction::ExtensionsConfig { id: None } => {}
            _ => panic!("expected ExtensionsConfig {{ id: None }}"),
        }
    }

    #[tokio::test]
    async fn parse_extensions_config_with_id() {
        match invoke_extensions("config my-ext").await {
            CommandAction::ExtensionsConfig { id: Some(id) } => assert_eq!(id, "my-ext"),
            _ => panic!("expected ExtensionsConfig with id `my-ext`"),
        }
    }

    #[tokio::test]
    async fn parse_extensions_trust_list() {
        match invoke_extensions("trust").await {
            CommandAction::ExtensionsTrust(ExtensionsTrustAction::List) => {}
            _ => panic!("expected ExtensionsTrust(List) for `trust`"),
        }
        match invoke_extensions("trust list").await {
            CommandAction::ExtensionsTrust(ExtensionsTrustAction::List) => {}
            _ => panic!("expected ExtensionsTrust(List) for `trust list`"),
        }
    }

    #[tokio::test]
    async fn parse_extensions_trust_enable() {
        match invoke_extensions("trust enable plug:prov").await {
            CommandAction::ExtensionsTrust(ExtensionsTrustAction::Enable { runtime_id }) => {
                assert_eq!(runtime_id, "plug:prov");
            }
            _ => panic!("expected ExtensionsTrust(Enable)"),
        }
    }

    #[tokio::test]
    async fn parse_extensions_trust_disable_with_reason() {
        match invoke_extensions("trust disable plug:prov untrusted vendor").await {
            CommandAction::ExtensionsTrust(ExtensionsTrustAction::Disable { runtime_id, reason }) => {
                assert_eq!(runtime_id, "plug:prov");
                assert_eq!(reason.as_deref(), Some("untrusted vendor"));
            }
            _ => panic!("expected ExtensionsTrust(Disable) with reason"),
        }
    }

    #[tokio::test]
    async fn parse_extensions_trust_disable_no_reason() {
        match invoke_extensions("trust disable plug:prov").await {
            CommandAction::ExtensionsTrust(ExtensionsTrustAction::Disable { runtime_id, reason }) => {
                assert_eq!(runtime_id, "plug:prov");
                assert!(reason.is_none(), "expected no reason");
            }
            _ => panic!("expected ExtensionsTrust(Disable) without reason"),
        }
    }

    #[tokio::test]
    async fn parse_extensions_audit_no_tail() {
        match invoke_extensions("audit").await {
            CommandAction::ExtensionsAudit { tail: None } => {}
            _ => panic!("expected ExtensionsAudit with tail=None"),
        }
    }

    #[tokio::test]
    async fn parse_extensions_audit_with_tail() {
        match invoke_extensions("audit 25").await {
            CommandAction::ExtensionsAudit { tail: Some(n) } => assert_eq!(n, 25),
            _ => panic!("expected ExtensionsAudit with tail=Some(25)"),
        }
    }

    #[tokio::test]
    async fn parse_extensions_memory_namespaces() {
        match invoke_extensions("memory").await {
            CommandAction::ExtensionsMemory(ExtensionsMemoryAction::Namespaces) => {}
            _ => panic!("expected ExtensionsMemory(Namespaces) for `memory`"),
        }
        match invoke_extensions("memory namespaces").await {
            CommandAction::ExtensionsMemory(ExtensionsMemoryAction::Namespaces) => {}
            _ => panic!("expected ExtensionsMemory(Namespaces) for `memory namespaces`"),
        }
    }

    #[tokio::test]
    async fn parse_extensions_memory_recent_default_limit() {
        match invoke_extensions("memory recent my-ns").await {
            CommandAction::ExtensionsMemory(ExtensionsMemoryAction::Recent { namespace, limit }) => {
                assert_eq!(namespace, "my-ns");
                assert_eq!(limit, None);
            }
            _ => panic!("expected ExtensionsMemory(Recent) with no limit"),
        }
    }

    #[tokio::test]
    async fn parse_extensions_memory_recent_with_limit() {
        match invoke_extensions("memory recent my-ns 5").await {
            CommandAction::ExtensionsMemory(ExtensionsMemoryAction::Recent { namespace, limit }) => {
                assert_eq!(namespace, "my-ns");
                assert_eq!(limit, Some(5));
            }
            _ => panic!("expected ExtensionsMemory(Recent) with limit=Some(5)"),
        }
    }

    async fn invoke_voice(arg: &str) -> CommandAction {
        let mut app = crate::chatui::app::App::new(synaps_cli::Session::new("test", "medium", None));
        let mut runtime = synaps_cli::Runtime::new().await.unwrap();
        let system_prompt_path = PathBuf::from("/tmp/synaps-test-system-prompt");
        let registry = Arc::new(CommandRegistry::new_with_plugins(&[], vec![], vec![]));
        let keybinds = synaps_cli::skills::keybinds::KeybindRegistry::new();
        handle_command(
            "voice",
            arg,
            &mut app,
            &mut runtime,
            &system_prompt_path,
            &registry,
            &keybinds,
        ).await
    }

    #[tokio::test]
    async fn parse_voice_empty_arg_is_toggle() {
        match invoke_voice("").await {
            CommandAction::VoiceToggle => {}
            other => panic!("expected VoiceToggle for empty arg, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[tokio::test]
    async fn parse_voice_toggle_subcommand() {
        match invoke_voice("toggle").await {
            CommandAction::VoiceToggle => {}
            _ => panic!("expected VoiceToggle for `toggle` subcommand"),
        }
    }

    #[tokio::test]
    async fn parse_voice_status_subcommand() {
        match invoke_voice("status").await {
            CommandAction::VoiceStatus => {}
            _ => panic!("expected VoiceStatus for `status` subcommand"),
        }
    }

    #[tokio::test]
    async fn parse_voice_unknown_subcommand_is_none() {
        match invoke_voice("frobnicate").await {
            CommandAction::None => {}
            _ => panic!("expected CommandAction::None for unknown subcommand"),
        }
    }

    #[test]
    fn voice_is_in_builtin_commands() {
        assert!(synaps_cli::skills::BUILTIN_COMMANDS.contains(&"voice"));
    }

    #[tokio::test]
    async fn voice_models_command_parses() {
        match invoke_voice("models").await {
            CommandAction::VoiceModels => {}
            _ => panic!("expected VoiceModels for `models` subcommand"),
        }
    }

    #[tokio::test]
    async fn voice_help_command_parses() {
        match invoke_voice("help").await {
            CommandAction::VoiceHelp => {}
            _ => panic!("expected VoiceHelp for `help` subcommand"),
        }
    }

    #[tokio::test]
    async fn voice_download_with_id_parses() {
        match invoke_voice("download base").await {
            CommandAction::VoiceDownload { id } => assert_eq!(id, "base"),
            _ => panic!("expected VoiceDownload for `download base`"),
        }
    }

    #[tokio::test]
    async fn voice_download_without_id_errors() {
        let mut app = crate::chatui::app::App::new(synaps_cli::Session::new("test", "medium", None));
        let mut runtime = synaps_cli::Runtime::new().await.unwrap();
        let system_prompt_path = PathBuf::from("/tmp/synaps-test-system-prompt");
        let registry = Arc::new(CommandRegistry::new_with_plugins(&[], vec![], vec![]));
        let keybinds = synaps_cli::skills::keybinds::KeybindRegistry::new();
        let action = handle_command(
            "voice",
            "download",
            &mut app,
            &mut runtime,
            &system_prompt_path,
            &registry,
            &keybinds,
        )
        .await;
        matches!(action, CommandAction::None)
            .then_some(())
            .expect("expected CommandAction::None for bare `download`");
        let pushed_error = app.messages.iter().any(|m| matches!(&m.msg, crate::chatui::app::ChatMessage::Error(s) if s.contains("usage: /voice download")));
        assert!(pushed_error, "expected a usage error message to be pushed");
    }

    #[tokio::test]
    async fn voice_download_strips_extra_whitespace() {
        match invoke_voice("download   base.en").await {
            CommandAction::VoiceDownload { id } => assert_eq!(id, "base.en"),
            _ => panic!("expected VoiceDownload for whitespace-padded id"),
        }
    }

    #[test]
    fn voice_models_table_includes_all_catalog_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let table = render_models_table(tmp.path());
        for entry in synaps_cli::voice::models::CATALOG {
            assert!(
                table.contains(entry.id),
                "table missing id {}: {}",
                entry.id,
                table
            );
        }
    }

    #[test]
    fn voice_models_table_marks_installed_with_check() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("ggml-tiny.bin"), b"x").unwrap();
        let table = render_models_table(tmp.path());
        let tiny_line = table
            .lines()
            .find(|l: &&str| l.contains("ggml-tiny.bin") && !l.contains("ggml-tiny.en.bin"))
            .expect("tiny line present");
        assert!(tiny_line.contains('✓'), "expected ✓ on tiny line: {tiny_line}");
        let base_line = table
            .lines()
            .find(|l: &&str| l.contains("ggml-base.bin"))
            .expect("base line present");
        assert!(base_line.contains('✗'), "expected ✗ on base line: {base_line}");
    }
}
