//! Boot discovery for MCP servers without cached descriptors (#427).
//!
//! A configured server with no fingerprint-matching descriptor-cache entry
//! used to contribute no tools, and the cache was only written after a leased
//! call, which needs a tool to call: a newly added server stayed invisible
//! forever. `setup_lazy_mcp` now lists such a server once at boot, records it,
//! and registers its tools. Own test binary: it points `SYNAPS_BASE_DIR` at a
//! temp dir for the whole process, so every test here is `#[serial]`.
//!
//! Release-blocker follow-up: discovery runs in parallel (worst case ≈ one
//! seed timeout), failures are negative-cached with exponential backoff
//! keyed to the config fingerprint, and the daemon's background mode boots
//! on cached descriptors only. Timeouts and the clock are injected through
//! the doc-hidden `setup_lazy_mcp_with` / `run_with` seam.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_engine::mcp::{
    descriptors, setup_cached_mcp, setup_lazy_mcp, setup_lazy_mcp_with, McpDiscoveryOpts,
};
use agent_engine::{EngineHost, HostOpts, ToolRegistry};
use serde_json::{json, Value};
use serial_test::serial;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// Injected per-server discovery bound: short, but generous enough for a
/// python fixture to start, initialize and list on a loaded CI machine.
const TEST_TIMEOUT: Duration = Duration::from_millis(1500);
/// Slack on top of one timeout for process teardown and cache writes.
const SLACK: Duration = Duration::from_millis(2500);
/// Injected "now" (unix seconds) for the negative-cache clock.
const NOW: u64 = 1_700_000_000;
const HOUR: u64 = 3600;

fn opts(now: u64) -> McpDiscoveryOpts {
    McpDiscoveryOpts {
        timeout: TEST_TIMEOUT,
        now_unix: Some(now),
    }
}

/// Fresh temp base dir as `SYNAPS_BASE_DIR`, discovery enabled.
fn fresh_base() -> tempfile::TempDir {
    let base = tempfile::tempdir().unwrap();
    std::env::set_var("SYNAPS_BASE_DIR", base.path());
    std::env::remove_var("SYNAPS_MCP_CACHE_WRITEBACK");
    base
}

fn spy(base: &Path, server: &str) -> PathBuf {
    base.join(format!("spy-{server}.log"))
}

/// How many times the fixture for `server` was started.
fn spawns(base: &Path, server: &str) -> usize {
    std::fs::read_to_string(spy(base, server))
        .unwrap_or_default()
        .lines()
        .filter(|line| *line == "spawn")
        .count()
}

/// Fixture server entry for `mcp.json`. `mode` is the env mode; a server
/// whose behaviour must change without a fingerprint change gets a mode file
/// instead (`mode_file`).
fn fixture_server(base: &Path, server: &str, mode: &str, mode_file: Option<&Path>) -> Value {
    let tools_json = base.join(format!("tools-{server}.json"));
    std::fs::write(
        &tools_json,
        json!([{
            "name": "echo_tool",
            "description": "Echo the input.",
            "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}
        }])
        .to_string(),
    )
    .unwrap();
    let mut env = json!({
        "MCP_FIXTURE_SPY": spy(base, server).display().to_string(),
        "MCP_FIXTURE_TOOLS_JSON": tools_json.display().to_string(),
        "MCP_FIXTURE_MODE": mode,
    });
    if let Some(file) = mode_file {
        env["MCP_FIXTURE_MODE_FILE"] = json!(file.display().to_string());
    }
    json!({
        "command": "python3",
        "args": [fixture("mcp_fixture_server.py").display().to_string()],
        "env": env,
    })
}

fn write_mcp(base: &Path, servers: Vec<(&str, Value)>) {
    let map: serde_json::Map<String, Value> =
        servers.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    std::fs::write(
        base.join("mcp.json"),
        json!({ "mcpServers": Value::Object(map) }).to_string(),
    )
    .unwrap();
}

/// Current launch fingerprint of `server` as `mcp.json` configures it.
fn fingerprint_of(server: &str) -> String {
    let config = agent_engine::mcp::load_mcp_config().expect("mcp.json loads");
    descriptors::server_config_fingerprint(&config.mcp_servers[server])
}

fn failure_of(server: &str) -> Option<descriptors::DiscoveryFailure> {
    descriptors::load_default_cache()
        .ok()
        .and_then(|cache| cache.discovery_failures.get(server).cloned())
}

fn registry() -> Arc<tokio::sync::RwLock<ToolRegistry>> {
    Arc::new(tokio::sync::RwLock::new(ToolRegistry::empty()))
}

