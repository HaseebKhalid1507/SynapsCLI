//! The in-flight turn draft: `sessions/<id>.turn`.
//!
//! The session snapshot is saved at every round boundary (the engine's round
//! checkpoints), so it always holds a VALID history. What it cannot hold is
//! the response still being streamed — partial text is not a valid history
//! entry until the turn stops. This small draft carries it instead:
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
//! Only the holder of the session lock writes a draft, and only the holder
//! reads one back: a draft found when the holder loads the session means the
//! previous holder died with a turn open, and it is folded into history as a
//! real assistant message plus an interruption marker
//! (`agent_engine::engine::interrupt`). The session actor's background
//! writer (`agent_engine::session::persister`) applies draft writes and
//! removals in order with the snapshot saves: the draft is removed only
//! after the history that ends its turn is saved.
//!
//! Separate from the snapshot on purpose: O(partial text) bytes per write in
//! every persistence mode, and the snapshot bytes stay exactly those of the
//! last completed round. Same confined, private (0600), atomic writes as the
//! snapshot. The `.turn` name keeps the session id as the file stem and is
//! invisible to `*.json` listings; deleting a session (and retention)
//! removes its draft with it.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Upper bound on persisted partial text (a response is bounded by the
/// model's max output tokens; this only guards pathological streams).
pub const TURN_DRAFT_MAX_TEXT_BYTES: usize = 1024 * 1024;

/// Upper bound on a draft file read at load: the capped text JSON-escaped
/// in the worst case (`\u00XX` = 6 bytes per input byte) plus framing. A
/// larger file was not written by Synaps and is rejected unread.
pub const TURN_DRAFT_MAX_FILE_BYTES: u64 = 6 * TURN_DRAFT_MAX_TEXT_BYTES as u64 + 4096;

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

/// Read the draft for `id`. `Ok(None)` when absent (the normal case); an
/// `InvalidData` error for an oversized (`TURN_DRAFT_MAX_FILE_BYTES`) or
/// malformed file.
pub fn read_turn_draft(dir: &Path, id: &str) -> std::io::Result<Option<TurnDraft>> {
    let Some(bytes) = read_artifact(dir, &artifact(id))? else {
        return Ok(None);
    };
    if bytes.len() as u64 > TURN_DRAFT_MAX_FILE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "turn draft exceeds its size limit",
        ));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Atomically (re)write the draft for `id` (0600, confined).
pub fn write_turn_draft(dir: &Path, id: &str, draft: &TurnDraft) -> std::io::Result<()> {
    let mut draft = draft.clone();
    truncate_on_char_boundary(&mut draft.partial_text, TURN_DRAFT_MAX_TEXT_BYTES);
    let bytes = serde_json::to_vec(&draft).map_err(std::io::Error::other)?;
    write_artifact(dir, &artifact(id), &bytes)
}

/// Remove the draft for `id`. Idempotent.
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

// ── confined I/O (mirrors session_journal's handle-relative artifacts) ──────

#[cfg(unix)]
fn read_artifact(dir: &Path, name: &str) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read;
    let handle = match crate::core::private_fs::ConfinedDir::open_absolute_no_symlinks(dir) {
        Ok(h) => h,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let file = match handle.open_file(&[name.to_string()]) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    // Read at most one byte past the limit: enough to reject, never more.
    let mut bytes = Vec::new();
    file.take(TURN_DRAFT_MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
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
        Ok(_) => {
            use std::io::Read;
            let mut bytes = Vec::new();
            std::fs::File::open(&path)?
                .take(TURN_DRAFT_MAX_FILE_BYTES + 1)
                .read_to_end(&mut bytes)?;
            Ok(Some(bytes))
        }
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
    fn draft_file_is_private() {
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
    fn corrupt_draft_is_an_error_not_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        write_turn_draft(&dir, "s1", &draft(0, "")).unwrap();
        std::fs::write(dir.join("s1.turn"), b"{not json").unwrap();
        assert!(read_turn_draft(&dir, "s1").is_err());
    }

    /// The largest draft Synaps can write reads back; anything bigger is
    /// rejected without reading it whole.
    #[test]
    fn draft_reads_are_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        // Worst-case escaping: every byte a control character.
        let worst = "\u{1}".repeat(TURN_DRAFT_MAX_TEXT_BYTES);
        write_turn_draft(&dir, "s1", &draft(0, &worst)).unwrap();
        assert!(std::fs::metadata(dir.join("s1.turn")).unwrap().len() <= TURN_DRAFT_MAX_FILE_BYTES);
        assert_eq!(read_turn_draft(&dir, "s1").unwrap().unwrap().partial_text, worst);

        let huge = vec![b' '; TURN_DRAFT_MAX_FILE_BYTES as usize + 1];
        std::fs::write(dir.join("s2.turn"), huge).unwrap();
        let err = read_turn_draft(&dir, "s2").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
