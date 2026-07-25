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
#[cfg(not(target_env = "musl"))]
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
#[cfg(not(target_env = "musl"))]
#[allow(non_upper_case_globals)]
#[export_name = "_rjem_malloc_conf"]
pub static MALLOC_CONF: &[u8] = b"background_thread:true,narenas:4,dirty_decay_ms:1000,muzzy_decay_ms:0\0";

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
    Status,
    /// Credential broker — serve short-lived access tokens to client machines
    /// over HTTP/HTTPS so they can share one OAuth credential without storing it.
    ///
    /// For non-loopback binds, pass --tls-cert + --tls-key to enable HTTPS (recommended),
    /// or --insecure-http to acknowledge you are running behind WireGuard / a private network.
    AuthBroker {
        /// Address to bind, e.g. `0.0.0.0:8181` or `127.0.0.1:8181`.
        #[arg(long, default_value = "127.0.0.1:8181")]
        bind: String,
        /// Token clients must present (`Authorization: Bearer <token>`). Falls
        /// back to `--machine-token-file`, then `SYNAPS_BROKER_TOKEN`.
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
        Some(Command::Status) => {
            cmd::status::run()
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
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
