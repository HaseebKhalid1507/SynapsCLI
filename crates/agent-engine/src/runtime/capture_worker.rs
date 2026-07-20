//! Nonblocking, bounded dispatch of completed memory captures.

use super::chat_capture::{
    build_chat_turn_capture, build_conversation_summary_capture, CaptureBuildError,
    ChatTurnCapture, ConversationSummaryCapture, SummaryCaptureBuildError, TerminalTurnHistory,
};
use super::memory_context::{ContextProviderId, MemoryContextLease, RetentionClass, TurnCapture};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;

/// Outcome of an idempotency-key query after an ambiguous capture call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureCommitState {
    Committed,
    Absent,
}

/// Provider seam. Implementations must treat every capture id as an
/// idempotency key. Summary capture is additive so existing turn providers
/// keep source compatibility while adopting the C4 record class.
pub trait CaptureProvider: Send + Sync + 'static {
    fn capture(&self, capture: ChatTurnCapture) -> Result<(), CaptureFailure>;

    /// Query whether this idempotency key is already durable after an
    /// interrupted/ambiguous call. Providers that cannot query return false;
    /// no automatic blind retry is performed in either case.
    fn contains_capture(
        &self,
        _capture_id: &[u8; 32],
    ) -> Result<CaptureCommitState, CaptureFailure> {
        Ok(CaptureCommitState::Absent)
    }

    fn capture_summary(&self, _capture: ConversationSummaryCapture) -> Result<(), CaptureFailure> {
        Err(CaptureFailure {
            code: "summary_capture_unsupported",
        })
    }
}

/// Content-free failure metadata suitable for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureFailure {
    pub code: &'static str,
}

enum CapturePayload {
    Turn(ChatTurnCapture),
    Summary(ConversationSummaryCapture),
}

impl CapturePayload {
    fn id_bytes(&self) -> [u8; 32] {
        match self {
            Self::Turn(capture) => *capture.capture_id.as_bytes(),
            Self::Summary(capture) => *capture.capture_id.as_bytes(),
        }
    }

    fn dispatch(&self, provider: &dyn CaptureProvider) -> Result<(), CaptureFailure> {
        match self {
            Self::Turn(capture) => provider.capture(capture.clone()),
            Self::Summary(capture) => provider.capture_summary(capture.clone()),
        }
    }

    fn query_committed(
        &self,
        provider: &dyn CaptureProvider,
    ) -> Result<CaptureCommitState, CaptureFailure> {
        match self {
            Self::Turn(capture) => provider.contains_capture(capture.capture_id.as_bytes()),
            // Summary retry/query support is additive and not part of the C5
            // terminal-turn crash gate.
            Self::Summary(_) => Ok(CaptureCommitState::Absent),
        }
    }
}

struct CaptureJob {
    provider_id: ContextProviderId,
    provider: Arc<dyn CaptureProvider>,
    payload: CapturePayload,
}

/// Fixed-capacity worker. Enqueue is always `try_send`; persistence never runs
/// on the turn-completion or compaction-transition path.
pub struct CaptureWorker {
    sender: mpsc::SyncSender<CaptureJob>,
    dropped: Arc<AtomicU64>,
    failures: Arc<AtomicU64>,
}

