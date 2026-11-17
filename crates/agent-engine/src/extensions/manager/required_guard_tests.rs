//! Required guards through the extension manager: a guard named in the
//! config's `required_guards` that is not loaded, for any cause, refuses
//! every tool call, and loading it again clears the refusal. Plus the
//! manifest's `on_failure` for a guard that is loaded and live.
//!
//! Each pin's undo row: break the control, see the pin red, restore it
//! byte for byte, see it green.

use super::*;
use crate::extensions::hooks::events::{HookEvent, HookKind, HookResult};
use crate::extensions::required_guards::RequiredGuards;
use crate::extensions::runtime::process::guard_fail_closed_tests::{
    blocked, init, signal, timed_out, GUARD, NO,
};
use crate::test_env::{BaseDirGuard, EnvVarGuard};
use serde_json::json;
use std::time::{Duration, Instant};

const ALLOW: &str = r#"{"action":"continue"}"#;

fn bash(command: &str) -> HookEvent {
    HookEvent::before_tool_call("bash", json!({"command": command}))
}

fn call() -> HookEvent {
    bash("rm -rf /")
}

fn tool_call(tool: &str) -> HookEvent {
    HookEvent::before_tool_call(tool, json!({}))
}

fn guard_hook(on_failure: Option<&str>) -> Value {
    let mut hook = json!({"hook": "before_tool_call"});
    if let Some(value) = on_failure {
        hook["on_failure"] = json!(value);
    }
    hook
}

/// The `extension` block of a guard running `command` with `args`.
fn guard_json(command: &str, args: Value, hooks: &[Value]) -> Value {
    json!({
        "runtime": "process",
        "command": command,
        "args": args,
        "permissions": ["tools.intercept"],
        "hooks": hooks
    })
}

fn guard_manifest(command: &str, args: Value, hooks: &[Value]) -> ExtensionManifest {
    serde_json::from_value(guard_json(command, args, hooks)).unwrap()
}

/// Install plugin `id` under `base` with this `extension` block.
fn install(base: &std::path::Path, id: &str, extension: Value) {
    let dir = base.join("plugins").join(id).join(".synaps-plugin");
    std::fs::create_dir_all(&dir).unwrap();
    let plugin = json!({"name": id, "extension": extension});
    std::fs::write(dir.join("plugin.json"), plugin.to_string()).unwrap();
}

// ---- `on_failure` for a guard that is loaded and live ----

/// Load a guard, SIGSTOP it, and emit one `before_tool_call`: the
/// verdict and how long it took.
async fn stopped_guard_verdict(on_failure: Option<&str>) -> (HookResult, Duration) {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let pid_arg = pidfile.display().to_string();
    let args = json!(["-c", GUARD, "guard", NO, "", init(), pid_arg]);
    let manifest = guard_manifest("/bin/sh", args, &[guard_hook(on_failure)]);
    let bus = Arc::new(HookBus::new());
    let mut mgr = ExtensionManager::new(bus.clone());
    mgr.load("example-guard", &manifest).await.unwrap();
    assert_eq!(bus.emit(&call()).await, HookResult::block("guard says no"));
    let pid = std::fs::read_to_string(&pidfile).unwrap();
    let pid = pid.trim();
    signal(pid, "-STOP");
    let started = Instant::now();
    let verdict = bus.emit(&call()).await;
    let waited = started.elapsed();
    signal(pid, "-KILL");
    mgr.shutdown_all().await;
    (verdict, waited)
}

/// A guard whose manifest says `on_failure: "block"` and that is
/// SIGSTOPped blocks the call within the hook timeout.
#[tokio::test]
async fn a_stopped_guard_from_a_block_manifest_blocks_the_call() {
    let (verdict, waited) = stopped_guard_verdict(Some("block")).await;
    assert_eq!(verdict, timed_out("example-guard"));
    assert!(waited < Duration::from_secs(6), "within the hook timeout: {waited:?}");
}

/// Without `on_failure` the same stopped guard is skipped, as before.
#[tokio::test]
async fn a_stopped_guard_from_a_default_manifest_still_continues() {
    let (verdict, _) = stopped_guard_verdict(None).await;
    assert_eq!(verdict, HookResult::Continue);
}

