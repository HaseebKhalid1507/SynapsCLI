//! `synaps daemon {start,status,stop,sessions,purge,reload,close,pin,rename,reload-extension}`
//! (PLAN-phase2 §2.11, B4; phase 3 C2 `purge`, C3 `reload`).

use std::path::PathBuf;
use std::time::Duration;

use agent_engine::daemon::close::{resolve_session, ResolveError};
use agent_engine::daemon::{self, pins, registry, DaemonOpts, EXIT_REFUSED, EXIT_VERSION};
use agent_engine::session::socket_transport::{Connected, SocketTransport};
use agent_engine::session::wire::Hello;
use agent_engine::session::{ClientKind, SessionLifecycle, SessionMeta, TransportError};
use clap::{Args, Subcommand};

#[derive(Args, Debug, Clone)]
pub(crate) struct DaemonArgs {
    #[command(subcommand)]
    pub action: Option<DaemonAction>,
    #[command(flatten)]
    pub start: StartArgs,
}

#[derive(Args, Debug, Clone, Default)]
pub(crate) struct StartArgs {
    /// Run in this terminal (default).
    #[arg(long, conflicts_with = "detach")]
    pub foreground: bool,
    /// Fork into the background (setsid) and return once the socket is ready.
    #[arg(long)]
    pub detach: bool,
    /// Socket path override (lock/json/pid stay under the run dir).
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
    /// Exit after SECS with zero clients, no turn running and no pinned
    /// session.
    #[arg(long, value_name = "SECS")]
    pub idle_exit: Option<u64>,
    /// Deprecated, ignored: legacy MCP was removed. Accepted so older
    /// clients can still spawn this daemon.
    #[arg(long, hide = true)]
    pub allow_legacy_mcp: bool,
    /// Print `{"binary_version","protocol_version"}` and exit 0 (the reload
    /// version gate probes the NEW binary with this).
    #[arg(long, hide = true)]
    pub print_version: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum DaemonAction {
    /// Start the daemon (same as no subcommand).
    Start(StartArgs),
    /// Registry + flock probe + Ping.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Graceful Shutdown{force} over the socket; --force escalates to SIGTERM/SIGKILL.
    Stop {
        #[arg(long)]
        force: bool,
    },
    /// List live sessions in the daemon.
    Sessions {
        #[arg(long)]
        json: bool,
    },
    /// Ask the daemon to return jemalloc dirty pages to the OS (bench hygiene:
    /// run before sampling RssAnon).
    Purge,
    /// Re-exec the daemon in place (same pid): checkpoint every session,
    /// exec the (newer-or-equal) binary, rehydrate, clients reconnect.
    Reload {
        /// Skip the drain wait (in-flight turns are checkpointed immediately).
        #[arg(long)]
        now: bool,
        /// Seconds to wait for turns to finish before checkpointing
        /// (default SYNAPS_DAEMON_RELOAD_DRAIN_SECS or 30).
        #[arg(long, value_name = "N")]
        drain_secs: Option<u64>,
        /// Binary to exec (default: the one recorded in daemon.json).
        #[arg(long, value_name = "PATH")]
        exe: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// End one session in the daemon (saved and ended, like /close); the
    /// others keep running. Resume it with `synaps --attach --continue <id>`.
    Close {
        /// Session id, id prefix, journal id, or name (see `daemon sessions`).
        #[arg(value_name = "ID|NAME")]
        target: String,
        /// Close even while a turn, compaction, worker or armed /auto run is
        /// in flight (it is cancelled).
        #[arg(long)]
        force: bool,
        /// Close even while clients are attached (their TUIs exit and any
        /// unsent input is lost). Independent of --force.
        #[arg(long)]
        attached: bool,
        #[arg(long)]
        json: bool,
    },
    /// Pin one session: it stays loaded (never parked or idle-ended), the
    /// daemon does not idle-exit while it is pinned, and it comes back when
    /// the daemon starts again. A parked session is woken. `--off` unpins,
    /// also a pin that is not live (see `daemon sessions`).
    Pin {
        /// Session id, id prefix, journal id, or name (see `daemon sessions`).
        #[arg(value_name = "ID|NAME")]
        target: String,
        /// Unpin instead (the session parks again when idle).
        #[arg(long)]
        off: bool,
        #[arg(long)]
        json: bool,
    },
    /// Name one session (as /saveas does), or clear its name with --clear,
    /// without attaching to it: no input ownership is needed and attached
    /// clients see the new name at once. A parked session is not woken; it
    /// is refused (exit 3) and can be renamed once it is live.
    Rename {
        /// Session id, id prefix, journal id, or current name (see `daemon sessions`).
        #[arg(value_name = "ID|NAME")]
        target: String,
        /// The new name: lowercase letters, digits and '-', at most 40.
        #[arg(value_name = "NEW_NAME", required_unless_present = "clear", conflicts_with = "clear")]
        name: Option<String>,
        /// Clear the session's name instead.
        #[arg(long)]
        clear: bool,
        #[arg(long)]
        json: bool,
    },
    /// Restart one extension from what is installed now, in every session,
    /// without `daemon reload`: no session is checkpointed and no turn is
    /// cancelled (only a call into the old process that is in flight
    /// fails). Also loads a plugin installed after the daemon started.
    ReloadExtension {
        /// The plugin's directory name, as `/extensions` lists it.
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(long)]
        json: bool,
    },
}

fn opts_from(profile: Option<String>, a: &StartArgs) -> DaemonOpts {
    DaemonOpts {
        socket: a.socket.clone(),
        profile,
        idle_exit: a.idle_exit.map(Duration::from_secs),
        allow_legacy_mcp: a.allow_legacy_mcp,
        runtime_dir: None,
        factory: None,
    }
}

/// Exit code 3 + one-line reason when `SYNAPS_DAEMON=0`.
pub(crate) fn require_enabled(what: &str) -> Result<(), i32> {
    if daemon::enabled() {
        return Ok(());
    }
    eprintln!("{what}: {}", daemon::DISABLED_NOTICE);
    Err(EXIT_REFUSED)
}

pub(crate) async fn run(profile: Option<String>, args: DaemonArgs) -> anyhow::Result<()> {
    match args.action {
        None => start(profile, args.start).await,
        Some(DaemonAction::Start(a)) => start(profile, a).await,
        Some(DaemonAction::Status { json }) => status(profile, json).await,
        Some(DaemonAction::Stop { force }) => stop(profile, force).await,
        Some(DaemonAction::Sessions { json }) => sessions(profile, json).await,
        Some(DaemonAction::Purge) => purge(profile).await,
        Some(DaemonAction::Reload { now, drain_secs, exe, json }) => reload(profile, now, drain_secs, exe, json).await,
        Some(DaemonAction::Close { target, force, attached, json }) => close(profile, target, force, attached, json).await,
        Some(DaemonAction::Pin { target, off, json }) => pin(profile, target, !off, json).await,
        Some(DaemonAction::Rename { target, name, clear, json }) => {
            rename(profile, target, if clear { None } else { name }, json).await
        }
        Some(DaemonAction::ReloadExtension { name, json }) => reload_extension(profile, name, json).await,
    }
}

/// `daemon reload-extension`: send `ReloadExtension`, report. Exit 0
/// reloaded (or loaded); 3 refused (daemon reloading); 2 daemon too old or
/// protocol mismatch; 1 otherwise (not installed, bad manifest, failed to
/// start, no daemon, timeout).
async fn reload_extension(profile: Option<String>, name: String, json: bool) -> anyhow::Result<()> {
    let paths = registry::daemon_paths(profile.as_deref());
    if !registry::is_alive(&paths) {
        reload_extension_exit(json, 1, &name, "no_daemon", None, false, "daemon not running");
    }
    let mut conn = match Connected::connect(&paths.sock, Hello::new(ClientKind::Attach)).await {
        Ok(c) => c,
        Err(e @ TransportError::Version { .. }) => {
            reload_extension_exit(json, EXIT_VERSION, &name, "version", None, false, &e.to_string())
        }
        Err(e) => reload_extension_exit(json, 1, &name, "error", None, false, &format!("cannot reach the daemon: {e}")),
    };
    let outcome = conn.reload_extension(&name).await;
    conn.bye().await;
    match outcome {
        Ok(Ok(report)) => {
            let verb = if report.was_loaded { "reloaded" } else { "loaded" };
            let mut text = format!("{verb} {}", report.extension);
            if report.deferred {
                text.push_str(" (deferred: its process starts on first use)");
            }
            match report.tools.len() {
                0 => text.push_str("; no tools"),
                n => text.push_str(&format!("; {n} tool{}: {}", if n == 1 { "" } else { "s" }, report.tools.join(", "))),
            }
            if !report.removed_tools.is_empty() {
                text.push_str(&format!("; removed: {}", report.removed_tools.join(", ")));
            }
            reload_extension_exit(json, 0, &name, verb, Some(&report), false, &text)
        }
        Ok(Err(failure)) => {
            let mut text = format!("{name} was not reloaded: {}", failure.message);
            if failure.unloaded {
                text.push_str(&format!("\n{name} is not loaded now; fix it and run this again"));
            } else {
                text.push_str("\nthe running instance (if any) was not touched");
            }
            reload_extension_exit(json, 1, &name, "failed", None, failure.unloaded, &text)
        }
        Err(TransportError::Refused(text)) => reload_extension_exit(json, EXIT_REFUSED, &name, "refused", None, false, &text),
        Err(e @ TransportError::DaemonTooOld { .. }) => {
            reload_extension_exit(json, EXIT_VERSION, &name, "too_old", None, false, &e.to_string())
        }
        Err(e) => reload_extension_exit(json, 1, &name, "error", None, false, &format!("extension reload failed: {e}")),
    }
}

/// Print the outcome (one JSON object with `--json`) and exit with `code`.
/// `report` is the daemon's report on success; `unloaded` says a failed
/// reload left the extension unloaded.
fn reload_extension_exit(
    json: bool,
    code: i32,
    name: &str,
    outcome: &str,
    report: Option<&agent_engine::session::socket_transport::ExtensionReloadReport>,
    unloaded: bool,
    text: &str,
) -> ! {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "ok": code == 0,
                "extension": name,
                "outcome": outcome,
                "unloaded": unloaded,
                "report": report,
                "text": text,
            })
        );
    } else if code == 0 {
        println!("{text}");
    } else {
        eprintln!("{text}");
    }
    std::process::exit(code);
}

