use serde::{Serialize, Deserialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum AgentEvent {
    Text(String),
    Thinking(String),
    Tool(ToolEvent),
    Subagent(SubagentEvent),
    Meta(MetaEvent),
    MessageHistory(Vec<Value>),
    TurnComplete,
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ToolEvent {
    Start { tool_name: String, tool_id: String },
    ArgsDelta(String),
    Invoke { tool_name: String, tool_id: String, input: Value },
    OutputDelta { tool_id: String, delta: String },
    Complete { tool_id: String, result: String, elapsed_ms: Option<u64> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SubagentEvent {
    Start { id: u64, agent_name: String, task_preview: String },
    Update { id: u64, agent_name: String, status: String },
    Done { id: u64, agent_name: String, result_preview: String, duration_secs: f64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum MetaEvent {
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
        model: String,
        cost_usd: f64,
    },
    SessionStats {
        total_input_tokens: u64,
        total_output_tokens: u64,
        total_cost_usd: f64,
        turn_count: u64,
        tool_call_count: u64,
    },
    Steered { message: String },
    Shutdown { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_agent_event_variants() {
        let _text = AgentEvent::Text("hello".to_string());
        let _thinking = AgentEvent::Thinking("thinking...".to_string());
        let _tool = AgentEvent::Tool(ToolEvent::Start { 
            tool_name: "test".to_string(), 
            tool_id: "1".to_string() 
        });
        let _subagent = AgentEvent::Subagent(SubagentEvent::Start {
            id: 1,
            agent_name: "test".to_string(),
            task_preview: "task".to_string(),
        });
        let _meta = AgentEvent::Meta(MetaEvent::Steered { message: "test".to_string() });
        let _msg_history = AgentEvent::MessageHistory(vec![]);
        let _complete = AgentEvent::TurnComplete;
        let _error = AgentEvent::Error("error".to_string());
    }

    #[test]
    fn test_tool_event_variants() {
        let _start = ToolEvent::Start { tool_name: "bash".to_string(), tool_id: "1".to_string() };
        let _args = ToolEvent::ArgsDelta("ls".to_string());
        let _invoke = ToolEvent::Invoke { 
            tool_name: "bash".to_string(), 
            tool_id: "1".to_string(), 
            input: json!({"command": "ls"}) 
        };
        let _output = ToolEvent::OutputDelta { tool_id: "1".to_string(), delta: "file1\n".to_string() };
        let _complete = ToolEvent::Complete { 
            tool_id: "1".to_string(), 
            result: "done".to_string(), 
            elapsed_ms: Some(100) 
        };
    }

    #[test]
    fn test_agent_event_clone() {
        let original = AgentEvent::Text("test".to_string());
        let cloned = original.clone();
        match (&original, &cloned) {
            (AgentEvent::Text(s1), AgentEvent::Text(s2)) => assert_eq!(s1, s2),
            _ => panic!("clone failed"),
        }
    }
}