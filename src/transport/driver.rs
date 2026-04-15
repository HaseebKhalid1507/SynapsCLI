use std::collections::VecDeque;
use super::{AgentBus, AgentEvent, SyncState};

#[derive(Debug, Clone)]
pub struct DriverConfig {
    pub agent_name: Option<String>,
    pub model: String,
    pub thinking_level: String,
    pub session_id: String,
    pub auto_save: bool,
    pub event_buffer_size: usize,
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self {
            agent_name: None,
            model: "claude-sonnet-4-20250514".to_string(),
            thinking_level: "medium".to_string(),
            session_id: String::new(),
            auto_save: true,
            event_buffer_size: 100,
        }
    }
}

pub struct ConversationDriver {
    bus: AgentBus,
    config: DriverConfig,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cost_usd: f64,
    turn_count: u64,
    tool_call_count: u64,
    recent_events: VecDeque<AgentEvent>,
    partial_text: String,
    partial_thinking: String,
    active_tool: Option<String>,
    is_streaming: bool,
}

impl ConversationDriver {
    pub fn new(config: DriverConfig) -> Self {
        Self {
            bus: AgentBus::new(),
            config,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cost_usd: 0.0,
            turn_count: 0,
            tool_call_count: 0,
            recent_events: VecDeque::new(),
            partial_text: String::new(),
            partial_thinking: String::new(),
            active_tool: None,
            is_streaming: false,
        }
    }

    pub fn bus(&self) -> &AgentBus { 
        &self.bus 
    }

    pub fn bus_mut(&mut self) -> &mut AgentBus { 
        &mut self.bus 
    }

    pub fn sync_state(&self) -> SyncState {
        SyncState {
            agent_name: self.config.agent_name.clone(),
            model: self.config.model.clone(),
            thinking_level: self.config.thinking_level.clone(),
            session_id: self.config.session_id.clone(),
            is_streaming: self.is_streaming,
            turn_count: self.turn_count,
            total_input_tokens: self.total_input_tokens,
            total_output_tokens: self.total_output_tokens,
            total_cost_usd: self.total_cost_usd,
            partial_text: if self.partial_text.is_empty() { None } else { Some(self.partial_text.clone()) },
            partial_thinking: if self.partial_thinking.is_empty() { None } else { Some(self.partial_thinking.clone()) },
            active_tool: self.active_tool.clone(),
            recent_events: self.recent_events.iter().cloned().collect(),
        }
    }

    pub fn is_streaming(&self) -> bool { 
        self.is_streaming 
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_driver_default_config() {
        let config = DriverConfig::default();
        assert_eq!(config.model, "claude-sonnet-4-20250514");
        assert_eq!(config.thinking_level, "medium");
        assert_eq!(config.auto_save, true);
        assert_eq!(config.event_buffer_size, 100);
    }

    #[test]
    fn test_driver_construct() {
        let config = DriverConfig::default();
        let driver = ConversationDriver::new(config);
        
        assert!(!driver.is_streaming());
        assert_eq!(driver.turn_count, 0);
        assert_eq!(driver.total_cost_usd, 0.0);
    }

    #[test]
    fn test_sync_state() {
        let mut config = DriverConfig::default();
        config.agent_name = Some("TestAgent".to_string());
        config.session_id = "test123".to_string();
        
        let driver = ConversationDriver::new(config);
        let sync = driver.sync_state();
        
        assert_eq!(sync.agent_name, Some("TestAgent".to_string()));
        assert_eq!(sync.session_id, "test123");
        assert_eq!(sync.model, "claude-sonnet-4-20250514");
        assert!(!sync.is_streaming);
        assert_eq!(sync.turn_count, 0);
    }

    #[test]
    fn test_bus_reference() {
        let driver = ConversationDriver::new(DriverConfig::default());
        let _rx = driver.bus().subscribe();
        // Just verify we can get a reference and subscribe
    }
}