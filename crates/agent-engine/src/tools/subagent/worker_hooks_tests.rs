//! [`WorkerHooks`]: while a guard is required, a subagent worker's
//! `before_tool_call` runs its caller's chain, whatever
//! `subagent_transform_hooks` says, and every other worker is left as it was.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use super::{apply_subagent_runtime_policy, WorkerHooks, CALLER_HOOKS};
use crate::extensions::hooks::events::{HookEvent, HookKind, HookResult};
use crate::extensions::hooks::HookBus;
use crate::extensions::permissions::{Permission, PermissionSet};
use crate::extensions::required_guards::{NotLoaded, RequiredGuards};
use crate::extensions::runtime::ExtensionHandler;

type Seen = Arc<Mutex<Vec<HookEvent>>>;

/// A live guard named `guard` that answers every call with `result`.
struct Guard {
    seen: Seen,
    result: HookResult,
}

#[async_trait]
impl ExtensionHandler for Guard {
    fn id(&self) -> &str {
        "guard"
    }

    async fn guard_down(&self) -> Option<String> {
        None
    }

    async fn handle(&self, event: &HookEvent) -> HookResult {
        self.seen.lock().unwrap().push(event.clone());
        self.result.clone()
    }

    async fn shutdown(&self) {}
}

const REQUIRED: &str = "required_guards = guard\n";
const TRANSFORMS_OFF: &str = "subagent_transform_hooks = off\n";

/// A caller bus gated on required guard `guard`, subscribed to
/// `before_tool_call` and (when `loaded`) marked loaded.
async fn caller(result: HookResult, loaded: bool) -> (Arc<HookBus>, Seen) {
    let gate = Arc::new(RequiredGuards::new(&["guard".to_string()]));
    let bus = Arc::new(HookBus::new().with_required_guards(Arc::clone(&gate)));
    let seen: Seen = Arc::default();
    let handler: Arc<dyn ExtensionHandler> = Arc::new(Guard {
        seen: Arc::clone(&seen),
        result,
    });
    if loaded {
        gate.mark_loaded("guard", Arc::clone(&handler));
    }
    let mut permissions = PermissionSet::new();
    permissions.grant(Permission::ToolsIntercept);
    bus.subscribe(HookKind::BeforeToolCall, handler, None, None, permissions)
        .await
        .unwrap();
    (bus, seen)
}

fn call() -> HookEvent {
    HookEvent::before_tool_call("bash", json!({"command": "echo hi"}))
}

/// A worker's runtime as a spawn path leaves it: the host's transform view
/// (or, with `subagent_transform_hooks = off`, the detached bus), then the
/// worker's hooks applied after the policy.
fn worker_runtime(
    config: &str,
    host: &Arc<HookBus>,
    hooks: &WorkerHooks,
) -> crate::Runtime {
    let config = agent_core::config::load_config_from_str(config);
    let mut runtime = crate::Runtime::new_headless();
    runtime.set_hook_bus(HookBus::worker_transforms(host));
    apply_subagent_runtime_policy(&mut runtime, &config, None, None, None, &Default::default());
    hooks.apply(&mut runtime, &config);
    runtime
}

fn caller_capabilities(bus: &Arc<HookBus>) -> crate::tools::ToolCapabilities {
    crate::tools::ToolCapabilities {
        hook_bus: Some(Arc::clone(bus)),
        session_id: Some("sess-parent".into()),
        ..Default::default()
    }
}

/// A worker's tool call reaches the required guard, with the transform
/// switch on or off, and the guard's `Block` ends it.
#[tokio::test]
async fn a_required_guard_blocks_a_workers_tool_call_whatever_the_transform_switch_says() {
    for switch in ["", TRANSFORMS_OFF] {
        let (host, seen) = caller(HookResult::block("guard says no"), true).await;
        let hooks = WorkerHooks::of(&caller_capabilities(&host), "sa_3");
        let runtime = worker_runtime(&format!("{REQUIRED}{switch}"), &host, &hooks);
        assert!(runtime.has_worker_hook_route(), "{switch:?}");
        assert!(runtime.hook_bus().is_worker_route(), "{switch:?}");
        assert_eq!(
            runtime.hook_bus().emit(&call()).await,
            HookResult::block("guard says no"),
            "{switch:?}"
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "{switch:?}: the guard saw the worker's call");
        assert_eq!(seen[0].subagent_id.as_deref(), Some("sa_3"));
        assert_eq!(seen[0].session_id.as_deref(), Some("sess-parent"));
    }
}

/// A required guard's `Continue` lets a worker's call through, and with the
/// transform switch on the worker keeps the host's output transforms.
#[tokio::test]
async fn a_required_guard_that_allows_lets_a_workers_tool_call_through() {
    let (host, seen) = caller(HookResult::Continue, true).await;
    let hooks = WorkerHooks::of(&caller_capabilities(&host), "sa_4");
    let runtime = worker_runtime(REQUIRED, &host, &hooks);
    assert_eq!(runtime.hook_bus().emit(&call()).await, HookResult::Continue);
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert!(runtime.hook_bus().is_worker_view(), "the host's transform view is kept");

    let off = worker_runtime(&format!("{REQUIRED}{TRANSFORMS_OFF}"), &host, &hooks);
    assert!(!off.hook_bus().is_worker_view(), "switched off: no transforms");
    assert!(off.hook_bus().is_worker_route(), "but still routed to the guard");
}