/// `daemon pin`: resolve the target against the `Welcome` listing (as
/// `daemon close` does), send `Pin`, report. Exit 0 done; 3 refused (daemon
/// reloading, session live in another process); 2 daemon too old or
/// protocol mismatch; 1 otherwise (not found, ambiguous, prefix too
/// short, no daemon, timeout).
async fn pin(profile: Option<String>, target: String, on: bool, json: bool) -> anyhow::Result<()> {
    let paths = registry::daemon_paths(profile.as_deref());
    let book = pins::PinBook::for_active_profile();
    if !registry::is_alive(&paths) {
        // A pin that will come back on the next start can be cleared now.
        if let Some(e) = pins::resolve_entry(&book.load(), &target).filter(|_| !on) {
            let sid = Some(e.journal_id.as_str());
            match book.remove(&e.journal_id) {
                Ok(_) => pin_exit(json, 0, sid, "unpinned", false, &format!("unpinned {} (it will not come back)", e.journal_id)),
                Err(err) => pin_exit(json, 1, sid, "error", on, &format!("pin book not changed: {err}")),
            }
        }
        pin_exit(json, 1, None, "no_daemon", on, "daemon not running");
    }
    let mut conn = match Connected::connect(&paths.sock, Hello::new(ClientKind::Attach)).await {
        Ok(c) => c,
        Err(e @ TransportError::Version { .. }) => pin_exit(json, EXIT_VERSION, None, "version", on, &e.to_string()),
        Err(e) => pin_exit(json, 1, None, "error", on, &format!("cannot reach the daemon: {e}")),
    };
    let offline = pins::resolve_entry(&pins::offline(book.load(), &conn.welcome.sessions), &target);
    let id = match resolve_session(&conn.welcome.sessions, &target) {
        Ok(id) => id,
        // Pinned but not live (waiting / failed / not restored): the daemon
        // clears it (--off) or retries it.
        Err(ResolveError::NotFound) if offline.is_some() => {
            agent_engine::session::SessionId::from(offline.map(|e| e.journal_id).unwrap_or_default().as_str())
        }
        Err(ResolveError::NotFound) => {
            conn.bye().await;
            let text = format!("no live session or pin matches {target} (see `synaps daemon sessions`)");
            pin_exit(json, 1, None, "not_found", on, &text);
        }
        Err(ResolveError::TooShort { min }) => {
            conn.bye().await;
            let text = format!("{target} is too short to pick a session by prefix; give at least {min} characters, the full id or the name");
            pin_exit(json, 1, None, "too_short", on, &text);
        }
        Err(ResolveError::Ambiguous(matches)) => {
            conn.bye().await;
            let mut text = format!("{target} matches {} sessions; use a longer prefix or the full id:", matches.len());
            for m in &matches {
                text.push_str(&format!("\n  {}", m.id));
                if let Some(name) = &m.name {
                    text.push_str(&format!("  name={name}"));
                }
            }
            pin_exit(json, 1, None, "ambiguous", on, &text);
        }
    };
    let outcome = conn.pin(&id, on).await;
    conn.bye().await;
    let sid = Some(id.as_str());
    match outcome {
        Ok(true) => pin_exit(json, 0, sid, "pinned", true, &format!("pinned {id}")),
        Ok(false) => pin_exit(json, 0, sid, "unpinned", false, &format!("unpinned {id}")),
        Err(TransportError::Refused(text)) => pin_exit(json, EXIT_REFUSED, sid, "refused", on, &text),
        Err(TransportError::UnknownSession(text)) => pin_exit(json, 1, sid, "not_found", on, &text),
        Err(e @ TransportError::DaemonTooOld { .. }) => pin_exit(json, EXIT_VERSION, sid, "too_old", on, &e.to_string()),
        Err(e) => pin_exit(json, 1, sid, "error", on, &format!("pin failed: {e}")),
    }
}

