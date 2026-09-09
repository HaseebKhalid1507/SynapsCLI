//! MCP (Model Context Protocol) integration — JSON-RPC client, tool bridging, lazy loading.
mod connection;
pub mod descriptors;
pub mod lease;

use std::collections::HashMap;
use std::sync::Arc;

pub use lease::{McpLeaseCapability, McpRuntimeManager, McpSessionEndGuard};

/// MCP server configuration — matches claude-code/gemini-cli format.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Daemon-mode opt-in: ONE child serves every session in the process
    /// (lease key `"*"`) instead of one child per session. The server then
    /// sees every session's calls and holds cross-session state
    /// (roots/cwd) — never the default. Excluded from the config
    /// fingerprint (launch identity only); flipping it starts a fresh
    /// lease key. See `docs/mcp.md`.
    #[serde(default)]
    pub shared: bool,
}

/// MCP config file format: { "mcpServers": { "name": { command, args, env } } }
#[derive(Debug, Clone, serde::Deserialize)]
pub struct McpConfig {
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,
}

/// Discovered tool definition from an MCP server.
#[derive(Debug, Clone)]
pub(crate) struct McpToolDef {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) input_schema: serde_json::Value,
}

/// Hard byte bound on `mcp.json`.
pub const MCP_CONFIG_MAX_BYTES: u64 = 1024 * 1024;

/// Load MCP config from ~/.synaps-cli/mcp.json (or profile variant).
///
/// Bounded local read (Task 19): one `O_NOFOLLOW|O_NONBLOCK` handle,
/// opened-metadata regular-file check, capped read with growth rejection —
/// symlinks, FIFOs, and oversized files are refused. Failures log only the
/// static file name and bounded typed metadata, never raw path or parse
/// content. Semantics are otherwise unchanged: any rejection yields `None`.
pub fn load_mcp_config() -> Option<McpConfig> {
    let path = crate::config::resolve_read_path("mcp.json");
    let content = match descriptors::read_bounded_regular_file(&path, MCP_CONFIG_MAX_BYTES) {
        Ok(content) => content,
        Err(descriptors::DescriptorCacheError::NotFound) => return None,
        Err(err) => {
            // Static category only: Io Display strings can embed paths.
            let category = match err {
                descriptors::DescriptorCacheError::NotFound => "not_found",
                descriptors::DescriptorCacheError::NotRegularFile => "not_regular_file",
                descriptors::DescriptorCacheError::Oversize { .. } => "oversize",
                descriptors::DescriptorCacheError::Io(_) => "io",
                descriptors::DescriptorCacheError::Parse(_) => "parse",
                descriptors::DescriptorCacheError::Version(_) => "version",
            };
            tracing::warn!(
                file = "mcp.json",
                category,
                "Refusing unsafe MCP config read"
            );
            return None;
        }
    };
    let config = match serde_json::from_str::<McpConfig>(&content) {
        Ok(config) => config,
        Err(e) => {
            // Numeric/class parse metadata ONLY: serde Display can embed
            // hostile key/value excerpts.
            tracing::warn!(
                file = "mcp.json",
                class = ?e.classify(),
                line = e.line(),
                column = e.column(),
                "Failed to parse MCP config"
            );
            return None;
        }
    };
    if let Err(reason) = validate_mcp_config(&config) {
        // The WHOLE config fails closed on any structural violation.
        tracing::warn!(
            file = "mcp.json",
            reason,
            "Refusing structurally unsafe MCP config"
        );
        return None;
    }
    Some(config)
}

/// Structural bounds on `mcp.json` (Task 19 review): the 1 MiB byte cap
/// alone leaves server/env counts and string shapes unbounded. Violations
/// fail the WHOLE config closed with a static reason.
pub const MCP_CONFIG_MAX_SERVERS: usize = 32;
pub const MCP_CONFIG_MAX_ARGS: usize = 64;
pub const MCP_CONFIG_MAX_ENV_ENTRIES: usize = 64;
pub const MCP_CONFIG_MAX_SERVER_NAME_BYTES: usize = 64;
pub const MCP_CONFIG_MAX_COMMAND_BYTES: usize = 512;
pub const MCP_CONFIG_MAX_ARG_BYTES: usize = 4096;
pub const MCP_CONFIG_MAX_ENV_KEY_BYTES: usize = 128;
pub const MCP_CONFIG_MAX_ENV_VALUE_BYTES: usize = 4096;