/// A deferred guard that started and answered, then died and cannot start
/// again, blocks the call when failing closed and continues (as before)
/// when failing open.
#[tokio::test]
async fn a_crashed_deferred_guard_that_cannot_respawn_blocks_when_failing_closed() {
    for (on_failure, expected) in [
        (Some("block"), blocked("example-guard")),
        (None, HookResult::Continue),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("started").display().to_string();
        let pidfile = dir.path().join("pid");
        let pid_arg = pidfile.display().to_string();
        let args = json!(["-c", GUARD, "guard", NO, marker, init(), pid_arg]);
        let mut manifest = guard_json("/bin/sh", args, &[guard_hook(on_failure)]);
        manifest["deferred"] = json!({"lifecycle": "hook"});
        let manifest: ExtensionManifest = serde_json::from_value(manifest).unwrap();
        let bus = Arc::new(HookBus::new());
        let mut mgr = ExtensionManager::new(bus.clone());
        mgr.set_progressive_deferral(true);
        mgr.load("example-guard", &manifest).await.unwrap();
        assert!(mgr.is_deferred("example-guard"));
        assert_eq!(bus.emit(&call()).await, HookResult::block("guard says no"));
        signal(std::fs::read_to_string(&pidfile).unwrap().trim(), "-KILL");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let verdict = tokio::time::timeout(Duration::from_secs(4), bus.emit(&call()))
            .await
            .expect("not the hook timeout");
        assert_eq!(verdict, expected, "on_failure {on_failure:?}");
        mgr.shutdown_all().await;
    }
}

/// A deferred guard that cannot start blocks when failing closed and
/// continues (as before) when failing open.
#[tokio::test]
async fn a_deferred_guard_that_cannot_start_blocks_when_failing_closed() {
    for (on_failure, expected) in [
        (Some("block"), blocked("example-guard")),
        (None, HookResult::Continue),
    ] {
        let manifest: ExtensionManifest = serde_json::from_value(json!({
            "runtime": "process",
            "command": "/bin/false",
            "permissions": ["tools.intercept"],
            "hooks": [guard_hook(on_failure)],
            "deferred": {"lifecycle": "hook"}
        }))
        .unwrap();
        let bus = Arc::new(HookBus::new());
        let mut mgr = ExtensionManager::new(bus.clone());
        mgr.set_progressive_deferral(true);
        mgr.load("example-guard", &manifest).await.unwrap();
        assert!(mgr.is_deferred("example-guard"));
        let verdict = tokio::time::timeout(Duration::from_secs(10), bus.emit(&call()))
            .await
            .expect("bounded");
        assert_eq!(verdict, expected, "on_failure {on_failure:?}");
        mgr.shutdown_all().await;
    }
}

// ---- Required guards ----

/// What every tool call gets while required guard `example-guard` is not
/// loaded for `cause` (`code: detail`). Spelled out here, not built with the
/// code's own `refusal_text`, so a changed text turns these pins red.
fn refused(cause: &str) -> HookResult {
    HookResult::block(format!(
        "Refused: required guard 'example-guard' is not loaded ({cause}). No tool call runs while a required guard is not loaded; restore the guard, then reload it."
    ))
}

/// The live guard's own answer: proof the guard itself answered.
fn guard_says_no() -> HookResult {
    HookResult::block("guard says no")
}

/// A live guard's `extension` block, failing OPEN (no `on_failure`):
/// answers every call with `NO` and writes its pid to `<dir>/pid`. It starts
/// once per marker `<dir>/started`: a respawn fails until [`restartable`].
/// (No arg is `""`: discovery resolves an arg naming a path in the plugin
/// folder, and `""` names the folder itself.)
fn example_guard(dir: &std::path::Path) -> Value {
    let marker = dir.join("started").display().to_string();
    let pidfile = dir.join("pid").display().to_string();
    let args = json!(["-c", GUARD, "guard", NO, marker, init(), pidfile]);
    guard_json("/bin/sh", args, &[guard_hook(None)])
}

/// [`example_guard`] in `dir` can start again.
fn restartable(dir: &std::path::Path) {
    std::fs::remove_file(dir.join("started")).unwrap();
}

fn write_config(base: &std::path::Path, text: &str) {
    std::fs::write(base.join("config"), text).unwrap();
}

