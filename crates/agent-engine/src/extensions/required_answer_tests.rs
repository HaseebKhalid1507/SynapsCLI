//! Pins: a required guard that is loaded and live but gives no usable
//! answer to a `before_tool_call` (stopped or hung, slow on a large input,
//! a JSON-RPC error, a garbled answer) blocks the call, naming the guard
//! and the cause, whatever its manifest's `on_failure` says. A guard that
//! is not required keeps its `on_failure` (unchanged).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::extensions::hooks::events::{HookEvent, HookFailurePolicy, HookKind, HookResult};
use crate::extensions::hooks::HookBus;
use crate::extensions::manager::ExtensionManager;
use crate::extensions::manifest::ExtensionManifest;
use crate::extensions::permissions::{Permission, PermissionSet};
use crate::extensions::required_guards::{NoAnswer, NotLoaded, RequiredGuards};
use crate::extensions::runtime::process::guard_fail_closed_tests::{
    blocked, init, signal, GUARD, NO,
};
use crate::extensions::runtime::process::ProcessExtension;
use crate::extensions::runtime::ExtensionHandler;

/// What a call gets when required guard `example-guard` gave no usable answer for
/// `cause` (`code: detail`). Spelled out here, not built with the code's
/// own `no_answer_text`, so a changed text turns these pins red.
fn no_answer(cause: &str) -> HookResult {
    HookResult::block(format!(
        "Refused: required guard 'example-guard' gave no usable answer ({cause}). No tool call runs without a required guard's answer, whatever its on_failure says."
    ))
}

const TIMEOUT: &str =
    "timeout: it did not answer within 5 s (hung, stopped, busy, or slow on this input)";

/// A JSON-RPC error reply from the [`GUARD`] fixture: its `result` slot
/// is `null`, then an `error` member, which the reader reads first.
const ERROR_REPLY: &str = r#"null,"error":{"code":-32000,"message":"guard broke"}"#;

/// Not a hook result.
const BOGUS: &str = r#"{"action":"bogus"}"#;

/// Not JSON at all: the frame does not parse.
const GARBAGE: &str = "garbage";

const ALLOW: &str = r#"{"action":"continue"}"#;

fn call() -> HookEvent {
    HookEvent::before_tool_call("bash", json!({"command": "rm -rf /"}))
}

fn write(content: &str) -> HookEvent {
    HookEvent::before_tool_call("write", json!({"path": "/tmp/x", "content": content}))
}

fn said_no() -> HookResult {
    HookResult::block("guard says no")
}

fn intercept() -> PermissionSet {
    let mut set = PermissionSet::new();
    set.grant(Permission::ToolsIntercept);
    set
}

// ---- The bus (fake handlers, paused time) ----

enum Mode {
    /// Never answers (a stopped or hung guard).
    Hang,
    /// Answers this.
    Answer(HookResult),
    /// `handle_required` reports this; `handle` answers `Continue`.
    Fail(NoAnswer),
}

/// A live handler (`guard_down` is `None`) answering per its [`Mode`],
/// counting its calls.
struct Fake {
    id: &'static str,
    mode: Mode,
    calls: AtomicUsize,
}

