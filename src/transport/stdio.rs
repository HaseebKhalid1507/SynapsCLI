use tokio::sync::mpsc;
use super::{Transport, Inbound, AgentEvent, SyncState};
use super::events::{ToolEvent, MetaEvent};

pub struct StdioTransport {
    rx: mpsc::UnboundedReceiver<String>,
}

impl Default for StdioTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl StdioTransport {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();

        std::thread::spawn(move || {
            use std::io::BufRead;
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                match line {
                    Ok(line) if line.is_empty() => continue,
                    Ok(line) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Self { rx }
    }
}

#[async_trait::async_trait]
impl Transport for StdioTransport {
    async fn recv(&mut self) -> Option<Inbound> {
        let line = self.rx.recv().await?;

        if line.starts_with('/') && line.len() > 1 {
            let parts: Vec<&str> = line[1..].splitn(2, ' ').collect();
            let cmd = parts[0].to_string();
            let args = parts.get(1).map(|s| s.trim()).unwrap_or("").to_string();
            Some(Inbound::Command { name: cmd, args })
        } else {
            Some(Inbound::Message { content: line })
        }
    }

    async fn send(&mut self, event: AgentEvent) -> bool {
        match event {
            AgentEvent::Thinking(t) => {
                eprint!("\x1b[2m{}\x1b[0m", t);
            }
            AgentEvent::Text(t) => {
                print!("{}", t);
            }
            AgentEvent::Tool(ToolEvent::Invoke { tool_name, .. }) => {
                eprintln!("\x1b[33m⚡ {}\x1b[0m", tool_name);
            }
            AgentEvent::Tool(ToolEvent::Complete { result, elapsed_ms, .. }) => {
                if let Some(ms) = elapsed_ms {
                    eprintln!("\x1b[2m({:.1}s)\x1b[0m", ms as f64 / 1000.0);
                }
                let preview: String = result.lines().take(20).collect::<Vec<_>>().join("\n");
                if result.lines().count() > 20 {
                    eprintln!("{}\n\x1b[2m... ({} more lines)\x1b[0m", preview, result.lines().count() - 20);
                } else if !result.is_empty() {
                    eprintln!("{}", preview);
                }
            }
            AgentEvent::TurnComplete => {
                println!();
                crate::flush_stdout();
            }
            AgentEvent::Error(e) => {
                eprintln!("\x1b[31mError: {}\x1b[0m", e);
            }
            AgentEvent::Meta(MetaEvent::Shutdown { reason }) => {
                eprintln!("Shutting down: {}", reason);
                return false;
            }
            _ => {}
        }
        crate::flush_stdout();
        true
    }

    async fn on_sync(&mut self, state: SyncState) {
        eprintln!("\x1b[2mSession: {} | Model: {} | Turns: {}\x1b[0m",
            state.session_id, state.model, state.turn_count);
    }

    fn name(&self) -> &str { "stdio" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stdio_transport_construction() {
        // Just verify it can be created without panicking.
        // The stdin thread will exit when the transport is dropped.
        let _transport = StdioTransport::new();
    }
}
