#![allow(
    clippy::too_many_arguments,
    clippy::collapsible_if,
    clippy::collapsible_match,
    clippy::single_match,
    clippy::field_reassign_with_default,
    clippy::manual_clamp,
    clippy::needless_borrow,
    clippy::explicit_auto_deref,
    clippy::manual_strip,
    clippy::unwrap_or_default,
    clippy::await_holding_lock,
    clippy::useless_format,
    clippy::cmp_owned,
    clippy::items_after_test_module
)]

use clap::{Parser, Subcommand};

use synaps_cli::tui;
mod cmd;
#[cfg(unix)]
mod watcher;

// ── Allocator ────────────────────────────────────────────────────────────────
// Long-lived sessions serialize the ENTIRE conversation to a JSON body every
// turn (prompt caching requires sending the full prefix — see
// runtime/api.rs `serde_json::to_vec(&body)`). That transient buffer scales
// with history length; on glibc the freed arena pages are never returned to the
// OS, so RSS ratchets up to ~history size and never shrinks (the classic
// transient-heavy-long-lived-process hysteresis).
//
// jemalloc's background thread purges cold dirty pages back to the OS, which
// reclaims exactly that dead-but-retained arena — with zero impact on caching
// or retained history. Gated to non-musl: musl's allocator already returns
// memory readily, and the Pria agentic-VM runtime is a musl build we don't want
// to perturb.
#[cfg(all(unix, not(target_env = "musl")))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

// Configure jemalloc at load time (no env var needed). Three settings:
//   - background_thread:true  → background thread purges cold dirty pages to OS
//   - narenas:4               → cap arenas (default is 4×ncpu = 48 on a 12-core
//                               box). 26 runtime threads sprawling across 48
//                               arenas reserve ~70 MB of 4 MB chunks at baseline
//                               for no benefit; 4 arenas is ample for our
//                               allocation concurrency and slashes idle RSS.
//   - dirty_decay_ms:1000     → return idle dirty pages ~1s after they go cold
//   - muzzy_decay_ms:0        → purge muzzy pages immediately
//
// CRITICAL: tikv-jemalloc-sys is built with the `_rjem_` symbol prefix, so
// jemalloc reads the config from `_rjem_malloc_conf` — NOT `malloc_conf`. An
// earlier revision exported the unmangled name, which jemalloc silently ignored
// (verified: opt.dirty_decay_ms sat at the 10000ms default). Keep this mangled.
#[cfg(all(unix, not(target_env = "musl")))]
#[allow(non_upper_case_globals)]
#[export_name = "_rjem_malloc_conf"]
pub static MALLOC_CONF: &[u8] =
    b"background_thread:true,narenas:4,dirty_decay_ms:1000,muzzy_decay_ms:0\0";

#[derive(Parser)]
#[command(
    name = "synaps",
    about = "SynapsCLI — terminal-native AI agent runtime (TUI, headless, server, RPC)",
    long_about = "SynapsCLI — terminal-native AI agent runtime.\n\nRun with no arguments for the interactive TUI. Subcommands provide headless chat,\na WebSocket server, autonomous agents, and event injection into running sessions.",
    version
)]
struct Cli {
    /// Configuration profile (loads ~/.synaps-cli/<PROFILE>/config)
    #[arg(long, global = true)]
    profile: Option<String>,

    /// Continue a previous session (TUI only). Optionally provide a session ID.
    #[arg(long = "continue", value_name = "NAME_OR_ID")]
    continue_session: Option<Option<String>>,

    /// System prompt: a string or path to a file (TUI only).
    #[arg(long = "system", short = 's', value_name = "PROMPT_OR_FILE")]
    system: Option<String>,

    /// Typed modular prompt manifest (validated offline before session start).
    #[arg(long = "prompt-manifest", value_name = "PATH")]
    prompt_manifest: Option<std::path::PathBuf>,

    /// Disable all extensions for this session.
    #[arg(long)]
    no_extensions: bool,