/// A guard that is not loaded refuses a worker's call, naming the guard,
/// and never sees it.
#[tokio::test]
async fn an_unloaded_required_guard_refuses_a_workers_tool_call() {
    for switch in ["", TRANSFORMS_OFF] {
        let (host, seen) = caller(HookResult::Continue, false).await;
        let hooks = WorkerHooks::of(&caller_capabilities(&host), "sa_5");
        let runtime = worker_runtime(&format!("{REQUIRED}{switch}"), &host, &hooks);
        match runtime.hook_bus().emit(&call()).await {
            HookResult::Block { reason } => assert!(
                reason.starts_with("Refused: required guard 'guard' is not loaded ("),
                "{switch:?}: {reason}"
            ),
            other => panic!("{switch:?}: expected the refusal, got {other:?}"),
        }
        assert!(seen.lock().unwrap().is_empty(), "{switch:?}");
    }
}

/// A route captured from the stream's tool-call scope wins over the tool
/// context's own bus, and a worker started in the scope of a bus whose
/// guard later goes away is refused from then on.
#[tokio::test]
async fn the_streams_caller_scope_is_the_route_and_a_later_unload_refuses() {
    let (host, seen) = caller(HookResult::Continue, true).await;
    let (other, other_seen) = caller(HookResult::Continue, true).await;
    let hooks = CALLER_HOOKS
        .scope(
            super::CallerHooks {
                bus: Arc::clone(&host),
                auto_approve: false,
            },
            async { WorkerHooks::of(&caller_capabilities(&other), "sa_6") },
        )
        .await;
    let runtime = worker_runtime(REQUIRED, &host, &hooks);
    assert_eq!(runtime.hook_bus().emit(&call()).await, HookResult::Continue);
    assert_eq!((seen.lock().unwrap().len(), other_seen.lock().unwrap().len()), (1, 0));

    host.required_guards().mark_not_loaded("guard", NotLoaded::Unloaded);
    assert!(matches!(
        runtime.hook_bus().emit(&call()).await,
        HookResult::Block { reason } if reason.contains("required guard 'guard' is not loaded")
    ));
    assert_eq!(seen.lock().unwrap().len(), 1, "the unloaded guard saw nothing more");
}

/// A worker with no route to a caller (no stream scope, no tool-context
/// bus) while a guard is required stays on a bus that holds no guard, and
/// refuses every tool call: it never runs unguarded.
#[tokio::test]
async fn a_worker_with_no_route_to_the_caller_is_refused_while_a_guard_is_required() {
    let (host, seen) = caller(HookResult::Continue, true).await;
    let hooks = WorkerHooks::of(&crate::tools::ToolCapabilities::default(), "sa_7");
    // The worker's own bus: a detached one on the host's gate (in a running
    // engine every bus made with `HookBus::new` shares the process's gate,
    // so `subagent_transform_hooks = off`'s detached bus is on it too).
    let config = agent_core::config::load_config_from_str(REQUIRED);
    let mut runtime = crate::Runtime::new_headless();
    runtime.set_hook_bus(Arc::new(
        HookBus::new().with_required_guards(Arc::clone(host.required_guards())),
    ));
    apply_subagent_runtime_policy(&mut runtime, &config, None, None, None, &Default::default());
    hooks.apply(&mut runtime, &config);
    assert!(!runtime.has_worker_hook_route());
    match runtime.hook_bus().emit(&call()).await {
        HookResult::Block { reason } => {
            assert!(reason.contains("required guard 'guard' is not subscribed"), "{reason}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(seen.lock().unwrap().is_empty());
}

/// With no guard required nothing is routed: the worker keeps its transform
/// view (or the detached bus), as before.
#[tokio::test]
async fn without_a_required_guard_a_worker_is_left_as_it_was() {
    let host = Arc::new(HookBus::new());
    let hooks = WorkerHooks::of(&caller_capabilities(&host), "sa_8");
    let on = worker_runtime("", &host, &hooks);
    assert!(!on.has_worker_hook_route() && !on.hook_bus().is_worker_route());
    assert!(on.hook_bus().is_worker_view());
    let off = worker_runtime(TRANSFORMS_OFF, &host, &hooks);
    assert!(!off.has_worker_hook_route() && !off.hook_bus().is_worker_route());
    assert!(!off.hook_bus().is_worker_view());
}

/// Every spawn path captures the route in the caller's task before its
/// worker thread starts, and applies it after the runtime policy (which is
/// where `subagent_transform_hooks = off` swaps the bus) and before the
/// stream.
#[test]
fn every_spawn_path_routes_the_worker_after_the_policy_and_before_streaming() {
    for (name, source) in [
        ("oneshot", include_str!("oneshot.rs")),
        ("start", include_str!("start.rs")),
        ("resume", include_str!("resume.rs")),
    ] {
        let production = source.split("#[cfg(test)]\nmod tests").next().unwrap();
        assert_eq!(production.matches("WorkerHooks::of(").count(), 1, "{name}");
        assert_eq!(production.matches("worker_hooks.apply(").count(), 1, "{name}");
        let captured = production.find("WorkerHooks::of(").unwrap();
        let spawned = production.find("std::thread::").unwrap();
        let policy = production.find("super::apply_subagent_runtime_policy(").unwrap();
        let applied = production.find("worker_hooks.apply(").unwrap();
        let stream = production.find("runtime.run_stream").unwrap();
        assert!(captured < spawned, "{name}: captured on the caller's task");
        assert!(policy < applied && applied < stream, "{name}: applied after the policy");
    }
}