impl Fake {
    fn new(id: &'static str, mode: Mode) -> Arc<Self> {
        Arc::new(Self { id, mode, calls: AtomicUsize::new(0) })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ExtensionHandler for Fake {
    fn id(&self) -> &str {
        self.id
    }

    async fn guard_down(&self) -> Option<String> {
        None
    }

    async fn handle(&self, _event: &HookEvent) -> HookResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.mode {
            Mode::Hang => std::future::pending().await,
            Mode::Answer(result) => result.clone(),
            Mode::Fail(_) => HookResult::Continue,
        }
    }

    async fn handle_required(
        &self,
        event: &HookEvent,
        _no_spawn: bool,
    ) -> Result<HookResult, NoAnswer> {
        match &self.mode {
            Mode::Fail(why) => {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Err(why.clone())
            }
            _ => Ok(self.handle(event).await),
        }
    }

    async fn shutdown(&self) {}
}

/// A bus gated on `example-guard`, with `guards` subscribed in order (each with its
/// `on_failure`; `example-guard` marked loaded and live), then a `later` handler
/// that answers `Continue`.
async fn gated(guards: &[(Arc<Fake>, HookFailurePolicy)]) -> (HookBus, Arc<Fake>) {
    let gate = Arc::new(RequiredGuards::new(&["example-guard".to_string()]));
    let bus = HookBus::new().with_required_guards(Arc::clone(&gate));
    for (fake, on_failure) in guards {
        let handler: Arc<dyn ExtensionHandler> = fake.clone();
        if fake.id == "example-guard" {
            gate.mark_loaded("example-guard", Arc::clone(&handler));
        }
        bus.subscribe_with_failure_policy(
            HookKind::BeforeToolCall,
            handler,
            None,
            None,
            intercept(),
            *on_failure,
        )
        .await
        .unwrap();
    }
    let later = Fake::new("later", Mode::Answer(HookResult::Continue));
    bus.subscribe(HookKind::BeforeToolCall, later.clone(), None, None, intercept())
        .await
        .unwrap();
    (bus, later)
}

/// Pin (stopped / hung, the bus's timeout): a required guard that never
/// answers blocks the call at the 5 s hook timeout with the timeout
/// cause, whether its manifest says `continue` or `block`; the chain ends.
#[tokio::test(start_paused = true)]
async fn a_hung_required_guard_blocks_at_the_hook_timeout_whatever_its_on_failure() {
    for on_failure in [HookFailurePolicy::Continue, HookFailurePolicy::Block] {
        let required = Fake::new("example-guard", Mode::Hang);
        let (bus, later) = gated(&[(required.clone(), on_failure)]).await;
        let started = tokio::time::Instant::now();
        let result = bus.emit(&call()).await;
        let waited = started.elapsed();
        assert_eq!(result, no_answer(TIMEOUT), "{on_failure:?}");
        assert!(
            waited >= Duration::from_secs(5) && waited < Duration::from_secs(6),
            "at the hook timeout: {waited:?}"
        );
        assert_eq!(required.calls(), 1);
        assert_eq!(later.calls(), 0, "a block ends the chain");
    }
}

/// Pin (garbled, the bus's check): an answer a guard may not give before
/// a tool call (`replace`, `inject`, `context_phase`), ignored for any
/// other guard, is a bad answer from a required one.
#[tokio::test]
async fn a_required_guards_answer_it_may_not_give_before_a_call_blocks() {
    let answers = [
        (HookResult::Replace { output: "x".into() }, "replace"),
        (HookResult::inject("x"), "inject"),
        (HookResult::ContextPhase { phase: "x".into() }, "context_phase"),
    ];
    for (answer, action) in answers {
        let required = Fake::new("example-guard", Mode::Answer(answer));
        let (bus, later) = gated(&[(required, HookFailurePolicy::Continue)]).await;
        let cause = format!(
            "bad-answer: its answer is not a usable hook result: '{action}' is not an answer to a before_tool_call"
        );
        assert_eq!(bus.emit(&call()).await, no_answer(&cause));
        assert_eq!(later.calls(), 0);
    }
}

/// Pin (exact reason text, the bus's mapping): every [`NoAnswer`] the
/// guard's handler reports blocks with the guard and the cause named.
#[tokio::test]
async fn the_bus_names_the_guard_and_each_cause_exactly() {
    let long = "x".repeat(300);
    let causes = [
        (NoAnswer::Timeout, TIMEOUT.to_string()),
        (
            NoAnswer::Error("guard broke".into()),
            "error: it answered with an error: guard broke".to_string(),
        ),
        (
            NoAnswer::CallFailed("Write error: broken pipe".into()),
            "error: its call failed: Write error: broken pipe".to_string(),
        ),
        (
            NoAnswer::BadAnswer("unknown variant `bogus`".into()),
            "bad-answer: its answer is not a usable hook result: unknown variant `bogus`".to_string(),
        ),
        (
            NoAnswer::Error(format!("{long}\nnext")),
            format!("error: it answered with an error: {}…", "x".repeat(200)),
        ),
        (
            NoAnswer::Error("two\nlines".into()),
            "error: it answered with an error: two lines".to_string(),
        ),
    ];
    for (why, cause) in causes {
        let required = Fake::new("example-guard", Mode::Fail(why));
        let (bus, later) = gated(&[(required.clone(), HookFailurePolicy::Continue)]).await;
        assert_eq!(bus.emit(&call()).await, no_answer(&cause));
        assert_eq!(required.calls(), 1);
        assert_eq!(later.calls(), 0);
    }
}

/// Pin (unchanged): a guard that is not required keeps its `on_failure`
/// while a required one is loaded and answering: hung and failing open,
/// it is skipped at the timeout; a `replace` from it is ignored; its
/// `handle_required` is never called.
#[tokio::test(start_paused = true)]
async fn a_guard_that_is_not_required_keeps_its_on_failure() {
    let modes = [
        Mode::Hang,
        Mode::Answer(HookResult::Replace { output: "x".into() }),
        Mode::Fail(NoAnswer::Error("not asked".into())),
    ];
    for mode in modes {
        let required = Fake::new("example-guard", Mode::Answer(HookResult::Continue));
        let other = Fake::new("other", mode);
        let (bus, later) = gated(&[
            (required.clone(), HookFailurePolicy::Continue),
            (other.clone(), HookFailurePolicy::Continue),
        ])
        .await;
        assert_eq!(bus.emit(&call()).await, HookResult::Continue);
        assert_eq!((required.calls(), other.calls(), later.calls()), (1, 1, 1));
    }
    // And one failing closed keeps its own (unchanged) text.
    let required = Fake::new("example-guard", Mode::Answer(HookResult::Continue));
    let other = Fake::new("other", Mode::Hang);
    let (bus, _) = gated(&[
        (required, HookFailurePolicy::Continue),
        (other, HookFailurePolicy::Block),
    ])
    .await;
    assert_eq!(
        bus.emit(&call()).await,
        HookResult::block(
            "guard other didn't answer within 5 s (busy or stopped); the call is blocked"
        )
    );
}

// ---- a required guard's Modify the bus cannot apply ----

/// The rewrite a guard answers with (the call without its `-rf /`).
fn rewrite() -> HookResult {
    HookResult::Modify { input: json!({"command": "rm -i /tmp/x"}) }
}

const MODIFY_AFTER_CONFIRM: &str = "bad-answer: its answer is not a usable hook result: 'modify' cannot apply here: an earlier extension (asker) asked to confirm the input as it was";

/// Pin (after a Confirm): an earlier extension asks to confirm the
/// call, then the required guard rewrites it. The bus cannot apply that
/// rewrite (the user confirms the input as it was), and ignoring it would
/// let an approval run the input the guard rewrote: it blocks, naming the
/// guard. Unchanged: with no earlier Confirm the rewrite applies, and a
/// guard that is not required still has its rewrite ignored.
#[tokio::test]
async fn a_required_guards_modify_after_a_confirm_blocks() {
    let asker = Fake::new("asker", Mode::Answer(HookResult::confirm("Run it?")));
    let required = Fake::new("example-guard", Mode::Answer(rewrite()));
    let (bus, later) = gated(&[
        (asker.clone(), HookFailurePolicy::Continue),
        (required.clone(), HookFailurePolicy::Continue),
    ])
    .await;
    assert_eq!(bus.emit(&call()).await, no_answer(MODIFY_AFTER_CONFIRM));
    assert_eq!((asker.calls(), required.calls(), later.calls()), (1, 1, 0));

    // Unchanged: no earlier Confirm, so the guard's rewrite applies.
    let required = Fake::new("example-guard", Mode::Answer(rewrite()));
    let (bus, _) = gated(&[(required, HookFailurePolicy::Continue)]).await;
    assert_eq!(bus.emit(&call()).await, rewrite());

    // Unchanged: a guard that is not required has its rewrite ignored.
    let required = Fake::new("example-guard", Mode::Answer(HookResult::Continue));
    let asker = Fake::new("asker", Mode::Answer(HookResult::confirm("Run it?")));
    let other = Fake::new("other", Mode::Answer(rewrite()));
    let (bus, later) = gated(&[
        (required, HookFailurePolicy::Continue),
        (asker, HookFailurePolicy::Continue),
        (other, HookFailurePolicy::Continue),
    ])
    .await;
    match bus.emit(&call()).await {
        HookResult::Confirm { message, sources, modified_input, .. } => {
            assert_eq!(message, "Run it?");
            assert_eq!(sources, vec!["asker".to_string()]);
            assert_eq!(modified_input, None, "the rewrite is ignored");
        }
        other => panic!("expected the asker's Confirm, got {other:?}"),
    }
    assert_eq!(later.calls(), 1);
}

// ---- The process handler (the GUARD sh fixture) ----

/// [`GUARD`] as `example-guard`, answering every `hook.handle` with `answer`,
/// writing its pid to `pidfile`, and (`slow_over` set) taking 7 s on a
/// request longer than `slow_over` bytes.
async fn spawn_guard(answer: &str, pidfile: &str, slow_over: &str) -> ProcessExtension {
    let init = init();
    let args = ["-c", GUARD, "guard", answer, "", init.as_str(), pidfile, "", "", slow_over]
        .map(String::from);
    let ext = ProcessExtension::spawn("example-guard", "/bin/sh", &args)
        .await
        .expect("spawn the guard fixture");
    tokio::time::timeout(Duration::from_secs(5), ext.initialize_for_test(None))
        .await
        .expect("bounded initialize")
        .expect("initialize");
    ext
}

fn read_pid(pidfile: &std::path::Path) -> String {
    std::fs::read_to_string(pidfile).unwrap().trim().to_string()
}

/// Pin (SIGSTOP, the handler's timeout): a stopped required guard is a
/// timeout within 5 s, never an answer.
#[tokio::test]
async fn a_stopped_required_guard_is_a_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let ext = spawn_guard(NO, &pidfile.display().to_string(), "").await;
    assert_eq!(ext.handle_required(&call(), false).await, Ok(said_no()));
    let pid = read_pid(&pidfile);
    signal(&pid, "-STOP");
    let started = Instant::now();
    let result = ext.handle_required(&call(), false).await;
    let waited = started.elapsed();
    signal(&pid, "-KILL");
    ext.force_shutdown().await;
    assert_eq!(result, Err(NoAnswer::Timeout));
    assert!(waited < Duration::from_secs(6), "within the hook timeout: {waited:?}");
}