    /// Run the TUI attached to a daemon session instead of in-process
    /// (the daemon is started on first use). Optionally a session ID
    /// (prefix ok); with no live session a new one is created (`--continue`
    /// continues); --new (alias --create) forces a fresh session even if
    /// one is live. Modifiers: --observe (read-only), --takeover (steal input),
    /// --keep-warm (never parked), --new (fresh session); default mode is
    /// SYNAPS_ATTACH_MODE or mirror (input only if nobody owns it).
    #[arg(long = "attach", value_name = "ID", global = true, num_args = 0..=1)]
    attach: Option<Option<String>>,

    /// With --attach: mirror without input (read-only).
    #[arg(long, global = true, conflicts_with = "takeover")]
    observe: bool,

    /// With --attach: steal input ownership from the current owner (the
    /// previous owner is told; without it a second client is read-only
    /// while the owner is attached).
    #[arg(long, global = true)]
    takeover: bool,

    /// With --attach: pin the session live (never parked).
    #[arg(long = "keep-warm", global = true)]
    keep_warm: bool,

    /// With --attach: always create a fresh session even if one is live
    /// (cannot be combined with an explicit session ID). Not global: the
    /// `attach` subcommand has its own `--create`/`--new`.
    #[arg(long = "new", alias = "create", requires = "attach")]
    new_session: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Headless chat with full engine (MCP, extensions, skills, sessions)
    Chat {
        /// Continue a previous session (optional session ID, name, or chain)
        #[arg(long, short = 'c')]
        continue_session: Option<String>,
        /// System prompt (path or inline)
        #[arg(long, short = 's')]
        system: Option<String>,
        /// Agent name (loads agent prompt from ~/.synaps-cli/agents/)
        #[arg(long, short = 'a')]
        agent: Option<String>,
        /// Profile name
        #[arg(long)]
        profile: Option<String>,
        /// Disable extensions
        #[arg(long)]
        no_extensions: bool,
    },
    /// WebSocket API server
    Server {
        #[arg(long, short, default_value = "3145")]
        port: u16,
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long = "system", short = 's')]
        system: Option<String>,
        #[arg(long = "continue", value_name = "NAME_OR_ID")]
        continue_session: Option<Option<String>>,
        /// Auth token (overrides config). Empty string disables auth.
        #[arg(long)]
        token: Option<String>,
        /// Auto-approve extension confirm hooks without prompting.
        #[arg(long)]
        auto_approve_confirms: bool,
        /// Comma-separated allowed origins (overrides config).
        #[arg(long)]
        allowed_origins: Option<String>,
    },
    /// Headless autonomous agent
    Agent {
        #[arg(long)]
        config: String,
        #[arg(long, default_value = "manual start")]
        trigger_context: String,
    },
    /// Agent supervisor and watcher
    Watcher {
        #[arg(default_value = "help")]
        subcommand: String,
        /// Additional arguments
        args: Vec<String>,
    },
    /// OAuth login
    Login {
        /// Non-interactive: log in directly with a named provider (skips the picker). e.g. openai-codex, claude
        #[arg(long)]
        provider: Option<String>,
    },
    /// Authentication management
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// Show account usage and reset times
    Status {
        /// Show memory (RSS/PSS/USS/RssAnon) per live session process tree
        /// instead of account usage. Linux only.
        #[arg(long)]
        memory: bool,
        /// With --memory: emit JSON instead of a table.
        #[arg(long, requires = "memory")]
        json: bool,
        /// With --memory: walk this pid's tree instead of the live sessions.
        #[arg(long, requires = "memory")]
        pid: Option<u32>,
    },
    /// Credential broker — serve short-lived access tokens to client machines
    /// over HTTP/HTTPS so they can share one OAuth credential without storing it.
    ///
    /// For non-loopback binds, pass --tls-cert + --tls-key to enable HTTPS (recommended),
    /// or --insecure-http to acknowledge you are running behind WireGuard / a private network.
    AuthBroker {
        /// Address to bind, e.g. `0.0.0.0:8181` or `127.0.0.1:8181`.
        #[arg(long, default_value = "127.0.0.1:8181")]
        bind: String,
        /// REJECTED — a token in argv leaks via `ps aux` and
        /// /proc/<pid>/cmdline. Use `--machine-token-file` or
        /// `SYNAPS_BROKER_TOKEN`. Still parsed so we can emit a migration error.
        #[arg(long)]
        machine_token: Option<String>,
        /// Read the machine token from a file (avoids exposing it in argv/`ps`).
        #[arg(long)]
        machine_token_file: Option<std::path::PathBuf>,
        /// Allow starting with auth OFF on a non-loopback bind (serves
        /// credentials unauthenticated to the network — NOT recommended).
        #[arg(long)]
        insecure_no_auth: bool,
        /// PEM file containing the TLS certificate chain. Must be paired with --tls-key.
        /// When both are set, the broker listens on HTTPS.
        #[arg(long, value_name = "PATH")]
        tls_cert: Option<std::path::PathBuf>,
        /// PEM file containing the TLS private key. Must be paired with --tls-cert.
        #[arg(long, value_name = "PATH")]
        tls_key: Option<std::path::PathBuf>,
        /// Allow plain HTTP on a non-loopback bind without TLS.
        /// Only use this when the broker is behind WireGuard or another encrypted
        /// private network — the traffic will be unencrypted in-process.
        #[arg(long)]
        insecure_http: bool,
    },
    /// Session daemon — status/stop/sessions/reload (auto-started by --attach)
    Daemon(cmd::daemon::DaemonArgs),
    /// Thin line client attached to a daemon session (starts the daemon if needed)
    Attach(cmd::attach::AttachArgs),
    /// Headless line-JSON RPC server on stdin/stdout (synaps-bridge IPC)
    Rpc {
        /// Resume an existing session by ID, name, or prefix.
        #[arg(long = "continue", value_name = "SESSION_ID")]
        continue_id: Option<String>,
        /// System prompt: a string or path to a file.
        #[arg(long = "system", short = 's', value_name = "PROMPT_OR_FILE")]
        system: Option<String>,
        /// Override the active model for this session.
        #[arg(long = "model", short = 'm', value_name = "MODEL_ID")]
        model: Option<String>,
        /// Configuration profile to load.
        #[arg(long = "profile", value_name = "PROFILE")]
        profile: Option<String>,
    },
    /// Send an event to the inbox (picked up by running session)
    Send {
        /// Message text
        message: String,
        #[arg(long, default_value = "cli")]
        source: String,
        #[arg(long, default_value = "medium")]
        severity: String,
        #[arg(long)]
        channel: Option<String>,
        #[arg(long = "content-type", default_value = "message")]
        content_type: String,
        /// Target a specific session by ID, name, or prefix
        #[arg(long, value_name = "SESSION")]
        session: Option<String>,
        /// Send to all active sessions
        #[arg(long)]
        broadcast: bool,
    },
    /// Generate shell completions (bash, zsh, fish, elvish, powershell)
    Completions {
        /// Target shell
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Offline modular prompt validation and inspection.
    Prompt {
        #[command(subcommand)]
        action: cmd::prompt::PromptAction,
    },
    /// Tool-surface utilities (schema export, etc.)
    Tools {
        #[command(subcommand)]
        action: cmd::tools::ToolsAction,
    },
    /// Request-trace export utilities (metadata-only by default).
    Trace {
        #[command(subcommand)]
        action: cmd::trace::TraceAction,
    },
    /// Unified retention: inspect/sweep/export/forget across sessions,
    /// memory, indexes, traces, and logs (chain-integrity-safe).
    Retention {
        #[command(subcommand)]
        action: cmd::retention::RetentionAction,
    },
}

