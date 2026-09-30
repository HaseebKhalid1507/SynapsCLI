//! The in-flight turn sidecar: `sessions/<id>.turn`.
//!
//! The session snapshot is saved at every round boundary (the engine's round
//! checkpoints), so it always holds a VALID history. What it cannot hold is
//! the response still being streamed — partial text is not a valid history
//! entry until the turn stops. This small sidecar carries it instead:
//!
//! - written when a turn starts (the "turn is open" signal), then at most
//!   once a second while text streams, and removed when the turn ends;
//! - `base_len` = how many messages of the saved history the in-flight
//!   response follows, so a draft that outlived its round is recognised as
//!   stale;
//! - `partial_text` = the text the model has streamed so far in that
//!   response (text only: unsigned thinking and unfinished tool calls are
//!   never replayable).
//!
//! A sidecar found when a session is loaded means the process died with a
//! turn open; the loader folds it into history as a real assistant message
//! plus an interruption marker (`agent_engine::engine::interrupt`).
//!
//! Separate from the snapshot on purpose: O(partial text) bytes per write in
//! every persistence mode, and the snapshot bytes stay exactly those of the
//! last completed round. Same confined, private (0600), atomic writes as the
//! snapshot. The `.turn` name keeps the session id as the file stem (retention
//! pairs artifacts on the stem) and is invisible to `*.json` listings.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// Upper bound on persisted partial text (a response is bounded by the
/// model's max output tokens; this only guards pathological streams).
pub const TURN_DRAFT_MAX_TEXT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnDraft {
    /// Messages in the saved history the in-flight response continues from.
    pub base_len: usize,
    /// Text streamed so far in the in-flight response.
    #[serde(default)]
    pub partial_text: String,
}

fn artifact(id: &str) -> String {
    format!("{id}.turn")
}

