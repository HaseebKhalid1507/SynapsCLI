//! `RuntimeView` + `RuntimeRead` (PLAN-phase2 §2.8).
//!
//! The TUI's synchronous getters (`runtime.model()` …) compile against
//! either a live `Runtime` (today) or a published `RuntimeView` snapshot
//! (day 2, over a transport). Trait signatures are copied EXACTLY from
//! `Runtime` so call sites do not change.

use agent_core::reasoning::ReasoningLevel;
use serde::{Deserialize, Serialize};

use super::types::reasoning_level_serde;

/// Snapshot of the read-mostly Runtime getters the clients render from.
/// Refreshed by the actor after every Set, after prompt reload, at turn start.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RuntimeView {
    pub model: String,
    pub thinking_level: String,
    #[serde(with = "reasoning_level_serde")]
    pub reasoning_level: ReasoningLevel,
    pub is_reasoning_explicit: bool,
    pub thinking_budget: u32,
    pub context_window: u64,
    pub system_prompt: Option<String>,
    pub compaction_model: String,
    pub api_retries: u32,
    pub subagent_timeout: u64,
    pub max_tool_output: usize,
    pub bash_timeout: u64,
    pub bash_max_timeout: u64,
    pub prompt_generation: u64,
    pub hook_handler_count: usize,
    /// `Runtime::prompt_inspection_json()` cached at snapshot time
    /// (read once per `/prompt`).
    pub prompt_inspection: Option<String>,
}

impl RuntimeView {
    /// Synchronous snapshot. `hook_handler_count` is supplied by the caller
    /// because `HookBus::handler_count` is async.
    pub fn snapshot(runtime: &crate::Runtime, hook_handler_count: usize) -> Self {
        Self {
            model: runtime.model().to_string(),
            thinking_level: runtime.thinking_level().to_string(),
            reasoning_level: runtime.reasoning_level(),
            is_reasoning_explicit: runtime.is_reasoning_explicit(),
            thinking_budget: runtime.thinking_budget(),
            context_window: runtime.context_window(),
            system_prompt: runtime.system_prompt().map(str::to_string),
            compaction_model: runtime.compaction_model().to_string(),
            api_retries: runtime.api_retries(),
            subagent_timeout: runtime.subagent_timeout(),
            max_tool_output: runtime.max_tool_output(),
            bash_timeout: runtime.bash_timeout(),
            bash_max_timeout: runtime.bash_max_timeout(),
            prompt_generation: runtime.prompt_generation(),
            hook_handler_count,
            prompt_inspection: runtime.prompt_inspection_json(),
        }
    }

    /// Snapshot including the live hook handler count.
    pub async fn from_runtime(runtime: &crate::Runtime) -> Self {
        let n = runtime.hook_bus().handler_count().await;
        Self::snapshot(runtime, n)
    }
}

/// The 14 getters, with the EXACT signatures `Runtime` has today, so TUI
/// call sites compile against either. Implemented for `Runtime` by
/// delegation and for `RuntimeView` by field access.
pub trait RuntimeRead {
    fn model(&self) -> &str;
    fn thinking_level(&self) -> &str;
    fn reasoning_level(&self) -> ReasoningLevel;
    fn is_reasoning_explicit(&self) -> bool;
    fn thinking_budget(&self) -> u32;
    fn context_window(&self) -> u64;
    fn system_prompt(&self) -> Option<&str>;
    fn compaction_model(&self) -> &str;
    fn api_retries(&self) -> u32;
    fn subagent_timeout(&self) -> u64;
    fn max_tool_output(&self) -> usize;
    fn bash_timeout(&self) -> u64;
    fn bash_max_timeout(&self) -> u64;
    /// `RuntimeView`: cached at snapshot time.
    fn prompt_inspection_json(&self) -> Option<String>;
}