/// A manager on a bus gated as `EngineHost::boot` gates the host's bus: the
/// active config's `required_guards`, pending discovery.
fn booted() -> (Arc<HookBus>, ExtensionManager) {
    let gate = Arc::new(RequiredGuards::none());
    gate.host_booting(&crate::config::load_config().required_guards, false);
    let bus = Arc::new(HookBus::new().with_required_guards(gate));
    let mgr = ExtensionManager::new(bus.clone());
    (bus, mgr)
}

/// A required guard listed in `disabled_plugins` refuses every tool call,
/// naming the guard and the cause; a reload while it is still disabled keeps
/// refusing; enabling it and reloading clears it.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn a_disabled_required_guard_refuses_every_tool_call() {
    let base = BaseDirGuard::new();
    let _no_project = EnvVarGuard::set("SYNAPS_DISABLE_PROJECT_PLUGINS", "1");
    let dir = tempfile::tempdir().unwrap();
    install(base.path(), "example-guard", example_guard(dir.path()));
    write_config(
        base.path(),
        "required_guards = example-guard\ndisabled_plugins = example-guard\n",
    );
    let (bus, mut mgr) = booted();
    let pending = refused("pending: extension discovery has not loaded it yet");
    assert_eq!(bus.emit(&call()).await, pending);

    mgr.discover_and_load().await;
    let disabled = refused("disabled: 'example-guard' is listed in disabled_plugins");
    assert_eq!(bus.emit(&call()).await, disabled);
    assert_eq!(bus.emit(&tool_call("read")).await, disabled);
    assert!(mgr.reload_from_disk("example-guard").await.is_err());
    assert_eq!(bus.emit(&tool_call("write")).await, disabled);

    write_config(base.path(), "required_guards = example-guard\n");
    mgr.reload_from_disk("example-guard").await.unwrap();
    assert_eq!(bus.emit(&call()).await, guard_says_no());
    mgr.shutdown_all().await;
}

/// No `example-guard` folder in any plugin root refuses every tool call,
/// naming the roots; installing it and reloading clears it.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn an_absent_required_guard_refuses_every_tool_call() {
    let base = BaseDirGuard::new();
    let _no_project = EnvVarGuard::set("SYNAPS_DISABLE_PROJECT_PLUGINS", "1");
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(base.path().join("plugins")).unwrap();
    write_config(base.path(), "required_guards = example-guard\n");
    let (bus, mut mgr) = booted();
    mgr.discover_and_load().await;
    let roots = base.path().join("plugins").display().to_string();
    let absent = refused(&format!("absent: no plugin folder named 'example-guard' in {roots}"));
    assert_eq!(bus.emit(&call()).await, absent);
    assert_eq!(bus.emit(&tool_call("read")).await, absent);
    assert!(mgr.reload_from_disk("example-guard").await.is_err());
    assert_eq!(bus.emit(&call()).await, absent);

    install(base.path(), "example-guard", example_guard(dir.path()));
    mgr.reload_from_disk("example-guard").await.unwrap();
    assert_eq!(bus.emit(&call()).await, guard_says_no());
    mgr.shutdown_all().await;
}

/// `example-guard` renamed to `example-guard.disabled` (a name discovery
/// skips) refuses every tool call, naming the folder; renaming it back and
/// reloading clears it.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn a_skip_renamed_required_guard_refuses_every_tool_call() {
    let base = BaseDirGuard::new();
    let _no_project = EnvVarGuard::set("SYNAPS_DISABLE_PROJECT_PLUGINS", "1");
    let dir = tempfile::tempdir().unwrap();
    install(base.path(), "example-guard", example_guard(dir.path()));
    let plugins = base.path().join("plugins");
    std::fs::rename(plugins.join("example-guard"), plugins.join("example-guard.disabled")).unwrap();
    write_config(base.path(), "required_guards = example-guard\n");
    let (bus, mut mgr) = booted();
    mgr.discover_and_load().await;
    let renamed = refused(
        "skip-renamed: its folder is named 'example-guard.disabled', a name discovery skips",
    );
    assert_eq!(bus.emit(&call()).await, renamed);
    assert_eq!(bus.emit(&tool_call("read")).await, renamed);
    assert!(mgr.reload_from_disk("example-guard").await.is_err());
    assert_eq!(bus.emit(&call()).await, renamed);

    std::fs::rename(plugins.join("example-guard.disabled"), plugins.join("example-guard")).unwrap();
    mgr.reload_from_disk("example-guard").await.unwrap();
    assert_eq!(bus.emit(&call()).await, guard_says_no());
    mgr.shutdown_all().await;
}

