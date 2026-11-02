//! A forked session's copy of its parent's runtime state (`fork`, side
//! chats): what the parent's next request is built from, taken as the
//! parent would send it, so the fork's first request carries the same prefix
//! (tools → system → messages) byte for byte and reads the parent's prompt
//! cache.
//!
//! Copied: the model and reasoning, the system prompt as composed (a `/system`
//! text, a prompt manifest's compiled stack), the frozen tool catalog, the
//! activated tools, the context window, the compaction model, the cache TTL and
//! the context-management settings. Never copied: anything of the parent's
//! turn, workers, prompts, spend, memory leases or confirm consent (the fork's
//! runtime is built fresh; only these values are set on it).

use std::sync::Arc;

use crate::tools::activation::SharedSessionToolSet;

/// The parent's runtime state a fork copies ([`super::Runtime::fork_seed`]).
#[derive(Clone)]
pub struct RuntimeForkSeed {
    model: String,
    thinking_budget: u32,
    named_level: Option<agent_core::reasoning::ReasoningLevel>,
    explicit_reasoning: bool,
    system_prompt: Option<String>,
    effective_prompt: Option<agent_core::prompt::PromptStack>,
    prompt_reload_source: Option<super::PromptReloadSource>,
    prompt_generation: u64,
    /// The frozen catalog slot as it is (outer `None`: not rendered yet).
    tool_catalog_prompt: Option<Option<String>>,
    context_config: agent_core::core::config::ContextManagementConfig,
    context_window_override: Option<u64>,
    compaction_model: Option<String>,
    cache_ttl: crate::core::config::CacheTtlSetting,
    /// The parent's retained tool set (its exact activations).
    tool_set: Option<SharedSessionToolSet>,
}

impl RuntimeForkSeed {
    /// The model the fork runs.
    pub fn model(&self) -> &str {
        &self.model
    }
}

impl std::fmt::Debug for RuntimeForkSeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeForkSeed")
            .field("model", &self.model)
            .field("system_prompt", &self.system_prompt.as_ref().map(|_| "<set>"))
            .field("manifest", &self.effective_prompt.is_some())
            .finish_non_exhaustive()
    }
}

impl super::Runtime {
    /// What a fork of this runtime's session copies, as this runtime would
    /// send it now.
    pub fn fork_seed(&self) -> RuntimeForkSeed {
        let context_config = self
            .continuation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .config;
        RuntimeForkSeed {
            model: self.model.clone(),
            thinking_budget: self.thinking_budget,
            named_level: self.named_level,
            explicit_reasoning: self.explicit_reasoning,
            system_prompt: self.system_prompt.clone(),
            effective_prompt: self.effective_prompt.clone(),
            prompt_reload_source: self.prompt_reload_source.clone(),
            prompt_generation: self.prompt_generation,
            tool_catalog_prompt: self
                .tool_catalog_prompt
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            context_config,
            context_window_override: self.context_window_override,
            compaction_model: self.compaction_model.clone(),
            cache_ttl: self.cache_ttl,
            tool_set: self
                .retained_tool_set
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        }
    }

    /// Make this runtime, built fresh for the fork `session_id` whose history
    /// is `messages`, send what the parent sends. Call after the settings
    /// replay (it overrides what the replay set). The context-continuation
    /// state is bound to the fork's id and restored from the markers in
    /// `messages`. Exact activations are granted again to this runtime's own
    /// tool session, with the checks of a re-activation; one that no longer
    /// matches the catalog is dropped (logged), as at a turn start.
    pub async fn apply_fork_seed(
        &mut self,
        seed: &RuntimeForkSeed,
        session_id: &str,
        messages: &[crate::SharedMessage],
    ) {
        if self.model != seed.model {
            self.set_model(seed.model.clone());
        }
        self.thinking_budget = seed.thinking_budget;
        self.named_level = seed.named_level;
        self.explicit_reasoning = seed.explicit_reasoning;
        self.system_prompt = seed.system_prompt.clone();
        self.effective_prompt = seed.effective_prompt.clone();
        self.prompt_reload_source = seed.prompt_reload_source.clone();
        self.prompt_generation = seed.prompt_generation;
        *self
            .tool_catalog_prompt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = seed.tool_catalog_prompt.clone();
        self.context_window_override = seed.context_window_override;
        self.compaction_model = seed.compaction_model.clone();
        self.cache_ttl = seed.cache_ttl;
        self.continuation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .config = seed.context_config;
        self.reset_context_continuation(session_id, messages);
        if let Some(parent) = &seed.tool_set {
            self.fork_tool_set(parent).await;
        }
    }

