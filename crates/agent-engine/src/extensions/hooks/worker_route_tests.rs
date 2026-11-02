//! A worker's (subagent's) hook bus ([`HookBus::worker_route`]): its
//! `before_tool_call` runs the caller's chain, on the caller's runtime,
//! stamped with the caller's conversation and the worker's id; nothing else
//! runs; and a chain that cannot run blocks the call.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use super::events::{HookEvent, HookKind, HookResult};
use super::HookBus;
use crate::extensions::permissions::{Permission, PermissionSet};

/// What a handler saw: the event and the name of the thread it ran on.
type Seen = Arc<Mutex<Vec<(HookEvent, Option<String>)>>>;

struct Recording {
    id: &'static str,
    seen: Seen,
    result: HookResult,
}

#[async_trait]
impl crate::extensions::runtime::ExtensionHandler for Recording {
    fn id(&self) -> &str {
        self.id
    }

    async fn handle(&self, event: &HookEvent) -> HookResult {
        let thread = std::thread::current().name().map(str::to_string);
        self.seen.lock().unwrap().push((event.clone(), thread));
        self.result.clone()
    }

    async fn shutdown(&self) {}
}

fn intercept() -> PermissionSet {
    let mut set = PermissionSet::new();
    set.grant(Permission::ToolsIntercept);
    set
}

fn confirm(message: &str) -> HookResult {
    HookResult::Confirm {
        message: message.into(),
        sources: vec![],
        generations: vec![],
        tool: None,
        modified_input: None,
        modified_by: vec![],
    }
}

/// A caller bus with one `kind` handler "guard" returning `result`.
async fn caller_bus(kind: HookKind, result: HookResult) -> (Arc<HookBus>, Seen) {
    let bus = Arc::new(HookBus::new());
    let seen: Seen = Arc::default();
    bus.subscribe(
        kind,
        Arc::new(Recording {
            id: "guard",
            seen: Arc::clone(&seen),
            result,
        }),
        None,
        None,
        intercept(),
    )
    .await
    .unwrap();
    (bus, seen)
}

/// Emit `event` on `bus` the way a worker does: from its own OS thread, on
/// its own current-thread runtime (the thread is named "worker-thread").
async fn emit_from_a_worker_thread(bus: Arc<HookBus>, event: HookEvent) -> HookResult {
    tokio::task::spawn_blocking(move || {
        std::thread::Builder::new()
            .name("worker-thread".into())
            .spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async move { bus.emit(&event).await })
            })
            .unwrap()
            .join()
            .unwrap()
    })
    .await
    .unwrap()
}

fn worker_bash_call() -> HookEvent {
    let mut event = HookEvent::before_tool_call("bash", json!({"command": "echo hi"}));
    event.tool_call_id = Some("toolu_w1".into());
    event
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_tool_call_runs_the_callers_chain_with_the_caller_and_worker_ids() {
    let (caller, seen) = caller_bus(HookKind::BeforeToolCall, confirm("check this")).await;
    // A reload bumps the guard's load generation on the caller's bus; the
    // worker's Confirm must carry the caller's generation (the latch key).
    caller.unsubscribe_all("guard").await;
    caller
        .subscribe(
            HookKind::BeforeToolCall,
            Arc::new(Recording {
                id: "guard",
                seen: Arc::clone(&seen),
                result: confirm("check this"),
            }),
            None,
            None,
            intercept(),
        )
        .await
        .unwrap();
    let generation = caller.extension_generation("guard");
    assert_eq!(generation, 1);

    let worker = Arc::new(HookBus::worker_route(
        Arc::clone(&caller),
        tokio::runtime::Handle::current(),
        Some("sess-parent".into()),
        "sa_7".into(),
    ));
    assert!(worker.is_worker_route() && !caller.is_worker_route());

    let result = emit_from_a_worker_thread(Arc::clone(&worker), worker_bash_call()).await;
    match result {
        HookResult::Confirm {
            message,
            sources,
            generations,
            ..
        } => {
            assert_eq!(message, "check this");
            assert_eq!(sources, vec!["guard".to_string()]);
            assert_eq!(generations, vec![generation], "the caller bus's generation");
        }
        other => panic!("expected the caller chain's Confirm, got {other:?}"),
    }

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "the caller's guard ran once");
    let (event, thread) = &seen[0];
    assert_eq!(event.tool_name.as_deref(), Some("bash"));
    assert_eq!(event.tool_call_id.as_deref(), Some("toolu_w1"));
    assert_eq!(event.session_id.as_deref(), Some("sess-parent"));
    assert_eq!(event.subagent_id.as_deref(), Some("sa_7"));
    assert_ne!(
        thread.as_deref(),
        Some("worker-thread"),
        "the caller's handlers run on the caller's runtime, not the worker's"
    );
    let wire = serde_json::to_value(event).unwrap();
    assert_eq!(wire["subagent_id"], "sa_7");
    assert_eq!(wire["session_id"], "sess-parent");
}