/// A `plugin.json` that is not JSON refuses every tool call with the parse
/// error; fixing it and reloading clears it.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn a_required_guard_with_a_malformed_manifest_refuses_every_tool_call() {
    let base = BaseDirGuard::new();
    let _no_project = EnvVarGuard::set("SYNAPS_DISABLE_PROJECT_PLUGINS", "1");
    let dir = tempfile::tempdir().unwrap();
    install(base.path(), "example-guard", example_guard(dir.path()));
    let manifest = base.path().join("plugins/example-guard/.synaps-plugin/plugin.json");
    let good = std::fs::read(&manifest).unwrap();
    std::fs::write(&manifest, br#"{"name": "example-guard", "extension": "#).unwrap();
    write_config(base.path(), "required_guards = example-guard\n");
    let (bus, mut mgr) = booted();
    mgr.discover_and_load().await;
    for verdict in [bus.emit(&call()).await, bus.emit(&tool_call("read")).await] {
        let HookResult::Block { reason } = verdict else {
            panic!("not refused: {verdict:?}");
        };
        assert!(
            reason.starts_with(
                "Refused: required guard 'example-guard' is not loaded (bad-manifest: Invalid plugin manifest JSON: "
            ),
            "{reason}"
        );
    }
    assert!(mgr.reload_from_disk("example-guard").await.is_err());
    assert!(
        matches!(bus.emit(&call()).await, HookResult::Block { reason } if reason.contains("(bad-manifest: "))
    );

    std::fs::write(&manifest, good).unwrap();
    mgr.reload_from_disk("example-guard").await.unwrap();
    assert_eq!(bus.emit(&call()).await, guard_says_no());
    mgr.shutdown_all().await;
}

/// A guard whose process cannot start (its command is not there) refuses
/// every tool call with the start error; fixing the manifest and reloading
/// clears it.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn a_required_guard_that_fails_to_start_refuses_every_tool_call() {
    let base = BaseDirGuard::new();
    let _no_project = EnvVarGuard::set("SYNAPS_DISABLE_PROJECT_PLUGINS", "1");
    let dir = tempfile::tempdir().unwrap();
    let broken = guard_json("/nonexistent/example-guard-binary", json!([]), &[guard_hook(None)]);
    install(base.path(), "example-guard", broken);
    write_config(base.path(), "required_guards = example-guard\n");
    let (bus, mut mgr) = booted();
    mgr.discover_and_load().await;
    for verdict in [bus.emit(&call()).await, bus.emit(&tool_call("read")).await] {
        let HookResult::Block { reason } = verdict else {
            panic!("not refused: {verdict:?}");
        };
        assert!(
            reason.starts_with("Refused: required guard 'example-guard' is not loaded (failed-start: "),
            "{reason}"
        );
    }
    assert!(mgr.reload_from_disk("example-guard").await.is_err());
    assert!(
        matches!(bus.emit(&call()).await, HookResult::Block { reason } if reason.contains("(failed-start: "))
    );

    install(base.path(), "example-guard", example_guard(dir.path()));
    mgr.reload_from_disk("example-guard").await.unwrap();
    assert_eq!(bus.emit(&call()).await, guard_says_no());
    mgr.shutdown_all().await;
}

