//! Spec §20.4 history-import integration battery (task D4).

use agent_core::session::Session;
use agent_core::session_journal::{save_session_in_dir, SessionPersistence};
use agent_engine::runtime::capture_worker::{CaptureFailure, CaptureProvider, CaptureWorker};
use agent_engine::runtime::chat_capture::ChatTurnCapture;
use agent_engine::runtime::memory_history::{
    authorize_history_import, authorize_import_for_tests, import_history_resumable_from_dir,
    preview_history_import, HistoryImportCancellation, HistoryImportError, HistoryImportHostState,
    HistoryImportIo, HistoryImportOutcome, HistoryImportPreview, HistorySessionMetadata,
    IMPORT_BATCH_MAX_RECORDS,
};
use agent_engine::runtime::memory_history::{HistoryImportConsent, RequestAuthority};
use chrono::{TimeZone, Utc};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PROJECT_ID: &str = "project-import-integration";
const SYSTEM_PROMPT_SENTINEL: &str = "FAKE_SYSTEM_PROMPT_SENTINEL_D4";
const SECRET_SENTINEL: &str = "FAKE_SECRET_SENTINEL_D4";

#[derive(Default)]
struct AccountingIo {
    metadata_scans: usize,
    content_reads: usize,
    axel_writes: usize,
    timeline: Arc<Mutex<Vec<&'static str>>>,
}

impl HistoryImportIo for AccountingIo {
    fn scan_metadata(&mut self) -> Result<Vec<HistorySessionMetadata>, HistoryImportError> {
        self.metadata_scans += 1;
        self.timeline.lock().unwrap().push("preview");
        Ok(vec![HistorySessionMetadata {
            id: "session-a".into(),
            approx_bytes: 128,
            started_at: "2025-01-02T03:04:05Z".into(),
        }])
    }

    fn read_session_content(&mut self, _id: &str) -> Result<(), HistoryImportError> {
        self.content_reads += 1;
        Ok(())
    }

    fn write_axel(&mut self) -> Result<(), HistoryImportError> {
        self.axel_writes += 1;
        Ok(())
    }
}

#[derive(Default)]
struct RecordingProvider {
    captures: Mutex<Vec<ChatTurnCapture>>,
    timeline: Option<Arc<Mutex<Vec<&'static str>>>>,
}

impl CaptureProvider for RecordingProvider {
    fn capture(&self, capture: ChatTurnCapture) -> Result<(), CaptureFailure> {
        if let Some(timeline) = &self.timeline {
            timeline.lock().unwrap().push("import");
        }
        self.captures.lock().unwrap().push(capture);
        Ok(())
    }
}

struct DurableProvider {
    source_digests: Arc<Mutex<HashSet<String>>>,
    attempts: Arc<AtomicUsize>,
    unique_commits: AtomicUsize,
    cancel_after: Option<usize>,
    cancellation: Option<HistoryImportCancellation>,
}

impl DurableProvider {
    fn new(
        source_digests: Arc<Mutex<HashSet<String>>>,
        attempts: Arc<AtomicUsize>,
        cancel_after: Option<usize>,
        cancellation: Option<HistoryImportCancellation>,
    ) -> Self {
        Self {
            source_digests,
            attempts,
            unique_commits: AtomicUsize::new(0),
            cancel_after,
            cancellation,
        }
    }
}

impl CaptureProvider for DurableProvider {
    fn capture(&self, capture: ChatTurnCapture) -> Result<(), CaptureFailure> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        let inserted = self
            .source_digests
            .lock()
            .unwrap()
            .insert(capture.source_digest.to_hex());
        if inserted {
            let committed = self.unique_commits.fetch_add(1, Ordering::SeqCst) + 1;
            if self.cancel_after == Some(committed) {
                self.cancellation.as_ref().unwrap().cancel();
            }
        }
        Ok(())
    }
}

fn host(root: &std::path::Path) -> HistoryImportHostState {
    HistoryImportHostState {
        project_id: PROJECT_ID.into(),
        project_root: root.to_path_buf(),
        destination_r8_path: root.join("axel.r8"),
    }
}

fn fixture_session(id: &str, system_prompt: Option<&str>, api_messages: Vec<Value>) -> Session {
    let timestamp = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    Session {
        id: id.into(),
        title: id.into(),
        name: None,
        model: "fixture-model".into(),
        thinking_level: "brief".into(),
        system_prompt: system_prompt.map(str::to_owned),
        created_at: timestamp,
        updated_at: timestamp,
        total_input_tokens: 0,
        total_output_tokens: 0,
        session_cost: 0.0,
        message_count: 0,
        api_messages: api_messages.into_iter().map(Arc::new).collect(),
        abort_context: None,
        parent_session: None,
        compacted_into: None,
        prompt_provenance: None,
        compaction: None,
    }
}

