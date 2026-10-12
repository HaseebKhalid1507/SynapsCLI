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
///
/// `shared` is deliberately EXCLUDED: it changes the lease key, not the
/// launched process, so flipping it must not discard a valid listing.
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
    /// Negative cache for boot discovery: servers whose last discovery
    /// attempt failed (error or timeout), keyed by server name. Additive and
    /// optional on disk (omitted when empty, defaulted when absent), so the
    /// format version is unchanged and older readers — which do not deny
    /// unknown fields — simply ignore it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub discovery_failures: BTreeMap<String, DiscoveryFailure>,
}

impl McpDescriptorCache {
    pub fn empty() -> Self {
        Self {
            version: CACHE_FORMAT_VERSION,
            servers: BTreeMap::new(),
            discovery_failures: BTreeMap::new(),
        }
    }
}

/// Maximum retained discovery-failure entries (oldest attempts evicted).
pub const DISCOVERY_FAILURES_MAX: usize = 64;
/// Byte bound on a recorded failure fingerprint (a hex digest in practice).
const FINGERPRINT_MAX_BYTES: usize = 128;
/// First retry delay after a failed boot discovery.
pub const DISCOVERY_BACKOFF_BASE_SECS: u64 = 60 * 60;
/// Retry delay cap after repeated failed boot discoveries.
pub const DISCOVERY_BACKOFF_MAX_SECS: u64 = 24 * 60 * 60;

/// One server's failed boot-discovery record: the launch fingerprint it
/// failed under, when it was last tried (unix seconds) and how many
/// consecutive attempts under that fingerprint failed.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiscoveryFailure {
    pub fingerprint: String,
    pub last_attempt_unix: u64,
    pub attempts: u32,
}

impl DiscoveryFailure {
    /// `1 h × 2^(attempts−1)`, capped at 24 h.
    pub fn backoff_secs(&self) -> u64 {
        let exp = self.attempts.max(1).saturating_sub(1).min(16);
        (DISCOVERY_BACKOFF_BASE_SECS << exp).min(DISCOVERY_BACKOFF_MAX_SECS)
    }

    /// Unix second from which the server may be retried.
    pub fn retry_after_unix(&self) -> u64 {
        self.last_attempt_unix.saturating_add(self.backoff_secs())
    }

