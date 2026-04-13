use serde::{Serialize, Deserialize};
use serde_json::Value;
use std::path::PathBuf;
use chrono::{DateTime, Utc};



#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub model: String,
    pub thinking_level: String,
    pub system_prompt: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub session_cost: f64,
    pub api_messages: Vec<Value>,
}

/// Lightweight info for listing sessions without loading full message history
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    pub model: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub session_cost: f64,
    pub message_count: usize,
}

impl Session {
    pub fn new(model: &str, thinking_level: &str, system_prompt: Option<&str>) -> Self {
        let now = Utc::now();
        let id = format!("{}-{}", now.format("%Y%m%d-%H%M%S"), &uuid::Uuid::new_v4().to_string()[..4]);
        Session {
            id,
            title: String::new(),
            model: model.to_string(),
            thinking_level: thinking_level.to_string(),
            system_prompt: system_prompt.map(|s| s.to_string()),
            created_at: now,
            updated_at: now,
            total_input_tokens: 0,
            total_output_tokens: 0,
            session_cost: 0.0,
            api_messages: Vec::new(),
        }
    }

    /// Set title from the first user message if not already set
    pub fn auto_title(&mut self) {
        if !self.title.is_empty() {
            return;
        }
        for msg in &self.api_messages {
            if msg["role"].as_str() == Some("user") {
                if let Some(content) = msg["content"].as_str() {
                    self.title = content.chars().take(80).collect();
                    return;
                }
            }
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let dir = crate::config::resolve_write_path("sessions");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.json", self.id));
        let json = serde_json::to_string(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        std::fs::write(path, json)
    }

    pub fn load(id: &str) -> std::io::Result<Self> {
        let path = sessions_dir().join(format!("{}.json", id));
        let content = std::fs::read_to_string(path)?;
        serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
    }

    pub fn info(&self) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            title: self.title.clone(),
            model: self.model.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
            session_cost: self.session_cost,
            message_count: self.api_messages.len(),
        }
    }
}

/// Find a session by full or partial ID match
pub fn find_session(partial_id: &str) -> std::io::Result<Session> {
    let dir = sessions_dir();
    if !dir.exists() {
        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no sessions directory"));
    }

    // Try exact match first
    let exact = dir.join(format!("{}.json", partial_id));
    if exact.exists() {
        return Session::load(partial_id);
    }

    // Partial match — find all that contain the partial ID
    let mut matches: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".json") {
            let id = name.trim_end_matches(".json");
            if id.contains(partial_id) {
                matches.push(id.to_string());
            }
        }
    }

    match matches.len() {
        0 => Err(std::io::Error::new(std::io::ErrorKind::NotFound, format!("no session matching '{}'", partial_id))),
        1 => Session::load(&matches[0]),
        _ => Err(std::io::Error::new(std::io::ErrorKind::Other, format!("ambiguous: {} sessions match '{}'", matches.len(), partial_id))),
    }
}

/// Load the most recently updated session
pub fn latest_session() -> std::io::Result<Session> {
    let sessions = list_sessions()?;
    sessions.into_iter()
        .max_by_key(|s| s.updated_at)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no sessions found"))
        .and_then(|info| Session::load(&info.id))
}

/// List all sessions, sorted by most recently updated.
/// Uses a lightweight struct to skip deserializing the full message history.
pub fn list_sessions() -> std::io::Result<Vec<SessionInfo>> {
    /// Lightweight struct for listing — skips api_messages entirely.
    #[derive(Deserialize)]
    struct SessionMetadata {
        id: String,
        #[serde(default)]
        title: String,
        model: String,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
        #[serde(default)]
        session_cost: f64,
        #[serde(default)]
        api_messages: Vec<serde::de::IgnoredAny>,
    }

    let dir = sessions_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut sessions: Vec<SessionInfo> = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map_or(false, |e| e == "json") {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(meta) = serde_json::from_str::<SessionMetadata>(&content) {
                    sessions.push(SessionInfo {
                        id: meta.id,
                        title: meta.title,
                        model: meta.model,
                        created_at: meta.created_at,
                        updated_at: meta.updated_at,
                        session_cost: meta.session_cost,
                        message_count: meta.api_messages.len(),
                    });
                }
            }
        }
    }

    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(sessions)
}

