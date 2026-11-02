//! `EngineHost::create_session` under concurrent creates: creates of
//! different sessions build their actors at the same time; creates of one
//! session build ONE actor and the rest attach to it; a build that fails or
//! panics lets the next create of that session build; a restore lane's cap
//! (`CREATE_PROBE.locked`) starts only once its create may build.
//!
//! A real host on a scratch HOME, no daemon. The build itself is the real
//! `SessionActor::create`; the test seam `CREATE_BUILD_HOOK` runs just before
//! it, to hold a build open, fail it or panic in it.

use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_engine::host::{CreateBuildHook, CreateProbe, CREATE_BUILD_HOOK, CREATE_PROBE};
use agent_engine::session::{ContinueLookup, SessionConfig, SessionHandle};
use agent_engine::{EngineHost, HostOpts};
use futures::future::BoxFuture;
use serial_test::serial;
use tokio::task::JoinHandle;

const MODEL: &str = "claude-sonnet-4-5";

struct Home {
    _dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path().join(".synaps-cli");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("config"), "").unwrap();
        std::fs::write(
            base.join("auth.json"),
            "{\"anthropic\": {\"type\": \"oauth\", \"refresh\": \"r\", \"access\": \"synthetic-token\", \"expires\": 9999999999999}}",
        )
        .unwrap();
        for k in ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GOOGLE_API_KEY"] {
            std::env::remove_var(k);
        }
        // The session registry (cards + per-session sockets) goes under the
        // scratch base dir, never an ambient runtime dir.
        std::env::remove_var("SYNAPS_RUNTIME_DIR");
        std::env::set_var("HOME", dir.path());
        std::env::set_var("SYNAPS_BASE_DIR", &base);
        Self { _dir: dir }
    }
}

async fn host() -> Arc<EngineHost> {
    EngineHost::boot(HostOpts {
        profile: None,
        no_extensions: true,
        mcp_discovery: agent_engine::McpDiscovery::Inline,
    })
    .await
    .unwrap()
}

/// A session saved on disk (no actor), optionally named. The name is set on
/// the record as is, past `set_name`'s checks (a decoy may take any name).
async fn saved_session(name: Option<&str>) -> String {
    let mut s = agent_engine::core::session::Session::new(MODEL, "medium", None);
    s.name = name.map(str::to_string);
    s.save().await.unwrap();
    s.id
}

/// `--continue <query>`, as a client's or a restore lane's create sends it.
fn cont(query: &str) -> SessionConfig {
    SessionConfig {
        continue_session: Some(Some(query.to_string())),
        model_override: Some(MODEL.into()),
        await_extensions: false,
        persist: false,
        ..SessionConfig::default()
    }
}

/// A restore lane's create: the exact journal id the daemon recorded.
fn exact(id: &str) -> SessionConfig {
    SessionConfig {
        continue_lookup: ContinueLookup::ExactJournalId,
        ..cont(id)
    }
}

fn hook<F>(f: F) -> CreateBuildHook
where
    F: Fn() -> BoxFuture<'static, Result<(), String>> + Send + Sync + 'static,
{
    Arc::new(f)
}

/// A build hook that lets the build go on at once.
fn go() -> CreateBuildHook {
    hook(|| Box::pin(async { Ok(()) }))
}

/// One create in its own task, with its probe and build hook in scope.
fn spawn_create(
    host: &Arc<EngineHost>,
    cfg: SessionConfig,
    build: CreateBuildHook,
    probe: Arc<CreateProbe>,
) -> JoinHandle<Result<SessionHandle, String>> {
    let host = Arc::clone(host);
    tokio::spawn(CREATE_PROBE.scope(
        probe,
        CREATE_BUILD_HOOK.scope(build, async move {
            host.create_session(cfg).await.map_err(|e| e.to_string())
        }),
    ))
}

async fn joined(t: JoinHandle<Result<SessionHandle, String>>, what: &str) -> SessionHandle {
    tokio::time::timeout(Duration::from_secs(30), t)
        .await
        .unwrap_or_else(|_| panic!("{what}: the create did not finish in 30 s"))
        .unwrap_or_else(|e| panic!("{what}: the create task failed: {e}"))
        .unwrap_or_else(|e| panic!("{what}: the create failed: {e}"))
}

