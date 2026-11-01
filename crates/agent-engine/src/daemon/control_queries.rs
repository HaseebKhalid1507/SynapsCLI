//! Control queries answered without a session (and on an attached
//! connection): they read daemon-held state only and never wait on a
//! session actor or a turn.

use crate::model_picker::{builtin_favorites, held_catalog_rows, plugin_favorites, FavoritesInputs, FavoritesView};
use crate::proc_identity::{BootKey, ProcessIdentity};
use crate::runtime::openai::catalog::validation::{default_level_for_model, thinking_options_with_basis, OptionsBasis};
use crate::session::children;
use crate::session::wire::{ChildListReply, DaemonFrame, SandboxStatusReply, ThinkingOptionsWire};
use crate::session::SessionId;
use crate::EngineHost;

/// Feature name for the `children` query (and the `tool_child` events).
pub const FEATURE_CHILDREN: &str = "children";
/// Feature name for the `thinking_options` query.
pub const FEATURE_THINKING_OPTIONS: &str = "thinking_options";
/// Feature name for the `model_favorites` query.
pub const FEATURE_MODEL_FAVORITES: &str = "model_favorites";
/// Feature name for the `sandbox_status` query.
pub const FEATURE_SANDBOX_STATUS: &str = "sandbox_status";
/// Feature name for the `skills_reload` and `skills_status` frames.
pub const FEATURE_SKILLS_V1: &str = "skills_v1";
/// How long the `model_favorites` query waits for the extension manager (a
/// discovery walk holds it) before it answers without plugin rows. This
/// bounds the plugin step only, not the whole answer: the file reads before
/// it wait for a place among [`MODEL_FAVORITES_READS`], and a client bounds
/// its own wait for the reply.
pub const MODEL_FAVORITES_MANAGER_WAIT: std::time::Duration = std::time::Duration::from_secs(1);
/// At most this many `model_favorites` file reads (the config, the stored
/// logins) run at once in the daemon. A read keeps its place until it
/// really ends: a blocking read cannot be cancelled, so a client that
/// leaves never frees its place early.
pub const MODEL_FAVORITES_READS: usize = 2;
/// Longest model id the `thinking_options` query answers.
pub const MAX_MODEL_ID_BYTES: usize = 256;

/// What this daemon lists in `Welcome.features`.
pub fn daemon_features() -> Vec<String> {
    let mut features = Vec::new();
    if children::supported() {
        features.push(FEATURE_CHILDREN.to_string());
    }
    features.push(FEATURE_THINKING_OPTIONS.to_string());
    features.push(FEATURE_MODEL_FAVORITES.to_string());
    features.push(FEATURE_SANDBOX_STATUS.to_string());
    features.push("ephemeral_layer_v1".to_string());
    features.push("memory_only_results_v1".to_string());
    features.push(crate::session::client_tools::FEATURE.to_string());
    features
}

/// The `skills_reload` reply: the new pass's report, or a session-less
/// `Error` when the walk failed (the last pass stays published).
pub async fn skills_reload_answer(host: &EngineHost, id: Option<u64>) -> DaemonFrame {
    match host.reload_skills().await {
        Ok(report) => DaemonFrame::SkillsResult { id, report },
        Err(message) => DaemonFrame::Error {
            session_id: None,
            message,
        },
    }
}

/// The `sandbox_status` reply for a [`crate::sandbox::Status`].
pub fn sandbox_status_reply(id: Option<u64>, session: String, s: crate::sandbox::Status) -> DaemonFrame {
    DaemonFrame::SandboxStatusResult(SandboxStatusReply {
        id,
        session,
        state: s.state.to_string(),
        kill_switch: s.kill_switch,
        tools: s.tools.to_string(),
        policy_hash: s.policy_hash,
        rules: s.rules,
        skipped: s.skipped,
        abi: s.abi,
    })
}

/// Answer `SandboxStatus`: the policy file and the kill switch read on a
/// blocking thread, against this daemon's own base dir, with the same read
/// and validation as a spawn ([`crate::sandbox::status`]). If the thread
/// can't run (the runtime is shutting down), the answer says `invalid`:
/// nothing it couldn't vouch for.
pub async fn sandbox_status_answer(id: Option<u64>, session: String) -> DaemonFrame {
    let asked = session.clone();
    let status = tokio::task::spawn_blocking(move || crate::sandbox::status(&asked)).await.unwrap_or(crate::sandbox::Status {
        state: "invalid",
        kill_switch: false,
        tools: "open",
        policy_hash: None,
        rules: 0,
        skipped: 0,
        abi: None,
    });
    sandbox_status_reply(id, session, status)
}

