use clap::{Parser, Subcommand};

mod chatui;
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

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// One-shot prompt execution
    Run {
        prompt: String,
        #[arg(long, short)]
        agent: Option<String>,
        #[arg(long, short = 'S')]
        system: Option<String>,
    },
    /// Plain text streaming chat
    Chat,
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
    /// WebSocket client
    Client {
        #[arg(long, default_value = "ws://127.0.0.1:3145")]
        url: String,
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
            chatui::run(cli.continue_session, cli.system, cli.profile).await?;
        }
        Some(Command::Run { prompt, agent, system }) => {
            cmd::run::run(prompt, agent, system).await?;
        }
        Some(Command::Chat) => {
            cmd::chat::run().await?;
        }
        Some(Command::Server { port, host, system, continue_session }) => {
            cmd::server::run(port, host, system, continue_session, cli.profile).await?;
        }
        Some(Command::Client { url }) => {
            cmd::client::run(url).await?;
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
        Some(Command::Send { message, source, severity, channel, content_type }) => {
            cmd::send::run(message, source, severity, channel, content_type).await?;
        }
    }
    Ok(())
}
