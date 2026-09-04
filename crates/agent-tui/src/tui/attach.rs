//! `synaps --attach [ID] [--observe|--takeover] [--keep-warm]` — the TUI
//! over `SocketTransport` (PLAN-phase3 A4). Client diet: no `EngineHost`,
//! no `Runtime`, no `ExtensionManager`, no MCP, no skills index — config,
//! builtin commands, keybinds, the render thread and the event stream only.
//! Plugin commands / interactive plugin commands are not available over the
//! socket yet (`Query{Commands}` is phase 4).

use std::path::PathBuf;
use std::sync::Arc;

use agent_engine::session::socket_transport::SocketTransport;
use agent_engine::session::wire::{Attach, Hello};
use agent_engine::session::{
    AttachMode, ClientKind, ClientTransport, CompactionPolicyWire, SessionCommand, SessionConfig,
    SessionId, TransportError,
};
use synaps_cli::skills::registry::CommandRegistry;
use synaps_cli::skills::BUILTIN_COMMANDS;
use synaps_cli::Result;

use super::app::ChatMessage;
use super::run_setup::{app_from_snapshot, finish_setup, TransportMode};

/// What to attach to.
pub struct AttachOpts {
    pub profile: Option<String>,
    /// Session id (prefix ok); `None` = the only live session, else create.
    pub id: Option<String>,
    /// Create by continuing a saved session (name or id).
    pub continue_session: Option<String>,
    pub system: Option<String>,
    pub prompt_manifest: Option<PathBuf>,
    pub mode: AttachMode,
    pub keep_warm: bool,
}

/// `SYNAPS_ATTACH_MODE=mirror|observe|takeover` default, overridden by flags.
pub fn attach_mode(observe: bool, takeover: bool) -> AttachMode {
    if takeover {
        return AttachMode::Takeover;
    }
    if observe {
        return AttachMode::Observe;
    }
    match std::env::var("SYNAPS_ATTACH_MODE").ok().as_deref() {
        Some("observe") => AttachMode::Observe,
        Some("takeover") => AttachMode::Takeover,
        _ => AttachMode::Mirror,
    }
}

fn cfg_err(e: impl std::fmt::Display) -> synaps_cli::RuntimeError {
    synaps_cli::RuntimeError::Config(e.to_string())
}

/// What `--attach` prints when there is no daemon to attach to: says so,
/// and how to start one (profile-aware).
pub fn daemon_not_running_message(profile: Option<&str>, detail: Option<&str>) -> String {
    let start = match profile {
        Some(p) => format!("synaps --profile {p} daemon --detach"),
        None => "synaps daemon --detach".to_string(),
    };
    let why = detail.map(|d| format!(" ({d})")).unwrap_or_default();
    format!(
        "no daemon is running{why} — nothing to attach to.\n\
         start one with:  {start}\n\
         or run in the foreground:  {}\n\
         then re-run `synaps --attach` (SYNAPS_DAEMON=1).",
        start.replace("--detach", "--foreground")
    )
}

