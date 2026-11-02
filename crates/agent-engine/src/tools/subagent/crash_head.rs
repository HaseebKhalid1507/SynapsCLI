//! A reload-stopped v2 run's head record left without its final (train30
//! item 4b): the next image marks it terminal.
//!
//! A run a daemon reload stopped whose final record was not published (its
//! write failed during the drain, e.g. with EMFILE, or the run had not
//! finished when the exec came) leaves its head, status "running", as its
//! crash checkpoint. The adopting image, when it leaves that worker to the
//! model, rewrites the head with status "cancelled" ([`mark_cancelled`]):
//! the same record (run, generation, history digest and history untouched)
//! published atomically, a temp file in the same directory renamed over it
//! and the directory synced. Inventories then no longer see a "running"
//! head no registry owns, and `subagent_resume` by its path still continues
//! it as a crash checkpoint (`record::resolve` yields the head with its
//! loss warning; resume admission never reads the status).

use std::path::Path;

use super::record::{record_file_name, RecordKind, Resolution};
use super::record_file;
use super::record_store::{self, Publication, RecordDir};
use crate::runtime::subagent::CarriedWorker;

/// What [`mark_cancelled`] did with a head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Marked {
    /// The head now reads status "cancelled".
    Rewritten,
    /// Left as it is, for this reason.
    Unchanged(&'static str),
}

/// The status a left crash checkpoint is marked with.
pub const CANCELLED: &str = "cancelled";

/// Rewrite the head record at `head` (below the trusted archive `root`) with
/// status [`CANCELLED`], atomically, when it is the run's authoritative
/// record: a head with no final. Never writes while an earlier writer may
/// still publish, nor over a published final's run. Blocking (file I/O).
pub fn mark_cancelled(head: &Path, root: &Path) -> Result<Marked, String> {
    let raw = head.to_str().ok_or("the head's path is not UTF-8")?;
    let location = record_file::locate(raw, root).map_err(|e| e.to_string())?;
    let writer = record_store::writer_probe(&location);
    let resolution =
        record_file::resolve_location(&location, writer, None).map_err(|e| e.to_string())?;
    let mut record = match resolution {
        Resolution::Authoritative {
            kind: RecordKind::Head,
            record,
            ..
        } => record,
        Resolution::Authoritative { .. } => return Ok(Marked::Unchanged("its final record is published")),
        Resolution::NotFound => return Ok(Marked::Unchanged("no record of it is there")),
        Resolution::Legacy(_) => return Ok(Marked::Unchanged("its record is a v1 archive")),
        Resolution::Unresolved(why) => return Err(format!("its records are unresolved: {why:?}")),
    };
    if record.status == CANCELLED {
        return Ok(Marked::Unchanged("already cancelled"));
    }
    // The status only: run, generation, history digest and history stay
    // as published, so the head still resolves as the same checkpoint.
    record.status = CANCELLED.into();
    let bytes = serde_json::to_vec(&*record).map_err(|e| e.to_string())?;
    let name = record_file_name(location.handle(), RecordKind::Head).map_err(|e| e.to_string())?;
    let dir = RecordDir::open_or_create(location.root(), location.process_dir())
        .map_err(|e| e.to_string())?;
    match record_store::publish(&dir, &name, &bytes) {
        Publication::Durable => Ok(Marked::Rewritten),
        Publication::NotPublished(e) => Err(format!("not rewritten: {e}")),
        Publication::Uncertain(e) => Err(format!("rewrite uncertain: {e}")),
    }
}

/// The head `w` left behind for this image to mark: a worker the reload
/// stopped whose final record was not published, with its head's path.
pub fn left_head(w: &CarriedWorker) -> Option<&Path> {
    if !w.stopped_by_reload || w.archive_path.is_some() {
        return None;
    }
    w.crash_head.as_deref()
}

/// [`mark_cancelled`] for each of `workers` that [`left_head`] names, below
/// `root`: (handle id, outcome), in order. Blocking (file I/O).
pub fn mark_left(root: &Path, workers: &[&CarriedWorker]) -> Vec<(String, Result<Marked, String>)> {
    workers
        .iter()
        .filter_map(|w| left_head(w).map(|head| (w.id.clone(), mark_cancelled(head, root))))
        .collect()
}