/// Read the sidecar for `id`. `Ok(None)` when absent (the normal case).
pub fn read_turn_draft(dir: &Path, id: &str) -> std::io::Result<Option<TurnDraft>> {
    let Some(bytes) = read_artifact(dir, &artifact(id))? else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Atomically (re)write the sidecar for `id` (0600, confined).
pub fn write_turn_draft(dir: &Path, id: &str, draft: &TurnDraft) -> std::io::Result<()> {
    let mut draft = draft.clone();
    truncate_on_char_boundary(&mut draft.partial_text, TURN_DRAFT_MAX_TEXT_BYTES);
    let bytes = serde_json::to_vec(&draft).map_err(std::io::Error::other)?;
    write_artifact(dir, &artifact(id), &bytes)
}

/// Remove the sidecar for `id`. Idempotent.
pub fn remove_turn_draft(dir: &Path, id: &str) -> std::io::Result<()> {
    match remove_artifact(dir, &artifact(id)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn truncate_on_char_boundary(s: &mut String, max: usize) {
    if s.len() > max {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
}

/// Non-blocking, per-session ORDERED writer for the sidecar.
///
/// Each operation is stamped with a per-id sequence number at the call site
/// (program order) and applied on the blocking pool under one lock; an
/// operation older than the last applied one for the same id is skipped. So
/// the file always ends in the state of the LAST call, even when blocking
/// tasks run out of order — a late write can never resurrect a sidecar that
/// a later `remove` deleted. Never blocks the caller (the session actor's
/// turn machine must stay responsive to Esc on a slow disk).
pub struct TurnDraftWriter {
    dir: PathBuf,
    next_seq: HashMap<String, u64>,
    applied: Arc<Mutex<HashMap<String, u64>>>,
}

impl TurnDraftWriter {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            next_seq: HashMap::new(),
            applied: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn write(&mut self, id: &str, draft: TurnDraft) {
        self.submit(id, Some(draft));
    }

    pub fn remove(&mut self, id: &str) {
        self.submit(id, None);
    }

    fn submit(&mut self, id: &str, op: Option<TurnDraft>) {
        let seq = {
            let n = self.next_seq.entry(id.to_string()).or_insert(0);
            *n += 1;
            *n
        };
        let (dir, id, applied) = (self.dir.clone(), id.to_string(), Arc::clone(&self.applied));
        let apply = move || {
            let mut applied = applied
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let last = applied.entry(id.clone()).or_insert(0);
            if seq <= *last {
                return; // superseded by a later operation already applied
            }
            *last = seq;
            let result = match &op {
                Some(draft) => write_turn_draft(&dir, &id, draft),
                None => remove_turn_draft(&dir, &id),
            };
            if let Err(e) = result {
                tracing::warn!(session = %id, "turn draft {}: {e}", if op.is_some() { "write" } else { "remove" });
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn_blocking(apply);
            }
            Err(_) => apply(), // no runtime (sync callers/tests): apply inline
        }
    }
}

// ── confined I/O (mirrors session_journal's handle-relative artifacts) ──────

#[cfg(unix)]
fn read_artifact(dir: &Path, name: &str) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read;
    let handle = match crate::core::private_fs::ConfinedDir::open_absolute_no_symlinks(dir) {
        Ok(h) => h,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut file = match handle.open_file(&[name.to_string()]) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

#[cfg(unix)]
fn write_artifact(dir: &Path, name: &str, data: &[u8]) -> std::io::Result<()> {
    crate::core::private_fs::ConfinedDir::create_absolute_no_symlinks(dir)?.write_atomic(name, data)
}

#[cfg(unix)]
fn remove_artifact(dir: &Path, name: &str) -> std::io::Result<()> {
    match crate::core::private_fs::ConfinedDir::open_absolute_no_symlinks(dir) {
        Ok(handle) => handle.remove_file(name),
        Err(e) => Err(e),
    }
}

#[cfg(not(unix))]
fn read_artifact(dir: &Path, name: &str) -> std::io::Result<Option<Vec<u8>>> {
    let path = dir.join(name);
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Ok(meta) if meta.file_type().is_symlink() => Err(std::io::Error::other(format!(
            "refusing symlinked session artifact {name:?}"
        ))),
        Err(e) => Err(e),
        Ok(_) => std::fs::read(&path).map(Some),
    }
}

#[cfg(not(unix))]
fn write_artifact(dir: &Path, name: &str, data: &[u8]) -> std::io::Result<()> {
    crate::core::private_fs::ensure_private_dir(dir).map_err(std::io::Error::other)?;
    crate::core::private_fs::write_atomic_private(&dir.join(name), data)
        .map_err(std::io::Error::other)
}

#[cfg(not(unix))]
fn remove_artifact(dir: &Path, name: &str) -> std::io::Result<()> {
    std::fs::remove_file(dir.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn draft(base_len: usize, text: &str) -> TurnDraft {
        TurnDraft {
            base_len,
            partial_text: text.into(),
        }
    }

    #[test]
    fn roundtrip_absent_and_idempotent_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        assert_eq!(read_turn_draft(&dir, "s1").unwrap(), None, "no dir yet");
        write_turn_draft(&dir, "s1", &draft(3, "partial")).unwrap();
        assert_eq!(
            read_turn_draft(&dir, "s1").unwrap(),
            Some(draft(3, "partial"))
        );
        assert!(dir.join("s1.turn").exists());
        remove_turn_draft(&dir, "s1").unwrap();
        remove_turn_draft(&dir, "s1").unwrap();
        assert_eq!(read_turn_draft(&dir, "s1").unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn sidecar_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        write_turn_draft(&dir, "s1", &draft(0, "x")).unwrap();
        let mode = std::fs::metadata(dir.join("s1.turn"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn oversized_text_is_truncated_on_a_char_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        let big = "é".repeat(TURN_DRAFT_MAX_TEXT_BYTES); // 2 bytes each
        write_turn_draft(&dir, "s1", &draft(1, &big)).unwrap();
        let back = read_turn_draft(&dir, "s1").unwrap().unwrap();
        assert!(back.partial_text.len() <= TURN_DRAFT_MAX_TEXT_BYTES);
        assert!(back.partial_text.chars().all(|c| c == 'é'));
    }

    #[test]
    fn corrupt_sidecar_is_an_error_not_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        write_turn_draft(&dir, "s1", &draft(0, "")).unwrap();
        std::fs::write(dir.join("s1.turn"), b"{not json").unwrap();
        assert!(read_turn_draft(&dir, "s1").is_err());
    }

    async fn settle<F: Fn() -> bool>(cond: F) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("writer never settled");
    }

    #[tokio::test]
    async fn writer_ends_in_the_state_of_the_last_call() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        let mut w = TurnDraftWriter::new(dir.clone());
        for i in 0..50 {
            w.write("s1", draft(i, "streaming"));
        }
        w.remove("s1");
        // Give every blocking op time to run, in whatever order.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            read_turn_draft(&dir, "s1").unwrap(),
            None,
            "a late write resurrected it"
        );

        w.write("s1", draft(7, "next turn"));
        let d = dir.clone();
        settle(move || read_turn_draft(&d, "s1").unwrap().is_some()).await;
        assert_eq!(
            read_turn_draft(&dir, "s1").unwrap(),
            Some(draft(7, "next turn"))
        );
    }

    #[tokio::test]
    async fn writer_orders_per_session_independently() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        let mut w = TurnDraftWriter::new(dir.clone());
        w.remove("old"); // removing one session's sidecar …
        w.write("new", draft(2, "x")); // … must not suppress another's write
        let d = dir.clone();
        settle(move || read_turn_draft(&d, "new").unwrap().is_some()).await;
    }
}
