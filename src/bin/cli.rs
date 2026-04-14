use clap::{Parser, Subcommand};
use synaps_cli::{Runtime, Result, flush_stdout};
use std::io;

#[derive(Parser)]
struct Cli {
    #[arg(long, global = true)]
    profile: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Run {
        prompt: String,
        /// Load agent system prompt by name (from ~/.synaps-cli/agents/<name>.md) or file path
        #[arg(long, short)]
        agent: Option<String>,
        /// Load system prompt from a file path
        #[arg(long, short)]
        system: Option<String>,
    },
    Chat,
}

fn load_agent_prompt(name: &str) -> std::result::Result<String, String> {
    // Reuse the same resolution logic as the subagent tool
    synaps_cli::tools::resolve_agent_prompt(name)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(ref prof) = cli.profile {
        synaps_cli::config::set_profile(Some(prof.clone()));
    }

    let _log_guard = synaps_cli::logging::init_logging();
    let mut runtime = Runtime::new().await?;
    
    match cli.command {
        Commands::Run { prompt, agent, system } => {
            // Load system prompt: --agent takes priority over --system
            if let Some(ref agent_name) = agent {
                match load_agent_prompt(agent_name) {
                    Ok(prompt) => {
                        eprintln!("🎭 Agent: {}", agent_name);
                        runtime.set_system_prompt(prompt);
                    }
                    Err(e) => {
                        eprintln!("❌ {}", e);
                        std::process::exit(1);
                    }
                }
            } else if let Some(ref path) = system {
                match std::fs::read_to_string(path) {
                    Ok(content) => {
                        eprintln!("📋 System prompt: {}", path);
                        runtime.set_system_prompt(content);
                    }
                    Err(e) => {
                        eprintln!("❌ Failed to read {}: {}", path, e);
                        std::process::exit(1);
                    }
                }
            }

            println!("🤖 Calling Claude...");
            let response = runtime.run_single(&prompt).await?;
            println!("{}", response);
        }
        Commands::Chat => {
            println!("💬 Chat mode - type 'quit' to exit\n");
            
            loop {
                print!("You: ");
                flush_stdout();
                
                let mut input = String::new();
                if io::stdin().read_line(&mut input).is_err() {
                    eprintln!("stdin closed");
                    break;
                }
                let input = input.trim();
                
                if input.is_empty() {
                    continue;
                }
                
                if input == "quit" || input == "exit" {
                    println!("Goodbye! 👋");
                    break;
                }
                
                print!("Claude: ");
                flush_stdout();
                
                match runtime.run_single(input).await {
                    Ok(response) => println!("{}\n", response),
                    Err(e) => println!("Error: {}\n", e),
                }
            }
        }
    }
    Ok(())
}
