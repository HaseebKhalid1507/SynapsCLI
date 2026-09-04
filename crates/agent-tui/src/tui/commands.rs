//! Slash command handling — dispatches /clear, /model, /system, etc.

use std::path::Path;

use chrono;
use synaps_cli::{list_recent_sessions, resolve_session, Runtime, Session};

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
    #[allow(dead_code)]
    StartStream,
    /// Trigger the quit animation.
    Quit,
    /// Launch the casino (requires dropping/recreating EventStream).
    LaunchGamba,
    /// Open the /model(s) router modal.
    OpenModels,
    /// Open the /effort lightbox (valid levels for the active exact model).
    OpenEffort,
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
    Compact { custom_instructions: Option<String> },
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
    Enable {
        runtime_id: String,
    },
    Disable {
        runtime_id: String,
        reason: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionsMemoryAction {
    /// List all known memory namespaces.
    Namespaces,
    /// Show the most recent N records of a namespace (default 20).
    Recent {
        namespace: String,
        limit: Option<usize>,
    },
}

pub(super) async fn execute_command_action(
    action: CommandAction,
    app: &mut App,
    runtime: &Runtime,
) {
    if let CommandAction::PluginCommand { command, arg } = action {
        match synaps_cli::skills::commands::execute_plugin_command_with_tools(
            &command,
            &arg,
            runtime.tools_shared(),
        )
        .await
        {
            Ok(output) => {
                let mut lines = vec![format!(
                    "plugin command /{}:{} exited with {}",
                    command.plugin,
                    command.name,
                    output
                        .status
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".to_string())
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
}

pub(crate) async fn execute_interactive_plugin_command_events(
    command: &synaps_cli::skills::registry::RegisteredPluginCommand,
    arg: &str,
    manager: &synaps_cli::extensions::manager::ExtensionManager,
    app: &mut App,
) {
    let synaps_cli::skills::registry::RegisteredPluginCommandBackend::Interactive {
        plugin_extension_id,
    } = &command.backend
    else {
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
    )
    .await;
}

pub(crate) async fn execute_interactive_plugin_command_by_parts(
    plugin_extension_id: &str,
    command_name: &str,
    args: Vec<String>,
    manager: &synaps_cli::extensions::manager::ExtensionManager,
    app: &mut App,
) {
    let request_id = uuid::Uuid::new_v4().to_string();
    // CP-11 fix-3: the collected entry point pairs the bounded event sink
    // with an eagerly concurrent collector, so a hostile command-output
    // flood is paced and capped at production time. The report holds the
    // budget-bounded retained events plus exact accounting; consuming it
    // here (post-hoc, as before) can no longer affect host retention.
    let (result, report) = manager
        .invoke_command_collected(plugin_extension_id, command_name, args, &request_id)
        .await;

    let notice = report.limit_notice();
    for event in report.events {
        match event {
            InvokeCommandEvent::Output(output) => {
                if let Some(msg) = command_output_event_to_chat_message(output) {
                    app.push_msg(msg);
                }
            }
            InvokeCommandEvent::Task(task) => {
                std::sync::Arc::make_mut(&mut app.active_tasks).apply(task)
            }
        }
    }
    if let Some(notice) = notice {
        // Preserve error visibility: dropped Error events surface the
        // notice on the error channel.
        if notice.severity_error {
            app.push_msg(ChatMessage::Error(notice.message));
        } else {
            app.push_msg(ChatMessage::System(notice.message));
        }
    }

    if let Err(err) = result {
        app.push_msg(ChatMessage::Error(format!(
            "interactive plugin command {}:{} failed: {}",
            plugin_extension_id, command_name, err
        )));
    }
}

pub(crate) fn command_output_event_to_chat_message(
    event: CommandOutputEvent,
) -> Option<ChatMessage> {
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
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
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
    if ambiguous {
        None
    } else {
        best.map(|(_, cmd)| cmd)
    }
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

/// Re-apply a saved session's reasoning level as explicit, clamped to the
/// current model. Returns a notice when the saved level was clamped.
fn restore_session_reasoning(runtime: &mut Runtime, thinking_level: &str) -> Option<String> {
    runtime
        .restore_session_reasoning(thinking_level)
        .map(|clamp| {
            format!(
                "thinking → {} (clamped from {}: not supported by {})",
                clamp.to.as_str(),
                clamp.from.as_str(),
                runtime.model()
            )
        })
}

/// Handle a slash command when NOT streaming.
pub(super) async fn handle_command(
    cmd: &str,
    arg: &str,
    app: &mut App,
    runtime: &mut Runtime,
    system_prompt_path: &Path,
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
            "" | "toggle" => {
                return CommandAction::SidecarToggle {
                    plugin_id: Some(claim.plugin.clone()),
                }
            }
            "status" => {
                return CommandAction::SidecarStatus {
                    plugin_id: Some(claim.plugin.clone()),
                }
            }
            _ => {
                // Fall through to the plugin-command resolver: the
                // plugin can define `<command> <other-sub>` (e.g.
                // `/capture models`) as a normal interactive command.
                if let Some(command) = registry.find_plugin_command_unqualified(&claim.command) {
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

    // ── Engine-level commands (shared with headless) ──
    // `/context` with the TUI's own conversation history: the runtime does
    // not own session messages, so the surface passes them in. Must run
    // before the generic engine intercept (which has no history access).
    if cmd == "context" {
        match synaps_cli::engine::commands::context_command(runtime, Some(&app.api_messages)) {
            synaps_cli::engine::commands::CommandResult::Output(text) => {
                app.push_msg(ChatMessage::System(text));
            }
            synaps_cli::engine::commands::CommandResult::Error(e) => {
                app.push_msg(ChatMessage::Error(e));
            }
            _ => {}
        }
        return CommandAction::None;
    }
    // NOTE: this intercept runs BEFORE the match below — any arm there for a
    // command the engine claims (model/thinking with args, compact, quit) is
    // unreachable for the intercepted case.
    // `/effort <level>` is the SAME checked mutation + persistence path as
    // `/thinking <level>` — normalize before the engine intercept so the
    // engine's validation (set_reasoning_level_checked, numeric legacy
    // budgets included) and the ThinkingChanged persist arm below apply.
    let cmd = if cmd == "effort" && !arg.is_empty() {
        "thinking"
    } else {
        cmd
    };
    if let Some(result) = synaps_cli::engine::commands::handle_engine_command(cmd, arg, runtime) {
        use synaps_cli::engine::commands::{
            persist_to_config, thinking_config_value, CommandResult,
        };
        return match result {
            CommandResult::Quit => CommandAction::Quit,
            CommandResult::ModelChanged {
                reasoning_clamped, ..
            } => {
                // Use the runtime's cleaned model string, not the raw arg.
                let applied = runtime.model().to_string();
                app.session.model = applied.clone();
                let status = persist_to_config("model", &applied);
                app.push_msg(ChatMessage::System(format!(
                    "model set to: {} {}",
                    applied, status
                )));
                // Session-only: the user's configured thinking value stays theirs.
                if let Some(clamp) = reasoning_clamped {
                    app.session.thinking_level = runtime.thinking_level().to_string();
                    app.push_msg(ChatMessage::System(reasoning_clamp_notice(
                        &clamp, &applied,
                    )));
                }
                CommandAction::None
            }
            CommandResult::ThinkingChanged { spec } => {
                app.session.thinking_level = thinking_config_value(spec);
                let status = persist_to_config("thinking", &thinking_config_value(spec));
                app.push_msg(ChatMessage::System(format!(
                    "thinking set to: {} {}",
                    spec.level(),
                    status,
                )));
                CommandAction::None
            }
            CommandResult::Compact {
                custom_instructions,
            } => CommandAction::Compact {
                custom_instructions,
            },
            CommandResult::Error(e) => {
                app.push_msg(ChatMessage::Error(e));
                CommandAction::None
            }
            CommandResult::Output(text) => {
                app.push_msg(ChatMessage::System(text));
                CommandAction::None
            }
            _ => CommandAction::None,
        };
    }

    match cmd {
        "prompt" => match arg.trim() {
            "reload" | "apply" => match runtime.reload_prompt() {
                Ok(generation) => app.push_msg(ChatMessage::System(format!(
                    "prompt applied (generation {generation})"
                ))),
                Err(error) => app.push_msg(ChatMessage::Error(format!(
                    "prompt reload rejected: {error}"
                ))),
            },
            "status" | "" => match runtime.prompt_inspection_json() {
                Some(status) => app.push_msg(ChatMessage::System(status)),
                None => app.push_msg(ChatMessage::Error("no prompt manifest is active".into())),
            },
            _ => app.push_msg(ChatMessage::Error(
                "usage: /prompt [status|reload|apply]".into(),
            )),
        },
        "clear" => {
            app.save_session().await;
            app.transcript.clear();
            app.invalidate();
            app.api_messages.clear();
            app.total_input_tokens = 0;
            app.total_output_tokens = 0;
            app.total_cache_read_tokens = 0;
            app.total_cache_creation_tokens = 0;
            app.session_cost = 0.0;
            app.input_tokens = 0;
            app.output_tokens = 0;
            app.session = Session::new(
                runtime.model(),
                runtime.thinking_level(),
                runtime.system_prompt(),
            );
            app.push_msg(ChatMessage::System("new session started".to_string()));
        }
        "model" | "models" => {
            // Non-empty args are intercepted by handle_engine_command above
            // (set + persist); only the empty-arg picker case reaches here.
            return CommandAction::OpenModels;
        }
        "effort" => {
            // Non-empty args were normalized to the engine "thinking" path
            // above; only the empty-arg lightbox case reaches here.
            return CommandAction::OpenEffort;
        }
        "system" => {
            if arg.is_empty() {
                app.push_msg(ChatMessage::System(
                    "usage: /system <prompt>  |  /system save  |  /system show".to_string(),
                ));
            } else if arg == "save" {
                let _ = std::fs::create_dir_all(synaps_cli::config::get_active_config_dir());
                match std::fs::write(system_prompt_path, runtime.system_prompt().unwrap_or("")) {
                    Ok(_) => app.push_msg(ChatMessage::System(format!(
                        "saved to {}",
                        system_prompt_path.display()
                    ))),
                    Err(e) => app.push_msg(ChatMessage::Error(format!("failed to save: {}", e))),
                }
            } else if arg == "show" {
                let prompt = runtime.system_prompt().unwrap_or("(none)");
                app.push_msg(ChatMessage::System(prompt.to_string()));
            } else {
                runtime.set_system_prompt(arg.to_string());
                app.push_msg(ChatMessage::System("system prompt updated".to_string()));
            }
        }
        "thinking" => {
            // Non-empty args are intercepted by handle_engine_command above
            // (set + persist); only the empty-arg status case reaches here.
            app.push_msg(ChatMessage::System(format!(
                "thinking: {} ({})",
                runtime.thinking_level(),
                runtime.thinking_budget()
            )));
        }
        "context" => {
            // Mirrors the settings cycler (settings/defs.rs `context_window`).
            let window = match arg {
                "200k" | "200K" => Some(Some(200_000u64)),
                "1m" | "1M" => Some(Some(1_000_000u64)),
                "auto" => Some(None),
                "" => {
                    app.push_msg(ChatMessage::System(format!(
                        "context window: {} tokens",
                        runtime.context_window()
                    )));
                    None
                }
                _ => {
                    app.push_msg(ChatMessage::Error(
                        "usage: /context 200k|1m|auto".to_string(),
                    ));
                    None
                }
            };
            if let Some(window) = window {
                runtime.set_context_window(window);
                app.last_turn_context_window = runtime.context_window();
                let canonical = arg.to_ascii_lowercase();
                let status =
                    synaps_cli::engine::commands::persist_to_config("context_window", &canonical);
                app.push_msg(ChatMessage::System(format!(
                    "context window set to: {} {}",
                    canonical, status
                )));
            }
        }
        "sessions" => match list_recent_sessions(20) {
            Ok(sessions) if sessions.is_empty() => {
                app.push_msg(ChatMessage::System("no saved sessions".to_string()));
            }
            Ok(sessions) => {
                app.push_msg(ChatMessage::System(format!(
                    "{} session(s):",
                    sessions.len()
                )));
                for s in sessions.iter().take(20) {
                    let active_marker = if s.id == app.session.id { " ●" } else { "" };
                    let name_tag = s
                        .name
                        .as_deref()
                        .map(|n| format!(" [@{}]", n))
                        .unwrap_or_default();
                    let age = {
                        let secs = chrono::Utc::now()
                            .signed_duration_since(s.created_at)
                            .num_seconds();
                        if secs < 3600 {
                            format!("{}m ago", secs / 60)
                        } else if secs < 86400 {
                            format!("{}h ago", secs / 3600)
                        } else {
                            format!("{}d ago", secs / 86400)
                        }
                    };
                    let title_display = if s.title.is_empty() {
                        "(no title)".to_string()
                    } else {
                        s.title.chars().take(60).collect::<String>()
                    };
                    // Last 4 chars — more recognizable than a prefix
                    let id_short = &s.id[s.id.len().saturating_sub(4)..];
                    let msg_str = if s.message_count > 0 {
                        format!("{} msgs · ", s.message_count)
                    } else {
                        String::new()
                    };
                    // Line 1: identity + meta
                    app.push_msg(ChatMessage::System(format!(
                        "  …{}{}{} · {}{}${:.3} · {}",
                        id_short, active_marker, name_tag,
                        msg_str, age, s.session_cost, s.model
                    )));
                    // Line 2: title
                    app.push_msg(ChatMessage::System(format!(
                        "     └ {}",
                        title_display
                    )));
                    // Blank separator between entries
                    app.push_msg(ChatMessage::System(String::new()));
                }
            }
            Err(e) => {
                app.push_msg(ChatMessage::Error(format!(
                    "failed to list sessions: {}",
                    e
                )));
            }
        },
        "resume" => {
            if arg.is_empty() {
                app.push_msg(ChatMessage::System(
                    "usage: /resume <name_or_id>".to_string(),
                ));
            } else {
                match resolve_session(arg) {
                    Ok(session) => {
                        runtime.set_model(session.model.clone());
                        // A resumed session owns its saved choice. Preserve that
                        // explicit provenance across later model switches.
                        let clamp_notice =
                            restore_session_reasoning(runtime, &session.thinking_level);
                        if let Some(ref sp) = session.system_prompt {
                            runtime.set_system_prompt(sp.clone());
                        }
                        app.save_session().await;
                        let old_id = app.session.id.clone();
                        app.transcript.clear();
                        app.invalidate();
                        app.api_messages = session.api_messages.clone();
                        app.total_input_tokens = session.total_input_tokens;
                        app.total_output_tokens = session.total_output_tokens;
                        app.session_cost = session.session_cost;
                        super::rebuild_display_messages(&session.api_messages, app);
                        let new_id = session.id.clone();
                        app.session = session;
                        if let Some(notice) = clamp_notice {
                            // Keep the session file in sync with the clamped runtime.
                            app.session.thinking_level = runtime.thinking_level().to_string();
                            app.push_msg(ChatMessage::System(notice));
                        }
                        let via = if synaps_cli::chain::load_chain(arg).is_ok() {
                            format!(" (via chain '{}')", arg)
                        } else if synaps_cli::session::find_session_by_name(arg).is_ok() {
                            format!(" (via name '{}')", arg)
                        } else {
                            String::new()
                        };
                        app.push_msg(ChatMessage::System(format!(
                            "switched from {} to {}{}",
                            old_id, new_id, via
                        )));
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
                let query = trimmed
                    .strip_prefix("find")
                    .unwrap_or("")
                    .trim()
                    .to_string();
                return CommandAction::OpenHelpFind { query };
            }

            let registry = synaps_cli::help::HelpRegistry::new(
                synaps_cli::help::builtin_entries(),
                registry.plugin_help_entries(),
            );
            if let Some(rendered) = synaps_cli::help::render_help(
                &registry,
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                },
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
        // NOTE: /compact is intercepted by handle_engine_command above
        // (CommandResult::Compact carries the custom instructions).
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
                    return CommandAction::ChainName {
                        name: rest.to_string(),
                    };
                }
                "unname" | "rm" => {
                    if rest.is_empty() {
                        app.push_msg(ChatMessage::System("usage: /chain unname <name>".into()));
                        return CommandAction::None;
                    }
                    return CommandAction::ChainUnname {
                        name: rest.to_string(),
                    };
                }
                _ => {
                    app.push_msg(ChatMessage::Error(format!(
                        "unknown /chain subcommand: {}",
                        sub
                    )));
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
                    return CommandAction::ExtensionsConfig {
                        id: Some(rest.to_string()),
                    };
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
                            return CommandAction::ExtensionsTrust(ExtensionsTrustAction::Enable {
                                runtime_id: trest.to_string(),
                            });
                        }
                        "disable" => {
                            if trest.is_empty() {
                                app.push_msg(ChatMessage::System(
                                    "usage: /extensions trust disable <runtime_id> [reason]"
                                        .to_string(),
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
        "stats" => {
            let receipt = build_stats_receipt(app, runtime);
            app.push_msg(ChatMessage::System(receipt));
            return CommandAction::None;
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
                let render_disambig =
                    |verb: &str,
                     claims: &[synaps_cli::skills::registry::LifecycleClaim]|
                     -> String {
                        let mut sorted: Vec<_> = claims.iter().collect();
                        sorted.sort_by(|a, b| a.plugin.cmp(&b.plugin));
                        let plugins = sorted
                            .iter()
                            .map(|c| c.plugin.clone())
                            .collect::<Vec<_>>()
                            .join(", ");
                        let cmds = sorted
                            .iter()
                            .map(|c| format!("/{}", c.command))
                            .collect::<Vec<_>>()
                            .join(", ");
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
                            return CommandAction::SidecarToggle {
                                plugin_id: Some(c.plugin.clone()),
                            };
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
                            return CommandAction::SidecarStatus {
                                plugin_id: Some(c.plugin.clone()),
                            };
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
                        sorted
                            .iter()
                            .map(|c| c.plugin.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    app.push_msg(ChatMessage::Error(format!(
                        "unknown sidecar plugin: '{}' (loaded: {})",
                        plugin_id, list
                    )));
                    return CommandAction::None;
                }
                match rest.as_str() {
                    "toggle" => {
                        return CommandAction::SidecarToggle {
                            plugin_id: Some(plugin_id.to_string()),
                        }
                    }
                    "status" => {
                        return CommandAction::SidecarStatus {
                            plugin_id: Some(plugin_id.to_string()),
                        }
                    }
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
                app.push_msg(ChatMessage::System(
                    "No plugin or user keybinds registered.".to_string(),
                ));
            } else {
                let mut lines = vec!["Keybinds:".to_string()];
                for bind in &custom {
                    let key = synaps_cli::skills::keybinds::format_key(&bind.key);
                    let source = match &bind.source {
                        synaps_cli::skills::keybinds::KeybindSource::Plugin(name) => {
                            format!(" ({})", name)
                        }
                        synaps_cli::skills::keybinds::KeybindSource::User => " (user)".to_string(),
                        _ => String::new(),
                    };
                    lines.push(format!("  {:18} {}{}", key, bind.description, source));
                }
                app.push_msg(ChatMessage::System(lines.join("\n")));
            }
        }
        _ => match registry.resolve(cmd) {
            Resolution::Skill(skill) => {
                return CommandAction::LoadSkill {
                    skill,
                    arg: arg.to_string(),
                };
            }
            Resolution::PluginCommand(command) => {
                return CommandAction::PluginCommand {
                    command,
                    arg: arg.to_string(),
                };
            }
            Resolution::Ambiguous(opts) => {
                app.push_msg(ChatMessage::Error(format!(
                    "ambiguous command /{}; try one of: {}",
                    cmd,
                    opts.iter()
                        .map(|o| format!("/{}", o))
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            Resolution::Builtin | Resolution::Unknown => {
                app.push_msg(ChatMessage::Error(format!("unknown command: /{}", cmd)));
            }
        },
    }
    CommandAction::None
}

/// Build the /stats session receipt string.
///
/// Returns a multi-line string ready to be pushed as `ChatMessage::System`.
/// Exported for unit testing.
pub(crate) fn build_stats_receipt(app: &App, runtime: &Runtime) -> String {
    use synaps_cli::pricing::calculate_cost_split;

    let model = runtime.model().to_string();

    // ── Session identity ──────────────────────────────────────────────────
    let session_id = &app.session.id;
    let created_at = app.session.created_at;
    let now = chrono::Utc::now();
    let elapsed = now.signed_duration_since(created_at);
    let duration_str = {
        let secs = elapsed.num_seconds().max(0) as u64;
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        if h > 0 {
            format!("{}h {:02}m {:02}s", h, m, s)
        } else if m > 0 {
            format!("{}m {:02}s", m, s)
        } else {
            format!("{}s", s)
        }
    };
    // Count user/assistant exchanges (each user msg is one turn).
    let turn_count = app
        .transcript
        .messages()
        .iter()
        .filter(|m| matches!(m.msg, ChatMessage::User(_)))
        .count();

    // ── Token totals ──────────────────────────────────────────────────────
    let input = app.total_input_tokens;
    let output = app.total_output_tokens;
    let c_read = app.total_cache_read_tokens;
    let c_write = app.total_cache_creation_tokens;
    let c5 = app.total_cache_write_5m;
    let c1 = app.total_cache_write_1h;

    // ── Cache hit-rate (mirrors the footer formula) ───────────────────────
    let total_input_all = input + c_read + c_write;
    let hit_rate_str = if total_input_all > 0 && c_read > 0 {
        let pct = (c_read as f64 / total_input_all as f64 * 100.0) as u32;
        format!("{}%", pct)
    } else {
        "0%".to_string()
    };

    // ── Cache savings ─────────────────────────────────────────────────────
    // What the cache-read tokens WOULD have cost at full input rate minus what
    // they actually cost at 0.1× — computed via calculate_cost_split directly.
    // Savings = cost_if_uncached − actual_read_cost
    //         = cost_split(model, c_read, 0, 0, 0, 0) − cost_split(model, 0, 0, c_read, 0, 0)
    let savings = if c_read > 0 {
        let full_price = calculate_cost_split(&model, c_read, 0, 0, 0, 0);
        let actual = calculate_cost_split(&model, 0, 0, c_read, 0, 0);
        full_price - actual
    } else {
        0.0
    };

    // ── Cost ─────────────────────────────────────────────────────────────
    let session_cost = app.session_cost;

    // ── Format ───────────────────────────────────────────────────────────
    let mut lines = vec![
        "─── Session Stats ───────────────────────────────".to_string(),
        format!(
            "  Session  {:>12}  model: {}",
            &session_id[..session_id.len().min(12)],
            model
        ),
        format!("  Duration {:>12}  turns: {}", duration_str, turn_count),
        String::new(),
        "  Tokens".to_string(),
        format!("    input        {:>10}", fmt_tokens(input)),
        format!("    output       {:>10}", fmt_tokens(output)),
        format!("    cache read   {:>10}", fmt_tokens(c_read)),
    ];

    if c5 > 0 || c1 > 0 {
        lines.push(format!(
            "    cache write  {:>10}  (5m: {} / 1h: {})",
            fmt_tokens(c_write),
            fmt_tokens(c5),
            fmt_tokens(c1)
        ));
    } else {
        lines.push(format!("    cache write  {:>10}", fmt_tokens(c_write)));
    }

    lines.push(String::new());
    lines.push(format!(
        "  Cache  hit rate {:>9}  saved: ${:.4}",
        hit_rate_str, savings
    ));
    lines.push(String::new());
    lines.push(format!("  Cost   ${:.4}", session_cost));
    lines.push("─────────────────────────────────────────────────".to_string());
    lines.join("\n")
}

/// Format a token count with K/M suffix.
fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
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
            let arg = full_input[1..]
                .split_once(' ')
                .map(|x| x.1)
                .unwrap_or("")
                .trim();
            app.handle_theme_command(arg);
            CommandAction::None
        }
        "quit" | "exit" => CommandAction::Quit,
        _ => CommandAction::None, // unknown — handled by caller as steer/queue
    }
}

/// Notice shown when a model change forced a reasoning-level substitution.
pub(crate) fn reasoning_clamp_notice(
    clamp: &synaps_cli::runtime::ReasoningClamp,
    model: &str,
) -> String {
    format!(
        "thinking → {} (clamped from {}: not supported by {})",
        clamp.to.as_str(),
        clamp.from.as_str(),
        model
    )
}

#[cfg(test)]
mod tests {
    use super::command_output_event_to_chat_message;
    use super::{
        build_stats_receipt, edit_distance, execute_command_action,
        execute_interactive_plugin_command_events, fuzzy_match, handle_command, resolve_prefix,
        restore_session_reasoning, CommandAction, ExtensionsMemoryAction, ExtensionsTrustAction,
    };
    use crate::tui::app::ChatMessage;
    use async_trait::async_trait;
    use serde_json::Value;
    use std::path::PathBuf;
    use std::sync::Arc;
    use synaps_cli::extensions::commands::CommandOutputEvent;
    use synaps_cli::skills::manifest::ManifestSkillPromptCommand;
    use synaps_cli::skills::registry::{
        CommandRegistry, RegisteredPluginCommand, RegisteredPluginCommandBackend,
    };
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
        let mut app = crate::tui::app::App::new(synaps_cli::Session::new("test", "medium", None));
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
        )
        .await
        {
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
        fn name(&self) -> &str {
            "policy:echo"
        }
        fn description(&self) -> &str {
            "echo"
        }
        fn parameters(&self) -> Value {
            serde_json::json!({"type":"object"})
        }
        async fn execute(&self, params: Value, _ctx: ToolContext) -> synaps_cli::Result<String> {
            Ok(format!(
                "echo {}",
                params["text"].as_str().unwrap_or_default()
            ))
        }
    }

    #[tokio::test]
    async fn plugin_command_action_executes_extension_tool_and_prints_result() {
        let mut tools = ToolRegistry::without_subagent();
        tools.register(Arc::new(EchoTool));
        let mut runtime = synaps_cli::Runtime::new().await.unwrap();
        runtime.set_tools(tools);
        let mut app = crate::tui::app::App::new(synaps_cli::Session::new("test", "medium", None));
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
            CommandAction::PluginCommand {
                command,
                arg: "hello".to_string(),
            },
            &mut app,
            &runtime,
        )
        .await;

        let last = app
            .transcript
            .messages()
            .last()
            .expect("system message should be pushed");
        match &last.msg {
            crate::tui::app::ChatMessage::System(text) => {
                assert!(
                    text.contains("plugin command /policy:echo exited with 0"),
                    "{text}"
                );
                assert!(text.contains("stdout:\necho hello"), "{text}");
            }
            _ => panic!("expected system message"),
        }
    }

    #[test]
    fn command_output_event_text_becomes_chat_text() {
        let msg = command_output_event_to_chat_message(CommandOutputEvent::Text {
            content: "hello".to_string(),
        })
        .expect("text event should produce chat message");
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
        })
        .expect("table event should produce chat message");
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
            theme_tokens: Default::default(),
            // Legacy EAGER lifecycle by declaration (Task 20): this fixture
            // loads a live process at `load` time, so `deferred: None` is
            // the semantically correct pre-Task-20-compatible value.
            deferred: None,
            protocol_version: 1,
            runtime: synaps_cli::extensions::manifest::ExtensionRuntime::Process,
            command: "python3".to_string(),
            setup: None,
            prebuilt: ::std::collections::HashMap::new(),
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
        let mut app = crate::tui::app::App::new(synaps_cli::Session::new("test", "medium", None));

        execute_interactive_plugin_command_events(&command, "models", &manager, &mut app).await;

        assert!(app.transcript.messages().iter().any(
            |m| matches!(&m.msg, ChatMessage::Text(text) if text.contains("hello from demo"))
        ));
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
            "clear".into(),
            "compact".into(),
            "chain".into(),
            "model".into(),
            "models".into(),
            "system".into(),
            "thinking".into(),
            "context".into(),
            "sessions".into(),
            "resume".into(),
            "saveas".into(),
            "theme".into(),
            "gamba".into(),
            "help".into(),
            "quit".into(),
            "exit".into(),
            "settings".into(),
            "plugins".into(),
            "status".into(),
            "stats".into(),
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
        let mut app = crate::tui::app::App::new(synaps_cli::Session::new("test", "medium", None));
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
        )
        .await
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
            CommandAction::ExtensionsTrust(ExtensionsTrustAction::Disable {
                runtime_id,
                reason,
            }) => {
                assert_eq!(runtime_id, "plug:prov");
                assert_eq!(reason.as_deref(), Some("untrusted vendor"));
            }
            _ => panic!("expected ExtensionsTrust(Disable) with reason"),
        }
    }

    #[tokio::test]
    async fn parse_extensions_trust_disable_no_reason() {
        match invoke_extensions("trust disable plug:prov").await {
            CommandAction::ExtensionsTrust(ExtensionsTrustAction::Disable {
                runtime_id,
                reason,
            }) => {
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
            CommandAction::ExtensionsMemory(ExtensionsMemoryAction::Recent {
                namespace,
                limit,
            }) => {
                assert_eq!(namespace, "my-ns");
                assert_eq!(limit, None);
            }
            _ => panic!("expected ExtensionsMemory(Recent) with no limit"),
        }
    }

    #[tokio::test]
    async fn parse_extensions_memory_recent_with_limit() {
        match invoke_extensions("memory recent my-ns 5").await {
            CommandAction::ExtensionsMemory(ExtensionsMemoryAction::Recent {
                namespace,
                limit,
            }) => {
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
        let mut app = crate::tui::app::App::new(synaps_cli::Session::new("test", "medium", None));
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
            other => panic!(
                "expected SidecarToggle with plugin_id, got {:?}",
                std::mem::discriminant(&other)
            ),
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
        let mut app = crate::tui::app::App::new(synaps_cli::Session::new("test", "medium", None));
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
        let mut app = crate::tui::app::App::new(synaps_cli::Session::new("test", "medium", None));
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
            other => panic!(
                "expected SidecarStatus with plugin_id, got {:?}",
                std::mem::discriminant(&other)
            ),
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
        let mut app = crate::tui::app::App::new(synaps_cli::Session::new("test", "medium", None));
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
        let pushed_legacy_error = app.transcript.messages().iter().any(|m| matches!(&m.msg, crate::tui::app::ChatMessage::Error(s) if s.contains("no plugin owns /capture")));
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
        let mut app = crate::tui::app::App::new(synaps_cli::Session::new("test", "medium", None));
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
        let pushed = app.transcript.messages().iter().any(|m| matches!(&m.msg, crate::tui::app::ChatMessage::Error(s) if s.contains("unknown /capture subcommand")));
        assert!(pushed);
    }

    // ---- Phase 8 slices 8A.6 / 8A.7 — `/sidecar` ambiguity-aware dispatcher ----

    async fn invoke_sidecar_with_plugins(
        arg: &str,
        plugins: Vec<synaps_cli::skills::Plugin>,
    ) -> (CommandAction, crate::tui::app::App) {
        let mut app = crate::tui::app::App::new(synaps_cli::Session::new("test", "medium", None));
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
        let pushed_err = app
            .transcript
            .messages()
            .iter()
            .any(|m| matches!(&m.msg, crate::tui::app::ChatMessage::Error(_)));
        assert!(!pushed_err, "no errors expected for zero-claim back-compat");
    }

    #[tokio::test]
    async fn sidecar_toggle_with_one_claim_dispatches_with_hint() {
        let (action, app) = invoke_sidecar_with_plugins(
            "toggle",
            vec![lifecycle_plugin("sample-sidecar", "capture")],
        )
        .await;
        assert!(matches!(action, CommandAction::SidecarToggle { .. }));
        let pushed_hint = app.transcript.messages().iter().any(|m| matches!(&m.msg, crate::tui::app::ChatMessage::System(s) if s.contains("try /capture toggle")));
        assert!(
            pushed_hint,
            "expected a System hint mentioning `try /capture toggle`"
        );
    }

    #[tokio::test]
    async fn sidecar_toggle_with_two_claims_errors_with_disambiguation() {
        let (action, app) = invoke_sidecar_with_plugins(
            "toggle",
            vec![
                lifecycle_plugin("sample-sidecar", "capture"),
                lifecycle_plugin("local-ocr", "ocr"),
            ],
        )
        .await;
        assert!(matches!(action, CommandAction::None));
        let pushed = app.transcript.messages().iter().find_map(|m| match &m.msg {
            crate::tui::app::ChatMessage::Error(s) => Some(s.clone()),
            _ => None,
        });
        let s = pushed.expect("expected an Error message");
        assert!(
            s.contains("sample-sidecar"),
            "error should list sample-sidecar; got: {s}"
        );
        assert!(
            s.contains("local-ocr"),
            "error should list local-ocr; got: {s}"
        );
        assert!(
            s.contains("/capture"),
            "error should mention /capture; got: {s}"
        );
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
        )
        .await;
        assert!(matches!(action, CommandAction::SidecarToggle { .. }));
        let pushed_err = app
            .transcript
            .messages()
            .iter()
            .any(|m| matches!(&m.msg, crate::tui::app::ChatMessage::Error(_)));
        assert!(!pushed_err, "no errors expected for valid qualified form");
    }

    #[tokio::test]
    async fn sidecar_qualified_unknown_plugin_id_errors() {
        let (action, app) = invoke_sidecar_with_plugins(
            "nonexistent toggle",
            vec![lifecycle_plugin("sample-sidecar", "capture")],
        )
        .await;
        assert!(matches!(action, CommandAction::None));
        let pushed = app.transcript.messages().iter().any(|m| matches!(&m.msg, crate::tui::app::ChatMessage::Error(s) if s.contains("unknown sidecar plugin")));
        assert!(pushed, "expected `unknown sidecar plugin` error");
    }

    #[tokio::test]
    async fn sidecar_qualified_plugin_id_status() {
        let (action, _app) = invoke_sidecar_with_plugins(
            "sample-sidecar status",
            vec![lifecycle_plugin("sample-sidecar", "capture")],
        )
        .await;
        assert!(matches!(action, CommandAction::SidecarStatus { .. }));
    }

    #[tokio::test]
    async fn sidecar_qualified_plugin_id_unknown_subcommand_errors() {
        let (action, app) = invoke_sidecar_with_plugins(
            "sample-sidecar bogus",
            vec![lifecycle_plugin("sample-sidecar", "capture")],
        )
        .await;
        assert!(matches!(action, CommandAction::None));
        let pushed = app.transcript.messages().iter().any(|m| matches!(&m.msg, crate::tui::app::ChatMessage::Error(s) if s.contains("unknown /sidecar subcommand")));
        assert!(pushed, "expected `unknown /sidecar subcommand` error");
    }

    // ── /stats unit tests ─────────────────────────────────────────────────

    /// Cache savings = (full_input_price − read_price) for the cache-read tokens.
    /// For Sonnet at $3/Mtok: 1M read tokens saved = $3.00 − $0.30 = $2.70.
    #[test]
    fn cache_savings_formula_sonnet() {
        use synaps_cli::pricing::calculate_cost_split;
        let model = "claude-sonnet-4-5";
        let c_read: u64 = 1_000_000;
        let full_price = calculate_cost_split(model, c_read, 0, 0, 0, 0); // at input rate
        let actual = calculate_cost_split(model, 0, 0, c_read, 0, 0); // at 0.1× rate
        let savings = full_price - actual;
        // $3.00 − $0.30 = $2.70
        assert!(
            (savings - 2.70).abs() < 1e-9,
            "expected $2.70, got ${savings}"
        );
    }

    /// Zero cache reads → zero savings.
    #[test]
    fn cache_savings_zero_when_no_reads() {
        use synaps_cli::pricing::calculate_cost_split;
        let model = "claude-sonnet-4-5";
        let full_price = calculate_cost_split(model, 0, 0, 0, 0, 0);
        let actual = calculate_cost_split(model, 0, 0, 0, 0, 0);
        let savings = full_price - actual;
        assert_eq!(savings, 0.0);
    }

    /// /stats receipt contains expected section headers.
    #[tokio::test]
    #[serial_test::serial]
    async fn stats_receipt_contains_expected_sections() {
        let session = synaps_cli::Session::new("claude-sonnet-4-5", "medium", None);
        let mut app = crate::tui::app::App::new(session);
        // Inject some usage so the receipt shows non-zero numbers.
        app.total_input_tokens = 1000;
        app.total_output_tokens = 500;
        app.total_cache_read_tokens = 2000;
        app.total_cache_creation_tokens = 800;
        app.total_cache_write_5m = 600;
        app.total_cache_write_1h = 200;
        app.session_cost = 0.0042;

        let runtime = synaps_cli::Runtime::new().await.unwrap();
        let receipt = build_stats_receipt(&app, &runtime);

        assert!(receipt.contains("Session Stats"), "missing header");
        assert!(receipt.contains("Tokens"), "missing Tokens section");
        assert!(receipt.contains("Cache"), "missing Cache section");
        assert!(receipt.contains("Cost"), "missing Cost section");
        assert!(receipt.contains("saved:"), "missing savings line");
        assert!(receipt.contains("5m:"), "missing 5m split");
        assert!(receipt.contains("1h:"), "missing 1h split");
    }

    #[tokio::test]
    async fn resume_restores_ultra_with_explicit_provenance() {
        let mut runtime = synaps_cli::Runtime::new().await.unwrap();
        runtime.set_model("openai-codex/gpt-5.6-sol".to_string());

        restore_session_reasoning(&mut runtime, "ultra");

        assert_eq!(
            runtime.reasoning_level(),
            agent_core::reasoning::ReasoningLevel::Ultra
        );
        assert!(runtime.is_reasoning_explicit());
        runtime.set_model("openai-codex/gpt-5.6-terra".to_string());
        assert_eq!(
            runtime.reasoning_level(),
            agent_core::reasoning::ReasoningLevel::Ultra,
            "restored explicit Ultra must survive model switches"
        );
    }

    #[tokio::test]
    async fn resume_clamps_unsupported_saved_level() {
        let mut runtime = synaps_cli::Runtime::new().await.unwrap();
        runtime.set_model("xai-auth/grok-4.6".to_string());

        let notice = restore_session_reasoning(&mut runtime, "xhigh");

        assert!(notice.is_some(), "clamp must be surfaced");
        assert_eq!(
            runtime.reasoning_level(),
            agent_core::reasoning::ReasoningLevel::High
        );
        assert!(runtime.is_reasoning_explicit());
    }
}
