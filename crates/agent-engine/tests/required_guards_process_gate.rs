//! The production wiring of the required-guards gate.
//!
//! The engine's unit tests cannot pin it: under `cfg(test)`
//! `RequiredGuards::process()` is a fresh, empty gate per call, so no test
//! can refuse another's tool calls. This file builds the lib as production
//! does. A base dir whose config says `required_guards = example-guard`, and
//! a bus made with `HookBus::new()` in a process that has no extension host:
//! every tool call is refused, with the exact no-extension-host text.
//!
//! One test, alone in its own binary: the process gate is read once per
//! process (a `OnceLock`), so the base dir is set before any bus exists.

use agent_engine::extensions::hooks::events::{HookEvent, HookResult};
use agent_engine::extensions::hooks::HookBus;
use agent_engine::extensions::required_guards::status_lines;
use serde_json::json;

/// Spelled out, not built with the code's own `refusal_text`, so a changed
/// text turns this pin red.
const REFUSED: &str = "Refused: required guard 'example-guard' is not loaded (no-extension-host: this process loads no extensions). No tool call runs while a required guard is not loaded; restore the guard, then reload it.";

#[tokio::test]
async fn a_bus_made_with_new_refuses_every_tool_call_while_the_configs_required_guard_is_not_loaded() {
    let base = tempfile::tempdir().expect("tempdir");
    std::fs::write(base.path().join("config"), "required_guards = example-guard\n")
        .expect("write config");
    std::env::set_var("SYNAPS_BASE_DIR", base.path());
    std::env::remove_var("SYNAPS_PROFILE");

    let bus = HookBus::new();
    for tool in ["bash", "read", "write", "edit", "multi_edit"] {
        let call = HookEvent::before_tool_call(tool, json!({}));
        assert_eq!(bus.emit(&call).await, HookResult::block(REFUSED), "{tool}");
    }
    // Every bus of the process shares the one gate.
    let other = HookBus::new();
    assert!(std::sync::Arc::ptr_eq(bus.required_guards(), other.required_guards()));
    let call = HookEvent::before_tool_call("bash", json!({"command": "true"}));
    assert_eq!(other.emit(&call).await, HookResult::block(REFUSED));
    // And its health row says why.
    let health = bus.required_guards().health().await;
    assert_eq!(
        status_lines(&health),
        ["required guard: example-guard NOT LOADED (no-extension-host: this process loads no extensions)"]
    );
}
