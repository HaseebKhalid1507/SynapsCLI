//! `ExtensionManager::reload_from_disk` (`synaps daemon reload-extension`):
//! one extension restarts from what is installed now, the others untouched.
//!
//! Uses the checked-in Python extension fixture (real Content-Length framed
//! JSON-RPC over stdio) as an EAGER plugin, so a reload is observed as a real
//! process restart in the fixture's spy log, and its tool set is read from a
//! file the test rewrites between loads. Plugins are installed under the
//! hermetic `SYNAPS_BASE_DIR`'s `plugins/`, which discovery walks whole, so
//! every test is serial and starts from an empty root.

#[path = "support/hermetic_base_dir.rs"]
mod hermetic_base_dir;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_engine::extensions::hooks::HookBus;
use agent_engine::extensions::manager::ExtensionManager;
use agent_engine::tools::ToolRegistry;
use serde_json::{json, Value};

const PLUGIN: &str = "reload-fixture";

fn fixture_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/extension_fixture.py")
}

/// A fresh, empty `plugins/` root under the hermetic base dir.
fn plugins_root() -> PathBuf {
    let root = hermetic_base_dir::pin().join("plugins");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn tool(name: &str) -> Value {
    json!({"name": name, "description": format!("{name} tool"), "input_schema": {"type": "object", "properties": {}}})
}

struct Installed {
    dir: PathBuf,
    spy: PathBuf,
    tools_json: PathBuf,
}

impl Installed {
    fn manifest_path(&self) -> PathBuf {
        self.dir.join(".synaps-plugin").join("plugin.json")
    }
    /// The eager fixture manifest, running `command`.
    fn write_manifest(&self, command: &str) {
        let manifest = json!({
            "name": PLUGIN,
            "extension": {
                "runtime": "process",
                "command": command,
                "args": [
                    fixture_script().display().to_string(),
                    self.spy.display().to_string(),
                    self.tools_json.display().to_string(),
                    "ok",
                ],
                "permissions": ["tools.register"],
            }
        });
        std::fs::write(self.manifest_path(), serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
    }
    /// What the fixture registers at its NEXT initialize.
    fn set_tools(&self, names: &[&str]) {
        let tools: Vec<Value> = names.iter().map(|n| tool(n)).collect();
        std::fs::write(&self.tools_json, serde_json::to_vec(&tools).unwrap()).unwrap();
    }
    fn count(&self, event: &str) -> usize {
        std::fs::read_to_string(&self.spy)
            .unwrap_or_default()
            .lines()
            .filter(|l| *l == event)
            .count()
    }
}

/// Install the eager fixture as plugin `name` under `root`.
fn install(root: &Path, name: &str, tools: &[&str]) -> Installed {
    let dir = root.join(name);
    std::fs::create_dir_all(dir.join(".synaps-plugin")).unwrap();
    let state = hermetic_base_dir::pin().join(format!("{name}-state"));
    let _ = std::fs::remove_dir_all(&state);
    std::fs::create_dir_all(&state).unwrap();
    let installed = Installed { dir, spy: state.join("spy.log"), tools_json: state.join("tools.json") };
    installed.set_tools(tools);
    installed.write_manifest("python3");
    installed
}

/// An eager (legacy path) manager over its own registry, discovery done.
async fn discovered() -> (ExtensionManager, Arc<tokio::sync::RwLock<ToolRegistry>>) {
    let registry = Arc::new(tokio::sync::RwLock::new(ToolRegistry::new()));
    let mut mgr = ExtensionManager::new_with_tools(Arc::new(HookBus::new()), Arc::clone(&registry));
    let (_loaded, failed) = mgr.discover_and_load().await;
    assert!(failed.is_empty(), "{failed:?}");
    (mgr, registry)
}

async fn names(registry: &Arc<tokio::sync::RwLock<ToolRegistry>>, plugin: &str) -> Vec<String> {
    registry.read().await.tool_names_for_extension(plugin)
}

#[tokio::test]
#[serial_test::serial(reload_plugins_root)]
async fn reload_restarts_the_process_and_swaps_its_tools_in_place() {
    let root = plugins_root();
    let fx = install(&root, PLUGIN, &["search", "sibling"]);
    let other = install(&root, "reload-bystander", &["keep"]);
    let (mut mgr, registry) = discovered().await;
    assert_eq!(names(&registry, PLUGIN).await, vec![format!("{PLUGIN}:search"), format!("{PLUGIN}:sibling")]);
    assert_eq!(fx.count("spawn"), 1);

    // The new version drops `sibling` and adds `fresh`.
    fx.set_tools(&["search", "fresh"]);
    let report = mgr.reload_from_disk(PLUGIN).await.expect("reload");

    assert!(report.was_loaded);
    assert!(!report.deferred);
    assert_eq!(report.tools, vec![format!("{PLUGIN}:fresh"), format!("{PLUGIN}:search")]);
    assert_eq!(report.removed_tools, vec![format!("{PLUGIN}:sibling")]);
    assert_eq!(report.plugin_dir, fx.dir);
    // The registry agrees: the dropped tool is gone, not left pointing at
    // the stopped process.
    assert_eq!(names(&registry, PLUGIN).await, report.tools);
    assert!(registry.read().await.get(&format!("{PLUGIN}:sibling")).is_none());
    // A real restart: the old process was shut down, a new one started.
    assert_eq!(fx.count("spawn"), 2, "a new process starts");
    assert!(fx.count("shutdown") + fx.count("eof") >= 1, "the old process was stopped");
    // The other extension was not touched.
    assert_eq!(other.count("spawn"), 1);
    assert_eq!(names(&registry, "reload-bystander").await, vec!["reload-bystander:keep".to_string()]);
    assert!(mgr.is_loaded("reload-bystander"));
    // Discovery's record still lists it once.
    let (loaded, failed) = mgr.discovery_done().unwrap();
    assert_eq!(loaded.iter().filter(|l| l.as_str() == PLUGIN).count(), 1);
    assert!(failed.is_empty());

    mgr.shutdown_all().await;
}

#[tokio::test]
#[serial_test::serial(reload_plugins_root)]
async fn a_bad_manifest_is_refused_before_the_running_instance_is_stopped() {
    let root = plugins_root();
    let fx = install(&root, PLUGIN, &["search"]);
    let (mut mgr, registry) = discovered().await;

    std::fs::write(fx.manifest_path(), b"{ not json").unwrap();
    let failure = mgr.reload_from_disk(PLUGIN).await.unwrap_err();
    assert!(failure.reason.contains("Invalid plugin manifest JSON"), "{failure:?}");

    // Still running, never restarted, tools intact.
    assert!(mgr.is_loaded(PLUGIN));
    assert_eq!(fx.count("spawn"), 1);
    assert_eq!(fx.count("shutdown") + fx.count("eof"), 0);
    assert_eq!(names(&registry, PLUGIN).await, vec![format!("{PLUGIN}:search")]);

    mgr.shutdown_all().await;
}

#[tokio::test]
#[serial_test::serial(reload_plugins_root)]
async fn a_new_version_that_cannot_start_leaves_it_unloaded_and_says_so_then_a_fix_loads_it() {
    let root = plugins_root();
    let fx = install(&root, PLUGIN, &["search"]);
    let (mut mgr, registry) = discovered().await;

    fx.write_manifest("/nonexistent/synaps-reload-test-binary");
    let failure = mgr.reload_from_disk(PLUGIN).await.unwrap_err();
    assert!(failure.reason.contains("not loaded now"), "{failure:?}");
    assert!(!mgr.is_loaded(PLUGIN));
    assert!(names(&registry, PLUGIN).await.is_empty(), "no tool may outlive its process");
    let (loaded, failed) = mgr.discovery_done().unwrap();
    assert!(!loaded.iter().any(|l| l == PLUGIN));
    assert_eq!(failed.iter().filter(|f| f.plugin == PLUGIN).count(), 1);

    // Fixed on disk: the next reload loads it (it was not loaded before).
    fx.write_manifest("python3");
    let report = mgr.reload_from_disk(PLUGIN).await.expect("reload after fix");
    assert!(!report.was_loaded);
    assert_eq!(report.tools, vec![format!("{PLUGIN}:search")]);
    let (loaded, failed) = mgr.discovery_done().unwrap();
    assert!(loaded.iter().any(|l| l == PLUGIN));
    assert!(failed.is_empty(), "{failed:?}");

    mgr.shutdown_all().await;
}

#[tokio::test]
#[serial_test::serial(reload_plugins_root)]
async fn a_plugin_installed_after_discovery_is_loaded() {
    let root = plugins_root();
    let (mut mgr, registry) = discovered().await;
    assert!(!mgr.is_loaded(PLUGIN));

    let fx = install(&root, PLUGIN, &["search"]);
    let report = mgr.reload_from_disk(PLUGIN).await.expect("load");
    assert!(!report.was_loaded);
    assert!(report.removed_tools.is_empty());
    assert_eq!(names(&registry, PLUGIN).await, vec![format!("{PLUGIN}:search")]);
    assert_eq!(fx.count("spawn"), 1);

    mgr.shutdown_all().await;
}

#[tokio::test]
#[serial_test::serial(reload_plugins_root)]
async fn names_that_discovery_would_never_load_are_refused() {
    let root = plugins_root();
    install(&root, "reload-fixture.disabled", &["search"]);
    // Before discovery: a load here would make discovery's own load fail.
    let registry = Arc::new(tokio::sync::RwLock::new(ToolRegistry::new()));
    let mut fresh = ExtensionManager::new_with_tools(Arc::new(HookBus::new()), Arc::clone(&registry));
    let early = fresh.reload_from_disk(PLUGIN).await.unwrap_err();
    assert!(early.reason.contains("not finished loading"), "{early:?}");

    let (mut mgr, _registry) = discovered().await;
    for bad in ["", ".", "..", "../etc", "a/b", "a\\b", "x:y", "tab\there"] {
        let f = mgr.reload_from_disk(bad).await.unwrap_err();
        assert!(f.reason.contains("is not a plugin name"), "{bad:?}: {f:?}");
    }
    let f = mgr.reload_from_disk("reload-fixture.disabled").await.unwrap_err();
    assert!(f.reason.contains("backup, disabled or temp"), "{f:?}");
    let f = mgr.reload_from_disk("not-installed-anywhere").await.unwrap_err();
    assert!(f.reason.contains("no plugin directory named"), "{f:?}");

    // A plugin without an `extension` block is not an extension.
    let skills_only = root.join("skills-only");
    std::fs::create_dir_all(skills_only.join(".synaps-plugin")).unwrap();
    std::fs::write(skills_only.join(".synaps-plugin/plugin.json"), br#"{"name":"skills-only"}"#).unwrap();
    let f = mgr.reload_from_disk("skills-only").await.unwrap_err();
    assert!(f.reason.contains("has no extension"), "{f:?}");
    assert_eq!(mgr.count(), 0);
}