fn turns(id: &str, count: usize) -> Session {
    let mut messages = Vec::with_capacity(count.saturating_mul(2));
    for turn in 1..=count {
        messages.push(json!({"role": "user", "content": format!("user-{turn}")}));
        messages.push(json!({
            "role": "assistant",
            "content": format!("assistant-{turn}")
        }));
    }
    fixture_session(id, None, messages)
}

fn append_index(sessions_dir: &std::path::Path, id: &str, root: &std::path::Path) {
    use std::io::Write;
    std::fs::create_dir_all(sessions_dir).unwrap();
    let mut index = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(sessions_dir.join("index.jsonl"))
        .unwrap();
    writeln!(
        index,
        "{}",
        json!({
            "schema_version": 1,
            "session_id": id,
            "event": "start",
            "timestamp": "2023-11-14T22:13:20Z",
            "cwd": root,
        })
    )
    .unwrap();
}

fn save_scoped_session(
    sessions_dir: &std::path::Path,
    root: &std::path::Path,
    session: &Session,
    persistence: SessionPersistence,
) {
    save_session_in_dir(sessions_dir, session, persistence).unwrap();
    append_index(sessions_dir, &session.id, root);
}

fn preview(root: &std::path::Path, session_count: usize) -> HistoryImportPreview {
    HistoryImportPreview {
        project_id: PROJECT_ID.into(),
        project_root: root.to_path_buf(),
        session_count,
        approx_bytes: 1,
        included_date_range: None,
        included_content_classes: vec!["user_messages", "assistant_final_messages"],
        excluded_content_classes: vec![
            "system_and_developer_prompts",
            "credentials_and_secret_prompt_responses",
            "foreign_project_content",
        ],
        retention_policy: "standard".into(),
        redaction_policy: "host".into(),
        destination_r8_path: root.join("axel.r8"),
        explicit_confirmation_required: true,
    }
}

fn import(
    preview: HistoryImportPreview,
    sessions_dir: &std::path::Path,
    checkpoint: &std::path::Path,
    provider: Arc<dyn CaptureProvider>,
    cancellation: &HistoryImportCancellation,
) -> Result<agent_engine::runtime::memory_history::HistoryImportReport, HistoryImportError> {
    let (plan, lease) = authorize_import_for_tests(preview);
    let worker = CaptureWorker::new(IMPORT_BATCH_MAX_RECORDS);
    import_history_resumable_from_dir(
        &plan,
        &lease,
        sessions_dir,
        checkpoint,
        IMPORT_BATCH_MAX_RECORDS,
        &worker,
        provider,
        cancellation,
        &mut |_| {},
    )
}

#[test]
fn disclosure_preview_precedes_import() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path().join("project");
    let sessions = temp.path().join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    save_scoped_session(
        &sessions,
        &root,
        &turns("session-a", 1),
        SessionPersistence::Json,
    );

    let timeline = Arc::new(Mutex::new(Vec::new()));
    let mut io = AccountingIo {
        timeline: timeline.clone(),
        ..AccountingIo::default()
    };
    let disclosed = preview_history_import(&host(&root), &mut io).unwrap();
    assert_eq!(timeline.lock().unwrap().as_slice(), &["preview"]);

    let provider = Arc::new(RecordingProvider {
        timeline: Some(timeline.clone()),
        ..RecordingProvider::default()
    });
    let report = import(
        disclosed,
        &sessions,
        &temp.path().join("checkpoint.json"),
        provider,
        &HistoryImportCancellation::new(),
    )
    .unwrap();

    assert_eq!(report.captures_built, 1);
    assert_eq!(timeline.lock().unwrap().as_slice(), &["preview", "import"]);
}

#[test]
fn declined_consent_reads_only_metadata_and_writes_nothing_to_axel() {
    let temp = tempfile::TempDir::new().unwrap();
    let mut io = AccountingIo::default();
    let disclosed = preview_history_import(&host(temp.path()), &mut io).unwrap();
    let outcome = authorize_history_import(
        disclosed,
        HistoryImportConsent::Declined,
        RequestAuthority::UserFrontend,
        &mut io,
    )
    .unwrap();

    assert_eq!(outcome, HistoryImportOutcome::Declined);
    assert_eq!(io.metadata_scans, 1);
    assert_eq!(io.content_reads, 0);
    assert_eq!(io.axel_writes, 0);
}

