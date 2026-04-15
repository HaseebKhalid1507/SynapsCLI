//! synaps-agent — Headless autonomous agent worker
//!
//! Boots with a system prompt + handoff state, runs the agentic loop
//! until limits are hit, writes handoff, and exits cleanly.
//!
//! Usage: synaps-agent --config <path/to/config.toml>

use clap::Parser;
use synaps_cli::transport::AgentHarness;

#[derive(Parser)]
#[command(name = "synaps-agent", about = "Headless autonomous agent worker")]
struct Cli {
    /// Path to the agent config.toml
    #[arg(long)]
    config: String,

    /// Trigger context passed by supervisor
    #[arg(long, default_value = "manual start")]
    trigger_context: String,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match AgentHarness::from_config(&cli.config, Some(&cli.trigger_context)).await {
        Ok(mut harness) => {
            if let Err(e) = harness.run().await {
                eprintln!("Agent error: {}", e);
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("Failed to initialize agent: {}", e);
            std::process::exit(1);
        }
    }
}
