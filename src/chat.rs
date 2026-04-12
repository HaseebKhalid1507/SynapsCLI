use synaps_cli::{Runtime, StreamEvent, Result, CancellationToken};
use futures::StreamExt;
use serde_json::{json, Value};
use std::io::{self, Write};
use tokio;

#[tokio::main]
async fn main() -> Result<()> {
    let _log_guard = synaps_cli::logging::init_logging();
    println!("💬 Terminal Chat with Thinking Blocks - type 'quit' to exit\n");
    let runtime = Runtime::new().await?;
    let mut messages: Vec<Value> = Vec::new();

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

        // Add user message to history
        messages.push(json!({"role": "user", "content": input}));

        print!("Claude: ");
        io::stdout().flush().unwrap();

        let cancel = CancellationToken::new();
        let mut stream = runtime.run_stream_with_messages(messages.clone(), cancel).await;
        let mut in_thinking = false;

        while let Some(event) = stream.next().await {
            match event {
                StreamEvent::Thinking(text) => {
                    if !in_thinking {
                        print!("\n🤔 ");
                        io::stdout().flush().unwrap();
                        in_thinking = true;
                    }
                    print!("{}", text);
                    io::stdout().flush().unwrap();
                }
                StreamEvent::Text(text) => {
                    if in_thinking {
                        print!("\n\n💬 ");
                        io::stdout().flush().unwrap();
                        in_thinking = false;
                    }
                    print!("{}", text);
                    io::stdout().flush().unwrap();
                }
                StreamEvent::ToolUseStart(tool_name) => {
                    if in_thinking {
                        print!("\n");
                        in_thinking = false;
                    }
                    print!("⚙️  Using tool: {} (args: ", tool_name);
                    io::stdout().flush().unwrap();
                }
                StreamEvent::ToolUseDelta(delta) => {
                    print!("{}", delta);
                    io::stdout().flush().unwrap();
                }
                StreamEvent::ToolUse { tool_name, tool_id, input: tool_input } => {
                    print!(")                                                                                          \r");
                    if in_thinking {
                        print!("\n");
                        in_thinking = false;
                    }
                    println!("⚙️  Using tool: {} ({})", tool_name, tool_id);
                    println!("📝 Input: {}", serde_json::to_string_pretty(&tool_input).unwrap_or_default());
                    io::stdout().flush().unwrap();
                }
                StreamEvent::ToolResultDelta { delta, .. } => {
                    print!("\x1b[38;2;140;180;150m{}\x1b[0m", delta);
                    io::stdout().flush().unwrap();
                }
                StreamEvent::ToolResult { tool_id, result } => {
                    println!("✅ Tool result ({}): {}", tool_id, result);
                    print!("💬 ");
                    io::stdout().flush().unwrap();
                }
                StreamEvent::MessageHistory(history) => {
                    messages = history;
                }
                StreamEvent::Usage { .. } => {}
                StreamEvent::Done => {
                    if in_thinking {
                        print!("\n");
                    }
                    println!("\n");
                    break;
                }
                StreamEvent::Error(err) => {
                    if in_thinking {
                        print!("\n");
                    }
                    println!("❌ Error: {}\n", err);
                    // Remove the user message that caused the error
                    messages.pop();
                    break;
                }
            }
        }
    }
    Ok(())
}