/// A loaded required guard that fails open (no `on_failure`) and cannot
/// respawn, killed mid-session, refuses every tool call (without the gate the
/// call would run unguarded); its health row says so; a reload restarts it and
/// clears it.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn a_required_guard_killed_mid_session_refuses_every_tool_call() {
    let base = BaseDirGuard::new();
    let _no_project = EnvVarGuard::set("SYNAPS_DISABLE_PROJECT_PLUGINS", "1");
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    install(base.path(), "example-guard", example_guard(dir.path()));
    write_config(base.path(), "required_guards = example-guard\n");
    let (bus, mut mgr) = booted();
    mgr.discover_and_load().await;
    assert_eq!(bus.emit(&call()).await, guard_says_no());
    let health = bus.required_guards().health().await;
    assert_eq!((health.len(), health[0].loaded), (1, true));

    signal(std::fs::read_to_string(&pidfile).unwrap().trim(), "-KILL");
    tokio::time::sleep(Duration::from_millis(300)).await;
    for event in [call(), tool_call("read"), call()] {
        let verdict = tokio::time::timeout(Duration::from_secs(4), bus.emit(&event))
            .await
            .expect("not the hook timeout");
        let HookResult::Block { reason } = verdict else {
            panic!("fail open: {verdict:?}");
        };
        assert!(
            reason.starts_with(
                "Refused: required guard 'example-guard' is not loaded (process-gone: its process "
            ),
            "{reason}"
        );
    }
    let health = bus.required_guards().health().await;
    assert!(!health[0].loaded);
    assert!(health[0].cause.as_deref().unwrap().starts_with("process-gone: its process "));

    restartable(dir.path());
    mgr.reload_from_disk("example-guard").await.unwrap();
    assert_eq!(bus.emit(&call()).await, guard_says_no());
    mgr.shutdown_all().await;
}

/// With no `required_guards` key, nothing changes: a disabled guard leaves
/// tool calls unguarded exactly as before, no refusal and no health row.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn without_required_guards_nothing_changes() {
    let base = BaseDirGuard::new();
    let _no_project = EnvVarGuard::set("SYNAPS_DISABLE_PROJECT_PLUGINS", "1");
    let dir = tempfile::tempdir().unwrap();
    install(base.path(), "example-guard", example_guard(dir.path()));
    write_config(base.path(), "disabled_plugins = example-guard\n");
    let (bus, mut mgr) = booted();
    assert!(bus.required_guards().is_empty());
    assert_eq!(bus.emit(&call()).await, HookResult::Continue);
    mgr.discover_and_load().await;
    assert_eq!(bus.emit(&call()).await, HookResult::Continue);
    assert!(bus.required_guards().health().await.is_empty());

    write_config(base.path(), "");
    mgr.reload_from_disk("example-guard").await.unwrap();
    assert_eq!(bus.emit(&call()).await, guard_says_no());
    mgr.shutdown_all().await;
    assert_eq!(bus.emit(&call()).await, HookResult::Continue);
}

/// The exact reason text, in full, and the health row's cause.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn the_refusal_names_the_guard_and_the_cause_exactly() {
    let base = BaseDirGuard::new();
    let _no_project = EnvVarGuard::set("SYNAPS_DISABLE_PROJECT_PLUGINS", "1");
    let dir = tempfile::tempdir().unwrap();
    install(base.path(), "example-guard", example_guard(dir.path()));
    write_config(
        base.path(),
        "required_guards = example-guard\ndisabled_plugins = example-guard\n",
    );
    let (bus, mut mgr) = booted();
    mgr.discover_and_load().await;
    assert_eq!(
        bus.emit(&call()).await,
        HookResult::block(
            "Refused: required guard 'example-guard' is not loaded (disabled: 'example-guard' is listed in disabled_plugins). No tool call runs while a required guard is not loaded; restore the guard, then reload it."
        )
    );
    let health = bus.required_guards().health().await;
    assert_eq!(
        crate::extensions::required_guards::status_lines(&health),
        ["required guard: example-guard NOT LOADED (disabled: 'example-guard' is listed in disabled_plugins)"]
    );
}