fn live_with_id(host: &EngineHost, id: &str) -> usize {
    host.sessions().iter().filter(|m| m.id.as_str() == id).count()
}

/// Builds in flight: each one waits (up to `alone`) until `want` builds have
/// been in flight at the same time, so two builds that can overlap do.
#[derive(Default)]
struct InFlight {
    now: AtomicUsize,
    peak: AtomicUsize,
    builds: AtomicUsize,
    changed: tokio::sync::Notify,
}

impl InFlight {
    async fn build(&self, want: usize, alone: Duration) {
        self.builds.fetch_add(1, SeqCst);
        let n = self.now.fetch_add(1, SeqCst) + 1;
        self.peak.fetch_max(n, SeqCst);
        self.changed.notify_waiters();
        let deadline = tokio::time::Instant::now() + alone;
        loop {
            let changed = self.changed.notified();
            if self.peak.load(SeqCst) >= want {
                break;
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                break;
            }
        }
        self.now.fetch_sub(1, SeqCst);
        self.changed.notify_waiters();
    }

    fn hook(self: &Arc<Self>, want: usize, alone: Duration) -> CreateBuildHook {
        let me = Arc::clone(self);
        hook(move || {
            let me = Arc::clone(&me);
            Box::pin(async move {
                me.build(want, alone).await;
                Ok(())
            })
        })
    }
}

/// Two restore-style creates of two different sessions build at the same
/// time: neither waits for the other's build.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn creates_of_two_different_sessions_build_at_the_same_time() {
    let _home = Home::new();
    let host = host().await;
    let a = saved_session(None).await;
    let b = saved_session(None).await;
    assert_ne!(a, b);
    let flight = Arc::new(InFlight::default());
    let alone = Duration::from_secs(3);
    let ta = spawn_create(&host, cont(&a), flight.hook(2, alone), Arc::default());
    let tb = spawn_create(&host, cont(&b), flight.hook(2, alone), Arc::default());
    let ha = joined(ta, "create of a").await;
    let hb = joined(tb, "create of b").await;
    assert_eq!((ha.id.as_str(), hb.id.as_str()), (a.as_str(), b.as_str()));
    assert_eq!(flight.builds.load(SeqCst), 2);
    assert_eq!(
        flight.peak.load(SeqCst),
        2,
        "the creates of two different sessions never built at the same time: one waited for the other's build"
    );
    assert_eq!(live_with_id(&host, &a) + live_with_id(&host, &b), 2);
}

/// `n` creates of one session at once (2 and 8): ONE actor is built, every
/// other create attaches to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn creates_of_one_session_at_once_build_one_actor() {
    let _home = Home::new();
    let host = host().await;
    for n in [2usize, 8] {
        let s = saved_session(None).await;
        let builds = Arc::new(AtomicUsize::new(0));
        let slow = {
            let builds = Arc::clone(&builds);
            hook(move || {
                let builds = Arc::clone(&builds);
                Box::pin(async move {
                    builds.fetch_add(1, SeqCst);
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    Ok(())
                })
            })
        };
        let probes: Vec<Arc<CreateProbe>> = (0..n).map(|_| Arc::default()).collect();
        let tasks: Vec<_> = probes
            .iter()
            .map(|p| spawn_create(&host, cont(&s), slow.clone(), Arc::clone(p)))
            .collect();
        for (i, t) in tasks.into_iter().enumerate() {
            let h = joined(t, &format!("create {i} of {n}")).await;
            assert_eq!(h.id.as_str(), s, "create {i} of {n}");
        }
        assert_eq!(builds.load(SeqCst), 1, "{n} creates of one session built {} actors", builds.load(SeqCst));
        let attached = probes.iter().filter(|p| p.found_live.load(SeqCst)).count();
        assert_eq!(attached, n - 1, "{n} creates: {attached} attached to the live one");
        assert_eq!(live_with_id(&host, &s), 1);
    }
}