/// Expand `${VAR}` / `${VAR:-default}` references in an `mcp.json` `env`
/// value against `lookup` (the parent process env at spawn time).
///
/// MCP children are spawned with a CLEARED environment (H3: never leak host
/// secrets such as `ANTHROPIC_API_KEY`/`AWS_*` to third-party servers). This
/// is the explicit, per-variable opt-in for forwarding a parent value — the
/// same `${VAR}` syntax Claude Code / Gemini CLI accept in their `mcpServers`
/// `env` maps, so configs stay portable. Semantics:
///   * `${VAR}`           → parent value, or "" when unset
///   * `${VAR:-fallback}` → parent value, or `fallback` when unset/empty
///   * anything else (incl. a lone `$`, `$VAR`, unterminated `${`) is literal
/// Only the FIRST expansion level is applied: a parent value containing `${…}`
/// is copied verbatim (no recursion, so no expansion loops).
pub fn expand_env_value(value: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            // Unterminated: keep the remainder literally.
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let inner = &after[..end];
        let (name, fallback) = match inner.split_once(":-") {
            Some((n, f)) => (n, Some(f)),
            None => (inner, None),
        };
        let is_ident = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_');
        if is_ident {
            match lookup(name) {
                Some(v) if !(v.is_empty() && fallback.is_some()) => out.push_str(&v),
                _ => out.push_str(fallback.unwrap_or("")),
            }
        } else {
            // Not a variable reference (e.g. `${}` or `${a-b}`): literal.
            out.push_str(&rest[start..start + 2 + end + 1]);
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn safe_str(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

/// Validate config structure before ANY use. Static reasons only — no
/// config content is ever echoed.
pub fn validate_mcp_config(config: &McpConfig) -> Result<(), &'static str> {
    if config.mcp_servers.len() > MCP_CONFIG_MAX_SERVERS {
        return Err("too_many_servers");
    }
    for (name, server) in &config.mcp_servers {
        if !safe_str(name, MCP_CONFIG_MAX_SERVER_NAME_BYTES) {
            return Err("invalid_server_name");
        }
        if !safe_str(&server.command, MCP_CONFIG_MAX_COMMAND_BYTES) {
            return Err("invalid_command");
        }
        if server.args.len() > MCP_CONFIG_MAX_ARGS {
            return Err("too_many_args");
        }
        for arg in &server.args {
            if arg.len() > MCP_CONFIG_MAX_ARG_BYTES || arg.chars().any(char::is_control) {
                return Err("invalid_arg");
            }
        }
        if server.env.len() > MCP_CONFIG_MAX_ENV_ENTRIES {
            return Err("too_many_env_entries");
        }
        for (key, value) in &server.env {
            if !safe_str(key, MCP_CONFIG_MAX_ENV_KEY_BYTES) {
                return Err("invalid_env_key");
            }
            if value.len() > MCP_CONFIG_MAX_ENV_VALUE_BYTES || value.chars().any(char::is_control) {
                return Err("invalid_env_value");
            }
        }
    }
    Ok(())
}

/// How engine boot treats configured MCP servers that have no
/// fingerprint-matching descriptor-cache entry (see [`setup_lazy_mcp`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum McpDiscovery {
    /// Discover them during boot, before the host is returned (bounded:
    /// in parallel, each under the seed timeout). In-process modes (TUI
    /// without daemon, `chat`, `agent`, `server`).
    #[default]
    Inline,
    /// Boot registers cached descriptors only; discovery of the missing
    /// servers runs later in a background task
    /// ([`crate::EngineHost::spawn_background_mcp_discovery`]), so a hung or
    /// dead server can never delay readiness. Used by the daemon, which must
    /// signal ready within the auto-spawner's bound.
    Background,
}

/// Upper bound for one server's boot-time discovery (spawn + initialize +
/// tools/list).
const SEED_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// How many missing servers are discovered at once. With N missing servers
/// boot discovery costs at most `ceil(N / DISCOVERY_CONCURRENCY)` seed
/// timeouts (one for any realistic N), never `N ×` one.
pub const DISCOVERY_CONCURRENCY: usize = 8;

/// Test seam for boot discovery — NOT user-facing configuration. Production
/// always uses [`McpDiscoveryOpts::default`] (20 s per server, system clock).
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct McpDiscoveryOpts {
    /// Per-server bound on spawn + initialize + tools/list.
    pub timeout: std::time::Duration,
    /// Clock override (unix seconds) for the negative-cache backoff; `None`
    /// uses the system clock.
    pub now_unix: Option<u64>,
}

impl Default for McpDiscoveryOpts {
    fn default() -> Self {
        Self {
            timeout: SEED_TIMEOUT,
            now_unix: None,
        }
    }
}

impl McpDiscoveryOpts {
    fn now_unix(&self) -> u64 {
        self.now_unix.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
        })
    }
}