#[derive(Subcommand)]
enum AuthAction {
    /// OAuth / API-key login (non-interactive with --provider, interactive without)
    Login {
        /// Non-interactive: log in directly with a named provider (skips the picker). e.g. openai-codex, claude
        #[arg(long)]
        provider: Option<String>,
    },
}

/// Tokio worker-thread count (§3.6 process diet). `SYNAPS_WORKER_THREADS`:
/// `n > 0` → exactly `n`; `0` → tokio's default (one per core, the old
/// behaviour — the kill-switch); unset/invalid → `min(4, available_parallelism)`.
/// Only the async worker pool is capped; `spawn_blocking` is untouched.
fn worker_threads() -> Option<usize> {
    let ncpu = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    worker_threads_from(std::env::var("SYNAPS_WORKER_THREADS").ok().as_deref(), ncpu)
}

fn worker_threads_from(raw: Option<&str>, ncpu: usize) -> Option<usize> {
    match raw.and_then(|v| v.trim().parse::<usize>().ok()) {
        Some(0) => None,
        Some(n) => Some(n),
        None => Some(ncpu.clamp(1, 4)),
    }
}

/// Which thin client argv asks for (`SYNAPS_DAEMON` not `0`): the TUI
/// (`--attach`/`--attach=` anywhere) or the line client (`attach` as the
/// first positional). `None` = the ordinary in-process boot, which must run
/// exactly like `synaps` does — no re-exec, no allocator diet, multi-thread
/// runtime (review H2).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ThinClient {
    Tui { profile: Option<String> },
    Line { profile: Option<String> },
}

