//! Pins at the engine for the removed Gemini Code Assist provider:
//!
//! - none of the removed provider's endpoint strings (its Code Assist host
//!   and its internal API path) remain in the shipped source trees, and its
//!   modules are gone;
//! - its old OAuth client variables, set in this test's own scoped env, change
//!   nothing, and no shipped source names them;
//! - a real boot drops a saved `google-gemini` sign-in by key name and keeps
//!   every other entry; with none saved the store is left as it was;
//! - the Google AI Studio API-key provider and Vertex are unchanged.
//!
//! The model picker's side is pinned in agent-tui (`tui::models`).
//!
//! Every needle is built from parts (`concat!`), so this file never holds one
//! whole and the scan, which covers test modules too, stays clean.

use crate::auth::provider::{
    parse_cli_provider, registry, removed_provider_refusal, DESCRIPTORS,
    REMOVED_GEMINI_CODE_ASSIST_REFUSAL,
};
use crate::auth::{CloudProviderId, OAuthProviderId};
use crate::runtime::openai::{
    registry as static_providers, resolve_route, AuthPolicy, ResolvedRoute, WireProtocol,
};
use crate::test_env::EnvVarGuard;
use std::path::{Path, PathBuf};

/// The removed provider's Code Assist host and internal API path.
const ENDPOINT_NEEDLES: [&str; 2] = [concat!("cloudcode", "-pa"), concat!("v1", "internal")];
/// The removed sign-in's OAuth client variables.
const CLIENT_ID_VAR: &str = concat!("SYNAPS_GOOGLE_GEMINI", "_CLIENT_ID");
const CLIENT_SECRET_VAR: &str = concat!("SYNAPS_GOOGLE_GEMINI", "_CLIENT_SECRET");
/// Every name the removed sign-in answered to.
const REMOVED_NAMES: [&str; 4] = ["google-gemini", "gemini", "gemini-cli", "gemini-code-assist"];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every file of the shipped source trees (root `src/` and each crate's
/// `src/`, test modules and embedded prompts included) and each build
/// script, as `(path, lowercased bytes)`.
fn shipped_sources() -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.push(p);
            }
        }
    }
    let root = workspace_root();
    let mut files = Vec::new();
    walk(&root.join("src"), &mut files);
    if root.join("build.rs").is_file() {
        files.push(root.join("build.rs"));
    }
    for e in std::fs::read_dir(root.join("crates")).unwrap() {
        let krate = e.unwrap().path();
        if krate.join("src").is_dir() {
            walk(&krate.join("src"), &mut files);
        }
        if krate.join("build.rs").is_file() {
            files.push(krate.join("build.rs"));
        }
    }
    // The scan must reach the places the removed provider lived beside, or it rotted.
    for must in [
        "src/cmd/login.rs",
        "src/main.rs",
        "crates/agent-core/src/core/auth/broker.rs",
        "crates/agent-core/src/core/auth/provider.rs",
        "crates/agent-core/src/core/auth/storage.rs",
        "crates/agent-engine/src/runtime/openai/mod.rs",
        "crates/agent-engine/src/runtime/openai/catalog/mod.rs",
        "crates/agent-engine/src/runtime/google_vertex.rs",
        "crates/agent-engine/src/runtime/trace/google.rs",
        "crates/agent-tui/src/tui/models/mod.rs",
    ] {
        assert!(files.iter().any(|f| f.ends_with(must)), "scan does not cover {must}");
    }
    assert!(files.len() > 300, "scan found only {} files", files.len());
    files
        .into_iter()
        .map(|f| {
            let bytes = std::fs::read(&f).unwrap().to_ascii_lowercase();
            (f, bytes)
        })
        .collect()
}

/// Each shipped file (path relative to the workspace root) that holds `needle`, any case.
fn holders(sources: &[(PathBuf, Vec<u8>)], needle: &str) -> Vec<String> {
    let needle = needle.to_ascii_lowercase().into_bytes();
    let root = workspace_root();
    sources
        .iter()
        .filter(|(_, bytes)| bytes.windows(needle.len()).any(|w| w == needle.as_slice()))
        .map(|(f, _)| f.strip_prefix(&root).unwrap_or(f).display().to_string())
        .collect()
}

/// No shipped source holds the removed provider's endpoint host or API
/// path, and its modules, catalog entry and spec are gone.
#[test]
fn no_shipped_source_names_the_removed_code_assist_endpoint() {
    let sources = shipped_sources();
    for needle in ENDPOINT_NEEDLES {
        assert_eq!(holders(&sources, needle), Vec::<String>::new(), "{needle}");
    }
    let root = workspace_root();
    for gone in [
        "crates/agent-core/src/core/auth/google_gemini.rs",
        "crates/agent-engine/src/runtime/google_gemini",
        "crates/agent-engine/src/runtime/openai/catalog/google_gemini.rs",
        "docs/specs/google-gemini-oauth-spec.md",
    ] {
        assert!(!root.join(gone).exists(), "{gone} is back");
    }
}

