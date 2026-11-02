//! A legacy (non-exclusive) binding restores a resumed window from the store
//! that binding seals into, never from the process's working-directory scope
//! (subagent rollover, slice 9 checklist item (e)).
//!
//! A daemon session's binding is scoped to its client's working directory and
//! a worker inherits its parent's binding, so neither need be the process's
//! store ([`archive_store`]). Restoring from the process's store left such a
//! session at window 1 with no archive: a daemon session numbered its next
//! window wrong, and a resumed v2 worker's first rollover marker named a
//! window its record refuses. A CLI session's binding is the process's store,
//! so it restores exactly as before (`e3`).

use super::*;
use crate::test_env::{BaseDirGuard, EnvVarGuard};
use agent_core::config::MemoryBackendConfig;
use agent_core::memory::store::ProjectScope;
use std::path::{Path, PathBuf};

fn history() -> Vec<SharedMessage> {
    vec![
        Arc::new(json!({"role":"user","content":"Build the plan. Do not deploy."})),
        Arc::new(json!({"role":"assistant","content":"prior evidence ".repeat(5000)})),
        Arc::new(json!({"role":"user","content":"Write the spec first."})),
        Arc::new(
            json!({"role":"assistant","content":[{"type":"tool_use","id":"checkpoint_1","name":"context_checkpoint","input":{"phase":"execute"}}]}),
        ),
        Arc::new(
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"checkpoint_1","content":"phase=execute"}]}),
        ),
    ]
}

/// Seal [`history`] on `binding` through the real rollover and commit its
/// head: the successor carries the marker of window 2 and its archive.
async fn rolled_over(binding: &MemoryBinding) -> (Vec<SharedMessage>, String) {
    let state: SharedContinuation = Arc::new(Mutex::new(ContinuationState::default()));
    {
        let mut s = state.lock().unwrap();
        s.config.mode = ContextManagementMode::Auto;
        s.checkpoint(WorkPhase::Plan, Some("The plan holds; do not deploy."))
            .unwrap();
        s.checkpoint(WorkPhase::Execute, None).unwrap();
    }
    let prepared = rollover(
        &history(),
        &state,
        binding,
        100_000,
        SuccessorBudget::UNBOUNDED,
        &Default::default(),
    )
    .await
    .unwrap();
    let head = prepared.commit(&state).unwrap();
    let id = state.lock().unwrap().latest_archive.clone().unwrap();
    (head, id)
}

/// A project root of its own under `base`, marked so discovery stops there.
fn project(base: &Path, name: &str) -> PathBuf {
    let dir = base.join(name);
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    dir.canonicalize().unwrap()
}

/// A legacy binding whose base directory and project scope are not the
/// process's, as a worker's or a daemon session's may be.
fn foreign(base: &Path) -> MemoryBinding {
    MemoryBinding::for_test(
        base.to_path_buf(),
        ProjectScope::for_root(&project(base, "elsewhere")).unwrap(),
        &MemoryBackendConfig::default(),
    )
}

fn process_scope_key() -> String {
    ProjectScope::discover(&std::env::current_dir().unwrap())
        .unwrap()
        .key()
        .to_owned()
}

fn in_process_store(id: &str) -> bool {
    archive_store().unwrap().fetch(id, 0, 1).is_ok()
}

/// A worker's runtime holds the binding it inherited
/// (`apply_subagent_runtime_policy`), and its run is bound by the same reset
/// `Runtime::start_worker_run` makes: the window and archive come back from
/// that binding's store.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn e1_a_worker_restores_its_window_from_its_own_binding_store() {
    let _env = BaseDirGuard::new();
    let store = tempfile::tempdir().unwrap();
    let parent = foreign(store.path());
    let (opening, id) = rolled_over(&parent).await;
    assert!(!in_process_store(&id), "sealed outside the process's store");
    let mut rt = crate::Runtime::new_headless();
    rt.inherit_memory_backend(parent);
    rt.reset_context_continuation("worker-run:e1", &opening);
    let s = rt.continuation.lock().unwrap();
    assert_eq!(s.window, 2, "the window comes from the run's own binding");
    assert_eq!(s.latest_archive.as_deref(), Some(id.as_str()));
}

