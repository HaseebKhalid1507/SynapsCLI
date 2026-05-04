//! Slash command handling — dispatches /clear, /model, /system, etc.

use std::path::PathBuf;

use synaps_cli::{Runtime, Session, list_sessions, resolve_session};

use super::app::{App, ChatMessage};
use synaps_cli::extensions::commands::CommandOutputEvent;
use synaps_cli::extensions::runtime::InvokeCommandEvent;

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
    /// Open the searchable /help find lightbox.
    OpenHelpFind { query: String },
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
    /// Toggle the active sidecar plugin on/off (`/sidecar` or `/sidecar toggle`).
    ///
    /// `plugin_id = Some(pid)` selects a specific claimed sidecar (Phase 8 8B).
    /// `plugin_id = None` falls back to the legacy single-sidecar slot.
    SidecarToggle { plugin_id: Option<String> },
    /// Show sidecar subsystem status (`/sidecar status`).
    SidecarStatus { plugin_id: Option<String> },
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


pub(crate) async fn execute_interactive_plugin_command_events(
    command: &synaps_cli::skills::registry::RegisteredPluginCommand,
    arg: &str,
    manager: &synaps_cli::extensions::manager::ExtensionManager,
    app: &mut App,
) {
    let synaps_cli::skills::registry::RegisteredPluginCommandBackend::Interactive {
        plugin_extension_id,
    } = &command.backend else {
        app.push_msg(ChatMessage::Error(
            "plugin command is not interactive".to_string(),
        ));
        return;
    };

    let args: Vec<String> = arg.split_whitespace().map(str::to_string).collect();
    execute_interactive_plugin_command_by_parts(
        plugin_extension_id,
        &command.name,
        args,
        manager,
        app,
    ).await;
}

pub(crate) async fn execute_interactive_plugin_command_by_parts(
    plugin_extension_id: &str,
    command_name: &str,
    args: Vec<String>,
    manager: &synaps_cli::extensions::manager::ExtensionManager,
    app: &mut App,
) {
    let request_id = uuid::Uuid::new_v4().to_string();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<InvokeCommandEvent>();
    let result = manager
        .invoke_command(plugin_extension_id, command_name, args, &request_id, tx)
        .await;

    while let Ok(event) = rx.try_recv() {
        match event {
            InvokeCommandEvent::Output(output) => {
                if let Some(msg) = command_output_event_to_chat_message(output) {
                    app.push_msg(msg);
                }
            }
            InvokeCommandEvent::Task(task) => app.active_tasks.apply(task),
        }
    }

    if let Err(err) = result {
        app.push_msg(ChatMessage::Error(format!(
            "interactive plugin command {}:{} failed: {}",
            plugin_extension_id, command_name, err
        )));
    }
}