fn sessions_dir() -> PathBuf {
    crate::config::get_active_config_dir().join("sessions")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_session_new() {
        let session = Session::new("gpt-4", "brief", Some("test prompt"));
        
        // Check model and thinking_level are set correctly
        assert_eq!(session.model, "gpt-4");
        assert_eq!(session.thinking_level, "brief");
        assert_eq!(session.system_prompt, Some("test prompt".to_string()));
        
        // Check ID is non-empty
        assert!(!session.id.is_empty());
        
        // Check title starts empty
        assert_eq!(session.title, "");
        
        // Check tokens are 0
        assert_eq!(session.total_input_tokens, 0);
        assert_eq!(session.total_output_tokens, 0);
        
        // Check cost is 0
        assert_eq!(session.session_cost, 0.0);
        
        // Check api_messages is empty
        assert!(session.api_messages.is_empty());
        
        // Test without system prompt
        let session_no_prompt = Session::new("gpt-3.5-turbo", "normal", None);
        assert_eq!(session_no_prompt.model, "gpt-3.5-turbo");
        assert_eq!(session_no_prompt.thinking_level, "normal");
        assert_eq!(session_no_prompt.system_prompt, None);
    }

    #[test]
    fn test_session_auto_title() {
        let mut session = Session::new("gpt-4", "brief", None);
        
        // Add a user message
        session.api_messages.push(json!({
            "role": "user",
            "content": "hello world"
        }));
        
        // Call auto_title
        session.auto_title();
        
        // Check title is set to message content
        assert_eq!(session.title, "hello world");
        
        // Test it doesn't overwrite existing title
        session.title = "existing title".to_string();
        session.auto_title();
        assert_eq!(session.title, "existing title");
        
        // Test with empty session (no messages)
        let mut empty_session = Session::new("gpt-4", "brief", None);
        empty_session.auto_title();
        assert_eq!(empty_session.title, "");
        
        // Test with non-user message
        let mut session_no_user = Session::new("gpt-4", "brief", None);
        session_no_user.api_messages.push(json!({
            "role": "assistant",
            "content": "response"
        }));
        session_no_user.auto_title();
        assert_eq!(session_no_user.title, "");
        
        // Test with long content (should truncate to 80 chars)
        let mut session_long = Session::new("gpt-4", "brief", None);
        let long_content = "a".repeat(100);
        session_long.api_messages.push(json!({
            "role": "user",
            "content": long_content
        }));
        session_long.auto_title();
        assert_eq!(session_long.title.len(), 80);
        assert_eq!(session_long.title, "a".repeat(80));
    }

    #[test]
    fn test_session_info() {
        let mut session = Session::new("gpt-4", "brief", Some("system prompt"));
        
        // Add some messages to test message count
        session.api_messages.push(json!({
            "role": "user",
            "content": "test message"
        }));
        session.api_messages.push(json!({
            "role": "assistant",
            "content": "test response"
        }));
        
        session.title = "Test Title".to_string();
        session.session_cost = 0.05;
        
        let info = session.info();
        
        assert_eq!(info.id, session.id);
        assert_eq!(info.title, "Test Title");
        assert_eq!(info.model, "gpt-4");
        assert_eq!(info.created_at, session.created_at);
        assert_eq!(info.updated_at, session.updated_at);
        assert_eq!(info.session_cost, 0.05);
        assert_eq!(info.message_count, 2);
    }

    #[test]
    fn test_session_info_struct() {
        let now = Utc::now();
        
        let session_info = SessionInfo {
            id: "test-id".to_string(),
            title: "Test Title".to_string(),
            model: "gpt-4".to_string(),
            created_at: now,
            updated_at: now,
            session_cost: 1.23,
            message_count: 5,
        };
        
        // Verify all fields are accessible
        assert_eq!(session_info.id, "test-id");
        assert_eq!(session_info.title, "Test Title");
        assert_eq!(session_info.model, "gpt-4");
        assert_eq!(session_info.created_at, now);
        assert_eq!(session_info.updated_at, now);
        assert_eq!(session_info.session_cost, 1.23);
        assert_eq!(session_info.message_count, 5);
    }
}