/// No shipped source names the old client variables, so nothing can read
/// them.
#[test]
fn no_shipped_source_names_the_old_client_variables() {
    let sources = shipped_sources();
    for var in [CLIENT_ID_VAR, CLIENT_SECRET_VAR] {
        assert_eq!(holders(&sources, var), Vec::<String>::new(), "{var}");
    }
}

/// What a provider name or model id resolves to, wherever the removed
/// provider used to answer: login parsing, the refusal, the OAuth registry
/// and model routing.
type Surface = (
    Vec<Result<OAuthProviderId, String>>,
    Vec<Option<&'static str>>,
    Vec<&'static str>,
    Vec<Option<ResolvedRoute>>,
);

fn surface() -> Surface {
    let names = REMOVED_NAMES
        .iter()
        .copied()
        .chain(["GEMINI", "google", "google-vertex", "anthropic"]);
    let mut oauth: Vec<&'static str> = registry().iter().map(|d| d.id.as_str()).collect();
    oauth.sort_unstable();
    (
        names.clone().map(parse_cli_provider).collect(),
        names.map(removed_provider_refusal).collect(),
        oauth,
        [
            "google-gemini/gemini-2.5-pro",
            "gemini/gemini-2.5-pro",
            "gemini-cli/gemini-2.5-pro",
            "gemini-code-assist/gemini-2.5-pro",
            "google/gemini-2.5-flash",
        ]
        .into_iter()
        .map(resolve_route)
        .collect(),
    )
}

/// The old client variables, set only in this test's own scoped env,
/// change nothing: every surface reads the same as with them unset, the
/// removed names are still refused, and nothing routes them.
#[test]
#[serial_test::serial(synaps_base_dir)]
fn the_old_client_variables_set_in_scope_change_nothing() {
    let _no_id = EnvVarGuard::remove(CLIENT_ID_VAR);
    let _no_secret = EnvVarGuard::remove(CLIENT_SECRET_VAR);
    let unset = surface();

    let _id = EnvVarGuard::set(CLIENT_ID_VAR, "fake-client-id.example");
    let _secret = EnvVarGuard::set(CLIENT_SECRET_VAR, "fake-client-secret");
    assert_eq!(std::env::var(CLIENT_ID_VAR).as_deref(), Ok("fake-client-id.example"));
    let set = surface();
    assert_eq!(set, unset, "the old client variables changed what a surface reads");

    for name in REMOVED_NAMES {
        assert_eq!(
            parse_cli_provider(name),
            Err(REMOVED_GEMINI_CODE_ASSIST_REFUSAL.to_string()),
            "{name}"
        );
        assert!(name.parse::<OAuthProviderId>().is_err(), "{name}");
        assert!(resolve_route(&format!("{name}/gemini-2.5-pro")).is_none(), "{name}");
    }
    assert!(DESCRIPTORS.iter().all(|d| !d.id.as_str().contains("gemini")));
    assert!(registry().iter().all(|d| !d.id.as_str().contains("gemini")));
}

/// The Google AI Studio API-key provider (`google`) and Vertex
/// (`google-vertex`) are as they were: the static spec, its route, and
/// neither is refused as a removed name.
#[test]
fn the_ai_studio_key_provider_and_vertex_are_unchanged() {
    let spec = static_providers::providers()
        .iter()
        .find(|s| s.key == "google")
        .expect("the AI Studio key provider is registered");
    assert_eq!(spec.name, "Google AI Studio");
    assert_eq!(spec.base_url, "https://generativelanguage.googleapis.com/v1beta/openai");
    assert_eq!(spec.env_vars, &["GOOGLE_API_KEY"]);
    assert_eq!(spec.default_model, "gemini-2.5-flash");
    assert_eq!(
        spec.models,
        &[
            ("gemini-2.5-flash", "Gemini 2.5 Flash", "A+"),
            ("gemini-2.0-flash", "Gemini 2.0 Flash", "B+"),
            ("gemma-3-27b-it", "Gemma 3 27B", "A"),
        ]
    );
    assert_eq!(
        resolve_route("google/gemini-2.5-flash"),
        Some(ResolvedRoute {
            endpoint: spec.base_url.into(),
            model: "gemini-2.5-flash".into(),
            provider: "google".into(),
            auth: AuthPolicy::BrokerProxy,
            wire: WireProtocol::OpenAiChatCompletions,
        })
    );
    assert_eq!("google-vertex".parse::<CloudProviderId>(), Ok(CloudProviderId::GoogleVertex));
    assert_eq!(CloudProviderId::GoogleVertex.as_str(), "google-vertex");
    for name in ["google", "google-vertex", "GOOGLE"] {
        assert_eq!(removed_provider_refusal(name), None, "{name}");
    }
}

