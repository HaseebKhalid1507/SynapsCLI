//! Generic active-task model for plugin-driven long-running work.
//!
//! Phase B Phase 3 — see
//! `docs/plans/2026-05-03-extension-contracts-for-rich-plugins.md`.
//!
//! Stored on `App` (or any container) as a `HashMap<String, TaskState>` so the
//! sticky progress UI can render N concurrent tasks generically without any
//! plugin-specific knowledge. This module is intentionally decoupled
//! from `App` so it can be unit-tested standalone.

use std::collections::HashMap;

use crate::extensions::tasks::{TaskEvent, TaskKind};

/// In-progress task aggregate updated from `task.start/update/log/done` events.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskState {
    pub id: String,
    pub label: String,
    pub kind: TaskKind,
    pub current: Option<u64>,
    pub total: Option<u64>,
    pub message: Option<String>,
    /// Most recent log line (for `rebuild`-style tasks). Bounded to N entries.
    pub recent_logs: Vec<String>,
    pub done: bool,
    pub error: Option<String>,
}

const MAX_RECENT_LOGS: usize = 8;

impl TaskState {
    pub fn new(id: String, label: String, kind: TaskKind) -> Self {
        Self {
            id,
            label,
            kind,
            current: None,
            total: None,
            message: None,
            recent_logs: Vec::new(),
            done: false,
            error: None,
        }
    }

    pub fn fraction(&self) -> Option<f32> {
        match (self.current, self.total) {
            (Some(c), Some(t)) if t > 0 => Some((c as f32 / t as f32).clamp(0.0, 1.0)),
            _ => None,
        }
    }
}

/// Generic task store keyed by task id. Drop-in for `App::active_tasks`.
#[derive(Debug, Default, Clone)]
pub struct ActiveTasks {
    map: HashMap<String, TaskState>,
    /// Insertion order of currently-tracked tasks. Used by render code so the
    /// sticky bar shows tasks in the order they were started.
    order: Vec<String>,
}

impl ActiveTasks {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn get(&self, id: &str) -> Option<&TaskState> {
        self.map.get(id)
    }

    /// Iterate tasks in insertion (start) order.
    pub fn iter(&self) -> impl Iterator<Item = &TaskState> {
        self.order.iter().filter_map(|id| self.map.get(id))
    }

    /// Apply a parsed `TaskEvent`. Idempotent for repeated `start` (label/kind
    /// updated). `done` keeps the task in the map so the UI can show a final
    /// state until callers explicitly `prune`.
    pub fn apply(&mut self, event: TaskEvent) {
        match event {
            TaskEvent::Start { id, label, kind } => {
                self.map
                    .entry(id.clone())
                    .and_modify(|s| {
                        s.label = label.clone();
                        s.kind = kind;
                        s.done = false;
                        s.error = None;
                    })
                    .or_insert_with(|| TaskState::new(id.clone(), label, kind));
                if !self.order.iter().any(|x| x == &id) {
                    self.order.push(id);
                }
            }
            TaskEvent::Update {
                id,
                current,
                total,
                message,
            } => {
                if let Some(s) = self.map.get_mut(&id) {
                    if current.is_some() {
                        s.current = current;
                    }
                    if total.is_some() {
                        s.total = total;
                    }
                    if message.is_some() {
                        s.message = message;
                    }
                }
            }
            TaskEvent::Log { id, line } => {
                if let Some(s) = self.map.get_mut(&id) {
                    s.recent_logs.push(line);
                    if s.recent_logs.len() > MAX_RECENT_LOGS {
                        let drop = s.recent_logs.len() - MAX_RECENT_LOGS;
                        s.recent_logs.drain(0..drop);
                    }
                }
            }
            TaskEvent::Done { id, error } => {
                if let Some(s) = self.map.get_mut(&id) {
                    s.done = true;
                    s.error = error;
                }
            }
        }
    }

    /// Remove a single task by id. Returns true if removed.
    pub fn prune(&mut self, id: &str) -> bool {
        let removed = self.map.remove(id).is_some();
        self.order.retain(|x| x != id);
        removed
    }