/// A daemon session's binding is scoped to its client's working directory
/// (the cwd goes in before the config, as `foreground_runtime_with` does),
/// not the daemon's; its window comes back from there.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn e2_a_daemon_session_restores_its_window_from_its_session_scope() {
    let env = BaseDirGuard::new();
    let _root = EnvVarGuard::remove("SYNAPS_PROJECT_ROOT");
    let mut rt = crate::Runtime::new_headless();
    rt.set_cwd(Some(project(env.path(), "client-project")));
    rt.apply_memory_backend_config(&MemoryBackendConfig::default());
    let binding = rt.memory_backend.clone();
    assert_ne!(binding.scope().unwrap().key(), process_scope_key());
    let (head, id) = rolled_over(&binding).await;
    assert!(!in_process_store(&id), "not in the daemon's own cwd scope");
    rt.reset_context_continuation("daemon-session-e2", &head);
    let s = rt.continuation.lock().unwrap();
    assert_eq!(s.window, 2, "the window comes from the session's scope");
    assert_eq!(s.latest_archive.as_deref(), Some(id.as_str()));
}

/// Gate: a CLI session's binding (no session cwd: the process cwd, as
/// `foreground_runtime` gives) is the process's store, the one a CLI has
/// always sealed into and restored from; it restores as before.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn e3_a_cli_session_restores_from_the_process_scope_as_before() {
    let _env = BaseDirGuard::new();
    let mut rt = crate::Runtime::new_headless();
    rt.apply_memory_backend_config(&MemoryBackendConfig::default());
    let binding = rt.memory_backend.clone();
    assert_eq!(binding.base(), agent_core::config::base_dir().as_path());
    assert_eq!(binding.scope().unwrap().key(), process_scope_key());
    let (head, id) = rolled_over(&MemoryBinding::legacy_current()).await;
    assert!(in_process_store(&id), "sealed into the process's store");
    rt.reset_context_continuation("cli-session-e3", &head);
    let s = rt.continuation.lock().unwrap();
    assert_eq!(s.window, 2, "a CLI session restores as before");
    assert_eq!(s.latest_archive.as_deref(), Some(id.as_str()));
}

/// No cross-scope read: an archive that only the process's store holds does
/// not restore a session bound elsewhere.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn e4_an_archive_only_in_the_process_scope_restores_nothing() {
    let _env = BaseDirGuard::new();
    let (head, id) = rolled_over(&MemoryBinding::legacy_current()).await;
    assert!(in_process_store(&id), "sealed into the process's store");
    let store = tempfile::tempdir().unwrap();
    let mut rt = crate::Runtime::new_headless();
    rt.inherit_memory_backend(foreign(store.path()));
    rt.reset_context_continuation("worker-run:e4", &head);
    let s = rt.continuation.lock().unwrap();
    assert_eq!(s.window, 1, "the process's store is not this binding's");
    assert!(s.latest_archive.is_none());
}

/// The stream's pre-inference barrier, on a state no reset has bound yet,
/// restores a legacy binding's window from that binding's store.
#[tokio::test]
#[serial_test::serial(synaps_base_dir)]
async fn e5_the_stream_barrier_restores_from_the_run_binding() {
    let _env = BaseDirGuard::new();
    let store = tempfile::tempdir().unwrap();
    let binding = foreign(store.path());
    let (head, id) = rolled_over(&binding).await;
    assert!(!in_process_store(&id), "sealed outside the process's store");
    let state: SharedContinuation = Arc::new(Mutex::new(ContinuationState::default()));
    assert_eq!(
        validate_restored_history(&head, &state, &binding)
            .await
            .unwrap(),
        None
    );
    let s = state.lock().unwrap();
    assert!(s.initialized);
    assert_eq!(s.window, 2, "the barrier restores from the run's binding");
    assert_eq!(s.latest_archive.as_deref(), Some(id.as_str()));
}
