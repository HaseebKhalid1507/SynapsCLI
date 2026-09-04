use crate::core::stream_types::SharedMessage;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub model: String,
    pub thinking_level: String,
    pub system_prompt: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub session_cost: f64,
    pub api_messages: Vec<SharedMessage>,
    /// Saved abort context — injected into the next user message on /continue
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abort_context: Option<String>,
    /// ID of the session this was compacted from (backward link)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    /// ID of the session created by compacting this one (forward link)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted_into: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_provenance: Option<crate::prompt::PromptProvenance>,
    /// Typed compaction summary provenance (spec §9.3). Present on sessions
    /// produced by (or updated in place by) a compaction transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<crate::compaction::CompactionRecord>,
}

/// Lightweight info for listing sessions without loading full message history
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub model: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub session_cost: f64,
    pub message_count: usize,
}

impl Session {
    pub fn new(model: &str, thinking_level: &str, system_prompt: Option<&str>) -> Self {
        let now = Utc::now();
        let id = format!(
            "{}-{}",
            now.format("%Y%m%d-%H%M%S"),
            &uuid::Uuid::new_v4().to_string()[..4]
        );
        Session {
            id,
            title: String::new(),
            name: None,
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
            prompt_provenance: None,
            compaction: None,
        }
    }

    /// Create a successor session from a compaction transition (spec §9.3).
    /// The summary enters context through the canonical sanitized rendering
    /// ([`crate::compaction::compaction_context_messages`]); the parent's
    /// system prompt stays TYPED metadata — the successor's `system_prompt`
    /// field and the provenance record — never plain user text.
    pub fn from_compaction_record(
        parent: &Session,
        summary_text: &str,
        record: crate::compaction::CompactionRecord,
    ) -> Self {
        let now = Utc::now();
        let id = format!(
            "{}-{}",
            now.format("%Y%m%d-%H%M%S"),
            &uuid::Uuid::new_v4().to_string()[..4]
        );
        // Transfer session name from parent — the compacted session is the
        // continuation, so the name should follow. Parent's name will be
        // cleared when the caller saves it with compacted_into set.
        let name = parent.name.clone();
        Session {
            id,
            title: format!(
                "↳ {}",
                if parent.title.is_empty() {
                    &parent.id
                } else {
                    &parent.title
                }
            ),
            name,
            model: parent.model.clone(),
            thinking_level: parent.thinking_level.clone(),
            system_prompt: parent.system_prompt.clone(),
            created_at: now,
            updated_at: now,
            total_input_tokens: 0,
            total_output_tokens: 0,
            session_cost: 0.0,
            api_messages: crate::compaction::compaction_context_messages(summary_text),
            abort_context: None,
            parent_session: Some(parent.id.clone()),
            compacted_into: None,
            prompt_provenance: None,
            compaction: Some(record),
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

    /// Persist this session under the configured persistence mode
    /// (`session_persistence` config key; default is the unchanged legacy
    /// JSON path — see Task 35 / `crate::core::session_journal`).
    pub async fn save(&self) -> std::io::Result<()> {
        let dir = crate::config::resolve_write_path("sessions");
        let mode = crate::config::load_config().session_persistence;
        let session = self.clone(); // messages are Arc-shared — cheap clone
        tokio::task::spawn_blocking(move || {
            crate::core::session_journal::save_session_in_dir(&dir, &session, mode).map(|_| ())
        })
        .await
        .map_err(std::io::Error::other)?
    }

    pub fn load(id: &str) -> std::io::Result<Self> {
        Self::load_from_dir(&sessions_dir(), id)
    }

    /// Canonical backward-compatible host load for a caller-selected sessions
    /// directory. Both legacy snapshots and journal-backed sessions resolve
    /// through the same journal overlay implementation.
    pub fn load_from_dir(dir: &std::path::Path, id: &str) -> std::io::Result<Self> {
        crate::core::session_journal::load_session_in_dir(dir, id)
    }

    pub fn info(&self) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            title: self.title.clone(),
            name: self.name.clone(),
            model: self.model.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
            session_cost: self.session_cost,
            message_count: self.api_messages.len(),
        }
    }

