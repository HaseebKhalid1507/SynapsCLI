pub mod capability;

use crate::prompt::QualifiedModelId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnforcementMode {
    Off,
    Advisory,
    Enforced,
}

/// One runtime-owned model descriptor. A manifest can only narrow these capabilities.
#[derive(Clone, Debug, Serialize)]
pub struct CatalogEntry {
    pub model: QualifiedModelId,
    pub available: bool,
    pub worker_eligible: bool,
}

/// Immutable, runtime-controlled worker catalog. Entries are exact qualified identities.
#[derive(Clone, Debug, Serialize)]
pub struct CatalogSnapshot {
    id: String,
    digest_sha256: String,
    entries: BTreeMap<QualifiedModelId, CatalogEntry>,
}
impl CatalogSnapshot {
    /// Trusted convenience constructor for callers whose descriptors are all active workers.
    pub fn new(entries: impl IntoIterator<Item = QualifiedModelId>) -> Self {
        Self::from_entries(entries.into_iter().map(|model| CatalogEntry {
            model,
            available: true,
            worker_eligible: true,
        }))
    }
    pub fn from_entries(entries: impl IntoIterator<Item = CatalogEntry>) -> Self {
        let entries: BTreeMap<_, _> = entries
            .into_iter()
            .map(|entry| (entry.model.clone(), entry))
            .collect();
        let encoded = serde_json::to_vec(&entries).expect("catalog is serializable");
        let digest_sha256 = format!("{:x}", Sha256::digest(encoded));
        Self {
            id: format!("catalog-{}", &digest_sha256[..16]),
            digest_sha256,
            entries,
        }
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn digest_sha256(&self) -> &str {
        &self.digest_sha256
    }
    pub fn contains(&self, model: &QualifiedModelId) -> bool {
        self.entries
            .get(model)
            .is_some_and(|entry| entry.available && entry.worker_eligible)
    }
    /// Admits one additional exact identity as an available worker-eligible
    /// descriptor, recomputing the snapshot id/digest. Used for explicit
    /// mid-session user grants; an existing entry is upgraded to eligible.
    pub fn with_model(&self, model: QualifiedModelId) -> Self {
        let mut entries: Vec<CatalogEntry> = self.entries.values().cloned().collect();
        entries.push(CatalogEntry {
            model,
            available: true,
            worker_eligible: true,
        });
        Self::from_entries(entries)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CrossProviderGrant {
    pub grant_id: String,
    pub from_provider: String,
    pub to_provider: String,
    pub allowed_models: BTreeSet<QualifiedModelId>,
    /// Trusted Unix expiry. `None` is retained for compatibility with pinned policy files.
    pub expires_at_unix: Option<u64>,
}
impl CrossProviderGrant {
    pub fn new(
        grant_id: impl Into<String>,
        from_provider: impl Into<String>,
        to_provider: impl Into<String>,
        models: impl IntoIterator<Item = QualifiedModelId>,
    ) -> Result<Self, &'static str> {
        let grant_id = grant_id.into();
        let from_provider = from_provider.into();
        let to_provider = to_provider.into();
        let allowed_models: BTreeSet<_> = models.into_iter().collect();
        if grant_id.is_empty()
            || from_provider.is_empty()
            || to_provider.is_empty()
            || allowed_models.is_empty()
            || allowed_models.iter().any(|m| m.provider() != to_provider)
        {
            return Err("invalid exact cross-provider grant");
        }
        Ok(Self {
            grant_id,
            from_provider,
            to_provider,
            allowed_models,
            expires_at_unix: None,
        })
    }
    pub fn expiring_at(mut self, expires_at_unix: u64) -> Result<Self, &'static str> {
        if expires_at_unix == 0 {
            return Err("invalid cross-provider grant expiry");
        }
        self.expires_at_unix = Some(expires_at_unix);
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchFailureCode {
    InvalidQualifiedModel,
    CatalogModelUnknown,
    ProviderNotAllowed,
    CrossProviderGrantExpired,
    ModelNotAllowed,
    ConcurrencyLimit,
    TotalWorkerLimit,
    PolicyUnavailable,
}
impl DispatchFailureCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidQualifiedModel => "invalid_qualified_model",
            Self::CatalogModelUnknown => "catalog_model_unknown",
            Self::ProviderNotAllowed => "provider_not_allowed",
            Self::CrossProviderGrantExpired => "cross_provider_grant_expired",
            Self::ModelNotAllowed => "model_not_allowed",
            Self::ConcurrencyLimit => "concurrency_limit",
            Self::TotalWorkerLimit => "total_limit",
            Self::PolicyUnavailable => "policy_unavailable",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct DelegationPolicy {
    pub mode: EnforcementMode,
    foreground: QualifiedModelId,
    catalog: CatalogSnapshot,
    allowed_models: BTreeSet<QualifiedModelId>,
    grants: Vec<CrossProviderGrant>,
    effective_choices: Vec<QualifiedModelId>,
    pub max_concurrent_workers: usize,
    pub max_total_workers: usize,
}
impl DelegationPolicy {
    /// Compatibility constructor. Its explicit models form the trusted pinned catalog.
    pub fn new(
        mode: EnforcementMode,
        foreground: QualifiedModelId,
        models: impl IntoIterator<Item = QualifiedModelId>,
        concurrent: usize,
        total: usize,
    ) -> Self {
        let allowed_models: BTreeSet<_> = models.into_iter().collect();
        let mut catalog_entries = allowed_models.clone();
        catalog_entries.insert(foreground.clone());
        let catalog = CatalogSnapshot::new(catalog_entries);
        Self::build(
            mode,
            foreground,
            catalog,
            allowed_models,
            Vec::new(),
            concurrent,
            total,
        )
        .expect("legacy delegation policy has valid limits")
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
    pub fn baseline(
        foreground: QualifiedModelId,
        catalog: CatalogSnapshot,
        concurrent: usize,
        total: usize,
    ) -> Result<Self, &'static str> {
        Self::build(
            EnforcementMode::Enforced,
            foreground.clone(),
            catalog,
            [foreground].into_iter().collect(),
            Vec::new(),
            concurrent,
            total,
        )
    }
    /// Secure manifestless baseline: every source-controlled worker-eligible
    /// descriptor for the foreground provider is allowed. Other providers still
    /// require explicit cross-provider grants.
    pub fn provider_baseline(
        foreground: QualifiedModelId,
        catalog: CatalogSnapshot,
        same_provider_models: impl IntoIterator<Item = QualifiedModelId>,
        concurrent: usize,
        total: usize,
    ) -> Result<Self, &'static str> {
        Self::build(
            EnforcementMode::Enforced,
            foreground,
            catalog,
            same_provider_models.into_iter().collect(),
            Vec::new(),
            concurrent,
            total,
        )
    }
    pub fn with_grants(
        foreground: QualifiedModelId,
        catalog: CatalogSnapshot,
        same_provider_models: impl IntoIterator<Item = QualifiedModelId>,
        grants: impl IntoIterator<Item = CrossProviderGrant>,
        concurrent: usize,
        total: usize,
    ) -> Result<Self, &'static str> {
        Self::build(
            EnforcementMode::Enforced,
            foreground,
            catalog,
            same_provider_models.into_iter().collect(),
            grants.into_iter().collect(),
            concurrent,
            total,
        )
    }
    fn build(
        mode: EnforcementMode,
        foreground: QualifiedModelId,
        catalog: CatalogSnapshot,
        allowed_models: BTreeSet<QualifiedModelId>,
        grants: Vec<CrossProviderGrant>,
        concurrent: usize,
        total: usize,
    ) -> Result<Self, &'static str> {
        if concurrent == 0 || total == 0 || concurrent > total || !catalog.contains(&foreground) {
            return Err("invalid delegation policy");
        }
        if allowed_models
            .iter()
            .any(|m| m.provider() != foreground.provider() || !catalog.contains(m))
        {
            return Err("same-provider allowlist is invalid");
        }
        if grants.iter().any(|g| {
            g.from_provider != foreground.provider()
                || g.allowed_models.iter().any(|m| !catalog.contains(m))
        }) {
            return Err("cross-provider grant is invalid");
        }
        let mut choices = allowed_models.clone();
        for grant in &grants {
            choices.extend(grant.allowed_models.iter().cloned());
        }
        Ok(Self {
            mode,
            foreground,
            catalog,
            allowed_models,
            grants,
            effective_choices: choices.into_iter().collect(),
            max_concurrent_workers: concurrent,
            max_total_workers: total,
        })
    }
    pub fn authorize(&self, model: &QualifiedModelId) -> Result<Option<&str>, DispatchDenied> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.authorize_at(model, now)
    }
    /// Atomically evaluates exact model, provenance, grant id and expiry against one
    /// trusted clock sample supplied by the runtime.
    pub fn authorize_at(
        &self,
        model: &QualifiedModelId,
        now_unix: u64,
    ) -> Result<Option<&str>, DispatchDenied> {
        if !self.catalog.contains(model) {
            return Err(DispatchDenied::new(
                DispatchFailureCode::CatalogModelUnknown,
            ));
        }
        if self.mode != EnforcementMode::Enforced {
            return Ok(None);
        }
        if model.provider() == self.foreground.provider() {
            return self
                .allowed_models
                .contains(model)
                .then_some(None)
                .ok_or_else(|| DispatchDenied::new(DispatchFailureCode::ModelNotAllowed));
        }
        if let Some(grant) = self
            .grants
            .iter()
            .find(|g| g.to_provider == model.provider() && g.allowed_models.contains(model))
        {
            if grant
                .expires_at_unix
                .is_some_and(|expiry| now_unix >= expiry)
            {
                return Err(DispatchDenied::new(
                    DispatchFailureCode::CrossProviderGrantExpired,
                ));
            }
            return Ok(Some(&grant.grant_id));
        }
        if self
            .grants
            .iter()
            .any(|g| g.to_provider == model.provider())
        {
            Err(DispatchDenied::new(DispatchFailureCode::ModelNotAllowed))
        } else {
            Err(DispatchDenied::new(DispatchFailureCode::ProviderNotAllowed))
        }
    }
    pub fn effective_choices(&self) -> &[QualifiedModelId] {
        &self.effective_choices
    }
    /// Honors an explicit user trust grant issued after session start. Trusted
    /// models were never meant to be pinned at session load: the exact identity
    /// joins the catalog and either the same-provider allowlist or a fresh
    /// non-expiring session cross-provider grant. Session grants are inserted
    /// ahead of pinned grants so a stale expiring grant for the same identity
    /// can never shadow the user's explicit decision. Worker lifecycle state
    /// and concurrency limits are untouched.
    pub fn grant_worker_model(&mut self, model: QualifiedModelId) -> Result<(), &'static str> {
        if !self.catalog.contains(&model) {
            self.catalog = self.catalog.with_model(model.clone());
        }
        if model.provider() == self.foreground.provider() {
            self.allowed_models.insert(model.clone());
        } else if !self.grants.iter().any(|grant| {
            grant.to_provider == model.provider()
                && grant.allowed_models.contains(&model)
                && grant.expires_at_unix.is_none()
        }) {
            let grant = CrossProviderGrant::new(
                format!("session-user-grant-{}", model.as_str()),
                self.foreground.provider().to_owned(),
                model.provider().to_owned(),
                [model.clone()],
            )?;
            self.grants.insert(0, grant);
        }
        if !self.effective_choices.contains(&model) {
            self.effective_choices.push(model);
            self.effective_choices.sort();
        }
        Ok(())
    }
    pub fn foreground_model(&self) -> &QualifiedModelId {
        &self.foreground
    }
    pub fn catalog_snapshot_id(&self) -> &str {
        self.catalog.id()
    }
    pub fn catalog_digest(&self) -> &str {
        self.catalog.digest_sha256()
    }
    pub fn digest(&self) -> String {
        let encoded = serde_json::to_vec(self).expect("policy is serializable");
        format!("{:x}", Sha256::digest(encoded))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerRole {
    Planner,
    Implementer,
    Tester,
    Reviewer,
    Researcher,
    Debugger,
}
#[derive(Clone, Debug, Deserialize)]
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
impl WorkerHandle {
    /// Stable policy-side identity (`worker-N`). Runtime layers map this back
    /// to tool-facing handles (`sa_*`) for remediation messages.
    pub fn id(&self) -> &str {
        &self.0
    }
}
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
    code: DispatchFailureCode,
}
impl DispatchDenied {
    fn new(code: DispatchFailureCode) -> Self {
        Self { code }
    }
    pub fn code(&self) -> &'static str {
        self.code.as_str()
    }
    pub fn typed_code(&self) -> DispatchFailureCode {
        self.code
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Dispatched,
    Starting,
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
        self.policy.foreground_model()
    }
    pub fn policy(&self) -> &DelegationPolicy {
        &self.policy
    }
    /// Applies an explicit mid-session user trust grant for one exact worker
    /// model. Existing worker lifecycle state and limits are unchanged; the
    /// grant is visible to the next dispatch decision.
    pub fn grant_worker_model(&mut self, model: QualifiedModelId) -> Result<(), &'static str> {
        self.policy.grant_worker_model(model)?;
        self.emit(OrchestrationEvent {
            name: "worker.model_granted",
            worker_id: None,
            worker_role: None,
            reason_code: Some("user_session_grant"),
        });
        Ok(())
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
        if worker.state != State::Dispatched && worker.state != State::Starting {
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
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.validate_dispatch_at(model, now)
    }
    pub fn validate_dispatch_at(
        &self,
        model: &QualifiedModelId,
        now_unix: u64,
    ) -> Result<(), DispatchDenied> {
        self.policy.authorize_at(model, now_unix)?;
        if self.total >= self.policy.max_total_workers {
            return Err(DispatchDenied::new(DispatchFailureCode::TotalWorkerLimit));
        }
        if self
            .workers
            .values()
            .filter(|w| {
                matches!(
                    w.state,
                    State::Dispatched | State::Starting | State::Running
                )
            })
            .count()
            >= self.policy.max_concurrent_workers
        {
            return Err(DispatchDenied::new(DispatchFailureCode::ConcurrencyLimit));
        }
        Ok(())
    }
    pub fn authorize_dispatch(
        &mut self,
        model: &QualifiedModelId,
        role: WorkerRole,
        writes: WorkerWritePolicy,
    ) -> Result<WorkerHandle, DispatchDenied> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.authorize_dispatch_at(model, role, writes, now)
    }
    pub fn authorize_dispatch_at(
        &mut self,
        model: &QualifiedModelId,
        role: WorkerRole,
        writes: WorkerWritePolicy,
        now_unix: u64,
    ) -> Result<WorkerHandle, DispatchDenied> {
        self.emit(OrchestrationEvent {
            name: "worker.dispatch_requested",
            worker_id: None,
            worker_role: None,
            reason_code: None,
        });
        self.emit(OrchestrationEvent {
            name: "worker.model_resolution_requested",
            worker_id: None,
            worker_role: None,
            reason_code: None,
        });
        if let Err(error) = self.validate_dispatch_at(model, now_unix) {
            self.emit(OrchestrationEvent {
                name: "worker.dispatch_denied",
                worker_id: None,
                worker_role: None,
                reason_code: Some(error.code()),
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
            name: "worker.dispatch_allowed",
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
    pub fn mark_starting(&mut self, h: &WorkerHandle) -> Result<(), &'static str> {
        self.transition(h, &[State::Dispatched], State::Starting, "worker.starting")
    }
    pub fn mark_running(&mut self, h: &WorkerHandle) -> Result<(), &'static str> {
        self.transition(h, &[State::Starting], State::Running, "worker.running")
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
        if self.workers.get(h).is_some_and(|worker| {
            matches!(
                worker.state,
                State::Terminal | State::Collected | State::Reconciled
            )
        }) {
            return Ok(());
        }
        self.transition(h, &[State::Running], State::Terminal, "worker.terminal")
    }
    pub fn collect(&mut self, h: &WorkerHandle) -> Result<(), &'static str> {
        if self
            .workers
            .get(h)
            .is_some_and(|w| matches!(w.state, State::Collected | State::Reconciled))
        {
            return Ok(());
        }
        self.transition(h, &[State::Terminal], State::Collected, "worker.collected")
    }
    pub fn reconcile(&mut self, h: &WorkerHandle) -> Result<(), &'static str> {
        if self
            .workers
            .get(h)
            .is_some_and(|w| w.state == State::Reconciled)
        {
            return Ok(());
        }
        self.transition(
            h,
            &[State::Collected],
            State::Reconciled,
            "worker.reconciled",
        )
    }
    pub fn completion_gate(&self) -> CompletionGate {
        // Only block on FINISHED-but-unreconciled workers. Running/Starting/
        // Dispatched workers pass through — that's the reactive pattern
        // (subagent_start → collect on wake). Blocking on Running makes
        // subagent_start functionally identical to subagent.
        let workers: Vec<_> = self
            .workers
            .iter()
            .filter(|(_, w)| matches!(w.state, State::Terminal | State::Collected))
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
    /// All non-reconciled worker IDs — including running ones. Used by the
    /// reaper to decide retention (don't GC a worker the orchestration still
    /// tracks, even if the completion gate lets the turn through).
    pub fn all_unreconciled_ids(&self) -> Vec<String> {
        self.workers
            .iter()
            .filter(|(_, w)| w.state != State::Reconciled)
            .map(|(h, _)| h.0.clone())
            .collect()
    }
    pub fn check_foreground_write(&self, path: &str) -> ScopeDecision {
        let workers: Vec<_> = self
            .workers
            .iter()
            .filter(|(_, w)| {
                matches!(
                    w.state,
                    State::Dispatched | State::Starting | State::Running
                ) && overlaps(&w.writes, path)
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