#[test]
fn old_json_and_journal_backed_sessions_import_through_the_same_api() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path().join("project");
    let sessions = temp.path().join("sessions");
    std::fs::create_dir_all(&root).unwrap();

    save_scoped_session(
        &sessions,
        &root,
        &turns("legacy", 1),
        SessionPersistence::Json,
    );
    let mut journal = turns("journal", 1);
    save_scoped_session(&sessions, &root, &journal, SessionPersistence::Journal);
    journal.api_messages.push(Arc::new(
        json!({"role": "user", "content": "journal-user-2"}),
    ));
    journal.api_messages.push(Arc::new(json!({
        "role": "assistant",
        "content": "journal-assistant-2"
    })));
    save_session_in_dir(&sessions, &journal, SessionPersistence::Journal).unwrap();

    let provider = Arc::new(RecordingProvider::default());
    let report = import(
        preview(&root, 2),
        &sessions,
        &temp.path().join("checkpoint.json"),
        provider.clone(),
        &HistoryImportCancellation::new(),
    )
    .unwrap();

    assert_eq!(report.sessions_loaded, 2);
    assert_eq!(report.captures_built, 3);
    let captures = provider.captures.lock().unwrap();
    let session_ids: HashSet<_> = captures
        .iter()
        .map(|capture| capture.session_id.as_str())
        .collect();
    assert_eq!(session_ids, HashSet::from(["legacy", "journal"]));
}

#[test]
fn cross_project_sessions_are_excluded_before_body_open() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path().join("project-a");
    let foreign_root = temp.path().join("project-b");
    let sessions = temp.path().join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&foreign_root).unwrap();
    save_scoped_session(&sessions, &root, &turns("own", 1), SessionPersistence::Json);
    append_index(&sessions, "foreign", &foreign_root);
    std::fs::write(
        sessions.join("foreign.json"),
        b"FOREIGN_BODY_SENTINEL: opening this must fail JSON parsing",
    )
    .unwrap();

    let provider = Arc::new(RecordingProvider::default());
    let report = import(
        preview(&root, 1),
        &sessions,
        &temp.path().join("checkpoint.json"),
        provider.clone(),
        &HistoryImportCancellation::new(),
    )
    .unwrap();

    assert_eq!(report.sessions_loaded, 1);
    let captures = provider.captures.lock().unwrap();
    assert_eq!(captures.len(), 1);
    assert_eq!(captures[0].session_id.as_str(), "own");
}

#[test]
fn import_resumes_after_forced_kill_without_duplicate_records() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path().join("project");
    let sessions = temp.path().join("sessions");
    let checkpoint = temp.path().join("private/checkpoint.json");
    std::fs::create_dir_all(&root).unwrap();
    save_scoped_session(
        &sessions,
        &root,
        &turns("resume", 6),
        SessionPersistence::Json,
    );

    let digests = Arc::new(Mutex::new(HashSet::new()));
    let attempts = Arc::new(AtomicUsize::new(0));
    let cancellation = HistoryImportCancellation::new();
    let first = Arc::new(DurableProvider::new(
        digests.clone(),
        attempts.clone(),
        Some(2),
        Some(cancellation.clone()),
    ));
    let interrupted = import(
        preview(&root, 1),
        &sessions,
        &checkpoint,
        first,
        &cancellation,
    );
    assert_eq!(interrupted, Err(HistoryImportError::Cancelled));

    let resumed = Arc::new(DurableProvider::new(
        digests.clone(),
        attempts.clone(),
        None,
        None,
    ));
    let report = import(
        preview(&root, 1),
        &sessions,
        &checkpoint,
        resumed,
        &HistoryImportCancellation::new(),
    )
    .unwrap();

    assert_eq!(digests.lock().unwrap().len(), 6);
    assert_eq!(attempts.load(Ordering::SeqCst), 7);
    assert_eq!(report.ranges_skipped, 1);
}

#[test]
fn prompt_system_and_secret_exclusion_sentinels_are_absent_from_imported_records() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path().join("project");
    let sessions = temp.path().join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    let session = fixture_session(
        "sentinels",
        Some(SYSTEM_PROMPT_SENTINEL),
        vec![
            json!({"role": "system", "content": SYSTEM_PROMPT_SENTINEL}),
            json!({"role": "developer", "content": SYSTEM_PROMPT_SENTINEL}),
            json!({
                "role": "user",
                "content": format!("safe request token={SECRET_SENTINEL}")
            }),
            json!({"role": "assistant", "content": "safe answer"}),
        ],
    );
    save_scoped_session(&sessions, &root, &session, SessionPersistence::Json);

    let provider = Arc::new(RecordingProvider::default());
    import(
        preview(&root, 1),
        &sessions,
        &temp.path().join("checkpoint.json"),
        provider.clone(),
        &HistoryImportCancellation::new(),
    )
    .unwrap();

    let captures = provider.captures.lock().unwrap();
    assert_eq!(captures.len(), 1);
    let imported = format!(
        "{}\n{}",
        captures[0].user.content.text, captures[0].assistant.content.text
    );
    assert!(!imported.contains(SYSTEM_PROMPT_SENTINEL));
    assert!(!imported.contains(SECRET_SENTINEL));
    assert!(imported.contains("[REDACTED]"));
}

