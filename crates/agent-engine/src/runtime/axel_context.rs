//! Host Axel continuous-memory adapter. No extension fallback or new consent.
use super::{
    capture_worker::{CaptureCommitState, CaptureFailure, CaptureProvider},
    chat_capture::{CaptureId, ChatTurnCapture, ConversationSummaryCapture},
    memory_context::{
        self as mc, MemoryContextLease, RecallCallError, RecallRequest, SessionMemoryState,
    },
};
use crate::memory_backend::MemoryBinding;
use agent_core::{
    memory::store::{MemoryRecord, MemorySensitivity, ProjectMemoryQuery},
    BoundedText,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

pub(super) const PROVIDER_ID: &str = "axel-host";

pub(super) struct AxelCaptureProvider {
    pub binding: MemoryBinding,
    pub state: Arc<Mutex<SessionMemoryState>>,
    pub lease: MemoryContextLease,
}

impl AxelCaptureProvider {
    fn live(&self) -> bool {
        self.binding.is_axel()
            && self.lease.provider_id.as_str() == PROVIDER_ID
            && self
                .binding
                .scope()
                .is_ok_and(|scope| scope.key() == self.lease.project_id.as_str())
            && self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .capture_lease_at(std::time::SystemTime::now())
                .as_ref()
                == Some(&self.lease)
    }

    fn call(&self, operation: &str, payload: Value) -> Result<Value, CaptureFailure> {
        // CaptureWorker owns a native worker thread; never block an async turn.
        // A frontend runtime may shut down while this queued native worker
        // drains. Own the timer/process driver here rather than use a stale
        // frontend Handle (which panics after shutdown).
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| failure("capture_runtime_unavailable"))?;
        runtime.block_on(async {
            if !self.live() { return Err(failure("capture_consent_revoked")); }
            tokio::select! {
                biased;
                _ = async {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                        if !self.live() { break; }
                    }
                } => Err(failure("capture_consent_revoked")),
                result = self.binding.rpc(operation, payload) => result.map_err(|_| failure("capture_commit_unknown")),
            }
        })
    }

    fn store(&self, payload: Value) -> Result<(), CaptureFailure> {
        if payload.get("project_id").and_then(Value::as_str) != Some(self.lease.project_id.as_str())
        {
            return Err(failure("capture_scope_mismatch"));
        }
        let id = payload
            .get("capture_id")
            .and_then(Value::as_str)
            .ok_or_else(|| failure("capture_identity_missing"))?
            .to_owned();
        let response = self.call("capture", payload)?;
        validate_ack(&response, &id)
    }
}

fn failure(code: &'static str) -> CaptureFailure {
    CaptureFailure { code }
}

fn validate_ack(value: &Value, id: &str) -> Result<(), CaptureFailure> {
    if value.get("capture_id").and_then(Value::as_str) != Some(id)
        || value.get("committed").and_then(Value::as_bool) != Some(true)
    {
        return Err(failure("capture_ack_invalid"));
    }
    Ok(())
}

fn validate_query(value: &Value, id: &str) -> Result<CaptureCommitState, CaptureFailure> {
    if value.get("capture_id").and_then(Value::as_str) != Some(id) {
        return Err(failure("capture_query_invalid"));
    }
    match (
        value.get("committed").and_then(Value::as_bool),
        value.get("tombstoned").and_then(Value::as_bool),
    ) {
        // Deletion evidence is final: never resurrect a forgotten capture.
        (Some(true), Some(_)) => Ok(CaptureCommitState::Committed),
        (Some(false), Some(false)) => Ok(CaptureCommitState::Absent),
        _ => Err(failure("capture_query_unknown")),
    }
}

