//! Parallel subagent workers share ONE real extension process with the
//! foreground (tests/fixtures/worker_transform_extension.py). Each worker
//! runs where subagents run, on its own OS thread and current-thread tokio
//! runtime, with the host's transform view (`HookBus::worker_transforms`).
//!
//! - each worker gets the replacement of its OWN output (JSON-RPC ids keep
//!   interleaved calls apart);
//! - no worker sends `before_tool_call`;
//! - a crash mid-call leaves that worker's output as it was, at once, and
//!   later worker calls pass through without respawning the process; the
//!   foreground's next call restarts it on the host runtime, and workers
//!   are transformed again.
//!
//! Own process: sets `SYNAPS_BASE_DIR` (a replace writes the intercept audit).

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use synaps_cli::extensions::hooks::HookBus;
use synaps_cli::extensions::manager::ExtensionManager;
use synaps_cli::extensions::manifest::{
    ExtensionManifest, ExtensionRuntime, HookSubscription, CURRENT_EXTENSION_PROTOCOL_VERSION,
};

const WORKERS: usize = 6;
const CALLS: usize = 8;

fn manifest(spy: &std::path::Path) -> ExtensionManifest {
    let fixture = std::env::current_dir()
        .unwrap()
        .join("tests/fixtures/worker_transform_extension.py");
    ExtensionManifest {
        theme_tokens: Default::default(),
        deferred: None,
        protocol_version: CURRENT_EXTENSION_PROTOCOL_VERSION,
        runtime: ExtensionRuntime::Process,
        command: "python3".to_string(),
        setup: None,
        prebuilt: Default::default(),
        args: vec![
            fixture.to_string_lossy().to_string(),
            spy.to_string_lossy().to_string(),
        ],
        permissions: vec!["tools.intercept".into(), "tools.transform_output".into()],
        hooks: vec![
            HookSubscription {
                hook: "after_tool_call".into(),
                tool: None,
                matcher: None,
            },
            HookSubscription {
                hook: "before_tool_call".into(),
                tool: None,
                matcher: None,
            },
        ],
        config: vec![],
    }
}

async fn after(bus: &Arc<HookBus>, output: &str) -> String {
    synaps_cli::runtime::emit_after_tool_call(
        bus,
        "bash",
        None,
        json!({"command": "make"}),
        output.to_string(),
        30_000,
        None,
    )
    .await
}

/// Run `body` on `n` worker-shaped threads (own OS thread, own
/// current-thread runtime, own view of `host`), joined off the host runtime.
async fn on_workers<F, Fut>(host: &Arc<HookBus>, n: usize, body: F)
where
    F: Fn(usize, Arc<HookBus>) -> Fut + Send + Sync + Clone + 'static,
    Fut: std::future::Future<Output = ()>,
{
    let threads: Vec<_> = (0..n)
        .map(|w| {
            let view = HookBus::worker_transforms(host);
            let body = body.clone();
            std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(body(w, view));
            })
        })
        .collect();
    tokio::task::spawn_blocking(move || {
        for t in threads {
            t.join().expect("worker thread");
        }
    })
    .await
    .unwrap();
}

fn spy_count(spy: &std::path::Path, line: &str) -> usize {
    std::fs::read_to_string(spy)
        .unwrap_or_default()
        .lines()
        .filter(|l| *l == line)
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_workers_share_one_extension_and_never_respawn_it() {
    let base = tempfile::tempdir().unwrap();
    synaps_cli::config::set_base_dir_for_tests(base.path().to_path_buf());
    let spy = base.path().join("spy.log");

    // The foreground host: the extension is loaded (spawned) here.
    let host = Arc::new(HookBus::new());
    let mut manager = ExtensionManager::new(Arc::clone(&host));
    manager
        .load("worker-crush", &manifest(&spy))
        .await
        .expect("load fixture extension");
    assert_eq!(spy_count(&spy, "spawn"), 1);

    // 1. Parallel workers each get their own replacement; none sends
    //    before_tool_call.
    on_workers(&host, WORKERS, |w, view| async move {
        for call in 0..CALLS {
            let output = format!("worker {w} call {call} {}", "x".repeat(w * 997));
            assert_eq!(after(&view, &output).await, format!("[crushed] {output}"));
        }
        let before = synaps_cli::runtime::emit_before_tool_call(
            &view,
            "bash",
            None,
            json!({"command": "rm -rf /"}),
            None,
        )
        .await;
        assert_eq!(
            before,
            synaps_cli::extensions::hooks::events::HookResult::Continue
        );
    })
    .await;
    assert_eq!(spy_count(&spy, "hook:after_tool_call"), WORKERS * CALLS);
    assert_eq!(spy_count(&spy, "hook:before_tool_call"), 0, "never from a worker");

    // 2. A crash mid-call: that worker keeps its own output, promptly.
    on_workers(&host, 1, |_, view| async move {
        let started = Instant::now();
        assert_eq!(after(&view, "__crash__").await, "__crash__");
        assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());
    })
    .await;
    // Later worker calls pass through and do not respawn the process.
    on_workers(&host, 3, |w, view| async move {
        let output = format!("after the crash {w}");
        let started = Instant::now();
        assert_eq!(after(&view, &output).await, output, "pass-through");
        assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
    })
    .await;
    assert_eq!(spy_count(&spy, "spawn"), 1, "no worker respawned the extension");

    // 3. The foreground's next call restarts it (on the host runtime)...
    assert_eq!(after(&host, "foreground").await, "[crushed] foreground");
    assert_eq!(spy_count(&spy, "spawn"), 2);
    // ...and workers are transformed again, by the restarted process.
    on_workers(&host, 2, |w, view| async move {
        let output = format!("back {w}");
        assert_eq!(after(&view, &output).await, format!("[crushed] {output}"));
    })
    .await;
    assert_eq!(spy_count(&spy, "spawn"), 2);

    manager.shutdown_all().await;
}