#[tokio::test]
#[serial]
async fn new_server_is_discovered_once_at_boot_and_its_tools_registered() {
    let base = tempfile::tempdir().unwrap();
    std::env::set_var("SYNAPS_BASE_DIR", base.path());
    std::env::remove_var("SYNAPS_MCP_CACHE_WRITEBACK");
    let spy = base.path().join("spy.log");
    let tools_json = base.path().join("tools.json");
    std::fs::write(
        &tools_json,
        json!([{
            "name": "echo_tool",
            "description": "Echo the input.",
            "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}
        }])
        .to_string(),
    )
    .unwrap();
    let mcp = json!({"mcpServers": {"fresh": {
        "command": "python3",
        "args": [fixture("mcp_fixture_server.py").display().to_string()],
        "env": {
            "MCP_FIXTURE_SPY": spy.display().to_string(),
            "MCP_FIXTURE_TOOLS_JSON": tools_json.display().to_string(),
            "MCP_FIXTURE_MODE": "ok"
        }
    }}});
    std::fs::write(base.path().join("mcp.json"), mcp.to_string()).unwrap();
    assert!(!descriptors::default_cache_path().exists(), "no cache yet");

    // First boot: no cache entry, so the server is listed once and recorded.
    let registry = Arc::new(tokio::sync::RwLock::new(ToolRegistry::empty()));
    let registered = setup_lazy_mcp(&registry).await;
    assert_eq!(registered, 1, "the discovered tool is registered");
    assert!(registry.read().await.get("ext__fresh__echo_tool").is_some());
    let cache = descriptors::load_default_cache().expect("cache written by discovery");
    assert_eq!(cache.servers["fresh"].tools.len(), 1);
    let spawns_after_first = std::fs::read_to_string(&spy).unwrap_or_default();

    // Second boot: the fingerprint-matching entry is used; nothing is spawned.
    let registry = Arc::new(tokio::sync::RwLock::new(ToolRegistry::empty()));
    assert_eq!(setup_lazy_mcp(&registry).await, 1);
    assert_eq!(
        std::fs::read_to_string(&spy).unwrap_or_default(),
        spawns_after_first,
        "a cached server is not started again at boot"
    );
}

/// (a) One hung + one good server: boot finishes within about one timeout,
/// the good server's tools are registered and the hung server's failure is
/// recorded under its fingerprint.
#[tokio::test]
#[serial]
async fn hung_server_costs_one_timeout_and_its_failure_is_recorded() {
    let base = fresh_base();
    let b = base.path();
    write_mcp(
        b,
        vec![
            ("stuck", fixture_server(b, "stuck", "hang", None)),
            ("fresh", fixture_server(b, "fresh", "ok", None)),
        ],
    );

    let registry = registry();
    let started = Instant::now();
    let registered = setup_lazy_mcp_with(&registry, opts(NOW)).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < TEST_TIMEOUT + SLACK,
        "boot discovery took {elapsed:?}; a hung server must cost at most one timeout"
    );
    assert_eq!(registered, 1, "only the good server's tool is registered");
    assert!(registry.read().await.get("ext__fresh__echo_tool").is_some());
    assert!(registry.read().await.get("ext__stuck__echo_tool").is_none());
    assert_eq!(spawns(b, "stuck"), 1);
    assert_eq!(spawns(b, "fresh"), 1);

    let cache = descriptors::load_default_cache().expect("cache written");
    assert!(cache.servers.contains_key("fresh"));
    assert!(!cache.servers.contains_key("stuck"));
    let failure = cache.discovery_failures.get("stuck").expect("failure recorded");
    assert_eq!(failure.fingerprint, fingerprint_of("stuck"));
    assert_eq!(failure.last_attempt_unix, NOW);
    assert_eq!(failure.attempts, 1);
    assert!(
        !cache.discovery_failures.contains_key("fresh"),
        "a successful server has no failure entry"
    );
}

