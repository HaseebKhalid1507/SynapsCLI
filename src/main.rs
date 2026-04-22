use clap::{Parser, Subcommand};

mod chatui;
mod watcher;
mod cmd;

/// Global tmux session name for cleanup on any exit.
/// Written once during startup, read by the atexit handler.
static TMUX_SESSION_NAME: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Kill the tmux session. Called from atexit and Drop guard.
/// Uses synchronous std::process::Command — no async runtime needed.
fn kill_tmux_session() {
    if let Ok(mut guard) = TMUX_SESSION_NAME.lock() {
        if let Some(name) = guard.take() {
            let _ = std::process::Command::new("tmux")
                .args(["kill-session", "-t", &name])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

/// Guard that kills the tmux session on drop (covers normal exit paths).
struct TmuxSessionGuard;

impl Drop for TmuxSessionGuard {
    fn drop(&mut self) {
        kill_tmux_session();
    }
}

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

    /// Launch in tmux mode with optional session name.
    #[arg(long = "tmux", value_name = "SESSION_NAME")]
    tmux: Option<Option<String>>,

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
            // Resolve tmux controller if --tmux flag was passed
            // _tmux_guard lives on the stack — Drop fires on normal exit
            let _tmux_guard: Option<TmuxSessionGuard>;

            let tmux_controller = if cli.tmux.is_some() {
                // Ensure tmux binary is available
                if synaps_cli::tmux::find_tmux().is_none() {
                    if synaps_cli::tmux::install::prompt_install() {
                        if let Err(e) = synaps_cli::tmux::install::run_install().await {
                            eprintln!("tmux installation failed: {}", e);
                            std::process::exit(1);
                        }
                    } else {
                        eprintln!("error: tmux is required for --tmux mode but was not found");
                        std::process::exit(1);
                    }
                }

                // Resolve session name: explicit name or auto-generated
                let session_name = match cli.tmux {
                    Some(Some(name)) => name,
                    _ => synaps_cli::tmux::auto_session_name(),
                };

                let mut tmux_ctrl = synaps_cli::tmux::TmuxController::new(session_name.clone());
                if let Err(e) = tmux_ctrl.start().await {
                    eprintln!("error: failed to start tmux session: {}", e);
                    std::process::exit(1);
                }

                // Arm cleanup: store session name globally + arm drop guard.
                // The global covers SIGTERM/SIGINT via the atexit handler.
                // The drop guard covers normal function return.
                *TMUX_SESSION_NAME.lock().unwrap() = Some(session_name);
                _tmux_guard = Some(TmuxSessionGuard);

                Some(std::sync::Arc::new(tmux_ctrl))
            } else {
                _tmux_guard = None;
                None
            };

            chatui::run(cli.continue_session, cli.system, cli.profile, tmux_controller).await?;
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
