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
    /// `--continue [NAME_OR_ID]`: `Some(None)` = most recent journal (bare
    /// `--continue`), `Some(Some(x))` = that one. Must stay two-level: the
    /// attach path used to flatten it, which turned a bare `--continue`
    /// into a fresh session under adopt / `--new` (soak F21).
    pub continue_session: Option<Option<String>>,
    pub system: Option<String>,
    pub prompt_manifest: Option<PathBuf>,
    pub mode: AttachMode,
    pub keep_warm: bool,
    /// `--new`/`--create`: always create a fresh session even if one is
    /// live. Rejected together with an explicit `id`.
    pub new_session: bool,
    /// `--name`: name the created session (`synaps send --session <name>`
    /// resolves it at once).
    pub name: Option<String>,
    /// A plain `synaps` adopted a running daemon (no explicit `--attach`):
    /// after attaching, show what else lives in this daemon so parked
    /// work stays discoverable (F1).
    pub adopted: bool,
}

/// Toast id for the routine attach notes ([`attach_notices`]).
const ATTACH_TOAST_ID: &str = "attach";

/// What [`attach_notices`] needs to know about the attach that just happened.
struct AttachNoticeInputs<'a> {
    session_id: &'a SessionId,
    client_id: u64,
    mode: AttachMode,
    /// `Some(notice)` when another client owns input, so this one is read-only.
    owned_elsewhere: Option<String>,
    /// `Some(continued)` when a plain `synaps` adopted the daemon.
    adopted: Option<bool>,
    /// The daemon's other sessions ([`adopt_banner`]), adopt only.
    others_banner: Option<String>,
    config_warnings: &'a [String],
}

/// Attach notes, split by whether they need the user.
#[derive(Debug, Default, PartialEq, Eq)]
struct AttachNotices {
    /// Stays in the transcript: the other-sessions list, config warnings,
    /// and the attach line when input is owned elsewhere (this client is
    /// read-only until it takes over).
    transcript: Vec<String>,
    /// Routine notes ("adopted the running daemon", "attached to … as
    /// client #N") for a short toast. Kept out of the transcript so a fresh
    /// session starts empty and shows the boot logo, like in-process.
    toast: Vec<String>,
}

fn attach_notices(i: AttachNoticeInputs<'_>) -> AttachNotices {
    let mut out = AttachNotices::default();
    if let Some(continued) = i.adopted {
        let how = if continued { "continued session" } else { "fresh session" };
        out.toast.push(format!("adopted the running daemon ({how})"));
        out.toast.push("SYNAPS_DAEMON_ADOPT=0 for in-process".to_string());
        out.transcript.extend(i.others_banner);
    }
    for w in i.config_warnings {
        out.transcript.push(format!("⚠ config: {w}"));
    }
    let attached = format!("attached to {} as client #{} ({:?})", i.session_id, i.client_id, i.mode);
    match i.owned_elsewhere {
        Some(owned) => out.transcript.push(format!("{attached} — {owned}")),
        None => out.toast.insert(0, attached),
    }
    out
}