type SharedRegistry = Arc<tokio::sync::RwLock<crate::ToolRegistry>>;

/// Set up MCP loading at engine boot, before any session tool set exists.
///
/// Register every configured server's cached descriptors as registry
/// entries (Task 19, spec §7.4). They are searchable and exactly activatable
/// under progressive disclosure, part of the core otherwise, and always
/// execution-gated; nothing is spawned until a leased execution. Servers
/// without a fingerprint-matching cache entry are discovered first, in
/// parallel ([`seed_missing_descriptors`]); one that fails contributes no
/// tools this run and is not retried until its backoff expires (see
/// [`descriptors::DiscoveryFailure`]). Descriptors are never invented from
/// config alone. The batch registers atomically; on failure MCP capabilities
/// fail closed to zero. Returns the number of tools registered.
pub async fn setup_lazy_mcp(registry: &SharedRegistry) -> usize {
    setup_lazy_mcp_with(registry, McpDiscoveryOpts::default()).await
}

/// [`setup_lazy_mcp`] with an injectable seed timeout and clock (tests).
#[doc(hidden)]
pub async fn setup_lazy_mcp_with(registry: &SharedRegistry, opts: McpDiscoveryOpts) -> usize {
    let Some(config) = load_nonempty_config() else {
        return 0;
    };
    let mut cache = load_cache_or_empty();
    if seed_missing_descriptors(&config, &cache, &opts).await > 0 {
        // Re-read through the validating loader: entries written by the seed
        // pass the same sanitisation as any other cache load.
        cache = load_cache_or_empty();
    }
    register_dormant(registry, &config, &cache, "boot").await
}

/// Background-mode phase 1 ([`McpDiscovery::Background`]): register the
/// cached descriptors only — never spawns — and hand back the discovery
/// still owed for servers without a fingerprint-matching entry (`None` when
/// there is none). Returns `(tools registered, pending discovery)`.
pub async fn setup_cached_mcp(registry: &SharedRegistry) -> (usize, Option<PendingMcpDiscovery>) {
    let Some(config) = load_nonempty_config() else {
        return (0, None);
    };
    let cache = load_cache_or_empty();
    let mut missing = HashMap::new();
    for (name, server) in &config.mcp_servers {
        let fingerprint = descriptors::server_config_fingerprint(server);
        let cached = cache
            .servers
            .get(name)
            .is_some_and(|entry| entry.fingerprint == fingerprint);
        if !cached {
            missing.insert(name.clone(), server.clone());
        }
    }
    let count = register_dormant(registry, &config, &cache, "cached").await;
    let pending = (!missing.is_empty() && descriptors::cache_writeback_enabled()).then_some(
        PendingMcpDiscovery {
            config: McpConfig {
                mcp_servers: missing,
            },
        },
    );
    (count, pending)
}

/// Discovery deferred by [`setup_cached_mcp`]: exactly the configured
/// servers (from the SAME config snapshot phase 1 used) that had no
/// fingerprint-matching cache entry, so none of them has registered tools.
#[derive(Debug)]
pub struct PendingMcpDiscovery {
    config: McpConfig,
}

