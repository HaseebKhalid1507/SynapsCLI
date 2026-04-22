use clap::{Parser, Subcommand};

mod chatui;
mod watcher;
mod cmd;

/// Global tmux session name for cleanup on any exit.
static TMUX_SESSION_NAME: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Kill the tmux session. Called from the Drop guard.
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

/// Guard that kills the tmux session on drop.
struct TmuxSessionGuard;

impl Drop for TmuxSessionGuard {
    fn drop(&mut self) {
        kill_tmux_session();
    }
}

/// Re-exec the current process inside a new tmux session.
/// This replaces the current process — does not return on success.
fn reexec_inside_tmux(session_name: &str, tmux_path: &str) -> ! {
    use std::os::unix::process::CommandExt;

    // Kill any stale session with the same name
    let _ = std::process::Command::new(tmux_path)
        .args(["kill-session", "-t", session_name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    // Reconstruct the full command line for the inner process.
    // We pass all original args unchanged — when re-exec'd inside tmux,
    // $TMUX will be set so we won't re-exec again.
    let args: Vec<String> = std::env::args().collect();
    let exe = &args[0];

    // tmux new-session -s <name> -- <exe> <args...>
    let mut cmd = std::process::Command::new(tmux_path);
    cmd.args(["new-session", "-s", session_name, "--"]);
    cmd.arg(exe);
    for arg in &args[1..] {
        cmd.arg(arg);
    }

    // exec replaces this process — never returns
    let err = cmd.exec();
    eprintln!("error: failed to exec into tmux: {}", err);
    std::process::exit(1);
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

    // ── tmux re-exec gate ──
    // If --tmux is passed and we're NOT already inside tmux,
    // re-exec the entire process inside a new tmux session.
    // On the second run, $TMUX is set so we skip this and proceed normally.
    if cli.tmux.is_some() && std::env::var("TMUX").is_err() {
        // Ensure tmux binary is available
        let tmux_path = match synaps_cli::tmux::find_tmux() {
            Some(p) => p,
            None => {
                if synaps_cli::tmux::install::prompt_install() {
                    if let Err(e) = synaps_cli::tmux::install::run_install().await {
                        eprintln!("tmux installation failed: {}", e);
                        std::process::exit(1);
                    }
                    synaps_cli::tmux::find_tmux().unwrap_or_else(|| {
                        eprintln!("error: tmux still not found after install");
                        std::process::exit(1);
                    })
                } else {
                    eprintln!("error: tmux is required for --tmux mode but was not found");
                    std::process::exit(1);
                }
            }
        };

        let session_name = match &cli.tmux {
            Some(Some(name)) => name.clone(),
            _ => synaps_cli::tmux::auto_session_name(),
        };

        // This never returns — replaces the process with tmux
        reexec_inside_tmux(&session_name, &tmux_path);
    }

    match cli.command {
        None => {
            // If --tmux was passed and $TMUX is set, we're inside tmux (re-exec'd).
            // Create the controller for tool access.
            let _tmux_guard: Option<TmuxSessionGuard>;

            let tmux_controller = if cli.tmux.is_some() {
                let session_name = match &cli.tmux {
                    Some(Some(name)) => name.clone(),
                    _ => synaps_cli::tmux::auto_session_name(),
                };

                let mut ctrl = synaps_cli::tmux::TmuxController::new(session_name.clone());
                if let Err(e) = ctrl.start().await {
                    eprintln!("error: tmux controller failed: {}", e);
                    std::process::exit(1);
                }

                // Apply tmux session settings: mouse, hotkeys, status bar
                ctrl.apply_session_defaults().await;

                // Arm cleanup guard
                *TMUX_SESSION_NAME.lock().unwrap() = Some(session_name);
                _tmux_guard = Some(TmuxSessionGuard);

                Some(std::sync::Arc::new(ctrl))
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