/// One session reached by its id and by its name at the same time: one
/// actor.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn creates_of_one_session_by_id_and_by_name_build_one_actor() {
    let _home = Home::new();
    let host = host().await;
    let s = saved_session(Some("reserve-by-name")).await;
    let builds = Arc::new(AtomicUsize::new(0));
    let slow = {
        let builds = Arc::clone(&builds);
        hook(move || {
            let builds = Arc::clone(&builds);
            Box::pin(async move {
                builds.fetch_add(1, SeqCst);
                tokio::time::sleep(Duration::from_millis(300)).await;
                Ok(())
            })
        })
    };
    let tasks: Vec<_> = (0..6)
        .map(|i| {
            let query = if i % 2 == 0 { s.as_str() } else { "reserve-by-name" };
            spawn_create(&host, cont(query), slow.clone(), Arc::default())
        })
        .collect();
    for (i, t) in tasks.into_iter().enumerate() {
        let h = joined(t, &format!("create {i}")).await;
        assert_eq!(h.id.as_str(), s, "create {i}");
    }
    assert_eq!(builds.load(SeqCst), 1, "by id and by name: {} actors built", builds.load(SeqCst));
    assert_eq!(live_with_id(&host, &s), 1);
}

/// A build that fails, or panics, while another create of the same session
/// waits for it: the waiting create then builds the session itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn a_build_that_fails_or_panics_lets_the_waiting_create_build() {
    let _home = Home::new();
    let host = host().await;
    for panics in [false, true] {
        let s = saved_session(None).await;
        let order: Arc<Mutex<Vec<&'static str>>> = Arc::default();
        let started = Arc::new(tokio::sync::Notify::new());
        let bad = {
            let (order, started) = (Arc::clone(&order), Arc::clone(&started));
            hook(move || {
                let (order, started) = (Arc::clone(&order), Arc::clone(&started));
                Box::pin(async move {
                    order.lock().unwrap().push("bad build starts");
                    started.notify_one();
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    order.lock().unwrap().push("bad build ends");
                    if panics {
                        panic!("injected build panic");
                    }
                    Err("injected build failure".to_string())
                })
            })
        };
        let good = {
            let order = Arc::clone(&order);
            hook(move || {
                let order = Arc::clone(&order);
                Box::pin(async move {
                    order.lock().unwrap().push("good build starts");
                    Ok(())
                })
            })
        };
        let t1 = spawn_create(&host, cont(&s), bad, Arc::default());
        tokio::time::timeout(Duration::from_secs(10), started.notified())
            .await
            .expect("the first create reached its build");
        let t2 = spawn_create(&host, cont(&s), good, Arc::default());
        let first = tokio::time::timeout(Duration::from_secs(30), t1).await.expect("first create ended");
        if panics {
            assert!(first.expect_err("the first create panicked").is_panic());
        } else {
            let err = first.expect("no panic").expect_err("the first create failed");
            assert!(err.contains("injected build failure"), "{err}");
        }
        let h = joined(t2, if panics { "after a panic" } else { "after a failure" }).await;
        assert_eq!(h.id.as_str(), s);
        assert_eq!(
            *order.lock().unwrap(),
            ["bad build starts", "bad build ends", "good build starts"],
            "panics={panics}: the waiting create built only after the first build ended"
        );
        assert_eq!(live_with_id(&host, &s), 1);
    }
}

/// A restore lane's cap counts from `CREATE_PROBE.locked`. A create of
/// another session gets there at once, while a build runs; a create of the
/// SAME session gets there only when that build is done (and then attaches).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn a_create_waiting_for_its_own_session_has_not_started_its_cap() {
    let _home = Home::new();
    let host = host().await;
    let s = saved_session(None).await;
    let t = saved_session(None).await;
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let held = {
        let (started, release) = (Arc::clone(&started), Arc::clone(&release));
        hook(move || {
            let (started, release) = (Arc::clone(&started), Arc::clone(&release));
            Box::pin(async move {
                started.notify_one();
                release.notified().await;
                Ok(())
            })
        })
    };
    let ta = spawn_create(&host, cont(&s), held, Arc::default());
    tokio::time::timeout(Duration::from_secs(10), started.notified())
        .await
        .expect("the first create reached its build");

    let same: Arc<CreateProbe> = Arc::default();
    let other: Arc<CreateProbe> = Arc::default();
    let tb = spawn_create(&host, cont(&s), go(), Arc::clone(&same));
    let tc = spawn_create(&host, cont(&t), go(), Arc::clone(&other));
    tokio::time::timeout(Duration::from_secs(5), other.locked.notified())
        .await
        .expect("a create of another session waited for the build of the first: its cap did not start");
    let hc = joined(tc, "create of the other session").await;
    assert_eq!(hc.id.as_str(), t);
    assert!(
        tokio::time::timeout(Duration::from_millis(300), same.locked.notified()).await.is_err(),
        "a create waiting for the build of its own session started its cap"
    );

    release.notify_one();
    let ha = joined(ta, "first create").await;
    let hb = joined(tb, "second create of the session").await;
    assert_eq!((ha.id.as_str(), hb.id.as_str()), (s.as_str(), s.as_str()));
    tokio::time::timeout(Duration::from_secs(1), same.locked.notified())
        .await
        .expect("the second create started its cap when it attached");
    assert!(same.found_live.load(SeqCst), "the second create attached to the first's actor");
    assert_eq!(live_with_id(&host, &s), 1);
}

