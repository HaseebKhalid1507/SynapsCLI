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
use crate::extensions::required_guards::{NotLoaded, RequiredGuards};

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

    async fn guard_down(&self) -> Option<String> {
        None
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
    HookEvent::before_tool_call("bash", json!({"command": "echo hi"}))
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
        (HookResult::block("guard says no"), true),
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
            assert_eq!(out, HookResult::block("guard says no"));
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

// ---- Required guards on a worker's bus ----

/// A caller bus gated on required guard `guard`, which holds a live
/// `guard` handler answering `result`. `loaded`: the gate is told it is
/// loaded (otherwise it is not loaded yet).
async fn gated_caller(result: HookResult, loaded: bool) -> (Arc<HookBus>, Arc<RequiredGuards>, Seen) {
    let (bus, gate, seen, _handler) = gated_caller_with_handler(result, loaded).await;
    (bus, gate, seen)
}

/// [`gated_caller`], also returning the subscribed handler.
async fn gated_caller_with_handler(
    result: HookResult,
    loaded: bool,
) -> (
    Arc<HookBus>,
    Arc<RequiredGuards>,
    Seen,
    Arc<dyn crate::extensions::runtime::ExtensionHandler>,
) {
    let gate = Arc::new(RequiredGuards::new(&["guard".to_string()]));
    let bus = Arc::new(HookBus::new().with_required_guards(Arc::clone(&gate)));
    let seen: Seen = Arc::default();
    let handler: Arc<dyn crate::extensions::runtime::ExtensionHandler> = Arc::new(Recording {
        id: "guard",
        seen: Arc::clone(&seen),
        result,
    });
    if loaded {
        gate.mark_loaded("guard", Arc::clone(&handler));
    }
    bus.subscribe(HookKind::BeforeToolCall, Arc::clone(&handler), None, None, intercept())
        .await
        .unwrap();
    (bus, gate, seen, handler)
}

fn route_of(caller: &Arc<HookBus>) -> Arc<HookBus> {
    Arc::new(HookBus::worker_route(
        Arc::clone(caller),
        tokio::runtime::Handle::current(),
        Some("sess-parent".into()),
        "sa_11".into(),
    ))
}

const NOT_LOADED: &str = "Refused: required guard 'guard' is not loaded (";

/// A worker's tool call is refused while a required guard is not loaded,
/// whatever the caller's chain would say, and the guard never sees it; once
/// the guard is loaded the call reaches it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_tool_call_is_refused_while_a_required_guard_is_not_loaded() {
    let (caller, gate, seen, handler) =
        gated_caller_with_handler(HookResult::Continue, false).await;
    let worker = route_of(&caller);
    match emit_from_a_worker_thread(Arc::clone(&worker), worker_bash_call()).await {
        HookResult::Block { reason } => assert!(reason.starts_with(NOT_LOADED), "{reason}"),
        other => panic!("expected the required-guard refusal, got {other:?}"),
    }
    assert!(seen.lock().unwrap().is_empty(), "the guard never saw the call");

    // The guard loads (the subscribed instance, marked live): the call goes
    // through it.
    gate.mark_loaded("guard", handler);
    assert_eq!(
        emit_from_a_worker_thread(Arc::clone(&worker), worker_bash_call()).await,
        HookResult::Continue
    );
    assert_eq!(seen.lock().unwrap().len(), 1, "the guard saw the call");

    // It is reloaded (unloaded: not loaded again until marked): refused.
    gate.mark_not_loaded("guard", NotLoaded::Unloaded);
    match emit_from_a_worker_thread(worker, worker_bash_call()).await {
        HookResult::Block { reason } => assert!(reason.starts_with(NOT_LOADED), "{reason}"),
        other => panic!("expected the required-guard refusal, got {other:?}"),
    }
    assert_eq!(seen.lock().unwrap().len(), 1, "and the guard did not see it");
}

/// A required guard that is loaded and live sees the worker's call, and
/// its Block ends it; its Continue lets it through.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_loaded_required_guard_decides_a_worker_tool_call() {
    for (result, blocked) in [
        (HookResult::block("guard says no"), true),
        (HookResult::Continue, false),
    ] {
        let (caller, _gate, seen) = gated_caller(result, true).await;
        let out = emit_from_a_worker_thread(route_of(&caller), worker_bash_call()).await;
        if blocked {
            assert_eq!(out, HookResult::block("guard says no"));
        } else {
            assert_eq!(out, HookResult::Continue);
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "the guard saw the worker's call");
        assert_eq!(seen[0].0.subagent_id.as_deref(), Some("sa_11"));
    }
}