/// A required guard unloaded, or stopped with every extension, refuses every
/// tool call; a guard whose manifest subscribes no `before_tool_call` hook
/// does not count as loaded.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn an_unloaded_or_hookless_required_guard_refuses() {
    let base = BaseDirGuard::new();
    let _no_project = EnvVarGuard::set("SYNAPS_DISABLE_PROJECT_PLUGINS", "1");
    let dir = tempfile::tempdir().unwrap();
    install(base.path(), "example-guard", example_guard(dir.path()));
    write_config(base.path(), "required_guards = example-guard\n");
    let (bus, mut mgr) = booted();
    mgr.discover_and_load().await;
    assert_eq!(bus.emit(&call()).await, guard_says_no());
    mgr.unload("example-guard").await.unwrap();
    assert_eq!(bus.emit(&call()).await, refused("unloaded: it was unloaded"));
    restartable(dir.path());
    mgr.reload_from_disk("example-guard").await.unwrap();
    assert_eq!(bus.emit(&call()).await, guard_says_no());
    mgr.shutdown_all().await;
    assert_eq!(bus.emit(&call()).await, refused("unloaded: it was unloaded"));

    let args = json!(["-c", GUARD, "guard", NO, "", init()]);
    let hookless = guard_json("/bin/sh", args, &[json!({"hook": "after_tool_call"})]);
    let hookless: ExtensionManifest = serde_json::from_value(hookless).unwrap();
    let mut mgr = ExtensionManager::new(bus.clone());
    mgr.load("example-guard", &hookless).await.unwrap();
    let no_hook = refused("no-guard-hook: its manifest subscribes no before_tool_call hook");
    assert_eq!(bus.emit(&call()).await, no_hook);
    mgr.shutdown_all().await;
}

/// A required guard loads eagerly under progressive deferral (a deferred one
/// has no process to check), and every worker bus checks its host's (or
/// caller's) gate.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn a_required_guard_is_never_deferred_and_worker_buses_share_the_gate() {
    let _base = BaseDirGuard::new();
    let _no_project = EnvVarGuard::set("SYNAPS_DISABLE_PROJECT_PLUGINS", "1");
    let gate = Arc::new(RequiredGuards::none());
    gate.host_booting(&["example-guard".to_string()], false);
    let bus = Arc::new(HookBus::new().with_required_guards(Arc::clone(&gate)));
    let view = HookBus::worker_transforms(&bus);
    assert!(Arc::ptr_eq(view.required_guards(), &gate));
    let route = HookBus::worker_route(
        Arc::clone(&bus),
        tokio::runtime::Handle::current(),
        None,
        "w1".to_string(),
    );
    assert!(Arc::ptr_eq(route.required_guards(), &gate));

    let mut manifest = guard_json(
        "/bin/sh",
        json!(["-c", GUARD, "guard", NO, "", init()]),
        &[guard_hook(None)],
    );
    manifest["deferred"] = json!({"lifecycle": "hook"});
    let manifest: ExtensionManifest = serde_json::from_value(manifest).unwrap();
    let mut mgr = ExtensionManager::new(bus.clone());
    mgr.set_progressive_deferral(true);
    mgr.load("example-guard", &manifest).await.unwrap();
    assert!(!mgr.is_deferred("example-guard"));
    assert_eq!(bus.emit(&call()).await, guard_says_no());
    // A view holds no guard handler: it refuses rather than let a call
    // through unvetted, until a route to the caller's chain is layered on.
    let no_route = HookResult::block(
        "required guard 'example-guard' is not subscribed to before_tool_call on this \
         hook bus (no route to it), so it cannot vet this tool call",
    );
    assert_eq!(view.emit(&call()).await, no_route);
    assert_eq!(route.emit(&call()).await, guard_says_no());
    mgr.unload("example-guard").await.unwrap();
    assert_eq!(view.emit(&call()).await, refused("unloaded: it was unloaded"));
    assert_eq!(route.emit(&call()).await, refused("unloaded: it was unloaded"));
    mgr.shutdown_all().await;
}

/// A required guard that dies while a call is in its chain, answering
/// nothing (its silence reads as `Continue`), does not let that call run: the
/// gate is checked again after the chain.
#[tokio::test]
async fn a_required_guard_that_dies_during_the_chain_refuses_that_call() {
    struct Dying {
        down: std::sync::atomic::AtomicBool,
    }
    #[async_trait::async_trait]
    impl ExtensionHandler for Dying {
        fn id(&self) -> &str {
            "example-guard"
        }
        async fn guard_down(&self) -> Option<String> {
            let down = self.down.load(std::sync::atomic::Ordering::SeqCst);
            down.then(|| "its process exited (signal: 9)".to_string())
        }
        async fn handle(&self, _event: &HookEvent) -> HookResult {
            self.down.store(true, std::sync::atomic::Ordering::SeqCst);
            HookResult::Continue
        }
        async fn shutdown(&self) {}
    }
    let gate = Arc::new(RequiredGuards::none());
    gate.host_booting(&["example-guard".to_string()], false);
    let bus = HookBus::new().with_required_guards(Arc::clone(&gate));
    let dying = Arc::new(Dying { down: false.into() });
    let mut permissions = PermissionSet::new();
    permissions.grant(Permission::ToolsIntercept);
    bus.subscribe(HookKind::BeforeToolCall, dying.clone(), None, None, permissions)
        .await
        .unwrap();
    gate.mark_loaded("example-guard", dying);
    assert_eq!(
        bus.emit(&call()).await,
        refused("process-gone: its process exited (signal: 9)")
    );
}