/// Pin (slow input, the handler): a guard that takes over 5 s on a large
/// input is a timeout for that input; a small one it answers.
#[tokio::test]
async fn a_required_guard_slow_on_a_large_input_is_a_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let ext = spawn_guard(NO, &pidfile.display().to_string(), "10000").await;
    assert_eq!(ext.handle_required(&write("small"), false).await, Ok(said_no()));
    let started = Instant::now();
    let large = write(&"a".repeat(64 * 1024));
    let result = ext.handle_required(&large, false).await;
    let waited = started.elapsed();
    ext.force_shutdown().await;
    assert_eq!(result, Err(NoAnswer::Timeout));
    assert!(waited < Duration::from_secs(6), "within the hook timeout: {waited:?}");
}

/// Pin (JSON-RPC error, the handler): an error answer is the `error`
/// cause with the guard's message; any other path is unchanged (failing
/// open it is `Continue`, failing closed the fixed block).
#[tokio::test]
async fn a_json_rpc_error_answer_is_an_error() {
    let ext = spawn_guard(ERROR_REPLY, "", "").await;
    assert_eq!(
        ext.handle_required(&call(), false).await,
        Err(NoAnswer::Error("guard broke".into()))
    );
    assert_eq!(ext.handle(&call()).await, HookResult::Continue);
    assert_eq!(ext.handle_fail_closed(&call(), false).await, blocked("example-guard"));
    ext.force_shutdown().await;
}