/// One-line inventory of the daemon's other sessions, shown when a plain
/// `synaps` adopts the daemon (F1): the lid-close user must be able to
/// *see* their parked task, without being surprise-attached to it.
/// `None` when nothing else is there.
pub fn adopt_banner(
    sessions: &[agent_engine::session::SessionMeta],
    ours: &SessionId,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<String> {
    use agent_engine::session::SessionLifecycle as L;
    let mut others: Vec<&agent_engine::session::SessionMeta> =
        sessions.iter().filter(|m| &m.id != ours).collect();
    if others.is_empty() {
        return None;
    }
    others.sort_by_key(|m| std::cmp::Reverse(m.created_at));
    let n = others.len();
    let parked = others.iter().filter(|m| matches!(m.lifecycle, L::Parked)).count();
    let live = n - parked;
    let mut line = format!(
        "{n} other session{} in this daemon ({live} live, {parked} parked) — `synaps --attach <ID>` joins a live one, `synaps --continue <ID>` resumes a parked one:",
        if n == 1 { "" } else { "s" }
    );
    for m in others.iter().take(5) {
        let age = now.signed_duration_since(m.created_at);
        let age = if age.num_hours() >= 1 {
            format!("{}h ago", age.num_hours())
        } else {
            format!("{}m ago", age.num_minutes().max(0))
        };
        let state = match m.lifecycle {
            L::Parked => "parked",
            _ if m.clients > 0 => "live, attached",
            _ => "live",
        };
        let short = m.id.as_str();
        let name = m.name.as_deref().map(|n| format!(" \"{n}\"")).unwrap_or_default();
        let cwd = m.cwd.as_ref().map(|c| format!(" {}", c.display())).unwrap_or_default();
        line.push_str(&format!("\n  {short}{name}  {state}  {age}{cwd}"));
    }
    if n > 5 {
        line.push_str(&format!("\n  … and {} more (`synaps daemon sessions`)", n - 5));
    }
    Some(line)
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
    await_extensions: bool,
) -> std::result::Result<(Attach, Option<String>), String> {
    let create = |continue_session: Option<Option<String>>| Attach::Create {
        config: SessionConfig {
            continue_session,
            system: opts.system.clone(),
            prompt_manifest: opts.prompt_manifest.clone(),
            cwd: cwd.clone(),
            env: None,
            compaction_policy: CompactionPolicyWire::LinkedSuccessor,
            await_extensions,
            keep_warm: opts.keep_warm,
            name: opts.name.clone(),
            ..Default::default()
        },
        mode: opts.mode,
    };
    if opts.new_session && opts.id.is_some() {
        return Err("cannot combine --attach <ID> with --new: pick one".to_string());
    }
    if let Some(c) = &opts.continue_session {
        return Ok((create(Some(c.clone())), None));
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

pub async fn run_attached(mut opts: AttachOpts) -> Result<()> {
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

    // T4: resolve path-like args (--system, --prompt-manifest) against the
    // client's cwd before shipping them to the daemon.
    if let Some(ref cwd_path) = cwd {
        let mut probe_cfg = SessionConfig {
            system: opts.system.clone(),
            prompt_manifest: opts.prompt_manifest.clone(),
            ..Default::default()
        };
        if let Err(e) = agent_engine::session::client_args::resolve_client_session_args(
            &mut probe_cfg,
            cwd_path,
        ) {
            // Print to stderr before entering raw mode so it survives.
            eprintln!("synaps: {e}");
            return Err(cfg_err(e));
        }
        opts.system = probe_cfg.system;
        opts.prompt_manifest = probe_cfg.prompt_manifest;
    }

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
    let welcome_sessions = conn.welcome.sessions.clone();
    // Quick Start (default on) => await_extensions=false: the daemon TUI
    // stops blocking on extension discovery before turn 1 (fast cold start),
    // matching the in-process TUI. Off => block like the historic daemon path.
    let await_extensions = !synaps_cli::load_config().startup.quick_start;
    let (attach, notice) =
        choose_attach(&opts, &live, cwd, await_extensions).map_err(cfg_err)?;
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
    // Notices queued before boot go first.
    for n in super::run_setup::take_boot_notices() {
        app.push_msg(ChatMessage::System(n));
    }
    let notices = attach_notices(AttachNoticeInputs {
        session_id: &snapshot.meta.id,
        client_id: transport.client_id().0,
        mode: transport.mode(),
        owned_elsewhere: snapshot.input_owned_elsewhere(transport.client_id()),
        adopted: opts.adopted.then_some(opts.continue_session.is_some()),
        others_banner: if opts.adopted {
            adopt_banner(&welcome_sessions, &snapshot.meta.id, chrono::Utc::now())
        } else {
            None
        },
        config_warnings: &config.warnings,
    });
    for n in notices.transcript {
        app.push_msg(ChatMessage::System(n));
    }
    if !notices.toast.is_empty() {
        app.toasts.upsert(
            super::toast::Toast::new(ATTACH_TOAST_ID, String::new())
                .titled("daemon")
                .lines(notices.toast),
        );
    }
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

    fn notice_inputs<'a>(id: &'a SessionId, warnings: &'a [String]) -> AttachNoticeInputs<'a> {
        AttachNoticeInputs {
            session_id: id,
            client_id: 1,
            mode: AttachMode::Mirror,
            owned_elsewhere: None,
            adopted: None,
            others_banner: None,
            config_warnings: warnings,
        }
    }

    #[test]
    fn routine_attach_notes_go_to_the_toast_so_the_logo_shows() {
        let id = SessionId::from("20261001-000000-ours".to_string());
        // plain `synaps` adopting the daemon, nothing else going on
        let n = attach_notices(AttachNoticeInputs { adopted: Some(false), ..notice_inputs(&id, &[]) });
        assert!(n.transcript.is_empty(), "a fresh session's transcript stays empty: {n:?}");
        assert_eq!(n.toast[0], "attached to 20261001-000000-ours as client #1 (Mirror)");
        assert_eq!(n.toast[1], "adopted the running daemon (fresh session)");
        assert!(n.toast[2].contains("SYNAPS_DAEMON_ADOPT=0"), "{n:?}");
        // `--continue` says so
        let n = attach_notices(AttachNoticeInputs { adopted: Some(true), ..notice_inputs(&id, &[]) });
        assert_eq!(n.toast[1], "adopted the running daemon (continued session)");
        // explicit `--attach`: only the attach line
        let n = attach_notices(notice_inputs(&id, &[]));
        assert_eq!(n, AttachNotices { transcript: vec![], toast: vec![
            "attached to 20261001-000000-ours as client #1 (Mirror)".to_string(),
        ] });
    }

    #[test]
    fn notes_that_need_the_user_stay_in_the_transcript() {
        let id = SessionId::from("20261001-000000-ours".to_string());
        let warnings = vec!["unknown key `fooo`".to_string()];
        let n = attach_notices(AttachNoticeInputs {
            adopted: Some(false),
            others_banner: Some("2 other sessions in this daemon".to_string()),
            owned_elsewhere: Some("input is owned by client #3 (tui)".to_string()),
            ..notice_inputs(&id, &warnings)
        });
        assert_eq!(n.transcript, vec![
            "2 other sessions in this daemon".to_string(),
            "⚠ config: unknown key `fooo`".to_string(),
            "attached to 20261001-000000-ours as client #1 (Mirror) — input is owned by client #3 (tui)"
                .to_string(),
        ]);
        // read-only attach: the attach line is in the transcript, not the toast
        assert!(n.toast.iter().all(|l| !l.starts_with("attached to")), "{n:?}");
        assert_eq!(n.toast[0], "adopted the running daemon (fresh session)");
    }

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
            name: None,
            adopted: false,
        }
    }

    fn meta(id: &str, lifecycle: agent_engine::session::SessionLifecycle, clients: usize, mins_ago: i64) -> agent_engine::session::SessionMeta {
        agent_engine::session::SessionMeta {
            id: SessionId::from(id.to_string()),
            name: None,
            model: "m".into(),
            cwd: Some(std::path::PathBuf::from("/w")),
            created_at: chrono::Utc::now() - chrono::Duration::minutes(mins_ago),
            continued: false,
            continue_info: None,
            host_pid: 1,
            lifecycle,
            clients,
            input_owner: None,
            awaiting_input: 0,
            journal_id: String::new(),
            locked_by: None,
        }
    }

    #[test]
    fn await_extensions_flows_from_quick_start() {
        // quick_start=false => caller passes await_extensions=true => the
        // created SessionConfig blocks on extensions (historic daemon path).
        let (a, _) = choose_attach(&opts(None, true), &[], None, true).unwrap();
        match a {
            Attach::Create { config, .. } => assert!(config.await_extensions),
            other => panic!("expected Create, got {other:?}"),
        }
        // quick_start=true (default) => await_extensions=false => fast start.
        let (a, _) = choose_attach(&opts(None, true), &[], None, false).unwrap();
        match a {
            Attach::Create { config, .. } => assert!(!config.await_extensions),
            other => panic!("expected Create, got {other:?}"),
        }
    }

    #[test]
    fn bare_continue_means_most_recent_not_fresh() {
        // F21: `synaps --continue` (no id) over the attach path must reach the
        // daemon as `continue_session: Some(None)` (= most recent journal),
        // even when --new is also set (adopt sets it).
        let mut o = opts(None, true);
        o.continue_session = Some(None);
        let (a, _) = choose_attach(&o, &[live("abc")], None, true).unwrap();
        match a {
            Attach::Create { config, .. } => assert_eq!(config.continue_session, Some(None)),
            other => panic!("expected Create, got {other:?}"),
        }
        let mut o = opts(None, false);
        o.continue_session = Some(Some("xyz".into()));
        let (a, _) = choose_attach(&o, &[], None, true).unwrap();
        match a {
            Attach::Create { config, .. } => assert_eq!(config.continue_session, Some(Some("xyz".into()))),
            other => panic!("expected Create, got {other:?}"),
        }
    }

    #[test]
    fn adopt_banner_lists_other_sessions_only() {
        use agent_engine::session::SessionLifecycle as L;
        let ours = SessionId::from("20260919-000000-ours".to_string());
        let now = chrono::Utc::now();
        // alone in the daemon → nothing to say
        assert!(adopt_banner(&[meta("20260919-000000-ours", L::Live, 1, 0)], &ours, now).is_none());
        let sessions = vec![
            meta("20260919-000000-ours", L::Live, 1, 0),
            meta("20260919-010000-aaaa", L::Parked, 0, 125),
            meta("20260919-020000-bbbb", L::Live, 1, 3),
        ];
        let b = adopt_banner(&sessions, &ours, now).unwrap();
        assert!(b.starts_with("2 other sessions in this daemon (1 live, 1 parked)"), "{b}");
        assert!(!b.contains("ours"), "{b}");
        // newest first, state + age + cwd per row
        let rows: Vec<&str> = b.lines().skip(1).collect();
        assert!(rows[0].contains("20260919-020000-bbbb") && rows[0].contains("live, attached") && rows[0].contains("m ago") && rows[0].contains("/w"), "{b}");
        assert!(rows[1].contains("20260919-010000-aaaa") && rows[1].contains("parked") && rows[1].contains("2h ago"), "{b}");
        // capped at 5 rows + overflow line
        let many: Vec<_> = (0..8).map(|i| meta(&format!("20260919-00000{i}-xxxx"), L::Parked, 0, i)).collect();
        let b = adopt_banner(&many, &ours, now).unwrap();
        assert_eq!(b.lines().count(), 1 + 5 + 1, "{b}");
        assert!(b.contains("and 3 more"), "{b}");
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
        let (a, notice) = choose_attach(&opts(None, true), &[live("abc")], None, true).unwrap();
        assert!(matches!(a, Attach::Create { .. }), "{a:?}");
        assert!(notice.is_none());
        let (a, _) = choose_attach(&opts(None, true), &[live("a"), live("b")], None, true).unwrap();
        assert!(matches!(a, Attach::Create { .. }), "{a:?}");
    }

    #[test]
    fn new_with_explicit_id_is_rejected() {
        let e = choose_attach(&opts(Some("abc"), true), &[live("abc")], None, true).unwrap_err();
        assert!(e.contains("cannot combine"), "{e}");
    }

    #[test]
    fn sole_live_session_is_picked_with_notice() {
        let (a, notice) = choose_attach(&opts(None, false), &[live("abc")], None, true).unwrap();
        assert!(
            matches!(&a, Attach::Existing { session_id, .. } if session_id.as_str() == "abc"),
            "{a:?}"
        );
        let n = notice.unwrap();
        assert!(n.contains("abc") && n.contains("--new") && n.contains("daemon sessions"), "{n}");
    }

    #[test]
    fn no_live_sessions_creates_and_many_errors_with_hint() {
        let (a, _) = choose_attach(&opts(None, false), &[], None, true).unwrap();
        assert!(matches!(a, Attach::Create { .. }), "{a:?}");
        let e = choose_attach(&opts(None, false), &[live("a"), live("b")], None, true).unwrap_err();
        assert!(e.contains("--new") && e.contains("  a  ") && e.contains("  b  "), "{e}");
    }

    #[test]
    fn attach_mode_flags_override_env() {
        assert_eq!(attach_mode(false, true), AttachMode::Takeover);
        assert_eq!(attach_mode(true, false), AttachMode::Observe);
    }
}