/// `daemon rename`: resolve the target against the `Welcome` listing (as
/// `daemon close` does), send `Rename`, report. Exit 0 renamed or cleared;
/// 3 refused (session parked, live in another process, daemon reloading);
/// 2 daemon too old or protocol mismatch; 1 otherwise (invalid name, name
/// taken, chain conflict, not found, ambiguous, prefix too short, no
/// daemon, timeout).
async fn rename(profile: Option<String>, target: String, name: Option<String>, json: bool) -> anyhow::Result<()> {
    use agent_engine::session::wire::RenameRefusal;
    let paths = registry::daemon_paths(profile.as_deref());
    if !registry::is_alive(&paths) {
        rename_exit(json, 1, None, "no_daemon", None, None, "daemon not running");
    }
    let mut conn = match Connected::connect(&paths.sock, Hello::new(ClientKind::Attach)).await {
        Ok(c) => c,
        Err(e @ TransportError::Version { .. }) => rename_exit(json, EXIT_VERSION, None, "version", None, None, &e.to_string()),
        Err(e) => rename_exit(json, 1, None, "error", None, None, &format!("cannot reach the daemon: {e}")),
    };
    let id = match resolve_session(&conn.welcome.sessions, &target) {
        Ok(id) => id,
        Err(err) => {
            conn.bye().await;
            let (outcome, text) = resolve_error_text(&target, err);
            rename_exit(json, 1, None, outcome, None, None, &text);
        }
    };
    let outcome = conn.rename(&id, name.as_deref()).await;
    conn.bye().await;
    let sid = Some(id.as_str());
    match outcome {
        Ok(Ok(Some(n))) => rename_exit(json, 0, sid, "renamed", Some(&n), None, &format!("renamed {id} to {n}")),
        Ok(Ok(None)) => rename_exit(json, 0, sid, "cleared", None, None, &format!("cleared the name of {id}")),
        Ok(Err(f)) => {
            let code = match f.reason {
                RenameRefusal::Parked | RenameRefusal::Locked => EXIT_REFUSED,
                _ => 1,
            };
            rename_exit(json, code, sid, f.reason.as_str(), None, f.taken_by.as_deref(), &f.message)
        }
        Err(TransportError::Refused(text)) => rename_exit(json, EXIT_REFUSED, sid, "refused", None, None, &text),
        Err(TransportError::UnknownSession(text)) => rename_exit(json, 1, sid, "not_found", None, None, &text),
        Err(e @ TransportError::DaemonTooOld { .. }) => {
            rename_exit(json, EXIT_VERSION, sid, "too_old", None, None, &e.to_string())
        }
        Err(e) => rename_exit(json, 1, sid, "error", None, None, &format!("rename failed: {e}")),
    }
}