/// Pin (garbled, the handler): an answer that is not a hook result (an
/// unknown action, a missing `result`) is a bad answer; a frame that is
/// not JSON is a failed call (after its one respawn and retry). Failing
/// open, each is `Continue`, as before.
#[tokio::test]
async fn a_garbled_answer_is_a_bad_answer_or_a_failed_call() {
    for answer in [BOGUS, "null"] {
        let ext = spawn_guard(answer, "", "").await;
        let result = ext.handle_required(&call(), false).await;
        assert!(matches!(result, Err(NoAnswer::BadAnswer(_))), "{answer}: {result:?}");
        assert_eq!(ext.handle(&call()).await, HookResult::Continue, "{answer}");
        ext.force_shutdown().await;
    }
    let ext = spawn_guard(GARBAGE, "", "").await;
    let result = ext.handle_required(&call(), false).await;
    ext.force_shutdown().await;
    match result {
        Err(NoAnswer::CallFailed(error)) => {
            assert!(error.contains("retry after restart failed"), "{error}")
        }
        other => panic!("a garbled frame is a failed call: {other:?}"),
    }
}

// ---- End to end: a real guard loaded by the manager, on a gated bus ----

/// A live guard manifest: [`GUARD`] answering with `answer`, its pid in
/// `pidfile`, slow over `slow_over` bytes, its hook's `on_failure` as
/// given (`None`: the default, continue).
fn guard_manifest(
    answer: &str,
    pidfile: &str,
    slow_over: &str,
    on_failure: Option<&str>,
) -> ExtensionManifest {
    let mut hook = json!({"hook": "before_tool_call"});
    if let Some(value) = on_failure {
        hook["on_failure"] = json!(value);
    }
    let args = json!(["-c", GUARD, "guard", answer, "", init(), pidfile, "", "", slow_over]);
    let extension: Value = json!({
        "runtime": "process",
        "command": "/bin/sh",
        "args": args,
        "permissions": ["tools.intercept"],
        "hooks": [hook]
    });
    serde_json::from_value(extension).unwrap()
}