    /// Whether boot discovery for a server currently fingerprinting as
    /// `fingerprint` should be skipped at `now_unix`. A changed fingerprint
    /// (config edit) retries immediately; so does a record stamped in the
    /// future (the clock moved backwards), rather than stretching the backoff.
    pub fn suppresses(&self, fingerprint: &str, now_unix: u64) -> bool {
        self.fingerprint == fingerprint
            && self.last_attempt_unix <= now_unix
            && now_unix < self.retry_after_unix()
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
    let content = read_bounded_regular_file(path, CACHE_MAX_BYTES)?;
    let raw: McpDescriptorCache = serde_json::from_str(&content).map_err(|err| {
        DescriptorCacheError::Parse(BoundedText::new(&err.to_string(), ERROR_ECHO_MAX_BYTES).text)
    })?;
    if raw.version != CACHE_FORMAT_VERSION {
        return Err(DescriptorCacheError::Version(raw.version));
    }
    sanitize_cache(raw)
}

/// Shared bounded local-file boundary (Task 19): exactly ONE handle is
/// opened (`O_NOFOLLOW|O_NONBLOCK` on Unix — symlinks and FIFOs refused
/// without following/blocking) and all checks run against that opened
/// handle's metadata; the read is capped via `Read::take` with a post-read
/// growth check. Used for both the descriptor cache and `mcp.json`.
pub(crate) fn read_bounded_regular_file(
    path: &Path,
    max_bytes: u64,
) -> Result<String, DescriptorCacheError> {
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
                } else if err.raw_os_error() == Some(libc::EISDIR) {
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
    if meta.len() > max_bytes {
        return Err(DescriptorCacheError::Oversize {
            actual: meta.len(),
            max: max_bytes,
        });
    }
    let mut content = String::new();
    file.take(max_bytes + 1)
        .read_to_string(&mut content)
        .map_err(|err| DescriptorCacheError::Io(map_io(&err)))?;
    if content.len() as u64 > max_bytes {
        // The file grew past the bound between metadata and read.
        return Err(DescriptorCacheError::Oversize {
            actual: content.len() as u64,
            max: max_bytes,
        });
    }
    Ok(content)
}

fn sanitize_cache(raw: McpDescriptorCache) -> Result<McpDescriptorCache, DescriptorCacheError> {
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
    sanitized.discovery_failures = sanitize_discovery_failures(raw.discovery_failures);
    Ok(sanitized)
}

/// Bound the negative cache like the rest of the file: valid server names,
/// bounded control-free fingerprints, `attempts >= 1`, and at most
/// [`DISCOVERY_FAILURES_MAX`] entries (the most recent attempts are kept).
fn sanitize_discovery_failures(
    raw: BTreeMap<String, DiscoveryFailure>,
) -> BTreeMap<String, DiscoveryFailure> {
    let mut kept: Vec<(String, DiscoveryFailure)> = raw
        .into_iter()
        .filter(|(server, failure)| {
            let ok = valid_server_name(server)
                && !failure.fingerprint.is_empty()
                && failure.fingerprint.len() <= FINGERPRINT_MAX_BYTES
                && !failure.fingerprint.chars().any(char::is_control);
            if !ok {
                tracing::warn!("Dropping invalid MCP discovery-failure cache entry");
            }
            ok
        })
        .map(|(server, mut failure)| {
            failure.attempts = failure.attempts.max(1);
            (server, failure)
        })
        .collect();
    if kept.len() > DISCOVERY_FAILURES_MAX {
        // Newest first; ties broken by name for determinism.
        kept.sort_by(|a, b| {
            b.1.last_attempt_unix
                .cmp(&a.1.last_attempt_unix)
                .then_with(|| a.0.cmp(&b.0))
        });
        kept.truncate(DISCOVERY_FAILURES_MAX);
    }
    kept.into_iter().collect()
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

/// `SYNAPS_MCP_CACHE_WRITEBACK=0` disables [`record_server_listing`].
pub fn cache_writeback_enabled() -> bool {
    !matches!(
        std::env::var("SYNAPS_MCP_CACHE_WRITEBACK").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

/// Write-back after a successful, lease-authorized `tools/list` (daemon-mode
/// phase 2, C4): merge `tools` into the cache at `path` under `server`,
/// keyed by the launch `fingerprint`, so the NEXT boot's `setup_lazy_mcp`
/// registers the same dormant descriptors without spawning anything (stable
/// prompt-cache prefix across daemon restarts).
///
/// Only the exact-lease path calls this — the listing it records comes from
/// a server the operator configured, started for an already gate-authorized
/// call. The legacy server-wide gateway never writes here (cache-poisoning
/// bridge, see module docs).
///
/// Concurrency: exclusive `fs4` lock on `<path>.lock` for the whole
/// read-merge-write, then [`store_cache_at`] (0600, atomic rename). A cache
/// that fails to load (corrupt/oversize/version) is REPLACED by a fresh one
/// carrying only this server — the reader refuses such a file anyway.
/// Entries are sanitized through the same bounds as a load, so a hostile
/// listing cannot write what a load would reject. A recorded listing also
/// clears the server's boot-discovery failure entry, if any. Returns the
/// number of descriptors recorded for `server`.
pub fn record_server_listing(
    path: &Path,
    server: &str,
    fingerprint: &str,
    tools: &[CachedToolDescriptor],
) -> Result<usize, DescriptorCacheError> {
    if !valid_server_name(server) {
        return Err(DescriptorCacheError::Parse("invalid server name".to_string()));
    }
    let cache = update_cache_locked(path, |cache| {
        let entry = CachedServerDescriptors {
            fingerprint: fingerprint.to_string(),
            tools: tools.to_vec(),
        };
        cache.servers.insert(server.to_string(), entry);
        // A successful listing clears any recorded discovery failure.
        cache.discovery_failures.remove(server);
    })?;
    Ok(cache.servers.get(server).map_or(0, |e| e.tools.len()))
}

/// Negative-cache sibling of [`record_server_listing`]: record that boot
/// discovery of `server` under `fingerprint` failed at `now_unix`. Same
/// locked read-merge-write and sanitisation. Consecutive failures under the
/// same fingerprint increment `attempts` (driving the backoff); a different
/// fingerprint restarts at 1. Returns the stored record.
pub fn record_discovery_failure(
    path: &Path,
    server: &str,
    fingerprint: &str,
    now_unix: u64,
) -> Result<DiscoveryFailure, DescriptorCacheError> {
    if !valid_server_name(server) {
        return Err(DescriptorCacheError::Parse("invalid server name".to_string()));
    }
    let cache = update_cache_locked(path, |cache| {
        let attempts = cache
            .discovery_failures
            .get(server)
            .filter(|prior| prior.fingerprint == fingerprint)
            .map_or(1, |prior| prior.attempts.saturating_add(1));
        cache.discovery_failures.insert(
            server.to_string(),
            DiscoveryFailure {
                fingerprint: fingerprint.to_string(),
                last_attempt_unix: now_unix,
                attempts,
            },
        );
    })?;
    cache
        .discovery_failures
        .get(server)
        .cloned()
        .ok_or_else(|| DescriptorCacheError::Parse("discovery failure not retained".to_string()))
}

/// Shared locked read-merge-write for the cache writers: exclusive `fs4`
/// lock on `<path>.lock`, load (an unreadable cache is replaced by a fresh
/// one), apply `mutate`, sanitise, [`store_cache_at`]. Returns the stored
/// (sanitised) cache.
fn update_cache_locked(
    path: &Path,
    mutate: impl FnOnce(&mut McpDescriptorCache),
) -> Result<McpDescriptorCache, DescriptorCacheError> {
    use fs4::fs_std::FileExt;

    let map_io = |err: std::io::Error| {
        DescriptorCacheError::Io(BoundedText::new(&err.to_string(), ERROR_ECHO_MAX_BYTES).text)
    };
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            agent_core::core::private_fs::ensure_private_dir(parent).map_err(|err| match err {
                agent_core::core::private_fs::PrivateFsError::SymlinkRefused(_) => {
                    DescriptorCacheError::NotRegularFile
                }
                agent_core::core::private_fs::PrivateFsError::Io(io) => map_io(io),
            })?;
        }
    }
    let lock_path = {
        let mut name = path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_else(|| std::ffi::OsString::from(DESCRIPTOR_CACHE_FILE));
        name.push(".lock");
        path.with_file_name(name)
    };
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(map_io)?;
    FileExt::lock_exclusive(&lock_file).map_err(map_io)?;

    let mut cache = match load_cache_from(path) {
        Ok(cache) => cache,
        Err(DescriptorCacheError::NotFound) => McpDescriptorCache::empty(),
        Err(err) => {
            tracing::warn!(error = %err,
                "MCP descriptor cache unreadable; replacing with a fresh cache on write-back");
            McpDescriptorCache::empty()
        }
    };
    mutate(&mut cache);
    let cache = sanitize_cache(cache)?;
    store_cache_at(path, &cache)?;
    let _ = FileExt::unlock(&lock_file);
    Ok(cache)
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

    async fn execute(&self, params: Value, ctx: ToolContext) -> crate::Result<String> {
        // Runs strictly AFTER the ExecutionGate authorized this exact call.
        let binding = ctx
            .capabilities
            .memory_backend
            .clone()
            .unwrap_or_else(crate::memory_backend::MemoryBinding::configured_current);
        if binding.exclusive() && crate::memory_backend::competing_note_tool(&self.server_tool_name)
        {
            return Err(crate::RuntimeError::Tool("independent MCP memory is disabled by the host-selected backend; use short memory tools".into()));
        }
        // Without the typed session lease capability, a deferred tool NEVER
        // starts a process.
        let Some(leases) = ctx.capabilities.mcp_leases.clone() else {
            return Err(crate::RuntimeError::Tool(format!(
                "MCP tool '{}' (server '{}') is activation-deferred and no MCP session \
                 lease manager is available in this context; no process was started",
                self.runtime_name, self.server_name
            )));
        };
        leases
            .call_exact(
                &self.server_name,
                &self.expected_fingerprint,
                &self.server_tool_name,
                &self.expected_digest,
                params,
            )
            .await
            .map_err(|err| {
                // Grant invalidation (spec 7.4): config removal, fingerprint
                // drift, and live name/schema mismatch poison the pinned
                // descriptor, so the EXACT session grant must fall with the
                // lease. Transport/capacity/revocation-race failures are
                // transient and must NOT revoke.
                use super::lease::McpLeaseError;
                if matches!(
                    err,
                    McpLeaseError::ServerNotConfigured(_)
                        | McpLeaseError::FingerprintDrift(_)
                        | McpLeaseError::NameNotListed(_, _)
                        | McpLeaseError::SchemaMismatch(_, _)
                ) {
                    if let Some(activation) = ctx.capabilities.tool_activation.as_ref() {
                        if activation.revoke_exact_mcp_grant(
                            leases.session(),
                            &self.server_name,
                            &self.server_tool_name,
                        ) {
                            tracing::debug!(
                                "revoked exact MCP activation after descriptor invalidation"
                            );
                        }
                    }
                }
                crate::RuntimeError::Tool(err.to_string())
            })
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

#[cfg(test)]
mod discovery_failure_tests {
    use super::*;

    fn failure(fingerprint: &str, last: u64, attempts: u32) -> DiscoveryFailure {
        DiscoveryFailure {
            fingerprint: fingerprint.to_string(),
            last_attempt_unix: last,
            attempts,
        }
    }

    #[test]
    fn backoff_doubles_per_attempt_and_caps_at_a_day() {
        let hour = DISCOVERY_BACKOFF_BASE_SECS;
        assert_eq!(failure("fp", 0, 0).backoff_secs(), hour, "0 is treated as 1");
        assert_eq!(failure("fp", 0, 1).backoff_secs(), hour);
        assert_eq!(failure("fp", 0, 2).backoff_secs(), 2 * hour);
        assert_eq!(failure("fp", 0, 5).backoff_secs(), 16 * hour);
        assert_eq!(failure("fp", 0, 6).backoff_secs(), DISCOVERY_BACKOFF_MAX_SECS);
        assert_eq!(failure("fp", 0, u32::MAX).backoff_secs(), DISCOVERY_BACKOFF_MAX_SECS);
        assert_eq!(failure("fp", u64::MAX, 1).retry_after_unix(), u64::MAX, "saturates");
    }

    #[test]
    fn suppression_is_keyed_to_fingerprint_and_window() {
        let f = failure("fp", 1_000, 1);
        assert!(f.suppresses("fp", 1_000));
        assert!(f.suppresses("fp", 1_000 + 3_599));
        assert!(!f.suppresses("fp", 1_000 + 3_600), "retry once the backoff expires");
        assert!(!f.suppresses("other", 1_001), "a config edit retries immediately");
        assert!(!f.suppresses("fp", 999), "a clock that went backwards retries");
    }

    #[test]
    fn failures_are_optional_on_disk_and_omitted_when_empty() {
        // An older cache (no field) still loads; an empty field is not written.
        let old: McpDescriptorCache =
            serde_json::from_str(r#"{"version":1,"servers":{}}"#).unwrap();
        assert!(old.discovery_failures.is_empty());
        let json = serde_json::to_string(&McpDescriptorCache::empty()).unwrap();
        assert!(!json.contains("discovery_failures"), "{json}");
    }

    #[test]
    fn sanitize_drops_invalid_entries_and_bounds_the_count() {
        let mut raw = McpDescriptorCache::empty();
        raw.discovery_failures.insert("ok".into(), failure("fp", 5, 0));
        raw.discovery_failures.insert("bad\u{7}".into(), failure("fp", 5, 1));
        raw.discovery_failures.insert("nofp".into(), failure("", 5, 1));
        raw.discovery_failures
            .insert("longfp".into(), failure(&"f".repeat(FINGERPRINT_MAX_BYTES + 1), 5, 1));
        let clean = sanitize_cache(raw).unwrap();
        assert_eq!(clean.discovery_failures.len(), 1);
        assert_eq!(clean.discovery_failures["ok"].attempts, 1, "attempts clamped to >= 1");

        let mut raw = McpDescriptorCache::empty();
        for i in 0..(DISCOVERY_FAILURES_MAX + 10) {
            raw.discovery_failures
                .insert(format!("s{i}"), failure("fp", i as u64, 1));
        }
        let clean = sanitize_cache(raw).unwrap();
        assert_eq!(clean.discovery_failures.len(), DISCOVERY_FAILURES_MAX);
        assert!(
            !clean.discovery_failures.contains_key("s0"),
            "the oldest attempts are evicted first"
        );
        let newest = format!("s{}", DISCOVERY_FAILURES_MAX + 9);
        assert!(clean.discovery_failures.contains_key(&newest));
    }

    #[test]
    fn record_failure_counts_per_fingerprint_and_listing_clears_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DESCRIPTOR_CACHE_FILE);

        let first = record_discovery_failure(&path, "srv", "fp1", 100).unwrap();
        assert_eq!((first.attempts, first.last_attempt_unix), (1, 100));
        let second = record_discovery_failure(&path, "srv", "fp1", 200).unwrap();
        assert_eq!((second.attempts, second.last_attempt_unix), (2, 200));
        let changed = record_discovery_failure(&path, "srv", "fp2", 300).unwrap();
        assert_eq!(changed.attempts, 1, "a new fingerprint restarts the count");
        record_discovery_failure(&path, "other", "fp", 300).unwrap();

        let loaded = load_cache_from(&path).unwrap();
        assert_eq!(loaded.discovery_failures.len(), 2);
        assert_eq!(loaded.discovery_failures["srv"].fingerprint, "fp2");

        let tools = [CachedToolDescriptor {
            name: "t".into(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
        }];
        assert_eq!(record_server_listing(&path, "srv", "fp2", &tools).unwrap(), 1);
        let loaded = load_cache_from(&path).unwrap();
        assert!(!loaded.discovery_failures.contains_key("srv"), "success clears");
        assert!(loaded.discovery_failures.contains_key("other"), "others kept");
        assert_eq!(loaded.servers["srv"].tools.len(), 1);

        assert!(record_discovery_failure(&path, "bad\u{7}", "fp", 1).is_err());
    }
}
