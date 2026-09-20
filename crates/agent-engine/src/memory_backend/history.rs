//! One history authority behind the selected backend. Projection is pure and
//! host-screened; selecting Axel never creates a legacy context archive.
use super::{error, MemoryBinding, Result};
use crate::SharedMessage;
use agent_core::context_archive::{
    self, ArchiveDescriptor, ArchiveRedactor, ArchiveRef, ArchiveStore, ArchivedMessage,
};
use serde_json::{json, Value};
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
            return tokio::task::spawn_blocking(move || {
                let store =
                    ArchiveStore::new(&base, scope.key(), &logical)?.with_redactor(redactor());
                let r = store.seal(&source, &note)?;
                let stored = store.fetch_note(&r.id)?;
                Ok::<_, std::io::Error>((r, stored))
            })
            .await
            .map_err(|_| error("history worker failed"))?
            .map_err(|e| error(e.to_string()));
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
            return tokio::task::spawn_blocking(move || {
                ArchiveStore::new(&base, scope.key(), "context-windows-v1")?.search(&query, limit)
            })
            .await
            .map_err(|_| error("history worker failed"))?
            .map_err(|e| error(e.to_string()));
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
            return tokio::task::spawn_blocking(move || {
                ArchiveStore::new(&base, scope.key(), "context-windows-v1")?
                    .fetch_with_provenance(&id, start, limit)
            })
            .await
            .map_err(|_| error("history worker failed"))?
            .map_err(|e| error(e.to_string()));
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
            return tokio::task::spawn_blocking(move || {
                ArchiveStore::new(&base, scope.key(), "context-windows-v1")?.fetch_note(&id)
            })
            .await
            .map_err(|_| error("history worker failed"))?
            .map_err(|e| error(e.to_string()));
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
            return tokio::task::spawn_blocking(move || {
                ArchiveStore::new(&base, scope.key(), "context-windows-v1")?.forget(&id)
            })
            .await
            .map_err(|_| error("history worker failed"))?
            .map_err(|e| error(e.to_string()));
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
        let (retry, _) = binding
            .history_seal("budget-session", &messages, "different note")
            .await
            .unwrap();
        assert_eq!(retry.id, receipt.id);
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
}