/// A required guard reloaded while a call is in its chain (the instance that
/// saw the call stays silent, `Continue`; the new one is loaded and live)
/// does not let that call run: the post-chain check wants the very instance
/// the pre-chain check saw.
#[tokio::test]
async fn a_required_guard_reloaded_during_the_chain_refuses_that_call() {
    struct Live;
    #[async_trait::async_trait]
    impl ExtensionHandler for Live {
        fn id(&self) -> &str {
            "example-guard"
        }
        async fn guard_down(&self) -> Option<String> {
            None
        }
        async fn handle(&self, _event: &HookEvent) -> HookResult {
            HookResult::Continue
        }
        async fn shutdown(&self) {}
    }
    /// Swaps a fresh, live instance in (as a reload does) and stays silent.
    struct Reloading {
        gate: Arc<RequiredGuards>,
    }
    #[async_trait::async_trait]
    impl ExtensionHandler for Reloading {
        fn id(&self) -> &str {
            "example-guard"
        }
        async fn guard_down(&self) -> Option<String> {
            None
        }
        async fn handle(&self, _event: &HookEvent) -> HookResult {
            self.gate.mark_loaded("example-guard", Arc::new(Live));
            HookResult::Continue
        }
        async fn shutdown(&self) {}
    }
    let gate = Arc::new(RequiredGuards::none());
    gate.host_booting(&["example-guard".to_string()], false);
    let bus = HookBus::new().with_required_guards(Arc::clone(&gate));
    let reloading = Arc::new(Reloading { gate: Arc::clone(&gate) });
    let mut permissions = PermissionSet::new();
    permissions.grant(Permission::ToolsIntercept);
    bus.subscribe(HookKind::BeforeToolCall, reloading.clone(), None, None, permissions)
        .await
        .unwrap();
    gate.mark_loaded("example-guard", reloading);
    assert_eq!(
        bus.emit(&call()).await,
        refused("reloaded: it was reloaded during the call")
    );
    // The new instance, once it is the one the pre-check sees, passes.
    let quiet = HookBus::new().with_required_guards(Arc::clone(&gate));
    let mut permissions = PermissionSet::new();
    permissions.grant(Permission::ToolsIntercept);
    quiet
        .subscribe(HookKind::BeforeToolCall, Arc::new(Live), None, None, permissions)
        .await
        .unwrap();
    assert_eq!(quiet.emit(&call()).await, HookResult::Continue);
}

/// [`PROJECT_DIR_FOR_TESTS`] is `dir` while this lives (under
/// `serial(synaps_base_dir)`).
struct ProjectDirGuard;

impl ProjectDirGuard {
    fn set(dir: &std::path::Path) -> Self {
        *PROJECT_DIR_FOR_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(dir.to_path_buf());
        ProjectDirGuard
    }
}

