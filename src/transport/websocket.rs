use tokio::sync::mpsc;
use super::{Transport, Inbound, AgentEvent, SyncState};
use super::events::{ToolEvent, MetaEvent};
use crate::protocol::{ClientMessage, ServerMessage};

pub struct WebSocketTransport {
    ws_rx: mpsc::UnboundedReceiver<String>,
    ws_tx: mpsc::UnboundedSender<String>,
}

impl WebSocketTransport {
    pub fn new(
        ws_rx: mpsc::UnboundedReceiver<String>,
        ws_tx: mpsc::UnboundedSender<String>,
    ) -> Self {
        Self { ws_rx, ws_tx }
    }

    fn send_msg(&self, msg: ServerMessage) -> bool {
        match serde_json::to_string(&msg) {
            Ok(json) => self.ws_tx.send(json).is_ok(),
            Err(_) => false,
        }
    }
}

#[async_trait::async_trait]
impl Transport for WebSocketTransport {
    async fn recv(&mut self) -> Option<Inbound> {
        loop {
            let text = self.ws_rx.recv().await?;
            let Ok(msg) = serde_json::from_str::<ClientMessage>(&text) else {
                continue;
            };
            return match msg {
                ClientMessage::Message { content } => Some(Inbound::Message { content }),
                ClientMessage::Cancel => Some(Inbound::Cancel),
                ClientMessage::Command { name, args } => Some(Inbound::Command { name, args }),
                ClientMessage::Status => Some(Inbound::SyncRequest),
                ClientMessage::History => {
                    Some(Inbound::Command { name: "history".into(), args: String::new() })
                }
            };
        }
    }

    async fn send(&mut self, event: AgentEvent) -> bool {
        match event {
            AgentEvent::Thinking(content) => {
                self.send_msg(ServerMessage::Thinking { content })
            }
            AgentEvent::Text(content) => {
                self.send_msg(ServerMessage::Text { content })
            }
            AgentEvent::Tool(ToolEvent::Start { tool_name, .. }) => {
                self.send_msg(ServerMessage::ToolUseStart { tool_name })
            }
            AgentEvent::Tool(ToolEvent::ArgsDelta(delta)) => {
                self.send_msg(ServerMessage::ToolUseDelta(delta))
            }
            AgentEvent::Tool(ToolEvent::Invoke { tool_name, tool_id, input }) => {
                self.send_msg(ServerMessage::ToolUse { tool_name, tool_id, input })
            }
            AgentEvent::Tool(ToolEvent::OutputDelta { tool_id, delta }) => {
                self.send_msg(ServerMessage::ToolResultDelta { tool_id, delta })
            }
            AgentEvent::Tool(ToolEvent::Complete { tool_id, result, .. }) => {
                self.send_msg(ServerMessage::ToolResult { tool_id, result })
            }
            AgentEvent::Meta(MetaEvent::Usage { input_tokens, output_tokens, .. }) => {
                self.send_msg(ServerMessage::Usage { input_tokens, output_tokens })
            }
            AgentEvent::TurnComplete => {
                self.send_msg(ServerMessage::Done)
            }
            AgentEvent::Error(message) => {
                self.send_msg(ServerMessage::Error { message })
            }
            AgentEvent::Meta(MetaEvent::Steered { message }) => {
                self.send_msg(ServerMessage::System { message })
            }
            AgentEvent::Meta(MetaEvent::Shutdown { reason }) => {
                self.send_msg(ServerMessage::System {
                    message: format!("shutdown: {}", reason),
                });
                false
            }
            // Events without wire protocol mapping — skip
            _ => true,
        }
    }

    async fn on_sync(&mut self, state: SyncState) {
        let _ = self.send_msg(ServerMessage::System {
            message: format!("connected — model: {}, session: {}", state.model, state.session_id),
        });
    }

