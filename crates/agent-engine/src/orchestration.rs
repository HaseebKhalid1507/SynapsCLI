use agent_core::orchestration::{
    CatalogSnapshot, CompletionGate, DelegationPolicy, WorkerRegistry, WorkerRole, WorkerTerminal,
    WorkerWritePolicy,
};
use agent_core::prompt::QualifiedModelId;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Mutex;

/// Why a delegation was refused before any worker was built. How many
/// workers may run (and exist unreconciled) is the delegation policy's job
/// (`subagent.max_concurrent` / `subagent.max_total`); the tree only adds
/// what the policy cannot see: how deep workers nest.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum DelegationTreeDenied {
    /// The new worker would sit deeper than `subagent.max_depth`.
    DepthLimit { max_depth: u16 },
    /// The handle is already reserved (a retried registration or spawn).
    DuplicateWorker,
    /// The parent worker is gone (finished or cancelled with its subtree).
    UnknownParent,
}

impl std::fmt::Display for DelegationTreeDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DepthLimit { max_depth } => write!(
                f,
                "subagent nesting limit reached: workers may nest {max_depth} level(s) deep \
                 (subagent.max_depth). Do this work directly instead of delegating further."
            ),
            Self::DuplicateWorker => write!(f, "this subagent handle is already registered"),
            Self::UnknownParent => write!(
                f,
                "the parent worker of this delegation has already finished or been cancelled"
            ),
        }
    }
}

#[derive(Debug, Default)]
struct DelegationTreeState {
    depth_by_worker: BTreeMap<String, u16>,
    parent_by_worker: BTreeMap<String, String>,
    descendants: usize,
    /// Who delegated each worker (`<root>` for the foreground). Unlike
    /// `parent_by_worker` it outlives the worker's live edge: a finished
    /// child still belongs to its parent's subtree until the policy forgets
    /// it, which is what a worker's own completion gate counts. A refused
    /// dispatch drops its entry at once ([`OrchestrationRuntime::abandon_delegation`]).
    /// The policy's `max_total` does not strictly bound the retained entries
    /// between reaper passes: a reconciled worker stops counting against it
    /// before the reaper retires it.
    owner_by_worker: BTreeMap<String, String>,
}

/// Session-scoped runtime enforcement shared by every subagent tool path.
pub struct OrchestrationRuntime {
    inner: Mutex<Inner>,
    max_depth: u16,
    tree: Mutex<DelegationTreeState>,
}
struct Inner {
    registry: WorkerRegistry,
    handles: HashMap<String, agent_core::orchestration::WorkerHandle>,
}