    /// The fork's retained tool set: its own core, plus each of the parent's
    /// exact activations granted again to this runtime's tool session (grants
    /// never cross sessions; this is a re-activation, checked as one).
    async fn fork_tool_set(&mut self, parent: &SharedSessionToolSet) {
        if !self.progressive_tool_disclosure || !crate::tools::activation::carry_forward_enabled() {
            return;
        }
        let registry = self.tools.read().await;
        let catalog = registry.catalog();
        let mut set = super::continuation::context_tool_set(
            self.host_tool_session.clone(),
            catalog,
            true,
            self.context_management_enabled(),
            self.memory_backend.is_axel(),
        );
        let wanted: Vec<_> = parent
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .activated()
            .map(|a| (a.grant().tool_id().clone(), a.schema_digest().clone(), a.provenance().clone()))
            .collect();
        for (id, digest, provenance) in wanted {
            if set.is_core(&id) || set.is_withheld(&id) {
                continue;
            }
            let same = catalog
                .get(&id)
                .is_some_and(|rec| rec.schema_digest() == &digest && rec.provenance() == &provenance);
            let granted = same
                && crate::tools::catalog::SessionActivationGrant::new(
                    self.host_tool_session.as_str(),
                    id.clone(),
                    catalog.generation(),
                    digest,
                )
                .ok()
                .is_some_and(|grant| set.activate_many(vec![grant], catalog).is_ok());
            if !granted {
                tracing::warn!(tool = %id, "activation not carried to the fork");
            }
        }
        drop(registry);
        *self
            .retained_tool_set
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(Arc::new(std::sync::RwLock::new(set)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::orchestration::capability::ToolId;
    use serde_json::{json, Value};

    struct LateTool;
    #[async_trait::async_trait]
    impl crate::Tool for LateTool {
        fn name(&self) -> &str {
            "late_tool"
        }
        fn description(&self) -> &str {
            "Registered after the parent's catalog froze."
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn execute(&self, _p: Value, _c: crate::ToolContext) -> crate::Result<String> {
            Ok(String::new())
        }
    }

    /// A parent and a fresh runtime for its fork, on one shared catalog (as
    /// in a host), with progressive disclosure on.
    fn pair() -> (super::super::Runtime, super::super::Runtime) {
        let mut parent = super::super::Runtime::new_headless();
        parent.set_model("anthropic/claude-sonnet-5-5".to_string());
        parent.progressive_tool_disclosure = true;
        let mut child = super::super::Runtime::new_headless();
        child.tools = Arc::clone(&parent.tools);
        child.progressive_tool_disclosure = true;
        (parent, child)
    }

    /// Mutants: drop `apply_fork_seed`'s prompt or catalog copy and the
    /// fork's system prompt differs from the parent's (no `/system` text, or
    /// a catalog that lists `late_tool`).
    #[tokio::test]
    async fn a_fork_sends_its_parents_composed_prompt_and_frozen_catalog() {
        let (mut parent, mut child) = pair();
        parent.set_system_prompt("PARENT BASE.".to_string());
        let sent = parent.effective_system_prompt().await.expect("a prompt");
        assert!(sent.contains("## Activatable tools"), "precondition: a catalog is frozen");
        parent.tools.write().await.register(Arc::new(LateTool));
        assert!(
            !parent.effective_system_prompt().await.unwrap().contains("late_tool"),
            "precondition: the parent's catalog is frozen"
        );
        let seed = parent.fork_seed();
        child.apply_fork_seed(&seed, "fork-child", &[]).await;
        assert_eq!(child.model(), parent.model());
        assert_eq!(child.effective_system_prompt().await.as_deref(), Some(sent.as_str()));
    }

    /// Mutants: skip `fork_tool_set` (the fork has no activation), or hand
    /// the fork the parent's own set (one set for two sessions: the grant
    /// would cross sessions).
    #[tokio::test]
    async fn a_fork_gets_its_parents_activations_under_its_own_tool_session() {
        let (parent, mut child) = pair();
        let deferred = ToolId::builtin("subagent_start");
        {
            let registry = parent.tools.read().await;
            let catalog = registry.catalog();
            let mut set = super::super::continuation::context_tool_set(
                parent.host_tool_session.clone(),
                catalog,
                true,
                parent.context_management_enabled(),
                false,
            );
            assert!(!set.is_core(&deferred), "precondition: an activatable tool");
            let rec = catalog.get(&deferred).expect("cataloged");
            let grant = crate::tools::catalog::SessionActivationGrant::new(
                parent.host_tool_session.as_str(),
                deferred.clone(),
                catalog.generation(),
                rec.schema_digest().clone(),
            )
            .unwrap();
            set.activate(grant, catalog).unwrap();
            *parent.retained_tool_set.lock().unwrap() = Some(Arc::new(std::sync::RwLock::new(set)));
        }
        let seed = parent.fork_seed();
        child.apply_fork_seed(&seed, "fork-child", &[]).await;

        let parent_set = parent.retained_tool_set.lock().unwrap().clone().unwrap();
        let child_set = child.retained_tool_set.lock().unwrap().clone().expect("the fork has a set");
        assert!(!Arc::ptr_eq(&parent_set, &child_set), "the fork's set is its own");
        let child_set = child_set.read().unwrap();
        assert_eq!(child_set.session(), &child.host_tool_session);
        assert_ne!(child_set.session(), &parent.host_tool_session);
        let carried = child_set.activation(&deferred).expect("the activation is carried");
        assert_eq!(carried.grant().tool_id(), &deferred);
        assert_eq!(child_set.activated().count(), 1);
    }
}
