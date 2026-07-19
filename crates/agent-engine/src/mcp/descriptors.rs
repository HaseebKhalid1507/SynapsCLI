//! Task 19 (Commit A) — safe local MCP descriptor cache and dormant
//! deferred-tool integration (spec §7.4).
//!
//! Before exact selection, MCP capability knowledge comes ONLY from bounded
//! local state: the user's `mcp.json` server config plus this descriptor
//! cache (`mcp-descriptors.json`), treated as pre-existing operator-managed
//! local state. Loading NEVER spawns a process or touches the network, and
//! descriptors are never invented from server config alone: a server without
//! a fingerprint-matching cache entry contributes no capabilities. This task
//! deliberately adds no cache-refresh path from live server responses — a
//! server-wide legacy connection is not exact-tool authorized and must not
//! become a cache-poisoning bridge.
//!
//! Under progressive tool disclosure, cached descriptors become truthful
//! DORMANT registry entries: live tools-map/catalog entries with a stable
//! identity, schema, and digest whose implementation is deferred
//! ([`DeferredMcpTool`]). Exact activation therefore never mutates the
//! catalog generation; execution is separately authorization-gated and
//! (lease lifecycle commit) acquires a session-scoped runtime lease before
//! any process starts.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};
use thiserror::Error;

use super::{McpConfig, McpServerConfig};
use crate::tools::catalog::SchemaDigest;
use crate::tools::{Tool, ToolContext, ToolOrigin};
use agent_core::BoundedText;

/// File name of the descriptor cache, resolved through the profile-aware
/// config path helper so profiles cannot collide on one cache file.
pub const DESCRIPTOR_CACHE_FILE: &str = "mcp-descriptors.json";
/// Supported cache format version. Anything else fails typed.
pub const CACHE_FORMAT_VERSION: u32 = 1;
/// Hard byte bound on the cache file — larger files are rejected unread.
pub const CACHE_MAX_BYTES: u64 = 1024 * 1024;
/// Maximum retained descriptors per server entry.
pub const SERVER_MAX_TOOLS: usize = 256;
/// Maximum bytes of a cached tool name (also: nonempty, no control chars).
pub const TOOL_NAME_MAX_BYTES: usize = 128;
/// Byte bound applied to cached descriptions on load.
pub const TOOL_DESCRIPTION_MAX_BYTES: usize = 1024;
/// Maximum serialized bytes of one cached input schema.
pub const TOOL_SCHEMA_MAX_BYTES: usize = 64 * 1024;
/// Maximum bytes of a server key in the cache file.
const SERVER_NAME_MAX_BYTES: usize = 64;
/// Byte bound for echoing parse failures into typed errors.
const ERROR_ECHO_MAX_BYTES: usize = 160;

/// Deterministic fingerprint of one server's local launch configuration
/// (command, args, env). Cached descriptors are only trusted while the
/// current config still fingerprints identically; any drift invalidates
/// them. Pure local computation — no process, no network.
pub fn server_config_fingerprint(config: &McpServerConfig) -> String {
    let env: BTreeMap<&str, &str> = config
        .env
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    SchemaDigest::of_schema(&json!({
        "command": config.command,
        "args": config.args,
        "env": env,
    }))
    .as_hex()
    .to_string()
}

/// One cached MCP tool descriptor: exactly the fields a live `tools/list`
/// would provide, pinned locally so activation can validate the live
/// listing against operator-known expectations.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CachedToolDescriptor {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub input_schema: Value,
}

/// Cached descriptors for one configured server, keyed to the exact config
/// fingerprint they were recorded under.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CachedServerDescriptors {
    pub fingerprint: String,
    #[serde(default)]
    pub tools: Vec<CachedToolDescriptor>,
}

/// Versioned on-disk descriptor cache model.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct McpDescriptorCache {
    pub version: u32,
    #[serde(default)]
    pub servers: BTreeMap<String, CachedServerDescriptors>,
}

impl McpDescriptorCache {
    pub fn empty() -> Self {
        Self {
            version: CACHE_FORMAT_VERSION,
            servers: BTreeMap::new(),
        }
    }
}

