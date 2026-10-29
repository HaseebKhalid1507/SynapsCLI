//! Tool-output spill files: where they live and how they go away.
//!
//! A tool result bigger than `tool_output_spill_bytes` is stored whole in
//! `<base>/spill/<session>/<tool_use_id>.txt`, and model history carries a
//! short head + tail preview that points at the file (the engine's
//! `runtime::tool_spill`). `<session>` is the persisted conversation id for
//! a foreground session and the runtime trace session id for a worker.
//!
//! Spill files are a cache of output the model already saw a preview of, so
//! they are disposable. They go away two ways:
//!
//! - with their session: retention ([`crate::retention`]) removes
//!   `<base>/spill/<id>/` when it deletes or forgets session `<id>`;
//! - by age: [`sweep_stale_spill`] runs at startup, like the trace
//!   content-capture sweep, and removes session dirs nothing has written to
//!   for [`SPILL_MAX_AGE`]. Worker dirs have no session file, so this is
//!   what removes them.
//!
//! Every directory is `0700` and every file `0600` (spec §5.4).

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Name of the spill root under the Synaps base dir.
pub const SPILL_DIR_NAME: &str = "spill";

/// A session spill dir nothing has written to for this long is removed by
/// [`sweep_stale_spill`]. A week covers a session resumed after a weekend;
/// past that the preview in history is what remains (the pointer goes
/// dead, and a `read` of it says so).
pub const SPILL_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Bound on entries one sweep inspects, so a planted directory with
/// millions of entries cannot stall startup (mirrors the capture sweep).
const MAX_SWEEP_ENTRIES: usize = 4096;

/// Longest session id accepted as a spill dir name.
const MAX_COMPONENT_BYTES: usize = 128;

/// `<base>/spill`, resolved through the base-dir helper
/// ([`crate::config::base_dir`], which honours `SYNAPS_BASE_DIR`).
/// Callers bind this ONCE at construction, like the capture dir.
pub fn default_spill_root() -> PathBuf {
    spill_root(&crate::config::base_dir())
}

/// `<base>/spill` for an explicit base dir.
pub fn spill_root(base: &Path) -> PathBuf {
    base.join(SPILL_DIR_NAME)
}

/// `id` when it is safe as ONE path component: 1-128 bytes of
/// `[A-Za-z0-9._-]`, and not `.` or `..`. Session ids
/// (`20261029-143000-ab12`) and runtime trace ids (`session-<pid>-<ms>-<seq>`)
/// both qualify; anything else is refused rather than rewritten, so two
/// different ids can never share a directory.
pub fn session_component(id: &str) -> Option<&str> {
    let ok = !id.is_empty()
        && id.len() <= MAX_COMPONENT_BYTES
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    ok.then_some(id)
}

/// What a removal freed: spill-root entries (session dirs, strays), the
/// regular files inside them, and their bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpillRemoval {
    pub entries: usize,
    pub files: usize,
    pub bytes: u64,
}

impl SpillRemoval {
    fn add(&mut self, other: SpillRemoval) {
        self.entries += other.entries;
        self.files += other.files;
        self.bytes += other.bytes;
    }
}

/// Remove session `session_id`'s spill dir under `root`. Idempotent: a
/// missing dir is `Ok` with nothing freed. An id that is not a safe path
/// component is a no-op (nothing could have been written for it). A
/// symlink planted at the dir's path is removed as a link, never followed.
pub fn remove_session_spill(root: &Path, session_id: &str) -> io::Result<SpillRemoval> {
    let Some(component) = session_component(session_id) else {
        return Ok(SpillRemoval::default());
    };
    remove_entry(&root.join(component))
}

/// Remove one entry of the spill root: a dir with everything in it, or a
/// stray file/symlink (removed as itself). Counts regular files and bytes.
fn remove_entry(path: &Path) -> io::Result<SpillRemoval> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(SpillRemoval::default()),
        Err(e) => return Err(e),
    };
    if !meta.is_dir() {
        // A symlink or a stray file: never followed, removed as itself.
        let (files, bytes) = if meta.is_file() { (1, meta.len()) } else { (0, 0) };
        std::fs::remove_file(path)?;
        return Ok(SpillRemoval { entries: 1, files, bytes });
    }
    let mut freed = SpillRemoval { entries: 1, ..Default::default() };
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    freed.files += 1;
                    freed.bytes += meta.len();
                }
            }
        }
    }
    // `remove_dir_all` does not follow symlinks inside the tree.
    match std::fs::remove_dir_all(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        other => other?,
    }
    Ok(freed)
}

