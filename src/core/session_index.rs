use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SessionIndexEventKind {
    Start,
    End,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionIndexRecord {
    pub schema_version: u8,
    pub session_id: String,
    pub event: SessionIndexEventKind,
    pub timestamp: DateTime<Utc>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turns: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl SessionIndexRecord {
    pub fn start(session_id: impl Into<String>) -> Self {
        Self::new(session_id, SessionIndexEventKind::Start)
    }

    pub fn end(session_id: impl Into<String>) -> Self {
        Self::new(session_id, SessionIndexEventKind::End)
    }

    fn new(session_id: impl Into<String>, event: SessionIndexEventKind) -> Self {
        Self {
            schema_version: 1,
            session_id: session_id.into(),
            event,
            timestamp: Utc::now(),
            cwd: None,
            profile: None,
            model: None,
            duration_ms: None,
            turns: None,
            tags: Vec::new(),
            note: None,
        }
    }
}

pub fn index_path() -> PathBuf {
    crate::core::config::base_dir().join("sessions").join("index.jsonl")
}

pub fn append_record(record: &SessionIndexRecord) -> crate::Result<()> {
    append_record_to_path(&index_path(), record)
}

pub fn read_recent(limit: usize) -> crate::Result<Vec<SessionIndexRecord>> {
    read_recent_from_path(&index_path(), limit)
}

fn append_record_to_path(path: &std::path::Path, record: &SessionIndexRecord) -> crate::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| crate::core::error::RuntimeError::Session(format!("create session index directory: {err}")))?;
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|err| crate::core::error::RuntimeError::Session(format!("open session index: {err}")))?;
    serde_json::to_writer(&mut file, record)
        .map_err(|err| crate::core::error::RuntimeError::Session(format!("serialize session index record: {err}")))?;
    use std::io::Write;
    file.write_all(b"\n")
        .map_err(|err| crate::core::error::RuntimeError::Session(format!("write session index record: {err}")))?;
    Ok(())
}

fn read_recent_from_path(path: &std::path::Path, limit: usize) -> crate::Result<Vec<SessionIndexRecord>> {
    if limit == 0 || !path.exists() {
        return Ok(Vec::new());
    }

    let contents = std::fs::read_to_string(path)
        .map_err(|err| crate::core::error::RuntimeError::Session(format!("read session index: {err}")))?;
    let mut records = Vec::new();
    for line in contents.lines().rev().take(limit) {
        if line.trim().is_empty() {
            continue;
        }
        records.push(
            serde_json::from_str::<SessionIndexRecord>(line)
                .map_err(|err| crate::core::error::RuntimeError::Session(format!("parse session index record: {err}")))?,
        );
    }
    records.reverse();
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        old_base_dir: Option<String>,
    }

    impl EnvGuard {
        fn set_base_dir(path: &std::path::Path) -> Self {
            let old_base_dir = std::env::var("SYNAPS_BASE_DIR").ok();
            std::env::set_var("SYNAPS_BASE_DIR", path);
            Self { old_base_dir }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(value) = self.old_base_dir.take() {
                std::env::set_var("SYNAPS_BASE_DIR", value);
            } else {
                std::env::remove_var("SYNAPS_BASE_DIR");
            }
        }
    }

    fn temp_base_dir(test_name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "synaps-session-index-{test_name}-{}",
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn append_record_creates_jsonl_under_base_dir() {
        let _lock = ENV_LOCK.lock().unwrap();
        let base = temp_base_dir("creates-jsonl");
        let _guard = EnvGuard::set_base_dir(&base);

        let record = SessionIndexRecord::start("sess-1");
        append_record(&record).unwrap();

        let path = base.join("sessions").join("index.jsonl");
        assert!(path.exists());
        let contents = std::fs::read_to_string(path).unwrap();
        let line: Value = serde_json::from_str(contents.trim()).unwrap();
        assert_eq!(line["schema_version"], 1);
        assert_eq!(line["session_id"], "sess-1");
        assert_eq!(line["event"], "start");
        assert!(line.get("timestamp").is_some());
        assert!(line.get("cwd").is_none());
    }

    #[test]
    fn append_start_and_end_are_valid_json_lines() {
        let _lock = ENV_LOCK.lock().unwrap();
        let base = temp_base_dir("start-end-lines");
        let _guard = EnvGuard::set_base_dir(&base);

        append_record(&SessionIndexRecord::start("sess-1")).unwrap();
        append_record(&SessionIndexRecord::end("sess-1")).unwrap();

        let contents = std::fs::read_to_string(index_path()).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(serde_json::from_str::<Value>(lines[0]).unwrap()["event"], "start");
        assert_eq!(serde_json::from_str::<Value>(lines[1]).unwrap()["event"], "end");
    }

    #[test]
    fn read_recent_returns_newest_records_in_chronological_order() {
        let _lock = ENV_LOCK.lock().unwrap();
        let base = temp_base_dir("read-recent");
        let _guard = EnvGuard::set_base_dir(&base);

        append_record(&SessionIndexRecord::start("sess-1")).unwrap();
        append_record(&SessionIndexRecord::start("sess-2")).unwrap();
        append_record(&SessionIndexRecord::end("sess-2")).unwrap();

        let records = read_recent(2).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].session_id, "sess-2");
        assert_eq!(records[0].event, SessionIndexEventKind::Start);
        assert_eq!(records[1].session_id, "sess-2");
        assert_eq!(records[1].event, SessionIndexEventKind::End);
    }

    #[test]
    fn read_recent_missing_index_returns_empty() {
        let _lock = ENV_LOCK.lock().unwrap();
        let base = temp_base_dir("missing-index");
        let _guard = EnvGuard::set_base_dir(&base);

        assert!(read_recent(10).unwrap().is_empty());
    }

    #[test]
    fn read_recent_limit_zero_returns_empty() {
        let _lock = ENV_LOCK.lock().unwrap();
        let base = temp_base_dir("limit-zero");
        let _guard = EnvGuard::set_base_dir(&base);

        append_record(&SessionIndexRecord::start("sess-1")).unwrap();

        assert!(read_recent(0).unwrap().is_empty());
    }
}
