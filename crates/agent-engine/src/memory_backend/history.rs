//! One history authority behind the selected backend. Projection is pure and
//! host-screened; selecting Axel never creates a legacy context archive.
use super::{error, MemoryBinding, Result};
use crate::SharedMessage;
use agent_core::context_archive::{
    self, ArchiveDescriptor, ArchivePart, ArchiveRedactor, ArchiveRef, ArchiveStore,
    ArchivedMessage, Forgotten, Unscanned,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn redactor() -> Arc<ArchiveRedactor> {
    Arc::new(|text| {
        let mut value = serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.into()));
        crate::runtime::trace::export::redact_value(&mut value);
        match value {
            Value::String(s) => s,
            other => other.to_string(),
        }
    })
}
fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}
/// The file store opened for a read: fetch, note or search. Unlike
/// `ArchiveStore::new`, which a seal and a forget go through, this creates no
/// directory, changes no mode and syncs nothing. `None` when the project has
/// no archive directory: there is nothing in it to find.
fn reader(
    base: &std::path::Path,
    scope_key: &str,
    abort: Arc<AtomicBool>,
) -> std::io::Result<Option<ArchiveStore>> {
    let store = ArchiveStore::open_existing(base, scope_key, "context-windows-v1")?;
    Ok(store.map(|store| store.with_abort(abort)))
}
/// What a fetch from a project without archives is told: what the store
/// says of an archive it does not have.
fn no_archive() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::NotFound, context_archive::NOT_FOUND)
}
/// Run one local archive operation on a blocking worker. The archive queues
/// operations within this process, so a worker that outlives its dropped
/// caller delays later operations rather than making them fail as busy.
///
/// Dropping the caller sets the abort flag. A worker that sees it before it
/// starts does nothing; one that sees it while waiting for admission reads and
/// writes no records, although for a seal or a forget opening the store
/// (creating the private directories) and projecting may already have
/// happened. A read creates nothing at any point. Once admitted an operation
/// always completes, so an abandoned seal may still be published.
async fn archive_worker<T: Send + 'static>(
    work: impl FnOnce(Arc<AtomicBool>) -> std::io::Result<T> + Send + 'static,
) -> Result<T> {
    struct AbortOnDrop(Arc<AtomicBool>);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let abort = AbortOnDrop(Arc::new(AtomicBool::new(false)));
    let flag = abort.0.clone();
    tokio::task::spawn_blocking(move || {
        if flag.load(Ordering::Acquire) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "archive operation canceled before it started",
            ));
        }
        work(flag)
    })
    .await
    .map_err(|_| error("history worker failed"))?
    .map_err(|e| error(e.to_string()))
}
impl MemoryBinding {
    pub async fn history_seal(
        &self,
        logical_id: &str,
        messages: &[SharedMessage],
        note: &str,
    ) -> Result<(ArchiveRef, String)> {
        let scope = self.scope()?.clone();
        if !self.exclusive() {
            let base = self.base().to_owned();
            let logical = logical_id.to_owned();
            let source = messages.to_vec();
            let note = note.to_owned();
            // One locked operation: the stored note comes from the same scan.
            return archive_worker(move |abort| {
                ArchiveStore::new(&base, scope.key(), &logical)?
                    .with_redactor(redactor())
                    .with_abort(abort)
                    .seal_with_note(&source, &note)
            })
            .await;
        }
        let projection =
            context_archive::project_archive(logical_id, messages, note, Some(redactor()))
                .map_err(|e| error(e.to_string()))?;
        let count = projection.messages.len();
        let source_count = projection.source_message_count;
        let value = self
            .rpc(
                "history_seal",
                serde_json::to_value(projection).map_err(|_| error("history encoding failed"))?,
            )
            .await?;
        let r:ArchiveRef=serde_json::from_value(json!({"id":value["id"],"message_count":value["message_count"],"source_message_count":value["source_message_count"]})).map_err(|_|error("invalid history commit acknowledgement"))?;
        let stored = value["note"]
            .as_str()
            .ok_or_else(|| error("history commit missing note"))?
            .to_owned();
        if !valid_id(&r.id)
            || r.message_count != count
            || r.source_message_count != source_count
            || stored.len() > 8192
        {
            return Err(error("history commit acknowledgement mismatch"));
        }
        Ok((r, stored))
    }
    /// [`history_seal`](Self::history_seal) for a whole window, as rollover
    /// needs it: the archives the window was sealed as ("parts", in source
    /// order, never empty) and the note stored with the last one.
    ///
    /// The file store never fails this for a window over a per-archive limit
    /// ([`ArchiveStore::seal_window`]): it cuts the window into several
    /// archives, and a window that fits is the one archive `history_seal`
    /// makes. The archives record nothing about each other, so the returned
    /// list is the only record of a split. Parts are sealed one at a time: a
    /// failure, or a caller dropped between two of them, leaves the earlier
    /// ones on disk and returns no list; sealing the same window again finds
    /// them and returns the same list.
    ///
    /// The Axel backend seals one archive exactly as `history_seal` does, so
    /// an oversized window still fails there.
    pub async fn history_seal_window(
        &self,
        logical_id: &str,
        messages: &[SharedMessage],
        note: &str,
    ) -> Result<(Vec<ArchivePart>, String)> {
        if self.exclusive() {
            let (archive, stored) = self.history_seal(logical_id, messages, note).await?;
            let whole = ArchivePart {
                archive,
                first_source_index: 0,
                reduced_messages: 0,
            };
            return Ok((vec![whole], stored));
        }
        let scope = self.scope()?.clone();
        let base = self.base().to_owned();
        let logical = logical_id.to_owned();
        let source = messages.to_vec();
        let note = note.to_owned();
        archive_worker(move |abort| {
            let window = ArchiveStore::new(&base, scope.key(), &logical)?
                .with_redactor(redactor())
                .with_abort(abort)
                .seal_window(&source, &note)?;
            Ok((window.parts, window.note))
        })
        .await
    }
    pub async fn history_search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ArchiveDescriptor>> {
        if query.len() > 512 || limit > 32 {
            return Err(error("history search exceeds bounds"));
        }
        let scope = self.scope()?.clone();
        if !self.exclusive() {
            let base = self.base().to_owned();
            let query = query.to_owned();
            return archive_worker(move |abort| match reader(&base, scope.key(), abort)? {
                Some(store) => store.search(&query, limit),
                None => Ok(Vec::new()),
            })
            .await;
        }
        let rows: Vec<ArchiveDescriptor> = serde_json::from_value(
            self.rpc("history_search", json!({"query":query,"limit":limit}))
                .await?,
        )
        .map_err(|_| error("invalid history descriptors"))?;
        if rows.len() > limit
            || rows.iter().any(|r| {
                !valid_id(&r.id)
                    || r.snippet.len() > 256
                    || r.message_count > 4096
                    || r.source_message_count > 4096
            })
        {
            return Err(error("history descriptors exceed bounds"));
        }
        Ok(rows)
    }
    /// [`history_search`](Self::history_search) that also reports what the
    /// file store did not read: `None` when nothing is missing, otherwise an
    /// [`Unscanned`] that says whether the scan budget stopped it
    /// (`stopped_after`) and whether a shelf, a legacy slot or a file was
    /// passed over because it could not be read (`unread`). With the Axel
    /// backend the service does the search and reports neither: always `None`.
    pub async fn history_search_scan(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<(Vec<ArchiveDescriptor>, Option<Unscanned>)> {
        if self.exclusive() {
            return Ok((self.history_search(query, limit).await?, None));
        }
        if query.len() > 512 || limit > 32 {
            return Err(error("history search exceeds bounds"));
        }
        let scope = self.scope()?.clone();
        let base = self.base().to_owned();
        let query = query.to_owned();
        archive_worker(move |abort| match reader(&base, scope.key(), abort)? {
            Some(store) => store.search_scan(&query, limit),
            None => Ok((Vec::new(), None)),
        })
        .await
    }
    pub async fn history_fetch(
        &self,
        id: &str,
        start: usize,
        limit: usize,
    ) -> Result<Vec<ArchivedMessage>> {
        if !valid_id(id) || limit > 128 {
            return Err(error("invalid history range"));
        }
        let scope = self.scope()?.clone();
        if !self.exclusive() {
            let base = self.base().to_owned();
            let id = id.to_owned();
            return archive_worker(move |abort| {
                reader(&base, scope.key(), abort)?
                    .ok_or_else(no_archive)?
                    .fetch_with_provenance(&id, start, limit)
            })
            .await;
        }
        let rows: Vec<ArchivedMessage> = serde_json::from_value(
            self.rpc(
                "history_fetch",
                json!({"id":id,"start":start,"limit":limit}),
            )
            .await?,
        )
        .map_err(|_| error("invalid history fetch"))?;
        if rows.len() > limit || rows.iter().any(|r| r.source_index >= 4096) {
            return Err(error("history range exceeds bounds"));
        }
        Ok(rows)
    }
    pub async fn history_note(&self, id: &str) -> Result<String> {
        if !valid_id(id) {
            return Err(error("invalid history id"));
        }
        let scope = self.scope()?.clone();
        if !self.exclusive() {
            let base = self.base().to_owned();
            let id = id.to_owned();
            return archive_worker(move |abort| {
                reader(&base, scope.key(), abort)?
                    .ok_or_else(no_archive)?
                    .fetch_note(&id)
            })
            .await;
        }
        let note: String =
            serde_json::from_value(self.rpc("history_note", json!({"id":id})).await?)
                .map_err(|_| error("invalid history note"))?;
        if note.len() > 8192 {
            return Err(error("history note exceeds bounds"));
        }
        Ok(note)
    }
    /// Forget the archived window `id`. The file store forgets by content:
    /// every file that holds the same window becomes a tombstone, and what
    /// comes back names each one ([`Forgotten::ids`]) and says whether a
    /// place that may hold another copy could not be read
    /// ([`Forgotten::unread`]); such a copy is still live. The Axel backend
    /// keeps one record for an id and reports that one.
    pub async fn history_forget(&self, id: &str) -> Result<Forgotten> {
        if !valid_id(id) {
            return Err(error("invalid history id"));
        }
        let scope = self.scope()?.clone();
        if !self.exclusive() {
            let base = self.base().to_owned();
            let id = id.to_owned();
            return archive_worker(move |abort| {
                ArchiveStore::new(&base, scope.key(), "context-windows-v1")?
                    .with_abort(abort)
                    .forget(&id)
            })
            .await;
        }
        if self.rpc("history_forget", json!({"id":id})).await? != true {
            return Err(error("history not found in project"));
        }
        Ok(Forgotten {
            ids: vec![id.to_owned()],
            unread: None,
        })
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use agent_core::config::{MemoryBackendConfig, MemoryBackendKind};
    use agent_core::memory::store::ProjectScope;
    use std::path::Path;

    fn binding(
        root: &Path,
        kind: MemoryBackendKind,
        exe: Option<std::path::PathBuf>,
    ) -> MemoryBinding {
        MemoryBinding::new(
            root.into(),
            Ok(ProjectScope::for_root(root).unwrap()),
            &MemoryBackendConfig {
                kind,
                executable: exe,
                brain: Some(root.join("brain.r8")),
                user_scope: false,
            },
        )
    }

    async fn roundtrip(binding: &MemoryBinding) {
        let messages = vec![
            Arc::new(json!({"role":"system","content":"x".repeat(9 * 1024 * 1024)})),
            Arc::new(
                json!({"role":"assistant","metadata":"x".repeat(9 * 1024 * 1024),"content":[
                    {"type":"text","text":"exact source αβ\n"},
                    {"type":"image","source":{"data":"x".repeat(1024 * 1024)}}
                ]}),
            ),
            Arc::new(json!({"role":"user","content":[{
                "type":"tool_result","tool_use_id":"r1","content":"exact tool log\n"
            }]})),
        ];
        let original = messages.clone();
        let (receipt, note) = binding
            .history_seal("budget-session", &messages, "bounded note")
            .await
            .unwrap();
        assert_eq!(receipt.source_message_count, 3);
        assert_eq!(receipt.message_count, 2);
        assert_eq!(note, "bounded note");
        let fetched = binding.history_fetch(&receipt.id, 0, 8).await.unwrap();
        assert_eq!(fetched[0].source_index, 1);
        assert_eq!(fetched[0].block_indices, vec![0]);
        assert_eq!(
            fetched[0].message["content"][0]["text"],
            "exact source αβ\n"
        );
        assert_eq!(fetched[1].message, messages[2]);
        assert_eq!(messages, original);
        let (retry, retry_note) = binding
            .history_seal("budget-session", &messages, "different note")
            .await
            .unwrap();
        assert_eq!(retry.id, receipt.id);
        // A deduplicated seal reports the note actually stored with the segment.
        assert_eq!(retry_note, "bounded note");
        let before = binding.history_search("", 8).await.unwrap();
        let too_big = vec![Arc::new(
            json!({"role":"assistant", "content":"x".repeat(context_archive::MAX_INPUT_BYTES + 1)}),
        )];
        let err = binding
            .history_seal("budget-session", &too_big, "")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("input byte budget exceeded"));
        assert_eq!(binding.history_search("", 8).await.unwrap(), before);
        binding.history_forget(&receipt.id).await.unwrap();
        assert!(binding
            .history_seal("budget-session", &messages, "bounded note")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn legacy_history_eligible_budget_roundtrip_and_atomic_refusal() {
        let temp = tempfile::tempdir().unwrap();
        roundtrip(&binding(temp.path(), MemoryBackendKind::Legacy, None)).await;
    }

    /// The file store keeps every archive: the 129th seal in a project used to
    /// fail with "archive segment budget exhausted".
    #[tokio::test]
    async fn legacy_history_seals_past_128_archives_and_reports_a_complete_search() {
        let temp = tempfile::tempdir().unwrap();
        let binding = binding(temp.path(), MemoryBackendKind::Legacy, None);
        let mut ids = Vec::new();
        for index in 0..129 {
            let messages = vec![Arc::new(
                json!({"role":"user","content":format!("window {index}.")}),
            )];
            let (reference, _) = binding
                .history_seal("synthetic-session", &messages, "")
                .await
                .unwrap();
            ids.push(reference.id);
        }
        for id in &ids {
            assert_eq!(binding.history_fetch(id, 0, 1).await.unwrap().len(), 1);
        }
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 129);
        // The oldest is found and nothing was left unread.
        let (rows, stopped_after) = binding.history_search_scan("window 0.", 32).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(stopped_after, None);
        assert_eq!(
            binding.history_fetch(&rows[0].id, 0, 1).await.unwrap()[0].message["content"],
            "window 0."
        );
        let (rows, stopped_after) = binding.history_search_scan("", 32).await.unwrap();
        assert_eq!((rows.len(), stopped_after), (32, None));
        assert_eq!(binding.history_search("", 32).await.unwrap(), rows);
        assert!(binding.history_search_scan("", 33).await.is_err());
        assert!(binding.history_search_scan(&"q".repeat(513), 1).await.is_err());
    }

    /// A history read with the file backend creates nothing: not for a
    /// project that has never archived anything, and not on the first read
    /// of one that has. Opening the store as a seal does made the base,
    /// `context-archives` and project directories and set their modes on
    /// every call.
    #[cfg(unix)]
    #[tokio::test]
    async fn legacy_history_reads_create_nothing_and_change_nothing() {
        use std::os::unix::fs::MetadataExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let binding = binding(&root, MemoryBackendKind::Legacy, None);
        let missing = "a".repeat(32);
        let not_there = context_archive::NOT_FOUND;
        // No archive yet: nothing is made to find that out.
        assert!(binding.history_search("", 8).await.unwrap().is_empty());
        let (rows, stopped_after) = binding.history_search_scan("", 8).await.unwrap();
        assert!(rows.is_empty() && stopped_after.is_none());
        let error = binding.history_fetch(&missing, 0, 1).await.unwrap_err();
        assert!(error.to_string().contains(not_there), "{error}");
        let error = binding.history_note(&missing).await.unwrap_err();
        assert!(error.to_string().contains(not_there), "{error}");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        let message = vec![Arc::new(json!({"role":"user","content":"kept words"}))];
        let (reference, _) = binding
            .history_seal("synthetic-session", &message, "kept note")
            .await
            .unwrap();
        // Mode, change time and modification time of everything there is.
        let stamps = || {
            let mut found = Vec::new();
            let mut pending = vec![root.clone()];
            while let Some(path) = pending.pop() {
                let meta = std::fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    let entries = std::fs::read_dir(&path).unwrap();
                    pending.extend(entries.map(|entry| entry.unwrap().path()));
                }
                let times = (meta.ctime(), meta.ctime_nsec(), meta.mtime(), meta.mtime_nsec());
                found.push((path, meta.mode(), times));
            }
            found.sort();
            found
        };
        // Whatever is changed from here on gets a later change time.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let before = stamps();
        assert_eq!(before.len(), 6, "base, context-archives, project, lock, shelf, archive");
        let fetched = binding.history_fetch(&reference.id, 0, 1).await.unwrap();
        assert_eq!(fetched[0].message["content"], "kept words");
        assert_eq!(binding.history_note(&reference.id).await.unwrap(), "kept note");
        assert_eq!(binding.history_search("kept", 8).await.unwrap().len(), 1);
        let (rows, _) = binding.history_search_scan("", 8).await.unwrap();
        assert_eq!(rows.len(), 1);
        // The same answer as from a project with no archive at all.
        let error = binding.history_fetch(&missing, 0, 1).await.unwrap_err();
        assert!(error.to_string().contains(not_there), "{error}");
        assert_eq!(stamps(), before);
    }

    #[tokio::test]
    async fn axel_eligible_overflow_refuses_before_service_or_storage() {
        let temp = tempfile::tempdir().unwrap();
        let b = binding(
            temp.path(),
            MemoryBackendKind::Axel,
            Some(temp.path().join("never-started")),
        );
        let too_big = vec![Arc::new(
            json!({"role":"user", "content":"x".repeat(context_archive::MAX_INPUT_BYTES + 1)}),
        )];
        let err = b
            .history_seal("budget-session", &too_big, "")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("input byte budget exceeded"));
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    fn lines(count: usize) -> Vec<SharedMessage> {
        (0..count)
            .map(|index| Arc::new(json!({"role":"user","content":format!("line {index}.")})))
            .collect()
    }

    /// The file store through `history_seal_window`: a window within the
    /// limits of one archive is the archive `history_seal` makes, and a
    /// window over them, which `history_seal` still refuses, is several.
    #[tokio::test]
    async fn legacy_history_seal_window_splits_what_history_seal_refuses() {
        let temp = tempfile::tempdir().unwrap();
        let binding = binding(temp.path(), MemoryBackendKind::Legacy, None);
        let small = lines(3);
        let (parts, note) = binding
            .history_seal_window("window-session", &small, "the note")
            .await
            .unwrap();
        let (reference, stored) = binding
            .history_seal("window-session", &small, "another note")
            .await
            .unwrap();
        let whole = ArchivePart {
            archive: reference,
            first_source_index: 0,
            reduced_messages: 0,
        };
        assert_eq!(parts, [whole]);
        assert_eq!((note.as_str(), stored.as_str()), ("the note", "the note"));
        assert_eq!(binding.history_search("", 8).await.unwrap().len(), 1);

        let large = lines(context_archive::MAX_MESSAGES + 1);
        let refused = binding
            .history_seal("window-session", &large, "")
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("budget exceeded"), "{refused}");
        assert_eq!(binding.history_search("", 8).await.unwrap().len(), 1);
        let (parts, note) = binding
            .history_seal_window("window-session", &large, "the note")
            .await
            .unwrap();
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert_eq!(note, "the note");
        let mut covered = 0;
        for part in &parts {
            assert_eq!(part.first_source_index, covered);
            assert_eq!(part.reduced_messages, 0);
            // Inside a part, source indices start again at 0.
            let first = binding.history_fetch(&part.archive.id, 0, 1).await.unwrap();
            assert_eq!(first[0].source_index, 0);
            assert_eq!(first[0].message, large[covered]);
            covered += part.archive.source_message_count;
        }
        assert_eq!(covered, large.len());
        // The note is with the last part, and the same window again is the
        // same list, with the note that was stored.
        assert_eq!(binding.history_note(&parts[0].archive.id).await.unwrap(), "");
        assert_eq!(binding.history_note(&parts[1].archive.id).await.unwrap(), "the note");
        let again = binding
            .history_seal_window("window-session", &large, "a later note")
            .await
            .unwrap();
        assert_eq!(again, (parts, "the note".to_owned()));
        assert_eq!(binding.history_search("", 8).await.unwrap().len(), 3);
    }

