use agent_core::orchestration::{
    CompletionGate, DelegationPolicy, WorkerRegistry, WorkerRole, WorkerTerminal, WorkerWritePolicy,
};
use agent_core::prompt::QualifiedModelId;
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
impl OrchestrationRuntime {
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