/// At startup: a real host boot on a scratch home.
#[cfg(unix)]
mod boot {
    use crate::test_env::{EnvVarGuard, OfflineProviderEnv};
    use crate::EngineHost;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;

    const SENTINEL: &str = "removed-gemini-SENTINEL-boot-not-a-real-refresh-token";

    struct Home {
        base: PathBuf,
        _env: Vec<EnvVarGuard>,
        _offline: OfflineProviderEnv,
        _run: tempfile::TempDir,
        _tmp: tempfile::TempDir,
    }

    impl Home {
        /// A scratch home whose `auth.json` holds `store`.
        fn new(store: &serde_json::Value) -> Self {
            let tmp = tempfile::Builder::new()
                .prefix("removed-gemini-boot-")
                .tempdir()
                .expect("tempdir");
            let root = fs::canonicalize(tmp.path()).expect("canonical tempdir");
            let base = root.join(".synaps-cli");
            fs::create_dir_all(&base).expect("mkdir");
            fs::write(base.join("config"), "").expect("config");
            fs::write(base.join("auth.json"), serde_json::to_vec_pretty(store).unwrap())
                .expect("auth.json");
            let run = tempfile::Builder::new()
                .prefix("removed-gemini-run-")
                .tempdir()
                .expect("run dir");
            let env = vec![
                EnvVarGuard::set("HOME", root.to_str().expect("utf-8 root")),
                EnvVarGuard::set("SYNAPS_BASE_DIR", base.to_str().expect("utf-8 base")),
                EnvVarGuard::set("SYNAPS_RUNTIME_DIR", run.path().to_str().expect("utf-8 run dir")),
                EnvVarGuard::set("SYNAPS_DAEMON_PARK_GRACE_SECS", "never"),
                EnvVarGuard::remove("OPENAI_API_KEY"),
                EnvVarGuard::remove("GOOGLE_API_KEY"),
            ];
            Self { base, _env: env, _offline: OfflineProviderEnv::new(), _run: run, _tmp: tmp }
        }

        fn auth(&self) -> PathBuf {
            self.base.join("auth.json")
        }
    }

    async fn boot() -> Arc<EngineHost> {
        EngineHost::boot(crate::HostOpts {
            profile: None,
            no_extensions: true,
            mcp_discovery: crate::McpDiscovery::Inline,
        })
        .await
        .expect("boot")
    }

    fn anthropic() -> serde_json::Value {
        serde_json::json!({"type": "oauth", "refresh": "r", "access": "synthetic-token", "expires": 9_999_999_999_999u64})
    }

    fn legacy() -> serde_json::Value {
        serde_json::json!({
            "type": "oauth",
            "access": SENTINEL,
            "refresh": SENTINEL,
            "expires": 9_999_999_999_999u64,
            "projectId": SENTINEL,
        })
    }

    /// A boot drops `google-gemini` and `google-gemini@<label>` (and the
    /// parked-refresh sidecar) from the active config dir's `auth.json` by
    /// key name; every other entry, a look-alike name included, is kept, and
    /// no sentinel value is left in the store. (The log line is pinned in
    /// agent-core: the global subscriber here belongs to whichever test
    /// booted first.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_boot_drops_a_saved_google_gemini_sign_in_by_name() {
        let home = Home::new(&serde_json::json!({
            "anthropic": anthropic(),
            "google-gemini": legacy(),
            "google-gemini@work": legacy(),
            "google-gemini-x": {"type": "api_key", "key": "look-alike"},
        }));
        let sidecar = home.base.join("auth.json.google-gemini.refresh.pending");
        fs::write(&sidecar, SENTINEL).expect("sidecar");

        let _host = boot().await;

        let text = fs::read_to_string(home.auth()).expect("auth.json");
        let root: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&text).unwrap();
        let mut keys: Vec<&str> = root.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["anthropic", "google-gemini-x"]);
        assert_eq!(root["anthropic"], anthropic());
        assert!(!text.contains(SENTINEL), "a removed value is left in the store");
        assert!(!sidecar.exists(), "the removed sign-in's parked refresh is left");
    }

    /// With no saved sign-in a boot leaves the store byte for byte.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial(synaps_base_dir)]
    async fn a_boot_without_one_leaves_the_store_as_it_was() {
        let home = Home::new(&serde_json::json!({
            "anthropic": anthropic(),
            "google-gemini-x": {"type": "api_key", "key": "look-alike"},
        }));
        let before = fs::read(home.auth()).expect("auth.json");

        let _host = boot().await;

        assert_eq!(fs::read(home.auth()).expect("auth.json"), before);
    }
}