fn thin_client() -> Option<ThinClient> {
    thin_client_from(std::env::args().skip(1), agent_engine::daemon::enabled())
}

fn thin_client_from<I: IntoIterator<Item = String>>(args: I, daemon_enabled: bool) -> Option<ThinClient> {
    if !daemon_enabled {
        return None;
    }
    let mut want_profile = false;
    let mut profile: Option<String> = None;
    let mut first_positional: Option<String> = None;
    let mut attach_flag = false;
    for a in args {
        if want_profile {
            want_profile = false;
            profile = Some(a);
            continue;
        }
        if a == "--attach" || a.starts_with("--attach=") {
            attach_flag = true;
        } else if a == "--profile" {
            want_profile = true;
        } else if let Some(p) = a.strip_prefix("--profile=") {
            profile = Some(p.to_string());
        } else if !a.starts_with('-') && first_positional.is_none() {
            first_positional = Some(a);
        }
    }
    if first_positional.as_deref() == Some("attach") {
        Some(ThinClient::Line { profile })
    } else if attach_flag && first_positional.is_none() {
        Some(ThinClient::Tui { profile })
    } else {
        None
    }
}

/// Auto-spawn (jcode model) before the runtime exists: a live daemon or a
/// freshly spawned one. Runs BEFORE the thin re-exec/diet so a failed spawn
/// can fall back to the untouched in-process boot. Returns the reason the
/// TUI should fall back in-process; the line client exits 3 instead.
fn ensure_daemon(kind: &ThinClient) -> Option<String> {
    use agent_engine::daemon::{self, EnsureError, Ensured, EXIT_REFUSED};
    let (profile, line) = match kind {
        ThinClient::Tui { profile } => (profile.clone(), false),
        ThinClient::Line { profile } => (profile.clone(), true),
    };
    let opts = daemon::DaemonOpts { profile, ..Default::default() };
    match daemon::ensure_running(&opts) {
        Ok(Ensured::Running) => None,
        Ok(Ensured::Spawned(pid)) => {
            eprintln!("starting daemon (pid {pid}) — synaps daemon stop to end it");
            None
        }
        Err(EnsureError::AutospawnDisabled) => {
            let msg = cmd::attach::no_daemon_message(opts.profile.as_deref());
            eprintln!("{msg}");
            std::process::exit(EXIT_REFUSED);
        }
        Err(EnsureError::Spawn(reason)) => {
            if line {
                eprintln!("synaps attach: daemon unavailable: {reason}");
                std::process::exit(EXIT_REFUSED);
            }
            Some(reason)
        }
    }
}

/// Our jemalloc boot conf for the re-exec'd thin client (`SYNAPS_CLIENT_MALLOC_CONF`
/// overrides). `narenas` is boot-only — the one knob a mallctl cannot set.
const THIN_MALLOC_CONF: &str = "narenas:1,background_thread:false,dirty_decay_ms:0,muzzy_decay_ms:0";

/// Set by `main` when the daemon could not be started: `--attach` runs the
/// ordinary TUI with a notice instead of the socket client.
const ATTACH_FALLBACK: &str = "SYNAPS_ATTACH_FALLBACK";

/// Env markers carried across the re-exec: the loop guard and the user's own
/// `_RJEM_MALLOC_CONF` (restored in the child so what we spawn sees it, M2).
const REEXECED: &str = "SYNAPS_CLIENT_REEXECED";
const USER_MALLOC_CONF: &str = "SYNAPS_CLIENT_USER_MALLOC_CONF";