/// Startup sweep: remove every session dir under `root` whose last write
/// (dir mtime: creating a spill file updates it) is older than `max_age`
/// at `now`, plus anything that is not a directory (nothing else lives
/// here). Bounded, fail-soft and confined to `root`'s own entries. A
/// missing root is a no-op.
pub fn sweep_stale_spill(root: &Path, max_age: Duration, now: SystemTime) -> SpillRemoval {
    let mut removed = SpillRemoval::default();
    let Ok(entries) = std::fs::read_dir(root) else {
        return removed;
    };
    for entry in entries.flatten().take(MAX_SWEEP_ENTRIES) {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let stale = if meta.is_dir() {
            meta.modified()
                .ok()
                .and_then(|m| now.duration_since(m).ok())
                .is_some_and(|age| age > max_age)
        } else {
            true
        };
        if stale {
            if let Ok(freed) = remove_entry(&path) {
                removed.add(freed);
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    fn spill_file(root: &Path, session: &str, name: &str, body: &str) {
        let dir = root.join(session);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn session_component_accepts_session_and_trace_ids_only() {
        assert_eq!(session_component("20261029-143000-ab12"), Some("20261029-143000-ab12"));
        assert_eq!(session_component("session-123-1700000000000-4"), Some("session-123-1700000000000-4"));
        for bad in ["", ".", "..", "a/b", "a\\b", "../x", "a b", "a:b", &"x".repeat(129)] {
            assert_eq!(session_component(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn remove_session_spill_removes_the_dir_and_counts_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = spill_root(tmp.path());
        spill_file(&root, "s1", "toolu_a.txt", "12345");
        spill_file(&root, "s1", "toolu_b.txt", "123");
        spill_file(&root, "s2", "toolu_c.txt", "keep");
        let freed = remove_session_spill(&root, "s1").unwrap();
        assert_eq!(freed, SpillRemoval { entries: 1, files: 2, bytes: 8 });
        assert!(!root.join("s1").exists());
        assert!(root.join("s2/toolu_c.txt").exists(), "other sessions untouched");
        // Idempotent, and unsafe ids are a no-op.
        assert_eq!(remove_session_spill(&root, "s1").unwrap(), SpillRemoval::default());
        assert_eq!(remove_session_spill(&root, "..").unwrap(), SpillRemoval::default());
        assert!(root.exists());
    }

    #[cfg(unix)]
    #[test]
    fn remove_session_spill_never_follows_a_planted_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let root = spill_root(tmp.path());
        let victim = tmp.path().join("victim");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("precious"), "x").unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&victim, root.join("s1")).unwrap();
        remove_session_spill(&root, "s1").unwrap();
        assert!(!root.join("s1").exists(), "the link itself is removed");
        assert!(victim.join("precious").exists(), "its target is untouched");
    }

    #[test]
    fn sweep_removes_only_stale_dirs_and_stray_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = spill_root(tmp.path());
        spill_file(&root, "old", "toolu_a.txt", "a");
        std::fs::write(root.join("stray.txt"), "x").unwrap();
        let now = SystemTime::now();
        // Fresh: nothing but the stray file goes.
        let swept = sweep_stale_spill(&root, SPILL_MAX_AGE, now);
        assert_eq!(swept, SpillRemoval { entries: 1, files: 1, bytes: 1 });
        assert!(root.join("old/toolu_a.txt").exists());
        // Eight days on, the dir is stale.
        let swept = sweep_stale_spill(&root, SPILL_MAX_AGE, now + 8 * DAY);
        assert_eq!(swept, SpillRemoval { entries: 1, files: 1, bytes: 1 });
        assert!(!root.join("old").exists());
        assert!(root.exists(), "the root itself stays");
        // Missing root: no-op.
        let swept = sweep_stale_spill(&tmp.path().join("nope"), SPILL_MAX_AGE, now);
        assert_eq!(swept, SpillRemoval::default());
    }
}