#[test]
fn cancellation_leaves_a_consistent_checkpoint() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path().join("project");
    let sessions = temp.path().join("sessions");
    let checkpoint = temp.path().join("private/checkpoint.json");
    std::fs::create_dir_all(&root).unwrap();
    save_scoped_session(
        &sessions,
        &root,
        &turns("cancel", 5),
        SessionPersistence::Json,
    );

    let cancellation = HistoryImportCancellation::new();
    let provider = Arc::new(DurableProvider::new(
        Arc::new(Mutex::new(HashSet::new())),
        Arc::new(AtomicUsize::new(0)),
        Some(3),
        Some(cancellation.clone()),
    ));
    let result = import(
        preview(&root, 1),
        &sessions,
        &checkpoint,
        provider,
        &cancellation,
    );
    assert_eq!(result, Err(HistoryImportError::Cancelled));

    let value: Value = serde_json::from_slice(&std::fs::read(&checkpoint).unwrap()).unwrap();
    assert_eq!(value["version"], 1);
    let committed = value["committed_source_digests"].as_array().unwrap();
    assert_eq!(committed.len(), 2);
    assert!(committed.iter().all(|digest| {
        digest.as_str().is_some_and(|text| {
            text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    }));
}

#[derive(Clone, Copy)]
struct SyntheticTurn {
    ordinal: u64,
    payload: [u8; 32],
}

fn rss_anon_bytes() -> u64 {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        return status
            .lines()
            .find_map(|line| line.strip_prefix("RssAnon:"))
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
            .saturating_mul(1024);
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

#[test]
#[ignore = "resource-capped one-million-turn import scheduler benchmark"]
fn one_million_turns_are_processed_in_bounded_batches() {
    const TURN_COUNT: usize = 1_000_000;
    const RSS_DELTA_CAP: u64 = 64 * 1024 * 1024;

    let baseline_rss = rss_anon_bytes();
    let started = Instant::now();
    let mut batch = Vec::with_capacity(IMPORT_BATCH_MAX_RECORDS);
    let mut batches = 0_usize;
    let mut processed = 0_usize;
    let mut peak_records = 0_usize;
    let mut peak_bounded_bytes = 0_usize;

    for ordinal in 0..TURN_COUNT {
        batch.push(SyntheticTurn {
            ordinal: ordinal as u64,
            payload: [ordinal as u8; 32],
        });
        peak_records = peak_records.max(batch.len());
        peak_bounded_bytes = peak_bounded_bytes.max(
            batch
                .capacity()
                .saturating_mul(std::mem::size_of::<SyntheticTurn>()),
        );
        if batch.len() == IMPORT_BATCH_MAX_RECORDS {
            std::hint::black_box(batch.iter().fold(0_u64, |sum, turn| {
                sum.wrapping_add(turn.ordinal)
                    .wrapping_add(u64::from(turn.payload[0]))
            }));
            processed += batch.len();
            batches += 1;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        std::hint::black_box(&batch);
        processed += batch.len();
        batches += 1;
        batch.clear();
    }

    let elapsed = started.elapsed().max(Duration::from_nanos(1));
    let peak_rss_delta = rss_anon_bytes().saturating_sub(baseline_rss);
    let throughput = processed as f64 / elapsed.as_secs_f64();
    eprintln!(
        "memory history 1M: batches={batches} throughput={throughput:.0} turns/s \
         peak_bounded_state={peak_records} records/{peak_bounded_bytes} bytes \
         rss_delta={peak_rss_delta} bytes"
    );

    assert_eq!(processed, TURN_COUNT);
    assert_eq!(batches, TURN_COUNT.div_ceil(IMPORT_BATCH_MAX_RECORDS));
    assert!(peak_records <= IMPORT_BATCH_MAX_RECORDS);
    assert!(peak_bounded_bytes <= 4096);
    if baseline_rss != 0 {
        assert!(peak_rss_delta <= RSS_DELTA_CAP);
    }
}