impl PendingMcpDiscovery {
    /// Names of the servers still to be discovered.
    pub fn server_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.config.mcp_servers.keys().cloned().collect();
        names.sort();
        names
    }

    /// Discover the pending servers (parallel, bounded, negative-cached)
    /// and register ONLY their dormant tools into the shared `registry` with
    /// one atomic `try_register_batch`. Servers already registered at boot
    /// are never in this set, so nothing is registered twice. A rejected
    /// batch is logged and fails closed (none of the new tools appear);
    /// everything registered before is untouched.
    ///
    /// The registry is the host's shared catalog, so this mutates it after
    /// sessions may exist — the same precedent as late extension
    /// registration. With progressive disclosure on the new tools are
    /// dormant (searchable/activatable). With it off they are core tools, so
    /// every live session sends them from its NEXT request: one prompt-cache
    /// rewrite per live session, once.
    pub async fn run(self, registry: &SharedRegistry) -> usize {
        self.run_with(registry, McpDiscoveryOpts::default()).await
    }

    /// [`Self::run`] with an injectable seed timeout and clock (tests).
    #[doc(hidden)]
    pub async fn run_with(self, registry: &SharedRegistry, opts: McpDiscoveryOpts) -> usize {
        // Fresh read: another process may have recorded meanwhile (its
        // matching entry is then registered here without a spawn).
        let mut cache = load_cache_or_empty();
        if seed_missing_descriptors(&self.config, &cache, &opts).await > 0 {
            cache = load_cache_or_empty();
        }
        register_dormant(registry, &self.config, &cache, "background").await
    }
}

fn load_nonempty_config() -> Option<McpConfig> {
    load_mcp_config().filter(|config| !config.mcp_servers.is_empty())
}

fn load_cache_or_empty() -> descriptors::McpDescriptorCache {
    match descriptors::load_default_cache() {
        Ok(cache) => cache,
        Err(descriptors::DescriptorCacheError::NotFound) => descriptors::McpDescriptorCache::empty(),
        Err(err) => {
            tracing::warn!(error = %err,
                "Refusing unsafe MCP descriptor cache; MCP capabilities stay undiscoverable this run");
            descriptors::McpDescriptorCache::empty()
        }
    }
}

/// Register `config`'s fingerprint-matching cached descriptors as one
/// atomic batch; fail closed (zero) on rejection.
async fn register_dormant(
    registry: &SharedRegistry,
    config: &McpConfig,
    cache: &descriptors::McpDescriptorCache,
    phase: &'static str,
) -> usize {
    let server_count = config.mcp_servers.len();
    let dormant = descriptors::dormant_tools_for_config(config, cache);
    if dormant.is_empty() {
        tracing::info!(
            servers = server_count,
            phase,
            "MCP: no valid cached descriptors; MCP tools are not discoverable this run"
        );
        return 0;
    }
    match registry.write().await.try_register_batch(dormant) {
        Ok(count) => {
            tracing::info!(
                tools = count,
                servers = server_count,
                phase,
                "MCP: descriptor-backed tools registered (no process started)"
            );
            count
        }
        Err(e) => {
            tracing::warn!(error = %e, phase,
                "MCP descriptor batch rejected; failing closed with zero MCP capabilities");
            0
        }
    }
}

fn format_unix(secs: u64) -> String {
    i64::try_from(secs)
        .ok()
        .and_then(|s| chrono::DateTime::<chrono::Utc>::from_timestamp(s, 0))
        .map_or_else(|| secs.to_string(), |dt| dt.to_rfc3339())
}