/// Answer `Children`. Each listed running leader is checked now, on a
/// blocking thread: one found gone, reused or a zombie is ended in the
/// ledger before the reply says anything. With `members`, one shared
/// `/proc` walk serves concurrent queries. Any query may also start a lazy
/// sweep (at most once per minute).
pub async fn children_reply(
    id: Option<u64>,
    session_id: Option<SessionId>,
    sid: Option<u32>,
    members: bool,
) -> DaemonFrame {
    if !children::supported() {
        return DaemonFrame::ChildList(ChildListReply {
            id,
            supported: false,
            boot_id: None,
            clk_tck: ProcessIdentity::clk_tck(),
            sessions: Vec::new(),
            host: Vec::new(),
            refused_records: 0,
            ledger_full: false,
            host_instance: None,
        });
    }
    children::maybe_sweep();
    let scan = if members { Some(children::shared_scan().await) } else { None };
    let session = session_id.as_ref().map(|s| s.as_str().to_string());
    let checked = tokio::task::spawn_blocking(move || {
        children::list_checked(session.as_deref(), sid, scan.as_ref().map(|s| s.as_slice()))
    })
    .await;
    // The check can only fail if the runtime is shutting down; then the
    // reply holds nothing it could not vouch for.
    let listing = checked.unwrap_or_default();
    DaemonFrame::ChildList(ChildListReply {
        id,
        supported: true,
        boot_id: ProcessIdentity::boot_key().and_then(BootKey::linux_boot_id).map(str::to_string),
        clk_tck: ProcessIdentity::clk_tck(),
        sessions: listing.sessions,
        host: listing.host,
        refused_records: listing.refused_records,
        ledger_full: listing.ledger_full,
        host_instance: ProcessIdentity::boot_key().cloned(),
    })
}

/// Non-empty, at most [`MAX_MODEL_ID_BYTES`], no whitespace or control
/// characters. Anything else gets `options: null`.
pub fn well_formed_model_id(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= MAX_MODEL_ID_BYTES
        && !model.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// The answer for one model id: the settings-UI list, its basis and the
/// model-switch default. Reads the process-local capability cache only.
pub fn thinking_options_for(model: &str) -> Option<ThinkingOptionsWire> {
    if !well_formed_model_id(model) {
        return None;
    }
    let (levels, basis) = thinking_options_with_basis(model);
    Some(ThinkingOptionsWire {
        levels,
        exact: basis != OptionsBasis::Fallback,
        basis,
        default_level: default_level_for_model(model).map(|l| l.as_str().to_string()),
    })
}

pub fn thinking_options_reply(id: Option<u64>, model: String) -> DaemonFrame {
    let options = thinking_options_for(&model);
    DaemonFrame::ThinkingOptionsResult { id, model, options }
}

/// The `thinking_options` answer. With `warm`, first the existing bounded
/// Muse warm-up (8 s, one attempt per 300 s per daemon) for a well-formed
/// `muse/` id with no live row, through `broker` (the host's own; no
/// credential crosses the wire). Without `warm`, no network call.
pub async fn thinking_options_answer(
    id: Option<u64>,
    model: String,
    warm: bool,
    broker: impl FnOnce() -> std::sync::Arc<dyn crate::auth::CredentialBroker>,
) -> DaemonFrame {
    if warm && well_formed_model_id(&model) && model.starts_with("muse/") {
        crate::runtime::openai::catalog::ensure_muse_catalog_for(&model, &broker()).await;
    }
    thinking_options_reply(id, model)
}

/// Answer `ModelFavorites` from this host: the config and broker reads on a
/// blocking thread (one of [`MODEL_FAVORITES_READS`]), the live catalogs
/// this process already holds, and the plugin rows under a read of the
/// extension manager that waits at most [`MODEL_FAVORITES_MANAGER_WAIT`]
/// and is taken last, after every file read. Reads only: no catalog fetch,
/// no token refresh. Its caller owns the task ([`OneAtATime`]).
pub async fn model_favorites_answer(id: Option<u64>, host: &EngineHost) -> DaemonFrame {
    let inputs = match read_favorites_inputs(favorites_reads(), FavoritesInputs::read).await {
        Ok(inputs) => inputs,
        Err(e) => return DaemonFrame::Error { session_id: None, message: format!("model favorites: {e}") },
    };
    model_favorites_from(
        id,
        &inputs,
        &held_catalog_rows(),
        host.ext_manager(),
        || host.extensions_settled(),
        MODEL_FAVORITES_MANAGER_WAIT,
    )
    .await
}

/// The daemon's places for `model_favorites` file reads.
fn favorites_reads() -> &'static std::sync::Arc<tokio::sync::Semaphore> {
    static READS: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    READS.get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(MODEL_FAVORITES_READS)))
}