    /// Assign a name to this session. Validates name, enforces uniqueness
    /// across sessions, and rejects collisions with existing chain names.
    /// Idempotent: re-applying the current name is a no-op.
    pub fn set_name(&mut self, name: &str) -> std::io::Result<()> {
        validate_name(name).map_err(std::io::Error::other)?;
        if self.name.as_deref() == Some(name) {
            return Ok(());
        }
        let sessions = list_sessions()?;
        for s in &sessions {
            if s.name.as_deref() == Some(name) && s.id != self.id {
                return Err(std::io::Error::other(format!(
                    "name '{}' already used by session {}",
                    name, s.id
                )));
            }
        }
        if crate::core::chain::load_chain(name).is_ok() {
            return Err(std::io::Error::other(format!(
                "name '{}' conflicts with an existing chain name",
                name
            )));
        }
        self.name = Some(name.to_string());
        Ok(())
    }

    pub fn clear_name(&mut self) {
        self.name = None;
    }
}

/// Blocking body of [`Session::save`]: create the sessions dir (0700), then
/// write `<id>.json` atomically via `private_fs` (temp file created with mode
/// 0600 — never create-then-chmod — then renamed; symlink targets refused).
#[cfg(test)]
pub(crate) fn save_json_in_dir(
    dir: &std::path::Path,
    id: &str,
    json: &[u8],
) -> std::io::Result<()> {
    // fix2: the SAME strict no-symlink root resolution and handle-relative
    // atomic write as every journal operation (see `session_journal`).
    crate::core::session_journal::write_json_snapshot(dir, id, json)
}

/// Find a session by full or partial ID match
pub fn find_session(partial_id: &str) -> std::io::Result<Session> {
    let dir = sessions_dir();
    if !dir.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no sessions directory",
        ));
    }

    // fix2: strict handle-relative enumeration for both match phases.
    let entries = crate::core::session_journal::session_dir_entries(&dir)?;

    // Try exact match first
    if entries
        .iter()
        .any(|e| e.name == format!("{partial_id}.json"))
    {
        return Session::load(partial_id);
    }

    // Partial match — find all that contain the partial ID
    let mut matches: Vec<String> = Vec::new();
    for entry in &entries {
        if entry.name.ends_with(".json") {
            let id = entry.name.trim_end_matches(".json");
            if id.contains(partial_id) {
                matches.push(id.to_string());
            }
        }
    }

    match matches.len() {
        0 => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no session matching '{}'", partial_id),
        )),
        1 => Session::load(&matches[0]),
        _ => Err(std::io::Error::other(format!(
            "ambiguous: {} sessions match '{}'",
            matches.len(),
            partial_id
        ))),
    }
}

/// Load the most recently updated session
pub fn latest_session() -> std::io::Result<Session> {
    // Find the most-recently-modified session file by FILE mtime — without
    // reading or JSON-parsing any of them. The previous impl called
    // list_sessions(), which read + serde-tokenized EVERY session file to sort
    // by the in-file `updated_at`; with hundreds of multi-MB sessions (221 files
    // / 76MB here) that made `--continue` boot take ~11s. mtime is a free, exact
    // proxy for "the session I was last in", and we then load ONLY that one.
    let dir = sessions_dir();
    if !dir.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no sessions found",
        ));
    }
    // fix2: strict handle-relative enumeration — a symlinked ancestor
    // refuses instead of being followed.
    let entries = crate::core::session_journal::session_dir_entries(&dir)?;
    let names: std::collections::HashSet<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    let mut newest: Option<(std::time::SystemTime, String)> = None;
    for entry in &entries {
        // A journal append IS a save of its session (Task 35): attribute the
        // journal's mtime to the sibling snapshot so "latest" stays exact
        // when snapshots lag behind appends.
        let id = if let Some(id) = entry.name.strip_suffix(".json") {
            id
        } else if let Some(id) = entry.name.strip_suffix(".journal") {
            if !names.contains(format!("{id}.json").as_str()) {
                continue; // orphan journal — not loadable
            }
            id
        } else {
            continue;
        };
        if let Some(mtime) = entry.mtime {
            if newest.as_ref().map_or(true, |(t, _)| mtime > *t) {
                newest = Some((mtime, id.to_string()));
            }
        }
    }
    let id = newest
        .map(|(_, id)| id)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no sessions found"))?;
    Session::load(&id)
}