/// Typed descriptor-cache boundary failures. All fail closed: a rejected
/// cache yields ZERO dormant capabilities, never a partial or guessed set.
#[derive(Debug, Error)]
pub enum DescriptorCacheError {
    #[error("descriptor cache not found")]
    NotFound,
    #[error("descriptor cache is not a regular file (symlinks and special files are rejected)")]
    NotRegularFile,
    #[error("descriptor cache is {actual} bytes, over the {max} byte bound")]
    Oversize { actual: u64, max: u64 },
    #[error("failed to read descriptor cache: {0}")]
    Io(String),
    #[error("descriptor cache is not valid for the expected shape: {0}")]
    Parse(String),
    #[error("unsupported descriptor cache version {0}")]
    Version(u32),
}

fn valid_tool_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= TOOL_NAME_MAX_BYTES && !name.chars().any(char::is_control)
}

fn valid_server_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= SERVER_NAME_MAX_BYTES && !name.chars().any(char::is_control)
}

/// Load and sanitize a descriptor cache from an explicit path. Boundary
/// rules: exactly ONE handle is opened (`O_NOFOLLOW|O_NONBLOCK` on Unix, so
/// a symlink or FIFO at the path is refused without following/blocking) and
/// ALL checks run against that opened handle's metadata — no
/// metadata-then-reopen TOCTOU. The opened file must be regular and within
/// [`CACHE_MAX_BYTES`]; the read itself is capped via `Read::take` and a
/// post-read length check rejects concurrent growth past the bound. The
/// content must be valid JSON of the supported version. Individual hostile
/// descriptor entries (invalid name, non-object schema, oversized schema,
/// duplicates) are dropped deterministically; descriptions are bounded.
/// Never spawns, never touches the network.
pub fn load_cache_from(path: &Path) -> Result<McpDescriptorCache, DescriptorCacheError> {
    use std::io::Read;

    let map_io =
        |err: &std::io::Error| BoundedText::new(&err.to_string(), ERROR_ECHO_MAX_BYTES).text;

    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|err| {
                if err.raw_os_error() == Some(libc::ELOOP) {
                    DescriptorCacheError::NotRegularFile
                } else if err.kind() == std::io::ErrorKind::NotFound {
                    DescriptorCacheError::NotFound
                } else if err.kind() == std::io::ErrorKind::IsADirectory {
                    DescriptorCacheError::NotRegularFile
                } else {
                    DescriptorCacheError::Io(map_io(&err))
                }
            })?
    };
    #[cfg(not(unix))]
    let file = {
        // Best-effort symlink refusal first, then re-verify on the opened
        // handle's metadata below (regular-file check).
        let meta = std::fs::symlink_metadata(path).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                DescriptorCacheError::NotFound
            } else {
                DescriptorCacheError::Io(map_io(&err))
            }
        })?;
        if meta.file_type().is_symlink() {
            return Err(DescriptorCacheError::NotRegularFile);
        }
        std::fs::File::open(path).map_err(|err| DescriptorCacheError::Io(map_io(&err)))?
    };

    // All trust decisions read the OPENED handle's metadata.
    let meta = file
        .metadata()
        .map_err(|err| DescriptorCacheError::Io(map_io(&err)))?;
    if !meta.file_type().is_file() {
        return Err(DescriptorCacheError::NotRegularFile);
    }
    if meta.len() > CACHE_MAX_BYTES {
        return Err(DescriptorCacheError::Oversize {
            actual: meta.len(),
            max: CACHE_MAX_BYTES,
        });
    }
    let mut content = String::new();
    file.take(CACHE_MAX_BYTES + 1)
        .read_to_string(&mut content)
        .map_err(|err| DescriptorCacheError::Io(map_io(&err)))?;
    if content.len() as u64 > CACHE_MAX_BYTES {
        // The file grew past the bound between metadata and read.
        return Err(DescriptorCacheError::Oversize {
            actual: content.len() as u64,
            max: CACHE_MAX_BYTES,
        });
    }
    let raw: McpDescriptorCache = serde_json::from_str(&content).map_err(|err| {
        DescriptorCacheError::Parse(BoundedText::new(&err.to_string(), ERROR_ECHO_MAX_BYTES).text)
    })?;
    if raw.version != CACHE_FORMAT_VERSION {
        return Err(DescriptorCacheError::Version(raw.version));
    }

    let mut sanitized = McpDescriptorCache::empty();
    for (server, entry) in raw.servers {
        if !valid_server_name(&server) {
            tracing::warn!(server = %BoundedText::new(&server, SERVER_NAME_MAX_BYTES).text,
                "Dropping descriptor cache entry with invalid server name");
            continue;
        }
        let mut seen: HashSet<String> = HashSet::new();
        let mut tools = Vec::new();
        for descriptor in entry.tools.into_iter() {
            if tools.len() >= SERVER_MAX_TOOLS {
                break;
            }
            if !valid_tool_name(&descriptor.name) {
                tracing::warn!(server = %server, "Dropping cached MCP descriptor with invalid name");
                continue;
            }
            if !seen.insert(descriptor.name.clone()) {
                tracing::warn!(server = %server, tool = %descriptor.name,
                    "Dropping duplicate cached MCP descriptor (keeping first)");
                continue;
            }
            if !descriptor.input_schema.is_object() {
                tracing::warn!(server = %server, tool = %descriptor.name,
                    "Dropping cached MCP descriptor with non-object schema");
                continue;
            }
            let schema_len = serde_json::to_vec(&descriptor.input_schema)
                .map(|bytes| bytes.len())
                .unwrap_or(usize::MAX);
            if schema_len > TOOL_SCHEMA_MAX_BYTES {
                tracing::warn!(server = %server, tool = %descriptor.name,
                    "Dropping cached MCP descriptor with oversized schema");
                continue;
            }
            tools.push(CachedToolDescriptor {
                name: descriptor.name,
                description: BoundedText::new(&descriptor.description, TOOL_DESCRIPTION_MAX_BYTES)
                    .text,
                input_schema: descriptor.input_schema,
            });
        }
        sanitized.servers.insert(
            server,
            CachedServerDescriptors {
                fingerprint: entry.fingerprint,
                tools,
            },
        );
    }
    Ok(sanitized)
}

