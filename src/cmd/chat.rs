use synaps_cli::{Runtime, StreamEvent, LlmEvent, SessionEvent, AgentEvent, Result, CancellationToken, flush_stdout};
use futures::StreamExt;
use serde_json::{json, Value};
use std::io;

pub async fn run() -> Result<()> {
    let _log_guard = synaps_cli::logging::init_logging();
    println!("💬 Terminal Chat with Thinking Blocks - type 'quit' to exit\n");
    let runtime = Runtime::new().await?;
    let mut messages: Vec<Value> = Vec::new();

    loop {
        print!("You: ");
        flush_stdout();

        let mut input = String::new();
        match io::stdin().read_line(&mut input) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let input = input.trim();

        if input.is_empty() {
            continue;
        }

        if input == "quit" || input == "exit" {
            println!("Goodbye! 👋");
            break;
        }

        messages.push(json!({"role": "user", "content": input}));

        print!("Claude: ");
        flush_stdout();

        let cancel = CancellationToken::new();
        let mut stream = runtime.run_stream_with_messages(messages.clone(), cancel, None).await;
        let mut in_thinking = false;

        while let Some(event) = stream.next().await {
            match event {
                StreamEvent::Llm(LlmEvent::Thinking(text)) => {
                    if !in_thinking {
                        print!("\n🤔 ");
                        flush_stdout();
                        in_thinking = true;
                    }
                    print!("{}", text);
                    flush_stdout();
                }
                StreamEvent::Llm(LlmEvent::Text(text)) => {
                    if in_thinking {
                        print!("\n\n💬 ");
                        flush_stdout();
                        in_thinking = false;
                    }
                    print!("{}", text);
                    flush_stdout();
                }
                StreamEvent::Llm(LlmEvent::ToolUseStart(tool_name)) => {
                    if in_thinking { println!(); in_thinking = false; }
                    print!("⚙️  Using tool: {} (args: ", tool_name);
                    flush_stdout();
                }
                StreamEvent::Llm(LlmEvent::ToolUseDelta(delta)) => {
                    print!("{}", delta);
                    flush_stdout();
                }
                StreamEvent::Llm(LlmEvent::ToolUse { tool_name, tool_id, input: tool_input }) => {
                    print!(")                                                                                          \r");
                    if in_thinking { println!(); in_thinking = false; }
                    println!("⚙️  Using tool: {} ({})", tool_name, tool_id);
                    println!("📝 Input: {}", serde_json::to_string_pretty(&tool_input).unwrap_or_default());
                    flush_stdout();
                }
                StreamEvent::Llm(LlmEvent::ToolResultDelta { delta, .. }) => {
                    print!("\x1b[38;2;140;180;150m{}\x1b[0m", delta);
                    flush_stdout();
                }
                StreamEvent::Llm(LlmEvent::ToolResult { tool_id, result }) => {
                    println!("✅ Tool result ({}): {}", tool_id, result);
                    print!("💬 ");
                    flush_stdout();
                }
                StreamEvent::Session(SessionEvent::MessageHistory(history)) => { messages = history; }
                StreamEvent::Session(SessionEvent::Usage { .. }) => {}
                StreamEvent::Agent(AgentEvent::SubagentStart { agent_name, task_preview, .. }) => {
                    println!("\n\x1b[35m🎭 [{}] dispatched: {}\x1b[0m", agent_name, task_preview);
                    flush_stdout();
                }
                StreamEvent::Agent(AgentEvent::SubagentUpdate { agent_name, status, .. }) => {
                    print!("\x1b[90m  [{}] {}\x1b[0m\r", agent_name, status);
                    flush_stdout();
                }
                StreamEvent::Agent(AgentEvent::SubagentDone { agent_name, duration_secs, .. }) => {
                    println!("\x1b[32m✔ [{}] done ({:.1}s)\x1b[0m", agent_name, duration_secs);
                    flush_stdout();
                }
                StreamEvent::Agent(AgentEvent::SteeringDelivered { message }) => {
                    println!("\n\x1b[33m→ [steering] {}\x1b[0m", message);
                    flush_stdout();
                }
                StreamEvent::Session(SessionEvent::Done) => {
                    if in_thinking { println!(); }
                    println!("\n");
                    break;
                }
                StreamEvent::Session(SessionEvent::Error(err)) => {
                    if in_thinking { println!(); }
                    println!("❌ Error: {}\n", err);
                    messages.pop();
                    break;
                }
            }
        }
    }
    Ok(())
}
