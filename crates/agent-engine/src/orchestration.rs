use agent_core::orchestration::{
    CatalogSnapshot, CompletionGate, DelegationPolicy, WorkerRegistry, WorkerRole, WorkerTerminal,
    WorkerWritePolicy,
};
use agent_core::prompt::QualifiedModelId;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;

pub struct DelegationTreeBudget {
    pub max_depth: u16,
    pub max_children_per_worker: usize,
    pub max_total_descendants: usize,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum DelegationTreeDenied {
    DepthLimit,
    ChildLimit,
    DescendantLimit,
    UnknownParent,
}

#[derive(Debug, Default)]
struct DelegationTreeState {
    depth_by_worker: BTreeMap<String, u16>,
    child_count: BTreeMap<String, usize>,
    descendants: usize,
}

/// Session-scoped runtime enforcement shared by every subagent tool path.
pub struct OrchestrationRuntime {
    inner: Mutex<Inner>,
    tree_budget: DelegationTreeBudget,
    tree: Mutex<DelegationTreeState>,
}
struct Inner {
    registry: WorkerRegistry,
    handles: HashMap<String, agent_core::orchestration::WorkerHandle>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AuthorizedWorkerModel {
    pub model: QualifiedModelId,
    pub selection_source: SelectionSource,
    pub network_attempted: bool,
    pub policy_digest: String,
    pub catalog_snapshot_id: String,
    pub catalog_digest: String,
    pub correlation_id: String,
    pub cross_provider_grant_id: Option<String>,
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionSource {
    ForegroundInheritance,
    ExplicitRequest,
}
impl SelectionSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ForegroundInheritance => "foreground_inheritance",
            Self::ExplicitRequest => "explicit_request",
        }
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct DispatchDenial {
    pub kind: &'static str,
    pub code: &'static str,
    pub requested_model: Option<String>,
    pub foreground_model: String,
    pub selection_source: SelectionSource,
    pub network_attempted: bool,
    pub remediation: &'static str,
}
impl std::fmt::Display for DispatchDenial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            serde_json::to_string(self).unwrap_or_else(|_| self.code.into())
        )
    }
}
impl std::error::Error for DispatchDenial {}

/// Canonicalize a runtime foreground model string into its exact qualified
/// identity. Legacy configs persist bare Anthropic IDs (e.g. `claude-fable-5`)
/// which the runtime router still deliberately routes to `anthropic`; the
/// canonical identity is derived from that single routing entry point — never
/// from substring heuristics. Unroutable values fail closed.
pub fn canonical_foreground_identity(raw: &str) -> Result<QualifiedModelId, String> {
    if let Ok(model) = QualifiedModelId::parse(raw) {
        return Ok(model);
    }
    let route = crate::runtime::openai::resolve_route(raw)
        .ok_or_else(|| format!("foreground model is unresolved: {raw}"))?;
    QualifiedModelId::parse(format!("{}/{}", route.provider, route.model))
        .map_err(|e| e.to_string())
}

/// Validate an exact identity against source-controlled runtime routing/catalog
/// descriptors before allowing a model-driven session grant. This operation is
/// credential- and network-blind: availability is checked later by normal
/// provider execution, while invented identities fail before policy mutation.
pub fn validate_user_authorizable_model(raw: &str) -> Result<QualifiedModelId, String> {
    let model = QualifiedModelId::parse(raw)
        .map_err(|_| "authorization denied: invalid qualified model".to_string())?;
    let known = if model.provider() == "anthropic" {
        agent_core::models::KNOWN_MODELS
            .iter()
            .any(|(id, _)| *id == model.model())
    } else if model.provider() == "openai-codex" {
        crate::runtime::openai::catalog::codex_static_catalog_models()
            .iter()
            .any(|entry| entry.runtime_id() == model.as_str())
    } else if model.provider() == "xai-auth" {
        crate::runtime::openai::catalog::xai_model(model.model()).is_some()
    } else if model.provider() == "github-copilot" {
        crate::runtime::openai::catalog::github_copilot_runtime_model(model.model()).is_some()
    } else if model.provider() == "google-gemini" {
        crate::runtime::openai::catalog::google_gemini_model(model.model()).is_some()
    } else {
        crate::runtime::openai::registry::providers()
            .iter()
            .find(|provider| provider.key == model.provider())
            .is_some_and(|provider| {
                provider
                    .models
                    .iter()
                    .any(|(id, _, _)| *id == model.model())
            })
    };
    if !known || crate::runtime::openai::resolve_route(model.as_str()).is_none() {
        return Err(format!(
            "authorization denied: '{}' is not a known routable model",
            model.as_str()
        ));
    }
    Ok(model)
}