/// Profile-aware default cache path. Uses the same profile FALLBACK
/// semantics as every other config read (`resolve_read_path`): the profile
/// copy is used when it exists, otherwise the shared base file. A shared
/// base cache can therefore be visible to multiple profiles; the per-server
/// config fingerprint still gates trust in every entry.
pub fn default_cache_path() -> PathBuf {
    crate::config::resolve_read_path(DESCRIPTOR_CACHE_FILE)
}

/// Load the descriptor cache from the profile-resolved default location.
pub fn load_default_cache() -> Result<McpDescriptorCache, DescriptorCacheError> {
    load_cache_from(&default_cache_path())
}

/// Atomically persist a descriptor cache with private-filesystem policy
/// (spec §5.4): parent directory ensured `0700`, temp file created
/// `create_new` at mode `0600` (never create-then-chmod), then renamed over
/// the target; symlinks at the target are refused typed. Used to
/// seed/maintain operator-local state (and by tests); this task adds no
/// production writer fed from live server responses.
pub fn store_cache_at(path: &Path, cache: &McpDescriptorCache) -> Result<(), DescriptorCacheError> {
    use agent_core::core::private_fs::{ensure_private_dir, write_atomic_private, PrivateFsError};

    let bytes = serde_json::to_vec_pretty(cache).map_err(|err| {
        DescriptorCacheError::Parse(BoundedText::new(&err.to_string(), ERROR_ECHO_MAX_BYTES).text)
    })?;
    let map_fs = |err: PrivateFsError| match err {
        PrivateFsError::SymlinkRefused(_) => DescriptorCacheError::NotRegularFile,
        PrivateFsError::Io(io) => {
            DescriptorCacheError::Io(BoundedText::new(&io.to_string(), ERROR_ECHO_MAX_BYTES).text)
        }
    };
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            ensure_private_dir(parent).map_err(map_fs)?;
        }
    }
    write_atomic_private(path, &bytes).map_err(map_fs)
}

/// A dormant, descriptor-backed MCP tool. Registered like any live tool
/// (stable identity, schema, digest — so wire resolution, discovery, exact
/// grants, and the execution gate all work unchanged), but its
/// implementation is DEFERRED: executing it requires the session MCP lease
/// capability, which starts only this tool's server after the execution
/// gate has already authorized the call. Without that capability it fails
/// typed and spawns nothing.
pub struct DeferredMcpTool {
    server_name: String,
    runtime_name: String,
    server_tool_name: String,
    description: String,
    input_schema: Value,
    expected_fingerprint: String,
    expected_digest: SchemaDigest,
}

