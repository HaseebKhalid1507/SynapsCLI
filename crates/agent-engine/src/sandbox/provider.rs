//! Asking the policy provider, and turning the answer into a session's
//! confinement.
//!
//! The provider is the plugin named by `sandbox.policy_provider`. It is
//! called through the ordinary `hook.handle` envelope: the params are a
//! serialised [`HookEvent`] (`kind = "sandbox_policy"`, `session_id`, `cwd`,
//! the rest null) with `workspace` and `parent_session_id` added beside
//! them. The result is the answer itself: `{"write": [...], "read": [...]}`
//! or `{"refuse": "..."}`. The core calls only that plugin, and only if it
//! declared the `sandbox_policy` hook and holds the `sandbox.policy`
//! permission; otherwise it counts as not loaded.

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_core::config::{SandboxConfine, SandboxSettings};
use serde_json::{json, Value};

use super::policy::{
    canon_lossy, parse_answer, resolve, Answer, PolicyEnv, Protected, ProviderOutcome, Resolution, ResolveInput,
};
use super::{Confinement, SessionSandbox};
use crate::extensions::hooks::events::{HookEvent, HookKind};
use crate::extensions::hooks::HookBus;

/// How long the provider has to answer.
pub const PROVIDER_TIMEOUT: Duration = Duration::from_secs(5);

/// The session id `synaps sandbox check` calls the provider with.
pub const CHECK_SESSION_ID: &str = "check";

/// Why a session was refused: the cause text it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxRefused {
    pub cause: String,
}

impl std::fmt::Display for SandboxRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sandbox: this session cannot be confined, so it was refused ({})", self.cause)
    }
}

impl std::error::Error for SandboxRefused {}

/// The workspace a session is confined around: its working folder, else the
/// process's, in canonical form.
pub fn workspace_for(cwd: Option<&Path>) -> PathBuf {
    let base = cwd.map(Path::to_path_buf).or_else(|| std::env::current_dir().ok()).unwrap_or_else(|| PathBuf::from("/"));
    canon_lossy(&base)
}

/// The `hook.handle` params for the `sandbox_policy` question.
pub fn hook_params(session_id: &str, workspace: &Path, cwd: Option<&Path>, parent_session_id: Option<&str>) -> Value {
    let event = HookEvent {
        kind: HookKind::SandboxPolicy,
        tool_name: None,
        tool_runtime_name: None,
        tool_input: None,
        tool_output: None,
        message: None,
        session_id: Some(session_id.to_string()),
        cwd: cwd.map(|p| p.to_string_lossy().into_owned()),
        subagent_id: None,
        cut_context: None,
        transcript: None,
        data: Value::Null,
    };
    let mut params = serde_json::to_value(&event).unwrap_or(Value::Null);
    if let Some(object) = params.as_object_mut() {
        object.insert("workspace".to_string(), json!(workspace.to_string_lossy()));
        object.insert("parent_session_id".to_string(), json!(parent_session_id));
    }
    params
}

/// Ask `provider` (when one is configured) for the session's policy.
pub async fn ask_provider(
    bus: &HookBus,
    provider: Option<&str>,
    session_id: &str,
    workspace: &Path,
    cwd: Option<&Path>,
    parent_session_id: Option<&str>,
) -> ProviderOutcome {
    let Some(name) = provider else {
        return ProviderOutcome::NotConfigured;
    };
    let Some(handler) = bus.sandbox_policy_handler(name).await else {
        return ProviderOutcome::Missing;
    };
    let params = hook_params(session_id, workspace, cwd, parent_session_id);
    match tokio::time::timeout(PROVIDER_TIMEOUT, handler.hook_value(params)).await {
        Err(_) => ProviderOutcome::Timeout,
        Ok(Err(error)) => {
            tracing::warn!(provider = name, %error, "sandbox policy provider failed");
            ProviderOutcome::Error
        }
        Ok(Ok(value)) => parse_answer(&value),
    }
}

/// Resolve the policy for a session: read the answer, screen it, decide.
/// With confinement off the provider is not called.
pub async fn evaluate(
    bus: &HookBus,
    settings: &SandboxSettings,
    session_id: &str,
    parent_session_id: Option<&str>,
    cwd: Option<&Path>,
) -> (Resolution, PolicyEnv) {
    evaluate_with_abi(super::kernel_abi(), bus, settings, session_id, parent_session_id, cwd).await
}