/// Bounded one-time discovery for configured servers that have no
/// fingerprint-matching descriptor-cache entry (a new server, or a changed
/// command/args/env). Without an entry a server contributes no tools, and the
/// cache was only ever written after a leased call, which needs a tool to
/// call: a new server stayed invisible forever. Missing servers are
/// discovered concurrently (at most [`DISCOVERY_CONCURRENCY`] at once), each
/// started once, listed, recorded through the same locked, sanitising
/// `record_server_listing` the lease write-back uses, and stopped (children
/// are `kill_on_drop`, so a timeout cannot leak a process). A failure (error
/// or timeout) is recorded with `record_discovery_failure`; a server whose
/// failure under its CURRENT fingerprint is still inside its backoff is
/// skipped without spawning. Respects `SYNAPS_MCP_CACHE_WRITEBACK=0` (no
/// discovery). Returns how many servers were recorded.
async fn seed_missing_descriptors(
    config: &McpConfig,
    cache: &descriptors::McpDescriptorCache,
    opts: &McpDiscoveryOpts,
) -> usize {
    use futures::StreamExt;

    if !descriptors::cache_writeback_enabled() {
        return 0;
    }
    let now = opts.now_unix();
    let mut names: Vec<&String> = config.mcp_servers.keys().collect();
    names.sort();
    let mut pending = Vec::new();
    for name in names {
        let server = &config.mcp_servers[name];
        let fingerprint = descriptors::server_config_fingerprint(server);
        if cache
            .servers
            .get(name)
            .is_some_and(|entry| entry.fingerprint == fingerprint)
        {
            continue;
        }
        if let Some(failure) = cache.discovery_failures.get(name) {
            if failure.suppresses(&fingerprint, now) {
                let when = format_unix(failure.retry_after_unix());
                tracing::info!(
                    server = %name,
                    attempts = failure.attempts,
                    "MCP discovery for '{}' failed recently; retrying after {}",
                    name,
                    when
                );
                continue;
            }
        }
        pending.push((name.clone(), server.clone(), fingerprint));
    }
    if pending.is_empty() {
        return 0;
    }
    let opts = *opts;
    let outcomes: Vec<bool> = futures::stream::iter(
        pending
            .into_iter()
            .map(|(name, server, fingerprint)| discover_one(name, server, fingerprint, opts)),
    )
    .buffer_unordered(DISCOVERY_CONCURRENCY)
    .collect()
    .await;
    outcomes.into_iter().filter(|recorded| *recorded).count()
}

/// Discover one server (bounded by `opts.timeout`) and record the outcome:
/// its listing on success, a negative-cache entry on error or timeout.
/// Returns whether a listing was recorded.
async fn discover_one(
    name: String,
    server: McpServerConfig,
    fingerprint: String,
    opts: McpDiscoveryOpts,
) -> bool {
    let listing = tokio::time::timeout(opts.timeout, async {
        let mut conn = connection::McpConnection::start(&server).await?;
        let result = conn.list_tools().await;
        conn.start_kill();
        result
    })
    .await;
    let path = descriptors::default_cache_path();
    let defs = match listing {
        Ok(Ok(defs)) => defs,
        Ok(Err(err)) => {
            tracing::warn!(server = %name, error = %err, "MCP discovery failed; its tools stay undiscoverable until a retry");
            record_failure(path, name, fingerprint, opts.now_unix()).await;
            return false;
        }
        Err(_) => {
            tracing::warn!(server = %name, "MCP discovery timed out; its tools stay undiscoverable until a retry");
            record_failure(path, name, fingerprint, opts.now_unix()).await;
            return false;
        }
    };
    let tools: Vec<descriptors::CachedToolDescriptor> = defs
        .into_iter()
        .map(|def| descriptors::CachedToolDescriptor {
            name: def.name,
            description: def.description,
            input_schema: def.input_schema,
        })
        .collect();
    let server_name = name.clone();
    let written = tokio::task::spawn_blocking(move || {
        descriptors::record_server_listing(&path, &server_name, &fingerprint, &tools)
    })
    .await;
    match written {
        Ok(Ok(count)) => {
            tracing::info!(server = %name, tools = count, "MCP server discovered and cached");
            true
        }
        Ok(Err(err)) => {
            tracing::warn!(server = %name, error = %err, "MCP discovery could not be cached");
            false
        }
        Err(err) => {
            tracing::warn!(server = %name, error = %err, "MCP discovery cache write panicked");
            false
        }
    }
}

