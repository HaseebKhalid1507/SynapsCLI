//! Plugin long-running task notification types and parser.
//!
//! Phase B Phase 3 contract — see
//! `docs/plans/2026-05-03-extension-contracts-for-rich-plugins.md`.
//!
//! Plugins push spontaneous JSON-RPC notifications for long-running tasks
//! (downloads, rebuilds, indexing) outside the slash-command request/response
//! cycle. Method names: `task.start`, `task.update`, `task.log`, `task.done`.
//!
//! Wire shapes (params per method):
//!
//! - `task.start`:  `{ id, label, kind: "download"|"rebuild"|"generic" }`
//! - `task.update`: `{ id, current?: u64, total?: u64, message?: string }`
//! - `task.log`:    `{ id, line }`
//! - `task.done`:   `{ id, error?: string|null }`

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Coarse classification of a task; affects how it's rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Download,
    Rebuild,
    Generic,
}

impl Default for TaskKind {
    fn default() -> Self {
        TaskKind::Generic
    }
}

/// Parsed task notification.
#[derive(Debug, Clone, PartialEq)]
pub enum TaskEvent {
    Start {
        id: String,
        label: String,
        kind: TaskKind,
    },
    Update {
        id: String,
        current: Option<u64>,
        total: Option<u64>,
        message: Option<String>,
    },
    Log {
        id: String,
        line: String,
    },
    Done {
        id: String,
        error: Option<String>,
    },
}

impl TaskEvent {
    pub fn id(&self) -> &str {
        match self {
            TaskEvent::Start { id, .. }
            | TaskEvent::Update { id, .. }
            | TaskEvent::Log { id, .. }
            | TaskEvent::Done { id, .. } => id,
        }
    }
}

/// Returns true if `method` is one of the recognised task notifications.
pub fn is_task_method(method: &str) -> bool {
    matches!(method, "task.start" | "task.update" | "task.log" | "task.done")
}

/// Parse a `task.*` notification given the JSON-RPC method and params.
pub fn parse_task_event(method: &str, params: &Value) -> Result<TaskEvent, String> {
    let obj = params
        .as_object()
        .ok_or_else(|| format!("{method} params must be a JSON object"))?;
    let id = obj
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{method} missing 'id'"))?
        .to_string();
    if id.is_empty() {
        return Err(format!("{method} 'id' must be non-empty"));
    }

    match method {
        "task.start" => {
            let label = obj
                .get("label")
                .and_then(Value::as_str)
                .ok_or_else(|| "task.start missing 'label'".to_string())?
                .to_string();
            let kind = match obj.get("kind").and_then(Value::as_str) {
                None => TaskKind::Generic,
                Some("download") => TaskKind::Download,
                Some("rebuild") => TaskKind::Rebuild,
                Some("generic") | Some("other") => TaskKind::Generic,
                Some(other) => return Err(format!("task.start unknown kind '{other}'")),
            };
            Ok(TaskEvent::Start { id, label, kind })
        }
        "task.update" => {
            let current = obj.get("current").and_then(Value::as_u64);
            let total = obj.get("total").and_then(Value::as_u64);
            let message = obj
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string);
            Ok(TaskEvent::Update {
                id,
                current,
                total,
                message,
            })
        }
        "task.log" => {
            let line = obj
                .get("line")
                .and_then(Value::as_str)
                .ok_or_else(|| "task.log missing 'line'".to_string())?
                .to_string();
            Ok(TaskEvent::Log { id, line })
        }
        "task.done" => {
            let error = match obj.get("error") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => {
                    if s.is_empty() {
                        None
                    } else {
                        Some(s.clone())
                    }
                }
                Some(other) => {
                    return Err(format!("task.done 'error' must be string or null, got {other}"));
                }
            };
            Ok(TaskEvent::Done { id, error })
        }
        other => Err(format!("not a task method: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_task_start_with_kind() {
        let ev = parse_task_event(
            "task.start",
            &json!({"id":"dl","label":"Downloading","kind":"download"}),
        )
        .unwrap();
        assert_eq!(
            ev,
            TaskEvent::Start {
                id: "dl".into(),
                label: "Downloading".into(),
                kind: TaskKind::Download
            }
        );
    }

    #[test]
    fn task_start_defaults_to_generic_kind() {
        let ev = parse_task_event("task.start", &json!({"id":"x","label":"y"})).unwrap();
        assert!(matches!(
            ev,
            TaskEvent::Start { kind: TaskKind::Generic, .. }
        ));
    }

    #[test]
    fn parses_task_update_partial() {
        let ev = parse_task_event(
            "task.update",
            &json!({"id":"dl","current":50,"total":100}),
        )
        .unwrap();
        assert_eq!(
            ev,
            TaskEvent::Update {
                id: "dl".into(),
                current: Some(50),
                total: Some(100),
                message: None
            }
        );
    }

    #[test]
    fn parses_task_log() {
        let ev = parse_task_event("task.log", &json!({"id":"r","line":"compiling..."})).unwrap();
        assert_eq!(
            ev,
            TaskEvent::Log { id: "r".into(), line: "compiling...".into() }
        );
    }

    #[test]
    fn parses_task_done_no_error() {
        let ev = parse_task_event("task.done", &json!({"id":"r"})).unwrap();
        assert_eq!(ev, TaskEvent::Done { id: "r".into(), error: None });
    }

    #[test]
    fn parses_task_done_with_error() {
        let ev = parse_task_event("task.done", &json!({"id":"r","error":"boom"})).unwrap();
        assert_eq!(
            ev,
            TaskEvent::Done {
                id: "r".into(),
                error: Some("boom".into())
            }
        );
    }

    #[test]
    fn rejects_missing_id() {
        assert!(parse_task_event("task.start", &json!({"label":"x"})).is_err());
    }

    #[test]
    fn rejects_unknown_kind() {
        let err = parse_task_event(
            "task.start",
            &json!({"id":"x","label":"y","kind":"alien"}),
        )
        .unwrap_err();
        assert!(err.contains("unknown kind"));
    }

    #[test]
    fn is_task_method_works() {
        assert!(is_task_method("task.start"));
        assert!(is_task_method("task.update"));
        assert!(is_task_method("task.log"));
        assert!(is_task_method("task.done"));
        assert!(!is_task_method("task.unknown"));
        assert!(!is_task_method("provider.stream.event"));
    }
}