/// [`evaluate`] with the kernel's ABI given, so a test need not have Landlock.
pub async fn evaluate_with_abi(
    abi: Result<u32, ()>,
    bus: &HookBus,
    settings: &SandboxSettings,
    session_id: &str,
    parent_session_id: Option<&str>,
    cwd: Option<&Path>,
) -> (Resolution, PolicyEnv) {
    let env = PolicyEnv::from_process();
    let workspace = workspace_for(cwd);
    let confine_on = settings.confine == SandboxConfine::Enforce;
    let provider = settings.policy_provider.as_deref();
    let outcome = if confine_on {
        ask_provider(bus, provider, session_id, &workspace, cwd, parent_session_id).await
    } else {
        ProviderOutcome::NotConfigured
    };
    let resolution = resolve(ResolveInput {
        confine_on,
        provider,
        abi,
        outcome,
        workspace: &workspace,
        env: &env,
    });
    (resolution, env)
}

/// Establish a session's sandbox: `Off` when confinement is off, the
/// resolved confinement when it is on and a valid policy exists, and a
/// refusal otherwise. Never returns an unconfined sandbox while confinement
/// is on.
pub async fn establish(
    bus: &HookBus,
    settings: &SandboxSettings,
    session_id: &str,
    parent_session_id: Option<&str>,
    cwd: Option<&Path>,
) -> Result<SessionSandbox, SandboxRefused> {
    establish_with_abi(super::kernel_abi(), bus, settings, session_id, parent_session_id, cwd).await
}

/// [`establish`] with the kernel's ABI given, so a test need not have Landlock.
pub async fn establish_with_abi(
    abi: Result<u32, ()>,
    bus: &HookBus,
    settings: &SandboxSettings,
    session_id: &str,
    parent_session_id: Option<&str>,
    cwd: Option<&Path>,
) -> Result<SessionSandbox, SandboxRefused> {
    if settings.confine == SandboxConfine::Off {
        return Ok(SessionSandbox::Off);
    }
    let (resolution, env) = evaluate_with_abi(abi, bus, settings, session_id, parent_session_id, cwd).await;
    if let Some(refusal) = &resolution.refusal {
        return Err(SandboxRefused { cause: refusal.session_cause.clone() });
    }
    let protected = Protected::new(&env);
    let workspace = workspace_for(cwd);
    let confinement = Confinement::new(resolution, protected, &workspace, cwd);
    // A root with a protected path inside that can't be split around it
    // refuses the session here, not at the first command.
    if let Err(split) = confinement.plan() {
        return Err(SandboxRefused { cause: split.cause() });
    }
    Ok(SessionSandbox::Confined(std::sync::Arc::new(confinement)))
}

/// Why a worker's write root was dropped for lying outside its parent's.
pub const REASON_WIDER_THAN_PARENT: &str = "outside the directories the parent session may write";

/// Establish a worker's sandbox: the provider is asked about the worker (its
/// own session id, the id of the session that spawned it as the engine
/// records it) and the answer is held to the parent's write roots, so a
/// worker is never less confined than the session that spawned it. The
/// provider is the parent's own (`resolution.provider`), never one read from
/// a file or named by the worker.
pub async fn establish_worker(
    bus: &HookBus,
    parent: &Confinement,
    worker_id: &str,
    parent_session_id: Option<&str>,
    cwd: Option<&Path>,
) -> Result<SessionSandbox, SandboxRefused> {
    establish_worker_with_abi(super::kernel_abi(), bus, parent, worker_id, parent_session_id, cwd).await
}

/// [`establish_worker`] with the kernel's ABI given (tests).
pub async fn establish_worker_with_abi(
    abi: Result<u32, ()>,
    bus: &HookBus,
    parent: &Confinement,
    worker_id: &str,
    parent_session_id: Option<&str>,
    cwd: Option<&Path>,
) -> Result<SessionSandbox, SandboxRefused> {
    let settings = SandboxSettings {
        confine: SandboxConfine::Enforce,
        policy_provider: parent.resolution.provider.clone(),
    };
    let (mut resolution, env) = evaluate_with_abi(abi, bus, &settings, worker_id, parent_session_id, cwd).await;
    if let Some(refusal) = &resolution.refusal {
        return Err(SandboxRefused { cause: refusal.session_cause.clone() });
    }
    let mut kept = Vec::new();
    for root in std::mem::take(&mut resolution.write) {
        if parent.roots().iter().any(|p| root.starts_with(p)) {
            kept.push(root);
        } else {
            resolution.dropped.push(super::policy::Dropped { path: root, reason: REASON_WIDER_THAN_PARENT.to_string() });
        }
    }
    resolution.write = kept;
    let protected = Protected::new(&env);
    let workspace = workspace_for(cwd);
    let confinement = Confinement::new(resolution, protected, &workspace, cwd);
    if let Err(split) = confinement.plan() {
        return Err(SandboxRefused { cause: split.cause() });
    }
    Ok(SessionSandbox::Confined(std::sync::Arc::new(confinement)))
}