/// `(outcome, text)` for a target that did not resolve to one session.
fn resolve_error_text(target: &str, err: ResolveError) -> (&'static str, String) {
    match err {
        ResolveError::NotFound => ("not_found", format!("no live session matches {target} (see `synaps daemon sessions`)")),
        ResolveError::TooShort { min } => (
            "too_short",
            format!("{target} is too short to pick a session by prefix; give at least {min} characters, the full id or the name"),
        ),
        ResolveError::Ambiguous(matches) => {
            let mut text = format!("{target} matches {} sessions; use a longer prefix or the full id:", matches.len());
            for m in &matches {
                text.push_str(&format!("\n  {}", m.id));
                if let Some(name) = &m.name {
                    text.push_str(&format!("  name={name}"));
                }
            }
            ("ambiguous", text)
        }
    }
}

/// Like [`close_exit`], plus `name` (the session's name afterwards on
/// success, `null` when cleared or nothing was confirmed) and `taken_by`.
fn rename_exit(
    json: bool,
    code: i32,
    session_id: Option<&str>,
    outcome: &str,
    name: Option<&str>,
    taken_by: Option<&str>,
    text: &str,
) -> ! {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "ok": code == 0,
                "session_id": session_id,
                "outcome": outcome,
                "name": name,
                "taken_by": taken_by,
                "text": text,
            })
        );
    } else if code == 0 {
        println!("{text}");
    } else {
        eprintln!("{text}");
    }
    std::process::exit(code);
}

/// Like [`close_exit`], plus `pinned`: the session's pin afterwards on
/// success, `null` when nothing was confirmed.
fn pin_exit(json: bool, code: i32, session_id: Option<&str>, outcome: &str, on: bool, text: &str) -> ! {
    if json {
        let pinned = if code == 0 { serde_json::Value::Bool(on) } else { serde_json::Value::Null };
        println!(
            "{}",
            serde_json::json!({ "ok": code == 0, "session_id": session_id, "outcome": outcome, "pinned": pinned, "text": text })
        );
    } else if code == 0 {
        println!("{text}");
    } else {
        eprintln!("{text}");
    }
    std::process::exit(code);
}