pub(crate) fn command_output_event_to_chat_message(event: CommandOutputEvent) -> Option<ChatMessage> {
    match event {
        CommandOutputEvent::Text { content } => Some(ChatMessage::Text(content)),
        CommandOutputEvent::System { content } => Some(ChatMessage::System(content)),
        CommandOutputEvent::Error { content } => Some(ChatMessage::Error(content)),
        CommandOutputEvent::Table { headers, rows } => {
            let mut lines = Vec::new();
            if !headers.is_empty() {
                lines.push(headers.join("  "));
            }
            for row in rows {
                lines.push(row.join("  "));
            }
            Some(ChatMessage::System(lines.join("\n")))
        }
        CommandOutputEvent::Done => None,
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
    // Phase 8 slice 8A: plugin-claimed lifecycle commands take precedence
    // over builtins. If a plugin's manifest claims `/capture` (or any other
    // top-level word) via `provides.sidecar.lifecycle`, route
    // `<word> toggle` and `<word> status` to the generic sidecar
    // lifecycle actions. Other subcommands (e.g. `/capture models`) fall
    // through to the normal plugin-command resolver below.
    if let Some(claim) = registry.lifecycle_for_command(cmd) {
        let trimmed = arg.trim();
        match trimmed {
            "" | "toggle" => return CommandAction::SidecarToggle { plugin_id: Some(claim.plugin.clone()) },
            "status" => return CommandAction::SidecarStatus { plugin_id: Some(claim.plugin.clone()) },
            _ => {
                // Fall through to the plugin-command resolver: the
                // plugin can define `<command> <other-sub>` (e.g.
                // `/capture models`) as a normal interactive command.
                if let Some(command) =
                    registry.find_plugin_command_unqualified(&claim.command)
                {
                    return CommandAction::PluginCommand {
                        command,
                        arg: trimmed.to_string(),
                    };
                }
                // No plugin command; surface a usage hint scoped to
                // the claimed display name.
                app.push_msg(ChatMessage::Error(format!(
                    "unknown /{} subcommand: `{}` (try: toggle, status)",
                    claim.command, trimmed,
                )));
                return CommandAction::None;
            }
        }
    }
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
            let trimmed = arg.trim();
            if trimmed == "find" || trimmed.starts_with("find ") {
                let query = trimmed.strip_prefix("find").unwrap_or("").trim().to_string();
                return CommandAction::OpenHelpFind { query };
            }

            let registry = synaps_cli::help::HelpRegistry::new(
                synaps_cli::help::builtin_entries(),
                registry.plugin_help_entries(),
            );
            if let Some(rendered) = synaps_cli::help::render_help(
                &registry,
                if trimmed.is_empty() { None } else { Some(trimmed) },
            ) {
                app.push_msg(ChatMessage::System(rendered));
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
        "sidecar" => {
            // Phase 8 8A.6 / 8A.7: ambiguity-aware dispatcher.
            //
            // Two surface forms:
            //   * unqualified — `/sidecar [toggle|status]` — back-compat
            //     for the single-sidecar slot. With ≥2 claims we refuse
            //     to dispatch and force disambiguation.
            //   * qualified   — `/sidecar <plugin-id> <subcommand>` —
            //     selects a specific claimed sidecar. (In slice 8A the
            //     action variants don't carry a plugin-id payload yet;
            //     we just validate the plugin-id against the loaded
            //     lifecycle claims and dispatch the bare action.)
            //
            // TODO(phase 8 8B): plumb plugin_id into SidecarToggle /
            //                   SidecarStatus so multi-sidecar hosting
            //                   can route to a specific instance.
            let trimmed = arg.trim();
            let mut tokens = trimmed.split_whitespace();
            let first = tokens.next().unwrap_or("");
            let rest: String = tokens.collect::<Vec<_>>().join(" ");

            if rest.is_empty() {
                // Unqualified form.
                let claims = registry.lifecycle_claims();
                let render_disambig = |verb: &str, claims: &[synaps_cli::skills::registry::LifecycleClaim]| -> String {
                    let mut sorted: Vec<_> = claims.iter().collect();
                    sorted.sort_by(|a, b| a.plugin.cmp(&b.plugin));
                    let plugins = sorted.iter().map(|c| c.plugin.clone()).collect::<Vec<_>>().join(", ");
                    let cmds = sorted.iter().map(|c| format!("/{}", c.command)).collect::<Vec<_>>().join(", ");
                    format!(
                        "multiple sidecars loaded: {}; use /sidecar <plugin-id> {} or one of the per-plugin commands ({})",
                        plugins, verb, cmds
                    )
                };
                match first {
                    "" | "toggle" => match claims.len() {
                        0 => return CommandAction::SidecarToggle { plugin_id: None },
                        1 => {
                            let c = &claims[0];
                            app.push_msg(ChatMessage::System(format!(
                                "hint: this sidecar is claimed by /{} — try /{} toggle",
                                c.command, c.command
                            )));
                            return CommandAction::SidecarToggle { plugin_id: Some(c.plugin.clone()) };
                        }
                        _ => {
                            app.push_msg(ChatMessage::Error(render_disambig("toggle", &claims)));
                            return CommandAction::None;
                        }
                    },
                    "status" => match claims.len() {
                        0 => return CommandAction::SidecarStatus { plugin_id: None },
                        1 => {
                            let c = &claims[0];
                            app.push_msg(ChatMessage::System(format!(
                                "hint: this sidecar is claimed by /{} — try /{} status",
                                c.command, c.command
                            )));
                            return CommandAction::SidecarStatus { plugin_id: Some(c.plugin.clone()) };
                        }
                        _ => {
                            app.push_msg(ChatMessage::Error(render_disambig("status", &claims)));
                            return CommandAction::None;
                        }
                    },
                    other => {
                        app.push_msg(ChatMessage::Error(format!(
                            "unknown /sidecar subcommand: `{}` (try: toggle, status)",
                            other
                        )));
                        return CommandAction::None;
                    }
                }
            } else {
                // Qualified form: first = plugin-id, rest = subcommand.
                let plugin_id = first;
                let claims = registry.lifecycle_claims();
                if !claims.iter().any(|c| c.plugin == plugin_id) {
                    let mut sorted: Vec<_> = claims.iter().collect();
                    sorted.sort_by(|a, b| a.plugin.cmp(&b.plugin));
                    let list = if sorted.is_empty() {
                        "none".to_string()
                    } else {
                        sorted.iter().map(|c| c.plugin.clone()).collect::<Vec<_>>().join(", ")
                    };
                    app.push_msg(ChatMessage::Error(format!(
                        "unknown sidecar plugin: '{}' (loaded: {})",
                        plugin_id, list
                    )));
                    return CommandAction::None;
                }
                match rest.as_str() {
                    "toggle" => return CommandAction::SidecarToggle { plugin_id: Some(plugin_id.to_string()) },
                    "status" => return CommandAction::SidecarStatus { plugin_id: Some(plugin_id.to_string()) },
                    other => {
                        app.push_msg(ChatMessage::Error(format!(
                            "unknown /sidecar subcommand: `{}` (try: toggle, status)",
                            other
                        )));
                        return CommandAction::None;
                    }
                }
            }
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

#[cfg(test)]
mod tests {
    use super::{edit_distance, execute_command_action, execute_interactive_plugin_command_events, fuzzy_match, handle_command, resolve_prefix, CommandAction, ExtensionsMemoryAction, ExtensionsTrustAction};
    use super::command_output_event_to_chat_message;
    use crate::chatui::app::ChatMessage;
    use synaps_cli::extensions::commands::CommandOutputEvent;
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
                    help_entries: vec![],
                    provides: None,
                    settings: None,
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


    #[test]
    fn command_output_event_text_becomes_chat_text() {
        let msg = command_output_event_to_chat_message(CommandOutputEvent::Text {
            content: "hello".to_string(),
        }).expect("text event should produce chat message");
        match msg {
            ChatMessage::Text(text) => assert_eq!(text, "hello"),
            _ => panic!("expected text chat message"),
        }
    }

    #[test]
    fn command_output_event_table_becomes_plain_text_table() {
        let msg = command_output_event_to_chat_message(CommandOutputEvent::Table {
            headers: vec!["ID".into(), "Status".into()],
            rows: vec![vec!["tiny".into(), "installed".into()]],
        }).expect("table event should produce chat message");
        match msg {
            ChatMessage::System(text) => {
                assert!(text.contains("ID"), "{text}");
                assert!(text.contains("tiny"), "{text}");
                assert!(text.contains("installed"), "{text}");
            }
            _ => panic!("expected system table message"),
        }
    }


    #[tokio::test]
    async fn interactive_plugin_command_invocation_pushes_output_and_updates_tasks() {
        let bus = Arc::new(synaps_cli::extensions::hooks::HookBus::new());
        let mut manager = synaps_cli::extensions::manager::ExtensionManager::new(bus);
        let manifest = synaps_cli::extensions::manifest::ExtensionManifest {
            protocol_version: 1,
            runtime: synaps_cli::extensions::manifest::ExtensionRuntime::Process,
            command: "python3".to_string(),
            setup: None,
            args: vec!["tests/fixtures/interactive_command_extension.py".to_string()],
            permissions: vec!["tools.register".to_string()],
            hooks: vec![],
            config: vec![],
        };
        manager.load("demo-plugin", &manifest).await.unwrap();
        let command = RegisteredPluginCommand {
            plugin: "demo-plugin".to_string(),
            name: "demo".to_string(),
            description: None,
            backend: RegisteredPluginCommandBackend::Interactive {
                plugin_extension_id: "demo-plugin".to_string(),
            },
            plugin_root: PathBuf::from("/tmp/demo"),
        };
        let mut app = crate::chatui::app::App::new(synaps_cli::Session::new("test", "medium", None));

        execute_interactive_plugin_command_events(&command, "models", &manager, &mut app).await;

        assert!(app.messages.iter().any(|m| matches!(&m.msg, ChatMessage::Text(text) if text.contains("hello from demo"))));
        assert!(app.active_tasks.get("demo-task").is_some());
        assert!(app.active_tasks.get("demo-task").unwrap().done);
        manager.shutdown_all().await;
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

    #[test]
    fn sidecar_is_in_builtin_commands_and_capture_is_plugin_owned() {
        assert!(synaps_cli::skills::BUILTIN_COMMANDS.contains(&"sidecar"));
        assert!(!synaps_cli::skills::BUILTIN_COMMANDS.contains(&"capture"));
    }

    // ---- Phase 8 slice 8A: lifecycle-claim dispatcher ----

    fn lifecycle_plugin(plugin: &str, command: &str) -> synaps_cli::skills::Plugin {
        use synaps_cli::skills::manifest::{
            PluginManifest, PluginProvides, SidecarLifecycle, SidecarManifest,
        };
        synaps_cli::skills::Plugin {
            name: plugin.to_string(),
            root: PathBuf::from(format!("/tmp/{plugin}")),
            marketplace: None,
            version: None,
            description: None,
            extension: None,
            manifest: Some(PluginManifest {
                name: plugin.to_string(),
                version: None,
                description: None,
                keybinds: vec![],
                compatibility: None,
                commands: vec![],
                extension: None,
                help_entries: vec![],
                provides: Some(PluginProvides {
                    sidecar: Some(SidecarManifest {
                        command: "bin/run".to_string(),
                        setup: None,
                        protocol_version: 1,
                        model: None,
                        lifecycle: Some(SidecarLifecycle {
                            command: command.to_string(),
                            settings_category: None,
                            display_name: None,
                            importance: 0,
                        }),
                    }),
                }),
                settings: None,
            }),
        }
    }

    #[tokio::test]
    async fn lifecycle_claim_routes_toggle_to_sidecar_toggle() {
        let registry = Arc::new(CommandRegistry::new_with_plugins(
            &[],
            vec![],
            vec![lifecycle_plugin("sample-sidecar", "capture")],
        ));
        let mut app = crate::chatui::app::App::new(synaps_cli::Session::new("test", "medium", None));
        let mut runtime = synaps_cli::Runtime::new().await.unwrap();
        let keybinds = synaps_cli::skills::keybinds::KeybindRegistry::new();
        let action = handle_command(
            "capture",
            "toggle",
            &mut app,
            &mut runtime,
            &PathBuf::from("/tmp/sp"),
            &registry,
            &keybinds,
        )
        .await;
        match action {
            CommandAction::SidecarToggle { plugin_id } => {
                assert_eq!(plugin_id.as_deref(), Some("sample-sidecar"));
            }
            other => panic!("expected SidecarToggle with plugin_id, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[tokio::test]
    async fn lifecycle_claim_routes_bare_command_to_toggle() {
        // `/capture` (no arg) is treated as `/capture toggle`.
        let registry = Arc::new(CommandRegistry::new_with_plugins(
            &[],
            vec![],
            vec![lifecycle_plugin("sample-sidecar", "capture")],
        ));
        let mut app = crate::chatui::app::App::new(synaps_cli::Session::new("test", "medium", None));
        let mut runtime = synaps_cli::Runtime::new().await.unwrap();
        let keybinds = synaps_cli::skills::keybinds::KeybindRegistry::new();
        let action = handle_command(
            "capture",
            "",
            &mut app,
            &mut runtime,
            &PathBuf::from("/tmp/sp"),
            &registry,
            &keybinds,
        )
        .await;
        assert!(matches!(action, CommandAction::SidecarToggle { .. }));
    }

    #[tokio::test]
    async fn lifecycle_claim_routes_status_to_sidecar_status() {
        let registry = Arc::new(CommandRegistry::new_with_plugins(
            &[],
            vec![],
            vec![lifecycle_plugin("sample-sidecar", "capture")],
        ));
        let mut app = crate::chatui::app::App::new(synaps_cli::Session::new("test", "medium", None));
        let mut runtime = synaps_cli::Runtime::new().await.unwrap();
        let keybinds = synaps_cli::skills::keybinds::KeybindRegistry::new();
        let action = handle_command(
            "capture",
            "status",
            &mut app,
            &mut runtime,
            &PathBuf::from("/tmp/sp"),
            &registry,
            &keybinds,
        )
        .await;
        match action {
            CommandAction::SidecarStatus { plugin_id } => {
                assert_eq!(plugin_id.as_deref(), Some("sample-sidecar"));
            }
            other => panic!("expected SidecarStatus with plugin_id, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[tokio::test]
    async fn lifecycle_claim_takes_precedence_over_capture_builtin_alias() {
        // When a plugin declares the `capture` lifecycle, the dispatcher
        // routes via the lifecycle path — NOT the legacy `"capture"`
        // builtin alias — so the lifecycle answer is reached even if
        // the alias would error out (no plugin command registered).
        let registry = Arc::new(CommandRegistry::new_with_plugins(
            &[],
            vec![],
            vec![lifecycle_plugin("sample-sidecar", "capture")],
        ));
        let mut app = crate::chatui::app::App::new(synaps_cli::Session::new("test", "medium", None));
        let mut runtime = synaps_cli::Runtime::new().await.unwrap();
        let keybinds = synaps_cli::skills::keybinds::KeybindRegistry::new();
        let action = handle_command(
            "capture",
            "toggle",
            &mut app,
            &mut runtime,
            &PathBuf::from("/tmp/sp"),
            &registry,
            &keybinds,
        )
        .await;
        // No "no plugin owns /capture" error pushed — lifecycle path won.
        let pushed_legacy_error = app.messages.iter().any(|m| matches!(&m.msg, crate::chatui::app::ChatMessage::Error(s) if s.contains("no plugin owns /capture")));
        assert!(!pushed_legacy_error);
        assert!(matches!(action, CommandAction::SidecarToggle { .. }));
    }

    #[tokio::test]
    async fn lifecycle_claim_unknown_subcommand_pushes_error() {
        let registry = Arc::new(CommandRegistry::new_with_plugins(
            &[],
            vec![],
            vec![lifecycle_plugin("sample-sidecar", "capture")],
        ));
        let mut app = crate::chatui::app::App::new(synaps_cli::Session::new("test", "medium", None));
        let mut runtime = synaps_cli::Runtime::new().await.unwrap();
        let keybinds = synaps_cli::skills::keybinds::KeybindRegistry::new();
        let action = handle_command(
            "capture",
            "bogus",
            &mut app,
            &mut runtime,
            &PathBuf::from("/tmp/sp"),
            &registry,
            &keybinds,
        )
        .await;
        assert!(matches!(action, CommandAction::None));
        let pushed = app.messages.iter().any(|m| matches!(&m.msg, crate::chatui::app::ChatMessage::Error(s) if s.contains("unknown /capture subcommand")));
        assert!(pushed);
    }

    // ---- Phase 8 slices 8A.6 / 8A.7 — `/sidecar` ambiguity-aware dispatcher ----

    async fn invoke_sidecar_with_plugins(
        arg: &str,
        plugins: Vec<synaps_cli::skills::Plugin>,
    ) -> (CommandAction, crate::chatui::app::App) {
        let mut app = crate::chatui::app::App::new(synaps_cli::Session::new("test", "medium", None));
        let mut runtime = synaps_cli::Runtime::new().await.unwrap();
        let registry = Arc::new(CommandRegistry::new_with_plugins(&[], vec![], plugins));
        let keybinds = synaps_cli::skills::keybinds::KeybindRegistry::new();
        let action = handle_command(
            "sidecar",
            arg,
            &mut app,
            &mut runtime,
            &PathBuf::from("/tmp/sp"),
            &registry,
            &keybinds,
        )
        .await;
        (action, app)
    }

    #[tokio::test]
    async fn sidecar_toggle_works_when_zero_claims_loaded() {
        let (action, app) = invoke_sidecar_with_plugins("toggle", vec![]).await;
        assert!(matches!(action, CommandAction::SidecarToggle { .. }));
        let pushed_err = app.messages.iter().any(|m| matches!(&m.msg, crate::chatui::app::ChatMessage::Error(_)));
        assert!(!pushed_err, "no errors expected for zero-claim back-compat");
    }

    #[tokio::test]
    async fn sidecar_toggle_with_one_claim_dispatches_with_hint() {
        let (action, app) = invoke_sidecar_with_plugins(
            "toggle",
            vec![lifecycle_plugin("sample-sidecar", "capture")],
        ).await;
        assert!(matches!(action, CommandAction::SidecarToggle { .. }));
        let pushed_hint = app.messages.iter().any(|m| matches!(&m.msg, crate::chatui::app::ChatMessage::System(s) if s.contains("try /capture toggle")));
        assert!(pushed_hint, "expected a System hint mentioning `try /capture toggle`");
    }

    #[tokio::test]
    async fn sidecar_toggle_with_two_claims_errors_with_disambiguation() {
        let (action, app) = invoke_sidecar_with_plugins(
            "toggle",
            vec![
                lifecycle_plugin("sample-sidecar", "capture"),
                lifecycle_plugin("local-ocr", "ocr"),
            ],
        ).await;
        assert!(matches!(action, CommandAction::None));
        let pushed = app.messages.iter().find_map(|m| match &m.msg {
            crate::chatui::app::ChatMessage::Error(s) => Some(s.clone()),
            _ => None,
        });
        let s = pushed.expect("expected an Error message");
        assert!(s.contains("sample-sidecar"), "error should list sample-sidecar; got: {s}");
        assert!(s.contains("local-ocr"), "error should list local-ocr; got: {s}");
        assert!(s.contains("/capture"), "error should mention /capture; got: {s}");
        assert!(s.contains("/ocr"), "error should mention /ocr; got: {s}");
    }

    #[tokio::test]
    async fn sidecar_qualified_plugin_id_toggle_works() {
        let (action, app) = invoke_sidecar_with_plugins(
            "sample-sidecar toggle",
            vec![
                lifecycle_plugin("sample-sidecar", "capture"),
                lifecycle_plugin("local-ocr", "ocr"),
            ],
        ).await;
        assert!(matches!(action, CommandAction::SidecarToggle { .. }));
        let pushed_err = app.messages.iter().any(|m| matches!(&m.msg, crate::chatui::app::ChatMessage::Error(_)));
        assert!(!pushed_err, "no errors expected for valid qualified form");
    }

    #[tokio::test]
    async fn sidecar_qualified_unknown_plugin_id_errors() {
        let (action, app) = invoke_sidecar_with_plugins(
            "nonexistent toggle",
            vec![lifecycle_plugin("sample-sidecar", "capture")],
        ).await;
        assert!(matches!(action, CommandAction::None));
        let pushed = app.messages.iter().any(|m| matches!(&m.msg, crate::chatui::app::ChatMessage::Error(s) if s.contains("unknown sidecar plugin")));
        assert!(pushed, "expected `unknown sidecar plugin` error");
    }

    #[tokio::test]
    async fn sidecar_qualified_plugin_id_status() {
        let (action, _app) = invoke_sidecar_with_plugins(
            "sample-sidecar status",
            vec![lifecycle_plugin("sample-sidecar", "capture")],
        ).await;
        assert!(matches!(action, CommandAction::SidecarStatus { .. }));
    }

    #[tokio::test]
    async fn sidecar_qualified_plugin_id_unknown_subcommand_errors() {
        let (action, app) = invoke_sidecar_with_plugins(
            "sample-sidecar bogus",
            vec![lifecycle_plugin("sample-sidecar", "capture")],
        ).await;
        assert!(matches!(action, CommandAction::None));
        let pushed = app.messages.iter().any(|m| matches!(&m.msg, crate::chatui::app::ChatMessage::Error(s) if s.contains("unknown /sidecar subcommand")));
        assert!(pushed, "expected `unknown /sidecar subcommand` error");
    }
}
