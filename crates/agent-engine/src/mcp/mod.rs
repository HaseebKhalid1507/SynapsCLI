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

/// Set up MCP loading at engine boot, before any session tool set exists.
///
/// Register every configured server's cached descriptors as registry
/// entries (Task 19, spec §7.4). They are searchable and exactly activatable
/// under progressive disclosure, part of the core otherwise, and always
/// execution-gated; nothing is spawned until a leased execution. A server
/// without a fingerprint-matching cache entry is discovered once first
/// ([`seed_missing_descriptors`]); if that fails it contributes no tools this
/// run. Descriptors are never invented from config alone. The batch registers
/// atomically; on failure MCP capabilities fail closed to zero. Returns the
/// number of tools registered.
pub async fn setup_lazy_mcp(registry: &Arc<tokio::sync::RwLock<crate::ToolRegistry>>) -> usize {
    let config = match load_mcp_config() {
        Some(c) => c,
        None => return 0,
    };

    let server_count = config.mcp_servers.len();
    if server_count == 0 {
        return 0;
    }

    let load = || match descriptors::load_default_cache() {
        Ok(cache) => cache,
        Err(descriptors::DescriptorCacheError::NotFound) => {
            descriptors::McpDescriptorCache::empty()
        }
        Err(err) => {
            tracing::warn!(error = %err,
                "Refusing unsafe MCP descriptor cache; MCP capabilities stay undiscoverable this run");
            descriptors::McpDescriptorCache::empty()
        }
    };
    let mut cache = load();
    if seed_missing_descriptors(&config, &cache).await > 0 {
        // Re-read through the validating loader: entries written by the seed
        // pass the same sanitisation as any other cache load.
        cache = load();
    }
    let dormant = descriptors::dormant_tools_for_config(&config, &cache);
    if dormant.is_empty() {
        tracing::info!(
            servers = server_count,
            "MCP: no valid cached descriptors; MCP tools are not discoverable this run"
        );
        return 0;
    }
    match registry.write().await.try_register_batch(dormant) {
        Ok(count) => {
            tracing::info!(
                tools = count,
                servers = server_count,
                "MCP: descriptor-backed tools registered (no process started)"
            );
            count
        }
        Err(e) => {
            tracing::warn!(error = %e,
                "MCP descriptor batch rejected; failing closed with zero MCP capabilities");
            0
        }
    }
}

/// Upper bound for one server's boot-time discovery (spawn + initialize +
/// tools/list).
const SEED_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Bounded one-time discovery for configured servers that have no
/// fingerprint-matching descriptor-cache entry (a new server, or a changed
/// command/args/env). Without an entry a server contributes no tools, and the
/// cache was only ever written after a leased call, which needs a tool to
/// call: a new server stayed invisible forever. Each missing server is
/// started once, listed, recorded through the same locked, sanitising
/// `record_server_listing` the lease write-back uses, and stopped (children
/// are `kill_on_drop`, so a timeout cannot leak a process). Respects
/// `SYNAPS_MCP_CACHE_WRITEBACK=0` (no discovery). Returns how many servers
/// were recorded.
async fn seed_missing_descriptors(
    config: &McpConfig,
    cache: &descriptors::McpDescriptorCache,
) -> usize {
    if !descriptors::cache_writeback_enabled() {
        return 0;
    }
    let mut recorded = 0;
    for (name, server) in &config.mcp_servers {
        let fingerprint = descriptors::server_config_fingerprint(server);
        if cache
            .servers
            .get(name)
            .is_some_and(|entry| entry.fingerprint == fingerprint)
        {
            continue;
        }
        let listing = tokio::time::timeout(SEED_TIMEOUT, async {
            let mut conn = connection::McpConnection::start(server).await?;
            let result = conn.list_tools().await;
            conn.start_kill();
            result
        })
        .await;
        let defs = match listing {
            Ok(Ok(defs)) => defs,
            Ok(Err(err)) => {
                tracing::warn!(server = %name, error = %err, "MCP discovery failed; its tools stay undiscoverable this run");
                continue;
            }
            Err(_) => {
                tracing::warn!(server = %name, "MCP discovery timed out; its tools stay undiscoverable this run");
                continue;
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
        let path = descriptors::default_cache_path();
        let server_name = name.clone();
        let written = tokio::task::spawn_blocking(move || {
            descriptors::record_server_listing(&path, &server_name, &fingerprint, &tools)
        })
        .await;
        match written {
            Ok(Ok(count)) => {
                tracing::info!(server = %name, tools = count, "MCP server discovered and cached");
                recorded += 1;
            }
            Ok(Err(err)) => {
                tracing::warn!(server = %name, error = %err, "MCP discovery could not be cached")
            }
            Err(err) => {
                tracing::warn!(server = %name, error = %err, "MCP discovery cache write panicked")
            }
        }
    }
    recorded
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
