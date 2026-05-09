use clap::{Parser, Subcommand};

mod tui;
mod watcher;
mod cmd;

#[derive(Parser)]
#[command(name = "synaps", about = "Neural interface for Claude", version)]
struct Cli {
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
    Login,
    /// Show account usage and reset times
    Status,
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
        Some(Command::Server { port, host, system, continue_session }) => {
            cmd::server::run(port, host, system, continue_session, cli.profile).await?;
        }
        Some(Command::Agent { config, trigger_context }) => {
            cmd::agent::run(config, trigger_context).await;
        }
        Some(Command::Watcher { subcommand, args }) => {
            cmd::watcher::run(subcommand, args).await;
        }
        Some(Command::Login) => {
            cmd::login::run(cli.profile).await;
        }
        Some(Command::Status) => {
            cmd::status::run().await.map_err(|e| anyhow::anyhow!(e.to_string()))?;
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