impl DeferredMcpTool {
    fn new(server: &str, descriptor: &CachedToolDescriptor, fingerprint: &str) -> Self {
        Self {
            server_name: server.to_string(),
            // Naming parity with legacy live registration ("ext__srv__tool")
            // keeps wire naming stable across dormant and connected modes.
            runtime_name: format!("ext__{}__{}", server, descriptor.name),
            server_tool_name: descriptor.name.clone(),
            description: format!("[MCP:{}] {}", server, descriptor.description),
            input_schema: descriptor.input_schema.clone(),
            expected_fingerprint: fingerprint.to_string(),
            expected_digest: SchemaDigest::of_schema(&descriptor.input_schema),
        }
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    pub fn server_tool_name(&self) -> &str {
        &self.server_tool_name
    }

    /// Config fingerprint the pinned descriptor was recorded under.
    pub fn expected_fingerprint(&self) -> &str {
        &self.expected_fingerprint
    }

    /// Digest of the pinned descriptor schema — equal by construction to
    /// this tool's catalog record digest, so the live listing check and the
    /// grant digest check validate the same value.
    pub fn expected_digest(&self) -> &SchemaDigest {
        &self.expected_digest
    }
}

#[async_trait::async_trait]
impl Tool for DeferredMcpTool {
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Mcp {
            server_id: self.server_name.clone(),
            server_tool_name: self.server_tool_name.clone(),
        }
    }

    fn name(&self) -> &str {
        &self.runtime_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.input_schema.clone()
    }

    async fn execute(&self, _params: Value, _ctx: ToolContext) -> crate::Result<String> {
        // Lease-manager execution arrives with the lease lifecycle commit;
        // until wired, a deferred tool NEVER starts a process.
        Err(crate::RuntimeError::Tool(format!(
            "MCP tool '{}' (server '{}') is activation-deferred and no MCP session \
             lease manager is available in this context; no process was started",
            self.runtime_name, self.server_name
        )))
    }
}

/// Build the dormant deferred tools for the current config from the cache.
/// Deterministic order (sorted server names, cached tool order). Only
/// servers present in the CURRENT config whose cache entry fingerprint
/// matches the current config fingerprint contribute tools; everything else
/// is skipped with a warning. Never spawns, never touches the network.
pub fn dormant_tools_for_config(
    config: &McpConfig,
    cache: &McpDescriptorCache,
) -> Vec<Arc<dyn Tool>> {
    let mut servers: Vec<(&String, &McpServerConfig)> = config.mcp_servers.iter().collect();
    servers.sort_by(|a, b| a.0.cmp(b.0));

    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    for (server, server_config) in servers {
        let Some(entry) = cache.servers.get(server) else {
            tracing::debug!(server = %server,
                "MCP server has no cached descriptors; it stays undiscoverable (no process is started to ask)");
            continue;
        };
        let current = server_config_fingerprint(server_config);
        if entry.fingerprint != current {
            tracing::warn!(server = %server,
                "MCP descriptor cache fingerprint does not match the current config; cached descriptors are invalid");
            continue;
        }
        for descriptor in &entry.tools {
            tools.push(Arc::new(DeferredMcpTool::new(
                server,
                descriptor,
                &entry.fingerprint,
            )));
        }
    }
    tools
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::os::unix::fs::PermissionsExt;

    /// Restores the previous process umask on drop, even on panic.
    struct UmaskGuard {
        old: libc::mode_t,
    }
    impl UmaskGuard {
        fn set(mask: libc::mode_t) -> Self {
            Self {
                old: unsafe { libc::umask(mask) },
            }
        }
    }
    impl Drop for UmaskGuard {
        fn drop(&mut self) {
            unsafe {
                libc::umask(self.old);
            }
        }
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// Private modes must be application-enforced (spec §5.4), never an
    /// accident of the process umask: under a fully permissive umask the
    /// cache file must still be 0600 and its created parent directory 0700.
    #[test]
    #[serial(umask)]
    fn store_is_private_even_under_permissive_umask() {
        let base = std::env::temp_dir().join(format!("synaps-mcp-umask-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let _guard = UmaskGuard::set(0);

        let path = base.join("nested").join(DESCRIPTOR_CACHE_FILE);
        store_cache_at(&path, &McpDescriptorCache::empty()).unwrap();

        assert_eq!(mode_of(&path), 0o600, "cache file must be private");
        assert_eq!(
            mode_of(&base.join("nested")),
            0o700,
            "created cache dir must be private"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