/// Thin-client re-exec (PLAN-phase4 §4.5, extended): `PR_SET_THP_DISABLE`
/// is inherited across `execve`, so re-exec'ing ourselves once means the
/// **whole** image — `.bss`, the main stack, jemalloc's first chunks — is
/// mapped at 4 KiB granularity instead of the three 2 MiB huge pages the A1
/// ladder found already resident at `main` (6 of the 7.4 MB). The same exec
/// carries `_RJEM_MALLOC_CONF` so jemalloc boots with one arena and no
/// background thread. `SYNAPS_CLIENT_REEXEC=0` disables; cost ≈ 3–5 ms
/// (the `reexec` ladder stage is the pre-exec process' last line).
///
/// Only worth it when the kernel's THP mode is `[always]` (M1): under
/// `[madvise]`/`[never]` nothing is huge-mapped before `main`, and the only
/// thing the exec would buy is `narenas:1` — `tune_allocator` sets the rest
/// in-process via mallctl. `SYNAPS_CLIENT_THP=1` (keep huge pages) skips too.
#[cfg(target_os = "linux")]
fn thin_client_reexec() {
    use agent_core::core::memstat;
    if std::env::var("SYNAPS_CLIENT_REEXEC").is_ok_and(|v| v == "0")
        || std::env::var_os(REEXECED).is_some()
        || std::env::var("SYNAPS_CLIENT_THP").is_ok_and(|v| v == "1")
        || tui::client_diet::allocator_tuning_disabled()
        || memstat::thp_disabled() == Some(true)
    {
        return;
    }
    if memstat::thp_sysfs_always() != Some(true) {
        memstat::ladder("reexec", &"skipped=thp-not-always");
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    if memstat::disable_thp().is_err() {
        return;
    }
    memstat::ladder("reexec", &"");
    use std::os::unix::process::CommandExt;
    let ours = std::env::var("SYNAPS_CLIENT_MALLOC_CONF").unwrap_or_else(|_| THIN_MALLOC_CONF.into());
    let user = std::env::var("_RJEM_MALLOC_CONF").ok().filter(|v| !v.is_empty());
    // Later keys win in jemalloc's conf parser: the user's value stays visible, ours applies.
    let conf = match &user {
        Some(u) => format!("{u},{ours}"),
        None => ours,
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(std::env::args_os().skip(1)).env(REEXECED, "1").env("_RJEM_MALLOC_CONF", conf);
    if let Some(u) = user {
        cmd.env(USER_MALLOC_CONF, u);
    }
    let err = cmd.exec();
    // exec only returns on failure: carry on in this image.
    let _ = err;
    std::env::remove_var(REEXECED);
}

#[cfg(not(target_os = "linux"))]
fn thin_client_reexec() {}

/// After the (possible) re-exec: jemalloc has read `_RJEM_MALLOC_CONF` at
/// load, so put the environment back the way the user had it — only what
/// the exec added is removed; a conf the user exported themselves is left
/// (or restored) for every child the client spawns (M2).
fn scrub_reexec_env() {
    if std::env::var_os(REEXECED).is_none() {
        return;
    }
    std::env::remove_var(REEXECED);
    match std::env::var(USER_MALLOC_CONF) {
        Ok(user) => std::env::set_var("_RJEM_MALLOC_CONF", user),
        Err(_) => std::env::remove_var("_RJEM_MALLOC_CONF"),
    }
    std::env::remove_var(USER_MALLOC_CONF);
}

fn main() -> anyhow::Result<()> {
    let mut thin = false;
    if let Some(kind) = thin_client() {
        match ensure_daemon(&kind) {
            None => thin = true,
            Some(reason) => {
                eprintln!("daemon unavailable: {reason} — running in-process");
                tui::push_boot_notice(format!("daemon unavailable: {reason} — running in-process"));
                std::env::set_var(ATTACH_FALLBACK, "1");
            }
        }
    }
    if thin {
        thin_client_reexec();
        scrub_reexec_env();
        // Ladder START pins on the first call — `main` must be first (§7.1).
        agent_core::core::memstat::ladder("main", &"");
        tui::client_diet::tune_allocator();
    }
    let rt = if thin {
        tokio::runtime::Builder::new_current_thread().enable_all().thread_name("synaps-rt").build()?
    } else {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        if let Some(n) = worker_threads() {
            builder.worker_threads(n);
        }
        builder.enable_all().thread_name("synaps-rt").build()?
    };
    if thin {
        agent_core::core::memstat::ladder("runtime", &"");
    }
    // The log-appender guard lives on the process `EngineHost` (a static —
    // never dropped by Rust). Flush it on every exit path so the teardown
    // burst (session save, hooks, extension shutdown) reaches disk.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        synaps_cli::EngineHost::flush_installed_logs();
    }));
    let result = rt.block_on(async_main());
    synaps_cli::EngineHost::flush_installed_logs();
    result
}

