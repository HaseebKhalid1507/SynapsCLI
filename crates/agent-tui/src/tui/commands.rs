//! Slash command handling — dispatches /clear, /model, /system, etc.

use std::path::Path;

use chrono;
use synaps_cli::list_recent_sessions;

use super::app::{App, ChatMessage};
use synaps_cli::extensions::commands::CommandOutputEvent;
use synaps_cli::extensions::runtime::InvokeCommandEvent;

/// All recognized built-in slash commands. Source of truth for the
/// built-in surface; the runtime merges this with discovered skills via
/// `CommandRegistry::all_commands()` for autocomplete and prefix resolution.
#[allow(dead_code)]
/// Commands that work while streaming.
pub(super) const STREAMING_COMMANDS: &[&str] = &[
    "gamba", "theme", "quit", "exit", "attachments", "detach", "watch", "settings", "clients", "takeover",
];

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
    /// `/clients` typed mid-stream: [`clients`] needs the session link,
    /// which [`handle_streaming_command`] doesn't have.
    Clients,
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
    /// `/takeover` typed mid-stream: [`takeover`] needs the session link,
    /// which [`handle_streaming_command`] doesn't have.
    TakeOver,
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
    link: &mut super::session_link::SessionLink,
) {
    if let CommandAction::PluginCommand { command, arg } = action {
        // Tools-backed plugin command runs on THE runtime's tool set
        // (actor_cmds::plugin_command); reply = {status, stdout, stderr}.
        match link
            .plugin_command(&command.plugin, &command.name, &arg)
            .await
        {
            Ok(value) if value["kind"] == "plugin_output" => {
                let status = value["status"]
                    .as_i64()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_string());
                let stdout = value["stdout"].as_str().unwrap_or("");
                let stderr = value["stderr"].as_str().unwrap_or("");
                let mut lines = vec![format!(
                    "plugin command /{}:{} exited with {}",
                    command.plugin, command.name, status
                )];
                if !stdout.trim().is_empty() {
                    lines.push(format!("stdout:\n{}", stdout.trim_end()));
                }
                if !stderr.trim().is_empty() {
                    lines.push(format!("stderr:\n{}", stderr.trim_end()));
                }
                app.push_msg(ChatMessage::System(lines.join("\n")));
            }
            Ok(value) => app.push_msg(ChatMessage::Error(format!(
                "plugin command failed: {}",
                value["text"].as_str().unwrap_or("unknown error")
            ))),
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

    // Settings editors also use this helper. They are NOT driver activation
    // surfaces; only the explicit slash-command task consumes the return value.
    let _ =
        apply_interactive_command_result(plugin_extension_id, command_name, result, report, app);
}

pub(crate) fn apply_interactive_command_result(
    plugin_extension_id: &str,
    command_name: &str,
    result: Result<serde_json::Value, String>,
    report: synaps_cli::extensions::invoke_output::InvokeOutputReport,
    app: &mut App,
) -> Option<serde_json::Value> {
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
        if notice.severity_error {
            app.push_msg(ChatMessage::Error(notice.message));
        } else {
            app.push_msg(ChatMessage::System(notice.message));
        }
    }

    match result {
        Ok(value) => Some(value),
        Err(err) => {
            app.push_msg(ChatMessage::Error(format!(
                "interactive plugin command {}:{} failed: {}",
                plugin_extension_id, command_name, err
            )));
            None
        }
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

/// `/pin` in an in-process TUI: the session ends with this process anyway.
pub(super) const PIN_IN_PROCESS: &str = "pin: only applies to a daemon session (synaps --attach)";

pub(super) const TAKEOVER_IN_PROCESS: &str =
    "takeover: nothing to take over; an in-process session has one client and it owns input";
pub(super) const TAKEOVER_OBSERVING: &str =
    "takeover: this client is observing (--observe) and never owns input; attach without --observe to take it";
pub(super) const TAKEOVER_ALREADY: &str = "takeover: you already own input";
pub(super) const TAKEOVER_SHARED: &str = "takeover: input is shared, so every client can already type; \
     this one is now the active client (it gets input back first after a daemon reload)";
pub(super) const TAKEOVER_SHARED_ALREADY: &str =
    "takeover: input is shared, so every client can already type; this one is already the active client";

/// `/takeover`: take input ownership from the client that holds it, in
/// place (the in-session form of `synaps --attach --takeover`). The session
/// answers with `InputOwnerChanged` ("you own input" toast; the old owner
/// gets "input taken over") or, from an observer, `Refused`. Works mid-turn:
/// the turn keeps running, only who drives the session changes.
pub(super) async fn takeover(app: &mut App, link: &mut super::session_link::SessionLink) {
    let t = link.transport();
    let shared = t.client_input() == agent_engine::session::ClientInput::Shared;
    let note = if t.daemon_pid() == 0 {
        Some(TAKEOVER_IN_PROCESS)
    } else if t.mode() == agent_engine::session::AttachMode::Observe {
        Some(TAKEOVER_OBSERVING)
    } else if t.input_owner() == Some(t.client_id()) {
        Some(if shared { TAKEOVER_SHARED_ALREADY } else { TAKEOVER_ALREADY })
    } else {
        None
    };
    if let Some(note) = note {
        app.push_msg(ChatMessage::System(note.to_string()));
        return;
    }
    match link.send(agent_engine::session::SessionCommand::TakeInput).await {
        // Shared mode shows no toast when the active client moves: say it here.
        Ok(()) if shared => app.push_msg(ChatMessage::System(TAKEOVER_SHARED.to_string())),
        Ok(()) => {}
        Err(e) => app.push_msg(ChatMessage::Error(format!("takeover failed: {e}"))),
    }
}

/// `/pin [on|off|status]`, alias `/keep-warm on|off`: the daemon session's
/// pin (`KeepWarm`), not `/keepwarm` (prompt-cache pings). `on|off` is sent
/// to the session, whose notice (or refusal, for a non-owner) arrives as an
/// event. The status asks the session (`Status.pinned`) rather than track it
/// here: another client or `synaps daemon pin` may have changed it.
async fn pin_command(cmd: &str, arg: &str, app: &mut App, link: &mut super::session_link::SessionLink) {
    if link.transport().daemon_pid() == 0 {
        app.push_msg(ChatMessage::System(PIN_IN_PROCESS.to_string()));
        return;
    }
    match arg.trim() {
        a @ ("on" | "off") => {
            let on = a == "on";
            if let Err(e) = link.send(agent_engine::session::SessionCommand::KeepWarm { on }).await {
                app.push_msg(ChatMessage::Error(format!("pin failed: {e}")));
            }
        }
        "" | "status" => match link.query(agent_engine::session::SessionQuery::Status).await {
            Ok(value) => match value["pinned"].as_bool() {
                Some(true) => app.push_msg(ChatMessage::System(
                    agent_engine::session::actor::PIN_ON_NOTICE.to_string(),
                )),
                Some(false) => app.push_msg(ChatMessage::System(
                    "not pinned: this session parks when idle. /pin on keeps it loaded.".to_string(),
                )),
                None => app.push_msg(ChatMessage::Error(
                    "pin: this daemon does not report the pin; `synaps daemon reload` updates it".to_string(),
                )),
            },
            Err(e) => app.push_msg(ChatMessage::Error(format!("pin failed: {e}"))),
        },
        _ => app.push_msg(ChatMessage::Error(format!("usage: /{cmd} [on|off|status]"))),
    }
}

/// `/clients` in an in-process session: there is no daemon, only this TUI.
pub(super) const CLIENTS_IN_PROCESS: &str = "clients: in-process session; this TUI is its only client";

/// `/clients`: every client attached to this session, from the transport's
/// roster (attach snapshot + `ClientJoined`/`ClientLeft`), one line each:
/// name and id, kind, `drive`/`observe`, who owns input (with
/// `client_input = shared`: who is active), which is you, and how long it
/// has been attached.
pub(super) fn clients(app: &mut App, link: &super::session_link::SessionLink) {
    let t = link.transport();
    if t.daemon_pid() == 0 {
        app.push_msg(ChatMessage::System(CLIENTS_IN_PROCESS.to_string()));
        return;
    }
    let roster = t.roster();
    let shared = t.client_input() == agent_engine::session::ClientInput::Shared;
    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let n = roster.clients().len();
    let mut lines = vec![format!(
        "{n} client{} attached{}",
        if n == 1 { "" } else { "s" },
        if shared { " · input shared (every drive client types; active = typed last)" } else { "" }
    )];
    for c in roster.clients() {
        let mut row = format!(
            "  {:<20} {:<7} {}",
            c.label(),
            c.kind.default_name(),
            if c.mode == agent_engine::session::AttachMode::Observe { "observe" } else { "drive" },
        );
        if t.input_owner() == Some(c.id) {
            // Shared input: everyone (but observers) types; this one did last.
            row.push_str(if shared { " · active" } else { " · owns input" });
        }
        if c.id == t.client_id() {
            row.push_str(" · you");
        }
        if c.attached_at_ms > 0 {
            row.push_str(&format!(" · {}", attached_for(now_ms.saturating_sub(c.attached_at_ms))));
        }
        lines.push(row);
    }
    app.push_msg(ChatMessage::System(lines.join("\n")));
}

/// `attached 3m`, `attached 2h 5m`: how long a client has been attached.
fn attached_for(ms: u64) -> String {
    let mins = ms / 60_000;
    match mins {
        0 => "attached just now".to_string(),
        1..=59 => format!("attached {mins}m"),
        _ => format!("attached {}h {}m", mins / 60, mins % 60),
    }
}

/// Handle a slash command when NOT streaming. Its notices show in the view
/// it was typed in ([`super::watch::begin_command`]).
pub(super) async fn handle_command(
    cmd: &str,
    arg: &str,
    app: &mut App,
    link: &mut super::session_link::SessionLink,
    system_prompt_path: &Path,
    registry: &std::sync::Arc<synaps_cli::skills::registry::CommandRegistry>,
    keybind_registry: &synaps_cli::skills::keybinds::KeybindRegistry,
) -> CommandAction {
    let prev = super::watch::begin_command(app);
    let action = run_command(cmd, arg, app, link, system_prompt_path, registry, keybind_registry).await;
    super::watch::end_command(app, prev);
    action
}

async fn run_command(
    cmd: &str,
    arg: &str,
    app: &mut App,
    link: &mut super::session_link::SessionLink,
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
        match link
            .query(agent_engine::session::SessionQuery::ContextReport)
            .await
        {
            Ok(value) => {
                if let Some(text) = value["text"].as_str() {
                    app.push_msg(ChatMessage::System(text.to_string()));
                } else if let Some(e) = value["error"].as_str() {
                    app.push_msg(ChatMessage::Error(e.to_string()));
                }
            }
            Err(e) => app.push_msg(ChatMessage::Error(e)),
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
    // `/compact` keeps the TUI's own guards + `SessionCommand::Compact`
    // (dispatch.rs Compact arm) instead of the actor's chat-style path.
    if let Some(synaps_cli::engine::commands::CommandResult::Compact {
        custom_instructions,
    }) = synaps_cli::engine::commands::evaluate_engine_command(cmd, arg)
    {
        return CommandAction::Compact {
            custom_instructions,
        };
    }
    if synaps_cli::engine::commands::evaluate_engine_command(cmd, arg).is_some()
        || matches!(cmd, "context" | "trace" | "memory" | "workers" | "confirms" | "auth" | "close")
    {
        use synaps_cli::engine::commands::persist_to_config;
        let value = match link.engine_command(cmd, arg).await {
            Ok(v) => v,
            Err(e) => {
                app.push_msg(ChatMessage::Error(e));
                return CommandAction::None;
            }
        };
        let text = |v: &serde_json::Value| v["text"].as_str().unwrap_or("").to_string();
        // `event` carries the structured kind (model_changed/thinking_changed)
        // alongside the chat-facing `kind`/`text`.
        let key = value["event"]
            .as_str()
            .or_else(|| value["kind"].as_str())
            .unwrap_or("none");
        return match key {
            "quit" => CommandAction::Quit,
            "model_changed" => {
                // The runtime's cleaned model string, not the raw arg.
                let applied = value["model"].as_str().unwrap_or("").to_string();
                app.session.model = applied.clone();
                let status = persist_to_config("model", &applied);
                app.push_msg(ChatMessage::System(format!(
                    "model set to: {} {}",
                    applied, status
                )));
                // Session-only: the user's configured thinking value stays theirs.
                if let (Some(from), Some(to)) = (
                    value["clamp"]["from"].as_str(),
                    value["clamp"]["to"].as_str(),
                ) {
                    app.session.thinking_level = to.to_string();
                    app.push_msg(ChatMessage::System(reasoning_clamp_notice_wire(
                        &agent_engine::session::ReasoningClampWire {
                            from: from.to_string(),
                            to: to.to_string(),
                        },
                        &applied,
                    )));
                }
                CommandAction::None
            }
            "thinking_changed" => {
                let config_value = value["config_value"].as_str().unwrap_or("").to_string();
                let level = value["level"].as_str().unwrap_or("").to_string();
                app.session.thinking_level = config_value.clone();
                let status = persist_to_config("thinking", &config_value);
                app.push_msg(ChatMessage::System(format!(
                    "thinking set to: {} {}",
                    level, status,
                )));
                CommandAction::None
            }
            "compact" => CommandAction::Compact {
                custom_instructions: value["instructions"].as_str().map(str::to_string),
            },
            "error" => {
                app.push_msg(ChatMessage::Error(text(&value)));
                CommandAction::None
            }
            "output" => {
                app.push_msg(ChatMessage::System(text(&value)));
                CommandAction::None
            }
            _ => CommandAction::None,
        };
    }

    match cmd {
        "prompt" => match arg.trim() {
            "reload" | "apply" => match link
                .set_checked(agent_engine::session::SessionSetting::ReloadPrompt)
                .await
            {
                Ok(applied) => app.push_msg(ChatMessage::System(format!(
                    "prompt applied (generation {})",
                    applied.view.prompt_generation
                ))),
                Err(error) => app.push_msg(ChatMessage::Error(format!(
                    "prompt reload rejected: {error}"
                ))),
            },
            "status" | "" => match link.view().prompt_inspection.clone() {
                Some(status) => app.push_msg(ChatMessage::System(status)),
                None => app.push_msg(ChatMessage::Error("no prompt manifest is active".into())),
            },
            _ => app.push_msg(ChatMessage::Error(
                "usage: /prompt [status|reload|apply]".into(),
            )),
        },
        "clear" => {
            // The actor saves + clears; presentation on `Cleared` +
            // `Conversation` (stream_handler).
            if let Err(e) = link
                .send(agent_engine::session::SessionCommand::NewSession)
                .await
            {
                app.push_msg(ChatMessage::Error(format!("clear failed: {e}")));
            }
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
                match std::fs::write(
                    system_prompt_path,
                    link.view().system_prompt.as_deref().unwrap_or(""),
                ) {
                    Ok(_) => app.push_msg(ChatMessage::System(format!(
                        "saved to {}",
                        system_prompt_path.display()
                    ))),
                    Err(e) => app.push_msg(ChatMessage::Error(format!("failed to save: {}", e))),
                }
            } else if arg == "show" {
                let prompt = link
                    .view()
                    .system_prompt
                    .clone()
                    .unwrap_or_else(|| "(none)".to_string());
                app.push_msg(ChatMessage::System(prompt));
            } else {
                match link
                    .set_checked(agent_engine::session::SessionSetting::SystemPrompt {
                        text: arg.to_string(),
                    })
                    .await
                {
                    Ok(_) => {
                        app.push_msg(ChatMessage::System("system prompt updated".to_string()))
                    }
                    Err(e) => app.push_msg(ChatMessage::Error(e)),
                }
            }
        }
        "thinking" => {
            // Non-empty args are intercepted by handle_engine_command above
            // (set + persist); only the empty-arg status case reaches here.
            app.push_msg(ChatMessage::System(format!(
                "thinking: {} ({})",
                link.view().thinking_level,
                link.view().thinking_budget
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
                        link.view().context_window
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
                match link
                    .set_checked(agent_engine::session::SessionSetting::ContextWindow {
                        tokens: window,
                    })
                    .await
                {
                    Ok(applied) => app.last_turn_context_window = applied.view.context_window,
                    Err(e) => {
                        app.push_msg(ChatMessage::Error(e));
                        return CommandAction::None;
                    }
                }
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
                            .signed_duration_since(s.updated_at)
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
                // The actor saves the current session, loads the target,
                // restores model/reasoning/system prompt and swaps the
                // conversation (actor_cmds::resume); the `Conversation`
                // that follows carries the new messages + header.
                match link.resume(arg).await {
                    Ok(reply) => {
                        app.transcript.clear();
                        app.invalidate();
                        app.resume_pending = Some(super::app::ResumePending {
                            old_id: reply.old_id,
                            new_id: reply.new_id,
                            via: reply.via,
                            clamp_notice: reply.clamp_notice,
                        });
                    }
                    Err(e) => {
                        app.push_msg(ChatMessage::Error(format!("failed to load session: {}", e)));
                    }
                }
            }
        }
        "pin" | "keep-warm" => pin_command(cmd, arg, app, link).await,
        "clients" => clients(app, link),
        "takeover" => takeover(app, link).await,
        "keepwarm" => {
            // Prompt-cache keep-warm lives on the actor (task #444):
            // `EngineCommand{keepwarm}` → status / per-session override.
            match link.engine_command("keepwarm", arg.trim()).await {
                Ok(value) => {
                    let text = value["text"].as_str().unwrap_or("").to_string();
                    if value["kind"] == "error" {
                        app.push_msg(ChatMessage::Error(text));
                    } else {
                        app.push_msg(ChatMessage::System(text));
                    }
                }
                Err(e) => app.push_msg(ChatMessage::Error(format!("keepwarm failed: {e}"))),
            }
        }
        "saveas" => {
            // Naming lives on the actor's session (it owns the journal):
            // `EngineCommand{saveas}` → actor_cmds sets/clears the name and
            // force-saves; reply text is rendered as before.
            let trimmed = arg.trim();
            match link.engine_command("saveas", trimmed).await {
                Ok(value) => match value["kind"].as_str() {
                    Some("output") => {
                        app.session.name = value["name"].as_str().map(str::to_string);
                        app.push_msg(ChatMessage::System(
                            value["text"].as_str().unwrap_or("").to_string(),
                        ));
                    }
                    _ => app.push_msg(ChatMessage::Error(format!(
                        "saveas failed: {}",
                        value["text"].as_str().unwrap_or("unknown error")
                    ))),
                },
                Err(e) => app.push_msg(ChatMessage::Error(format!("saveas failed: {}", e))),
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
        // ── attachment commands (client-local, no wire traffic) ──
        "attach" => {
            if arg.trim().is_empty() {
                app.push_msg(ChatMessage::System("usage: /attach <path>".into()));
                return CommandAction::None;
            }
            // Refuse while streaming or compacting.
            if app.streaming {
                app.push_msg(ChatMessage::System("cannot attach while streaming".into()));
                return CommandAction::None;
            }
            if app.compacting {
                app.push_msg(ChatMessage::System("cannot attach while compacting".into()));
                return CommandAction::None;
            }
            let path_str = arg.trim().trim_matches(|c| c == '"' || c == '\'');
            let path = std::path::PathBuf::from(path_str);
            match agent_engine::attachments::load_attachment_sync(&path) {
                Ok(loaded) => {
                    let summaries_before = app.pending_attachments.len();
                    match app.pending_attachments.add(loaded) {
                        Ok(()) => {
                            let s = &app.pending_attachments.summaries()[summaries_before..];
                            for line in s {
                                app.push_msg(ChatMessage::System(format!("📎 staged: {line}")));
                            }
                        }
                        Err(e) => app.push_msg(ChatMessage::Error(e)),
                    }
                }
                Err(e) => app.push_msg(ChatMessage::Error(format!("{path_str}: {e}"))),
            }
        }
        "attachments" => {
            let summaries = app.pending_attachments.summaries();
            if summaries.is_empty() {
                app.push_msg(ChatMessage::System("no pending attachments".into()));
            } else {
                for (i, s) in summaries.iter().enumerate() {
                    app.push_msg(ChatMessage::System(format!("  [{i}] {s}")));
                }
            }
        }
        "detach" => {
            let trimmed = arg.trim();
            if trimmed.is_empty() || trimmed == "all" || trimmed == "clear" {
                let n = app.pending_attachments.len();
                app.pending_attachments.clear();
                if n > 0 {
                    app.push_msg(ChatMessage::System(format!("detached {n} file(s)")));
                } else {
                    app.push_msg(ChatMessage::System("no pending attachments".into()));
                }
            } else if let Ok(idx) = trimmed.parse::<usize>() {
                if idx < app.pending_attachments.len() {
                    app.pending_attachments.remove(idx);
                    app.push_msg(ChatMessage::System(format!("detached [{idx}]")));
                } else {
                    app.push_msg(ChatMessage::Error(format!("no attachment at index {idx}")));
                }
            } else {
                app.push_msg(ChatMessage::System("usage: /detach [n|all]".into()));
            }
        }
        "quit" | "exit" => {
            return CommandAction::Quit;
        }
        "watch" => {
            super::watch::command(app, arg, std::time::Instant::now());
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
            let receipt = build_stats_receipt(app, &**link.view());
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
        // Phase 4: reload the transcript from the engine's history (the
        // manual recovery path for a Digest client whose DisplayTail
        // roundtrip failed). In-process it is a harmless rebuild.
        "resync" => {
            if super::helpers::reload_display_tail(app, link).await {
                app.push_msg(ChatMessage::System(format!(
                    "transcript resynced from session {} ({} messages)",
                    app.session.id, app.transcript.message_count()
                )));
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
            // Nothing here handled it: ask the session. A daemon newer than
            // this client (an attached TUI keeps its binary across `daemon
            // reload`) may know it — `/workers` reached daemons before the
            // TUIs attached to them — and some builtins are engine-only
            // (`/budget`). The actor answers `unhandled` for anything it
            // doesn't know either.
            Resolution::Builtin | Resolution::Unknown => {
                return forward_unhandled_command(cmd, arg, app, link).await
            }
        },
    }
    CommandAction::None
}

/// A `/cmd` the client didn't handle, forwarded as an engine command;
/// renders the generic reply kinds (`output`/`notice`/`error`/`quit`), and
/// "unknown command" when the session doesn't know it either.
async fn forward_unhandled_command(
    cmd: &str,
    arg: &str,
    app: &mut App,
    link: &mut super::session_link::SessionLink,
) -> CommandAction {
    let unknown = || ChatMessage::Error(format!("unknown command: /{}", cmd));
    let value = match link.engine_command(cmd, arg).await {
        Ok(v) => v,
        Err(_) => {
            app.push_msg(unknown());
            return CommandAction::None;
        }
    };
    match forwarded_reply(&value) {
        ForwardedReply::Unknown => app.push_msg(unknown()),
        ForwardedReply::Quit => return CommandAction::Quit,
        ForwardedReply::Error(text) => app.push_msg(ChatMessage::Error(text)),
        ForwardedReply::Text(text) => app.push_msg(ChatMessage::System(text)),
        ForwardedReply::Nothing => {}
    }
    CommandAction::None
}

#[derive(Debug, PartialEq, Eq)]
enum ForwardedReply {
    Unknown,
    Quit,
    Error(String),
    Text(String),
    Nothing,
}

/// How a forwarded command's `QueryResult` renders (`engine_command_reply`
/// kinds). Pure, for tests.
fn forwarded_reply(value: &serde_json::Value) -> ForwardedReply {
    let text = value["text"].as_str().unwrap_or("").to_string();
    match value["kind"].as_str().unwrap_or("unhandled") {
        "unhandled" => ForwardedReply::Unknown,
        "quit" => ForwardedReply::Quit,
        "error" => ForwardedReply::Error(text),
        _ if text.is_empty() => ForwardedReply::Nothing,
        _ => ForwardedReply::Text(text),
    }
}

#[cfg(test)]
mod forwarded_command_tests {
    use super::{forwarded_reply, ForwardedReply};
    use serde_json::json;

    #[test]
    fn a_command_the_session_knows_renders_its_reply() {
        assert_eq!(
            forwarded_reply(&json!({"kind": "output", "text": "2 workers running"})),
            ForwardedReply::Text("2 workers running".into())
        );
        assert_eq!(
            forwarded_reply(&json!({"kind": "notice", "text": "done"})),
            ForwardedReply::Text("done".into())
        );
        assert_eq!(
            forwarded_reply(&json!({"kind": "error", "text": "no such worker"})),
            ForwardedReply::Error("no such worker".into())
        );
        assert_eq!(forwarded_reply(&json!({"kind": "quit"})), ForwardedReply::Quit);
        assert_eq!(forwarded_reply(&json!({"kind": "none"})), ForwardedReply::Nothing);
    }

    #[test]
    fn a_command_nobody_knows_is_unknown() {
        assert_eq!(forwarded_reply(&json!({"kind": "unhandled"})), ForwardedReply::Unknown);
        assert_eq!(forwarded_reply(&json!({})), ForwardedReply::Unknown);
    }

    /// End to end over the scripted session: a command only the engine
    /// handles (`/budget`) answers instead of "unknown command" (that is how
    /// a client older than its daemon reaches the daemon's new commands),
    /// and a command nobody knows is still reported as unknown.
    #[test]
    fn unhandled_commands_go_to_the_session() {
        use crate::tui::testing::TestHarness;
        let mut h = TestHarness::boot_with_size(120, 30);
        h.run_slash_command("budget", "");
        let frame = h.snapshot();
        assert!(frame.contains("Turn budget"), "{frame}");
        assert!(!frame.contains("unknown command"), "{frame}");

        let mut h = TestHarness::boot_with_size(120, 30);
        h.run_slash_command("zzz-no-such-command", "");
        let frame = h.snapshot();
        assert!(frame.contains("unknown command: /zzz-no-such-command"), "{frame}");
    }
}

/// Build the /stats session receipt string.
///
/// Returns a multi-line string ready to be pushed as `ChatMessage::System`.
/// Exported for unit testing.
pub(crate) fn build_stats_receipt(
    app: &App,
    runtime: &impl agent_engine::session::RuntimeRead,
) -> String {
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

    // Aggregate counters retain neither request tiers nor model history.
    // The actual-cost subtotal cannot reconstruct an uncached counterfactual.

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
    // Keep-warm pings are inside the totals above (and the hit rate below);
    // their share is shown so the numbers add up.
    let kw = &app.keepwarm_tokens;
    if kw.cache_read + kw.cache_creation + kw.input + kw.output > 0 {
        lines.push(format!(
            "    incl. keep-warm  read {} / write {} / input {} / output {}",
            fmt_tokens(kw.cache_read),
            fmt_tokens(kw.cache_creation),
            fmt_tokens(kw.input),
            fmt_tokens(kw.output)
        ));
    }

    lines.push(String::new());
    lines.push(format!(
        "  Cache  hit rate {:>9}",
        hit_rate_str
    ));
    lines.push(String::new());
    lines.push("  Cache savings unavailable: per-request model/tier history not retained".to_string());
    lines.push(format!("  Cost   ${:.4}", session_cost));
    if app.keepwarm_cost > 0.0 {
        lines.push(format!(
            "         incl. cache keep-warm ${:.4}  (/keepwarm status)",
            app.keepwarm_cost
        ));
    }
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
/// Whether `cmd` (already prefix-resolved against [`STREAMING_COMMANDS`]) is
/// one of the commands [`handle_streaming_command`] runs mid-stream. Some of
/// them (`/theme`, `/attachments`, `/detach`) finish with
/// `CommandAction::None`, which on its own can't be told apart from "not a
/// streaming command" — the caller asks this before refusing.
pub(super) fn is_streaming_command(cmd: &str) -> bool {
    STREAMING_COMMANDS.contains(&cmd)
}

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
        "clients" => CommandAction::Clients,
        // Mid-turn rules live with the rows (settings::mid_turn_refusal,
        // helpers::apply_setting's next-turn notice).
        "settings" => CommandAction::OpenSettings,
        "takeover" => CommandAction::TakeOver,
        "attachments" => {
            let summaries = app.pending_attachments.summaries();
            if summaries.is_empty() {
                app.push_msg(super::app::ChatMessage::System("no pending attachments".into()));
            } else {
                for (i, s) in summaries.iter().enumerate() {
                    app.push_msg(super::app::ChatMessage::System(format!("  [{i}] {s}")));
                }
            }
            CommandAction::None
        }
        "detach" => {
            let arg = full_input[1..]
                .split_once(' ')
                .map(|x| x.1)
                .unwrap_or("")
                .trim();
            let n = app.pending_attachments.len();
            if arg.is_empty() || arg == "all" || arg == "clear" {
                app.pending_attachments.clear();
                if n > 0 {
                    app.push_msg(super::app::ChatMessage::System(format!("detached {n} file(s)")));
                } else {
                    app.push_msg(super::app::ChatMessage::System("no pending attachments".into()));
                }
            } else if let Ok(idx) = arg.parse::<usize>() {
                if idx < n {
                    app.pending_attachments.remove(idx);
                    app.push_msg(super::app::ChatMessage::System(format!("detached [{idx}]")));
                } else {
                    app.push_msg(super::app::ChatMessage::Error(format!("no attachment at index {idx}")));
                }
            } else {
                app.push_msg(super::app::ChatMessage::System("usage: /detach [n|all]".into()));
            }
            CommandAction::None
        }
        "watch" => {
            let arg = full_input[1..]
                .split_once(' ')
                .map(|x| x.1)
                .unwrap_or("");
            super::watch::command(app, arg, std::time::Instant::now());
            CommandAction::None
        }
        _ => CommandAction::None, // unknown — handled by caller as steer/queue
    }
}

/// What a line typed mid-turn resolves to (the `StreamingInput` arm).
pub(super) enum StreamingSlash {
    /// A streaming command ran; the caller performs its `CommandAction`
    /// (`None` when it finished in place, e.g. `/theme`).
    Ran(CommandAction),
    /// A known command that can't run mid-turn: the refusal notice is
    /// already in the transcript; nothing is sent to the model.
    Refused,
    /// Plain text or unknown slash text: send it as a steer.
    Steer,
}

/// Resolve input typed while a turn streams. Known slash commands never
/// leak into the stream as steering text: they run (the streaming set) or
/// are refused with a notice.
pub(super) fn resolve_streaming_input(
    input: &str,
    app: &mut App,
    registry: &synaps_cli::skills::registry::CommandRegistry,
) -> StreamingSlash {
    let Some(rest) = input.strip_prefix('/') else {
        return StreamingSlash::Steer;
    };
    let raw_cmd = rest.split_whitespace().next().unwrap_or("");
    let streaming_cmds = to_owned_commands(STREAMING_COMMANDS);
    let cmd = resolve_prefix(raw_cmd, &streaming_cmds);
    match handle_streaming_command(&cmd, input, app) {
        CommandAction::None if !is_streaming_command(&cmd) => {
            let all_cmds = all_commands_with_skills(registry);
            let resolved_full = resolve_prefix(raw_cmd, &all_cmds);
            if all_cmds.iter().any(|c| c == &resolved_full) {
                app.push_msg(super::app::ChatMessage::System(format!(
                    "/{} can't run while streaming — press Esc to cancel first",
                    resolved_full
                )));
                StreamingSlash::Refused
            } else {
                StreamingSlash::Steer
            }
        }
        action => StreamingSlash::Ran(action),
    }
}

/// Notice shown when a model change forced a reasoning-level substitution.
#[allow(dead_code)]
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

/// Same line from the wire form (`SettingApplied.clamp`).
pub(crate) fn reasoning_clamp_notice_wire(
    clamp: &agent_engine::session::ReasoningClampWire,
    model: &str,
) -> String {
    format!(
        "thinking → {} (clamped from {}: not supported by {})",
        clamp.to, clamp.from, model
    )
}

#[cfg(test)]
mod tests {
    use super::command_output_event_to_chat_message;
    use super::{
        build_stats_receipt, edit_distance, execute_command_action,
        execute_interactive_plugin_command_events, fuzzy_match, handle_command, resolve_prefix,
        CommandAction, ExtensionsMemoryAction, ExtensionsTrustAction,
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
        let mut link = scripted_link(synaps_cli::Runtime::new().await.unwrap());
        let system_prompt_path = PathBuf::from("/tmp/synaps-test-system-prompt");
        let registry = Arc::new(registry);
        let keybinds = synaps_cli::skills::keybinds::KeybindRegistry::new();

        match handle_command(
            "policy:mode",
            "strict",
            &mut app,
            &mut link,
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

    /// `/close` is the session's to run (it ends it): the TUI forwards it
    /// as the `close` engine command, shows the reply, and the loop then
    /// meets the session's `Ended` (which `stream_handler` records for the
    /// exit line). A refusal is shown as an error and nothing ends.
    #[tokio::test]
    async fn close_is_sent_to_the_session_which_then_ends() {
        let mut app = crate::tui::app::App::new(synaps_cli::Session::new("test", "medium", None));
        let mut link = scripted_link(synaps_cli::Runtime::new().await.unwrap());
        let system_prompt_path = PathBuf::from("/tmp/synaps-test-system-prompt");
        let registry = Arc::new(CommandRegistry::new_with_plugins(&[], vec![], vec![]));
        let keybinds = synaps_cli::skills::keybinds::KeybindRegistry::new();

        let action =
            handle_command("close", "later", &mut app, &mut link, &system_prompt_path, &registry, &keybinds)
                .await;
        assert!(matches!(action, CommandAction::None));
        assert!(app.transcript.messages().iter().any(|m| matches!(
            &m.msg,
            crate::tui::app::ChatMessage::Error(s) if s == "usage: /close [force]"
        )));

        let action =
            handle_command("close", "", &mut app, &mut link, &system_prompt_path, &registry, &keybinds).await;
        assert!(matches!(action, CommandAction::None), "the session ends it, not the TUI");
        assert!(app.transcript.messages().iter().any(|m| matches!(
            &m.msg,
            crate::tui::app::ChatMessage::System(s)
                if s.starts_with("closing session ") && s.contains("--attach --continue")
        )));
        let env = link.next_event().await.expect("the session's end");
        assert!(matches!(
            env.event,
            agent_engine::session::SessionEventWire::Ended {
                reason: agent_engine::session::EndReason::ClientQuit
            }
        ));
        assert!(synaps_cli::skills::BUILTIN_COMMANDS.contains(&"close"));
    }

    /// `/pin on|off` is the session's pin: the TUI sends `KeepWarm` and the
    /// session's notice follows; `/pin` asks the session for its state;
    /// `/keep-warm` is the same command, never the prompt-cache `/keepwarm`.
    #[tokio::test]
    async fn pin_sends_keep_warm_to_a_daemon_session_and_reports_its_state() {
        let transport = crate::tui::testing::scripted::ScriptedTransport::new(synaps_cli::Runtime::new().await.unwrap())
            .with_daemon_pid(4242);
        let log = Arc::clone(&transport.log);
        let mut link = crate::tui::session_link::SessionLink::new(Box::new(transport));
        let mut app = crate::tui::app::App::new(synaps_cli::Session::new("test", "medium", None));
        let system_prompt_path = PathBuf::from("/tmp/synaps-test-system-prompt");
        let registry = Arc::new(CommandRegistry::new_with_plugins(&[], vec![], vec![]));
        let keybinds = synaps_cli::skills::keybinds::KeybindRegistry::new();
        let has_system = |app: &crate::tui::app::App, want: &str| {
            app.transcript.messages().iter().any(|m| matches!(&m.msg, ChatMessage::System(s) if s.starts_with(want)))
        };

        handle_command("pin", "on", &mut app, &mut link, &system_prompt_path, &registry, &keybinds).await;
        let sent = log.lock().unwrap().sent.clone();
        assert!(sent.iter().any(|c| c == "KeepWarm { on: true }"), "{sent:?}");
        let env = link.next_event().await.expect("the session's notice");
        assert!(
            matches!(env.event, agent_engine::session::SessionEventWire::SystemNotice(ref n) if n.starts_with("pinned:")),
            "{:?}",
            env.event
        );
        handle_command("pin", "", &mut app, &mut link, &system_prompt_path, &registry, &keybinds).await;
        assert!(has_system(&app, "pinned: this session stays loaded"));

        handle_command("keep-warm", "off", &mut app, &mut link, &system_prompt_path, &registry, &keybinds).await;
        let sent = log.lock().unwrap().sent.clone();
        assert!(sent.iter().any(|c| c == "KeepWarm { on: false }"), "{sent:?}");
        let env = link.next_event().await.expect("the session's notice");
        assert!(matches!(env.event, agent_engine::session::SessionEventWire::SystemNotice(ref n) if n.starts_with("unpinned")));
        handle_command("pin", "status", &mut app, &mut link, &system_prompt_path, &registry, &keybinds).await;
        assert!(has_system(&app, "not pinned"));

        handle_command("pin", "maybe", &mut app, &mut link, &system_prompt_path, &registry, &keybinds).await;
        assert!(app.transcript.messages().iter().any(|m| matches!(
            &m.msg,
            ChatMessage::Error(s) if s == "usage: /pin [on|off|status]"
        )));
        for name in ["pin", "keep-warm", "keepwarm"] {
            assert!(synaps_cli::skills::BUILTIN_COMMANDS.contains(&name), "{name}");
        }
        let all = super::to_owned_commands(synaps_cli::skills::BUILTIN_COMMANDS);
        assert_eq!(resolve_prefix("keep-warm", &all), "keep-warm", "never the prompt-cache /keepwarm");
        assert_eq!(resolve_prefix("pin", &all), "pin", "not /ping");
    }

    /// `/clients` lists the roster by name, marks the owner and this
    /// client, tells drivers from observers, and sends nothing. Mid-stream
    /// it is a `CommandAction`; in-process it says there's one client.
    #[tokio::test]
    async fn clients_lists_the_roster_by_name_and_sends_nothing() {
        use agent_engine::session::{AttachMode, ClientId, ClientInfo, ClientKind};
        let info = |id: u64, name: &str, kind: ClientKind, mode: AttachMode| ClientInfo {
            id: ClientId(id),
            name: name.to_string(),
            kind,
            mode,
            attached_at_ms: 0,
        };
        let transport = crate::tui::testing::scripted::ScriptedTransport::new(synaps_cli::Runtime::new().await.unwrap())
            .with_daemon_pid(4242)
            .with_roster(vec![
                info(1, "terminal", ClientKind::Tui, AttachMode::Mirror),
                info(2, "dashboard", ClientKind::Attach, AttachMode::Observe),
            ]);
        let log = Arc::clone(&transport.log);
        let mut link = crate::tui::session_link::SessionLink::new(Box::new(transport));
        let mut app = crate::tui::app::App::new(synaps_cli::Session::new("test", "medium", None));
        let system_prompt_path = PathBuf::from("/tmp/synaps-test-system-prompt");
        let registry = Arc::new(CommandRegistry::new_with_plugins(&[], vec![], vec![]));
        let keybinds = synaps_cli::skills::keybinds::KeybindRegistry::new();

        let action = handle_command("clients", "", &mut app, &mut link, &system_prompt_path, &registry, &keybinds).await;
        assert!(matches!(action, CommandAction::None));
        let listing = app
            .transcript
            .messages()
            .iter()
            .find_map(|m| match &m.msg {
                ChatMessage::System(s) if s.starts_with("2 clients attached") => Some(s.clone()),
                _ => None,
            })
            .expect("the listing");
        let rows: Vec<&str> = listing.lines().skip(1).collect();
        assert_eq!(rows.len(), 2, "{listing}");
        assert!(rows[0].contains("terminal (#1)") && rows[0].contains("drive"), "{listing}");
        assert!(rows[0].contains("owns input") && rows[0].contains("you"), "{listing}");
        assert!(rows[1].contains("dashboard (#2)") && rows[1].contains("observe"), "{listing}");
        assert!(!rows[1].contains("you") && !rows[1].contains("owns input"), "{listing}");
        assert!(log.lock().unwrap().sent.is_empty(), "nothing reaches the session");

        // Mid-stream: the dispatcher runs it with the link.
        assert!(super::STREAMING_COMMANDS.contains(&"clients"));
        assert!(matches!(super::handle_streaming_command("clients", "/clients", &mut app), CommandAction::Clients));
        assert!(synaps_cli::skills::BUILTIN_COMMANDS.contains(&"clients"));

        // In-process: one client, no roster to show.
        let transport = crate::tui::testing::scripted::ScriptedTransport::new(synaps_cli::Runtime::new().await.unwrap());
        let link = crate::tui::session_link::SessionLink::new(Box::new(transport));
        super::clients(&mut app, &link);
        assert!(app.transcript.messages().iter().any(|m| matches!(&m.msg, ChatMessage::System(s) if s == super::CLIENTS_IN_PROCESS)));
    }

    /// In-process the session ends with the TUI, so there is nothing to pin.
    #[tokio::test]
    async fn pin_in_process_says_it_only_applies_to_a_daemon_session() {
        let transport = crate::tui::testing::scripted::ScriptedTransport::new(synaps_cli::Runtime::new().await.unwrap());
        let log = Arc::clone(&transport.log);
        let mut link = crate::tui::session_link::SessionLink::new(Box::new(transport));
        let mut app = crate::tui::app::App::new(synaps_cli::Session::new("test", "medium", None));
        let system_prompt_path = PathBuf::from("/tmp/synaps-test-system-prompt");
        let registry = Arc::new(CommandRegistry::new_with_plugins(&[], vec![], vec![]));
        let keybinds = synaps_cli::skills::keybinds::KeybindRegistry::new();

        for arg in ["", "on"] {
            let action =
                handle_command("pin", arg, &mut app, &mut link, &system_prompt_path, &registry, &keybinds).await;
            assert!(matches!(action, CommandAction::None));
        }
        let said = app
            .transcript
            .messages()
            .iter()
            .filter(|m| matches!(&m.msg, ChatMessage::System(s) if s == super::PIN_IN_PROCESS))
            .count();
        assert_eq!(said, 2);
        assert!(log.lock().unwrap().sent.is_empty(), "nothing reaches the session");
    }

    /// `/takeover` from a read-only client sends `TakeInput`; the session's
    /// `InputOwnerChanged` makes this client the owner. Nothing is sent
    /// in-process, from an observer or by the owner, and each says why.
    #[tokio::test]
    async fn takeover_takes_input_from_another_client_and_says_why_when_it_cannot() {
        use agent_engine::session::{AttachMode, ClientId, SessionEventWire};
        let system_prompt_path = PathBuf::from("/tmp/synaps-test-system-prompt");
        let registry = Arc::new(CommandRegistry::new_with_plugins(&[], vec![], vec![]));
        let keybinds = synaps_cli::skills::keybinds::KeybindRegistry::new();
        let scripted = || async {
            crate::tui::testing::scripted::ScriptedTransport::new(synaps_cli::Runtime::new().await.unwrap())
        };

        // Read-only mirror, client #7 owns input: takes it.
        let transport = scripted().await.with_daemon_pid(4242).with_ownership(AttachMode::Mirror, Some(ClientId(7)));
        let log = Arc::clone(&transport.log);
        let mut link = crate::tui::session_link::SessionLink::new(Box::new(transport));
        let mut app = crate::tui::app::App::new(synaps_cli::Session::new("test", "medium", None));
        let action =
            handle_command("takeover", "", &mut app, &mut link, &system_prompt_path, &registry, &keybinds).await;
        assert!(matches!(action, CommandAction::None));
        assert_eq!(log.lock().unwrap().sent, vec!["TakeInput".to_string()]);
        let env = link.next_event().await.expect("the owner change");
        assert!(
            matches!(
                env.event,
                SessionEventWire::InputOwnerChanged { from: Some(ClientId(7)), to: Some(ClientId(1)), .. }
            ),
            "{:?}",
            env.event
        );
        assert_eq!(link.transport().input_owner(), Some(ClientId(1)));

        // Now the owner: nothing to send.
        handle_command("takeover", "", &mut app, &mut link, &system_prompt_path, &registry, &keybinds).await;
        assert_eq!(log.lock().unwrap().sent.len(), 1, "the owner sends nothing");

        let says = |app: &crate::tui::app::App, want: &str| {
            app.transcript.messages().iter().any(|m| matches!(&m.msg, ChatMessage::System(s) if s == want))
        };
        assert!(says(&app, super::TAKEOVER_ALREADY));

        // In-process and observing: a note, nothing sent.
        for (transport, note) in [
            (scripted().await.with_ownership(AttachMode::Mirror, Some(ClientId(7))), super::TAKEOVER_IN_PROCESS),
            (
                scripted().await.with_daemon_pid(4242).with_ownership(AttachMode::Observe, Some(ClientId(7))),
                super::TAKEOVER_OBSERVING,
            ),
        ] {
            let log = Arc::clone(&transport.log);
            let mut link = crate::tui::session_link::SessionLink::new(Box::new(transport));
            let mut app = crate::tui::app::App::new(synaps_cli::Session::new("test", "medium", None));
            handle_command("takeover", "", &mut app, &mut link, &system_prompt_path, &registry, &keybinds).await;
            assert!(says(&app, note), "{note}");
            assert!(log.lock().unwrap().sent.is_empty(), "{note}: nothing reaches the session");
        }
        assert!(synaps_cli::skills::BUILTIN_COMMANDS.contains(&"takeover"));
    }

    /// With `client_input = shared` every client already types: `/takeover`
    /// only makes this one the active client, and says so (no toast comes).
    #[tokio::test]
    async fn takeover_with_shared_input_says_it_only_moves_the_active_client() {
        use agent_engine::session::{AttachMode, ClientId, ClientInput};
        let system_prompt_path = PathBuf::from("/tmp/synaps-test-system-prompt");
        let registry = Arc::new(CommandRegistry::new_with_plugins(&[], vec![], vec![]));
        let keybinds = synaps_cli::skills::keybinds::KeybindRegistry::new();
        let transport = crate::tui::testing::scripted::ScriptedTransport::new(synaps_cli::Runtime::new().await.unwrap())
            .with_daemon_pid(4242)
            .with_ownership(AttachMode::Mirror, Some(ClientId(7)))
            .with_client_input(ClientInput::Shared);
        let log = Arc::clone(&transport.log);
        let mut link = crate::tui::session_link::SessionLink::new(Box::new(transport));
        assert!(crate::tui::session_link::may_drive(link.transport()), "shared: a non-active mirror drives");
        let mut app = crate::tui::app::App::new(synaps_cli::Session::new("test", "medium", None));
        let says = |app: &crate::tui::app::App, want: &str| {
            app.transcript.messages().iter().any(|m| matches!(&m.msg, ChatMessage::System(s) if s == want))
        };
        handle_command("takeover", "", &mut app, &mut link, &system_prompt_path, &registry, &keybinds).await;
        assert_eq!(log.lock().unwrap().sent, vec!["TakeInput".to_string()]);
        assert!(says(&app, super::TAKEOVER_SHARED));
        link.next_event().await.expect("the active marker moves");
        handle_command("takeover", "", &mut app, &mut link, &system_prompt_path, &registry, &keybinds).await;
        assert_eq!(log.lock().unwrap().sent.len(), 1, "already active: nothing sent");
        assert!(says(&app, super::TAKEOVER_SHARED_ALREADY));
    }

    /// Mid-stream `/takeover` runs (it never steers): the dispatcher gets
    /// `TakeOver` and sends it over the link.
    #[tokio::test]
    async fn takeover_runs_mid_stream() {
        let registry = CommandRegistry::new_with_plugins(&[], vec![], vec![]);
        let mut app = crate::tui::app::App::new(synaps_cli::Session::new("test", "medium", None));
        app.streaming = true;
        for typed in ["/takeover", "/tak"] {
            let r = super::resolve_streaming_input(typed, &mut app, &registry);
            assert!(matches!(r, super::StreamingSlash::Ran(CommandAction::TakeOver)), "{typed}");
        }
    }

    /// A `SessionLink` over the scripted in-memory actor (A5).
    fn scripted_link(runtime: synaps_cli::Runtime) -> crate::tui::session_link::SessionLink {
        crate::tui::session_link::SessionLink::new(Box::new(
            crate::tui::testing::scripted::ScriptedTransport::new(runtime),
        ))
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
        let mut link = scripted_link(runtime);
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
        // The actor resolves the command by plugin:name on the host's
        // registry; the scripted actor resolves it here.
        crate::tui::testing::scripted::register_plugin_command(Arc::clone(&command));

        execute_command_action(
            CommandAction::PluginCommand {
                command,
                arg: "hello".to_string(),
            },
            &mut app,
            &mut link,
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
        let mut link = scripted_link(synaps_cli::Runtime::new().await.unwrap());
        let system_prompt_path = PathBuf::from("/tmp/synaps-test-system-prompt");
        let registry = Arc::new(CommandRegistry::new_with_plugins(&[], vec![], vec![]));
        let keybinds = synaps_cli::skills::keybinds::KeybindRegistry::new();
        handle_command(
            "extensions",
            arg,
            &mut app,
            &mut link,
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
        let mut link = scripted_link(synaps_cli::Runtime::new().await.unwrap());
        let keybinds = synaps_cli::skills::keybinds::KeybindRegistry::new();
        let action = handle_command(
            "capture",
            "toggle",
            &mut app,
            &mut link,
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
        let mut link = scripted_link(synaps_cli::Runtime::new().await.unwrap());
        let keybinds = synaps_cli::skills::keybinds::KeybindRegistry::new();
        let action = handle_command(
            "capture",
            "",
            &mut app,
            &mut link,
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
        let mut link = scripted_link(synaps_cli::Runtime::new().await.unwrap());
        let keybinds = synaps_cli::skills::keybinds::KeybindRegistry::new();
        let action = handle_command(
            "capture",
            "status",
            &mut app,
            &mut link,
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
        let mut link = scripted_link(synaps_cli::Runtime::new().await.unwrap());
        let keybinds = synaps_cli::skills::keybinds::KeybindRegistry::new();
        let action = handle_command(
            "capture",
            "toggle",
            &mut app,
            &mut link,
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
        let mut link = scripted_link(synaps_cli::Runtime::new().await.unwrap());
        let keybinds = synaps_cli::skills::keybinds::KeybindRegistry::new();
        let action = handle_command(
            "capture",
            "bogus",
            &mut app,
            &mut link,
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
        let mut link = scripted_link(synaps_cli::Runtime::new().await.unwrap());
        let registry = Arc::new(CommandRegistry::new_with_plugins(&[], vec![], plugins));
        let keybinds = synaps_cli::skills::keybinds::KeybindRegistry::new();
        let action = handle_command(
            "sidecar",
            arg,
            &mut app,
            &mut link,
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

    #[tokio::test]
    #[serial_test::serial]
    async fn stats_does_not_reprice_aggregate_haiku_cache_reads() {
        let session = synaps_cli::Session::new("claude-haiku-5-5", "medium", None);
        let mut app = crate::tui::app::App::new(session);
        // Two short requests, or mixed-model history: totals cannot tell.
        app.total_cache_read_tokens = 120_000;
        app.session_cost = 0.0012;
        let runtime = synaps_cli::Runtime::new_headless();
        let receipt = build_stats_receipt(&app, &runtime);
        assert!(receipt.contains("Cache savings unavailable"));
        assert!(!receipt.contains("saved: $"));
        assert!(receipt.contains("100%"));
        assert!(receipt.contains("Cost   $0.0012"));
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

        let runtime = synaps_cli::Runtime::new_headless();
        let receipt = build_stats_receipt(&app, &runtime);

        assert!(receipt.contains("Session Stats"), "missing header");
        assert!(receipt.contains("Tokens"), "missing Tokens section");
        assert!(receipt.contains("Cache"), "missing Cache section");
        assert!(receipt.contains("Cost"), "missing Cost section");
        assert!(receipt.contains("Cache savings unavailable"));
        assert!(!receipt.contains("saved: $"));
        assert!(receipt.contains("5m:"), "missing 5m split");
        assert!(receipt.contains("1h:"), "missing 1h split");
        assert!(!receipt.contains("keep-warm"), "no keep-warm line without spend");
    }

    /// Keep-warm spend is labelled inside the session cost (task #444).
    #[tokio::test]
    #[serial_test::serial]
    async fn stats_receipt_labels_keepwarm_share() {
        let session = synaps_cli::Session::new("claude-sonnet-4-5", "medium", None);
        let mut app = crate::tui::app::App::new(session);
        app.session_cost = 1.25;
        app.keepwarm_cost = 0.18;
        app.total_cache_read_tokens = 400_000;
        app.keepwarm_tokens = agent_engine::session::ConversationTokens {
            input: 80,
            output: 2,
            cache_read: 300_000,
            cache_creation: 0,
        };
        let runtime = synaps_cli::Runtime::new_headless();
        let receipt = build_stats_receipt(&app, &runtime);
        assert!(receipt.contains("Cost   $1.2500"), "{receipt}");
        assert!(receipt.contains("incl. cache keep-warm $0.1800"), "{receipt}");
        assert!(receipt.contains("400.0K"), "the total includes pings: {receipt}");
        assert!(
            receipt.contains("incl. keep-warm  read 300.0K / write 0 / input 80 / output 2"),
            "{receipt}"
        );
    }
}

#[cfg(test)]
mod streaming_command_tests {
    use super::{is_streaming_command, resolve_prefix, to_owned_commands, STREAMING_COMMANDS};

    /// Regression: `/theme` mid-stream applied the theme and then also said
    /// "/theme can't run while streaming" — its `CommandAction::None` was
    /// read as "not a streaming command". The caller now checks this first.
    #[test]
    fn theme_and_friends_are_streaming_commands() {
        let cmds = to_owned_commands(STREAMING_COMMANDS);
        for typed in [
            "theme", "th", "attachments", "detach", "quit", "gamba", "watch", "settings",
            "takeover",
        ] {
            let cmd = resolve_prefix(typed, &cmds);
            assert!(is_streaming_command(&cmd), "/{typed} runs mid-stream");
        }
        for typed in ["model", "compact", "effort"] {
            let cmd = resolve_prefix(typed, &cmds);
            assert!(
                !is_streaming_command(&cmd),
                "/{typed} is refused mid-stream"
            );
        }
    }
}
