use synaps_cli::{Runtime, Result};

fn load_agent_prompt(name: &str) -> std::result::Result<String, String> {
    synaps_cli::tools::resolve_agent_prompt(name)
}

pub async fn run(prompt: String, agent: Option<String>, system: Option<String>) -> Result<()> {
    let _log_guard = synaps_cli::logging::init_logging();
    let mut runtime = Runtime::new().await?;

    if let Some(ref agent_name) = agent {
        match load_agent_prompt(agent_name) {
            Ok(p) => {
                eprintln!("🎭 Agent: {}", agent_name);
                runtime.set_system_prompt(p);
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
    Ok(())
}