/// `daemon close`: resolve the target against the `Welcome` listing, send
/// `Close`, report. Exit 0 closed; 3 refused (work in flight, clients
/// attached, daemon reloading); 2 daemon too old or protocol mismatch; 1
/// otherwise (not found, ambiguous, prefix too short, no daemon, timeout).
async fn close(profile: Option<String>, target: String, force: bool, attached: bool, json: bool) -> anyhow::Result<()> {
    let paths = registry::daemon_paths(profile.as_deref());
    if !registry::is_alive(&paths) {
        close_exit(json, 1, None, "no_daemon", "daemon not running");
    }
    let mut conn = match Connected::connect(&paths.sock, Hello::new(ClientKind::Attach)).await {
        Ok(c) => c,
        Err(e @ TransportError::Version { .. }) => close_exit(json, EXIT_VERSION, None, "version", &e.to_string()),
        Err(e) => close_exit(json, 1, None, "error", &format!("cannot reach the daemon: {e}")),
    };
    let id = match resolve_session(&conn.welcome.sessions, &target) {
        Ok(id) => id,
        Err(ResolveError::NotFound) => {
            conn.bye().await;
            close_exit(json, 1, None, "not_found", &format!("no live session matches {target} (see `synaps daemon sessions`)"));
        }
        Err(ResolveError::TooShort { min }) => {
            conn.bye().await;
            close_exit(
                json,
                1,
                None,
                "too_short",
                &format!("{target} is too short to pick a session by prefix; give at least {min} characters, the full id or the name"),
            );
        }
        Err(ResolveError::Ambiguous(matches)) => {
            conn.bye().await;
            let mut text = format!("{target} matches {} sessions; use a longer prefix or the full id:", matches.len());
            for m in &matches {
                text.push_str(&format!("\n  {}", m.id));
                if let Some(name) = &m.name {
                    text.push_str(&format!("  name={name}"));
                }
            }
            close_exit(json, 1, None, "ambiguous", &text);
        }
    };
    let outcome = conn.close(&id, force, attached).await;
    conn.bye().await;
    let sid = Some(id.as_str());
    match outcome {
        Ok(text) => close_exit(json, 0, sid, "closed", &text),
        // The actor's refusal names `/close force`; here the flag is `--force`.
        Err(TransportError::Refused(text)) => {
            close_exit(json, EXIT_REFUSED, sid, "refused", &text.replace("/close force", "--force"))
        }
        Err(TransportError::UnknownSession(text)) => close_exit(json, 1, sid, "not_found", &text),
        Err(e @ TransportError::DaemonTooOld { .. }) => close_exit(json, EXIT_VERSION, sid, "too_old", &e.to_string()),
        Err(e) => close_exit(json, 1, sid, "error", &format!("close failed: {e}")),
    }
}

/// One line (stdout when closed, stderr otherwise) or one JSON object on
/// stdout, then exit with `code`.
fn close_exit(json: bool, code: i32, session_id: Option<&str>, outcome: &str, text: &str) -> ! {
    if json {
        println!(
            "{}",
            serde_json::json!({ "ok": code == 0, "session_id": session_id, "outcome": outcome, "text": text })
        );
    } else if code == 0 {
        println!("{text}");
    } else {
        eprintln!("{text}");
    }
    std::process::exit(code);
}

async fn reload(
    profile: Option<String>,
    now: bool,
    drain_secs: Option<u64>,
    exe: Option<PathBuf>,
    json: bool,
) -> anyhow::Result<()> {
    let paths = registry::daemon_paths(profile.as_deref());
    if !registry::is_alive(&paths) {
        anyhow::bail!("daemon not running");
    }
    // A Ping first: the daemon puts a lost daemon.json back before we read it.
    let _ = SocketTransport::ping(&paths.sock).await;
    let before = registry::read_daemon_json(&paths);
    let exe = exe.map(|p| p.canonicalize().unwrap_or(p));
    let outcome = SocketTransport::reload(&paths.sock, now, drain_secs, exe).await;
    match outcome {
        Ok(gen) => {
            // Wait for the new image to answer (≤ 15 s).
            let t0 = std::time::Instant::now();
            let mut pong = None;
            while t0.elapsed() < Duration::from_secs(15) {
                tokio::time::sleep(Duration::from_millis(250)).await;
                if let Ok(p) = SocketTransport::ping(&paths.sock).await {
                    pong = Some(p);
                    break;
                }
            }
            let after = registry::read_daemon_json(&paths);
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": pong.is_some(),
                        "pid": pong.as_ref().map(|p| p.pid),
                        "generation": after.as_ref().map(|i| i.generation),
                        "announced_generation": gen,
                        "sessions": pong.as_ref().map(|p| p.sessions),
                        "exe": after.as_ref().and_then(|i| i.exe.clone()),
                        "previous_version": before.as_ref().map(|i| i.daemon_version.clone()),
                        "version": after.as_ref().map(|i| i.daemon_version.clone()),
                    })
                );
            } else if let Some(p) = &pong {
                println!(
                    "reloaded (pid {}, generation {}, sessions {})",
                    p.pid,
                    after.as_ref().map_or(gen, |i| i.generation),
                    p.sessions
                );
            } else {
                anyhow::bail!("reload announced (generation {gen}) but the daemon did not answer within 15 s");
            }
            if pong.is_none() {
                std::process::exit(1);
            }
            Ok(())
        }
        Err(e) => {
            if json {
                println!("{}", serde_json::json!({"ok": false, "error": e.to_string()}));
            } else {
                eprintln!("reload refused: {e}");
            }
            std::process::exit(EXIT_REFUSED);
        }
    }
}

