use synaps_cli::protocol::{ClientMessage, ServerMessage, HistoryEntry};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio::io::{AsyncBufReadExt, BufReader};


#[derive(Parser)]
#[command(name = "client", about = "SynapsCLI terminal client")]
struct Cli {
    /// Server URL
    #[arg(long, short, default_value = "ws://localhost:3145/ws")]
    url: String,

    #[arg(long, global = true)]
    profile: Option<String>,
}

#[allow(unused_assignments)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if let Some(ref prof) = cli.profile {
        synaps_cli::config::set_profile(Some(prof.clone()));
    }
    
    let _log_guard = synaps_cli::logging::init_logging();

    eprintln!("Connecting to {}...", cli.url);

    let (ws_stream, _) = connect_async(&cli.url).await?;
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    eprintln!("Connected. Type a message and press Enter. Commands start with /\n");

    // Request history on connect
    let history_msg = serde_json::to_string(&ClientMessage::History)?;
    ws_tx.send(Message::Text(history_msg)).await?;

    // Request status
    let status_msg = serde_json::to_string(&ClientMessage::Status)?;
    ws_tx.send(Message::Text(status_msg)).await?;

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();

    // Track if we're in a streaming response (to know when to show prompt)
    let mut streaming = false;
    let mut needs_newline = false;

    loop {
        tokio::select! {
            // Read from stdin
            line = reader.next_line() => {
                match line {
                    Ok(Some(input)) => {
                        let input = input.trim().to_string();
                        if input.is_empty() {
                            continue;
                        }

                        let msg = if let Some(rest) = input.strip_prefix('/') {
                            let parts: Vec<&str> = rest.splitn(2, ' ').collect();
                            let cmd = parts[0];
                            let args = parts.get(1).map(|s| s.trim()).unwrap_or("");

                            match cmd {
                                "quit" | "exit" | "q" => {
                                    eprintln!("\nbye.");
                                    return Ok(());
                                }
                                "cancel" | "c" => {
                                    ClientMessage::Cancel
                                }
                                "status" | "s" => {
                                    ClientMessage::Status
                                }
                                "history" | "h" => {
                                    ClientMessage::History
                                }
                                _ => {
                                    ClientMessage::Command {
                                        name: cmd.to_string(),
                                        args: args.to_string(),
                                    }
                                }
                            }
                        } else {
                            ClientMessage::Message { content: input }
                        };

                        let json = serde_json::to_string(&msg)?;
                        ws_tx.send(Message::Text(json)).await?;
                    }
                    Ok(None) => break, // EOF
                    Err(e) => {
                        eprintln!("stdin error: {}", e);
                        break;
                    }
                }
            }

            // Read from WebSocket
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(server_msg) = serde_json::from_str::<ServerMessage>(&text) {
                            match server_msg {
                                ServerMessage::Thinking { content } => {
                                    if !streaming {
                                        streaming = true;
                                        eprint!("\x1b[2m"); // dim
                                    }
                                    eprint!("{}", content);
                                    needs_newline = true;
                                }
                                ServerMessage::Text { content } => {
                                    // End thinking dim if transitioning to text
                                    if streaming {
                                        eprint!("\x1b[0m"); // reset
                                        if needs_newline {
                                            eprintln!();
                                        }
                                        needs_newline = false;
                                        streaming = false;
                                        eprint!("\n\x1b[38;2;80;200;160m◈ agent\x1b[0m\n");
                                    }
                                    print!("{}", content);
                                    needs_newline = !content.ends_with('\n');
                                }
                                ServerMessage::ToolUseStart { tool_name } => {
                                    if needs_newline {
                                        println!();
                                        needs_newline = false;
                                    }
                                    let icon = match tool_name.as_str() {
                                        "bash"  => "❯",
                                        "read"  => "▸",
                                        "write" => "◂",
                                        "edit"  => "Δ",
                                        "grep"  => "⌕",
                                        "find"  => "○",
                                        "ls"    => "≡",
                                        _       => "→",
                                    };
                                    eprint!("\x1b[38;2;100;180;220m  {} {}\x1b[0m ", icon, tool_name);
                                    synaps_cli::flush_stderr();
                                }
                                ServerMessage::ToolUseDelta(delta) => {
                                    eprint!("\x1b[38;2;80;110;140m{}\x1b[0m", delta);
                                    synaps_cli::flush_stderr();
                                }
                                ServerMessage::ToolUse { tool_name, input, .. } => {
                                    eprint!("                                                                                          \r");
                                    if needs_newline {
                                        println!();
                                        needs_newline = false;
                                    }
                                    let icon = match tool_name.as_str() {
                                        "bash"  => "❯",
                                        "read"  => "▸",
                                        "write" => "◂",
                                        "edit"  => "Δ",
                                        "grep"  => "⌕",
                                        "find"  => "○",
                                        "ls"    => "≡",
                                        _       => "→",
                                    };
                                    eprintln!("\x1b[38;2;100;180;220m  {} {}\x1b[0m", icon, tool_name);
                                    // Show compact params
                                    if let Some(obj) = input.as_object() {
                                        for (k, v) in obj {
                                            let val = match v.as_str() {
                                                Some(s) if s.len() > 100 => {
                                                    let end = s.char_indices().nth(100).map(|(i, _)| i).unwrap_or(s.len());
                                                    format!("{}…", &s[..end])
                                                }
                                                Some(s) => s.to_string(),
                                                None => v.to_string(),
                                            };
                                            eprintln!("\x1b[38;2;80;110;140m    {}: {}\x1b[0m", k, val);
                                        }
                                    }
                                }
                                ServerMessage::ToolResultDelta { delta, .. } => {
                                    eprint!("\x1b[38;2;140;180;150m{}\x1b[0m", delta);
                                }
                                ServerMessage::ToolResult { result, .. } => {
                                    let lines: Vec<&str> = result.lines().collect();
                                    let is_error = result.starts_with("Tool execution failed");
                                    if is_error {
                                        eprintln!("\x1b[38;2;220;80;80m    ✗ {}\x1b[0m", result.lines().next().unwrap_or(""));
                                    } else {
                                        eprintln!("\x1b[38;2;60;160;110m    └─ ok ({} lines)\x1b[0m", lines.len());
                                        for line in lines.iter().take(8) {
                                            eprintln!("\x1b[38;2;65;130;100m      {}\x1b[0m", line);
                                        }
                                        if lines.len() > 8 {
                                            eprintln!("\x1b[38;2;55;62;75m      +{} more\x1b[0m", lines.len() - 8);
                                        }
                                    }
                                }
                                ServerMessage::Usage { input_tokens, output_tokens } => {
                                    // Subtle token display
                                    let _ = (input_tokens, output_tokens); // tracked server-side
                                }
                                ServerMessage::Done => {
                                    if needs_newline {
                                        println!();
                                        needs_newline = false;
                                    }
                                    streaming = false;
                                    eprintln!();
                                }
                                ServerMessage::Error { message } => {
                                    if needs_newline { eprintln!(); needs_newline = false; }
                                    streaming = false;
                                    eprintln!("\x1b[38;2;220;80;80m✗ {}\x1b[0m", message);
                                }
                                ServerMessage::System { message } => {
                                    eprintln!("\x1b[2m  {}\x1b[0m", message);
                                }
                                ServerMessage::StatusResponse {
                                    model, thinking, streaming: is_streaming,
                                    session_id, total_input_tokens, total_output_tokens,
                                    session_cost, connected_clients,
                                } => {
                                    eprintln!("\x1b[38;2;80;200;160m┌─ Status ─────────────────────┐\x1b[0m");
                                    eprintln!("\x1b[38;2;80;200;160m│\x1b[0m Model:    {}", model);
                                    eprintln!("\x1b[38;2;80;200;160m│\x1b[0m Thinking: {}", thinking);
                                    eprintln!("\x1b[38;2;80;200;160m│\x1b[0m Session:  {}", session_id);
                                    eprintln!("\x1b[38;2;80;200;160m│\x1b[0m Tokens:   {}in / {}out", total_input_tokens, total_output_tokens);
                                    eprintln!("\x1b[38;2;80;200;160m│\x1b[0m Cost:     ${:.4}", session_cost);
                                    eprintln!("\x1b[38;2;80;200;160m│\x1b[0m Clients:  {}", connected_clients);
                                    eprintln!("\x1b[38;2;80;200;160m│\x1b[0m Streaming: {}", if is_streaming { "yes" } else { "no" });
                                    eprintln!("\x1b[38;2;80;200;160m└─────────────────────────────-┘\x1b[0m");
                                }
                                ServerMessage::HistoryResponse { messages } => {
                                    if messages.is_empty() {
                                        eprintln!("\x1b[2m  (empty session)\x1b[0m");
                                    } else {
                                        eprintln!("\x1b[2m  ── history ({} entries) ──\x1b[0m", messages.len());
                                        for entry in &messages {
                                            match entry {
                                                HistoryEntry::User { content, .. } => {
                                                    let preview: String = content.chars().take(80).collect();
                                                    eprintln!("\x1b[38;2;190;200;220m  ❯ {}\x1b[0m", preview);
                                                }
                                                HistoryEntry::Text { content, .. } => {
                                                    let preview: String = content.chars().take(120).collect();
                                                    eprintln!("\x1b[38;2;195;200;210m  ◈ {}\x1b[0m", preview);
                                                }
                                                HistoryEntry::ToolUse { tool_name, .. } => {
                                                    eprintln!("\x1b[38;2;100;180;220m    → {}\x1b[0m", tool_name);
                                                }
                                                _ => {}
                                            }
                                        }
                                        eprintln!("\x1b[2m  ── end history ──\x1b[0m\n");
                                    }
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        eprintln!("\nServer disconnected.");
                        break;
                    }
                    Some(Err(e)) => {
                        eprintln!("\nWebSocket error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}