impl CaptureProvider for AxelCaptureProvider {
    fn capture(&self, capture: ChatTurnCapture) -> Result<(), CaptureFailure> {
        use agent_core::core::disclosure::DisclosureClass;
        if capture.sensitivity != super::chat_capture::Sensitivity::Normal
            || capture.disclosure.classes.iter().any(|class| {
                !matches!(
                    class,
                    DisclosureClass::ModelVisible | DisclosureClass::ModelVisibleAfterRedaction
                )
            })
        {
            return Err(failure("capture_disclosure_refused"));
        }
        self.store(mc::capture_request_wire(&capture))
    }
    fn capture_summary(&self, capture: ConversationSummaryCapture) -> Result<(), CaptureFailure> {
        let id = *capture.capture_id.as_bytes();
        match self.store(mc::summary_capture_request_wire(&capture)) {
            Ok(()) => Ok(()),
            Err(error) => {
                // The legacy worker's summary branch does not query ambiguous
                // commits. Reconcile here, without retrying the write.
                match self.contains_capture(&id) {
                    Ok(CaptureCommitState::Committed) => Ok(()),
                    Ok(CaptureCommitState::Absent) | Err(_) => Err(error),
                }
            }
        }
    }
    fn contains_capture(&self, id: &[u8; 32]) -> Result<CaptureCommitState, CaptureFailure> {
        let id = CaptureId::from_bytes(*id).to_hex();
        let response = self.call("capture_query", json!({"capture_id": id}))?;
        validate_query(&response, &id)
    }
}

/// A stable literal from the actual request, not a semantic embedding or the
/// whole prompt. Longest eligible word, first occurrence on ties; at most 64 B.
fn task_literal(prompt: &str) -> Option<String> {
    const STOP: &[&str] = &[
        "please",
        "would",
        "could",
        "should",
        "implement",
        "complete",
        "explain",
        "about",
        "with",
        "this",
        "that",
        "these",
        "those",
        "their",
        "there",
        "which",
        "using",
        "memory",
        "recall",
    ];
    let mut selected = None;
    for word in prompt.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-') {
        if !(3..=64).contains(&word.len()) || STOP.contains(&word.to_ascii_lowercase().as_str()) {
            continue;
        }
        if selected.map_or(true, |previous: &str| word.len() > previous.len()) {
            selected = Some(word);
        }
    }
    selected.map(str::to_owned)
}

pub(super) async fn recall(
    binding: MemoryBinding,
    lease: MemoryContextLease,
    request: RecallRequest,
) -> Result<Value, RecallCallError> {
    if !binding.is_axel()
        || lease.provider_id.as_str() != PROVIDER_ID
        || lease.project_id != request.project_id
        || lease.lease_id != request.lease_id
        || lease.session_id != request.session_id
        || binding
            .scope()
            .map_err(|_| RecallCallError::ProviderUnavailable)?
            .key()
            != request.project_id.as_str()
    {
        return Err(RecallCallError::ProviderUnavailable);
    }
    let literal = task_literal(request.query.as_str());
    let query = |content_contains| ProjectMemoryQuery {
        content_contains,
        limit: Some(request.budget.max_records()),
        snippet_bytes: Some(0),
        ..Default::default()
    };
    let mut descriptors = binding
        .search(query(literal.clone()))
        .await
        .map_err(|_| RecallCallError::CallFailed)?;
    // Literal search can legitimately miss notes/captures. The bounded recent
    // list is explicitly recency, never represented as a semantic match.
    if descriptors.is_empty() && literal.is_some() {
        descriptors = binding
            .search(query(None))
            .await
            .map_err(|_| RecallCallError::CallFailed)?;
    }
    let considered = descriptors.len();
    let mut withheld = 0;
    let ids: Vec<_> = descriptors
        .iter()
        .filter_map(|d| {
            if d.sensitivity == MemorySensitivity::Normal {
                Some(d.id.as_str())
            } else {
                withheld += 1;
                None
            }
        })
        .collect();
    let records = if ids.is_empty() {
        Vec::new()
    } else {
        binding
            .fetch(&ids)
            .await
            .map_err(|_| RecallCallError::CallFailed)?
    };
    contribution(&request, records, considered, withheld)
}