/// The sandbox for engine work that belongs to no session: MCP tool
/// discovery starts each configured server once to list its tools. With
/// confinement off it is `Off`. With it on the process gets the baseline
/// only (no workspace, no provider folders), or an error when the kernel
/// can't confine it: the server is never started loose.
pub fn discovery_sandbox(settings: &SandboxSettings) -> Result<SessionSandbox, String> {
    discovery_sandbox_with(super::kernel_abi(), settings, &PolicyEnv::from_process())
}

/// [`discovery_sandbox`] with the kernel's ABI and the environment given.
pub fn discovery_sandbox_with(
    abi: Result<u32, ()>,
    settings: &SandboxSettings,
    env: &PolicyEnv,
) -> Result<SessionSandbox, String> {
    if settings.confine == SandboxConfine::Off {
        return Ok(SessionSandbox::Off);
    }
    // `/tmp` as the "workspace" adds nothing the baseline doesn't grant.
    let scratch = Path::new("/tmp");
    let resolution = resolve(ResolveInput {
        confine_on: true,
        provider: Some("discovery"),
        abi,
        outcome: ProviderOutcome::Answer(Answer { write: Vec::new(), read: Vec::new() }),
        workspace: scratch,
        env,
    });
    if let Some(refusal) = &resolution.refusal {
        return Err(refusal.session_cause.clone());
    }
    // No split check here: every spawn builds its ruleset afresh and
    // refuses when it can't (`SessionSandbox::for_spawn`).
    let confinement = Confinement::new(resolution, Protected::new(env), scratch, None);
    Ok(SessionSandbox::Confined(std::sync::Arc::new(confinement)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_params_are_the_hook_envelope_with_two_fields_added() {
        let params = hook_params("s1", Path::new("/w"), Some(Path::new("/w/sub")), Some("p0"));
        assert_eq!(params["kind"], "sandbox_policy");
        assert_eq!(params["session_id"], "s1");
        assert_eq!(params["cwd"], "/w/sub");
        assert_eq!(params["workspace"], "/w");
        assert_eq!(params["parent_session_id"], "p0");
        let top = hook_params("s1", Path::new("/w"), None, None);
        assert!(top["parent_session_id"].is_null());
        assert!(top["cwd"].is_null());
        // The same envelope a tool hook has: the other fields are present, null.
        assert!(top["tool_name"].is_null() && top["tool_input"].is_null());
    }

    #[tokio::test]
    async fn no_provider_configured_refuses_with_no_policy() {
        let bus = HookBus::new();
        let settings = SandboxSettings { confine: SandboxConfine::Enforce, policy_provider: None };
        let refused = establish(&bus, &settings, "s", None, Some(Path::new("/tmp"))).await.err();
        let cause = refused.expect("must refuse").cause;
        // Without Landlock the ABI refusal comes first; either way: refused.
        assert!(cause == "no-policy" || cause == "abi-too-old", "{cause}");
    }

    #[tokio::test]
    async fn a_provider_that_is_not_loaded_refuses() {
        let bus = HookBus::new();
        let settings = SandboxSettings { confine: SandboxConfine::Enforce, policy_provider: Some("example-guard".into()) };
        let refused = establish(&bus, &settings, "s", None, Some(Path::new("/tmp"))).await.err();
        let cause = refused.expect("must refuse").cause;
        assert!(cause == "provider-missing" || cause == "abi-too-old", "{cause}");
    }

    /// A provider that records what it is asked and answers as told.
    struct Fake {
        id: &'static str,
        answer: Result<Value, String>,
        hang: bool,
        seen: std::sync::Mutex<Vec<Value>>,
    }

    impl Fake {
        fn new(id: &'static str, answer: Result<Value, String>) -> Arc<Self> {
            Arc::new(Self { id, answer, hang: false, seen: std::sync::Mutex::new(Vec::new()) })
        }
    }

    #[async_trait::async_trait]
    impl crate::extensions::runtime::ExtensionHandler for Fake {
        fn id(&self) -> &str {
            self.id
        }
        async fn handle(&self, _event: &HookEvent) -> crate::extensions::hooks::events::HookResult {
            crate::extensions::hooks::events::HookResult::Continue
        }
        async fn hook_value(&self, params: Value) -> Result<Value, String> {
            self.seen.lock().unwrap().push(params);
            if self.hang {
                std::future::pending::<()>().await;
            }
            self.answer.clone()
        }
        async fn shutdown(&self) {}
    }

    use std::sync::Arc;

    use crate::extensions::permissions::PermissionSet;

    async fn bus_with(handler: Arc<Fake>, permissions: &[&str]) -> HookBus {
        let bus = HookBus::new();
        let perms = PermissionSet::from_strings(&permissions.iter().map(|p| p.to_string()).collect::<Vec<_>>());
        bus.subscribe(HookKind::SandboxPolicy, handler, None, None, perms).await.expect("subscribe");
        bus
    }

    async fn ask(bus: &HookBus, provider: &str, parent: Option<&str>) -> ProviderOutcome {
        ask_provider(bus, Some(provider), "s1", Path::new("/w"), Some(Path::new("/w")), parent).await
    }

    #[tokio::test]
    async fn the_parent_id_is_the_one_the_engine_passes_and_a_top_level_session_gets_null() {
        let fake = Fake::new("guard", Ok(json!({"write": []})));
        let bus = bus_with(fake.clone(), &["sandbox.policy"]).await;
        // Whatever a worker or an agent claims, the only source is the
        // argument the engine fills from its own session record.
        let claimed = "claimed-by-the-agent";
        let _ = ask(&bus, "guard", Some("true-parent")).await;
        let _ = ask(&bus, "guard", None).await;
        let seen = fake.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0]["parent_session_id"], "true-parent");
        assert_ne!(seen[0]["parent_session_id"], claimed);
        assert!(seen[1]["parent_session_id"].is_null());
        // No other field of the envelope can carry a parent.
        let text = seen[0].to_string();
        assert!(!text.contains(claimed));
    }

    #[test]
    fn the_engine_fills_the_parent_from_its_session_record_at_every_start() {
        // Both session starts (create and unpark) pass the session's own
        // record; nothing else in the actor feeds the provider question.
        let actor = include_str!("../session/actor.rs");
        let calls = actor.matches(".confine_session(").count();
        assert_eq!(calls, 2, "a session start that does not confine");
        let passing = actor.matches("sb.session.parent_session.as_deref(),").count();
        assert_eq!(passing, calls, "a confine_session call that takes its parent from elsewhere");
    }

    #[tokio::test]
    async fn only_the_named_plugin_with_hook_and_permission_is_asked() {
        // Another plugin that declares the hook is never asked.
        let other = Fake::new("other", Ok(json!({"write": ["/data"]})));
        let bus = bus_with(other.clone(), &["sandbox.policy"]).await;
        assert_eq!(ask(&bus, "guard", None).await, ProviderOutcome::Missing);
        assert!(other.seen.lock().unwrap().is_empty());
        // The named plugin without the permission cannot even subscribe.
        let bare = Fake::new("guard", Ok(json!({"write": []})));
        let bus = HookBus::new();
        let refused = bus
            .subscribe(HookKind::SandboxPolicy, bare.clone(), None, None, PermissionSet::new())
            .await;
        assert!(refused.is_err());
        assert_eq!(ask(&bus, "guard", None).await, ProviderOutcome::Missing);
        assert!(bare.seen.lock().unwrap().is_empty());
        // A worker's view has none.
        let named = Fake::new("guard", Ok(json!({"write": []})));
        let host = Arc::new(bus_with(named.clone(), &["sandbox.policy"]).await);
        let worker = HookBus::worker_transforms(&host);
        assert_eq!(ask(&worker, "guard", None).await, ProviderOutcome::Missing);
        assert!(named.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn every_provider_failure_has_its_own_outcome() {
        let good = Fake::new("guard", Ok(json!({"write": ["/data/x"], "read": ["/etc"]})));
        let bus = bus_with(good, &["sandbox.policy"]).await;
        assert_eq!(
            ask(&bus, "guard", None).await,
            ProviderOutcome::Answer(super::super::policy::Answer { write: vec!["/data/x".into()], read: vec!["/etc".into()] })
        );
        let refuse = Fake::new("guard", Ok(json!({"refuse": "no\u{1b} way"})));
        let bus = bus_with(refuse, &["sandbox.policy"]).await;
        assert_eq!(ask(&bus, "guard", None).await, ProviderOutcome::Refused("no way".into()));
        let error = Fake::new("guard", Err("boom".into()));
        let bus = bus_with(error, &["sandbox.policy"]).await;
        assert_eq!(ask(&bus, "guard", None).await, ProviderOutcome::Error);
        let garbage = Fake::new("guard", Ok(json!("nope")));
        let bus = bus_with(garbage, &["sandbox.policy"]).await;
        assert_eq!(ask(&bus, "guard", None).await, ProviderOutcome::BadAnswer);
    }

    #[tokio::test(start_paused = true)]
    async fn a_provider_that_never_answers_times_out() {
        let slow = Arc::new(Fake {
            id: "guard",
            answer: Ok(json!({"write": []})),
            hang: true,
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let bus = bus_with(slow, &["sandbox.policy"]).await;
        assert_eq!(ask(&bus, "guard", None).await, ProviderOutcome::Timeout);
    }

    /// A parent session confined around `ws`, with `extra` provider roots.
    fn parent_for(ws: &Path, extra: &[&Path]) -> Confinement {
        use super::super::policy::Answer;
        let env = PolicyEnv::from_process();
        let resolution = resolve(ResolveInput {
            confine_on: true,
            provider: Some("guard"),
            abi: Ok(7),
            outcome: ProviderOutcome::Answer(Answer {
                write: extra.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
                read: vec![],
            }),
            workspace: ws,
            env: &env,
        });
        assert!(resolution.refusal.is_none(), "{:?}", resolution.refusal);
        Confinement::new(resolution, Protected::new(&env), ws, Some(ws))
    }

    #[tokio::test]
    async fn a_worker_is_held_to_its_parents_roots_and_told_the_true_parent() {
        // Not under /tmp: the baseline grants all of that to every session.
        let tmp = tempfile::tempdir_in("/var/tmp").unwrap();
        let root = canon_lossy(tmp.path());
        let (ws, cache, elsewhere) = (root.join("ws"), root.join("data/cache"), root.join("other/cache"));
        for dir in [&ws, &cache, &elsewhere] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let parent = parent_for(&ws, &[&cache]);
        // The provider offers the worker a folder the parent may not write.
        let answer = json!({"write": [cache.to_string_lossy(), elsewhere.to_string_lossy()]});
        let fake = Fake::new("guard", Ok(answer));
        let bus = bus_with(fake.clone(), &["sandbox.policy"]).await;
        let sandbox = establish_worker_with_abi(Ok(7), &bus, &parent, "sa_7", Some("true-parent"), Some(&ws))
            .await
            .expect("a worker inside its parent's roots is confined");
        let worker = sandbox.confinement().expect("confined").clone();
        assert!(worker.roots().contains(&cache), "{:?}", worker.roots());
        assert!(!worker.roots().contains(&elsewhere), "{:?}", worker.roots());
        let cut = worker.resolution.dropped.iter().find(|d| d.path == elsewhere).expect("reported");
        assert_eq!(cut.reason, REASON_WIDER_THAN_PARENT);
        // The provider was asked about the worker, with the parent the
        // engine recorded, whatever anything else claims.
        let seen = fake.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0]["session_id"], "sa_7");
        assert_eq!(seen[0]["parent_session_id"], "true-parent");
    }

    #[tokio::test]
    async fn a_worker_the_provider_refuses_or_cannot_answer_for_never_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = canon_lossy(tmp.path()).join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let parent = parent_for(&ws, &[]);
        for (answer, cause) in [
            (Ok(json!({"refuse": "no workers today"})), "refused: no workers today"),
            (Err("boom".to_string()), "error"),
            (Ok(json!("garbage")), "bad-answer"),
        ] {
            let bus = bus_with(Fake::new("guard", answer), &["sandbox.policy"]).await;
            let refused = establish_worker_with_abi(Ok(7), &bus, &parent, "sa_1", None, Some(&ws))
                .await
                .expect_err("must refuse");
            assert_eq!(refused.cause, cause);
        }
        // A bus that has no provider (a worker's own view) refuses too.
        let refused = establish_worker_with_abi(Ok(7), &HookBus::new(), &parent, "sa_1", None, Some(&ws))
            .await
            .expect_err("must refuse");
        assert_eq!(refused.cause, "provider-missing");
        // And so does a kernel without the ABI.
        let bus = bus_with(Fake::new("guard", Ok(json!({"write": []}))), &["sandbox.policy"]).await;
        let refused = establish_worker_with_abi(Err(()), &bus, &parent, "sa_1", None, Some(&ws))
            .await
            .expect_err("must refuse");
        assert_eq!(refused.cause, "abi-too-old");
    }

    /// A workspace with a protected path inside it that can't be split around
    /// (too many entries to grant one by one) refuses the session at its
    /// start, naming the path, for a top-level session and for a worker; the
    /// same workspace with a few entries is confined.
    #[tokio::test]
    async fn a_grant_that_cannot_be_split_refuses_the_session_with_the_inner_path() {
        let tmp = tempfile::tempdir_in("/var/tmp").unwrap();
        let ws = canon_lossy(tmp.path()).join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let settings = SandboxSettings { confine: SandboxConfine::Enforce, policy_provider: Some("guard".into()) };
        let bus = bus_with(Fake::new("guard", Ok(json!({"write": []}))), &["sandbox.policy"]).await;
        let parent = parent_for(&ws, &[]);
        // Control: few entries, split around `<ws>/.synaps/plugins`, confined.
        let ok = establish_with_abi(Ok(7), &bus, &settings, "s1", None, Some(&ws)).await.expect("splittable");
        assert!(ok.confinement().is_some());
        establish_worker_with_abi(Ok(7), &bus, &parent, "sa_1", Some("s1"), Some(&ws)).await.expect("splittable");
        for i in 0..=super::super::confinement::MAX_SPLIT_ENTRIES {
            std::fs::write(ws.join(format!("f{i}")), "").unwrap();
        }
        let inner = ws.join(".synaps").join("plugins");
        let want = format!("protected-inside-grant: {}", inner.display());
        let refused = establish_with_abi(Ok(7), &bus, &settings, "s1", None, Some(&ws)).await.expect_err("must refuse");
        assert_eq!(refused.cause, want);
        let refused = establish_worker_with_abi(Ok(7), &bus, &parent, "sa_1", Some("s1"), Some(&ws))
            .await
            .expect_err("must refuse");
        assert_eq!(refused.cause, want);
    }

    #[tokio::test]
    async fn confinement_off_is_off_and_never_asks() {
        let bus = HookBus::new();
        let settings = SandboxSettings::default();
        let s = establish(&bus, &settings, "s", None, None).await.unwrap();
        assert!(s.is_off());
    }

    /// MCP discovery belongs to no session. With confinement off it starts
    /// servers as before; with it on it gets the baseline and nothing else,
    /// or no server at all.
    #[test]
    fn discovery_is_confined_to_the_baseline_or_does_not_run() {
        let env = PolicyEnv::fixed(Path::new("/home/u"), Path::new("/home/u/.synaps-cli"));
        let off = SandboxSettings::default();
        assert!(discovery_sandbox_with(Ok(7), &off, &env).unwrap().is_off());

        let on = SandboxSettings { confine: SandboxConfine::Enforce, policy_provider: None };
        let sandbox = discovery_sandbox_with(Ok(7), &on, &env).expect("a kernel with Landlock confines it");
        let confinement = sandbox.confinement().expect("confined, not off").clone();
        let roots: Vec<String> = confinement.roots().iter().map(|r| r.to_string_lossy().into_owned()).collect();
        let baseline: Vec<String> = super::super::policy::BASELINE
            .iter()
            .map(|b| canon_lossy(Path::new(b.path)).to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            roots.iter().collect::<std::collections::BTreeSet<_>>(),
            baseline.iter().collect::<std::collections::BTreeSet<_>>(),
            "the baseline only: no workspace, no home, no provider folder"
        );
        assert!(confinement.allows_write(Path::new("/home/u/x")).is_err());
        assert!(confinement.allows_write(Path::new("/home/u/.synaps-cli/config")).is_err());

        // No usable kernel: no sandbox, so no server.
        assert_eq!(discovery_sandbox_with(Err(()), &on, &env).err().as_deref(), Some("abi-too-old"));
    }

    /// The discovery start takes its sandbox from `discovery_sandbox`; a
    /// start that names `SessionSandbox::Off` would run servers loose.
    #[test]
    fn mcp_discovery_never_starts_a_server_unconfined() {
        let source = include_str!("../mcp/mod.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        let start = production.find("async fn discover_one").expect("discover_one");
        let end = production[start..].find("\nasync fn ").map(|n| start + n).unwrap_or(production.len());
        let body = &production[start..end];
        assert!(body.contains("discovery_sandbox("), "discovery must ask for its sandbox");
        assert!(!body.contains("SessionSandbox::Off"), "discovery must not start a server unconfined");
        assert_eq!(body.matches("McpConnection::start(").count(), 1);
    }
}