/// (b) Inside the backoff the failed server is not started again; (c) once
/// the backoff expires it is retried, and the next backoff doubles.
#[tokio::test]
#[serial]
async fn failed_server_is_skipped_inside_backoff_and_retried_after_it() {
    let base = fresh_base();
    let b = base.path();
    write_mcp(
        b,
        vec![
            ("stuck", fixture_server(b, "stuck", "hang", None)),
            ("fresh", fixture_server(b, "fresh", "ok", None)),
        ],
    );
    assert_eq!(setup_lazy_mcp_with(&registry(), opts(NOW)).await, 1);
    assert_eq!(spawns(b, "stuck"), 1);

    // (b) Just inside the 1 h backoff: no new spawn, and no wait.
    let started = Instant::now();
    assert_eq!(setup_lazy_mcp_with(&registry(), opts(NOW + HOUR - 1)).await, 1);
    assert!(started.elapsed() < TEST_TIMEOUT, "a suppressed server costs no timeout");
    assert_eq!(spawns(b, "stuck"), 1, "no spawn inside the backoff");
    assert_eq!(spawns(b, "fresh"), 1, "the cached server is not started either");
    assert_eq!(failure_of("stuck").unwrap().attempts, 1);

    // (c) Backoff expired: retried once, attempts incremented.
    assert_eq!(setup_lazy_mcp_with(&registry(), opts(NOW + HOUR)).await, 1);
    assert_eq!(spawns(b, "stuck"), 2, "retried after the backoff");
    let failure = failure_of("stuck").unwrap();
    assert_eq!(failure.attempts, 2);
    assert_eq!(failure.last_attempt_unix, NOW + HOUR);
    assert_eq!(failure.backoff_secs(), 2 * HOUR, "backoff doubles");

    // The doubled backoff holds, then expires.
    setup_lazy_mcp_with(&registry(), opts(NOW + HOUR + 2 * HOUR - 1)).await;
    assert_eq!(spawns(b, "stuck"), 2, "still inside the 2 h backoff");
    setup_lazy_mcp_with(&registry(), opts(NOW + HOUR + 2 * HOUR)).await;
    assert_eq!(spawns(b, "stuck"), 3);
    assert_eq!(failure_of("stuck").unwrap().attempts, 3);
}

/// (d) A config edit (new fingerprint) retries immediately, inside the old
/// backoff, and restarts the attempt count.
#[tokio::test]
#[serial]
async fn changed_fingerprint_retries_immediately() {
    let base = fresh_base();
    let b = base.path();
    write_mcp(b, vec![("stuck", fixture_server(b, "stuck", "hang", None))]);
    assert_eq!(setup_lazy_mcp_with(&registry(), opts(NOW)).await, 0);
    assert_eq!(spawns(b, "stuck"), 1);
    let old_fingerprint = fingerprint_of("stuck");

    let mut edited = fixture_server(b, "stuck", "hang", None);
    edited["env"]["EXTRA_SETTING"] = json!("1");
    write_mcp(b, vec![("stuck", edited)]);
    let new_fingerprint = fingerprint_of("stuck");
    assert_ne!(old_fingerprint, new_fingerprint);

    assert_eq!(setup_lazy_mcp_with(&registry(), opts(NOW + 1)).await, 0);
    assert_eq!(spawns(b, "stuck"), 2, "a changed config is retried at once");
    let failure = failure_of("stuck").unwrap();
    assert_eq!(failure.fingerprint, new_fingerprint);
    assert_eq!(failure.attempts, 1, "a new fingerprint restarts the count");
    assert_eq!(failure.last_attempt_unix, NOW + 1);
}

/// (e) A server that recovers (same fingerprint) is discovered after its
/// backoff, its tools are registered and its failure entry is removed.
#[tokio::test]
#[serial]
async fn success_after_backoff_clears_the_failure_entry() {
    let base = fresh_base();
    let b = base.path();
    let mode_file = b.join("flaky.mode");
    std::fs::write(&mode_file, "hang").unwrap();
    write_mcp(
        b,
        vec![("flaky", fixture_server(b, "flaky", "ok", Some(&mode_file)))],
    );
    assert_eq!(setup_lazy_mcp_with(&registry(), opts(NOW)).await, 0);
    assert!(failure_of("flaky").is_some(), "first attempt failed");

    std::fs::write(&mode_file, "ok").unwrap();
    let registry = registry();
    assert_eq!(setup_lazy_mcp_with(&registry, opts(NOW + HOUR)).await, 1);
    assert!(registry.read().await.get("ext__flaky__echo_tool").is_some());
    assert_eq!(spawns(b, "flaky"), 2);
    let cache = descriptors::load_default_cache().unwrap();
    assert!(cache.servers.contains_key("flaky"));
    assert!(
        !cache.discovery_failures.contains_key("flaky"),
        "success removes the failure entry"
    );
}

/// (f) Several hung servers are discovered in parallel: the boot costs about
/// one timeout, not one per server.
#[tokio::test]
#[serial]
async fn several_hung_servers_cost_about_one_timeout_in_parallel() {
    let base = fresh_base();
    let b = base.path();
    let names = ["h1", "h2", "h3", "h4", "h5", "h6"];
    write_mcp(
        b,
        names
            .iter()
            .map(|name| (*name, fixture_server(b, name, "hang", None)))
            .collect(),
    );
    let started = Instant::now();
    assert_eq!(setup_lazy_mcp_with(&registry(), opts(NOW)).await, 0);
    let elapsed = started.elapsed();
    // Sequential discovery would take names.len() × TEST_TIMEOUT (9 s).
    assert!(
        elapsed < TEST_TIMEOUT + SLACK,
        "{} hung servers took {elapsed:?}; discovery must run in parallel",
        names.len()
    );
    for name in names {
        assert_eq!(spawns(b, name), 1, "{name} started once");
        let failure = failure_of(name).unwrap_or_else(|| panic!("{name} failure recorded"));
        assert_eq!(failure.attempts, 1);
    }
}

