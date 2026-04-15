use serde::{Serialize, Deserialize};
use super::events::AgentEvent;
use super::sync::SyncState;

/// Server → client over the attach NDJSON stream
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttachEvent {
    Sync { state: SyncState },
    Event { event: AgentEvent },
}

/// Client → server over the attach NDJSON stream
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttachInbound {
    Message { content: String },
    Cancel,
    Detach,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json;

    #[test]
    fn test_attach_event_sync_roundtrip() {
        let state = SyncState {
            agent_name: Some("test".to_string()),
            model: "claude-sonnet-4".to_string(),
            thinking_level: "medium".to_string(),
            session_id: "s1".to_string(),
            is_streaming: false,
            turn_count: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cost_usd: 0.0,
            partial_text: None,
            partial_thinking: None,
            active_tool: None,
            recent_events: Vec::new(),
        };
        let ev = AttachEvent::Sync { state };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"type\":\"sync\""));
        let decoded: AttachEvent = serde_json::from_str(&json).unwrap();
        match decoded {
            AttachEvent::Sync { state } => assert_eq!(state.model, "claude-sonnet-4"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_attach_event_event_roundtrip() {
        let ev = AttachEvent::Event {
            event: AgentEvent::Text("hello".to_string()),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"type\":\"event\""));
        let decoded: AttachEvent = serde_json::from_str(&json).unwrap();
        match decoded {
            AttachEvent::Event { event } => match event {
                AgentEvent::Text(s) => assert_eq!(s, "hello"),
                _ => panic!("wrong event variant"),
            },
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_attach_inbound_roundtrip() {
        let cases = vec![
            AttachInbound::Message { content: "hi".to_string() },
            AttachInbound::Cancel,
            AttachInbound::Detach,
        ];
        for inbound in cases {
            let json = serde_json::to_string(&inbound).unwrap();
            let decoded: AttachInbound = serde_json::from_str(&json).unwrap();
            match (&inbound, &decoded) {
                (AttachInbound::Message { content: a }, AttachInbound::Message { content: b }) => assert_eq!(a, b),
                (AttachInbound::Cancel, AttachInbound::Cancel) => {}
                (AttachInbound::Detach, AttachInbound::Detach) => {}
                _ => panic!("roundtrip mismatch"),
            }
        }
    }

    #[test]
    fn test_agent_event_serialize_all_variants() {
        let events = vec![
            AgentEvent::Text("hello".to_string()),
            AgentEvent::Thinking("hmm".to_string()),
            AgentEvent::TurnComplete,
            AgentEvent::Error("boom".to_string()),
            AgentEvent::Tool(super::super::ToolEvent::Start {
                tool_name: "bash".to_string(),
                tool_id: "1".to_string(),
            }),
            AgentEvent::Subagent(super::super::SubagentEvent::Start {
                id: 1,
                agent_name: "sub".to_string(),
                task_preview: "task".to_string(),
            }),
            AgentEvent::Meta(super::super::MetaEvent::Steered {
                message: "ok".to_string(),
            }),
        ];
        for ev in events {
            let json = serde_json::to_string(&ev).unwrap();
            let decoded: AgentEvent = serde_json::from_str(&json).unwrap();
            // Just confirm roundtrip doesn't panic
            let _ = format!("{:?}", decoded);
        }
    }
}