pub async fn run_attached(opts: AttachOpts) -> Result<()> {
    let paths = agent_engine::daemon::registry::daemon_paths(opts.profile.as_deref());
    if !agent_engine::daemon::registry::is_alive(&paths) {
        return Err(cfg_err(daemon_not_running_message(
            opts.profile.as_deref(),
            None,
        )));
    }
    let conn = match SocketTransport::connect(&paths.sock, Hello::new(ClientKind::Tui)).await {
        Ok(c) => c,
        Err(TransportError::Version { client, daemon }) => {
            return Err(cfg_err(format!(
                "protocol version mismatch (client {client}, daemon {daemon}); restart or reload the daemon with this binary"
            )))
        }
        Err(TransportError::Io(e)) => {
            // Lock held but the socket is gone/refusing: the daemon is dead
            // or still booting — same advice, with the cause.
            return Err(cfg_err(daemon_not_running_message(
                opts.profile.as_deref(),
                Some(&format!("{}: {e}", paths.sock.display())),
            )));
        }
        Err(e) => return Err(cfg_err(format!("connect: {e}"))),
    };

    let cwd = std::env::current_dir().ok();
    let create = |continue_session: Option<Option<String>>| Attach::Create {
        config: SessionConfig {
            continue_session,
            system: opts.system.clone(),
            prompt_manifest: opts.prompt_manifest.clone(),
            cwd: cwd.clone(),
            compaction_policy: CompactionPolicyWire::LinkedSuccessor,
            await_extensions: true,
            keep_warm: opts.keep_warm,
            ..Default::default()
        },
        mode: opts.mode,
    };
    let attach = if let Some(c) = &opts.continue_session {
        create(Some(Some(c.clone())))
    } else if let Some(id) = &opts.id {
        let sid = conn
            .welcome
            .sessions
            .iter()
            .find(|m| m.id.as_str() == id || m.id.as_str().starts_with(id.as_str()))
            .map(|m| m.id.clone())
            .unwrap_or_else(|| SessionId::from(id.as_str()));
        Attach::Existing {
            session_id: sid,
            mode: opts.mode,
        }
    } else if conn.welcome.sessions.len() == 1 {
        Attach::Existing {
            session_id: conn.welcome.sessions[0].id.clone(),
            mode: opts.mode,
        }
    } else if conn.welcome.sessions.is_empty() {
        create(None)
    } else {
        let mut lines = vec!["several sessions; pick one with --attach <ID>:".to_string()];
        for m in &conn.welcome.sessions {
            lines.push(format!("  {}  model={}  clients={}", m.id, m.model, m.clients));
        }
        return Err(cfg_err(lines.join("\n")));
    };

    let (transport, snapshot) = SocketTransport::attach(conn, attach)
        .await
        .map_err(|e| cfg_err(format!("attach: {e}")))?;

    // ── Client diet: config + builtin commands + keybinds, nothing else ──
    let config = synaps_cli::load_config();
    let registry = Arc::new(CommandRegistry::new(BUILTIN_COMMANDS, Vec::new()));
    let mut keybinds = synaps_cli::skills::keybinds::KeybindRegistry::new();
    if !config.keybinds.is_empty() {
        keybinds.register_user(&config.keybinds);
    }
    let keybind_registry = Arc::new(std::sync::RwLock::new(keybinds));
    let system_prompt_path = synaps_cli::config::resolve_read_path("system.md");
    let http = agent_engine::runtime::build_host_http_client()?;

    let mut app = app_from_snapshot(&snapshot);
    for w in &config.warnings {
        app.push_msg(ChatMessage::System(format!("⚠ config: {}", w)));
    }
    app.push_msg(ChatMessage::System(format!(
        "attached to {} as client #{} ({:?}){}",
        snapshot.meta.id,
        transport.client_id().0,
        transport.mode(),
        match snapshot.input_owner {
            Some(owner) if owner != transport.client_id() =>
                format!(" — input is owned by client #{}", owner.0),
            _ => String::new(),
        }
    )));
    // Mid-turn attach: rebuild the partial turn from the replay ring, then
    // the actor's pending prompts.
    let replay = snapshot.replay.clone();
    let pending = snapshot.pending_prompts.clone();
    app.streaming = snapshot.streaming;

    let mut ctx = finish_setup(
        app,
        Box::new(transport),
        http,
        TransportMode::Socket,
        config,
        registry,
        keybind_registry,
        system_prompt_path,
        None,
    )
    .await?;
    // No extension host in this process: the loader arm never runs.
    ctx.app.extension_loader_running = false;
    for env in replay {
        // Presentation only; a render handle is not needed for correctness.
        let _ = super::stream_handler::handle_session_event_arm(
            env,
            &mut ctx.app,
            &mut ctx.link,
            &ctx.registry,
            &ctx.render_handle,
            &mut ctx.prompt_bridge,
            None,
        )
        .await;
    }
    for pr in pending {
        ctx.prompt_bridge.on_prompt(pr);
    }
    if opts.keep_warm {
        let _ = ctx.link.send(SessionCommand::KeepWarm { on: true }).await;
    }
    super::run_loop(ctx).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_running_message_says_so_and_how_to_start() {
        let m = daemon_not_running_message(None, None);
        assert!(m.starts_with("no daemon is running"), "{m}");
        assert!(m.contains("synaps daemon --detach"), "{m}");
        assert!(m.contains("synaps daemon --foreground"), "{m}");
        let m = daemon_not_running_message(Some("work"), Some("connection refused"));
        assert!(m.contains("(connection refused)"), "{m}");
        assert!(m.contains("synaps --profile work daemon --detach"), "{m}");
    }

    #[test]
    fn attach_mode_flags_override_env() {
        assert_eq!(attach_mode(false, true), AttachMode::Takeover);
        assert_eq!(attach_mode(true, false), AttachMode::Observe);
    }
}
