use serde::{Serialize, Deserialize};
use serde_json::Value;
use std::path::PathBuf;
use chrono::{DateTime, Utc};

const SESSIONS_DIR: &str = ".agent-runtime/sessions";

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
        let dir = sessions_dir();
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

/// List all sessions, sorted by most recently updated
pub fn list_sessions() -> std::io::Result<Vec<SessionInfo>> {
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
                if let Ok(session) = serde_json::from_str::<Session>(&content) {
                    sessions.push(session.info());
                }
            }
        }
    }

    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(sessions)
}

fn sessions_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(SESSIONS_DIR)
}
