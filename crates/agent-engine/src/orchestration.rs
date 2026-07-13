use agent_core::orchestration::{
    CatalogSnapshot, CompletionGate, DelegationPolicy, WorkerRegistry, WorkerRole, WorkerTerminal,
    WorkerWritePolicy,
};
use agent_core::prompt::QualifiedModelId;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;

/// Session-scoped runtime enforcement shared by every subagent tool path.
pub struct OrchestrationRuntime {
    inner: Mutex<Inner>,
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
        let catalog = Self::trusted_catalog(&foreground, std::iter::empty())?;
        Ok(Self::new(DelegationPolicy::baseline(
            foreground, catalog, concurrent, total,
        )?))
    }

    pub fn new(policy: DelegationPolicy) -> Self {
        Self {
            inner: Mutex::new(Inner {
                registry: WorkerRegistry::new(policy),
                handles: HashMap::new(),
            }),
        }
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
                remediation: "Omit model to inherit foreground or select an exact session choice.",
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
    pub fn completion_gate(&self) -> CompletionGate {
        self.inner.lock().unwrap().registry.completion_gate()
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
}