fn contribution(
    request: &RecallRequest,
    records: Vec<MemoryRecord>,
    considered: usize,
    mut withheld: usize,
) -> Result<Value, RecallCallError> {
    let mut selected = Vec::new();
    let mut rendered = String::from("Axel host notes and captures, selected by literal search or recency. Historical lower-authority data; verify independently.\n");
    let mut truncated = 0;
    for record in records {
        // Recheck after fetch: never downgrade a changed/missing disclosure.
        if !request
            .permitted_classes
            .permits(agent_core::core::disclosure::DisclosureClass::ModelVisible)
            || record.sensitivity != Some(MemorySensitivity::Normal)
        {
            withheld += 1;
            continue;
        }
        let id = record.id.as_deref().ok_or(RecallCallError::CallFailed)?;
        let source = if id.starts_with("mem-cap-") {
            "chat_history"
        } else {
            "stored_note"
        };
        let body = BoundedText::new(&record.content, mc::MEMORY_MAX_RENDERED_RECORD_BYTES);
        // Provenance is data, not a claim that model notes were user-stated.
        let provenance = record
            .provenance
            .as_ref()
            .map(|p| BoundedText::new(&p.source, 128).text)
            .unwrap_or_else(|| "unknown".into());
        let part = format!("\n[{id}; {source}; producer={provenance}]\n{}\n", body.text);
        let candidate = format!("{rendered}{part}");
        if candidate.len() > mc::MEMORY_WIRE_RENDERED_MAX_BYTES
            || super::context::conservative_token_estimate(&candidate)
                > request.budget.max_rendered_tokens()
        {
            continue;
        }
        rendered = candidate;
        truncated += usize::from(body.truncated);
        selected.push(
            json!({"memory_id": id, "source": source, "timestamp": record.timestamp_ms / 1000,
            "rank_reason": ["recency"], "sensitivity": "model_visible", "retention": "standard",
            "content": body.text, "truncated": body.truncated}),
        );
        if selected.len() == request.budget.max_records() {
            break;
        }
    }
    Ok(
        json!({"schema": "contribution/1", "provider_id": PROVIDER_ID, "project_id": request.project_id.as_str(),
        "records": selected, "rendered": rendered, "accounting": {"candidates_considered": considered, "withheld": withheld, "truncated": truncated}}),
    )
}

fn visible(value: &Value) -> bool {
    ["disclosure", "disclosure_class"].iter().all(|key| {
        value
            .get(key)
            .map_or(true, |v| v.as_str() == Some("model_visible"))
    }) && ["sensitivity", "retention", "retention_class"]
        .iter()
        .all(|key| {
            value.get(key).map_or(true, |v| {
                matches!(v.as_str(), Some("normal" | "standard" | "model_visible"))
            })
        })
        && ["private", "restricted"]
            .iter()
            .all(|key| value.get(key).map_or(true, |v| v.as_bool() == Some(false)))
        && value
            .get("channel")
            .map_or(true, |v| v.as_str() == Some("final"))
        && value.get("content_class").map_or(true, |v| {
            matches!(v.as_str(), Some("user_text" | "assistant_text"))
        })
}

/// Summary sources must not contain any excluded nested evidence. Terminal
/// capture can select clean text blocks individually; a derived summary cannot.
pub(super) fn capture_source_safe(value: &Value) -> bool {
    visible(value)
        && !value
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| matches!(kind, "thinking" | "redacted_thinking" | "reasoning"))
        && match value {
            Value::Array(items) => items.iter().all(capture_source_safe),
            Value::Object(items) => items.values().all(capture_source_safe),
            _ => true,
        }
}

/// Only ordinary visible text blocks may become normal captures. Never flatten
/// reasoning/tool JSON, credentials, local-only or per-item-consent material.
fn screen_text(text: &str) -> String {
    let mut value = Value::String(text.to_owned());
    super::trace::export::redact_value(&mut value);
    value
        .as_str()
        .unwrap_or("[withheld]")
        .split_inclusive(char::is_whitespace)
        .map(|word| {
            let trimmed = word.trim_end_matches(char::is_whitespace);
            let lower = trimmed.to_ascii_lowercase();
            if ["password=", "token=", "secret=", "api_key="]
                .iter()
                .any(|prefix| lower.starts_with(prefix))
            {
                format!("[REDACTED]{}", &word[trimmed.len()..])
            } else {
                word.to_owned()
            }
        })
        .collect()
}