/// A manager on a bus gated on `example-guard`, with `guards` (id, manifest) loaded.
async fn loaded(guards: &[(&str, ExtensionManifest)]) -> (Arc<HookBus>, ExtensionManager) {
    let gate = Arc::new(RequiredGuards::new(&["example-guard".to_string()]));
    gate.host_booting(&[], false);
    let bus = Arc::new(HookBus::new().with_required_guards(gate));
    let mut mgr = ExtensionManager::new(bus.clone());
    for (id, manifest) in guards {
        mgr.load(id, manifest).await.unwrap();
    }
    (bus, mgr)
}

/// Pin (SIGSTOP, end to end, exact text): a required guard whose manifest
/// says `on_failure: "continue"`, loaded and answering, then SIGSTOPped,
/// refuses every tool call within the hook timeout, naming the timeout.
#[tokio::test]
async fn a_stopped_required_guard_refuses_every_call_naming_the_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let manifest = guard_manifest(NO, &pidfile.display().to_string(), "", Some("continue"));
    let (bus, mut mgr) = loaded(&[("example-guard", manifest)]).await;
    assert_eq!(bus.emit(&call()).await, said_no());
    let pid = read_pid(&pidfile);
    signal(&pid, "-STOP");
    let started = Instant::now();
    let result = bus.emit(&write("x")).await;
    let waited = started.elapsed();
    signal(&pid, "-KILL");
    mgr.shutdown_all().await;
    assert_eq!(result, no_answer(TIMEOUT));
    assert!(waited < Duration::from_secs(6), "within the hook timeout: {waited:?}");
}