/// Exact source-controlled OpenRouter worker stack. These identities are
/// trusted local descriptors (not live catalog results or historical logs).
const OPENROUTER_WORKER_MODELS: &[&str] = &[
    "openrouter/deepseek/deepseek-v4-pro",
    "openrouter/moonshotai/kimi-k2.7-code",
    "openrouter/z-ai/glm-5.2",
];

/// Exact manifestless worker choices for a foreground provider.
fn manifestless_worker_choices(foreground: &QualifiedModelId) -> Vec<QualifiedModelId> {
    let source = if foreground.provider() == "openrouter" {
        OPENROUTER_WORKER_MODELS
    } else {
        &[]
    };
    let mut choices: Vec<_> = source
        .iter()
        .filter_map(|value| QualifiedModelId::parse(*value).ok())
        .collect();
    choices.push(foreground.clone());
    choices.sort();
    choices.dedup();
    choices
}

impl OrchestrationRuntime {
    /// Build a snapshot exclusively from runtime-owned routing/catalog descriptors.
    /// `manifest_references` are deliberately ignored here: manifests may narrow the
    /// snapshot, but can never create catalog membership. The foreground is admitted
    /// only after the credential-free runtime router resolves its exact identity.
    pub fn trusted_catalog(
        foreground: &QualifiedModelId,
        _manifest_references: impl IntoIterator<Item = QualifiedModelId>,
    ) -> Result<CatalogSnapshot, &'static str> {
        use crate::runtime::openai::catalog;
        use agent_core::orchestration::CatalogEntry;

        let route = crate::runtime::openai::resolve_route(foreground.as_str())
            .ok_or("foreground model is unresolved")?;
        if route.provider != foreground.provider() {
            return Err("foreground provider did not resolve exactly");
        }

