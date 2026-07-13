use crate::prompt::QualifiedModelId;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnforcementMode {
    Off,
    Advisory,
    Enforced,
}

#[derive(Clone, Debug, Serialize)]
pub struct DelegationPolicy {
    pub mode: EnforcementMode,
    foreground: QualifiedModelId,
    allowed_providers: BTreeSet<String>,
    allowed_models: BTreeSet<QualifiedModelId>,
    pub max_concurrent_workers: usize,
    pub max_total_workers: usize,
}
impl DelegationPolicy {
    pub fn new(
        mode: EnforcementMode,
        foreground: QualifiedModelId,
        models: impl IntoIterator<Item = QualifiedModelId>,
        concurrent: usize,
        total: usize,
    ) -> Self {
        let allowed_models: BTreeSet<_> = models.into_iter().collect();
        let allowed_providers = allowed_models
            .iter()
            .map(|m| m.provider().to_owned())
            .collect();
        Self {
            mode,
            foreground,
            allowed_providers,
            allowed_models,
            max_concurrent_workers: concurrent,
            max_total_workers: total,
        }
    }
    pub fn enforced(
        foreground: QualifiedModelId,
        models: impl IntoIterator<Item = QualifiedModelId>,
        concurrent: usize,
        total: usize,
    ) -> Self {
        Self::new(
            EnforcementMode::Enforced,
            foreground,
            models,
            concurrent,
            total,
        )
    }
    pub fn digest(&self) -> String {
        use sha2::{Digest, Sha256};
        let encoded = serde_json::to_vec(self).expect("delegation policy is serializable");
        format!("{:x}", Sha256::digest(encoded))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerRole {
    Planner,
    Implementer,
    Tester,
    Reviewer,
    Researcher,
    Debugger,
}
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode", content = "scopes")]
pub enum WorkerWritePolicy {
    ReadOnly,
    IsolatedWorktree,
    NonOverlappingPaths(Vec<String>),
}
#[derive(Clone, Copy, Debug)]
pub enum WorkerTerminal {
    Completed,
    Failed,
    TimedOut,
}
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct WorkerHandle(String);
#[derive(Debug, Eq, PartialEq)]
pub enum CompletionGate {
    Allowed,
    Warning { workers: Vec<String> },
    Blocked { workers: Vec<String> },
}
#[derive(Debug, Eq, PartialEq)]
pub enum ScopeDecision {
    Allowed,
    Warning { workers: Vec<String> },
    ReconciliationRequired { workers: Vec<String> },
}
#[derive(Debug)]
pub struct DispatchDenied {
    code: &'static str,
}
impl DispatchDenied {
    pub fn code(&self) -> &'static str {
        self.code
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Dispatched,
    Running,
    Terminal,
    Collected,
    Reconciled,
}
#[derive(Debug)]
struct Worker {
    state: State,
    role: WorkerRole,
    writes: WorkerWritePolicy,
    unchanged_polls: usize,
    steered: bool,
    last_fingerprint: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct OrchestrationEvent {
    pub name: &'static str,
    pub worker_id: Option<String>,
    pub worker_role: Option<WorkerRole>,
    pub reason_code: Option<&'static str>,
}

pub struct WorkerRegistry {
    policy: DelegationPolicy,
    workers: BTreeMap<WorkerHandle, Worker>,
    total: usize,
    events: VecDeque<OrchestrationEvent>,
    dropped_events: u64,
}
impl WorkerRegistry {
    pub fn new(policy: DelegationPolicy) -> Self {
        Self {
            policy,
            workers: BTreeMap::new(),
            total: 0,
            events: VecDeque::new(),
            dropped_events: 0,
        }
    }
    pub fn foreground_model(&self) -> &QualifiedModelId {
        &self.policy.foreground
    }
    pub fn total_dispatched(&self) -> usize {
        self.total
    }
    pub fn telemetry(&self) -> &VecDeque<OrchestrationEvent> {
        &self.events
    }
    pub fn dropped_telemetry(&self) -> u64 {
        self.dropped_events
    }
    fn emit(&mut self, event: OrchestrationEvent) {
        const CAPACITY: usize = 256;
        if self.events.len() == CAPACITY {
            self.events.pop_front();
            self.dropped_events += 1;
        }
        self.events.push_back(event);
    }
    pub fn mode(&self) -> EnforcementMode {
        self.policy.mode
    }
    pub fn rollback_dispatch(&mut self, h: &WorkerHandle) -> Result<(), &'static str> {
        let worker = self.workers.get(h).ok_or("unknown worker")?;
        if worker.state != State::Dispatched {
            return Err("worker already started");
        }
        self.workers.remove(h);
        self.total = self.total.saturating_sub(1);
        self.emit(OrchestrationEvent {
            name: "worker.dispatch_rolled_back",
            worker_id: Some(h.0.clone()),
            worker_role: None,
            reason_code: Some("post_authorization_failure"),
        });
        Ok(())
    }
    pub fn validate_dispatch(&self, model: &QualifiedModelId) -> Result<(), DispatchDenied> {
        if self.policy.mode != EnforcementMode::Enforced {
            return Ok(());
        }
        let deny = if !self.policy.allowed_providers.contains(model.provider()) {
            Some("provider_not_allowed")
        } else if !self.policy.allowed_models.contains(model) {
            Some("model_not_allowed")
        } else if self.total >= self.policy.max_total_workers {
            Some("total_limit")
        } else if self
            .workers
            .values()
            .filter(|w| matches!(w.state, State::Dispatched | State::Running))
            .count()
            >= self.policy.max_concurrent_workers
        {
            Some("concurrency_limit")
        } else {
            None
        };
        deny.map_or(Ok(()), |code| Err(DispatchDenied { code }))
    }
    pub fn authorize_dispatch(
        &mut self,
        model: &QualifiedModelId,
        role: WorkerRole,
        writes: WorkerWritePolicy,
    ) -> Result<WorkerHandle, DispatchDenied> {
        self.emit(OrchestrationEvent {
            name: "worker.dispatch_requested",
            worker_id: None,
            worker_role: None,
            reason_code: None,
        });
        if let Err(error) = self.validate_dispatch(model) {
            self.emit(OrchestrationEvent {
                name: "worker.dispatch_denied",
                worker_id: None,
                worker_role: None,
                reason_code: Some(error.code),
            });
            return Err(error);
        }
        self.total += 1;
        let h = WorkerHandle(format!("worker-{}", self.total));
        self.workers.insert(
            h.clone(),
            Worker {
                state: State::Dispatched,
                role,
                writes,
                unchanged_polls: 0,
                steered: false,
                last_fingerprint: None,
            },
        );
        self.emit(OrchestrationEvent {
            name: "worker.dispatched",
            worker_id: Some(h.0.clone()),
            worker_role: Some(role),
            reason_code: None,
        });
        Ok(h)
    }
    fn transition(
        &mut self,
        h: &WorkerHandle,
        from: &[State],
        to: State,
        event: &'static str,
    ) -> Result<(), &'static str> {
        let w = self.workers.get_mut(h).ok_or("unknown worker")?;
        if !from.contains(&w.state) {
            return Err("invalid lifecycle transition");
        }
        w.state = to;
        let role = w.role;
        self.emit(OrchestrationEvent {
            name: event,
            worker_id: Some(h.0.clone()),
            worker_role: Some(role),
            reason_code: None,
        });
        Ok(())
    }
    pub fn mark_running(&mut self, h: &WorkerHandle) -> Result<(), &'static str> {
        self.transition(h, &[State::Dispatched], State::Running, "worker.running")
    }
    pub fn poll(&mut self, h: &WorkerHandle, fingerprint: &str) -> Result<(), &'static str> {
        let w = self.workers.get_mut(h).ok_or("unknown worker")?;
        if w.state != State::Running {
            return Err("worker not running");
        }
        if w.last_fingerprint.as_deref() == Some(fingerprint) {
            w.unchanged_polls += 1;
        } else {
            w.unchanged_polls = 0;
            w.steered = false;
            w.last_fingerprint = Some(fingerprint.to_owned());
        }
        self.emit(OrchestrationEvent {
            name: "worker.polled",
            worker_id: Some(h.0.clone()),
            worker_role: None,
            reason_code: None,
        });
        Ok(())
    }
    pub fn is_stalled(&self, h: &WorkerHandle) -> Result<bool, &'static str> {
        let w = self.workers.get(h).ok_or("unknown worker")?;
        Ok(w.unchanged_polls >= 2 && w.steered)
    }
    pub fn steer(&mut self, h: &WorkerHandle) -> Result<(), &'static str> {
        let w = self.workers.get_mut(h).ok_or("unknown worker")?;
        if w.state != State::Running {
            return Err("worker not running");
        }
        w.steered = true;
        self.emit(OrchestrationEvent {
            name: "worker.steered",
            worker_id: Some(h.0.clone()),
            worker_role: None,
            reason_code: None,
        });
        Ok(())
    }
    pub fn mark_terminal(
        &mut self,
        h: &WorkerHandle,
        _: WorkerTerminal,
    ) -> Result<(), &'static str> {
        self.transition(h, &[State::Running], State::Terminal, "worker.terminal")
    }
    pub fn collect(&mut self, h: &WorkerHandle) -> Result<(), &'static str> {
        self.transition(h, &[State::Terminal], State::Collected, "worker.collected")
    }
    pub fn reconcile(&mut self, h: &WorkerHandle) -> Result<(), &'static str> {
        self.transition(
            h,
            &[State::Collected],
            State::Reconciled,
            "worker.reconciled",
        )
    }
    pub fn completion_gate(&self) -> CompletionGate {
        let workers: Vec<_> = self
            .workers
            .iter()
            .filter(|(_, w)| w.state != State::Reconciled)
            .map(|(h, _)| h.0.clone())
            .collect();
        if workers.is_empty() || self.policy.mode == EnforcementMode::Off {
            CompletionGate::Allowed
        } else if self.policy.mode == EnforcementMode::Advisory {
            CompletionGate::Warning { workers }
        } else {
            CompletionGate::Blocked { workers }
        }
    }
    pub fn check_foreground_write(&self, path: &str) -> ScopeDecision {
        let workers: Vec<_> = self
            .workers
            .iter()
            .filter(|(_, w)| {
                matches!(w.state, State::Dispatched | State::Running) && overlaps(&w.writes, path)
            })
            .map(|(h, _)| h.0.clone())
            .collect();
        if workers.is_empty() || self.policy.mode == EnforcementMode::Off {
            ScopeDecision::Allowed
        } else if self.policy.mode == EnforcementMode::Advisory {
            ScopeDecision::Warning { workers }
        } else {
            ScopeDecision::ReconciliationRequired { workers }
        }
    }
}
fn overlaps(policy: &WorkerWritePolicy, path: &str) -> bool {
    match policy {
        WorkerWritePolicy::ReadOnly | WorkerWritePolicy::IsolatedWorktree => false,
        WorkerWritePolicy::NonOverlappingPaths(paths) => paths.iter().any(|p| {
            p.strip_suffix("/**").map_or(p == path, |prefix| {
                path == prefix || path.starts_with(&format!("{prefix}/"))
            })
        }),
    }
}