async fn async_main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if matches!(cli.command, Some(Command::Prompt { .. })) {
        if let Some(Command::Prompt { action }) = cli.command {
            return cmd::prompt::run(action);
        }
        unreachable!();
    }
    if let Some(ref prof) = cli.profile {
        synaps_cli::config::set_profile(Some(prof.clone()));
    }

    match cli.command {
        None if cli.attach.is_some() => {
            let fallback = std::env::var_os(ATTACH_FALLBACK).is_some();
            std::env::remove_var(ATTACH_FALLBACK);
            if !agent_engine::daemon::enabled() || fallback {
                if !fallback {
                    eprintln!("--attach ignored: {}", agent_engine::daemon::DISABLED_NOTICE);
                }
                tui::run(
                    cli.continue_session,
                    cli.system,
                    cli.prompt_manifest,
                    cli.profile,
                    cli.no_extensions,
                )
                .await?;
            } else {
                let id = cli.attach.flatten();
                tui::attach::run_attached(tui::attach::AttachOpts {
                    profile: cli.profile,
                    id,
                    continue_session: cli.continue_session.flatten(),
                    system: cli.system,
                    prompt_manifest: cli.prompt_manifest,
                    mode: tui::attach::attach_mode(cli.observe, cli.takeover),
                    keep_warm: cli.keep_warm,
                    new_session: cli.new_session,
                })
                .await?;
            }
        }
        None => {
            tui::run(
                cli.continue_session,
                cli.system,
                cli.prompt_manifest,
                cli.profile,
                cli.no_extensions,
            )
            .await?;
        }
        Some(Command::Chat {
            continue_session,
            system,
            agent,
            profile,
            no_extensions,
        }) => {
            cmd::chat::run(continue_session, system, agent, profile, no_extensions).await?;
        }
        Some(Command::Server {
            port,
            host,
            system,
            continue_session,
            token,
            auto_approve_confirms,
            allowed_origins,
        }) => {
            cmd::server::run(
                port,
                host,
                system,
                continue_session,
                cli.profile,
                token,
                auto_approve_confirms,
                allowed_origins,
            )
            .await?;
        }
        Some(Command::Agent {
            config,
            trigger_context,
        }) => {
            cmd::agent::run(config, trigger_context).await;
        }
        Some(Command::Watcher { subcommand, args }) => {
            cmd::watcher::run(subcommand, args).await;
        }
        Some(Command::Login { provider }) => {
            cmd::login::run(cli.profile, provider)
                .await
                .map_err(anyhow::Error::msg)?;
        }
        Some(Command::Auth { action }) => match action {
            AuthAction::Login { provider } => cmd::login::run(cli.profile, provider)
                .await
                .map_err(anyhow::Error::msg)?,
        },
        Some(Command::Status { memory, json, pid }) => {
            if memory {
                cmd::status::run_memory(json, pid).map_err(|e| anyhow::anyhow!(e.to_string()))?;
            } else {
                cmd::status::run()
                    .await
                    .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            }
        }
        Some(Command::AuthBroker {
            bind,
            machine_token,
            machine_token_file,
            insecure_no_auth,
            tls_cert,
            tls_key,
            insecure_http,
        }) => {
            cmd::auth_broker::run(
                bind,
                machine_token,
                machine_token_file,
                insecure_no_auth,
                tls_cert,
                tls_key,
                insecure_http,
            )
            .await?;
        }
        Some(Command::Completions { shell }) => {
            use clap::CommandFactory;
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_string();
            clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
        }
        Some(Command::Prompt { .. }) => {
            unreachable!("prompt dispatched before profile initialization")
        }
        Some(Command::Tools { action }) => {
            cmd::tools::run(action).await?;
        }
        Some(Command::Trace { action }) => {
            cmd::trace::run(action)?;
        }
        Some(Command::Retention { action }) => {
            cmd::retention::run(action)?;
        }
        Some(Command::Daemon(args)) => {
            cmd::daemon::run(cli.profile, args).await?;
        }
        Some(Command::Attach(args)) => {
            cmd::attach::run(cli.profile, args).await?;
        }
        Some(Command::Rpc {
            continue_id,
            system,
            model,
            profile,
        }) => {
            cmd::rpc::run(continue_id, system, model, profile).await?;
        }
        Some(Command::Send {
            message,
            source,
            severity,
            channel,
            content_type,
            session,
            broadcast,
        }) => {
            cmd::send::run(
                message,
                source,
                severity,
                channel,
                content_type,
                session,
                broadcast,
            )
            .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod worker_threads_tests {
    use super::{thin_client_from, worker_threads_from};

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn thin_client_requires_daemon_flag() {
        use super::ThinClient::{Line, Tui};
        assert_eq!(thin_client_from(v(&["--attach"]), false), None);
        assert_eq!(thin_client_from(v(&["attach"]), false), None);
        assert_eq!(thin_client_from(v(&["--attach"]), true), Some(Tui { profile: None }));
        assert_eq!(thin_client_from(v(&["--attach=abc"]), true), Some(Tui { profile: None }));
        assert_eq!(thin_client_from(v(&["attach"]), true), Some(Line { profile: None }));
        assert_eq!(thin_client_from(v(&["--profile", "x", "attach"]), true), Some(Line { profile: Some("x".into()) }));
        assert_eq!(thin_client_from(v(&["--profile=x", "--attach", "--new"]), true), Some(Tui { profile: Some("x".into()) }));
        // bare `attach` is only the subcommand position, not a value elsewhere
        assert_eq!(thin_client_from(v(&["send", "attach"]), true), None);
        assert_eq!(thin_client_from(v(&["--profile", "attach"]), true), None);
        // `--attach` next to a subcommand is not the TUI attach path
        assert_eq!(thin_client_from(v(&["daemon", "status", "--attach"]), true), None);
        assert_eq!(thin_client_from(v(&[]), true), None);
    }

    #[test]
    fn attach_new_flag_combos() {
        use clap::Parser;
        let ok = super::Cli::try_parse_from(["synaps", "--attach", "--new"]).unwrap();
        assert!(ok.new_session && ok.attach.is_some());
        let alias = super::Cli::try_parse_from(["synaps", "--attach", "--create"]).unwrap();
        assert!(alias.new_session);
        // `--attach ID --new` parses; the attach path rejects the combination
        // (`choose_attach`: "cannot combine") so the message names both flags.
        let both = super::Cli::try_parse_from(["synaps", "--attach", "abc", "--new"]).unwrap();
        assert_eq!(both.attach.flatten().as_deref(), Some("abc"));
        assert!(both.new_session);
        // The line client's own --create/--new must not collide with the
        // top-level alias (clap's debug assert panics on duplicate longs).
        let line = super::Cli::try_parse_from(["synaps", "attach", "--create"]).unwrap();
        assert!(matches!(line.command, Some(super::Command::Attach(ref a)) if a.create));
        let line = super::Cli::try_parse_from(["synaps", "attach", "--new"]).unwrap();
        assert!(matches!(line.command, Some(super::Command::Attach(ref a)) if a.create));
        // --new without --attach is a clap error
        assert!(super::Cli::try_parse_from(["synaps", "--new"]).is_err());
    }

    #[test]
    fn worker_threads_env_matrix() {
        assert_eq!(worker_threads_from(None, 24), Some(4));
        assert_eq!(worker_threads_from(None, 2), Some(2));
        assert_eq!(worker_threads_from(None, 0), Some(1));
        assert_eq!(worker_threads_from(Some("0"), 24), None);
        assert_eq!(worker_threads_from(Some("8"), 24), Some(8));
        assert_eq!(worker_threads_from(Some(" 3 "), 24), Some(3));
        assert_eq!(worker_threads_from(Some("bogus"), 24), Some(4));
    }
}
