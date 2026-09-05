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
    AttachMode, ClientKind, ClientTransport, CompactionPolicyWire, HistoryMode, SessionCommand,
    SessionConfig, SessionId, TransportError,
};
use synaps_cli::skills::registry::CommandRegistry;
use synaps_cli::skills::BUILTIN_COMMANDS;
use synaps_cli::Result;

use agent_core::core::memstat::ladder;

use super::app::ChatMessage;
use super::run_setup::{app_from_snapshot, finish_setup, LazyHttp, TransportMode};

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
    /// `--new`/`--create`: always create a fresh session even if one is
    /// live. Rejected together with an explicit `id`.
    pub new_session: bool,
}

/// The slice of `Welcome.sessions` the attach decision needs.
pub struct LiveSession {
    pub id: SessionId,
    pub model: String,
    pub clients: usize,
}

/// What to send: `Existing` for an explicit or sole live session, `Create`
/// for `--new`, `--continue`, or no live sessions. Errors are user-facing.
/// `notice` is a one-line stderr message when a live session was picked
/// implicitly (no id given).
pub fn choose_attach(
    opts: &AttachOpts,
    sessions: &[LiveSession],
    cwd: Option<PathBuf>,
) -> std::result::Result<(Attach, Option<String>), String> {
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
    if opts.new_session && opts.id.is_some() {
        return Err("cannot combine --attach <ID> with --new: pick one".to_string());
    }
    if let Some(c) = &opts.continue_session {
        return Ok((create(Some(Some(c.clone()))), None));
    }
    if opts.new_session {
        return Ok((create(None), None));
    }
    if let Some(id) = &opts.id {
        let sid = sessions
            .iter()
            .find(|m| m.id.as_str() == id || m.id.as_str().starts_with(id.as_str()))
            .map(|m| m.id.clone())
            .unwrap_or_else(|| SessionId::from(id.as_str()));
        return Ok((
            Attach::Existing {
                session_id: sid,
                mode: opts.mode,
            },
            None,
        ));
    }
    match sessions {
        [] => Ok((create(None), None)),
        [only] => Ok((
            Attach::Existing {
                session_id: only.id.clone(),
                mode: opts.mode,
            },
            Some(format!(
                "attaching to live session {} (--new for a fresh one; `synaps daemon sessions` lists all)",
                only.id
            )),
        )),
        many => {
            let mut lines =
                vec!["several sessions; pick one with --attach <ID> (or --new for a fresh one):".to_string()];
            for m in many {
                lines.push(format!("  {}  model={}  clients={}", m.id, m.model, m.clients));
            }
            Err(lines.join("\n"))
        }
    }
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
         then re-run `synaps --attach` (or unset SYNAPS_DAEMON_AUTOSPAWN=0 to auto-start).",
        start.replace("--detach", "--foreground")
    )
}