        let mut descriptors = catalog::codex_static_catalog_models();
        descriptors.extend(catalog::xai_static_catalog_models());
        descriptors.extend(catalog::copilot_static_catalog_models());
        descriptors.extend(catalog::google_gemini_static_catalog_models());
        let mut entries: Vec<_> = descriptors
            .into_iter()
            .filter_map(|descriptor| QualifiedModelId::parse(descriptor.runtime_id()).ok())
            .map(|model| CatalogEntry {
                model,
                available: true,
                worker_eligible: true,
            })
            .collect();
        entries.extend(
            manifestless_worker_choices(foreground)
                .into_iter()
                .map(|model| CatalogEntry {
                    model,
                    available: true,
                    worker_eligible: true,
                }),
        );
        // `resolve_route` is itself a trusted local provider descriptor. This is
        // independent of manifest content and permits configured generic/Anthropic
        // foreground routes whose live catalogs are not fetched during startup.
        entries.push(CatalogEntry {
            model: foreground.clone(),
            available: true,
            worker_eligible: true,
        });
        Ok(CatalogSnapshot::from_entries(entries))
    }

    /// Secure manifestless baseline: the exact foreground identity is the only
    /// worker choice in a deterministic runtime-controlled catalog.
    pub fn baseline(
        foreground: QualifiedModelId,
        concurrent: usize,
        total: usize,
    ) -> Result<Self, &'static str> {
        let choices = manifestless_worker_choices(&foreground);
        let catalog = Self::trusted_catalog(&foreground, std::iter::empty())?;
        Ok(Self::new(DelegationPolicy::provider_baseline(
            foreground, catalog, choices, concurrent, total,
        )?))
    }

    pub fn new(policy: DelegationPolicy) -> Self {
        let max_total_descendants = policy.max_total_workers;
        Self {
            inner: Mutex::new(Inner {
                registry: WorkerRegistry::new(policy),
                handles: HashMap::new(),
            }),
            tree_budget: DelegationTreeBudget {
                max_depth: 4,
                max_children_per_worker: 8,
                max_total_descendants,
            },
            tree: Mutex::new(DelegationTreeState::default()),
        }
    }

    pub fn with_tree_budget(mut self, budget: DelegationTreeBudget) -> Result<Self, &'static str> {
        if budget.max_depth == 0
            || budget.max_children_per_worker == 0
            || budget.max_total_descendants == 0
        {
            return Err("invalid delegation tree budget");
        }
        self.tree_budget = budget;
        Ok(self)
    }

    /// Reserve one tree edge before allocating channels/threads/provider
    /// runtimes. `parent=None` is a foreground-root child. Every denial is
    /// fail-closed and leaves the counters unchanged.
    pub fn reserve_delegation(
        &self,
        worker_id: &str,
        parent: Option<&str>,
    ) -> Result<u16, DelegationTreeDenied> {
        let mut tree = self.tree.lock().unwrap();
        if tree.descendants >= self.tree_budget.max_total_descendants {
            return Err(DelegationTreeDenied::DescendantLimit);
        }
        let depth = match parent {
            None => 1,
            Some(parent_id) => tree
                .depth_by_worker
                .get(parent_id)
                .copied()
                .ok_or(DelegationTreeDenied::UnknownParent)?
                .saturating_add(1),
        };
        if depth > self.tree_budget.max_depth {
            return Err(DelegationTreeDenied::DepthLimit);
        }
        let parent_key = parent.unwrap_or("<root>");
        if tree.child_count.get(parent_key).copied().unwrap_or(0)
            >= self.tree_budget.max_children_per_worker
        {
            return Err(DelegationTreeDenied::ChildLimit);
        }
        tree.depth_by_worker.insert(worker_id.to_string(), depth);
        *tree.child_count.entry(parent_key.to_string()).or_default() += 1;
        tree.descendants += 1;
        Ok(depth)
    }

    pub fn release_delegation(&self, worker_id: &str, parent: Option<&str>) {
        let mut tree = self.tree.lock().unwrap();
        if tree.depth_by_worker.remove(worker_id).is_some() {
            tree.descendants = tree.descendants.saturating_sub(1);
            let parent_key = parent.unwrap_or("<root>");
            if let Some(count) = tree.child_count.get_mut(parent_key) {
                *count = count.saturating_sub(1);
            }
        }
    }

    pub fn delegation_descendants(&self) -> usize {
        self.tree.lock().unwrap().descendants
    }
    pub fn preflight(&self, model: &str) -> Result<(), String> {
        let model = QualifiedModelId::parse(model)
            .map_err(|_| "delegation denied: invalid qualified model".to_string())?;
        self.inner
            .lock()
            .unwrap()
            .registry
            .validate_dispatch(&model)
            .map_err(|e| format!("delegation denied: {}", e.code()))
    }

    /// Honors an explicit mid-session user trust grant for one exact worker
    /// model. Trusted models were never meant to be pinned at session start:
    /// the live policy and catalog are extended in place while worker
    /// lifecycle state and concurrency limits are preserved. The grant takes
    /// effect for the next dispatch decision.
    pub fn grant_worker_model(&self, model: &str) -> Result<(), String> {
        let model = QualifiedModelId::parse(model)
            .map_err(|_| "grant denied: invalid qualified model".to_string())?;
        self.inner
            .lock()
            .unwrap()
            .registry
            .grant_worker_model(model)
            .map_err(|error| format!("grant denied: {error}"))
    }

    /// Read-only UltraCode authorization snapshot. The exact foreground identity,
    /// policy authorization, and configured limits are checked under one lock; no
    /// worker reservation or lifecycle state is created.
    pub fn ultracode_readiness(&self, model: &str) -> Result<(usize, usize), String> {
        let model = QualifiedModelId::parse(model)
            .map_err(|_| "delegation denied: invalid qualified model".to_string())?;
        let inner = self.inner.lock().unwrap();
        if inner.registry.foreground_model() != &model {
            return Err("delegation denied: model is not exact foreground".into());
        }
        inner
            .registry
            .validate_dispatch(&model)
            .map_err(|e| format!("delegation denied: {}", e.code()))?;
        let policy = inner.registry.policy();
        if policy.max_concurrent_workers == 0 || policy.max_total_workers == 0 {
            return Err("delegation denied: invalid worker limits".into());
        }
        Ok((policy.max_concurrent_workers, policy.max_total_workers))
    }

    /// Single parse/resolve/catalog/policy/limits decision point used by every
    /// public spawn path. No credential or provider object is reachable here.
    pub fn resolve_and_authorize(
        &self,
        runtime_handle: &str,
        requested: Option<&str>,
    ) -> Result<AuthorizedWorkerModel, DispatchDenial> {
        let mut inner = self.inner.lock().unwrap();
        let foreground = inner.registry.foreground_model().clone();
        let selection_source = if requested.is_some() {
            SelectionSource::ExplicitRequest
        } else {
            SelectionSource::ForegroundInheritance
        };
        let model = match requested {
            Some(value) => QualifiedModelId::parse(value).map_err(|_| DispatchDenial {
                kind: "dispatch_denied",
                code: "invalid_qualified_model",
                requested_model: None,
                foreground_model: foreground.as_str().into(),
                selection_source,
                network_attempted: false,
                remediation: "Omit model to inherit foreground or select an exact session choice.",
            })?,
            None => foreground.clone(),
        };
        if inner.handles.contains_key(runtime_handle) {
            return Err(DispatchDenial {
                kind: "dispatch_denied",
                code: "duplicate_runtime_handle",
                requested_model: Some(model.as_str().into()),
                foreground_model: foreground.as_str().into(),
                selection_source,
                network_attempted: false,
                remediation: "Use a new worker handle.",
            });
        }
        let policy_digest = inner.registry.policy().digest();
        let catalog_snapshot_id = inner.registry.policy().catalog_snapshot_id().to_owned();
        let catalog_digest = inner.registry.policy().catalog_digest().to_owned();
        let cross_provider_grant_id = inner
            .registry
            .policy()
            .authorize(&model)
            .ok()
            .flatten()
            .map(str::to_owned);
        let handle = inner
            .registry
            .authorize_dispatch(&model, WorkerRole::Implementer, WorkerWritePolicy::ReadOnly)
            .map_err(|error| DispatchDenial {
                kind: "dispatch_denied",
                code: error.code(),
                requested_model: Some(model.as_str().into()),
                foreground_model: foreground.as_str().into(),
                selection_source,
                network_attempted: false,
                remediation: "Omit model to inherit foreground, select an exact session choice, or trust the model mid-session (favorite it in the models picker).",
            })?;
        inner.handles.insert(runtime_handle.to_owned(), handle);
        Ok(AuthorizedWorkerModel {
            model,
            selection_source,
            network_attempted: false,
            policy_digest,
            catalog_snapshot_id,
            catalog_digest,
            correlation_id: runtime_handle.to_owned(),
            cross_provider_grant_id,
        })
    }

    /// Marks the persisted handle Starting immediately before the runtime/thread
    /// factory is invoked. Starting remains rollback-safe until `mark_running`.
    pub fn mark_starting(&self, runtime_handle: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        let handle = inner
            .handles
            .get(runtime_handle)
            .cloned()
            .ok_or_else(|| "worker lifecycle unavailable".to_string())?;
        inner
            .registry
            .mark_starting(&handle)
            .map_err(|_| "worker lifecycle unavailable".into())
    }
    pub fn mark_running(&self, runtime_handle: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        let handle = inner
            .handles
            .get(runtime_handle)
            .cloned()
            .ok_or_else(|| "worker lifecycle unavailable".to_string())?;
        inner
            .registry
            .mark_running(&handle)
            .map_err(|_| "worker lifecycle unavailable".into())
    }

    pub fn rollback(&self, runtime_handle: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(handle) = inner.handles.get(runtime_handle).cloned() {
            let _ = inner.registry.rollback_dispatch(&handle);
            inner.handles.remove(runtime_handle);
        }
    }

    pub fn effective_choices(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .registry
            .policy()
            .effective_choices()
            .iter()
            .map(|model| model.as_str().to_owned())
            .collect()
    }

    pub fn foreground_model(&self) -> String {
        self.inner
            .lock()
            .unwrap()
            .registry
            .foreground_model()
            .as_str()
            .to_owned()
    }
    pub fn authorize_with_policy(
        &self,
        runtime_handle: &str,
        model: &str,
        role: WorkerRole,
        writes: WorkerWritePolicy,
    ) -> Result<(), String> {
        let model = QualifiedModelId::parse(model)
            .map_err(|_| "delegation denied: invalid qualified model".to_string())?;
        let mut inner = self.inner.lock().unwrap();
        if inner.handles.contains_key(runtime_handle) {
            return Err("delegation denied: duplicate runtime handle".to_string());
        }
        let handle = inner
            .registry
            .authorize_dispatch(&model, role, writes)
            .map_err(|e| format!("delegation denied: {}", e.code()))?;
        inner.registry.mark_starting(&handle).map_err(|error| {
            let _ = inner.registry.rollback_dispatch(&handle);
            error.to_string()
        })?;
        if let Err(error) = inner.registry.mark_running(&handle) {
            let _ = inner.registry.rollback_dispatch(&handle);
            return Err(error.to_string());
        }
        inner.handles.insert(runtime_handle.to_string(), handle);
        Ok(())
    }
    pub fn authorize(&self, runtime_handle: &str, model: &str) -> Result<(), String> {
        self.authorize_with_policy(
            runtime_handle,
            model,
            WorkerRole::Implementer,
            WorkerWritePolicy::ReadOnly,
        )
    }
    pub fn poll(&self, id: &str, fingerprint: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        let h = inner
            .handles
            .get(id)
            .cloned()
            .ok_or_else(|| "unknown worker".to_string())?;
        inner.registry.poll(&h, fingerprint).map_err(str::to_string)
    }
    pub fn steer(&self, id: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        let h = inner
            .handles
            .get(id)
            .cloned()
            .ok_or_else(|| "unknown worker".to_string())?;
        inner.registry.steer(&h).map_err(str::to_string)
    }
    pub fn terminal_and_collect(&self, id: &str, terminal: WorkerTerminal) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        let h = inner
            .handles
            .get(id)
            .cloned()
            .ok_or_else(|| "unknown worker".to_string())?;
        inner
            .registry
            .mark_terminal(&h, terminal)
            .map_err(str::to_string)?;
        inner.registry.collect(&h).map_err(str::to_string)
    }
    pub fn finish_one_shot(&self, id: &str, terminal: WorkerTerminal) -> Result<(), String> {
        self.terminal_and_collect(id, terminal)?;
        self.reconcile(id)
    }
    pub fn reconcile(&self, id: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        let h = inner
            .handles
            .get(id)
            .cloned()
            .ok_or_else(|| "unknown worker".to_string())?;
        inner.registry.reconcile(&h).map_err(str::to_string)
    }
    /// Snapshot runtime handle IDs still named by the completion gate. Taking this
    /// snapshot before locking the subagent registry avoids cross-registry lock order.
    pub fn unreconciled_runtime_handles(&self) -> HashSet<String> {
        let inner = self.inner.lock().unwrap();
        let workers = match inner.registry.completion_gate() {
            CompletionGate::Allowed => return HashSet::new(),
            CompletionGate::Warning { workers } | CompletionGate::Blocked { workers } => workers,
        };
        inner
            .handles
            .iter()
            .filter(|(_, handle)| workers.iter().any(|id| id == handle.id()))
            .map(|(runtime_id, _)| runtime_id.clone())
            .collect()
    }

    /// Whether a runtime handle is still named by the policy completion gate.
    pub fn is_unreconciled(&self, runtime_handle: &str) -> bool {
        self.unreconciled_runtime_handles().contains(runtime_handle)
    }

    pub fn completion_gate(&self) -> CompletionGate {
        // Core WorkerRegistry reports internal policy IDs (`worker-N`). Map them
        // back through the runtime handle table so tool-facing remediation can
        // cite `sa_*` handles accepted by `subagent_collect`.
        let inner = self.inner.lock().unwrap();
        match inner.registry.completion_gate() {
            CompletionGate::Allowed => CompletionGate::Allowed,
            CompletionGate::Warning { workers } => CompletionGate::Warning {
                workers: Self::map_policy_ids_to_runtime(&inner.handles, workers),
            },
            CompletionGate::Blocked { workers } => CompletionGate::Blocked {
                workers: Self::map_policy_ids_to_runtime(&inner.handles, workers),
            },
        }
    }

    /// Map policy `WorkerHandle` IDs (`worker-N`) to runtime handles (`sa_*`).
    /// Preserves the deterministic order of the policy gate's worker list.
    fn map_policy_ids_to_runtime(
        handles: &HashMap<String, agent_core::orchestration::WorkerHandle>,
        policy_ids: Vec<String>,
    ) -> Vec<String> {
        let reverse: HashMap<&str, &str> = handles
            .iter()
            .map(|(runtime_handle, policy_handle)| (policy_handle.id(), runtime_handle.as_str()))
            .collect();
        policy_ids
            .into_iter()
            .map(|policy_id| {
                reverse
                    .get(policy_id.as_str())
                    .map(|runtime| (*runtime).to_owned())
                    .unwrap_or(policy_id)
            })
            .collect()
    }
    pub fn check_foreground_write(&self, path: &str) -> agent_core::orchestration::ScopeDecision {
        self.inner
            .lock()
            .unwrap()
            .registry
            .check_foreground_write(path)
    }
    pub fn telemetry_json(&self) -> String {
        serde_json::to_string(self.inner.lock().unwrap().registry.telemetry())
            .unwrap_or_else(|_| "[]".into())
    }
    pub fn enforcement_mode(&self) -> agent_core::orchestration::EnforcementMode {
        self.inner.lock().unwrap().registry.mode()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn model(s: &str) -> QualifiedModelId {
        QualifiedModelId::parse(s).unwrap()
    }
    /// Regression: a legacy config (`model = claude-fable-5`, no provider
    /// segment) must not brick startup. The router deliberately routes bare
    /// Claude IDs to `anthropic`, so the baseline foreground identity must be
    /// the router's canonical `anthropic/<id>` — not a startup Config error.
    #[test]
    fn legacy_bare_claude_foreground_canonicalizes_via_router() {
        let id = canonical_foreground_identity("claude-fable-5").unwrap();
        assert_eq!(id.as_str(), "anthropic/claude-fable-5");
        // The canonical identity must satisfy the manifestless baseline.
        OrchestrationRuntime::baseline(id, 8, 64).unwrap();
    }

    #[test]
    fn baseline_openrouter_catalog_authorizes_exact_chinese_worker_stack() {
        let deepseek = model("openrouter/deepseek/deepseek-v4-pro");
        let glm = model("openrouter/z-ai/glm-5.2");
        let kimi = model("openrouter/moonshotai/kimi-k2.7-code");
        let rt = OrchestrationRuntime::baseline(deepseek.clone(), 3, 8).unwrap();

        assert_eq!(
            rt.effective_choices(),
            vec![
                deepseek.as_str().to_owned(),
                kimi.as_str().to_owned(),
                glm.as_str().to_owned(),
            ]
        );
        assert_eq!(
            rt.resolve_and_authorize("sa_glm", Some(glm.as_str()))
                .unwrap()
                .model,
            glm
        );
        assert_eq!(
            rt.resolve_and_authorize("sa_kimi", Some(kimi.as_str()))
                .unwrap()
                .model,
            kimi
        );
    }

    #[test]
    fn manifestless_non_openrouter_baseline_remains_foreground_only() {
        let foreground = model("anthropic/claude-fable-5");
        let rt = OrchestrationRuntime::baseline(foreground.clone(), 3, 8).unwrap();
        assert_eq!(rt.effective_choices(), vec![foreground.as_str().to_owned()]);
        let denied = rt
            .resolve_and_authorize(
                "sa_openrouter",
                Some("openrouter/moonshotai/kimi-k2.7-code"),
            )
            .unwrap_err();
        assert!(matches!(
            denied.code,
            "provider_not_allowed" | "catalog_model_unknown"
        ));
    }

    /// Regression: an explicit mid-session user trust grant must flip an
    /// exact-model dispatch from `provider_not_allowed` to authorized without
    /// restarting the session. Trusted models were never meant to be pinned
    /// at session start.
    #[test]
    fn baseline_honors_mid_session_user_grant() {
        let foreground = model("anthropic/claude-fable-5");
        let requested = "openai-codex/gpt-5.6-sol";
        let rt = OrchestrationRuntime::baseline(foreground.clone(), 3, 8).unwrap();
        assert_eq!(rt.effective_choices(), vec![foreground.as_str().to_owned()]);
        let denied = rt
            .resolve_and_authorize("sa_denied", Some(requested))
            .unwrap_err();
        assert!(matches!(
            denied.code,
            "provider_not_allowed" | "catalog_model_unknown"
        ));
        assert!(!denied.network_attempted);

        rt.grant_worker_model(requested).unwrap();

        assert!(rt.effective_choices().contains(&requested.to_owned()));
        let authorized = rt
            .resolve_and_authorize("sa_granted", Some(requested))
            .unwrap();
        assert_eq!(authorized.model.as_str(), requested);
        assert_eq!(
            authorized.cross_provider_grant_id.as_deref(),
            Some("session-user-grant-openai-codex/gpt-5.6-sol")
        );
        assert!(!authorized.network_attempted);
        rt.rollback("sa_granted");
        assert_eq!(rt.completion_gate(), CompletionGate::Allowed);
    }

    /// A grant for a malformed identity fails closed and changes nothing.
    #[test]
    fn mid_session_grant_rejects_invalid_qualified_model() {
        let foreground = model("anthropic/claude-fable-5");
        let rt = OrchestrationRuntime::baseline(foreground.clone(), 3, 8).unwrap();
        assert!(rt.grant_worker_model("").is_err());
        assert!(rt.grant_worker_model("not-qualified").is_err());
        assert_eq!(rt.effective_choices(), vec![foreground.as_str().to_owned()]);
    }

    #[test]
    fn qualified_foreground_identity_passes_through_unchanged() {
        for raw in [
            "openai-codex/gpt-5.6-sol",
            "anthropic/claude-fable-5",
            "openrouter/z-ai/glm-5.1",
        ] {
            assert_eq!(canonical_foreground_identity(raw).unwrap().as_str(), raw);
        }
    }

    #[test]
    fn unroutable_foreground_identity_fails_closed() {
        // Neither a valid qualified ID nor routable: empty model segment
        // under an unknown provider prefix.
        assert!(canonical_foreground_identity("foo/").is_err());
        assert!(canonical_foreground_identity("").is_err());
    }

    #[test]
    fn trusted_catalog_rejects_unresolved_foreground_and_ignores_manifest_candidates() {
        let unresolved = model("invented/foreground");
        assert!(OrchestrationRuntime::trusted_catalog(
            &unresolved,
            [model("anthropic/claude-manifest-injected")]
        )
        .is_err());

        let foreground = model("openai-codex/gpt-5.1-codex-mini");
        let catalog = OrchestrationRuntime::trusted_catalog(
            &foreground,
            [model("anthropic/claude-manifest-injected")],
        )
        .unwrap();
        assert!(catalog.contains(&foreground));
        assert!(!catalog.contains(&model("anthropic/claude-manifest-injected")));
    }

    #[test]
    fn authorization_reserves_without_claiming_worker_is_running() {
        let foreground = model("anthropic/claude-foreground");
        let rt = OrchestrationRuntime::new(DelegationPolicy::enforced(
            foreground.clone(),
            [foreground.clone()],
            1,
            1,
        ));
        rt.resolve_and_authorize("sa_reserved", None).unwrap();
        assert!(rt.poll("sa_reserved", "not-started").is_err());
        rt.mark_starting("sa_reserved").unwrap();
        assert!(rt.poll("sa_reserved", "not-running").is_err());
        rt.rollback("sa_reserved");
        assert_eq!(rt.completion_gate(), CompletionGate::Allowed);
    }

    #[test]
    fn one_shot_uses_the_full_lifecycle_before_terminal_collection() {
        let foreground = model("anthropic/foreground");
        let rt = OrchestrationRuntime::new(DelegationPolicy::enforced(
            foreground.clone(),
            [foreground],
            1,
            1,
        ));
        rt.resolve_and_authorize("sa_one_shot", None).unwrap();
        rt.mark_starting("sa_one_shot").unwrap();
        rt.mark_running("sa_one_shot").unwrap();
        rt.finish_one_shot("sa_one_shot", WorkerTerminal::Completed)
            .unwrap();
        assert_eq!(rt.completion_gate(), CompletionGate::Allowed);
        let telemetry = rt.telemetry_json();
        for event in [
            "worker.dispatch_allowed",
            "worker.starting",
            "worker.running",
            "worker.terminal",
            "worker.collected",
            "worker.reconciled",
        ] {
            assert!(telemetry.contains(event), "missing {event}: {telemetry}");
        }
    }

    #[test]
    fn authorization_happens_before_a_worker_is_registered() {
        let rt = OrchestrationRuntime::new(DelegationPolicy::enforced(
            model("anthropic/foreground"),
            [model("anthropic/worker")],
            1,
            1,
        ));
        assert!(rt.authorize("sa_1", "openrouter/worker").is_err());
        assert_eq!(rt.completion_gate(), CompletionGate::Allowed);
        rt.authorize("sa_2", "anthropic/worker").unwrap();
        assert!(matches!(
            rt.completion_gate(),
            CompletionGate::Blocked { .. }
        ));
        rt.poll("sa_2", "same").unwrap();
        rt.steer("sa_2").unwrap();
        rt.terminal_and_collect("sa_2", WorkerTerminal::Completed)
            .unwrap();
        assert!(matches!(
            rt.completion_gate(),
            CompletionGate::Blocked { .. }
        ));
        rt.reconcile("sa_2").unwrap();
        assert_eq!(rt.completion_gate(), CompletionGate::Allowed);
    }

    /// Completion remediation must cite tool-facing `sa_*` handles, never the
    /// internal policy IDs (`worker-N`) that `WorkerRegistry` allocates.
    #[test]
    fn delegation_tree_depth_children_and_total_are_independently_bounded() {
        let foreground = model("anthropic/foreground");
        let rt = OrchestrationRuntime::new(DelegationPolicy::enforced(
            foreground.clone(),
            [foreground],
            8,
            16,
        ))
        .with_tree_budget(DelegationTreeBudget {
            max_depth: 2,
            max_children_per_worker: 2,
            max_total_descendants: 3,
        })
        .unwrap();

        assert_eq!(rt.reserve_delegation("a", None), Ok(1));
        assert_eq!(rt.reserve_delegation("b", Some("a")), Ok(2));
        assert_eq!(
            rt.reserve_delegation("too-deep", Some("b")),
            Err(DelegationTreeDenied::DepthLimit)
        );
        assert_eq!(rt.reserve_delegation("c", Some("a")), Ok(2));
        assert_eq!(
            rt.reserve_delegation("too-many", Some("a")),
            Err(DelegationTreeDenied::DescendantLimit)
        );
        assert_eq!(rt.delegation_descendants(), 3);
        rt.release_delegation("c", Some("a"));
        assert_eq!(rt.delegation_descendants(), 2);
    }

    #[test]
    fn completion_gate_reports_runtime_handles_not_policy_ids() {
        let rt = OrchestrationRuntime::new(DelegationPolicy::enforced(
            model("anthropic/foreground"),
            [model("anthropic/worker")],
            2,
            2,
        ));
        rt.authorize("sa_alpha", "anthropic/worker").unwrap();
        rt.authorize("sa_beta", "anthropic/worker").unwrap();

        match rt.completion_gate() {
            CompletionGate::Blocked { workers } => {
                assert_eq!(
                    workers,
                    vec!["sa_alpha".to_string(), "sa_beta".to_string()],
                    "blocked IDs must be runtime handles in deterministic policy order"
                );
                for id in &workers {
                    assert!(
                        id.starts_with("sa_"),
                        "blocked id must be a subagent_collect handle, got {id}"
                    );
                    assert!(
                        !id.starts_with("worker-"),
                        "blocked id must never leak policy WorkerHandle, got {id}"
                    );
                }
            }
            other => panic!("expected Blocked with two sa_* handles, got {other:?}"),
        }

        // After one reconcile, only the remaining runtime handle is reported.
        rt.terminal_and_collect("sa_alpha", WorkerTerminal::Completed)
            .unwrap();
        rt.reconcile("sa_alpha").unwrap();
        match rt.completion_gate() {
            CompletionGate::Blocked { workers } => {
                assert_eq!(workers, vec!["sa_beta".to_string()]);
            }
            other => panic!("expected only sa_beta still blocked, got {other:?}"),
        }
    }
}