    /// Remove all tasks whose `done` flag is set.
    pub fn prune_completed(&mut self) {
        let to_drop: Vec<String> = self
            .order
            .iter()
            .filter(|id| self.map.get(*id).map(|s| s.done).unwrap_or(false))
            .cloned()
            .collect();
        for id in to_drop {
            self.prune(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev_start(id: &str, label: &str, kind: TaskKind) -> TaskEvent {
        TaskEvent::Start {
            id: id.into(),
            label: label.into(),
            kind,
        }
    }

    #[test]
    fn start_update_done_lifecycle() {
        let mut t = ActiveTasks::new();
        t.apply(ev_start("dl", "Downloading", TaskKind::Download));
        assert_eq!(t.len(), 1);
        let s = t.get("dl").unwrap();
        assert_eq!(s.label, "Downloading");
        assert_eq!(s.kind, TaskKind::Download);
        assert!(!s.done);

        t.apply(TaskEvent::Update {
            id: "dl".into(),
            current: Some(50),
            total: Some(100),
            message: Some("connecting".into()),
        });
        let s = t.get("dl").unwrap();
        assert_eq!(s.current, Some(50));
        assert_eq!(s.total, Some(100));
        assert_eq!(s.message.as_deref(), Some("connecting"));
        assert!((s.fraction().unwrap() - 0.5).abs() < 1e-6);

        t.apply(TaskEvent::Done {
            id: "dl".into(),
            error: None,
        });
        assert!(t.get("dl").unwrap().done);

        t.prune_completed();
        assert!(t.is_empty());
    }

    #[test]
    fn update_for_unknown_id_is_noop() {
        let mut t = ActiveTasks::new();
        t.apply(TaskEvent::Update {
            id: "nope".into(),
            current: Some(1),
            total: Some(2),
            message: None,
        });
        assert!(t.is_empty());
    }

    #[test]
    fn log_lines_are_bounded() {
        let mut t = ActiveTasks::new();
        t.apply(ev_start("rb", "Rebuilding", TaskKind::Rebuild));
        for i in 0..20 {
            t.apply(TaskEvent::Log {
                id: "rb".into(),
                line: format!("line {i}"),
            });
        }
        let s = t.get("rb").unwrap();
        assert_eq!(s.recent_logs.len(), MAX_RECENT_LOGS);
        assert_eq!(s.recent_logs.last().unwrap(), "line 19");
        assert_eq!(s.recent_logs.first().unwrap(), &format!("line {}", 20 - MAX_RECENT_LOGS));
    }

    #[test]
    fn iteration_preserves_start_order() {
        let mut t = ActiveTasks::new();
        t.apply(ev_start("a", "A", TaskKind::Generic));
        t.apply(ev_start("b", "B", TaskKind::Generic));
        t.apply(ev_start("c", "C", TaskKind::Generic));
        let labels: Vec<_> = t.iter().map(|s| s.label.clone()).collect();
        assert_eq!(labels, vec!["A", "B", "C"]);
    }

    #[test]
    fn restart_resets_done_and_error() {
        let mut t = ActiveTasks::new();
        t.apply(ev_start("x", "X", TaskKind::Generic));
        t.apply(TaskEvent::Done {
            id: "x".into(),
            error: Some("boom".into()),
        });
        assert!(t.get("x").unwrap().done);
        // Re-start with same id resets state.
        t.apply(ev_start("x", "X2", TaskKind::Generic));
        let s = t.get("x").unwrap();
        assert!(!s.done);
        assert!(s.error.is_none());
        assert_eq!(s.label, "X2");
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn fraction_handles_zero_and_missing_total() {
        let s = TaskState::new("a".into(), "a".into(), TaskKind::Generic);
        assert!(s.fraction().is_none());
        let s2 = TaskState {
            current: Some(5),
            total: Some(0),
            ..s.clone()
        };
        assert!(s2.fraction().is_none());
    }
}