/// List all sessions, sorted by most recently updated.
///
/// Reads only the metadata HEADER of each session file (see
/// [`read_session_header`]) — never the message history. Used by the session
/// resolvers (by name/id). For the capped `/sessions` display prefer
/// [`list_recent_sessions`], which reads only the N most-recent headers.
pub fn list_sessions() -> std::io::Result<Vec<SessionInfo>> {
    let dir = sessions_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut sessions: Vec<SessionInfo> = Vec::new();
    for entry in crate::core::session_journal::session_dir_entries(&dir)? {
        if entry.name.ends_with(".json") {
            if let Some(info) = parse_session_header(&dir, &entry.name) {
                sessions.push(info);
            }
        }
    }
    sessions.sort_by_key(|b| std::cmp::Reverse(b.updated_at));
    Ok(sessions)
}

/// The `limit` most-recently-modified sessions (by file mtime), most-recent
/// first. Sorts the directory entries by mtime WITHOUT parsing, then reads only
/// the top `limit` headers — so `/sessions` is O(limit) reads instead of
/// O(#sessions). mtime is an exact proxy for `updated_at` (the file is rewritten
/// on every save).
pub fn list_recent_sessions(limit: usize) -> std::io::Result<Vec<SessionInfo>> {
    let dir = sessions_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    // fix2: strict handle-relative enumeration.
    let entries = crate::core::session_journal::session_dir_entries(&dir)?;
    let names: std::collections::HashSet<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    let mut by_snapshot: std::collections::HashMap<String, std::time::SystemTime> =
        std::collections::HashMap::new();
    for entry in &entries {
        // Journal mtimes attribute to their session snapshot (Task 35).
        let id = if let Some(id) = entry.name.strip_suffix(".json") {
            id
        } else if let Some(id) = entry.name.strip_suffix(".journal") {
            if !names.contains(format!("{id}.json").as_str()) {
                continue;
            }
            id
        } else {
            continue;
        };
        if let Some(mtime) = entry.mtime {
            let slot = by_snapshot
                .entry(id.to_string())
                .or_insert(std::time::SystemTime::UNIX_EPOCH);
            if mtime > *slot {
                *slot = mtime;
            }
        }
    }
    let mut files: Vec<(std::time::SystemTime, String)> =
        by_snapshot.into_iter().map(|(id, t)| (t, id)).collect();
    files.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    files.truncate(limit);
    let mut sessions: Vec<SessionInfo> = Vec::new();
    for (_, id) in files {
        if let Some(info) = parse_session_header(&dir, &format!("{id}.json")) {
            sessions.push(info);
        }
    }
    Ok(sessions)
}