/// A foreground call's event has no `subagent_id` key at all.
#[test]
fn a_foreground_event_has_no_subagent_id_key() {
    let wire = serde_json::to_value(worker_bash_call()).unwrap();
    assert!(!wire.as_object().unwrap().contains_key("subagent_id"), "{wire}");
}

/// The caller's Block stops the worker's call; the caller's Continue lets
/// it through.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_tool_call_takes_the_callers_block_and_continue() {
    for (result, blocked) in [
        (HookResult::Block { reason: "guard says no".into() }, true),
        (HookResult::Continue, false),
    ] {
        let (caller, seen) = caller_bus(HookKind::BeforeToolCall, result).await;
        let worker = Arc::new(HookBus::worker_route(
            caller,
            tokio::runtime::Handle::current(),
            None,
            "sa_8".into(),
        ));
        let out = emit_from_a_worker_thread(worker, worker_bash_call()).await;
        if blocked {
            assert_eq!(out, HookResult::Block { reason: "guard says no".into() });
        } else {
            assert_eq!(out, HookResult::Continue);
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0.session_id, None, "a caller with no conversation");
        assert_eq!(seen[0].0.subagent_id.as_deref(), Some("sa_8"));
    }
}

/// Only `before_tool_call` is routed: a worker's other hook kinds find no
/// handlers, as on an empty bus.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_routes_no_other_hook_kind() {
    let (caller, seen) = caller_bus(
        HookKind::AfterToolCall,
        HookResult::Replace { output: "rewritten".into() },
    )
    .await;
    let worker = Arc::new(HookBus::worker_route(
        caller,
        tokio::runtime::Handle::current(),
        Some("sess-parent".into()),
        "sa_9".into(),
    ));
    let after = HookEvent::after_tool_call("bash", json!({}), "out".into());
    assert_eq!(emit_from_a_worker_thread(worker, after).await, HookResult::Continue);
    assert!(seen.lock().unwrap().is_empty());
}

/// The caller's runtime is gone: the chain cannot run, so the call is
/// blocked, never let through unchecked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_tool_call_is_blocked_when_the_callers_runtime_is_gone() {
    let (caller, seen) = caller_bus(HookKind::BeforeToolCall, HookResult::Continue).await;
    let gone = tokio::task::spawn_blocking(|| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let handle = rt.handle().clone();
        rt.shutdown_timeout(std::time::Duration::from_secs(5));
        handle
    })
    .await
    .unwrap();
    let worker = Arc::new(HookBus::worker_route(caller, gone, None, "sa_10".into()));
    match emit_from_a_worker_thread(worker, worker_bash_call()).await {
        HookResult::Block { reason } => {
            assert!(reason.contains("could not run"), "{reason}");
        }
        other => panic!("expected Block, got {other:?}"),
    }
    assert!(seen.lock().unwrap().is_empty());
}
