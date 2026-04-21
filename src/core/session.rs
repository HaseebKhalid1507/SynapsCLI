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
    /// Saved abort context — injected into the next user message on /continue
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abort_context: Option<String>,
    /// ID of the session this was compacted from (backward link)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    /// ID of the session created by compacting this one (forward link)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted_into: Option<String>,
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
            abort_context: None,
            parent_session: None,
            compacted_into: None,
        }
    }

    /// Create a new session from a compaction summary, linked to the parent.
    pub fn new_from_compaction(parent: &Session, summary: String) -> Self {
        let now = Utc::now();
        let id = format!("{}-{}", now.format("%Y%m%d-%H%M%S"), &uuid::Uuid::new_v4().to_string()[..4]);
        let mut summary_parts = String::new();
        if let Some(ref sp) = parent.system_prompt {
            summary_parts.push_str(&format!("<system-prompt>\n{}\n</system-prompt>\n\n", sp));
        }
        summary_parts.push_str(&format!(
            "The conversation history before this point was compacted into the following summary:\n\n<context-summary>\n{}\n</context-summary>\n\nContinue from where we left off. The summary and system prompt above contain all the context you need.",
            summary
        ));
        Session {
            id,
            title: format!("↳ {}", if parent.title.is_empty() { &parent.id } else { &parent.title }),
            model: parent.model.clone(),
            thinking_level: parent.thinking_level.clone(),
            system_prompt: parent.system_prompt.clone(),
            created_at: now,
            updated_at: now,
            total_input_tokens: 0,
            total_output_tokens: 0,
            session_cost: 0.0,
            api_messages: vec![
                serde_json::json!({"role": "user", "content": summary_parts}),
                serde_json::json!({"role": "assistant", "content": "I've loaded the conversation summary and system prompt. Ready to continue."}),
            ],
            abort_context: None,
            parent_session: Some(parent.id.clone()),
            compacted_into: None,
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

    pub async fn save(&self) -> std::io::Result<()> {
        let dir = crate::config::resolve_write_path("sessions");
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(format!("{}.json", self.id));
        let tmp = path.with_extension("tmp");
        let json = serde_json::to_string(self)
            .map_err(std::io::Error::other)?;
        tokio::fs::write(&tmp, &json).await?;
        tokio::fs::rename(&tmp, &path).await
    }

    pub fn load(id: &str) -> std::io::Result<Self> {
        let path = sessions_dir().join(format!("{}.json", id));
        let content = std::fs::read_to_string(path)?;
        serde_json::from_str(&content)
            .map_err(std::io::Error::other)
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
        _ => Err(std::io::Error::other(format!("ambiguous: {} sessions match '{}'", matches.len(), partial_id))),
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
        if path.extension().is_some_and(|e| e == "json") {
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

    #[test]
    fn test_session_serialization_round_trip() {
        let mut session = Session::new("gpt-4-turbo", "detailed", Some("You are a helpful assistant"));
        session.title = "Test Session".to_string();
        session.api_messages.push(json!({"role": "user", "content": "test"}));
        session.total_input_tokens = 100;
        session.total_output_tokens = 200;
        session.session_cost = 0.15;

        // Serialize to JSON string
        let json_str = serde_json::to_string(&session).expect("Failed to serialize session");
        
        // Deserialize back from JSON string
        let deserialized: Session = serde_json::from_str(&json_str).expect("Failed to deserialize session");

        // Verify all fields match
        assert_eq!(deserialized.id, session.id);
        assert_eq!(deserialized.title, session.title);
        assert_eq!(deserialized.model, session.model);
        assert_eq!(deserialized.thinking_level, session.thinking_level);
        assert_eq!(deserialized.system_prompt, session.system_prompt);
        assert_eq!(deserialized.created_at, session.created_at);
        assert_eq!(deserialized.updated_at, session.updated_at);
        assert_eq!(deserialized.total_input_tokens, session.total_input_tokens);
        assert_eq!(deserialized.total_output_tokens, session.total_output_tokens);
        assert_eq!(deserialized.session_cost, session.session_cost);
        assert_eq!(deserialized.api_messages.len(), session.api_messages.len());
        assert_eq!(deserialized.api_messages[0], session.api_messages[0]);
    }

    #[test] 
    fn test_session_serialization_preserves_all_fields() {
        let mut session = Session::new("claude-3-opus", "comprehensive", Some("Custom system prompt"));
        session.title = "Complex Session".to_string();
        
        // Add multiple messages
        session.api_messages.push(json!({"role": "user", "content": "First message"}));
        session.api_messages.push(json!({"role": "assistant", "content": "First response"}));
        session.api_messages.push(json!({"role": "user", "content": "Second message"}));
        
        // Set token counts and cost
        session.total_input_tokens = 1500;
        session.total_output_tokens = 2500;
        session.session_cost = 0.75;

        // Serialize and deserialize
        let json_str = serde_json::to_string(&session).unwrap();
        let restored: Session = serde_json::from_str(&json_str).unwrap();

        // Verify every field is preserved
        assert_eq!(restored.id, session.id);
        assert_eq!(restored.title, "Complex Session");
        assert_eq!(restored.model, "claude-3-opus");
        assert_eq!(restored.thinking_level, "comprehensive");
        assert_eq!(restored.system_prompt.as_ref().unwrap(), "Custom system prompt");
        assert_eq!(restored.created_at, session.created_at);
        assert_eq!(restored.updated_at, session.updated_at);
        assert_eq!(restored.total_input_tokens, 1500);
        assert_eq!(restored.total_output_tokens, 2500);
        assert_eq!(restored.session_cost, 0.75);
        assert_eq!(restored.api_messages.len(), 3);
        assert_eq!(restored.api_messages[0]["role"], "user");
        assert_eq!(restored.api_messages[0]["content"], "First message");
        assert_eq!(restored.api_messages[1]["role"], "assistant");
        assert_eq!(restored.api_messages[2]["content"], "Second message");
    }

    #[test]
    fn test_session_info_from_session_with_messages() {
        let mut session = Session::new("gpt-3.5-turbo", "normal", None);
        
        // Add exactly 3 messages
        session.api_messages.push(json!({"role": "user", "content": "message 1"}));
        session.api_messages.push(json!({"role": "assistant", "content": "response 1"}));
        session.api_messages.push(json!({"role": "user", "content": "message 2"}));
        
        let info = session.info();
        
        // Verify message count is exactly 3
        assert_eq!(info.message_count, 3);
        assert_eq!(info.id, session.id);
        assert_eq!(info.model, "gpt-3.5-turbo");
    }

    #[test] 
    fn test_session_auto_title_truncation() {
        let mut session = Session::new("gpt-4", "brief", None);
        
        // Create a user message with exactly 200 characters
        let long_content = "a".repeat(200);
        session.api_messages.push(json!({
            "role": "user",
            "content": long_content
        }));
        
        session.auto_title();
        
        // Verify title is exactly 80 characters
        assert_eq!(session.title.len(), 80);
        assert_eq!(session.title, "a".repeat(80));
    }

    #[test]
    fn test_session_auto_title_skips_non_user_messages() {
        let mut session = Session::new("gpt-4", "brief", None);
        
        // Push only an assistant message (no user messages)
        session.api_messages.push(json!({
            "role": "assistant", 
            "content": "This should be ignored for auto title"
        }));
        
        session.auto_title();
        
        // Verify title stays empty since there are no user messages
        assert_eq!(session.title, "");
        
        // Test with system message too
        session.api_messages.push(json!({
            "role": "system",
            "content": "System message should also be ignored"
        }));
        
        session.auto_title();
        assert_eq!(session.title, "");
    }

    #[test]
    fn test_session_new_generates_unique_ids() {
        let session1 = Session::new("gpt-4", "brief", None);
        let session2 = Session::new("gpt-4", "brief", None);
        
        // Verify IDs are different
        assert_ne!(session1.id, session2.id);
        assert!(!session1.id.is_empty());
        assert!(!session2.id.is_empty());
    }

    #[test]
    fn test_session_new_timestamps() {
        let before = Utc::now();
        let session = Session::new("gpt-4", "brief", None);
        let after = Utc::now();
        
        // Verify created_at and updated_at are close to now (within 2 seconds)
        let created_diff = (session.created_at - before).num_seconds().abs();
        let updated_diff = (session.updated_at - before).num_seconds().abs();
        
        assert!(created_diff <= 2, "created_at should be within 2 seconds of now");
        assert!(updated_diff <= 2, "updated_at should be within 2 seconds of now");
        
        // Verify both timestamps are the same for new sessions
        assert_eq!(session.created_at, session.updated_at);
        
        // Verify timestamps are not in the future
        assert!(session.created_at <= after);
        assert!(session.updated_at <= after);
    }
}