/// Read + parse just the metadata header of one session file into a
/// [`SessionInfo`] (no message history). `message_count` is left 0 — it isn't
/// parsed in the header read; use [`Session::info`] when an exact count matters.
fn parse_session_header(dir: &std::path::Path, file_name: &str) -> Option<SessionInfo> {
    #[derive(Deserialize)]
    struct SessionMetadata {
        id: String,
        #[serde(default)]
        title: String,
        #[serde(default)]
        name: Option<String>,
        model: String,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
        #[serde(default)]
        session_cost: f64,
    }
    let header = read_session_header(dir, file_name)?;
    let meta: SessionMetadata = serde_json::from_str(&header).ok()?;
    // Cheap message count: each api_message has exactly one "role" field.
    // Count occurrences without deserializing the full array.
    let message_count = header.matches("\"role\":").count();
    let mut info = SessionInfo {
        id: meta.id,
        title: meta.title,
        name: meta.name,
        model: meta.model,
        created_at: meta.created_at,
        updated_at: meta.updated_at,
        session_cost: meta.session_cost,
        message_count,
    };
    // Journal freshness overlay (Task 35): when an opt-in journal exists,
    // its bounded meta tail is newer than the (possibly lagging) snapshot
    // header. Legacy sessions have no journal — zero extra I/O.
    if let Some(tail) = crate::core::session_journal::journal_meta_tail(dir, &info.id) {
        if tail.updated_at > info.updated_at {
            info.updated_at = tail.updated_at;
            info.session_cost = tail.session_cost;
        }
    }
    Some(info)
}

/// Read just the metadata header of a session file — everything BEFORE the
/// (potentially multi-MB) `"api_messages"` array — and return it as a complete,
/// parseable JSON object string. Reads in bounded chunks and STOPS as soon as
/// the `api_messages` key is found, so it never reads or tokenizes the message
/// history. Falls back to the whole file if the key isn't found (small/new
/// sessions). This is what keeps `list_sessions()` O(#sessions) instead of
/// O(total bytes on disk).
fn read_session_header(dir: &std::path::Path, file_name: &str) -> Option<String> {
    use std::io::Read;
    const KEY: &[u8] = b"\"api_messages\"";
    const MAX_HEADER: usize = 256 * 1024; // safety cap if the key is never found

    // fix2: confined strict open — same root resolution as every other
    // session read; a symlinked ancestor or artifact yields None, never
    // foreign bytes.
    let mut file = crate::core::session_journal::confined_open(dir, file_name).ok()??;
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut chunk = [0u8; 16 * 1024];
    let mut cut: Option<usize> = None;
    while buf.len() <= MAX_HEADER {
        let n = file.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(KEY.len()).position(|w| w == KEY) {
            cut = Some(pos);
            break;
        }
    }

    let end = cut.unwrap_or(buf.len());
    let trimmed = String::from_utf8_lossy(&buf[..end]);
    let mut s = trimmed.trim_end().to_string();
    if s.ends_with(',') {
        s.pop();
    }
    if !s.ends_with('}') {
        s.push('}');
    }
    Some(s)
}

fn sessions_dir() -> PathBuf {
    crate::config::get_active_config_dir().join("sessions")
}

/// Remove a persisted session file (compaction-transition rollback) AND its
/// journal, when one exists (Task 35) — a session and its journal live and
/// die together. A missing file is not an error — rollback must be idempotent.
pub fn delete_session_file(id: &str) -> std::io::Result<()> {
    crate::core::session_journal::delete_session_files_in_dir(&sessions_dir(), id)
}

/// Validate a session or chain name: [a-z0-9-]{1,40}.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("name cannot be empty".into());
    }
    if name.len() > 40 {
        return Err(format!("invalid name '{}': must be 40 chars or less", name));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!(
            "invalid name '{}': allowed characters are lowercase letters, digits, and '-'",
            name
        ));
    }
    Ok(())
}