    fn name(&self) -> &str { "websocket" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_pair() -> (mpsc::UnboundedSender<String>, WebSocketTransport, mpsc::UnboundedReceiver<String>) {
        let (client_tx, ws_rx) = mpsc::unbounded_channel();
        let (ws_tx, server_rx) = mpsc::unbounded_channel();
        let transport = WebSocketTransport::new(ws_rx, ws_tx);
        (client_tx, transport, server_rx)
    }

    #[tokio::test]
    async fn test_recv_message() {
        let (client_tx, mut transport, _server_rx) = make_pair();
        let msg = serde_json::to_string(&ClientMessage::Message {
            content: "hello".into(),
        }).unwrap();
        client_tx.send(msg).unwrap();

        let inbound = transport.recv().await.unwrap();
        match inbound {
            Inbound::Message { content } => assert_eq!(content, "hello"),
            _ => panic!("expected Message"),
        }
    }

    #[tokio::test]
    async fn test_recv_cancel() {
        let (client_tx, mut transport, _server_rx) = make_pair();
        let msg = serde_json::to_string(&ClientMessage::Cancel).unwrap();
        client_tx.send(msg).unwrap();

        let inbound = transport.recv().await.unwrap();
        assert!(matches!(inbound, Inbound::Cancel));
    }

    #[tokio::test]
    async fn test_recv_command() {
        let (client_tx, mut transport, _server_rx) = make_pair();
        let msg = serde_json::to_string(&ClientMessage::Command {
            name: "model".into(),
            args: "opus".into(),
        }).unwrap();
        client_tx.send(msg).unwrap();

        let inbound = transport.recv().await.unwrap();
        match inbound {
            Inbound::Command { name, args } => {
                assert_eq!(name, "model");
                assert_eq!(args, "opus");
            }
            _ => panic!("expected Command"),
        }
    }

    #[tokio::test]
    async fn test_recv_status() {
        let (client_tx, mut transport, _server_rx) = make_pair();
        let msg = serde_json::to_string(&ClientMessage::Status).unwrap();
        client_tx.send(msg).unwrap();

        let inbound = transport.recv().await.unwrap();
        assert!(matches!(inbound, Inbound::SyncRequest));
    }

    #[tokio::test]
    async fn test_recv_skips_malformed() {
        let (client_tx, mut transport, _server_rx) = make_pair();
        client_tx.send("not json".into()).unwrap();
        let valid = serde_json::to_string(&ClientMessage::Cancel).unwrap();
        client_tx.send(valid).unwrap();

        // Should skip malformed and return Cancel
        let inbound = transport.recv().await.unwrap();
        assert!(matches!(inbound, Inbound::Cancel));
    }

    #[tokio::test]
    async fn test_send_text() {
        let (_client_tx, mut transport, mut server_rx) = make_pair();
        let ok = transport.send(AgentEvent::Text("hi".into())).await;
        assert!(ok);

        let json = server_rx.recv().await.unwrap();
        let msg: ServerMessage = serde_json::from_str(&json).unwrap();
        match msg {
            ServerMessage::Text { content } => assert_eq!(content, "hi"),
            _ => panic!("expected Text"),
        }
    }

    #[tokio::test]
    async fn test_send_thinking() {
        let (_client_tx, mut transport, mut server_rx) = make_pair();
        transport.send(AgentEvent::Thinking("hmm".into())).await;

        let json = server_rx.recv().await.unwrap();
        let msg: ServerMessage = serde_json::from_str(&json).unwrap();
        match msg {
            ServerMessage::Thinking { content } => assert_eq!(content, "hmm"),
            _ => panic!("expected Thinking"),
        }
    }

    #[tokio::test]
    async fn test_send_tool_use() {
        let (_client_tx, mut transport, mut server_rx) = make_pair();
        transport.send(AgentEvent::Tool(ToolEvent::Invoke {
            tool_name: "bash".into(),
            tool_id: "t1".into(),
            input: json!({"cmd": "ls"}),
        })).await;

        let json = server_rx.recv().await.unwrap();
        let msg: ServerMessage = serde_json::from_str(&json).unwrap();
        match msg {
            ServerMessage::ToolUse { tool_name, tool_id, input } => {
                assert_eq!(tool_name, "bash");
                assert_eq!(tool_id, "t1");
                assert_eq!(input["cmd"], "ls");
            }
            _ => panic!("expected ToolUse"),
        }
    }

    #[tokio::test]
    async fn test_send_done() {
        let (_client_tx, mut transport, mut server_rx) = make_pair();
        transport.send(AgentEvent::TurnComplete).await;

        let json = server_rx.recv().await.unwrap();
        let msg: ServerMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(msg, ServerMessage::Done));
    }

    #[tokio::test]
    async fn test_send_error() {
        let (_client_tx, mut transport, mut server_rx) = make_pair();
        transport.send(AgentEvent::Error("boom".into())).await;

        let json = server_rx.recv().await.unwrap();
        let msg: ServerMessage = serde_json::from_str(&json).unwrap();
        match msg {
            ServerMessage::Error { message } => assert_eq!(message, "boom"),
            _ => panic!("expected Error"),
        }
    }

    #[tokio::test]
    async fn test_send_shutdown_returns_false() {
        let (_client_tx, mut transport, mut server_rx) = make_pair();
        let ok = transport.send(AgentEvent::Meta(MetaEvent::Shutdown {
            reason: "bye".into(),
        })).await;
        assert!(!ok);

        let json = server_rx.recv().await.unwrap();
        let msg: ServerMessage = serde_json::from_str(&json).unwrap();
        match msg {
            ServerMessage::System { message } => assert!(message.contains("bye")),
            _ => panic!("expected System"),
        }
    }

    #[tokio::test]
    async fn test_send_usage() {
        let (_client_tx, mut transport, mut server_rx) = make_pair();
        transport.send(AgentEvent::Meta(MetaEvent::Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            model: "test".into(),
            cost_usd: 0.01,
        })).await;

        let json = server_rx.recv().await.unwrap();
        let msg: ServerMessage = serde_json::from_str(&json).unwrap();
        match msg {
            ServerMessage::Usage { input_tokens, output_tokens } => {
                assert_eq!(input_tokens, 100);
                assert_eq!(output_tokens, 50);
            }
            _ => panic!("expected Usage"),
        }
    }

    #[tokio::test]
    async fn test_recv_none_on_closed_channel() {
        let (client_tx, mut transport, _server_rx) = make_pair();
        drop(client_tx);
        assert!(transport.recv().await.is_none());
    }

    #[tokio::test]
    async fn test_on_sync_sends_system() {
        let (_client_tx, mut transport, mut server_rx) = make_pair();
        let state = SyncState {
            agent_name: None,
            model: "sonnet".into(),
            thinking_level: "low".into(),
            session_id: "abc123".into(),
            is_streaming: false,
            turn_count: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cost_usd: 0.0,
            partial_text: None,
            partial_thinking: None,
            active_tool: None,
            recent_events: vec![],
        };
        transport.on_sync(state).await;

        let json = server_rx.recv().await.unwrap();
        let msg: ServerMessage = serde_json::from_str(&json).unwrap();
        match msg {
            ServerMessage::System { message } => {
                assert!(message.contains("sonnet"));
                assert!(message.contains("abc123"));
            }
            _ => panic!("expected System"),
        }
    }
}
