use serde::{Serialize, Deserialize};
use super::AgentEvent;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncState {
    pub agent_name: Option<String>,
    pub model: String,
    pub thinking_level: String,
    pub session_id: String,
    pub is_streaming: bool,
    pub turn_count: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost_usd: f64,
    pub partial_text: Option<String>,
    pub partial_thinking: Option<String>,
    pub active_tool: Option<String>,
    pub recent_events: Vec<AgentEvent>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sync_state_construct() {
        let state = SyncState {
            agent_name: Some("Claude".to_string()),
            model: "claude-sonnet-4".to_string(),
            thinking_level: "medium".to_string(),
            session_id: "test123".to_string(),
            is_streaming: false,
            turn_count: 5,
            total_input_tokens: 1000,
            total_output_tokens: 800,
            total_cost_usd: 0.05,
            partial_text: None,
            partial_thinking: Some("thinking...".to_string()),
            active_tool: Some("bash".to_string()),
            recent_events: vec![AgentEvent::TurnComplete],
        };
        
        assert_eq!(state.turn_count, 5);
        assert_eq!(state.model, "claude-sonnet-4");
        assert_eq!(state.agent_name, Some("Claude".to_string()));
    }
}