/// Find a session by its assigned name (not partial ID).
/// Iterates session headers one file at a time and returns on first match,
/// avoiding a full directory scan when the named session appears early.
pub fn find_session_by_name(name: &str) -> std::io::Result<Session> {
    let dir = sessions_dir();
    for entry in crate::core::session_journal::session_dir_entries(&dir)? {
        if !entry.name.ends_with(".json") {
            continue;
        }
        if let Some(info) = parse_session_header(&dir, &entry.name) {
            if info.name.as_deref() == Some(name) {
                return Session::load(&info.id);
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("no session named '{}'", name),
    ))
}

/// Resolve a query string to a Session. Resolution order:
/// 1. Chain name  2. Session name  3. Partial session ID
pub fn resolve_session(query: &str) -> std::io::Result<Session> {
    if let Ok(ptr) = crate::core::chain::load_chain(query) {
        match Session::load(&ptr.head) {
            Ok(s) => {
                tracing::info!("resolved '{}' via chain → session {}", query, ptr.head);
                return Ok(s);
            }
            Err(e) => {
                return Err(std::io::Error::new(
                    e.kind(),
                    format!(
                        "chain '{}' points to session '{}' which failed to load: {} (try /chain unname {})",
                        query, ptr.head, e, query
                    ),
                ));
            }
        }
    }
    if let Ok(s) = find_session_by_name(query) {
        tracing::info!("resolved '{}' via session name → {}", query, s.id);
        return Ok(s);
    }
    find_session(query)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Minimal typed provenance for constructor tests.
    fn test_record(parent: &Session) -> crate::compaction::CompactionRecord {
        crate::compaction::CompactionRecord {
            schema_version: crate::compaction::COMPACTION_SUMMARY_SCHEMA_VERSION,
            source_session: parent.id.clone(),
            source_message_count: parent.api_messages.len(),
            source_range_digest: crate::compaction::message_range_digest(&parent.api_messages),
            summary_provider: "anthropic".into(),
            summary_model: "claude-sonnet-4-6".into(),
            created_at: Utc::now(),
            prompt_stack_digest: crate::compaction::prompt_stack_digest(&["test"]),
            included_classes: crate::compaction::ContentClass::ALL.to_vec(),
            excluded_classes: Vec::new(),
            redaction_policy: crate::compaction::RedactionPolicy::TruncationOnly,
            prior_system_prompt: parent.system_prompt.clone(),
        }
    }

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
        session.api_messages.push(std::sync::Arc::new(json!({
            "role": "user",
            "content": "hello world"
        })));

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
        session_no_user
            .api_messages
            .push(std::sync::Arc::new(json!({
                "role": "assistant",
                "content": "response"
            })));
        session_no_user.auto_title();
        assert_eq!(session_no_user.title, "");

        // Test with long content (should truncate to 80 chars)
        let mut session_long = Session::new("gpt-4", "brief", None);
        let long_content = "a".repeat(100);
        session_long.api_messages.push(std::sync::Arc::new(json!({
            "role": "user",
            "content": long_content
        })));
        session_long.auto_title();
        assert_eq!(session_long.title.len(), 80);
        assert_eq!(session_long.title, "a".repeat(80));
    }

    #[test]
    fn test_session_info() {
        let mut session = Session::new("gpt-4", "brief", Some("system prompt"));

        // Add some messages to test message count
        session.api_messages.push(std::sync::Arc::new(json!({
            "role": "user",
            "content": "test message"
        })));
        session.api_messages.push(std::sync::Arc::new(json!({
            "role": "assistant",
            "content": "test response"
        })));

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
            name: None,
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
        let mut session = Session::new(
            "gpt-4-turbo",
            "detailed",
            Some("You are a helpful assistant"),
        );
        session.title = "Test Session".to_string();
        session.api_messages.push(std::sync::Arc::new(
            json!({"role": "user", "content": "test"}),
        ));
        session.total_input_tokens = 100;
        session.total_output_tokens = 200;
        session.session_cost = 0.15;

        // Serialize to JSON string
        let json_str = serde_json::to_string(&session).expect("Failed to serialize session");

        // Deserialize back from JSON string
        let deserialized: Session =
            serde_json::from_str(&json_str).expect("Failed to deserialize session");

        // Verify all fields match
        assert_eq!(deserialized.id, session.id);
        assert_eq!(deserialized.title, session.title);
        assert_eq!(deserialized.model, session.model);
        assert_eq!(deserialized.thinking_level, session.thinking_level);
        assert_eq!(deserialized.system_prompt, session.system_prompt);
        assert_eq!(deserialized.created_at, session.created_at);
        assert_eq!(deserialized.updated_at, session.updated_at);
        assert_eq!(deserialized.total_input_tokens, session.total_input_tokens);
        assert_eq!(
            deserialized.total_output_tokens,
            session.total_output_tokens
        );
        assert_eq!(deserialized.session_cost, session.session_cost);
        assert_eq!(deserialized.api_messages.len(), session.api_messages.len());
        assert_eq!(deserialized.api_messages[0], session.api_messages[0]);
    }

    #[test]
    fn codex_ultra_round_trips_and_compaction_preserves_logical_mode() {
        let parent = Session::new("openai-codex/gpt-5.6-sol", "ultra", None);
        let encoded = serde_json::to_string(&parent).expect("serialize Ultra session");
        let persisted: serde_json::Value =
            serde_json::from_str(&encoded).expect("inspect persisted session");
        assert_eq!(persisted["thinking_level"], "ultra");
        let restored: Session = serde_json::from_str(&encoded).expect("restore Ultra session");
        assert_eq!(restored.model, "openai-codex/gpt-5.6-sol");
        assert_eq!(restored.thinking_level, "ultra");

        let compacted =
            Session::from_compaction_record(&restored, "summary", test_record(&restored));
        assert_eq!(compacted.model, restored.model);
        assert_eq!(compacted.thinking_level, "ultra");
        assert_eq!(
            compacted.parent_session.as_deref(),
            Some(restored.id.as_str())
        );
    }

    #[test]
    fn codex_max_round_trips_without_becoming_ultra_or_xhigh() {
        let session = Session::new("openai-codex/gpt-5.6-luna", "max", None);
        let encoded = serde_json::to_string(&session).expect("serialize Max session");
        let restored: Session = serde_json::from_str(&encoded).expect("restore Max session");
        assert_eq!(restored.thinking_level, "max");
        assert_ne!(restored.thinking_level, "ultra");
        assert_ne!(restored.thinking_level, "xhigh");
    }

    #[test]
    fn test_session_serialization_preserves_all_fields() {
        let mut session = Session::new(
            "claude-3-opus",
            "comprehensive",
            Some("Custom system prompt"),
        );
        session.title = "Complex Session".to_string();

        // Add multiple messages
        session.api_messages.push(std::sync::Arc::new(
            json!({"role": "user", "content": "First message"}),
        ));
        session.api_messages.push(std::sync::Arc::new(
            json!({"role": "assistant", "content": "First response"}),
        ));
        session.api_messages.push(std::sync::Arc::new(
            json!({"role": "user", "content": "Second message"}),
        ));

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
        assert_eq!(
            restored.system_prompt.as_ref().unwrap(),
            "Custom system prompt"
        );
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
        session.api_messages.push(std::sync::Arc::new(
            json!({"role": "user", "content": "message 1"}),
        ));
        session.api_messages.push(std::sync::Arc::new(
            json!({"role": "assistant", "content": "response 1"}),
        ));
        session.api_messages.push(std::sync::Arc::new(
            json!({"role": "user", "content": "message 2"}),
        ));

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
        session.api_messages.push(std::sync::Arc::new(json!({
            "role": "user",
            "content": long_content
        })));

        session.auto_title();

        // Verify title is exactly 80 characters
        assert_eq!(session.title.len(), 80);
        assert_eq!(session.title, "a".repeat(80));
    }

    #[test]
    fn test_session_auto_title_skips_non_user_messages() {
        let mut session = Session::new("gpt-4", "brief", None);

        // Push only an assistant message (no user messages)
        session.api_messages.push(std::sync::Arc::new(json!({
            "role": "assistant",
            "content": "This should be ignored for auto title"
        })));

        session.auto_title();

        // Verify title stays empty since there are no user messages
        assert_eq!(session.title, "");

        // Test with system message too
        session.api_messages.push(std::sync::Arc::new(json!({
            "role": "system",
            "content": "System message should also be ignored"
        })));

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

        assert!(
            created_diff <= 2,
            "created_at should be within 2 seconds of now"
        );
        assert!(
            updated_diff <= 2,
            "updated_at should be within 2 seconds of now"
        );

        // Verify both timestamps are the same for new sessions
        assert_eq!(session.created_at, session.updated_at);

        // Verify timestamps are not in the future
        assert!(session.created_at <= after);
        assert!(session.updated_at <= after);
    }

    #[test]
    fn test_validate_name() {
        assert!(validate_name("work").is_ok());
        assert!(validate_name("my-project-2").is_ok());
        assert!(validate_name("a").is_ok());
        assert!(validate_name(&"a".repeat(40)).is_ok());

        assert!(validate_name("").is_err());
        assert!(validate_name(&"a".repeat(41)).is_err());
        assert!(validate_name("UPPER").is_err());
        assert!(validate_name("has space").is_err());
        assert!(validate_name("under_score").is_err());
        assert!(validate_name("dots.bad").is_err());

        let err = validate_name("Bad").unwrap_err();
        assert!(err.contains("Bad"));
        assert!(err.contains("lowercase") || err.contains("a-z") || err.contains("allowed"));
    }

    #[test]
    fn test_clear_name() {
        let mut s = Session::new("m", "brief", None);
        s.name = Some("foo".into());
        s.clear_name();
        assert_eq!(s.name, None);
    }

    #[test]
    fn ultracode_serialization_and_compaction_roundtrip_is_distinct() {
        let original = Session::new("anthropic/claude-fable-5", "ultracode", None);
        let json = serde_json::to_string(&original).unwrap();
        let restored: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.thinking_level, "ultracode");
        for other in ["ultra", "max", "xhigh"] {
            assert_ne!(restored.thinking_level, other);
        }
        let compacted =
            Session::from_compaction_record(&restored, "summary", test_record(&restored));
        let resumed: Session =
            serde_json::from_str(&serde_json::to_string(&compacted).unwrap()).unwrap();
        assert_eq!(resumed.thinking_level, "ultracode");
        assert_eq!(
            resumed.parent_session.as_deref(),
            Some(original.id.as_str())
        );
    }

    /// Private-mode tests (spec §5.4). Umask isolation: `#[serial(umask)]`
    /// serializes umask-mutating tests crate-wide; `UmaskGuard` restores the
    /// previous mask on drop (panic-safe). These exercise the blocking body
    /// of `Session::save` directly with a temp dir — no env mutation.
    #[cfg(unix)]
    mod private_modes {
        use super::*;
        use crate::core::private_fs::test_support::UmaskGuard;
        use serial_test::serial;
        use std::os::unix::fs::PermissionsExt;

        fn mode_of(path: &std::path::Path) -> u32 {
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777
        }

        #[test]
        #[serial(umask)]
        fn save_creates_0600_session_file_and_0700_dir_under_permissive_umask() {
            let _umask = UmaskGuard::set(0);
            let tmp = tempfile::TempDir::new().unwrap();
            let dir = tmp.path().join("sessions");
            save_json_in_dir(&dir, "sess-1", b"{}").unwrap();
            assert_eq!(mode_of(&dir), 0o700, "sessions dir must be 0700");
            assert_eq!(
                mode_of(&dir.join("sess-1.json")),
                0o600,
                "session file must be 0600"
            );
        }

        #[test]
        fn save_refuses_symlink_target() {
            let tmp = tempfile::TempDir::new().unwrap();
            let dir = tmp.path().join("sessions");
            std::fs::create_dir_all(&dir).unwrap();
            let victim = tmp.path().join("victim.json");
            std::fs::write(&victim, "original").unwrap();
            std::os::unix::fs::symlink(&victim, dir.join("sess-1.json")).unwrap();
            let res = save_json_in_dir(&dir, "sess-1", b"{}");
            assert!(res.is_err(), "save onto a symlink must fail");
            assert_eq!(
                std::fs::read_to_string(&victim).unwrap(),
                "original",
                "no bytes may be written through the planted symlink"
            );
        }
    }
}
