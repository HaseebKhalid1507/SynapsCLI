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
    clippy::items_after_test_module,
)]

use clap::{Parser, Subcommand};

use synaps_cli::tui;
mod watcher;
mod cmd;

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
    if let Some(ref prof) = cli.profile {
        synaps_cli::config::set_profile(Some(prof.clone()));
    }

    match cli.command {
        None => {
            tui::run(cli.continue_session, cli.system, cli.profile, cli.no_extensions).await?;
        }
        Some(Command::Chat { continue_session, system, agent, profile, no_extensions }) => {
            cmd::chat::run(continue_session, system, agent, profile, no_extensions).await?;
        }
        Some(Command::Server { port, host, system, continue_session, token, auto_approve_confirms, allowed_origins }) => {
            cmd::server::run(port, host, system, continue_session, cli.profile, token, auto_approve_confirms, allowed_origins).await?;
        }
        Some(Command::Agent { config, trigger_context }) => {
            cmd::agent::run(config, trigger_context).await;
        }
        Some(Command::Watcher { subcommand, args }) => {
            cmd::watcher::run(subcommand, args).await;
        }
        Some(Command::Login { provider }) => {
            cmd::login::run(cli.profile, provider).await;
        }
        Some(Command::Auth { action }) => match action {
            AuthAction::Login { provider } => cmd::login::run(cli.profile, provider).await,
        },
        Some(Command::Status) => {
            cmd::status::run().await.map_err(|e| anyhow::anyhow!(e.to_string()))?;
        }
        Some(Command::AuthBroker { bind, machine_token, machine_token_file, insecure_no_auth, tls_cert, tls_key, insecure_http }) => {
            cmd::auth_broker::run(bind, machine_token, machine_token_file, insecure_no_auth, tls_cert, tls_key, insecure_http).await?;
        }
        Some(Command::Completions { shell }) => {
            use clap::CommandFactory;
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_string();
            clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
        }
        Some(Command::Rpc { continue_id, system, model, profile }) => {
            cmd::rpc::run(continue_id, system, model, profile).await?;
        }
        Some(Command::Send { message, source, severity, channel, content_type, session, broadcast }) => {
            cmd::send::run(message, source, severity, channel, content_type, session, broadcast).await?;
        }
    }
    Ok(())
}
