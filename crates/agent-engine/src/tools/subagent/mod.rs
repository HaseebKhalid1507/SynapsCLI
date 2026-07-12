//! Subagent tools — oneshot and reactive (start/status/steer/collect/resume).

mod oneshot;
pub(crate) mod finalize;
pub mod start;
pub mod status;
pub mod steer;
pub mod collect;
pub mod resume;

pub use oneshot::SubagentTool;
pub use start::SubagentStartTool;
pub use status::SubagentStatusTool;
pub use steer::SubagentSteerTool;
pub use collect::SubagentCollectTool;
pub use resume::SubagentResumeTool;

/// Apply the subagent-spawn credential policy to a freshly-created `Runtime`
/// (which has already had `Runtime::new()` called), then **unconditionally
/// force** the cache TTL to `FiveMinutes`.
///
/// Subagents are short-lived one-shots. Paying the 1h-cache write premium
/// (~2× input price) on them is unrecoverable waste — a 10-spawn fan-out
/// costs ~$0.23 extra per session when the parent config opts into `1h` or
/// `hybrid`. This function is the single enforcement point; all three spawn
/// paths (`oneshot`, `start`, `resume`) call it so the policy cannot regress.
///
/// Called immediately after `Runtime::new()` in each spawn path, before any
/// streaming starts.
pub(crate) fn apply_subagent_runtime_policy(
    runtime: &mut crate::Runtime,
    config: &crate::config::SynapsConfig,
) {
    // Inherit credential source / token cache from the parent session's
    // resolved config — Remote broker endpoints must be reachable from
    // the subagent thread. (#158 A3)
    runtime.apply_auth_config(config);

    // Policy: subagent spawns are always 5m cache TTL regardless of what the
    // parent session configured. `Runtime::new()` already defaults to
    // `FiveMinutes`, but this explicit call makes the invariant contract-level
    // so a future `apply_config` addition can't silently break it.
    runtime.set_cache_ttl(crate::core::config::CacheTtl::FiveMinutes);
}

/// Build the subagent tool registry: extension tools if the routing manager
/// has a shared registry, otherwise the bare without_subagent set.
///
/// Single source of truth for both start.rs and resume.rs — divergence is
/// structurally impossible when both call this.
///
/// Mirrors start.rs:174-185 exactly.
pub(crate) async fn subagent_tools() -> crate::ToolRegistry {
    if let Some(ext_mgr) = crate::runtime::openai::extension_manager_for_routing() {
        let mgr = ext_mgr.read().await;
        if let Some(shared) = mgr.tools_shared() {
            let extension_tools = shared.read().await;
            return crate::ToolRegistry::without_subagent_with_extensions(&extension_tools);
        }
    }
    crate::ToolRegistry::without_subagent()
}

#[cfg(test)]
mod cache_ttl_policy_tests {
    use super::apply_subagent_runtime_policy;
    use crate::core::config::CacheTtl;

    /// Verify that `apply_subagent_runtime_policy` forces `FiveMinutes` even
    /// when the parent session has `cache_ttl = OneHour` in its config.
    ///
    /// This uses a real async `Runtime` (spawned on a single-threaded Tokio
    /// runtime) to confirm the full derivation path: `Runtime::new()` →
    /// `apply_config(1h parent)` → `apply_subagent_runtime_policy` →
    /// TTL must be `FiveMinutes`.
    ///
    /// The OLD code path (`apply_auth_config` only, no forced TTL) would
    /// leave the TTL at `OneHour` when `apply_config` is also called — this
    /// test is the regression guard for that scenario.
    #[tokio::test]
    async fn subagent_policy_forces_five_minutes_even_when_parent_is_one_hour() {
        // Build a config representing a parent session with 1h cache TTL.
        let parent_config = crate::config::SynapsConfig {
            cache_ttl: CacheTtl::OneHour,
            ..Default::default()
        };

        // Create a fresh runtime (as each subagent spawn does).
        let mut runtime = crate::Runtime::new().await
            .expect("Runtime::new() must succeed in test environment");

        // Simulate a scenario where apply_config was called with a 1h parent
        // config (realistic if a future refactor wires apply_config instead of
        // apply_auth_config). This is the "before" state the policy must override.
        runtime.set_cache_ttl(CacheTtl::OneHour);

        // Confirm the runtime IS at 1h before we apply the policy.
        assert_eq!(
            runtime.cache_ttl(),
            CacheTtl::OneHour,
            "pre-condition: runtime must be at OneHour before applying subagent policy"
        );

        // Apply the subagent runtime policy — this is what the spawn paths call.
        apply_subagent_runtime_policy(&mut runtime, &parent_config);

        // Post-condition: TTL must be FiveMinutes regardless of parent config.
        assert_eq!(
            runtime.cache_ttl(),
            CacheTtl::FiveMinutes,
            "subagent spawn must always use 5m TTL, even when parent config is 1h \
             (paying 1h write premium on short-lived one-shots is unrecoverable waste)"
        );
    }

    #[tokio::test]
    async fn subagent_policy_forces_five_minutes_even_when_parent_is_hybrid() {
        let parent_config = crate::config::SynapsConfig {
            cache_ttl: CacheTtl::Hybrid,
            ..Default::default()
        };

        let mut runtime = crate::Runtime::new().await
            .expect("Runtime::new() must succeed in test environment");

        // Simulate the parent having configured Hybrid on this runtime.
        runtime.set_cache_ttl(CacheTtl::Hybrid);

        assert_eq!(runtime.cache_ttl(), CacheTtl::Hybrid, "pre-condition: must be Hybrid");

        apply_subagent_runtime_policy(&mut runtime, &parent_config);

        assert_eq!(
            runtime.cache_ttl(),
            CacheTtl::FiveMinutes,
            "subagent spawn must always use 5m TTL, even when parent config is hybrid"
        );
    }

    #[tokio::test]
    async fn subagent_policy_is_idempotent_when_parent_already_five_minutes() {
        let parent_config = crate::config::SynapsConfig::default(); // FiveMinutes by default

        let mut runtime = crate::Runtime::new().await
            .expect("Runtime::new() must succeed in test environment");

        // Runtime::new() default is already FiveMinutes, but confirm it.
        assert_eq!(runtime.cache_ttl(), CacheTtl::FiveMinutes, "pre-condition: Runtime::new() must default to 5m");

        apply_subagent_runtime_policy(&mut runtime, &parent_config);

        assert_eq!(
            runtime.cache_ttl(),
            CacheTtl::FiveMinutes,
            "subagent spawn must stay 5m when parent is already 5m (idempotent)"
        );
    }
}