/// Pin (slow input, end to end): a large write that takes the required
/// guard past 5 s is refused with the timeout; a small one it answers.
#[tokio::test]
async fn a_large_input_that_takes_the_required_guard_past_5_s_is_refused() {
    let manifest = guard_manifest(NO, "", "10000", None);
    let (bus, mut mgr) = loaded(&[("example-guard", manifest)]).await;
    assert_eq!(bus.emit(&write("small")).await, said_no());
    let started = Instant::now();
    let result = bus.emit(&write(&"a".repeat(64 * 1024))).await;
    let waited = started.elapsed();
    mgr.shutdown_all().await;
    assert_eq!(result, no_answer(TIMEOUT));
    assert!(waited < Duration::from_secs(6), "within the hook timeout: {waited:?}");
}

/// Pin (JSON-RPC error and garbled, end to end, exact text): each refuses
/// the call naming the cause.
#[tokio::test]
async fn a_required_guards_error_or_garbled_answer_refuses_the_call() {
    let bogus: Value = serde_json::from_str(BOGUS).unwrap();
    let unknown = serde_json::from_value::<HookResult>(bogus).unwrap_err().to_string();
    let cases = [
        (ERROR_REPLY, "error: it answered with an error: guard broke".to_string()),
        (BOGUS, format!("bad-answer: its answer is not a usable hook result: {unknown}")),
    ];
    for (answer, cause) in cases {
        let (bus, mut mgr) = loaded(&[("example-guard", guard_manifest(answer, "", "", None))]).await;
        assert_eq!(bus.emit(&call()).await, no_answer(&cause), "{answer}");
        mgr.shutdown_all().await;
    }
    let (bus, mut mgr) = loaded(&[("example-guard", guard_manifest(GARBAGE, "", "", None))]).await;
    let result = bus.emit(&call()).await;
    mgr.shutdown_all().await;
    let HookResult::Block { reason, .. } = result else {
        panic!("a garbled frame refuses the call: {result:?}");
    };
    assert!(
        reason.starts_with("Refused: required guard 'example-guard' gave no usable answer (error: its call failed: "),
        "{reason}"
    );
}

/// Pin (unchanged, end to end): with required guard `example-guard` loaded and
/// allowing, a guard that is not required and fails open still lets the
/// call run when it answers with an error, garbles, or is stopped.
#[tokio::test]
async fn a_guard_that_is_not_required_still_continues_when_it_fails_open() {
    for answer in [ERROR_REPLY, BOGUS, GARBAGE] {
        let guards = [
            ("example-guard", guard_manifest(ALLOW, "", "", None)),
            ("other", guard_manifest(answer, "", "", None)),
        ];
        let (bus, mut mgr) = loaded(&guards).await;
        assert_eq!(bus.emit(&call()).await, HookResult::Continue, "{answer}");
        mgr.shutdown_all().await;
    }
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let guards = [
        ("example-guard", guard_manifest(ALLOW, "", "", None)),
        ("other", guard_manifest(NO, &pidfile.display().to_string(), "", None)),
    ];
    let (bus, mut mgr) = loaded(&guards).await;
    assert_eq!(bus.emit(&call()).await, said_no());
    let pid = read_pid(&pidfile);
    signal(&pid, "-STOP");
    let result = bus.emit(&call()).await;
    signal(&pid, "-KILL");
    mgr.shutdown_all().await;
    assert_eq!(result, HookResult::Continue);
}

// ---- The post-chain check (a guard that changes while the call is in its chain) ----

/// A handler that, when the chain reaches it, changes required guard
/// `example-guard`'s state: unloads it, or reloads it as `reload_as`.
struct Flipper {
    gate: Arc<RequiredGuards>,
    reload_as: Option<Arc<dyn ExtensionHandler>>,
}

#[async_trait]
impl ExtensionHandler for Flipper {
    fn id(&self) -> &str {
        "flipper"
    }

    async fn guard_down(&self) -> Option<String> {
        None
    }