/// A restore lane loads its own journal by its id, with no chain or name
/// scan: a session NAMED exactly like that id (which a user's `--continue
/// <id>` still resolves to first) is not what it loads.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn a_restore_lane_loads_its_own_journal_past_a_session_named_like_its_id() {
    let _home = Home::new();
    let host = host().await;
    let own = saved_session(None).await;
    let decoy = saved_session(Some(&own)).await;
    assert_ne!(own, decoy);
    // The user's order is unchanged: chain, then name, then id.
    let user = agent_engine::core::session::resolve_session(&own).unwrap();
    assert_eq!(user.id, decoy, "a user's --continue <id> finds the session named so first");

    let h = joined(spawn_create(&host, exact(&own), go(), Arc::default()), "restore lane").await;
    assert_eq!(
        h.id.as_str(),
        own,
        "the restore lane loaded the session named like its id ({decoy}), not its own journal"
    );
    assert_eq!(h.journal_id(), own);
    assert_eq!(live_with_id(&host, &decoy), 0, "the decoy was not loaded");
    assert_eq!(live_with_id(&host, &own), 1);
}

/// A restore lane whose journal is not on disk fails exactly as a user's
/// `--continue` of that id does, and no session is created.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn a_restore_lane_whose_journal_is_missing_fails_as_a_user_continue_does() {
    let _home = Home::new();
    let host = host().await;
    let kept = saved_session(None).await;
    let missing = "20250101-000000-dead";
    // (SessionHandle is not Debug, so no expect_err.)
    let user = match host.create_session(cont(missing)).await {
        Ok(_) => panic!("a user's continue of a missing id created a session"),
        Err(e) => e.to_string(),
    };
    let lane = match host.create_session(exact(missing)).await {
        Ok(_) => panic!("the restore lane of a missing id created a session"),
        Err(e) => e.to_string(),
    };
    assert_eq!(lane, user, "the same failure");
    assert!(lane.contains("Failed to load session '20250101-000000-dead'"), "{lane}");
    assert!(host.sessions().is_empty(), "no session was created");
    let dir = std::path::PathBuf::from(std::env::var("SYNAPS_BASE_DIR").unwrap()).join("sessions");
    let mut files: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".json"))
        .collect();
    files.sort();
    assert_eq!(files, [format!("{kept}.json")], "no session file was written");
}