pub async fn run_attached(opts: AttachOpts) -> Result<()> {
    ladder("attach:enter", &"");
    let paths = agent_engine::daemon::registry::daemon_paths(opts.profile.as_deref());
    if !agent_engine::daemon::registry::is_alive(&paths) {
        return Err(cfg_err(daemon_not_running_message(
            opts.profile.as_deref(),
            None,
        )));
    }
    let hello = Hello::new(ClientKind::Tui)
        .with_history(HistoryMode::from_env_or(HistoryMode::attach_client_default()));
    let conn = match SocketTransport::connect(&paths.sock, hello).await {
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
    ladder(
        "connect",
        &format_args!("sessions={}", conn.welcome.sessions.len()),
    );

    let cwd = std::env::current_dir().ok();
    let live: Vec<LiveSession> = conn
        .welcome
        .sessions
        .iter()
        .map(|m| LiveSession {
            id: m.id.clone(),
            model: m.model.clone(),
            clients: m.clients,
        })
        .collect();
    let (attach, notice) = choose_attach(&opts, &live, cwd).map_err(cfg_err)?;
    if let Some(n) = notice {
        // Before the TUI takes the terminal, so it survives on the scrollback.
        eprintln!("{n}");
    }

    let (transport, snapshot) = SocketTransport::attach(conn, attach)
        .await
        .map_err(|e| cfg_err(format!("attach: {e}")))?;
    // `frame_bytes` is filled once the transport records its last frame
    // length (B4 `last_frame_bytes()`); until then the wire size is not known here.
    ladder(
        "attached",
        &format_args!(
            "frame_bytes=n/a messages_len={} api_messages={} replay={} tail_items={}",
            snapshot.conversation.messages_len,
            snapshot.conversation.api_messages.len(),
            snapshot.replay.len(),
            agent_engine::session::DEFAULT_TAIL_ITEMS
        ),
    );
    // The decode transient of `Attached` is garbage now — give it back (§4.4).
    super::client_diet::purge_arenas("purge:attached");

    // ── Client diet: config + builtin commands + keybinds, nothing else ──
    let config = synaps_cli::load_config();
    let registry = Arc::new(CommandRegistry::new(BUILTIN_COMMANDS, Vec::new()));
    let mut keybinds = synaps_cli::skills::keybinds::KeybindRegistry::new();
    if !config.keybinds.is_empty() {
        keybinds.register_user(&config.keybinds);
    }
    let keybind_registry = Arc::new(std::sync::RwLock::new(keybinds));
    let system_prompt_path = synaps_cli::config::resolve_read_path("system.md");
    ladder("config", &"");
    let http = LazyHttp::new();
    if std::env::var("SYNAPS_CLIENT_HTTP").is_ok_and(|v| v == "eager") {
        http.get()?;
    }

    let mut app = app_from_snapshot(&snapshot);
    let (msgs, bytes) = super::app::scrollback_from_env(&TransportMode::Socket);
    app.transcript.set_scrollback(msgs, bytes);
    ladder("app", &"");
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
    ladder("replay", &"");
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

    fn opts(id: Option<&str>, new_session: bool) -> AttachOpts {
        AttachOpts {
            profile: None,
            id: id.map(str::to_string),
            continue_session: None,
            system: None,
            prompt_manifest: None,
            mode: AttachMode::Mirror,
            keep_warm: false,
            new_session,
        }
    }

    fn live(id: &str) -> LiveSession {
        LiveSession {
            id: SessionId::from(id),
            model: "m".into(),
            clients: 1,
        }
    }

    #[test]
    fn new_creates_even_with_live_sessions() {
        let (a, notice) = choose_attach(&opts(None, true), &[live("abc")], None).unwrap();
        assert!(matches!(a, Attach::Create { .. }), "{a:?}");
        assert!(notice.is_none());
        let (a, _) = choose_attach(&opts(None, true), &[live("a"), live("b")], None).unwrap();
        assert!(matches!(a, Attach::Create { .. }), "{a:?}");
    }

    #[test]
    fn new_with_explicit_id_is_rejected() {
        let e = choose_attach(&opts(Some("abc"), true), &[live("abc")], None).unwrap_err();
        assert!(e.contains("cannot combine"), "{e}");
    }

    #[test]
    fn sole_live_session_is_picked_with_notice() {
        let (a, notice) = choose_attach(&opts(None, false), &[live("abc")], None).unwrap();
        assert!(
            matches!(&a, Attach::Existing { session_id, .. } if session_id.as_str() == "abc"),
            "{a:?}"
        );
        let n = notice.unwrap();
        assert!(n.contains("abc") && n.contains("--new") && n.contains("daemon sessions"), "{n}");
    }

    #[test]
    fn no_live_sessions_creates_and_many_errors_with_hint() {
        let (a, _) = choose_attach(&opts(None, false), &[], None).unwrap();
        assert!(matches!(a, Attach::Create { .. }), "{a:?}");
        let e = choose_attach(&opts(None, false), &[live("a"), live("b")], None).unwrap_err();
        assert!(e.contains("--new") && e.contains("  a  ") && e.contains("  b  "), "{e}");
    }

    #[test]
    fn attach_mode_flags_override_env() {
        assert_eq!(attach_mode(false, true), AttachMode::Takeover);
        assert_eq!(attach_mode(true, false), AttachMode::Observe);
    }
}