/// Run `read` on a blocking thread once a place of `reads` is free. The
/// place moves into the blocking job and is given back when `read` returns,
/// also when the caller is gone by then. Waiting for a place ends with the
/// caller; the read itself cannot be cancelled.
pub(crate) async fn read_favorites_inputs(
    reads: &std::sync::Arc<tokio::sync::Semaphore>,
    read: impl FnOnce() -> FavoritesInputs + Send + 'static,
) -> Result<FavoritesInputs, String> {
    let place = reads.clone().acquire_owned().await.map_err(|e| e.to_string())?;
    tokio::task::spawn_blocking(move || {
        let _place = place;
        read()
    })
    .await
    .map_err(|e| e.to_string())
}

/// A connection's `model_favorites` answer: one at a time, owned by the
/// connection. The connection cancels it on every way it ends, before it
/// waits on its writer; dropping this aborts one still running as well. So
/// nothing outlives the connection but a file read already under way,
/// which keeps its place until it ends.
#[derive(Default)]
pub(crate) struct OneAtATime(Option<tokio::task::JoinHandle<()>>);

impl OneAtATime {
    /// Start `answer`, unless the last one is still running: false then,
    /// and nothing is started.
    pub(crate) fn start(&mut self, answer: impl std::future::Future<Output = ()> + Send + 'static) -> bool {
        if self.0.as_ref().is_some_and(|task| !task.is_finished()) {
            return false;
        }
        self.0 = Some(tokio::spawn(answer));
        true
    }

