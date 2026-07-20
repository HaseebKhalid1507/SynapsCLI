//! Nonblocking, bounded dispatch of completed memory captures.

use super::chat_capture::{
    build_chat_turn_capture, CaptureBuildError, ChatTurnCapture, TerminalTurnHistory,
};
use super::memory_context::{ContextProviderId, MemoryContextLease, RetentionClass, TurnCapture};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;

/// Provider seam. Implementations must treat `capture_id` as an idempotency key.
pub trait CaptureProvider: Send + Sync + 'static {
    fn capture(&self, capture: ChatTurnCapture) -> Result<(), CaptureFailure>;
}

/// Content-free failure metadata suitable for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureFailure {
    pub code: &'static str,
}

struct CaptureJob {
    provider_id: ContextProviderId,
    provider: Arc<dyn CaptureProvider>,
    capture: ChatTurnCapture,
}

/// Fixed-capacity worker. Enqueue is always `try_send`; persistence never runs
/// on the turn-completion path.
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
                    let id = (job.provider_id.clone(), *job.capture.capture_id.as_bytes());
                    if submitted.contains(&id) {
                        continue;
                    }
                    if job.provider.capture(job.capture).is_err() {
                        // No automatic retry: completion stays decoupled. Because a
                        // failed ID is not committed, an explicit retry remains safe.
                        worker_failures.fetch_add(1, Ordering::Relaxed);
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
            capture,
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
        fail: AtomicBool,
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
                Ok(())
            }
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
            fail: AtomicBool::new(false),
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
            fail: AtomicBool::new(false),
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
    fn memory_failed_capture_id_can_be_explicitly_retried() {
        let provider = Arc::new(ProviderDouble {
            calls: AtomicU64::new(0),
            fail: AtomicBool::new(true),
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
            fail: AtomicBool::new(true),
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