impl CaptureWorker {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "capture queue capacity must be nonzero");
        let (sender, receiver) = mpsc::sync_channel::<CaptureJob>(capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        let failures = Arc::new(AtomicU64::new(0));
        let worker_failures = Arc::clone(&failures);
        thread::Builder::new()
            .name("memory-capture".into())
            .spawn(move || {
                let mut submitted = HashSet::new();
                while let Ok(job) = receiver.recv() {
                    let id = (job.provider_id.clone(), job.payload.id_bytes());
                    if submitted.contains(&id) {
                        continue;
                    }
                    if job.payload.dispatch(job.provider.as_ref()).is_err() {
                        // The call may have committed before its acknowledgement
                        // was lost. Reconcile by idempotency key before accepting
                        // an explicit retry; never blindly dispatch it again.
                        match job.payload.query_committed(job.provider.as_ref()) {
                            Ok(CaptureCommitState::Committed) => {
                                submitted.insert(id);
                            }
                            Ok(CaptureCommitState::Absent) | Err(_) => {
                                worker_failures.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    } else {
                        submitted.insert(id);
                    }
                }
            })
            .expect("capture worker spawn");
        Self {
            sender,
            dropped,
            failures,
        }
    }

    /// Apply lease capability and capture gates, build the bounded payload, and
    /// attempt a nonblocking enqueue. `Ok(false)` means capture was gated off.
    pub fn submit_terminal(
        &self,
        lease: &MemoryContextLease,
        history: TerminalTurnHistory,
        retention: RetentionClass,
        provider: Arc<dyn CaptureProvider>,
    ) -> Result<bool, CaptureBuildError> {
        if lease.mode.turn_capture() != TurnCapture::Enabled
            || lease.project_id != history.project_id
            || lease.session_id != history.session_id
        {
            return Ok(false);
        }
        let capture = build_chat_turn_capture(&lease.project_id, history, retention)?;
        let job = CaptureJob {
            provider_id: lease.provider_id.clone(),
            provider,
            payload: CapturePayload::Turn(capture),
        };
        match self.sender.try_send(job) {
            Ok(()) => Ok(true),
            Err(mpsc::TrySendError::Full(_)) | Err(mpsc::TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                Ok(true)
            }
        }
    }

    /// Apply the same exact lease gate as turn capture, build a bounded
    /// first-class compaction memory, and attempt a nonblocking enqueue.
    pub fn submit_summary(
        &self,
        lease: &MemoryContextLease,
        source: super::chat_capture::CompactionSource,
        source_message_count: usize,
        summary_text: &str,
        redaction_policy: agent_core::compaction::RedactionPolicy,
        retention: RetentionClass,
        provider: Arc<dyn CaptureProvider>,
    ) -> Result<bool, SummaryCaptureBuildError> {
        if lease.mode.turn_capture() != TurnCapture::Enabled
            || lease.project_id != source.project_id
        {
            return Ok(false);
        }
        let capture = build_conversation_summary_capture(
            &lease.project_id,
            source,
            source_message_count,
            summary_text,
            redaction_policy,
            retention,
        )?;
        let job = CaptureJob {
            provider_id: lease.provider_id.clone(),
            provider,
            payload: CapturePayload::Summary(capture),
        };
        match self.sender.try_send(job) {
            Ok(()) => Ok(true),
            Err(mpsc::TrySendError::Full(_)) | Err(mpsc::TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                Ok(true)
            }
        }
    }

    pub fn overflow_drops(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
    pub fn provider_failures(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::chat_capture::{CanonicalCaptureItem, CaptureContentClass};
    use crate::runtime::memory_context::{
        mint_explicit_command_proof, CapturePolicy, MemoryContextMode, MemoryLeaseId, ProjectId,
        RecallPolicy, SessionId,
    };
    use agent_core::core::disclosure::DisclosureClass;
    use agent_core::TurnOutcome;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;
    use std::time::{Duration, Instant, SystemTime};

    struct ProviderDouble {
        calls: AtomicU64,
        queries: AtomicU64,
        fail: AtomicBool,
        committed: AtomicBool,
        block: Mutex<Option<mpsc::Receiver<()>>>,
    }

    impl CaptureProvider for ProviderDouble {
        fn capture(&self, _capture: ChatTurnCapture) -> Result<(), CaptureFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(receiver) = self.block.lock().expect("lock").take() {
                let _ = receiver.recv();
            }
            if self.fail.load(Ordering::SeqCst) {
                Err(CaptureFailure {
                    code: "fixture_failure",
                })
            } else {
                self.committed.store(true, Ordering::SeqCst);
                Ok(())
            }
        }

        fn contains_capture(
            &self,
            _capture_id: &[u8; 32],
        ) -> Result<CaptureCommitState, CaptureFailure> {
            self.queries.fetch_add(1, Ordering::SeqCst);
            Ok(if self.committed.load(Ordering::SeqCst) {
                CaptureCommitState::Committed
            } else {
                CaptureCommitState::Absent
            })
        }
    }

    fn lease() -> MemoryContextLease {
        let now = SystemTime::now();
        MemoryContextLease::grant(
            MemoryLeaseId::parse("lease-c3").unwrap(),
            SessionId::parse("session-c3").unwrap(),
            ProjectId::parse("project-c3").unwrap(),
            ContextProviderId::parse("provider-c3").unwrap(),
            MemoryContextMode::CaptureOnly,
            CapturePolicy::EligibleTurnsOnly,
            RecallPolicy::BoundedPerPrompt,
            mint_explicit_command_proof(),
            now,
            None,
        )
        .unwrap()
    }

    fn history(turn: u64) -> TerminalTurnHistory {
        let project_id = ProjectId::parse("project-c3").unwrap();
        let started_at = SystemTime::UNIX_EPOCH + Duration::from_secs(turn);
        TerminalTurnHistory {
            project_id: project_id.clone(),
            session_id: SessionId::parse("session-c3").unwrap(),
            turn_id: super::super::memory_context::TurnId::parse(&format!("turn-{turn}")).unwrap(),
            turn_ordinal: turn,
            started_at,
            completed_at: started_at + Duration::from_millis(1),
            outcome: TurnOutcome::Completed,
            items: vec![
                CanonicalCaptureItem {
                    project_id: project_id.clone(),
                    class: CaptureContentClass::UserMessage,
                    disclosure: DisclosureClass::ModelVisible,
                    sensitivity: super::super::chat_capture::Sensitivity::Normal,
                    text: "user".into(),
                    tool_name: None,
                },
                CanonicalCaptureItem {
                    project_id,
                    class: CaptureContentClass::AssistantFinal,
                    disclosure: DisclosureClass::ModelVisible,
                    sensitivity: super::super::chat_capture::Sensitivity::Normal,
                    text: "assistant".into(),
                    tool_name: None,
                },
            ],
            compaction: None,
        }
    }

    fn wait_until(predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !predicate() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        assert!(predicate());
    }

    #[test]
    fn memory_duplicate_capture_id_idempotent_at_engine_seam() {
        let provider = Arc::new(ProviderDouble {
            calls: AtomicU64::new(0),
            queries: AtomicU64::new(0),
            fail: AtomicBool::new(false),
            committed: AtomicBool::new(false),
            block: Mutex::new(None),
        });
        let worker = CaptureWorker::new(4);
        worker
            .submit_terminal(
                &lease(),
                history(1),
                RetentionClass::Standard,
                provider.clone(),
            )
            .unwrap();
        worker
            .submit_terminal(
                &lease(),
                history(1),
                RetentionClass::Standard,
                provider.clone(),
            )
            .unwrap();
        wait_until(|| provider.calls.load(Ordering::SeqCst) == 1);
        thread::sleep(Duration::from_millis(10));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn memory_overflow_drops_exactly_and_never_blocks_completion() {
        let (release_tx, release_rx) = mpsc::channel();
        let provider = Arc::new(ProviderDouble {
            calls: AtomicU64::new(0),
            queries: AtomicU64::new(0),
            fail: AtomicBool::new(false),
            committed: AtomicBool::new(false),
            block: Mutex::new(Some(release_rx)),
        });
        let worker = CaptureWorker::new(1);
        worker
            .submit_terminal(
                &lease(),
                history(1),
                RetentionClass::Standard,
                provider.clone(),
            )
            .unwrap();
        wait_until(|| provider.calls.load(Ordering::SeqCst) == 1);
        worker
            .submit_terminal(
                &lease(),
                history(2),
                RetentionClass::Standard,
                provider.clone(),
            )
            .unwrap();
        let started = Instant::now();
        for turn in 3..103 {
            worker
                .submit_terminal(
                    &lease(),
                    history(turn),
                    RetentionClass::Standard,
                    provider.clone(),
                )
                .unwrap();
        }
        assert!(started.elapsed() < Duration::from_millis(50));
        assert_eq!(worker.overflow_drops(), 100);
        release_tx.send(()).unwrap();
    }

    #[test]
    fn memory_possibly_committed_capture_is_queried_before_retry() {
        struct AckLostAfterCommit {
            calls: AtomicU64,
            queries: AtomicU64,
            committed: AtomicBool,
        }
        impl CaptureProvider for AckLostAfterCommit {
            fn capture(&self, _capture: ChatTurnCapture) -> Result<(), CaptureFailure> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.committed.store(true, Ordering::SeqCst);
                Err(CaptureFailure { code: "ack_lost" })
            }

            fn contains_capture(
                &self,
                _capture_id: &[u8; 32],
            ) -> Result<CaptureCommitState, CaptureFailure> {
                self.queries.fetch_add(1, Ordering::SeqCst);
                Ok(if self.committed.load(Ordering::SeqCst) {
                    CaptureCommitState::Committed
                } else {
                    CaptureCommitState::Absent
                })
            }
        }

        let provider = Arc::new(AckLostAfterCommit {
            calls: AtomicU64::new(0),
            queries: AtomicU64::new(0),
            committed: AtomicBool::new(false),
        });
        let worker = CaptureWorker::new(2);
        for _ in 0..2 {
            worker
                .submit_terminal(
                    &lease(),
                    history(99),
                    RetentionClass::Standard,
                    provider.clone(),
                )
                .unwrap();
            wait_until(|| provider.queries.load(Ordering::SeqCst) == 1);
        }
        thread::sleep(Duration::from_millis(10));
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            1,
            "must not blindly retry"
        );
        assert_eq!(provider.queries.load(Ordering::SeqCst), 1);
        assert_eq!(worker.provider_failures(), 0, "query proved the commit");
    }

    #[test]
    fn memory_failed_capture_id_can_be_explicitly_retried() {
        let provider = Arc::new(ProviderDouble {
            calls: AtomicU64::new(0),
            queries: AtomicU64::new(0),
            fail: AtomicBool::new(true),
            committed: AtomicBool::new(false),
            block: Mutex::new(None),
        });
        let worker = CaptureWorker::new(2);
        worker
            .submit_terminal(
                &lease(),
                history(7),
                RetentionClass::Standard,
                provider.clone(),
            )
            .unwrap();
        wait_until(|| worker.provider_failures() == 1);
        provider.fail.store(false, Ordering::SeqCst);
        worker
            .submit_terminal(
                &lease(),
                history(7),
                RetentionClass::Standard,
                provider.clone(),
            )
            .unwrap();
        wait_until(|| provider.calls.load(Ordering::SeqCst) == 2);
    }

    #[test]
    fn memory_provider_capture_failure_leaves_typed_terminal_turn_completed() {
        let provider = Arc::new(ProviderDouble {
            calls: AtomicU64::new(0),
            queries: AtomicU64::new(0),
            fail: AtomicBool::new(true),
            committed: AtomicBool::new(false),
            block: Mutex::new(None),
        });
        let worker = CaptureWorker::new(1);
        let terminal = history(1);
        assert_eq!(terminal.outcome, TurnOutcome::Completed);
        assert!(worker
            .submit_terminal(
                &lease(),
                terminal,
                RetentionClass::Standard,
                provider.clone()
            )
            .unwrap());
        wait_until(|| worker.provider_failures() == 1);
    }
}