impl RuntimeRead for crate::Runtime {
    fn model(&self) -> &str {
        crate::Runtime::model(self)
    }
    fn thinking_level(&self) -> &str {
        crate::Runtime::thinking_level(self)
    }
    fn reasoning_level(&self) -> ReasoningLevel {
        crate::Runtime::reasoning_level(self)
    }
    fn is_reasoning_explicit(&self) -> bool {
        crate::Runtime::is_reasoning_explicit(self)
    }
    fn thinking_budget(&self) -> u32 {
        crate::Runtime::thinking_budget(self)
    }
    fn context_window(&self) -> u64 {
        crate::Runtime::context_window(self)
    }
    fn system_prompt(&self) -> Option<&str> {
        crate::Runtime::system_prompt(self)
    }
    fn compaction_model(&self) -> &str {
        crate::Runtime::compaction_model(self)
    }
    fn api_retries(&self) -> u32 {
        crate::Runtime::api_retries(self)
    }
    fn subagent_timeout(&self) -> u64 {
        crate::Runtime::subagent_timeout(self)
    }
    fn max_tool_output(&self) -> usize {
        crate::Runtime::max_tool_output(self)
    }
    fn bash_timeout(&self) -> u64 {
        crate::Runtime::bash_timeout(self)
    }
    fn bash_max_timeout(&self) -> u64 {
        crate::Runtime::bash_max_timeout(self)
    }
    fn prompt_inspection_json(&self) -> Option<String> {
        crate::Runtime::prompt_inspection_json(self)
    }
}

impl RuntimeRead for RuntimeView {
    fn model(&self) -> &str {
        &self.model
    }
    fn thinking_level(&self) -> &str {
        &self.thinking_level
    }
    fn reasoning_level(&self) -> ReasoningLevel {
        self.reasoning_level
    }
    fn is_reasoning_explicit(&self) -> bool {
        self.is_reasoning_explicit
    }
    fn thinking_budget(&self) -> u32 {
        self.thinking_budget
    }
    fn context_window(&self) -> u64 {
        self.context_window
    }
    fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }
    fn compaction_model(&self) -> &str {
        &self.compaction_model
    }
    fn api_retries(&self) -> u32 {
        self.api_retries
    }
    fn subagent_timeout(&self) -> u64 {
        self.subagent_timeout
    }
    fn max_tool_output(&self) -> usize {
        self.max_tool_output
    }
    fn bash_timeout(&self) -> u64 {
        self.bash_timeout
    }
    fn bash_max_timeout(&self) -> u64 {
        self.bash_max_timeout
    }
    fn prompt_inspection_json(&self) -> Option<String> {
        self.prompt_inspection.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> RuntimeView {
        RuntimeView {
            model: "m".into(),
            thinking_level: "high".into(),
            reasoning_level: ReasoningLevel::High,
            is_reasoning_explicit: false,
            thinking_budget: 1024,
            context_window: 200_000,
            system_prompt: Some("sys".into()),
            compaction_model: "c".into(),
            api_retries: 3,
            subagent_timeout: 60,
            max_tool_output: 4096,
            bash_timeout: 30,
            bash_max_timeout: 300,
            prompt_generation: 1,
            hook_handler_count: 0,
            prompt_inspection: None,
        }
    }

    #[test]
    fn view_json_round_trip() {
        let v = view();
        let json = serde_json::to_string(&v).unwrap();
        let back: RuntimeView = serde_json::from_str(&json).unwrap();
        assert_eq!(v, back);
        assert!(json.contains(r#""reasoning_level":"high""#));
    }

    #[test]
    fn view_reads_through_trait() {
        fn read(r: &impl RuntimeRead) -> (String, u64) {
            (r.model().to_string(), r.context_window())
        }
        assert_eq!(read(&view()), ("m".to_string(), 200_000));
    }

    #[test]
    fn headless_runtime_snapshot_matches_getters() {
        let rt = crate::Runtime::new_headless();
        let v = RuntimeView::snapshot(&rt, 0);
        assert_eq!(v.model, crate::Runtime::model(&rt));
        assert_eq!(v.thinking_level, crate::Runtime::thinking_level(&rt));
        assert_eq!(v.context_window, crate::Runtime::context_window(&rt));
        assert_eq!(v.bash_timeout, crate::Runtime::bash_timeout(&rt));
        assert_eq!(RuntimeRead::model(&v), RuntimeRead::model(&rt));
    }
}