    /// End the answer still running, if any, and wait until it has gone,
    /// so its copy of the connection's sender is dropped when this returns.
    /// A file read already under way keeps its place until it ends.
    pub(crate) async fn cancel(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for OneAtATime {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

/// [`model_favorites_answer`] with its inputs given. `settled`: no
/// discovery is running (asked before the manager read; a manager whose
/// discovery is done counts as settled too).
pub(crate) async fn model_favorites_from(
    id: Option<u64>,
    inputs: &FavoritesInputs,
    held: &std::collections::BTreeMap<String, crate::model_picker::HeldCatalogRows>,
    manager: &tokio::sync::RwLock<crate::extensions::manager::ExtensionManager>,
    settled: impl FnOnce() -> bool,
    wait: std::time::Duration,
) -> DaemonFrame {
    let mut view = builtin_favorites(inputs, held);
    let settled = settled();
    let plugins_loaded = match tokio::time::timeout(wait, manager.read()).await {
        Ok(manager) => {
            let rows = plugin_favorites(&inputs.favorites, &manager);
            let done = manager.discovery_done().is_some();
            drop(manager);
            view.models.extend(rows);
            settled || done
        }
        Err(_) => {
            tracing::warn!(
                wait_ms = wait.as_millis() as u64,
                "model favorites: the extension manager stayed busy; replying without plugin rows"
            );
            false
        }
    };
    model_favorites_reply(id, view, plugins_loaded)
}

/// The `ModelFavoritesResult` frame for a view.
pub fn model_favorites_reply(id: Option<u64>, view: FavoritesView, plugins_loaded: bool) -> DaemonFrame {
    DaemonFrame::ModelFavoritesResult {
        id,
        models: view.models,
        plugins_loaded,
        live_providers: view.live_providers,
    }
}

/// The host's credential broker (the one sessions use).
pub fn host_broker(host: &EngineHost) -> std::sync::Arc<dyn crate::auth::CredentialBroker> {
    let parts = host.parts();
    crate::auth::broker_from_source(&parts.credential_source, &parts.token_cache, parts.client.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::openai::catalog::muse_warmup_tests::{broker, CATALOG};
    use crate::runtime::openai::catalog::{capability_cache, muse, reset_muse_warmup_backoff};

    fn favorites_ids(frame: &DaemonFrame) -> (Option<u64>, Vec<String>, bool) {
        match frame {
            DaemonFrame::ModelFavoritesResult { id, models, plugins_loaded, .. } => {
                (*id, models.iter().map(|m| m.id.clone()).collect(), *plugins_loaded)
            }
            other => panic!("{other:?}"),
        }
    }

    /// The reply: the built-in favorites, then the plugin ones; the id is
    /// echoed; the plugin rows are final once discovery is settled.
    #[tokio::test]
    async fn model_favorites_append_plugin_rows_after_the_builtin_ones() {
        let manager = tokio::sync::RwLock::new(crate::model_picker::tests::manager());
        let inputs = crate::model_picker::tests::inputs(&["plug:prov:m1", "local/qwen3:8b"], &[], &[]);
        let none = std::collections::BTreeMap::new();
        let wait = std::time::Duration::from_millis(500);
        let frame = model_favorites_from(Some(4), &inputs, &none, &manager, || true, wait).await;
        assert_eq!(favorites_ids(&frame), (Some(4), vec!["local/qwen3:8b".into(), "plug:prov:m1".into()], true));
        // Discovery still running: the rows so far, marked not final.
        let frame = model_favorites_from(None, &inputs, &none, &manager, || false, wait).await;
        assert_eq!(favorites_ids(&frame), (None, vec!["local/qwen3:8b".into(), "plug:prov:m1".into()], false));
    }

    /// A manager that stays write-locked (a discovery walk) costs the query
    /// the bound and the plugin rows, never the answer.
    #[tokio::test]
    async fn a_busy_extension_manager_bounds_the_wait_and_drops_only_the_plugin_rows() {
        let manager = tokio::sync::RwLock::new(crate::model_picker::tests::manager());
        let held = manager.write().await;
        let inputs = crate::model_picker::tests::inputs(&["plug:prov:m1", "local/qwen3:8b"], &[], &[]);
        let none = std::collections::BTreeMap::new();
        let started = std::time::Instant::now();
        let frame = model_favorites_from(Some(5), &inputs, &none, &manager, || true, std::time::Duration::from_millis(100)).await;
        let took = started.elapsed();
        assert!(took >= std::time::Duration::from_millis(100) && took < std::time::Duration::from_secs(5), "{took:?}");
        assert_eq!(favorites_ids(&frame), (Some(5), vec!["local/qwen3:8b".into()], false));
        drop(held);
    }

    /// A gate the injected reads block on until the test opens it.
    #[derive(Default)]
    struct Gate(std::sync::Mutex<bool>, std::sync::Condvar);

    impl Gate {
        fn wait(&self) {
            let mut open = self.0.lock().unwrap();
            while !*open {
                open = self.1.wait(open).unwrap();
            }
        }
        fn open(&self) {
            *self.0.lock().unwrap() = true;
            self.1.notify_all();
        }
    }

    async fn until(what: &str, done: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "never: {what}");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// A flood of `model_favorites` frames on three connections, with the
    /// file reads blocked, then every connection gone: one answer per
    /// connection, at most two reads, the waiting answer never reads, the
    /// running reads keep their places until they end, and no reply is
    /// made after a disconnect.
    #[tokio::test]
    async fn a_flood_and_a_disconnect_keep_the_favorites_work_bounded() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        use std::sync::Arc;
        let reads = Arc::new(tokio::sync::Semaphore::new(2));
        let (gate, started, replied) = (Arc::new(Gate::default()), Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let answer = || {
            let (reads, gate, started, replied) = (reads.clone(), gate.clone(), started.clone(), replied.clone());
            async move {
                let read = move || {
                    started.fetch_add(1, SeqCst);
                    gate.wait();
                    FavoritesInputs::default()
                };
                if read_favorites_inputs(&reads, read).await.is_ok() {
                    replied.fetch_add(1, SeqCst);
                }
            }
        };

        let mut connections: Vec<OneAtATime> = (0..3).map(|_| OneAtATime::default()).collect();
        let accepted: usize = connections
            .iter_mut()
            .map(|connection| (0..10).filter(|_| connection.start(answer())).count())
            .sum();
        assert_eq!(accepted, 3, "one answer per connection; the other 27 frames start nothing");
        until("two reads under way", || started.load(SeqCst) == 2).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!((started.load(SeqCst), reads.available_permits()), (2, 0), "the third answer waits for a place");

        drop(connections);
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(reads.available_permits(), 0, "a read under way keeps its place after its client left");
        gate.open();
        until("both places back", || reads.available_permits() == 2).await;
        assert_eq!(started.load(SeqCst), 2, "the answer that was waiting never read");
        assert_eq!(replied.load(SeqCst), 0, "no reply after a disconnect");

        // A new connection is served, then may ask again.
        let mut connection = OneAtATime::default();
        assert!(connection.start(answer()));
        until("the new answer", || replied.load(SeqCst) == 1).await;
        until("the slot frees", || connection.0.as_ref().is_some_and(|task| task.is_finished())).await;
        assert!(connection.start(answer()));
        until("the next answer", || replied.load(SeqCst) == 2).await;
        assert_eq!(started.load(SeqCst), 4);
    }

    /// How a connection ends: `cancel` returns only once the running
    /// answer is gone, so when the connection then drops its own sender the
    /// channel its writer drains is closed at once, with no reply in it,
    /// while the read under way is still blocked; that read keeps its place
    /// until it ends. Nothing running: `cancel` does nothing.
    #[tokio::test]
    async fn cancel_ends_the_answer_before_the_connection_waits_on_its_writer() {
        use std::sync::Arc;
        use tokio::sync::mpsc::error::TryRecvError;
        let reads = Arc::new(tokio::sync::Semaphore::new(1));
        let gate = Arc::new(Gate::default());
        let (tx, mut rx) = tokio::sync::mpsc::channel::<DaemonFrame>(4);
        let mut favorites = OneAtATime::default();
        let (task_reads, task_gate, task_tx) = (reads.clone(), gate.clone(), tx.clone());
        assert!(favorites.start(async move {
            let read = move || {
                task_gate.wait();
                FavoritesInputs::default()
            };
            if read_favorites_inputs(&task_reads, read).await.is_ok() {
                let _ = task_tx.send(model_favorites_reply(Some(9), FavoritesView::default(), true)).await;
            }
        }));
        until("the read is under way", || reads.available_permits() == 0).await;

        favorites.cancel().await;
        drop(tx);
        let next = rx.try_recv();
        assert!(matches!(next, Err(TryRecvError::Disconnected)), "the answer's sender outlived cancel: {next:?}");
        assert_eq!(reads.available_permits(), 0, "the read under way keeps its place");
        gate.open();
        until("the place comes back", || reads.available_permits() == 1).await;

        favorites.cancel().await;
        assert!(favorites.start(async {}), "the slot is free again");
    }

    #[test]
    fn the_daemon_lists_the_model_favorites_feature() {
        assert!(daemon_features().iter().any(|f| f == FEATURE_MODEL_FAVORITES));
        let frame = model_favorites_reply(Some(1), FavoritesView::default(), true);
        assert_eq!(
            serde_json::to_value(frame).unwrap(),
            serde_json::json!({"type": "model_favorites_result", "id": 1, "models": [], "plugins_loaded": true, "live_providers": []})
        );
    }

    fn options(frame: DaemonFrame) -> (String, Option<ThinkingOptionsWire>) {
        match frame {
            DaemonFrame::ThinkingOptionsResult { model, options, .. } => (model, options),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn malformed_ids_get_no_options_and_any_other_id_an_answer() {
        let long = format!("x/{}", "a".repeat(MAX_MODEL_ID_BYTES - 2));
        assert_eq!(long.len(), MAX_MODEL_ID_BYTES);
        for bad in ["", "muse/a b", "a\tb", "a\u{7}b", "a\u{a0}b", &format!("{long}z")] {
            assert!(thinking_options_for(bad).is_none(), "{bad:?}");
        }
        let unknown = thinking_options_for(&long).expect("well formed");
        assert_eq!((unknown.basis, unknown.exact, unknown.default_level), (OptionsBasis::Fallback, false, None));
        let xai = thinking_options_for("xai-auth/grok-4.5").unwrap();
        assert_eq!((xai.basis, xai.exact, xai.default_level.as_deref()), (OptionsBasis::Static, true, Some("high")));
        assert_eq!(xai.levels, crate::runtime::openai::catalog::validation::thinking_options_for_model("xai-auth/grok-4.5"));
    }

    // The guard serializes the process-global Muse cache across tests; this
    // test's runtime is current-thread, so nothing else here waits on it.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn warm_is_opt_in_and_goes_through_the_given_broker_once() {
        let _guard = muse::test_cache_guard();
        capability_cache::replace_provider("muse", &[]);
        reset_muse_warmup_backoff();
        let model = "muse/muse-warmup-probe-zz7".to_string();
        let b = broker(Ok(CATALOG));
        let dyn_b = || -> std::sync::Arc<dyn crate::auth::CredentialBroker> { b.clone() };

        let (echo, cold) = options(thinking_options_answer(Some(1), model.clone(), false, dyn_b).await);
        assert_eq!(echo, model);
        assert_eq!(b.seen.lock().unwrap().len(), 0, "warm:false makes no call");
        assert_eq!((cold.unwrap().basis, b.seen.lock().unwrap().len()), (OptionsBasis::Fallback, 0));

        let (_, warm) = options(thinking_options_answer(Some(2), model.clone(), true, dyn_b).await);
        let warm = warm.unwrap();
        assert_eq!(b.seen.lock().unwrap().len(), 1);
        assert_eq!((warm.basis, warm.exact, warm.levels.clone()), (OptionsBasis::Live, true, vec!["adaptive".to_string(), "high".to_string()]));

        // A live row now exists: a second warm makes no call.
        let _ = thinking_options_answer(Some(3), model, true, dyn_b).await;
        assert_eq!(b.seen.lock().unwrap().len(), 1);
        // Not a muse id: warm is a no-op.
        let _ = thinking_options_answer(Some(4), "xai-auth/grok-4.5".into(), true, dyn_b).await;
        assert_eq!(b.seen.lock().unwrap().len(), 1);
        capability_cache::replace_provider("muse", &[]);
        reset_muse_warmup_backoff();
    }

    /// An extension process is in the host list from its spawn, labelled
    /// with its id, and leaves the reply as soon as it is gone.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_host_list_names_a_live_extension_process_only() {
        use crate::extensions::runtime::ExtensionHandler;
        const ID: &str = "host-list-fixture";
        let host_of = |frame: DaemonFrame| match frame {
            DaemonFrame::ChildList(reply) => reply.host.into_iter().filter(|h| h.owner_id == ID).collect::<Vec<_>>(),
            other => panic!("{other:?}"),
        };
        let ext = crate::extensions::runtime::process::ProcessExtension::spawn(ID, "/bin/cat", &[]).await.expect("spawn /bin/cat");
        let listed = host_of(children_reply(Some(1), None, None, false).await);
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].owner_kind, children::HostOwnerKind::Extension);
        let pid = listed[0].child.pid;
        assert_eq!(children::identify(pid).as_ref(), Some(&listed[0].child), "the spawned process itself");
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap();
        assert!(cmdline.starts_with(b"/bin/cat"), "{cmdline:?}");

        ext.shutdown().await;
        let t0 = std::time::Instant::now();
        while !host_of(children_reply(Some(2), None, None, false).await).is_empty() {
            assert!(t0.elapsed() < std::time::Duration::from_secs(10), "still listed after shutdown");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// The reply reads every listed leader now, and carries the ledger's
    /// refusal count. The records name pids that never exist, in this
    /// process's own (live) sid, so a sweep may end them but never drops them.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_reply_checks_each_leader_now_and_counts_refusals() {
        let Some(boot) = ProcessIdentity::boot_key().map(|boot| boot.as_str()) else { return };
        let sid = ProcessIdentity::identify(std::process::id()).expect("own stat").sid;
        let session = format!("cq-fresh-{}", std::process::id());
        let before = {
            let mut ledger = children::ledger().lock().unwrap();
            let before = ledger.list(None, None).refused_records;
            for i in 0..children::MAX_PER_SESSION as u32 + 1 {
                let pid = u32::MAX - 1 - i; // above any pid_max
                let instance = crate::proc_identity::InstanceKey::Linux { boot_id: boot.to_string(), start_ticks: 1 };
                let child = children::ChildIdentity::new(pid, sid, sid, instance);
                let spawn = children::ChildSpawn {
                    tool_id: format!("t{i}"),
                    tool_name: "bash".into(),
                    handle_id: None,
                    shell_session_id: None,
                    child,
                };
                assert_eq!(ledger.insert(&session, spawn), i < children::MAX_PER_SESSION as u32);
            }
            before
        };
        let DaemonFrame::ChildList(reply) = children_reply(Some(3), Some(SessionId::from(session.as_str())), None, false).await
        else {
            panic!("not a child_list")
        };
        assert!(reply.refused_records > before, "{} > {before}", reply.refused_records);
        assert!(!reply.ledger_full);
        let host = crate::proc_identity::BootKey::Linux { boot_id: boot.to_string() };
        assert_eq!(reply.host_instance, Some(host), "the host's boot, tagged");
        let group = &reply.sessions[0];
        assert!(group.truncated);
        assert_eq!(group.children.len(), children::MAX_PER_SESSION);
        assert!(group.children.iter().all(|c| c.state == children::ChildState::Ended), "no leader runs");
        let stored = children::list(Some(&session), None);
        assert!(stored.sessions[0].children.iter().all(|c| c.state == children::ChildState::Ended), "the ledger agrees");
    }

    /// macOS, live: a short-lived child in its own session is listed with
    /// its darwin instance, running, as a member of its own sid, and the
    /// reply names the host's boot session; no Linux-only field is filled.
    /// With `--nocapture` it prints the reply as a client gets it.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_live_child_on_macos_lists_its_darwin_instance() {
        use crate::proc_identity::InstanceKey;
        use std::os::unix::process::CommandExt;
        assert!(children::supported(), "macOS identifies processes");
        let mut cmd = std::process::Command::new("/bin/sleep");
        cmd.arg("3")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // SAFETY: setsid is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = cmd.spawn().expect("spawn /bin/sleep");
        let pid = child.id();
        let identity = children::identify(pid).expect("readable while it runs");
        let session = format!("cq-darwin-{pid}");
        let spawn = children::ChildSpawn {
            tool_id: "toolu_darwin".into(),
            tool_name: "bash".into(),
            handle_id: None,
            shell_session_id: None,
            child: identity.clone(),
        };
        assert!(children::record_spawn(&session, spawn));
        let frame = children_reply(Some(9), Some(SessionId::from(session.as_str())), None, true).await;
        // SAFETY: kill(2) on the child's own process group.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
        let _ = child.wait();
        children::record_end(&identity, None, Some(libc::SIGKILL));
        let DaemonFrame::ChildList(reply) = frame else { panic!("not a child_list") };
        let text = serde_json::to_string(&reply).unwrap();
        println!("child_list reply on macOS: {text}");

        assert!(reply.supported);
        let host = reply.host_instance.clone().expect("the host's boot");
        assert!(matches!(&host, BootKey::Darwin { boot_session } if !boot_session.is_empty()), "{host:?}");
        assert_eq!(reply.boot_id, None, "no Linux boot id");
        let entry = &reply.sessions[0].children[0];
        assert_eq!((entry.child.pid, entry.child.sid, entry.child.pgid), (pid, pid, pid));
        assert!(matches!(entry.child.instance, InstanceKey::Darwin { unique_id, .. } if unique_id > 0), "{entry:?}");
        assert_eq!(entry.child.instance.boot(), host, "in the host's boot");
        assert_eq!(entry.child, identity, "the child as identified");
        assert_eq!(entry.state, children::ChildState::Running);
        let members = entry.members.as_ref().expect("members asked for");
        let leader = members.iter().find(|m| m.pid == pid).expect("the leader is a member of its sid");
        assert_eq!(leader.instance.as_ref(), Some(&entry.child.instance));
        let json = serde_json::to_value(&reply).unwrap();
        assert_eq!(json["host_instance"]["kind"], "darwin", "{json}");
        let child_json = &json["sessions"][0]["children"][0];
        assert_eq!(child_json["child"]["instance"]["kind"], "darwin", "{json}");
        for field in ["start_ticks", "boot_id"] {
            assert!(child_json["child"].get(field).is_none(), "no {field}: {json}");
        }
        assert!(child_json["members"].as_array().unwrap().iter().all(|m| m.get("start_ticks").is_none()), "{json}");
    }
}
