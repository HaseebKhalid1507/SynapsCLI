use clap::{Parser, Subcommand};
use synaps_cli::{Runtime, Result};
use std::io::{self, Write};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Run { prompt: String },
    Chat,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let runtime = Runtime::new().await?;
    
    match cli.command {
        Commands::Run { prompt } => {
            println!("🤖 Calling Claude...");
            let response = runtime.run_single(&prompt).await?;
            println!("{}", response);
        }
        Commands::Chat => {
            println!("💬 Chat mode - type 'quit' to exit\n");
            
            loop {
                print!("You: ");
                io::stdout().flush().unwrap();
                
                let mut input = String::new();
                io::stdin().read_line(&mut input).unwrap();
                let input = input.trim();
                
                if input.is_empty() {
                    continue;
                }
                
                if input == "quit" || input == "exit" {
                    println!("Goodbye! 👋");
                    break;
                }
                
                print!("Claude: ");
                io::stdout().flush().unwrap();
                
                match runtime.run_single(input).await {
                    Ok(response) => println!("{}\n", response),
                    Err(e) => println!("Error: {}\n", e),
                }
            }
        }
    }
    Ok(())
}