/// Sixteen restore lanes of sixteen different sessions at once, with sixty
/// live session cards in the registry dir. Each build's endpoint start
/// scans every card while it holds the registry's file lock, which another
/// taker gives up on after 100 ms ("registry lock busy"): builds that run
/// at the same time in one host must not fail on it. Every lane builds its
/// own actor; none fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn sixteen_restore_lanes_at_once_all_register_past_sixty_live_cards() {
    use agent_engine::events::queue::EventQueue;
    use agent_engine::events::registry::SessionEndpoint;
    const CARDS: usize = 60;
    const LANES: usize = 16;
    let _home = Home::new();
    let host = host().await;
    let queue = Arc::new(EventQueue::new(16));
    let start = |id: String| -> SessionEndpoint {
        SessionEndpoint::start(&id, None, Arc::clone(&queue)).unwrap_or_else(|e| panic!("card {id}: {e}"))
    };
    let _cards: Vec<SessionEndpoint> = (0..CARDS).map(|i| start(format!("seeded-card-{i:02}"))).collect();
    // One endpoint start with that many live cards, alone: how long each
    // build holds the registry lock.
    let t0 = std::time::Instant::now();
    let timed: Vec<SessionEndpoint> = (0..LANES).map(|i| start(format!("timed-card-{i:02}"))).collect();
    let each = t0.elapsed().as_secs_f64() * 1e3 / LANES as f64;
    drop(timed);
    eprintln!(
        "endpoint start with {CARDS} live cards: {each:.2} ms each; 57 one after another: {:.0} ms",
        each * 57.0
    );

    let mut ids: Vec<String> = Vec::new();
    while ids.len() < LANES {
        // Two sessions saved in the same second share an id 1 time in 65,536.
        let id = saved_session(None).await;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    // Every build starts at the same moment.
    let together = Arc::new(tokio::sync::Barrier::new(LANES));
    let at_once = {
        let together = Arc::clone(&together);
        hook(move || {
            let together = Arc::clone(&together);
            Box::pin(async move {
                let _ = tokio::time::timeout(Duration::from_secs(10), together.wait()).await;
                Ok(())
            })
        })
    };
    let tasks: Vec<_> = ids
        .iter()
        .map(|id| spawn_create(&host, exact(id), at_once.clone(), Arc::default()))
        .collect();
    let mut failed = Vec::new();
    for (id, t) in ids.iter().zip(tasks) {
        match tokio::time::timeout(Duration::from_secs(60), t).await {
            Ok(Ok(Ok(h))) => assert_eq!(h.id.as_str(), id),
            Ok(Ok(Err(e))) => failed.push(format!("{id}: {e}")),
            Ok(Err(e)) => failed.push(format!("{id}: the create task failed: {e}")),
            Err(_) => failed.push(format!("{id}: no answer in 60 s")),
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {LANES} restore lanes of different sessions failed: {failed:#?}",
        failed.len()
    );
    for id in &ids {
        assert_eq!(live_with_id(&host, id), 1, "{id}");
    }
}

/// A restore of an old journal that was compacted into a successor (its
/// `compacted_into` names it) lands on the successor, resolved via
/// "compacted". A client's `--continue <old>` while that restore builds
/// meets it on the same actor: one build, the client attaches.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn a_restore_of_a_compacted_journal_lands_on_its_successor_and_a_client_continue_meets_it() {
    let _home = Home::new();
    let host = host().await;
    let successor = saved_session(None).await;
    let old = loop {
        let mut s = agent_engine::core::session::Session::new(MODEL, "medium", None);
        if s.id == successor {
            continue;
        }
        s.compacted_into = Some(successor.clone());
        s.save().await.unwrap();
        break s.id;
    };
    let builds = Arc::new(AtomicUsize::new(0));
    let building = Arc::new(tokio::sync::Notify::new());
    let slow = {
        let builds = Arc::clone(&builds);
        let building = Arc::clone(&building);
        hook(move || {
            let builds = Arc::clone(&builds);
            let building = Arc::clone(&building);
            Box::pin(async move {
                builds.fetch_add(1, SeqCst);
                building.notify_one();
                tokio::time::sleep(Duration::from_millis(300)).await;
                Ok(())
            })
        })
    };
    let lane = spawn_create(&host, exact(&old), slow.clone(), Arc::default());
    tokio::time::timeout(Duration::from_secs(30), building.notified())
        .await
        .expect("the restore lane's build did not start in 30 s");
    let client_probe = Arc::new(CreateProbe::default());
    let client = spawn_create(&host, cont(&old), slow.clone(), Arc::clone(&client_probe));
    let lane = joined(lane, "the restore lane").await;
    let client = joined(client, "the client's --continue").await;
    assert_eq!(builds.load(SeqCst), 1, "a restore and a client continue of one journal built {} actors", builds.load(SeqCst));
    assert!(client_probe.found_live.load(SeqCst), "the client's create did not attach to the restored actor");
    assert_eq!(lane.id.as_str(), successor, "the restore landed on the successor");
    assert_eq!(lane.journal_id(), successor);
    assert_eq!(client.id.as_str(), successor);
    assert_eq!(live_with_id(&host, &successor), 1);
    assert_eq!(live_with_id(&host, &old), 0, "no actor on the compacted journal");
    let meta = host
        .sessions()
        .into_iter()
        .find(|m| m.id.as_str() == successor)
        .expect("the successor is live");
    let info = meta.continue_info.expect("a continue records how it resolved");
    assert_eq!(info.resolved_via.as_deref(), Some("compacted"), "{info:?}");
    assert_eq!(info.query, old, "{info:?}");
}