/// A worker's worker runs the chain of the session that started the first
/// one, not the empty handler list of the route in between.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_workers_worker_runs_the_original_callers_chain() {
    let (caller, _gate, seen) = gated_caller(HookResult::block("guard says no"), true).await;
    let worker = route_of(&caller);
    let nested = Arc::new(HookBus::worker_route(
        Arc::clone(&worker),
        tokio::runtime::Handle::current(),
        Some("sess-worker".into()),
        "sa_12".into(),
    ));
    assert!(Arc::ptr_eq(nested.required_guards(), caller.required_guards()));
    let out = emit_from_a_worker_thread(nested, worker_bash_call()).await;
    assert_eq!(out, HookResult::block("guard says no"));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0.subagent_id.as_deref(), Some("sa_12"));
    assert_eq!(
        seen[0].0.session_id.as_deref(),
        Some("sess-parent"),
        "the conversation that started the first worker"
    );
}

/// The detached bus `subagent_transform_hooks = false` leaves a worker
/// holds no guard: while a guard is required it refuses every call (no
/// route to the guard is no allow); with none required it is today's
/// empty bus.
#[tokio::test]
async fn a_bus_with_no_route_to_a_required_guard_refuses_a_worker_tool_call() {
    let gate = Arc::new(RequiredGuards::new(&["guard".to_string()]));
    let handler: Arc<dyn crate::extensions::runtime::ExtensionHandler> = Arc::new(Recording {
        id: "guard",
        seen: Arc::default(),
        result: HookResult::Continue,
    });
    gate.mark_loaded("guard", handler);
    let detached = HookBus::new().with_required_guards(Arc::clone(&gate));
    match detached.emit(&worker_bash_call()).await {
        HookResult::Block { reason } => {
            assert!(reason.contains("required guard 'guard' is not subscribed"), "{reason}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(HookBus::new().emit(&worker_bash_call()).await, HookResult::Continue);
}

/// A worker's transform view of a host whose guard is required refuses a
/// `before_tool_call` the same way: the view holds no guard.
#[tokio::test]
async fn a_transform_view_with_a_required_guard_configured_refuses_a_tool_call() {
    let (host, _gate, seen) = gated_caller(HookResult::Continue, true).await;
    let view = HookBus::worker_transforms(&host);
    assert!(Arc::ptr_eq(view.required_guards(), host.required_guards()));
    match view.emit(&worker_bash_call()).await {
        HookResult::Block { reason } => {
            assert!(reason.contains("required guard 'guard' is not subscribed"), "{reason}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(seen.lock().unwrap().is_empty());
}

/// `with_worker_transforms_of` layers the host's output transforms on a
/// route, and a plain bus (the detached one) contributes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(synaps_base_dir)]
async fn a_route_with_the_hosts_transform_view_runs_both() {
    // A `Replace` is audited under the base dir.
    let _base = crate::test_env::BaseDirGuard::new();
    let host = Arc::new(HookBus::new());
    let transform_seen: Seen = Arc::default();
    let mut permissions = intercept();
    permissions.grant(Permission::ToolsTransformOutput);
    host.subscribe(
        HookKind::AfterToolCall,
        Arc::new(Recording {
            id: "redactor",
            seen: Arc::clone(&transform_seen),
            result: HookResult::Replace { output: "redacted".into() },
        }),
        None,
        None,
        permissions,
    )
    .await
    .unwrap();
    let (caller, _gate, _seen) = gated_caller(HookResult::block("guard says no"), true).await;
    let after = HookEvent::after_tool_call("bash", json!({}), "secret".into());

    let view = HookBus::worker_transforms(&host);
    let both = Arc::new(
        HookBus::worker_route(
            Arc::clone(&caller),
            tokio::runtime::Handle::current(),
            None,
            "sa_13".into(),
        )
        .with_worker_transforms_of(&view),
    );
    assert!(both.is_worker_route() && both.is_worker_view());
    assert_eq!(
        emit_from_a_worker_thread(Arc::clone(&both), after.clone()).await,
        HookResult::Replace { output: "redacted".into() }
    );
    assert_eq!(
        emit_from_a_worker_thread(both, worker_bash_call()).await,
        HookResult::block("guard says no"),
        "and the call still goes to the caller's guard"
    );

    // A plain bus contributes no host to read from.
    let plain = Arc::new(
        HookBus::worker_route(caller, tokio::runtime::Handle::current(), None, "sa_14".into())
            .with_worker_transforms_of(&HookBus::new()),
    );
    assert!(!plain.is_worker_view());
    assert_eq!(emit_from_a_worker_thread(plain, after).await, HookResult::Continue);
}