async fn purge(profile: Option<String>) -> anyhow::Result<()> {
    let paths = registry::daemon_paths(profile.as_deref());
    if !registry::is_alive(&paths) {
        anyhow::bail!("daemon not running");
    }
    let pong = SocketTransport::purge(&paths.sock).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("purged (pid {}, sessions {})", pong.pid, pong.sessions);
    Ok(())
}

async fn start(profile: Option<String>, a: StartArgs) -> anyhow::Result<()> {
    if a.print_version {
        println!("{}", serde_json::to_string(&daemon::reload::PrintVersion::current())?);
        return Ok(());
    }
    if let Err(code) = require_enabled("synaps daemon") {
        std::process::exit(code);
    }
    let opts = opts_from(profile, &a);
    if a.detach {
        let paths = opts.paths();
        if registry::is_alive(&paths) {
            let _ = SocketTransport::ping(&paths.sock).await;
            let pid = registry::read_daemon_json(&paths).map(|i| i.pid);
            println!("daemon already running (pid {})", pid.map_or("?".into(), |p| p.to_string()));
            return Ok(());
        }
        let pid = daemon::spawn_detached(&opts)?;
        println!("daemon started (pid {pid}, socket {})", opts.paths().sock.display());
        return Ok(());
    }
    match daemon::run_foreground(opts).await {
        Ok(()) => Ok(()),
        Err(e) => {
            eprintln!("synaps daemon: {e}");
            std::process::exit(EXIT_REFUSED);
        }
    }
}

async fn status(profile: Option<String>, json: bool) -> anyhow::Result<()> {
    let paths = registry::daemon_paths(profile.as_deref());
    let alive = registry::is_alive(&paths);
    let pong = if alive { SocketTransport::ping(&paths.sock).await.ok() } else { None };
    // After the Ping: the daemon puts a lost daemon.json back first.
    let info = registry::read_daemon_json(&paths);
    let state = match (info.is_some(), alive, pong.is_some()) {
        (_, true, true) => "running",
        (_, true, false) => "running (not answering)",
        (true, false, _) => "stale (pid dead)",
        (false, false, _) => "not running (auto-starts on first --attach)",
    };
    if json {
        println!(
            "{}",
            serde_json::json!({
                "state": state,
                "ok": pong.is_some(),
                "socket": paths.sock,
                "pid": pong.as_ref().map(|p| p.pid).or(info.as_ref().map(|i| i.pid)),
                "uptime_s": pong.as_ref().map(|p| p.uptime_s),
                "sessions": pong.as_ref().map(|p| p.sessions),
                "daemon_version": info.as_ref().map(|i| i.daemon_version.clone()),
                "protocol_version": info.as_ref().map(|i| i.protocol_version),
                "profile": info.as_ref().and_then(|i| i.profile.clone()),
            })
        );
    } else {
        println!("daemon: {state}");
        println!("socket: {}", paths.sock.display());
        if let Some(i) = &info {
            println!("pid: {}  version: {}  protocol: {}  started: {}", i.pid, i.daemon_version, i.protocol_version, i.started_at);
        }
        if let Some(p) = &pong {
            println!("uptime: {}s  sessions: {}", p.uptime_s, p.sessions);
        }
    }
    if pong.is_none() {
        std::process::exit(1);
    }
    Ok(())
}