    async fn handle(&self, _event: &HookEvent) -> HookResult {
        match &self.reload_as {
            Some(handler) => self.gate.mark_loaded("example-guard", Arc::clone(handler)),
            None => self.gate.mark_not_loaded("example-guard", NotLoaded::Unloaded),
        }
        HookResult::Continue
    }

    async fn shutdown(&self) {}
}

/// A bus gated on `example-guard` (loaded and live in `guard`), whose chain
/// is `guard` then a [`Flipper`]; or the flipper first when `flipper_first`.
async fn flipping(
    guard: Arc<Fake>,
    reload_as: Option<Arc<dyn ExtensionHandler>>,
    flipper_first: bool,
) -> HookBus {
    let gate = Arc::new(RequiredGuards::new(&["example-guard".to_string()]));
    let bus = HookBus::new().with_required_guards(Arc::clone(&gate));
    let guard: Arc<dyn ExtensionHandler> = guard;
    gate.mark_loaded("example-guard", Arc::clone(&guard));
    let flipper: Arc<dyn ExtensionHandler> = Arc::new(Flipper { gate, reload_as });
    let chain = if flipper_first { [flipper, guard] } else { [guard, flipper] };
    for handler in chain {
        bus.subscribe(HookKind::BeforeToolCall, handler, None, None, intercept())
            .await
            .unwrap();
    }
    bus
}

/// Pin (after the chain): a required guard that is unloaded while the call
/// is in its chain refuses the call, even though the chain (the guard
/// included) said `Continue`: the guard's silence is no allow.
#[tokio::test]
async fn a_required_guard_unloaded_during_the_chain_refuses_the_call() {
    let guard = Fake::new("example-guard", Mode::Answer(HookResult::Continue));
    let bus = flipping(Arc::clone(&guard), None, false).await;
    match bus.emit(&call()).await {
        HookResult::Block { reason } => {
            assert!(
                reason.starts_with("Refused: required guard 'example-guard' is not loaded (unloaded: "),
                "{reason}"
            );
        }
        other => panic!("expected the refusal, got {other:?}"),
    }
    assert_eq!(guard.calls(), 1, "the guard did see the call");
}

/// Pin (after the chain): a required guard reloaded while the call is in
/// its chain is not the instance that vetted it, so the call is refused as
/// reloaded.
#[tokio::test]
async fn a_required_guard_reloaded_during_the_chain_refuses_the_call() {
    let guard = Fake::new("example-guard", Mode::Answer(HookResult::Continue));
    let reloaded: Arc<dyn ExtensionHandler> =
        Fake::new("example-guard", Mode::Answer(HookResult::Continue));
    let bus = flipping(Arc::clone(&guard), Some(reloaded), false).await;
    match bus.emit(&call()).await {
        HookResult::Block { reason } => {
            assert!(
                reason.starts_with(
                    "Refused: required guard 'example-guard' is not loaded (reloaded: it was reloaded during the call)"
                ),
                "{reason}"
            );
        }
        other => panic!("expected the refusal, got {other:?}"),
    }
}

/// Pin (after the chain): a chain that blocked keeps its own block, not the
/// refusal, and a guard that is unchanged through the chain lets the call
/// through.
#[tokio::test]
async fn the_post_chain_check_keeps_a_chain_block_and_an_unchanged_guards_allow() {
    let guard = Fake::new("example-guard", Mode::Answer(HookResult::Continue));
    // The flipper runs first and unloads the guard; the guard, still holding
    // its instance, then blocks: its Block is the answer.
    let blocker = Fake::new("example-guard", Mode::Answer(said_no()));
    let bus = flipping(Arc::clone(&blocker), None, true).await;
    assert_eq!(bus.emit(&call()).await, said_no());
    assert_eq!(blocker.calls(), 1);

    // Nothing changes: the call goes through.
    let (bus, later) = gated(&[(guard.clone(), HookFailurePolicy::Continue)]).await;
    assert_eq!(bus.emit(&call()).await, HookResult::Continue);
    assert_eq!((guard.calls(), later.calls()), (1, 1));
}
