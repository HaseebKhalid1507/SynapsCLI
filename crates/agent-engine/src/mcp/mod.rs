//! MCP (Model Context Protocol) integration — JSON-RPC client, tool bridging, lazy loading.
mod connection;
pub mod descriptors;
mod lazy;
mod tool;

use crate::ToolRegistry;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

pub use lazy::McpConnectTool;
pub use tool::McpTool;

/// MCP server configuration — matches claude-code/gemini-cli format.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
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

/// Load MCP config from ~/.synaps-cli/mcp.json (or profile variant).
pub fn load_mcp_config() -> Option<McpConfig> {
    let path = crate::config::resolve_read_path("mcp.json");
    if !path.exists() {
        return None;
    }

    let content = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str::<McpConfig>(&content) {
        Ok(config) => Some(config),
        Err(e) => {
            tracing::warn!("Failed to parse MCP config at {}: {}", path.display(), e);
            None
        }
    }
}

/// Connect to all configured MCP servers and register their tools.
/// Returns the number of tools registered.
pub async fn connect_mcp_servers(registry: &mut ToolRegistry) -> usize {
    let config = match load_mcp_config() {
        Some(c) => c,
        None => return 0,
    };

    let mut total_tools = 0;

    for (server_name, server_config) in &config.mcp_servers {
        tracing::info!(server = %server_name, command = %server_config.command, "Connecting to MCP server");

        match connection::McpConnection::start(server_config).await {
            Ok(mut conn) => {
                match conn.list_tools().await {
                    Ok(tools) => {
                        let tool_count = tools.len();
                        let connection = Arc::new(Mutex::new(conn));

                        for tool_def in tools {
                            // Prefix tool names with server name to avoid collisions
                            // e.g. "filesystem__read_file" for server "filesystem"
                            let prefixed_name = format!("ext__{}__{}", server_name, tool_def.name);
                            let tool_name_for_log = prefixed_name.clone();

                            let mcp_tool = McpTool {
                                tool_name: prefixed_name,
                                server_tool_name: tool_def.name.clone(),
                                server_name: server_name.clone(),
                                description: format!(
                                    "[MCP:{}] {}",
                                    server_name, tool_def.description
                                ),
                                input_schema: tool_def.input_schema,
                                connection: Arc::clone(&connection),
                            };

                            if let Err(e) = registry.try_register(Arc::new(mcp_tool)) {
                                tracing::warn!(
                                    server = %server_name,
                                    tool = %tool_name_for_log,
                                    error = %e,
                                    "Refusing to expose MCP tool the capability catalog could not record"
                                );
                                continue;
                            }
                            total_tools += 1;
                        }

                        tracing::info!(
                            server = %server_name,
                            tools = tool_count,
                            "MCP server connected — {} tools registered",
                            tool_count
                        );
                    }
                    Err(e) => {
                        tracing::error!(server = %server_name, error = %e, "Failed to list MCP tools");
                    }
                }
            }
            Err(e) => {
                tracing::error!(server = %server_name, error = %e, "Failed to connect to MCP server");
            }
        }
    }

    total_tools
}

/// Set up MCP loading at engine boot, before any session tool set exists.
///
/// Flag-off (`progressive_exact_only == false`): the legacy lazy path is
/// preserved byte-for-byte — register the `connect_mcp_server` gateway and
/// return the number of available servers.
///
/// Flag-on (Task 19, spec §7.4): NO gateway is registered (a server-wide
/// connect would bypass exact per-tool activation), and cached descriptors
/// become dormant deferred registry entries instead: searchable, exactly
/// activatable, execution-gated, spawning nothing until a leased execution.
/// A server without a fingerprint-matching cache entry contributes no
/// capabilities — descriptors are never invented from config alone and no
/// process is started to discover them. Returns the number of dormant tools
/// registered. The whole dormant batch registers atomically; on failure the
/// registry is unchanged and MCP capabilities fail closed to zero.
pub async fn setup_lazy_mcp(
    registry: &Arc<tokio::sync::RwLock<crate::ToolRegistry>>,
    progressive_exact_only: bool,
) -> usize {
    let config = match load_mcp_config() {
        Some(c) => c,
        None => return 0,
    };

    let server_count = config.mcp_servers.len();
    if server_count == 0 {
        return 0;
    }

    if progressive_exact_only {
        let cache = match descriptors::load_default_cache() {
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
        let dormant = descriptors::dormant_tools_for_config(&config, &cache);
        if dormant.is_empty() {
            tracing::info!(
                servers = server_count,
                "MCP exact mode: no valid cached descriptors; MCP tools are not discoverable this run"
            );
            return 0;
        }
        return match registry.write().await.try_register_batch(dormant) {
            Ok(count) => {
                tracing::info!(
                    tools = count,
                    servers = server_count,
                    "MCP exact mode: dormant descriptor-backed tools registered (no process started)"
                );
                count
            }
            Err(e) => {
                tracing::warn!(error = %e,
                    "MCP dormant descriptor batch rejected; failing closed with zero MCP capabilities");
                0
            }
        };
    }

    let server_names: Vec<&str> = config.mcp_servers.keys().map(|s| s.as_str()).collect();
    tracing::info!(servers = ?server_names, "MCP lazy loading: {} servers available", server_count);

    let connect_tool = McpConnectTool::new(config.mcp_servers);

    if let Err(e) = registry.write().await.try_register(Arc::new(connect_tool)) {
        tracing::warn!(error = %e, "Refusing to expose MCP gateway the capability catalog could not record");
        return 0;
    }

    server_count
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
