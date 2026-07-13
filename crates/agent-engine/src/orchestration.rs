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
    /// Build the trusted snapshot from runtime routing descriptors, never from manifest
    /// authority. Candidates merely request entries; only routable worker identities enter.
    pub fn trusted_catalog(
        foreground: &QualifiedModelId,
        candidates: impl IntoIterator<Item = QualifiedModelId>,
    ) -> CatalogSnapshot {
        use agent_core::orchestration::CatalogEntry;
        let models = std::iter::once((foreground.clone(), true))
            .chain(candidates.into_iter().map(|model| (model, false)));
        CatalogSnapshot::from_entries(models.map(|(model, is_foreground)| {
            let available =
                is_foreground || crate::runtime::openai::resolve_route(model.as_str()).is_some();
            CatalogEntry {
                model,
                available,
                worker_eligible: available,
            }
        }))
    }

    /// Secure manifestless baseline: the exact foreground identity is the only
    /// worker choice in a deterministic runtime-controlled catalog.
    pub fn baseline(foreground: QualifiedModelId, concurrent: usize, total: usize) -> Self {
        let catalog = Self::trusted_catalog(&foreground, std::iter::empty());
        Self::new(
            DelegationPolicy::baseline(foreground, catalog, concurrent, total)
                .expect("resolved foreground always forms a valid baseline"),
        )
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
        if let Err(error) = inner.registry.mark_running(&handle) {
            let _ = inner.registry.rollback_dispatch(&handle);
            return Err(DispatchDenial {
                kind: "dispatch_denied",
                code: error,
                requested_model: Some(model.as_str().into()),
                foreground_model: foreground.as_str().into(),
                selection_source,
                network_attempted: false,
                remediation: "Retry with a new worker handle.",
            });
        }
        inner.handles.insert(runtime_handle.to_owned(), handle);
        Ok(AuthorizedWorkerModel {
            model,
            selection_source,
            network_attempted: false,
        })
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
