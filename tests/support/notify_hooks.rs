//! A real process extension subscribed to the notify-only hooks
//! (`tests/fixtures/notify_only_extension.py`), for tests that prove what
//! such a hook receives and that it cannot hold its sender up.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use synaps_cli::extensions::manager::ExtensionManager;
use synaps_cli::extensions::manifest::ExtensionManifest;

/// Load the fixture as extension `id`, answering every hook as `mode` says
/// (`record`, `block`, `error`, `exit`, `hang`; see the fixture) and
/// subscribed to `hooks`. Its files live in `<home>/plugins/<id>`. Returns
/// the JSONL log the fixture appends each event to before it answers.
pub async fn load(
    manager: &mut ExtensionManager,
    home: &Path,
    id: &str,
    mode: &str,
    hooks: &[&str],
) -> PathBuf {
    let script = std::env::current_dir()
        .unwrap()
        .join("tests/fixtures/notify_only_extension.py");
    assert!(script.exists(), "fixture missing: {script:?}");
    let plugin_dir = home.join("plugins").join(id);
    std::fs::create_dir_all(&plugin_dir).unwrap();
    let log = plugin_dir.join("hooks.jsonl");
    let manifest: ExtensionManifest = serde_json::from_value(json!({
        "protocol_version": 1,
        "runtime": "process",
        "command": "python3",
        "args": [script, mode, log],
        "permissions": ["privacy.llm_content", "session.lifecycle"],
        "hooks": hooks.iter().map(|hook| json!({"hook": hook})).collect::<Vec<_>>(),
    }))
    .unwrap();
    manager
        .load_with_cwd(id, &manifest, Some(plugin_dir))
        .await
        .expect("load the notify-only fixture");
    log
}

/// The events logged so far: `{"pid": .., "params": <hook.handle params>}`.
/// A line still being written is left for the next read.
pub fn logged(log: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let complete = text.rfind('\n').map_or("", |end| &text[..end]);
    complete
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// The log once it holds `count` events. Deliveries are detached from
/// whatever sent them, so a test has to wait; 30 s without them fails.
pub async fn wait_for(log: &Path, count: usize) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let events = logged(log);
        if events.len() >= count {
            return events;
        }
        assert!(
            Instant::now() < deadline,
            "expected {count} hook events in {log:?}, got {events:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Whether process `pid` is gone (not running, not a zombie), waiting up to
/// 10 s for it.
#[cfg(target_os = "linux")]
pub async fn exited(pid: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Path::new(&format!("/proc/{pid}")).exists() {
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}