async fn record_failure(path: std::path::PathBuf, name: String, fingerprint: String, now: u64) {
    let server_name = name.clone();
    let written = tokio::task::spawn_blocking(move || {
        descriptors::record_discovery_failure(&path, &server_name, &fingerprint, now)
    })
    .await;
    match written {
        Ok(Ok(failure)) => tracing::info!(
            server = %name,
            attempts = failure.attempts,
            retry_after = %format_unix(failure.retry_after_unix()),
            "MCP discovery failure recorded"
        ),
        Ok(Err(err)) => {
            tracing::warn!(server = %name, error = %err, "MCP discovery failure could not be cached")
        }
        Err(err) => {
            tracing::warn!(server = %name, error = %err, "MCP discovery failure write panicked")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_mcp_config_deserialize() {
        let json_str = r#"{"mcpServers": {"test": {"command": "echo", "args": ["hi"]}}}"#;
        let config: McpConfig = serde_json::from_str(json_str).unwrap();

        assert_eq!(config.mcp_servers.len(), 1);
        assert!(config.mcp_servers.contains_key("test"));

        let server = &config.mcp_servers["test"];
        assert_eq!(server.command, "echo");
        assert_eq!(server.args, vec!["hi"]);
    }

    #[test]
    fn expand_env_value_forwards_parent_vars_and_defaults() {
        let lookup = |k: &str| match k {
            "BASE_URL" => Some("http://api:3000".to_string()),
            "EMPTY" => Some(String::new()),
            _ => None,
        };
        assert_eq!(expand_env_value("${BASE_URL}", lookup), "http://api:3000");
        assert_eq!(expand_env_value("${BASE_URL}/v1", lookup), "http://api:3000/v1");
        assert_eq!(expand_env_value("a=${BASE_URL};b=${BASE_URL}", lookup), "a=http://api:3000;b=http://api:3000");
        // Unset: empty (or the fallback).
        assert_eq!(expand_env_value("${MISSING}", lookup), "");
        assert_eq!(expand_env_value("${MISSING:-dflt}", lookup), "dflt");
        assert_eq!(expand_env_value("${EMPTY:-dflt}", lookup), "dflt");
        assert_eq!(expand_env_value("${EMPTY}", lookup), "");
        // Literal forms are untouched.
        assert_eq!(expand_env_value("plain", lookup), "plain");
        assert_eq!(expand_env_value("$BASE_URL", lookup), "$BASE_URL");
        assert_eq!(expand_env_value("${BASE_URL", lookup), "${BASE_URL");
        assert_eq!(expand_env_value("${}", lookup), "${}");
        assert_eq!(expand_env_value("${a-b}", lookup), "${a-b}");
        assert_eq!(expand_env_value("cost $5", lookup), "cost $5");
    }

    #[test]
    fn expand_env_value_does_not_recurse() {
        let lookup = |k: &str| match k {
            "OUTER" => Some("${INNER}".to_string()),
            "INNER" => Some("leak".to_string()),
            _ => None,
        };
        assert_eq!(expand_env_value("${OUTER}", lookup), "${INNER}");
    }

    #[test]
    fn test_mcp_config_empty_servers() {
        let json_str = r#"{"mcpServers": {}}"#;
        let config: McpConfig = serde_json::from_str(json_str).unwrap();

        assert_eq!(config.mcp_servers.len(), 0);
        assert!(config.mcp_servers.is_empty());
    }

    #[test]
    fn test_mcp_server_config_defaults() {
        let json_str = r#"{"command": "echo"}"#;
        let server_config: McpServerConfig = serde_json::from_str(json_str).unwrap();

        assert_eq!(server_config.command, "echo");
        assert_eq!(server_config.args, Vec::<String>::new());
        assert_eq!(server_config.env, HashMap::new());
    }

    #[test]
    fn test_mcp_config_deserialize_from_value() {
        let json_value = json!({
            "mcpServers": {
                "test": {
                    "command": "echo",
                    "args": ["hi"]
                }
            }
        });

        let config: McpConfig = serde_json::from_value(json_value).unwrap();

        assert_eq!(config.mcp_servers.len(), 1);
        assert!(config.mcp_servers.contains_key("test"));

        let server = &config.mcp_servers["test"];
        assert_eq!(server.command, "echo");
        assert_eq!(server.args, vec!["hi"]);
    }

    fn base_server() -> McpServerConfig {
        McpServerConfig {
            command: "/bin/true".to_string(),
            args: vec!["--flag".to_string()],
            env: HashMap::from([("KEY".to_string(), "value".to_string())]),
            shared: false,
        }
    }

    fn config_of(servers: Vec<(&str, McpServerConfig)>) -> McpConfig {
        McpConfig {
            mcp_servers: servers
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        }
    }

    #[test]
    fn validate_mcp_config_accepts_sane_configs() {
        assert!(validate_mcp_config(&config_of(vec![("srv", base_server())])).is_ok());
        assert!(validate_mcp_config(&config_of(vec![])).is_ok());
    }

    #[test]
    fn validate_mcp_config_fails_whole_config_closed_on_structural_violations() {
        // Too many servers.
        let many: Vec<(String, McpServerConfig)> = (0..=MCP_CONFIG_MAX_SERVERS)
            .map(|i| (format!("s{i}"), base_server()))
            .collect();
        let config = McpConfig {
            mcp_servers: many.into_iter().collect(),
        };
        assert_eq!(validate_mcp_config(&config), Err("too_many_servers"));

        // Hostile server name (control chars) and oversized name.
        let mut c = base_server();
        assert_eq!(
            validate_mcp_config(&config_of(vec![("evil\u{7}", c.clone())])),
            Err("invalid_server_name")
        );
        let long_name = "n".repeat(MCP_CONFIG_MAX_SERVER_NAME_BYTES + 1);
        assert_eq!(
            validate_mcp_config(&config_of(vec![(long_name.as_str(), c.clone())])),
            Err("invalid_server_name")
        );

        // Command: empty, oversized, control-bearing.
        c.command = String::new();
        assert_eq!(
            validate_mcp_config(&config_of(vec![("srv", c.clone())])),
            Err("invalid_command")
        );
        c.command = "x".repeat(MCP_CONFIG_MAX_COMMAND_BYTES + 1);
        assert_eq!(
            validate_mcp_config(&config_of(vec![("srv", c.clone())])),
            Err("invalid_command")
        );
        c.command = "bad\u{0}cmd".to_string();
        assert_eq!(
            validate_mcp_config(&config_of(vec![("srv", c.clone())])),
            Err("invalid_command")
        );

        // Args: count and shape.
        let mut c = base_server();
        c.args = vec!["a".to_string(); MCP_CONFIG_MAX_ARGS + 1];
        assert_eq!(
            validate_mcp_config(&config_of(vec![("srv", c.clone())])),
            Err("too_many_args")
        );
        c.args = vec!["ok".to_string(), "bad\u{1b}arg".to_string()];
        assert_eq!(
            validate_mcp_config(&config_of(vec![("srv", c.clone())])),
            Err("invalid_arg")
        );

        // Env: entry count, key shape, value shape.
        let mut c = base_server();
        c.env = (0..=MCP_CONFIG_MAX_ENV_ENTRIES)
            .map(|i| (format!("K{i}"), "v".to_string()))
            .collect();
        assert_eq!(
            validate_mcp_config(&config_of(vec![("srv", c.clone())])),
            Err("too_many_env_entries")
        );
        c.env = HashMap::from([(String::new(), "v".to_string())]);
        assert_eq!(
            validate_mcp_config(&config_of(vec![("srv", c.clone())])),
            Err("invalid_env_key")
        );
        c.env = HashMap::from([(
            "K".to_string(),
            "v".repeat(MCP_CONFIG_MAX_ENV_VALUE_BYTES + 1),
        )]);
        assert_eq!(
            validate_mcp_config(&config_of(vec![("srv", c.clone())])),
            Err("invalid_env_value")
        );
    }

    #[test]
    fn test_load_mcp_config_returns_some_or_none() {
        // This test verifies that load_mcp_config() returns either Some or None
        // depending on whether the config file exists
        let result = load_mcp_config();

        // Result can be either Some(config) or None - both are valid
        // depending on whether ~/.synaps-cli/mcp.json exists
        match result {
            Some(_config) => {
                // If file exists and parses correctly, we get a config
                // (mcp_servers can be empty — that's valid)
            }
            None => {
                // If file doesn't exist or fails to parse, we get None
                // This is expected behavior
            }
        }
    }
}