/// Test-only (`cfg(test)` / the `testing` feature): write worker
/// `handle_id`'s v2 head record (status "running", no final) into this
/// process's archive directory, as a run the reload stopped before its
/// final record was published leaves it, and return its path.
#[cfg(any(test, feature = "testing"))]
#[doc(hidden)]
pub fn write_test_crash_head(
    handle_id: &str,
    history: &[serde_json::Value],
) -> std::io::Result<std::path::PathBuf> {
    use super::record::{BackendKind, Binding, RunId, WorkerMeta};
    use super::record_store::{publish, RecordFs};
    let io = |e: String| std::io::Error::other(e);
    let history: Vec<crate::SharedMessage> =
        history.iter().cloned().map(std::sync::Arc::new).collect();
    let binding = Binding {
        backend: BackendKind::Legacy,
        scope_digest: format!("sha256:{}", "0".repeat(64)),
    };
    let meta = WorkerMeta {
        agent_name: "worker".into(),
        model: "provider/model".into(),
        system_prompt: String::new(),
        timeout_secs: 600,
        thinking: None,
        watch_usage: None,
    };
    let record = super::persist::start_template(RunId::random(), handle_id, binding, meta, &history)
        .map_err(io)?;
    let root = super::archive::archive_root();
    let process_dir = super::archive::process_dir_name();
    super::archive::create_private_dir(&root.join(process_dir))?;
    let dir = RecordDir::open_or_create(&root, process_dir).map_err(|e| io(e.to_string()))?;
    let name = record_file_name(handle_id, RecordKind::Head).map_err(|e| io(e.to_string()))?;
    let bytes = serde_json::to_vec(&record).map_err(|e| io(e.to_string()))?;
    match publish(&dir, &name, &bytes) {
        Publication::Durable => dir
            .location(handle_id, RecordKind::Head)
            .map(|location| location.path())
            .map_err(|e| io(e.to_string())),
        other => Err(io(format!("{other:?}"))),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::record::{BackendKind, Binding, RecordKind, WorkerRecord};
    use super::super::test_fixtures as fx;
    use super::*;
    use std::path::PathBuf;

    /// A temporary archive root with the fixture's process directory.
    fn layout() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("subagent-history");
        let dir = root.join(fx::PROCESS_DIR);
        fx::private_dir(&root);
        fx::private_dir(&dir);
        (tmp, root, dir)
    }

    /// Worker `handle`'s head (status "running"), as a run the reload
    /// stopped leaves it when its final was not published.
    fn running_head(dir: &Path, handle: &str) -> PathBuf {
        let binding = Binding {
            backend: BackendKind::Legacy,
            scope_digest: format!("sha256:{}", "0".repeat(64)),
        };
        let mut head = fx::record(&fx::run(9), binding, vec![], &[], None);
        head.handle_id = handle.into();
        head.kind = RecordKind::Head;
        head.status = "running".into();
        let path = dir.join(format!("{handle}.head.json"));
        fx::private_file(&path, &serde_json::to_vec(&head).unwrap());
        path
    }

    fn status_of(path: &Path) -> String {
        let record: WorkerRecord = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        record.status
    }

    fn worker(id: &str, archive: Option<PathBuf>, head: Option<PathBuf>) -> CarriedWorker {
        CarriedWorker {
            id: id.into(),
            subagent_id: id.trim_start_matches("sa_").parse().unwrap(),
            agent: "digger".into(),
            status: "cancelled".into(),
            archive_path: archive,
            stopped_by_reload: true,
            wake: true,
            crash_head: head,
            ..Default::default()
        }
    }

    /// Only a reload-stopped worker with a head and no final is marked: one
    /// whose final is published, a lost completion (not stopped by the
    /// reload) and one with no head are left as they are. Mutants: drop the
    /// archive check; drop the reload check.
    #[test]
    fn mark_left_marks_only_reload_stopped_workers_without_a_final() {
        let (_tmp, root, dir) = layout();
        let lost = running_head(&dir, "sa_31");
        let archived = running_head(&dir, "sa_32");
        let completion = running_head(&dir, "sa_33");
        let workers = [
            worker("sa_31", None, Some(lost.clone())),
            worker("sa_32", Some(dir.join("sa_32.json")), Some(archived.clone())),
            CarriedWorker {
                stopped_by_reload: false,
                wake_lost: true,
                ..worker("sa_33", None, Some(completion.clone()))
            },
            worker("sa_34", None, None),
        ];
        let refs: Vec<&CarriedWorker> = workers.iter().collect();
        assert_eq!(left_head(&workers[0]), Some(lost.as_path()));
        assert_eq!(
            mark_left(&root, &refs),
            vec![("sa_31".to_string(), Ok(Marked::Rewritten))]
        );
        assert_eq!(status_of(&lost), CANCELLED);
        assert_eq!(status_of(&archived), "running");
        assert_eq!(status_of(&completion), "running");
    }
}