impl Drop for ProjectDirGuard {
    fn drop(&mut self) {
        *PROJECT_DIR_FOR_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

/// A project root's `.synaps/plugins/example-guard`, here an allow-all guard,
/// would override the base dir's `example-guard`. A required guard loads only
/// from the base dir's `plugins/`, so neither copy starts, and every tool call
/// is refused, naming the shadow. A reload keeps refusing. Removing the shadow
/// and reloading clears it. A shadow that appears while the base guard runs
/// refuses the reload and every call again.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn a_project_root_shadow_of_a_required_guard_refuses_every_tool_call() {
    let base = BaseDirGuard::new();
    let _project_plugins = EnvVarGuard::remove("SYNAPS_DISABLE_PROJECT_PLUGINS");
    let project = tempfile::tempdir().unwrap();
    let _project = ProjectDirGuard::set(project.path());
    let dir = tempfile::tempdir().unwrap();
    let shadow_dir = tempfile::tempdir().unwrap();
    install(base.path(), "example-guard", example_guard(dir.path()));
    let shadow_marker = shadow_dir.path().join("started");
    let allow_all = guard_json(
        "/bin/sh",
        json!([
            "-c",
            GUARD,
            "shadow",
            ALLOW,
            shadow_marker.display().to_string(),
            init(),
            shadow_dir.path().join("pid").display().to_string()
        ]),
        &[guard_hook(None)],
    );
    install(&project.path().join(".synaps"), "example-guard", allow_all.clone());
    write_config(base.path(), "required_guards = example-guard\n");
    let (bus, mut mgr) = booted();
    let (loaded, failed) = mgr.discover_and_load().await;
    let shadow = project.path().join(".synaps").join("plugins").join("example-guard");
    let shadowed = refused(&format!(
        "shadowed: '{}' would override it, and a required guard loads only from the base dir's plugins/",
        shadow.display()
    ));
    assert_eq!(bus.emit(&call()).await, shadowed);
    assert_eq!(bus.emit(&tool_call("read")).await, shadowed);
    assert!(!loaded.iter().any(|id| id == "example-guard"), "{loaded:?}");
    assert!(failed.iter().any(|f| f.plugin == "example-guard"), "{failed:?}");
    assert!(!shadow_marker.exists(), "the shadow never ran");
    assert!(!dir.path().join("started").exists(), "nor did the base copy");
    assert!(mgr.reload_from_disk("example-guard").await.is_err());
    assert_eq!(bus.emit(&call()).await, shadowed);
    assert!(!shadow_marker.exists(), "a reload runs no shadow");

    std::fs::remove_dir_all(&shadow).unwrap();
    mgr.reload_from_disk("example-guard").await.unwrap();
    assert_eq!(bus.emit(&call()).await, guard_says_no());

    install(&project.path().join(".synaps"), "example-guard", allow_all);
    assert!(mgr.reload_from_disk("example-guard").await.is_err());
    assert_eq!(bus.emit(&call()).await, shadowed);
    assert!(!shadow_marker.exists());
    mgr.shutdown_all().await;
}

fn boot_host() -> impl std::future::Future<Output = crate::Result<Arc<crate::EngineHost>>> {
    crate::EngineHost::boot(crate::HostOpts {
        profile: None,
        no_extensions: false,
        mcp_discovery: crate::McpDiscovery::Inline,
    })
}

/// Engine boot reads `required_guards` from the config: the host's bus
/// refuses from boot (pending) until discovery loads the guard.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn engine_boot_gates_the_host_bus_with_the_configs_required_guards() {
    let base = BaseDirGuard::new();
    let _no_project = EnvVarGuard::set("SYNAPS_DISABLE_PROJECT_PLUGINS", "1");
    let _offline = crate::test_env::OfflineProviderEnv::new();
    let dir = tempfile::tempdir().unwrap();
    install(base.path(), "example-guard", example_guard(dir.path()));
    write_config(base.path(), "required_guards = example-guard\n");
    let host = boot_host().await.expect("boot");
    let bus = host.ext_manager().read().await.hook_bus().clone();
    let pending = refused("pending: extension discovery has not loaded it yet");
    assert_eq!(bus.emit(&tool_call("read")).await, pending);
    host.ext_manager().write().await.discover_and_load().await;
    assert_eq!(bus.emit(&call()).await, guard_says_no());
    let health = bus.required_guards().health().await;
    assert_eq!(
        crate::extensions::required_guards::status_lines(&health),
        ["required guard: example-guard loaded"]
    );
    host.ext_manager().write().await.shutdown_all().await;
}

/// With no `required_guards` key the booted host's bus has no gate.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn engine_boot_without_required_guards_has_no_gate() {
    let _base = BaseDirGuard::new();
    let _no_project = EnvVarGuard::set("SYNAPS_DISABLE_PROJECT_PLUGINS", "1");
    let _offline = crate::test_env::OfflineProviderEnv::new();
    let host = boot_host().await.expect("boot");
    let bus = host.ext_manager().read().await.hook_bus().clone();
    assert!(bus.required_guards().is_empty());
    assert_eq!(bus.emit(&tool_call("read")).await, HookResult::Continue);
}