/// Background mode, MCP level: the cached-only phase never spawns and
/// returns at once even with a hung server configured; the deferred run
/// registers ONLY the newly discovered server's tools into the same shared
/// registry (the already-registered cached server is not re-registered, so
/// the batch does not collide) and records the hung server's failure.
#[tokio::test]
#[serial]
async fn background_discovery_registers_only_new_servers_without_blocking_boot() {
    let base = fresh_base();
    let b = base.path();
    write_mcp(
        b,
        vec![
            ("cached", fixture_server(b, "cached", "ok", None)),
            ("fresh", fixture_server(b, "fresh", "ok", None)),
            ("stuck", fixture_server(b, "stuck", "hang", None)),
        ],
    );
    descriptors::record_server_listing(
        &descriptors::default_cache_path(),
        "cached",
        &fingerprint_of("cached"),
        &[descriptors::CachedToolDescriptor {
            name: "cached_tool".to_string(),
            description: "pre-cached".to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
        }],
    )
    .unwrap();

    let registry = registry();
    let started = Instant::now();
    let (registered, pending) = setup_cached_mcp(&registry).await;
    assert!(
        started.elapsed() < TEST_TIMEOUT,
        "cached-only boot must not wait for any server"
    );
    assert_eq!(registered, 1);
    assert!(registry.read().await.get("ext__cached__cached_tool").is_some());
    assert!(registry.read().await.get("ext__fresh__echo_tool").is_none());
    for name in ["cached", "fresh", "stuck"] {
        assert_eq!(spawns(b, name), 0, "{name}: nothing is spawned at cached-only boot");
    }
    let pending = pending.expect("uncached servers are pending");
    assert_eq!(pending.server_names(), vec!["fresh".to_string(), "stuck".to_string()]);

    let shared = Arc::clone(&registry);
    let task = tokio::spawn(async move { pending.run_with(&shared, opts(NOW)).await });
    let deadline = Instant::now() + TEST_TIMEOUT + SLACK + Duration::from_secs(5);
    while registry.read().await.get("ext__fresh__echo_tool").is_none() {
        assert!(
            Instant::now() < deadline,
            "the new server's tools never reached the shared registry"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let added = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("background discovery finishes")
        .expect("task did not panic");
    assert_eq!(added, 1, "only the newly discovered server's tool is registered");
    assert!(registry.read().await.get("ext__cached__cached_tool").is_some());
    assert!(registry.read().await.get("ext__stuck__echo_tool").is_none());
    assert_eq!(spawns(b, "cached"), 0, "a cached server is never started");
    assert_eq!(spawns(b, "stuck"), 1);
    assert!(failure_of("stuck").is_some(), "the hung server's failure is recorded");
}

/// Background mode, host level: `HostOpts { mcp_discovery: Background }`
/// boots without discovering, and `spawn_background_mcp_discovery` (once)
/// adds the new server's tools to the host's shared registry.
#[tokio::test]
#[serial]
async fn host_background_mode_defers_discovery_to_the_spawned_task() {
    let base = fresh_base();
    let b = base.path();
    write_mcp(b, vec![("fresh", fixture_server(b, "fresh", "ok", None))]);

    let host = EngineHost::boot(HostOpts {
        profile: None,
        no_extensions: true,
        mcp_discovery: agent_engine::McpDiscovery::Background,
    })
    .await
    .expect("host boot");
    assert_eq!(host.mcp_server_count(), 0, "nothing cached yet");
    assert_eq!(spawns(b, "fresh"), 0, "boot did not discover");
    assert!(host.parts().tools.read().await.get("ext__fresh__echo_tool").is_none());

    let task = host
        .spawn_background_mcp_discovery()
        .expect("discovery is pending");
    assert!(
        host.spawn_background_mcp_discovery().is_none(),
        "the pending discovery is spawned at most once"
    );
    let added = tokio::time::timeout(Duration::from_secs(30), task)
        .await
        .expect("background discovery finishes")
        .expect("task did not panic");
    assert_eq!(added, 1);
    assert_eq!(spawns(b, "fresh"), 1);
    assert!(host.parts().tools.read().await.get("ext__fresh__echo_tool").is_some());
}