    /// Decision 6.2: the Axel backend is unchanged. A window over a
    /// per-archive limit is still refused there, before the service is
    /// started or anything is stored, through the rollover's entry point too.
    #[tokio::test]
    async fn axel_oversized_window_still_refuses() {
        let temp = tempfile::tempdir().unwrap();
        let b = binding(
            temp.path(),
            MemoryBackendKind::Axel,
            Some(temp.path().join("never-started")),
        );
        let too_big = vec![Arc::new(
            json!({"role":"user", "content":"x".repeat(context_archive::MAX_INPUT_BYTES + 1)}),
        )];
        let err = b
            .history_seal_window("budget-session", &too_big, "")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("input byte budget exceeded"), "{err}");
        let too_many = lines(context_archive::MAX_MESSAGES + 1);
        let err = b
            .history_seal_window("budget-session", &too_many, "")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("message/note budget exceeded"), "{err}");
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "requires SYNAPS_AXEL_TEST_BIN; isolated synthetic archive only"]
    async fn real_axel_history_eligible_budget_roundtrip_and_atomic_refusal() {
        let temp = tempfile::tempdir().unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let exe =
            std::env::var_os("SYNAPS_AXEL_TEST_BIN").expect("explicit synthetic service binary");
        roundtrip(&binding(
            temp.path(),
            MemoryBackendKind::Axel,
            Some(exe.into()),
        ))
        .await;
        assert!(!temp.path().join("context-archives").exists());
    }

    /// Parallel history tools and rollovers share one archive directory per
    /// project; in-process overlap must queue, never surface "archive busy".
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_legacy_history_operations_queue_instead_of_reporting_busy() {
        let temp = tempfile::tempdir().unwrap();
        let binding = binding(temp.path(), MemoryBackendKind::Legacy, None);
        let tasks: Vec<_> = (0..8)
            .map(|worker| {
                let binding = binding.clone();
                tokio::spawn(async move {
                    for round in 0..6 {
                        let text = format!("worker {worker} round {round}");
                        let messages = vec![Arc::new(json!({"role":"user","content":text}))];
                        let logical = format!("synthetic-session-{worker}");
                        let (reference, note) =
                            binding.history_seal(&logical, &messages, "note").await?;
                        assert_eq!(note, "note");
                        assert_eq!(binding.history_fetch(&reference.id, 0, 1).await?.len(), 1);
                        binding.history_search("worker", 4).await?;
                    }
                    Ok::<_, crate::RuntimeError>(())
                })
            })
            .collect();
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert_eq!(binding.history_search("", 32).await.unwrap().len(), 32);
    }

    /// A caller dropped while its operation waits (e.g. a canceled rollover
    /// queued behind a history read) must not start it later.
    #[tokio::test]
    async fn a_dropped_caller_aborts_its_archive_operation_before_it_starts() {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (seen_tx, seen_rx) = std::sync::mpsc::channel();
        let call = archive_worker(move |abort| {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            seen_tx.send(abort.load(Ordering::Acquire)).unwrap();
            Ok(())
        });
        tokio::select! {
            biased;
            _ = tokio::task::spawn_blocking(move || entered_rx.recv().unwrap()) => {}
            _ = call => panic!("worker cannot finish before release"),
        }
        // `call` is dropped; the worker is still running and must see it.
        release_tx.send(()).unwrap();
        assert!(tokio::task::spawn_blocking(move || seen_rx.recv().unwrap())
            .await
            .unwrap());

        let completed = archive_worker(|abort| Ok(abort.load(Ordering::Acquire)))
            .await
            .unwrap();
        assert!(!completed, "an awaited operation is never aborted");
    }
}
