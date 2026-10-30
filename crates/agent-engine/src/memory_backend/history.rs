//! One history authority behind the selected backend. Projection is pure and
//! host-screened; selecting Axel never creates a legacy context archive.
use super::{error, MemoryBinding, Result};
use crate::SharedMessage;
use agent_core::context_archive::{
    self, ArchiveDescriptor, ArchiveRedactor, ArchiveRef, ArchiveStore, ArchivedMessage,
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
/// Run one local archive operation on a blocking worker. The archive queues
/// operations within this process, so a worker that outlives its dropped
/// caller delays later operations rather than making them fail as busy.
///
/// Dropping the caller sets the abort flag. A worker that sees it before it
/// starts does nothing; one that sees it while waiting for admission reads and
/// writes no records, although opening the store (creating the private
/// directories) and projecting a seal may already have happened. Once admitted
/// an operation always completes, so an abandoned seal may still be published.
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
            return archive_worker(move |abort| {
                ArchiveStore::new(&base, scope.key(), "context-windows-v1")?
                    .with_abort(abort)
                    .search(&query, limit)
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
                ArchiveStore::new(&base, scope.key(), "context-windows-v1")?
                    .with_abort(abort)
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
                ArchiveStore::new(&base, scope.key(), "context-windows-v1")?
                    .with_abort(abort)
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
    pub async fn history_forget(&self, id: &str) -> Result<()> {
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
        Ok(())
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