pub(super) fn capture_text(message: &Value) -> Option<String> {
    if message.get("_synaps_context").is_some() || !visible(message) {
        return None;
    }
    let content = message.get("content")?;
    if let Some(text) = content.as_str() {
        return Some(screen_text(text));
    }
    let text = content
        .as_array()?
        .iter()
        .filter(|block| visible(block) && block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then(|| screen_text(&text))
}

#[cfg(test)]
mod tests {
    use super::super::Runtime;
    use super::*;
    use agent_core::memory::store::{MemoryProvenance, MemoryRetention};

    #[test]
    fn axel_capture_ack_query_are_exact_and_unknown_is_not_absent() {
        let id = "a".repeat(64);
        assert!(validate_ack(&json!({"capture_id":id,"committed":true}), &id).is_ok());
        for value in [
            json!({}),
            json!({"capture_id":id}),
            json!({"capture_id":id,"committed":false}),
            json!({"capture_id":"foreign","committed":true}),
        ] {
            assert!(validate_ack(&value, &id).is_err());
        }
        assert_eq!(
            validate_query(
                &json!({"capture_id":id,"committed":false,"tombstoned":false}),
                &id
            ),
            Ok(CaptureCommitState::Absent)
        );
        assert_eq!(
            validate_query(
                &json!({"capture_id":id,"committed":true,"tombstoned":true}),
                &id
            ),
            Ok(CaptureCommitState::Committed)
        );
        for value in [
            json!({}),
            json!({"capture_id":id,"committed":false}),
            json!({"capture_id":id,"committed":false,"tombstoned":true}),
            json!({"capture_id":"foreign","committed":false,"tombstoned":false}),
            json!({"capture_id":id,"committed":"false","tombstoned":false}),
        ] {
            assert!(validate_query(&value, &id).is_err());
        }
    }

    #[test]
    fn axel_capture_screen_typed_blocks_and_all_retention_labels() {
        let clean = json!({"role":"assistant","content":[
            {"type":"thinking","thinking":"PRIVATE"}, {"type":"redacted_thinking","data":"PRIVATE"},
            {"type":"tool_use","input":{"credential":"PRIVATE"}},
            {"type":"text","text":"visible final"},
            {"type":"text","text":{"value":"PRIVATE","retention":"never_persist"}}
        ]});
        assert_eq!(capture_text(&clean).as_deref(), Some("visible final"));
        for key in [
            "disclosure",
            "disclosure_class",
            "sensitivity",
            "retention",
            "retention_class",
        ] {
            for label in [
                "secret",
                "sensitive",
                "never_persist",
                "persist_never_transmit",
                "local_only",
                "visible_after_consent",
                "model_visible_after_consent",
            ] {
                let mut message = json!({"role":"user", "content":"PRIVATE"});
                message[key] = json!(label);
                assert_eq!(capture_text(&message), None, "{key}/{label}");
                let mut message =
                    json!({"role":"user", "content":[{"type":"text","text":"PRIVATE"}]});
                message["content"][0][key] = json!(label);
                assert_eq!(capture_text(&message), None, "block {key}/{label}");
            }
        }
        assert!(capture_text(
            &json!({"role":"assistant","channel":"analysis","content":"PRIVATE"})
        )
        .is_none());
        assert!(capture_text(&json!({"role":"assistant","content":[{"type":"text","channel":"analysis","text":"PRIVATE"}]})).is_none());
    }

    #[test]
    fn capture_omits_all_attachment_sources_including_plaintext_documents() {
        let message = json!({"role":"user","content":[
            {"type":"text","text":"Compare selected files"},
            {"type":"image","source":{"type":"base64","data":"IMAGE_SECRET"}},
            {"type":"document","source":{"type":"base64","data":"PDF_SECRET"}},
            {"type":"document","source":{"type":"text","data":"FILE_TEXT_SECRET"}}
        ]});
        assert_eq!(
            capture_text(&message).as_deref(),
            Some("Compare selected files")
        );
    }

    #[test]
    fn capture_text_screens_credentials_and_omits_continuation_notes() {
        let text=capture_text(&json!({"role":"user","content":"keep this password=canary-do-not-store token=canary-token"})).unwrap();
        assert!(!text.contains("canary-do-not-store"));
        assert!(!text.contains("canary-token"));
        assert!(text.contains("keep this"));
        assert!(capture_text(
            &json!({"role":"user","_synaps_context":{},"content":"private working note"})
        )
        .is_none());
    }

    #[test]
    fn axel_query_is_minimal_deterministic_literal_not_prompt() {
        let prompt = "Please implement cancellation handling with tests";
        assert_eq!(task_literal(prompt).as_deref(), Some("cancellation"));
        assert_eq!(task_literal("please recall memory"), None);
        assert_eq!(task_literal("abc def"), Some("abc".into()));
        assert!(task_literal(&"z".repeat(4096)).is_none());
    }

    fn request() -> RecallRequest {
        RecallRequest {
            schema: mc::RecallSchemaVersion::parse("recall/1").unwrap(),
            lease_id: mc::MemoryLeaseId::parse("lease").unwrap(),
            project_id: mc::ProjectId::parse("proj").unwrap(),
            session_id: mc::SessionId::parse("session").unwrap(),
            turn_id: mc::TurnId::parse("turn").unwrap(),
            query: mc::BoundedUserQuery::new("synthetic cancellation"),
            recent_context_digest: mc::ContextDigest::from_bytes([0; 32]),
            budget: mc::RecallBudget::from_engine_tokens(512).unwrap(),
            permitted_classes: mc::DisclosureGrantSet::model_visible_only(),
        }
    }
    fn record(id: &str, body: &str, sensitivity: MemorySensitivity) -> MemoryRecord {
        MemoryRecord {
            namespace: "notes".into(),
            timestamp_ms: 1000,
            content: body.into(),
            tags: vec![],
            meta: None,
            id: Some(id.into()),
            project: Some("proj".into()),
            provenance: Some(MemoryProvenance {
                source: "model".into(),
                session: None,
            }),
            sensitivity: Some(sensitivity),
            retention: Some(MemoryRetention::Standard),
        }
    }

    #[test]
    fn axel_contribution_bounds_sources_and_automatic_disclosure() {
        let request = request();
        let wire = contribution(
            &request,
            vec![
                record("note", "visible", MemorySensitivity::Normal),
                record(
                    &format!("mem-cap-{}", "a".repeat(64)),
                    "capture text",
                    MemorySensitivity::Normal,
                ),
                record("secret", "PRIVATE SECRET", MemorySensitivity::Secret),
                record(
                    "sensitive",
                    "PRIVATE SENSITIVE",
                    MemorySensitivity::Sensitive,
                ),
                record("large", &"界".repeat(4096), MemorySensitivity::Normal),
            ],
            5,
            0,
        )
        .unwrap();
        assert!(!wire.to_string().contains("PRIVATE"));
        let parsed = mc::parse_contribution_wire(&wire).unwrap();
        mc::validate_contribution(
            &parsed,
            &request.project_id,
            512,
            &request.permitted_classes,
        )
        .unwrap();
        assert_eq!(parsed.records[0].source, mc::MemorySource::StoredNote);
        assert_eq!(parsed.records[1].source, mc::MemorySource::ChatHistory);
        assert_eq!(parsed.accounting.withheld, 2);
        assert!(parsed.rendered.text.contains("producer=model"));
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn axel_off_and_model_control_do_not_start_process_or_grant() {
        let _base = crate::test_env::BaseDirGuard::new();
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let executable = root.join("service");
        let marker = root.join("started");
        std::fs::write(
            &executable,
            format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut runtime = Runtime::new_headless();
        runtime.apply_memory_backend_config(&crate::config::MemoryBackendConfig {
            kind: crate::config::MemoryBackendKind::Axel,
            executable: Some(executable),
            brain: Some(root.join("synthetic.r8")),
            user_scope: false,
        });
        let capability = runtime.memory_tool_capability().unwrap();
        assert_eq!(capability.status().durable, mc::DurableStatus::Off);
        assert_eq!(
            capability.recall_once(None),
            Err(mc::MemoryContextError::RequiresHostConfirmation)
        );
        let mut messages = vec![Arc::new(
            json!({"role":"user","content":"synthetic cancellation"}),
        )];
        runtime.apply_turn_memory_recall(&mut messages).await;
        assert_eq!(messages.len(), 1);
        assert!(runtime
            .capture_completed_turn_for_harness(messages.clone(), 1)
            .is_err());
        capability.disable();
        assert!(!marker.exists());
        assert!(!root.join("synthetic.r8").exists());
    }
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    #[ignore = "requires pinned Axel service; consented synthetic session import only"]
    async fn real_axel_history_import_uses_backend_receipts_and_preserves_private_exclusions() {
        use std::os::unix::fs::PermissionsExt;
        let base = crate::test_env::BaseDirGuard::new();
        std::fs::set_permissions(base.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut runtime = Runtime::new_headless();
        runtime.apply_memory_backend_config(&crate::config::MemoryBackendConfig {
            kind: crate::config::MemoryBackendKind::Axel,
            executable: Some(
                std::env::var_os("SYNAPS_AXEL_TEST_BIN")
                    .expect("explicit binary")
                    .into(),
            ),
            brain: Some(base.path().join("import.r8")),
            user_scope: false,
        });
        let mut session = agent_core::session::Session::new("synthetic", "off", None);
        session.api_messages = vec![
            Arc::new(json!({"role":"user","content":"IMPORT_USER_CANARY"})),
            Arc::new(
                json!({"role":"assistant","content":[{"type":"thinking","thinking":"IMPORT_REASONING_MUST_NOT_PERSIST"},{"type":"text","text":"IMPORT_ASSISTANT_CANARY"}]}),
            ),
            Arc::new(
                json!({"role":"user","retention":"never_persist","content":"IMPORT_RESTRICTED_MUST_NOT_PERSIST"}),
            ),
        ];
        session.save().await.unwrap();
        let mut entry = agent_core::core::session_index::SessionIndexRecord::start(&session.id);
        entry.cwd = Some(runtime.memory_backend.scope().unwrap().root().to_owned());
        agent_core::core::session_index::append_record(&entry).unwrap();
        runtime.memory_history_preview().unwrap();
        assert!(!base.path().join("import.r8").exists());
        assert!(runtime.memory_history_confirm().is_err());
        runtime
            .memory_context_enable(
                mc::MemoryContextMode::CaptureOnly,
                mc::mint_explicit_command_proof(),
            )
            .unwrap();
        runtime.memory_history_preview().unwrap();
        let report = runtime.memory_history_confirm().unwrap();
        assert_eq!(report.captures_built, 1);
        let rows = runtime
            .memory_backend
            .search(ProjectMemoryQuery {
                content_contains: Some("IMPORT_ASSISTANT_CANARY".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        let inventory = runtime
            .memory_backend
            .rpc("export", json!({"full":true}))
            .await
            .unwrap()
            .to_string();
        assert!(!inventory.contains("IMPORT_REASONING_MUST_NOT_PERSIST"));
        assert!(!inventory.contains("IMPORT_RESTRICTED_MUST_NOT_PERSIST"));
        runtime.memory_backend.forget(&rows[0].id).await.unwrap();
        runtime.memory_history_preview().unwrap();
        let retry = runtime.memory_history_confirm().unwrap();
        assert_eq!(retry.ranges_skipped, 1);
        assert!(!base.path().join("sessions/unused-axel-progress").exists());
        assert!(runtime
            .memory_backend
            .search(Default::default())
            .await
            .unwrap()
            .is_empty());
    }

    /// Explicit real-process test, synthetic brain only. Includes the exact
    /// Runtime recall entry point with its production 150 ms deadline.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial(synaps_base_dir)]
    #[ignore = "requires SYNAPS_AXEL_TEST_BIN pointing to the separately built pinned service"]
    async fn real_axel_runtime_capture_recall_consent_and_tombstone() {
        use super::super::chat_capture as cc;
        use std::{os::unix::fs::PermissionsExt, time::SystemTime};
        let base = crate::test_env::BaseDirGuard::new();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let executable = std::path::PathBuf::from(
            std::env::var_os("SYNAPS_AXEL_TEST_BIN").expect("explicit synthetic test executable"),
        );
        let mut runtime = Runtime::new_headless();
        runtime.apply_memory_backend_config(&crate::config::MemoryBackendConfig {
            kind: crate::config::MemoryBackendKind::Axel,
            executable: Some(executable),
            brain: Some(root.join("brain.r8")),
            user_scope: false,
        });
        let binding = runtime.memory_backend.clone();
        let original = vec![Arc::new(
            json!({"role":"user","content":"Recall cobalt_27"}),
        )];
        let mut off = original.clone();
        runtime.apply_turn_memory_recall(&mut off).await;
        assert_eq!(off, original);
        assert!(!root.join("brain.r8").exists());
        assert!(runtime.capture_completed_turn_for_harness(off, 0).is_err());
        assert_eq!(
            runtime
                .memory_history_preview()
                .unwrap()
                .destination_r8_path,
            root.join("brain.r8")
        );
        runtime
            .memory_context_enable(
                mc::MemoryContextMode::CaptureAndRecall,
                mc::mint_explicit_command_proof(),
            )
            .unwrap();
        let lease = runtime
            .compaction_capture_lease_at(SystemTime::now())
            .unwrap();
        assert_eq!(lease.provider_id.as_str(), PROVIDER_ID);
        assert_eq!(lease.project_id.as_str(), binding.scope().unwrap().key());
        let provider = runtime.extension_capture_provider(&lease).unwrap();
        let messages = vec![
            Arc::new(
                json!({"role":"user","content":"Remember cobalt_27 for synthetic regression tests"}),
            ),
            Arc::new(
                json!({"role":"assistant","content":[{"type":"thinking","thinking":"PRIVATE_SENTINEL"},{"type":"text","text":"cobalt_27 is synthetic fixture data."}]}),
            ),
        ];
        let history = super::super::terminal_capture_history(&lease, &messages, SystemTime::now());
        let capture =
            cc::build_chat_turn_capture(&lease.project_id, history, mc::RetentionClass::Standard)
                .unwrap();
        let id_bytes = *capture.capture_id.as_bytes();
        let note_id = format!("mem-cap-{}", capture.capture_id.to_hex());
        let p = provider.clone();
        tokio::task::spawn_blocking(move || {
            p.capture(capture).unwrap();
            assert_eq!(
                p.contains_capture(&id_bytes).unwrap(),
                CaptureCommitState::Committed
            );
        })
        .await
        .unwrap();
        let found = binding
            .search(ProjectMemoryQuery {
                content_contains: Some("cobalt_27".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, note_id);
        let fetched = binding.fetch(&[&note_id]).await.unwrap();
        assert!(fetched[0].content.contains("cobalt_27"));
        assert!(!fetched[0].content.contains("PRIVATE_SENTINEL"));
        let mut recalled = original.clone();
        runtime.apply_turn_memory_recall(&mut recalled).await;
        assert_eq!(
            recalled.len(),
            2,
            "real recall must fit production 150ms budget"
        );
        let why = runtime.memory_recall_why().unwrap();
        assert!(why
            .selected_memory_ids
            .iter()
            .any(|id| id.as_str() == note_id));
        assert!(recalled[0].to_string().contains("lower-authority"));

        let summary = cc::build_conversation_summary_capture(
            &lease.project_id,
            cc::CompactionSource {
                schema: cc::CompactionSchemaVersion::V1,
                project_id: lease.project_id.clone(),
                source_session_id: lease.session_id.clone(),
                first_turn_ordinal: 0,
                last_turn_ordinal: 1,
                source_digest: cc::MessageRangeDigest::from_bytes([3; 32]),
                summary_origin: cc::CompactionSummaryOrigin::LocalOnly,
                prompt_stack_digest: cc::PromptStackDigest::from_bytes([4; 32]),
                redaction: cc::RedactionPolicy::HostRedacted,
                content_classes: vec![agent_core::compaction::ContentClass::UserText],
                summarized_at: SystemTime::now(),
            },
            2,
            "LOCAL_ONLY_SENTINEL cobalt_27",
            agent_core::compaction::RedactionPolicy::PolicyExclusions,
            mc::RetentionClass::Standard,
        )
        .unwrap();
        let summary_id = format!("mem-cap-{}", summary.capture_id.to_hex());
        let p = provider.clone();
        tokio::task::spawn_blocking(move || p.capture_summary(summary).unwrap())
            .await
            .unwrap();
        // Restricted summaries are fully hidden, not descriptor-only notes.
        assert!(binding.fetch(&[&summary_id]).await.is_err());
        let mut next = vec![Arc::new(
            json!({"role":"user","content":"cobalt_27 details"}),
        )];
        runtime.apply_turn_memory_recall(&mut next).await;
        assert!(!next
            .iter()
            .any(|m| m.to_string().contains("LOCAL_ONLY_SENTINEL")));

        binding.forget(&note_id).await.unwrap();
        let p = provider.clone();
        tokio::task::spawn_blocking(move || {
            assert_eq!(
                p.contains_capture(&id_bytes).unwrap(),
                CaptureCommitState::Committed
            )
        })
        .await
        .unwrap();
        assert!(binding
            .search(ProjectMemoryQuery {
                content_contains: Some("cobalt_27".into()),
                ..Default::default()
            })
            .await
            .unwrap()
            .is_empty());
        runtime.memory_tool_capability().unwrap().disable();
        let mut retry = original.clone();
        runtime.apply_turn_memory_recall(&mut retry).await;
        assert_eq!(retry, original);
        tokio::task::spawn_blocking(move || assert!(provider.contains_capture(&id_bytes).is_err()))
            .await
            .unwrap();
        assert!(
            !base.path().join("memory").exists(),
            "no alternate legacy writes"
        );
    }
}