async fn stop(profile: Option<String>, force: bool) -> anyhow::Result<()> {
    let paths = registry::daemon_paths(profile.as_deref());
    if !registry::is_alive(&paths) {
        println!("daemon not running");
        registry::reap_stale(&paths);
        return Ok(());
    }
    // A Ping first: the daemon puts a lost daemon.json back, so the pid for
    // the kill fallback below is known.
    let _ = SocketTransport::ping(&paths.sock).await;
    let pid = registry::read_daemon_json(&paths).map(|i| i.pid);
    match SocketTransport::shutdown(&paths.sock, force).await {
        Ok(()) => {}
        Err(e) => eprintln!("shutdown request failed: {e}"),
    }
    // Wait for the flock to be released.
    let grace = Duration::from_secs(if force { 10 } else { 30 });
    let t0 = std::time::Instant::now();
    while registry::is_alive(&paths) {
        if t0.elapsed() > grace {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    #[cfg(unix)]
    if force && registry::is_alive(&paths) {
        if let Some(pid) = pid {
            eprintln!("daemon still alive after {grace:?}; SIGTERM {pid}");
            let _ = std::process::Command::new("kill").args(["-TERM", &pid.to_string()]).status();
            let t1 = std::time::Instant::now();
            while registry::is_alive(&paths) && t1.elapsed() < Duration::from_secs(5) {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if registry::is_alive(&paths) {
                eprintln!("SIGKILL {pid}");
                let _ = std::process::Command::new("kill").args(["-KILL", &pid.to_string()]).status();
            }
        }
    }
    if registry::is_alive(&paths) {
        anyhow::bail!("daemon did not stop");
    }
    registry::reap_stale(&paths);
    println!("daemon stopped{}", pid.map_or(String::new(), |p| format!(" (pid {p})")));
    if let Some(line) = comes_back_line(&pins::PinBook::for_active_profile().load()) {
        println!("{line}");
    }
    Ok(())
}

/// `clients=` in `daemon sessions`: the count, then the names when the
/// daemon sent them — `2 (tui*, discord)`, `*` = owns input, observers
/// marked `(observe)`.
fn clients_field(m: &agent_engine::session::SessionMeta) -> String {
    if m.roster.is_empty() {
        return m.clients.to_string();
    }
    let names: Vec<String> = m
        .roster
        .iter()
        .map(|c| {
            let owner = if m.input_owner == Some(c.id) { "*" } else { "" };
            let observe = if c.mode == agent_engine::session::AttachMode::Observe { "(observe)" } else { "" };
            format!("{}{owner}{observe}", c.name)
        })
        .collect();
    format!("{} ({})", m.clients, names.join(", "))
}

/// `owner=`: `tui(#1)`, or `#1` from an older daemon.
fn owner_field(m: &agent_engine::session::SessionMeta, c: agent_engine::session::ClientId) -> String {
    match m.roster.iter().find(|i| i.id == c) {
        Some(i) => format!("{}(#{})", i.name, c.0),
        None => format!("#{}", c.0),
    }
}

/// `streaming 2m` / `waiting 1m` / `idle 4m · parks in 42s` /
/// `parked 12m · evicts in 48m` — what `/sessions` shows, for the CLI.
/// An older daemon sends no `state_since`: the age is left out.
fn state_phrase(m: &SessionMeta, now: chrono::DateTime<chrono::Utc>) -> String {
    let word = match m.lifecycle {
        SessionLifecycle::Parked => "parked",
        SessionLifecycle::Parking => "parking",
        SessionLifecycle::Ending => "ending",
        SessionLifecycle::Live if m.awaiting_input > 0 => "waiting",
        SessionLifecycle::Live if m.busy => "streaming",
        SessionLifecycle::Live => "idle",
    };
    let mut s = word.to_string();
    if let Some(since) = m.state_since {
        s.push(' ');
        s.push_str(&short_dur(now - since));
    }
    if let Some(due) = m.park_due {
        s.push_str(&format!(" · parks in {}", short_dur(due - now)));
    }
    if let Some(due) = m.evict_due {
        s.push_str(&format!(" · evicts in {}", short_dur(due - now)));
    }
    s
}

/// `42s` / `4m` / `3h` / `2d`; negative (clock skew, overdue) reads `0s`.
fn short_dur(d: chrono::TimeDelta) -> String {
    let secs = d.num_seconds().max(0);
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// `  $1.03  58 msgs  "title"` (cost/messages only once there are any).
fn title_part(m: &SessionMeta) -> String {
    let mut s = String::new();
    if m.messages > 0 {
        s.push_str(&format!("  ${:.2}  {} msgs", m.cost_usd, m.messages));
    }
    if let Some(t) = m.title.as_deref().filter(|t| !t.is_empty()) {
        s.push_str(&format!("  \"{t}\""));
    }
    s
}

async fn sessions(profile: Option<String>, json: bool) -> anyhow::Result<()> {
    let paths = registry::daemon_paths(profile.as_deref());
    let book = pins::PinBook::for_active_profile();
    if !registry::is_alive(&paths) {
        if !json {
            if let Some(line) = comes_back_line(&book.load()) {
                println!("{line}");
            }
        }
        anyhow::bail!("daemon not running");
    }
    let list = SocketTransport::sessions(&paths.sock).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let offline = pins::offline(book.load(), &list);
    if json {
        println!("{}", serde_json::to_string_pretty(&list)?);
    } else if list.is_empty() && offline.is_empty() {
        println!("no sessions");
    } else {
        let now = chrono::Utc::now();
        for m in list {
            // Shared input: every drive client types; `*` marks the active
            // one (typed last), so don't call it the owner.
            let owner_key = if m.client_input == Some(agent_engine::session::ClientInput::Shared) {
                "active"
            } else {
                "owner"
            };
            println!(
                "{}  {:?}  {}{}  clients={}  {owner_key}={}  model={}  cwd={}  created={}{}{}{}",
                m.id,
                m.lifecycle,
                state_phrase(&m, now),
                if m.pinned { "  pinned" } else { "" },
                clients_field(&m),
                m.input_owner.map_or("-".into(), |c| owner_field(&m, c)),
                m.model,
                m.cwd.as_deref().map_or("-".into(), |p| p.display().to_string()),
                m.created_at.format("%H:%M:%S"),
                m.name.as_deref().map_or(String::new(), |n| format!("  name={n}")),
                if m.journal_id != m.id.as_str() { format!("  journal={}", m.journal_id) } else { String::new() },
                title_part(&m)
            );
        }
    }
    if !json {
        for e in &offline {
            println!("{}", offline_line(e));
        }
    }
    Ok(())
}

/// A pin that is not live: `<id>  pinned (not live)  waiting: held by …`.
fn offline_line(e: &pins::PinEntry) -> String {
    format!(
        "{}  pinned (not live)  {}{}{}  (`synaps daemon pin {} --off` clears it)",
        e.journal_id,
        e.state_text(),
        e.name.as_deref().map_or(String::new(), |n| format!("  name={n}")),
        e.cwd.as_deref().map_or(String::new(), |p| format!("  cwd={}", p.display())),
        e.journal_id,
    )
}

/// `2 pinned sessions will come back on next start: axon, 20261025-…`.
fn comes_back_line(entries: &[pins::PinEntry]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let n = entries.len();
    let names: Vec<&str> = entries.iter().map(|e| e.label()).collect();
    Some(format!(
        "{n} pinned session{} will come back on next start: {}",
        if n == 1 { "" } else { "s" },
        names.join(", ")
    ))
}

#[cfg(test)]
mod pin_line_tests {
    use super::*;

    #[test]
    fn stop_names_what_comes_back() {
        assert_eq!(comes_back_line(&[]), None);
        let e = |id: &str, name: Option<&str>| pins::PinEntry { journal_id: id.into(), name: name.map(Into::into), ..Default::default() };
        assert_eq!(
            comes_back_line(&[e("j1", Some("axon")), e("j2", None)]).as_deref(),
            Some("2 pinned sessions will come back on next start: axon, j2")
        );
        assert_eq!(comes_back_line(&[e("j1", None)]).as_deref(), Some("1 pinned session will come back on next start: j1"));
    }

    #[test]
    fn an_offline_pin_names_its_state_and_how_to_clear_it() {
        let e = pins::PinEntry { journal_id: "j1".into(), name: Some("axon".into()), ..Default::default() };
        assert_eq!(offline_line(&e), "j1  pinned (not live)  not restored yet  name=axon  (`synaps daemon pin j1 --off` clears it)");
    }
}

#[cfg(test)]
mod state_phrase_tests {
    use super::*;
    use chrono::{TimeDelta, Utc};

    fn meta() -> SessionMeta {
        let mut m = agent_engine::session::handle::echo::meta_for(&"s".into());
        m.state_since = None;
        m
    }

    #[test]
    fn phrases() {
        let now = Utc::now();
        let mut m = meta();
        assert_eq!(state_phrase(&m, now), "idle", "old daemon: no age");
        m.busy = true;
        m.state_since = Some(now - TimeDelta::seconds(125));
        assert_eq!(state_phrase(&m, now), "streaming 2m");
        m.busy = false;
        m.state_since = Some(now - TimeDelta::minutes(4));
        m.park_due = Some(now + TimeDelta::seconds(42));
        assert_eq!(state_phrase(&m, now), "idle 4m · parks in 42s");
        m.park_due = None;
        m.lifecycle = SessionLifecycle::Parked;
        m.state_since = Some(now - TimeDelta::minutes(12));
        m.evict_due = Some(now + TimeDelta::minutes(48) + TimeDelta::seconds(5));
        assert_eq!(state_phrase(&m, now), "parked 12m · evicts in 48m");
    }

    #[test]
    fn title_and_cost() {
        let mut m = meta();
        assert_eq!(title_part(&m), "");
        m.messages = 58;
        m.cost_usd = 1.034;
        m.title = Some("any ai events".into());
        assert_eq!(title_part(&m), "  $1.03  58 msgs  \"any ai events\"");
    }
}

#[cfg(test)]
mod rename_cli_tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        action: DaemonAction,
    }

    fn parse(args: &[&str]) -> Result<DaemonAction, clap::Error> {
        Cli::try_parse_from(std::iter::once("daemon").chain(args.iter().copied())).map(|c| c.action)
    }

    #[test]
    fn rename_takes_a_target_and_a_name_or_clear() {
        match parse(&["rename", "20261028", "axon", "--json"]).unwrap() {
            DaemonAction::Rename { target, name, clear, json } => {
                assert_eq!((target.as_str(), name.as_deref(), clear, json), ("20261028", Some("axon"), false, true));
            }
            other => panic!("{other:?}"),
        }
        match parse(&["rename", "axon", "--clear"]).unwrap() {
            DaemonAction::Rename { target, name, clear, .. } => {
                assert_eq!((target.as_str(), name, clear), ("axon", None, true));
            }
            other => panic!("{other:?}"),
        }
        assert!(parse(&["rename", "axon"]).is_err(), "a new name or --clear is required");
        assert!(parse(&["rename", "axon", "new", "--clear"]).is_err(), "not both");
        assert!(parse(&["rename"]).is_err());
    }

    #[test]
    fn an_unresolved_target_names_its_outcome() {
        assert_eq!(resolve_error_text("x", ResolveError::NotFound).0, "not_found");
        let (outcome, text) = resolve_error_text("2026", ResolveError::TooShort { min: 8 });
        assert_eq!(outcome, "too_short");
        assert!(text.contains("at least 8"), "{text}");
    }
}