/// Who asks the pending-worker gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelegationOwner<'a> {
    /// The top-level session: the root owner of every worker.
    Session,
    /// One worker, by runtime handle (`sa_*`).
    Worker(&'a str),
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
    } else if model.provider() == "muse" {
        // Documented seed or an exact row from this process's authenticated
        // live Muse catalog — never an invented id.
        crate::runtime::openai::catalog::muse_runtime_model(model.model())
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

    /// The manifestless baseline sized by the user's `subagent.*` limits.
    /// Every production install without a prompt-manifest policy uses this.
    pub fn from_limits(
        foreground: QualifiedModelId,
        limits: crate::config::SubagentLimits,
    ) -> Result<Self, &'static str> {
        Self::baseline(foreground, limits.max_concurrent, limits.max_total)?
            .with_max_depth(limits.max_depth)
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
        let mut policy =
            DelegationPolicy::provider_baseline(foreground, catalog, choices, concurrent, total)?;
        policy.recycle_reconciled_workers = true;
        Ok(Self::new(policy))
    }

    pub fn new(policy: DelegationPolicy) -> Self {
        Self {
            inner: Mutex::new(Inner {
                registry: WorkerRegistry::new(policy),
                handles: HashMap::new(),
            }),
            max_depth: crate::config::SubagentLimits::default().max_depth,
            tree: Mutex::new(DelegationTreeState::default()),
        }
    }

    /// How deep workers may nest (`subagent.max_depth`): 1 = only the
    /// foreground delegates. Zero would forbid all delegation and is refused.
    pub fn with_max_depth(mut self, max_depth: u16) -> Result<Self, &'static str> {
        if max_depth == 0 {
            return Err("invalid subagent nesting depth");
        }
        self.max_depth = max_depth;
        Ok(self)
    }

    /// Reserve one tree edge before allocating channels/threads/provider
    /// runtimes. `parent=None` is a foreground-root child. Only nesting depth
    /// is bounded here: fan-out and totals are the delegation policy's
    /// limits, checked right after in the same spawn path. Every denial is
    /// fail-closed and leaves the tree unchanged. A worker ID the completion
    /// registry still holds, or whose owner is still recorded, is a duplicate
    /// even after its edge was released, so a recorded owner is never
    /// replaced. On success the worker's owner is recorded in
    /// `owner_by_worker`, which outlives the edge (completion-lifetime
    /// ownership, read by `completion_gate_for` and
    /// `unreconciled_descendants_of`). Lock order: tree, then the completion
    /// registry.
    pub fn reserve_delegation(
        &self,
        worker_id: &str,
        parent: Option<&str>,
    ) -> Result<u16, DelegationTreeDenied> {
        let mut tree = self.tree.lock().unwrap();
        // A handle the completion registry still holds is a duplicate too,
        // also after its edge was released: its first owner is kept.
        let held = self.inner.lock().unwrap().handles.contains_key(worker_id);
        if tree.depth_by_worker.contains_key(worker_id)
            || tree.owner_by_worker.contains_key(worker_id)
            || held
        {
            return Err(DelegationTreeDenied::DuplicateWorker);
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
        if depth > self.max_depth {
            return Err(DelegationTreeDenied::DepthLimit {
                max_depth: self.max_depth,
            });
        }
        let parent_key = parent.unwrap_or("<root>");
        tree.depth_by_worker.insert(worker_id.to_string(), depth);
        tree.parent_by_worker
            .insert(worker_id.to_string(), parent_key.to_string());
        tree.owner_by_worker
            .insert(worker_id.to_string(), parent_key.to_string());
        tree.descendants += 1;
        Ok(depth)
    }

    /// Release one reserved tree edge. Descendants are recursively removed
    /// because a live child cannot outlive its parent lineage. The caller-
    /// supplied `parent` is intentionally ignored: ownership is taken from the
    /// reservation recorded under the tree lock, so a stale or hostile caller
    /// cannot decrement another parent's fanout.
    pub fn release_delegation(&self, worker_id: &str, _parent: Option<&str>) {
        let mut tree = self.tree.lock().unwrap();
        let mut stack = vec![worker_id.to_string()];
        while let Some(current) = stack.pop() {
            let children = tree
                .parent_by_worker
                .iter()
                .filter_map(|(child, parent)| (parent == &current).then_some(child.clone()))
                .collect::<Vec<_>>();
            stack.extend(children);
            if tree.depth_by_worker.remove(&current).is_some() {
                tree.descendants = tree.descendants.saturating_sub(1);
                tree.parent_by_worker.remove(&current);
            }
        }
    }

    /// Runtime handles of `owner`'s running and terminal-but-unreconciled
    /// descendants (children, grandchildren, ...), never the owner itself or
    /// a sibling. `Session` is the root owner and reports exactly the
    /// session-wide unreconciled set (`unreconciled_runtime_handles`).
    ///
    /// Ownership is the policy's owner tree (`owner_by_worker`, the same one
    /// `completion_gate_for` reads): it outlives the live edge, a retired
    /// worker's children pass to its owner, and a refused dispatch leaves no
    /// entry. Lock order: the tree, then the completion registry, both held
    /// for one consistent snapshot.
    pub fn unreconciled_descendants_of(&self, owner: DelegationOwner<'_>) -> BTreeSet<String> {
        let tree = self.tree.lock().unwrap();
        let inner = self.inner.lock().unwrap();
        let unreconciled = inner.registry.all_unreconciled_ids();
        let in_subtree = |worker: &str, owner: &str| {
            let mut current = worker;
            // Owner chains end at `<root>`; the bound only guards against a
            // corrupted map, never a real tree (depth is bounded far below).
            for _ in 0..=usize::from(u16::MAX) {
                match tree.owner_by_worker.get(current) {
                    Some(parent) if parent == owner => return true,
                    Some(parent) => current = parent,
                    None => return false,
                }
            }
            false
        };
        inner
            .handles
            .iter()
            .filter(|(_, handle)| unreconciled.iter().any(|id| id == handle.id()))
            .filter(|(runtime_id, _)| match owner {
                DelegationOwner::Session => true,
                DelegationOwner::Worker(owner) => in_subtree(runtime_id, owner),
            })
            .map(|(runtime_id, _)| runtime_id.clone())
            .collect()
    }

    /// Undo a reservation whose worker never launched: its model, policy or
    /// capacity check refused it, or anything else failed before the policy
    /// authorized a handle for it. The live edge is released and its owner
    /// entry dropped, so a refused dispatch leaves nothing behind. Only for
    /// a worker the policy never authorized (it has no children).
    pub fn abandon_delegation(&self, worker_id: &str) {
        self.release_delegation(worker_id, None);
        self.forget_owners([worker_id]);
    }

    pub fn delegation_descendants(&self) -> usize {
        self.tree.lock().unwrap().descendants
    }

    /// How many workers the policy still remembers an owner for.
    #[cfg(test)]
    pub(crate) fn owner_entries(&self) -> usize {
        self.tree.lock().unwrap().owner_by_worker.len()
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
                remediation: match error.typed_code() {
                    agent_core::orchestration::DispatchFailureCode::ConcurrencyLimit =>
                        if inner.registry.policy().recycle_reconciled_workers {
                            "Wait for running workers to finish; collect and reconcile their results. The limit is subagent.max_concurrent in config. Changing model does not release capacity."
                        } else {
                            "Wait for running workers to finish; collect and reconcile their results. The prompt manifest's delegation policy sets this limit. Changing model does not release capacity."
                        },
                    agent_core::orchestration::DispatchFailureCode::TotalWorkerLimit =>
                        if inner.registry.policy().recycle_reconciled_workers {
                            "Collect finished workers with reconciled=true to release outstanding-worker capacity (subagent.max_total in config). Changing model does not release capacity."
                        } else {
                            "The explicit delegation policy cumulative worker budget is exhausted. Changing model or collecting workers does not renew it."
                        },
                    _ => "Omit model to inherit foreground, select an exact session choice, or trust the model mid-session (favorite it in the models picker).",
                },
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
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(handle) = inner.handles.get(runtime_handle).cloned() {
                let _ = inner.registry.rollback_dispatch(&handle);
                inner.handles.remove(runtime_handle);
            }
        }
        self.forget_owners([runtime_handle]);
    }

    /// Drop the owner entries of workers the policy no longer tracks. A
    /// forgotten worker's own children pass to its owner, so a worker that
    /// ended before its child (a running child never blocks) still leaves
    /// that child in its parent's subtree.
    fn forget_owners<'a>(&self, workers: impl IntoIterator<Item = &'a str>) {
        let mut tree = self.tree.lock().unwrap();
        for worker in workers {
            let Some(owner) = tree.owner_by_worker.remove(worker) else {
                continue;
            };
            for parent in tree.owner_by_worker.values_mut() {
                if parent == worker {
                    parent.clone_from(&owner);
                }
            }
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
        self.reconcile(id)?;
        // One-shot results have no retained reactive handle to be reaped later.
        self.retire_reconciled(&[id.to_owned()].into_iter().collect());
        Ok(())
    }
    /// A worker the user (or host teardown) cancelled has no result to
    /// integrate: the cancellation IS the attestation. Move it straight to
    /// Reconciled so it never pins rollover, model changes or the completion
    /// gate waiting for a collect the model may never make. The handle and
    /// its partial output stay collectable from the subagent registry.
    ///
    /// Only a Running (or already-terminal) worker is retired; one still
    /// Starting is left for the post-`mark_running` re-check. Idempotent.
    pub fn retire_cancelled(&self, id: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let Some(h) = inner.handles.get(id).cloned() else {
            return false;
        };
        inner
            .registry
            .mark_terminal(&h, WorkerTerminal::Failed)
            .and_then(|()| inner.registry.collect(&h))
            .and_then(|()| inner.registry.reconcile(&h))
            .is_ok()
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
    /// Reaper handoff: use exact IDs removed under the runtime registry lock,
    /// never a complement snapshot that could include newly registered workers.
    pub fn retire_reconciled(&self, removed_handles: &HashSet<String>) {
        let mut retired = Vec::new();
        {
            let mut inner = self.inner.lock().unwrap();
            for id in removed_handles {
                if let Some(handle) = inner.handles.get(id).cloned() {
                    if inner.registry.retire_reconciled(&handle) {
                        inner.handles.remove(id);
                        retired.push(id.as_str());
                    }
                }
            }
        }
        self.forget_owners(retired);
    }

    /// Snapshot runtime handle IDs still named by the completion gate. Taking this
    /// snapshot before locking the subagent registry avoids cross-registry lock order.
    pub fn unreconciled_runtime_handles(&self) -> HashSet<String> {
        let inner = self.inner.lock().unwrap();
        // Use all_unreconciled_ids (includes Running workers) — NOT
        // completion_gate (which only reports Terminal|Collected). The reaper
        // must retain handles the orchestration still tracks, even if the
        // gate lets the foreground turn through.
        let unreconciled = inner.registry.all_unreconciled_ids();
        if unreconciled.is_empty() {
            return HashSet::new();
        }
        inner
            .handles
            .iter()
            .filter(|(_, handle)| unreconciled.iter().any(|id| id == handle.id()))
            .map(|(runtime_id, _)| runtime_id.clone())
            .collect()
    }

    /// Whether a runtime handle is still named by the policy completion gate.
    pub fn is_unreconciled(&self, runtime_handle: &str) -> bool {
        self.unreconciled_runtime_handles().contains(runtime_handle)
    }

    /// The completion gate as one runtime sees it. `None` is the foreground
    /// root: the session-wide gate, exactly `completion_gate()`. A worker's
    /// own runtime (`Some(its handle)`) counts only its delegation subtree,
    /// the workers it delegated directly or through its descendants: a
    /// sibling the parent read but has not reconciled, or any other worker
    /// of the session, never blocks it.
    ///
    /// The subtree is per run. A resume is a new handle whose subtree starts
    /// empty: children the earlier run started stay under that run (and pass
    /// to its owner when it retires), and the foreground always counts them.
    /// The resumed run does not inherit them, because that would block it on
    /// children it may not be able to reconcile.
    pub fn completion_gate_for(&self, owner: Option<&str>) -> CompletionGate {
        let gate = self.completion_gate();
        let Some(owner) = owner else {
            return gate;
        };
        let tree = self.tree.lock().unwrap();
        let in_subtree = |worker: &str| {
            let mut current = worker;
            // Owner chains end at `<root>`; the bound only guards against a
            // corrupted map, never a real tree (depth is bounded far below).
            for _ in 0..=usize::from(u16::MAX) {
                match tree.owner_by_worker.get(current) {
                    Some(parent) if parent == owner => return true,
                    Some(parent) => current = parent,
                    None => return false,
                }
            }
            false
        };
        let mine = |workers: Vec<String>| -> Vec<String> {
            workers.into_iter().filter(|w| in_subtree(w)).collect()
        };
        match gate {
            CompletionGate::Allowed => CompletionGate::Allowed,
            CompletionGate::Warning { workers } => match mine(workers) {
                workers if workers.is_empty() => CompletionGate::Allowed,
                workers => CompletionGate::Warning { workers },
            },
            CompletionGate::Blocked { workers } => match mine(workers) {
                workers if workers.is_empty() => CompletionGate::Allowed,
                workers => CompletionGate::Blocked { workers },
            },
        }
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
    use std::sync::Arc;
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
        // Running workers pass through the gate (reactive pattern).
        assert_eq!(rt.completion_gate(), CompletionGate::Allowed);
        rt.poll("sa_2", "same").unwrap();
        rt.steer("sa_2").unwrap();
        rt.terminal_and_collect("sa_2", WorkerTerminal::Completed)
            .unwrap();
        // Terminal+Collected: gate blocks until reconciled.
        assert!(matches!(
            rt.completion_gate(),
            CompletionGate::Blocked { .. }
        ));
        rt.reconcile("sa_2").unwrap();
        assert_eq!(rt.completion_gate(), CompletionGate::Allowed);
    }

    /// The tree bounds nesting depth only. Fan-out is the policy's
    /// concurrency limit (see `fanout_is_bounded_by_policy_concurrency_not_a_tree_cap`).
    #[test]
    fn delegation_tree_bounds_depth_and_rejects_duplicate_handles() {
        let foreground = model("anthropic/foreground");
        let rt = OrchestrationRuntime::new(DelegationPolicy::enforced(
            foreground.clone(),
            [foreground],
            8,
            16,
        ))
        .with_max_depth(2)
        .unwrap();

        assert_eq!(rt.reserve_delegation("a", None), Ok(1));
        assert_eq!(
            rt.reserve_delegation("a", None),
            Err(DelegationTreeDenied::DuplicateWorker),
            "registration/spawn retry cannot double-reserve one handle"
        );
        assert_eq!(rt.reserve_delegation("b", Some("a")), Ok(2));
        assert_eq!(
            rt.reserve_delegation("too-deep", Some("b")),
            Err(DelegationTreeDenied::DepthLimit { max_depth: 2 })
        );
        // No per-parent child cap: siblings are limited by the policy only.
        for i in 0..20 {
            assert_eq!(
                rt.reserve_delegation(&format!("sibling-{i}"), Some("a")),
                Ok(2)
            );
        }
        assert_eq!(rt.delegation_descendants(), 22);
        rt.release_delegation("a", None);
        assert_eq!(
            rt.delegation_descendants(),
            0,
            "a parent takes its subtree with it"
        );
        assert!(OrchestrationRuntime::new(DelegationPolicy::enforced(
            model("anthropic/foreground"),
            [model("anthropic/foreground")],
            1,
            1,
        ))
        .with_max_depth(0)
        .is_err());
    }

    #[test]
    fn depth_denial_names_the_config_key() {
        let message = DelegationTreeDenied::DepthLimit { max_depth: 3 }.to_string();
        assert!(message.contains("3 level"), "{message}");
        assert!(message.contains("subagent.max_depth"), "{message}");
        assert!(
            !message.contains("DepthLimit"),
            "no Debug names in user text: {message}"
        );
    }

    /// Active reservation count is exact across reserve/release races.
    #[test]
    fn delegation_release_is_idempotent_under_completion_cancel_race() {
        let foreground = model("anthropic/foreground");
        let rt = Arc::new(OrchestrationRuntime::new(DelegationPolicy::enforced(
            foreground.clone(),
            [foreground],
            4,
            8,
        )));
        rt.reserve_delegation("child", None).unwrap();
        let a = Arc::clone(&rt);
        let b = Arc::clone(&rt);
        let t1 = std::thread::spawn(move || a.release_delegation("child", None));
        let t2 = std::thread::spawn(move || b.release_delegation("child", None));
        t1.join().unwrap();
        t2.join().unwrap();
        assert_eq!(rt.delegation_descendants(), 0);
    }

    #[test]
    fn parent_exit_releases_entire_live_descendant_subtree() {
        let foreground = model("anthropic/foreground");
        let rt = OrchestrationRuntime::new(DelegationPolicy::enforced(
            foreground.clone(),
            [foreground],
            8,
            16,
        ));
        rt.reserve_delegation("parent", None).unwrap();
        rt.reserve_delegation("child", Some("parent")).unwrap();
        rt.reserve_delegation("grandchild", Some("child")).unwrap();
        assert_eq!(rt.delegation_descendants(), 3);
        rt.release_delegation("parent", None);
        assert_eq!(rt.delegation_descendants(), 0);
        assert_eq!(
            rt.reserve_delegation("orphan", Some("child")),
            Err(DelegationTreeDenied::UnknownParent)
        );
    }

    /// Active reservation count is exact when a live worker completes while
    /// its parent is cancelled and recursively releases the same subtree.
    #[test]
    fn parent_cancel_racing_child_completion_releases_every_lease() {
        let foreground = model("anthropic/foreground");
        let rt = Arc::new(OrchestrationRuntime::new(DelegationPolicy::enforced(
            foreground.clone(),
            [foreground],
            8,
            16,
        )));
        rt.reserve_delegation("parent", None).unwrap();
        rt.reserve_delegation("child", Some("parent")).unwrap();
        rt.reserve_delegation("grandchild", Some("child")).unwrap();
        let cancel = Arc::clone(&rt);
        let complete = Arc::clone(&rt);
        let t1 = std::thread::spawn(move || cancel.release_delegation("parent", None));
        let t2 = std::thread::spawn(move || complete.release_delegation("child", Some("parent")));
        t1.join().unwrap();
        t2.join().unwrap();

        assert_eq!(rt.delegation_descendants(), 0);
        assert_eq!(
            rt.reserve_delegation("orphan", Some("grandchild")),
            Err(DelegationTreeDenied::UnknownParent)
        );
        assert_eq!(rt.reserve_delegation("replacement", None), Ok(1));
        rt.release_delegation("replacement", None);
        assert_eq!(rt.delegation_descendants(), 0);
    }

    /// A child receives tree lineage only, never a new policy/grant mutation:
    /// even when the session already has one explicit cross-provider grant,
    /// child lineage cannot use that fact to authorize a different identity.
    #[test]
    fn descendant_lineage_cannot_mint_or_broaden_session_model_grants() {
        let foreground = model("anthropic/foreground");
        let rt = OrchestrationRuntime::new(DelegationPolicy::enforced(
            foreground.clone(),
            [foreground],
            4,
            8,
        ));
        rt.grant_worker_model("openai-codex/gpt-5.6-sol").unwrap();
        rt.reserve_delegation("parent", None).unwrap();
        rt.reserve_delegation("child", Some("parent")).unwrap();

        assert!(rt.preflight("openai-codex/gpt-5.6-sol").is_ok());
        assert!(
            rt.preflight("openai-codex/gpt-5.6-luna").is_err(),
            "a child lineage must not broaden one exact session grant to a sibling model"
        );
        assert!(
            rt.preflight("openrouter/ungranted").is_err(),
            "tree lineage must never mint a cross-provider grant"
        );
        rt.release_delegation("parent", None);
        assert_eq!(rt.delegation_descendants(), 0);
    }

    /// Completion remediation must cite tool-facing `sa_*` handles, never the
    /// internal policy IDs (`worker-N`) that `WorkerRegistry` allocates.
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
        // Transition both to Terminal so the gate reports them.
        rt.terminal_and_collect("sa_alpha", WorkerTerminal::Completed)
            .unwrap();
        rt.terminal_and_collect("sa_beta", WorkerTerminal::Completed)
            .unwrap();

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
        rt.reconcile("sa_alpha").unwrap();
        match rt.completion_gate() {
            CompletionGate::Blocked { workers } => {
                assert_eq!(workers, vec!["sa_beta".to_string()]);
            }
            other => panic!("expected only sa_beta still blocked, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod capacity_recovery_tests {
    use super::*;
    fn baseline(concurrent: usize, total: usize) -> OrchestrationRuntime {
        OrchestrationRuntime::baseline(
            QualifiedModelId::parse("anthropic/claude-fable-5").unwrap(),
            concurrent,
            total,
        )
        .unwrap()
    }
    #[test]
    fn manifestless_long_session_reuses_capacity_without_replacing_policy() {
        let rt = baseline(8, 64);
        let choices = rt.effective_choices();
        for n in 0..150 {
            let id = format!("sa_{n}");
            rt.resolve_and_authorize(&id, None).unwrap();
            rt.mark_starting(&id).unwrap();
            rt.mark_running(&id).unwrap();
            rt.terminal_and_collect(&id, WorkerTerminal::Completed)
                .unwrap();
            rt.reconcile(&id).unwrap();
            rt.reconcile(&id).unwrap();
        }
        assert_eq!(rt.effective_choices(), choices);
        assert_eq!(rt.completion_gate(), CompletionGate::Allowed);
    }
    #[test]
    fn retirement_only_touches_explicit_reconciled_ids() {
        let rt = baseline(8, 64);
        for id in ["old", "new", "uncollected"] {
            rt.authorize(id, &rt.foreground_model()).unwrap();
            rt.terminal_and_collect(id, WorkerTerminal::Completed)
                .unwrap();
        }
        rt.reconcile("old").unwrap();
        rt.reconcile("new").unwrap();
        rt.retire_reconciled(&["old".into(), "uncollected".into()].into_iter().collect());
        let inner = rt.inner.lock().unwrap();
        assert!(!inner.handles.contains_key("old"));
        assert!(inner.handles.contains_key("new"));
        assert!(inner.handles.contains_key("uncollected"));
        assert_eq!(inner.registry.total_dispatched(), 3);
    }

    #[test]
    fn fanout_is_bounded_by_policy_concurrency_not_a_tree_cap() {
        let limits = crate::config::SubagentLimits {
            max_concurrent: 16,
            max_total: 64,
            max_depth: 4,
            ..Default::default()
        };
        let rt = OrchestrationRuntime::from_limits(
            QualifiedModelId::parse("anthropic/claude-fable-5").unwrap(),
            limits,
        )
        .unwrap();
        assert_eq!(
            rt.ultracode_readiness("anthropic/claude-fable-5"),
            Ok((16, 64))
        );
        // Same order as the spawn path: tree reservation, then the policy.
        for i in 0..16 {
            let id = format!("sa_{i}");
            assert_eq!(rt.reserve_delegation(&id, None), Ok(1), "worker {i}");
            rt.resolve_and_authorize(&id, None).unwrap();
        }
        assert_eq!(
            rt.reserve_delegation("sa_16", None),
            Ok(1),
            "the 17th root child is not refused by the tree"
        );
        let denial = rt.resolve_and_authorize("sa_16", None).unwrap_err();
        assert_eq!(denial.code, "concurrency_limit");
        assert!(
            denial.remediation.contains("subagent.max_concurrent"),
            "{}",
            denial.remediation
        );
        assert!(denial.remediation.contains("Changing model does not"));
    }

    #[test]
    fn capacity_returns_only_after_reconcile_and_concurrency_still_applies() {
        let rt = baseline(1, 2);
        rt.authorize("a", &rt.foreground_model()).unwrap();
        let denial = rt.resolve_and_authorize("b", None).unwrap_err();
        assert_eq!(denial.code, "concurrency_limit");
        assert!(denial.remediation.contains("Changing model does not"));
        rt.terminal_and_collect("a", WorkerTerminal::Failed)
            .unwrap();
        rt.authorize("b", &rt.foreground_model()).unwrap();
        rt.terminal_and_collect("b", WorkerTerminal::TimedOut)
            .unwrap();
        assert_eq!(
            rt.resolve_and_authorize("c", None).unwrap_err().code,
            "total_limit"
        );
        rt.reconcile("a").unwrap();
        rt.resolve_and_authorize("c", None).unwrap();
        assert!(matches!(
            rt.completion_gate(),
            CompletionGate::Blocked { .. }
        ));
    }
}

/// G*: the owner-scoped pending-worker gate over completion-lifetime
/// ownership (the policy's owner tree).
#[cfg(test)]
mod descendant_gate_tests {
    use super::*;
    use std::sync::Arc;

    fn rt() -> OrchestrationRuntime {
        OrchestrationRuntime::baseline(
            QualifiedModelId::parse("anthropic/claude-fable-5").unwrap(),
            16,
            64,
        )
        .unwrap()
    }

    /// The start path's order: tree edge, policy authorization, lifecycle.
    fn spawn(rt: &OrchestrationRuntime, id: &str, parent: Option<&str>) {
        rt.reserve_delegation(id, parent).unwrap();
        rt.resolve_and_authorize(id, None).unwrap();
        rt.mark_starting(id).unwrap();
        rt.mark_running(id).unwrap();
    }

    /// Terminal + collected, then the finalizer's reservation release.
    fn finish(rt: &OrchestrationRuntime, id: &str, parent: Option<&str>) {
        rt.terminal_and_collect(id, WorkerTerminal::Completed)
            .unwrap();
        rt.release_delegation(id, parent);
    }

    fn of(rt: &OrchestrationRuntime, owner: &str) -> Vec<String> {
        rt.unreconciled_descendants_of(DelegationOwner::Worker(owner))
            .into_iter()
            .collect()
    }

    fn session(rt: &OrchestrationRuntime) -> Vec<String> {
        rt.unreconciled_descendants_of(DelegationOwner::Session)
            .into_iter()
            .collect()
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn g1_running_child_counts_for_its_parent_not_for_a_sibling_or_itself() {
        let rt = rt();
        spawn(&rt, "sa_p", None);
        spawn(&rt, "sa_s", None);
        spawn(&rt, "sa_c", Some("sa_p"));
        assert_eq!(of(&rt, "sa_p"), ids(&["sa_c"]));
        assert_eq!(of(&rt, "sa_s"), ids(&[]), "a sibling never defers");
        assert_eq!(of(&rt, "sa_c"), ids(&[]), "a worker never counts itself");
        assert_eq!(of(&rt, "sa_unknown"), ids(&[]));
        assert_eq!(session(&rt), ids(&["sa_c", "sa_p", "sa_s"]));
    }

    #[test]
    fn g2_terminal_child_still_counts_after_its_reservation_is_released() {
        let rt = rt();
        spawn(&rt, "sa_p", None);
        spawn(&rt, "sa_c", Some("sa_p"));
        finish(&rt, "sa_c", Some("sa_p"));
        assert_eq!(
            rt.delegation_descendants(),
            1,
            "the child's reservation is really gone"
        );
        assert_eq!(of(&rt, "sa_p"), ids(&["sa_c"]));
        rt.reconcile("sa_c").unwrap();
        assert_eq!(of(&rt, "sa_p"), ids(&[]), "reconciliation removes it");
        assert_eq!(session(&rt), ids(&["sa_p"]));
    }

    #[test]
    fn g3_grandchild_under_a_released_edge_counts_for_the_grandparent_until_reconciled() {
        let rt = rt();
        spawn(&rt, "sa_p", None);
        spawn(&rt, "sa_c", Some("sa_p"));
        spawn(&rt, "sa_g", Some("sa_c"));
        // The intermediate finishes; its release takes the grandchild's edge.
        finish(&rt, "sa_c", Some("sa_p"));
        assert_eq!(rt.delegation_descendants(), 1, "only sa_p is reserved");
        assert_eq!(of(&rt, "sa_p"), ids(&["sa_c", "sa_g"]));
        assert_eq!(of(&rt, "sa_c"), ids(&["sa_g"]));
        assert_eq!(of(&rt, "sa_g"), ids(&[]));
        rt.reconcile("sa_c").unwrap();
        // A reconciled (even retired) intermediate does not hide the grandchild.
        rt.retire_reconciled(&["sa_c".to_owned()].into_iter().collect());
        assert_eq!(of(&rt, "sa_p"), ids(&["sa_g"]));
        finish(&rt, "sa_g", Some("sa_c"));
        assert_eq!(of(&rt, "sa_p"), ids(&["sa_g"]), "terminal, unreconciled");
        rt.reconcile("sa_g").unwrap();
        assert_eq!(of(&rt, "sa_p"), ids(&[]));
        assert_eq!(session(&rt), ids(&["sa_p"]));
    }

    /// The session is the root owner: its answer is exactly today's
    /// session-wide set in every lifecycle state, including workers that
    /// never reserved a tree edge (oneshot) and edge-released subtrees.
    #[test]
    fn g4_session_owner_is_exactly_the_session_wide_unreconciled_set() {
        let rt = rt();
        let check = |rt: &OrchestrationRuntime| {
            let expected: BTreeSet<String> =
                rt.unreconciled_runtime_handles().into_iter().collect();
            assert_eq!(
                rt.unreconciled_descendants_of(DelegationOwner::Session),
                expected
            );
        };
        check(&rt);
        spawn(&rt, "sa_a", None);
        rt.resolve_and_authorize("sa_oneshot", None).unwrap();
        check(&rt);
        spawn(&rt, "sa_b", Some("sa_a"));
        spawn(&rt, "sa_c", Some("sa_b"));
        check(&rt);
        finish(&rt, "sa_a", None);
        check(&rt);
        assert_eq!(session(&rt).len(), 4);
        rt.reconcile("sa_a").unwrap();
        rt.rollback("sa_oneshot");
        check(&rt);
        assert_eq!(session(&rt), ids(&["sa_b", "sa_c"]));
    }

    /// Ancestry has completion lifetime, never longer: denied, unauthorized,
    /// rolled-back and retired handles leave nothing behind, and a duplicate
    /// reservation cannot rewrite an existing owner.
    #[test]
    fn g5_ownership_is_pruned_with_the_completion_entry_and_never_rewritten() {
        let rt = rt();
        spawn(&rt, "sa_p", None);
        spawn(&rt, "sa_q", None);
        spawn(&rt, "sa_c", Some("sa_p"));
        assert_eq!(
            rt.reserve_delegation("sa_c", Some("sa_q")),
            Err(DelegationTreeDenied::DuplicateWorker)
        );
        assert_eq!(of(&rt, "sa_p"), ids(&["sa_c"]));
        assert_eq!(of(&rt, "sa_q"), ids(&[]));

        // Reserved, then refused before authorization (start.rs error path).
        rt.reserve_delegation("sa_denied", Some("sa_p")).unwrap();
        rt.abandon_delegation("sa_denied");
        // Authorized, then rolled back before running (mark_starting failure).
        rt.reserve_delegation("sa_rolled", Some("sa_p")).unwrap();
        rt.resolve_and_authorize("sa_rolled", None).unwrap();
        rt.rollback("sa_rolled");
        rt.release_delegation("sa_rolled", Some("sa_p"));
        // Reconciled and retired by the reaper.
        finish(&rt, "sa_c", Some("sa_p"));
        rt.reconcile("sa_c").unwrap();
        rt.retire_reconciled(&["sa_c".to_owned()].into_iter().collect());

        let tree = rt.tree.lock().unwrap();
        let left: Vec<_> = tree.owner_by_worker.keys().cloned().collect();
        assert_eq!(left, ids(&["sa_p", "sa_q"]));
        assert!(
            tree.owner_by_worker.values().all(|owner| owner == "<root>"),
            "root children"
        );
    }

    /// A child reserved but not yet registered when its parent's release
    /// sweeps the subtree keeps its owners if it still registers, and leaves
    /// nothing behind if its own spawn then fails and releases it.
    #[test]
    fn g7_mid_spawn_descendant_swept_by_a_parent_release() {
        let rt = rt();
        spawn(&rt, "sa_p", None);
        rt.reserve_delegation("sa_late", Some("sa_p")).unwrap();
        rt.reserve_delegation("sa_fail", Some("sa_p")).unwrap();
        finish(&rt, "sa_p", None);
        rt.resolve_and_authorize("sa_late", None).unwrap();
        assert_eq!(of(&rt, "sa_p"), ids(&["sa_late"]));
        // Its spawn fails before authorization: start.rs abandons it.
        rt.abandon_delegation("sa_fail");
        assert!(!rt.tree.lock().unwrap().owner_by_worker.contains_key("sa_fail"));
    }

    /// Lock order is tree, then completion registry, and the gate holds both
    /// for its snapshot. Concurrent spawn/finish/query must neither deadlock
    /// nor see a worker counted for anyone but its own ancestors. Each worker thread
    /// holds some running children until the query thread has seen them in
    /// its owner's answer, so non-empty answers are observed while the other
    /// thread keeps changing the registry.
    #[test]
    fn g6_concurrent_lifecycle_and_queries_keep_owner_scope() {
        use std::sync::mpsc::{channel, Sender, TryRecvError};
        type Hold = (&'static str, String, Sender<()>);
        let rt = Arc::new(rt());
        spawn(&rt, "sa_root_a", None);
        spawn(&rt, "sa_root_b", None);
        let (hold_tx, hold_rx) = channel::<Hold>();
        let workers: Vec<_> = ["sa_root_a", "sa_root_b"]
            .into_iter()
            .map(|parent| {
                let rt = Arc::clone(&rt);
                let hold_tx = hold_tx.clone();
                std::thread::spawn(move || {
                    for i in 0..50 {
                        let id = format!("{parent}_{i}");
                        spawn(&rt, &id, Some(parent));
                        if i % 10 == 5 {
                            let (release_tx, release_rx) = channel();
                            hold_tx.send((parent, id.clone(), release_tx)).unwrap();
                            release_rx.recv().unwrap();
                        }
                        finish(&rt, &id, Some(parent));
                        rt.reconcile(&id).unwrap();
                        rt.retire_reconciled(&[id].into_iter().collect());
                    }
                })
            })
            .collect();
        drop(hold_tx);
        let query = {
            let rt = Arc::clone(&rt);
            std::thread::spawn(move || {
                let scoped = |owner: &str, other: &str| {
                    let answer = rt.unreconciled_descendants_of(DelegationOwner::Worker(owner));
                    for id in &answer {
                        assert!(id.starts_with(&format!("{owner}_")), "{id}");
                        assert!(!id.starts_with(other));
                    }
                    answer
                };
                let mut held_seen = 0;
                loop {
                    scoped("sa_root_a", "sa_root_b");
                    scoped("sa_root_b", "sa_root_a");
                    match hold_rx.try_recv() {
                        Ok((owner, id, release)) => {
                            let other = if owner == "sa_root_a" {
                                "sa_root_b"
                            } else {
                                "sa_root_a"
                            };
                            let answer = scoped(owner, other);
                            assert!(answer.contains(&id), "{owner} must see {id}: {answer:?}");
                            held_seen += 1;
                            release.send(()).unwrap();
                        }
                        Err(TryRecvError::Empty) => {}
                        Err(TryRecvError::Disconnected) => return held_seen,
                    }
                }
            })
        };
        for t in workers {
            t.join().unwrap();
        }
        assert_eq!(query.join().unwrap(), 10, "every held child was seen");
        assert_eq!(of(&rt, "sa_root_a"), ids(&[]));
        assert_eq!(session(&rt), ids(&["sa_root_a", "sa_root_b"]));
    }

    /// A reservation for a handle the completion registry still holds is
    /// refused, also after its edge was released: the owners recorded first
    /// are never replaced. Only once the entry is retired is the ID free.
    #[test]
    fn g8_duplicate_reservation_after_release_keeps_the_first_owner_chain() {
        let rt = rt();
        spawn(&rt, "sa_p", None);
        spawn(&rt, "sa_q", None);
        spawn(&rt, "sa_c", Some("sa_p"));
        finish(&rt, "sa_c", Some("sa_p"));
        let before = rt.delegation_descendants();
        for parent in [Some("sa_q"), None] {
            assert_eq!(
                rt.reserve_delegation("sa_c", parent),
                Err(DelegationTreeDenied::DuplicateWorker),
                "{parent:?}"
            );
        }
        assert_eq!(rt.delegation_descendants(), before, "nothing reserved");
        assert_eq!(of(&rt, "sa_p"), ids(&["sa_c"]));
        assert_eq!(of(&rt, "sa_q"), ids(&[]));
        assert_eq!(rt.tree.lock().unwrap().owner_by_worker["sa_c"], "sa_p");

        // An authorized handle that never reserved an edge (oneshot) is
        // held by the completion registry too.
        rt.resolve_and_authorize("sa_one", None).unwrap();
        assert_eq!(
            rt.reserve_delegation("sa_one", Some("sa_q")),
            Err(DelegationTreeDenied::DuplicateWorker)
        );
        assert!(!rt.tree.lock().unwrap().owner_by_worker.contains_key("sa_one"));

        rt.reconcile("sa_c").unwrap();
        rt.retire_reconciled(&["sa_c".to_owned()].into_iter().collect());
        rt.reserve_delegation("sa_c", Some("sa_q")).unwrap();
        assert_eq!(rt.tree.lock().unwrap().owner_by_worker["sa_c"], "sa_q");
    }
}
