//! Task 19 (Commit B) — session-scoped exact MCP runtime leases (spec §7.4).
//!
//! Deferred descriptor-backed MCP tools execute ONLY through this manager,
//! AFTER the `ExecutionGate` has already authorized the exact call. The
//! manager owns every live MCP child for the runtime:
//!
//! - a lease is keyed by (session, server) and PINNED to the config
//!   fingerprint it was acquired under; per-key SINGLE-FLIGHT acquisition
//!   (a `Starting` placeholder holding a `watch` receiver, whose stored
//!   state makes wakeups lost-proof) guarantees concurrent first calls
//!   never spawn duplicate children, and the manager map lock is NEVER
//!   held across process/pipe I/O;
//! - acquisition starts exactly one child (`kill_on_drop`), initializes
//!   and lists ONCE, and pins the bounded listing; every call validates
//!   the selected exact runtime name and canonical schema digest against
//!   the cached expectation BEFORE `tools/call` — a mismatch terminates
//!   the poisoned lease without calling;
//! - revocation/termination semantics (exact and honest): revocation
//!   marks the lease cancelled under the per-lease call gate. A call that
//!   already passed the gate check (taken while holding the connection
//!   lock) MAY still complete; NO call passes the gate after revocation
//!   returns, and a completing call that observes cancellation kills the
//!   child before releasing the connection. Teardown is guaranteed by
//!   `kill_on_drop` once the last `Arc` drops (map removal plus bailing
//!   callers make that inevitable) with an immediate best-effort
//!   `start_kill` accelerator — the manager retains nothing;
//! - idle leases are reaped opportunistically with a CONSTANT per-pass
//!   scan cap; session end is enforced by the RAII [`McpSessionEndGuard`]
//!   on every stream exit path. No PID signalling, no background task, no
//!   unbounded channel.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use thiserror::Error;

use super::connection::McpConnection;
use super::descriptors::server_config_fingerprint;
use super::McpServerConfig;
use crate::tools::activation::SessionId;
use crate::tools::catalog::SchemaDigest;

/// Injectable current-config lookup: production reads `mcp.json` per
/// acquisition (bounded local file read); tests inject a mutable map.
pub type ConfigSource = Arc<dyn Fn(&str) -> Option<McpServerConfig> + Send + Sync>;

/// Production config source: profile-resolved `mcp.json` lookup.
pub fn config_source_from_disk() -> ConfigSource {
    Arc::new(|server: &str| {
        super::load_mcp_config().and_then(|c| c.mcp_servers.get(server).cloned())
    })
}

/// Default idle bound for opportunistic reaping.
pub const DEFAULT_IDLE_MAX: Duration = Duration::from_secs(300);
/// Constant cap on map keys examined per reap pass — reaping work is bounded
/// regardless of map size; repeated passes make progress as entries drop.
pub const REAP_SCAN_MAX: usize = 32;
/// Hard cap on simultaneously live leases per manager, so termination
/// fanout (and cleanup-task count) is always bounded.
pub const MAX_LIVE_LEASES: usize = 64;
/// Outer total bound on one lease cleanup task (lock wait + shutdown).
const CLEANUP_TOTAL_TIMEOUT: Duration = Duration::from_secs(3);

/// Typed lease failures. Provider-controlled content never appears here —
/// only static text, local identities, and numeric metadata.
#[derive(Debug, Error)]
pub enum McpLeaseError {
    #[error("MCP server '{0}' is no longer configured; lease revoked")]
    ServerNotConfigured(String),
    #[error(
        "MCP server '{0}' config fingerprint changed; cached descriptors and lease invalidated"
    )]
    FingerprintDrift(String),
    #[error("MCP server '{0}' did not list expected tool '{1}'; poisoned lease terminated without calling")]
    NameNotListed(String, String),
    #[error("MCP tool '{1}' on server '{0}' listed a schema that does not match the pinned descriptor digest; poisoned lease terminated without calling")]
    SchemaMismatch(String, String),
    #[error("MCP lease for server '{0}' was revoked before the call could start")]
    Revoked(String),
    #[error("MCP transport failure for server '{0}': {1}")]
    Transport(String, String),
    #[error(
        "MCP lease capacity of {0} live leases reached; try again after idle leases are reaped"
    )]
    Capacity(usize),
}

struct LeaseInner {
    fingerprint: String,
    connection: tokio::sync::Mutex<McpConnection>,
    /// Bounded pinned listing from the single `tools/list`.
    listing: HashMap<String, SchemaDigest>,
    /// Per-lease call/revocation gate: `true` = cancelled. Held only for
    /// flag reads/writes — never across I/O.
    gate: std::sync::Mutex<bool>,
    last_used: std::sync::Mutex<Instant>,
    /// Test-observable ownership token: a `Weak` to this proves when the
    /// manager (and any bounded shutdown task) released the lease.
    liveness: Arc<()>,
}

impl LeaseInner {
    fn cancelled(&self) -> bool {
        *self
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Mark cancelled: no NEW call passes the gate after this returns.
    fn set_cancelled(&self) {
        *self
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    }
}

enum LeaseState {
    /// Single-flight placeholder. The `watch` channel stores completion
    /// state, so a follower that clones the receiver under the map lock and
    /// awaits after releasing it can never miss the wakeup (`changed()`
    /// resolves immediately once the starter sent or dropped the sender).
    Starting {
        token: u64,
        done: tokio::sync::watch::Receiver<bool>,
    },
    Ready(Arc<LeaseInner>),
}

/// Session-scoped exact MCP runtime lease manager. See module docs.
pub struct McpRuntimeManager {
    leases: std::sync::Mutex<HashMap<(String, String), LeaseState>>,
    config_source: ConfigSource,
    idle_max: Duration,
    next_token: std::sync::atomic::AtomicU64,
}

impl McpRuntimeManager {
    pub fn new(config_source: ConfigSource, idle_max: Duration) -> Self {
        Self {
            leases: std::sync::Mutex::new(HashMap::new()),
            config_source,
            idle_max,
            next_token: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// Sync RAII-safe termination: mark cancelled, then — when the
    /// connection is not in-flight — close stdin immediately (graceful EOF)
    /// and hand the Arc to one BOUNDED spawned task that waits ~250ms for
    /// the child to exit before `start_kill` + bounded reap. Outside a
    /// runtime the fallback is an immediate `start_kill`. If the connection
    /// IS in-flight, the completing call observes cancellation and runs the
    /// async graceful shutdown itself. In every path the last `Arc` drop
    /// triggers `kill_on_drop` as the hard backstop.
    fn cancel_state(state: LeaseState) {
        match state {
            LeaseState::Ready(inner) => {
                inner.set_cancelled();
                match inner.connection.try_lock() {
                    Ok(mut conn) => {
                        conn.close_stdin();
                        drop(conn);
                        Self::spawn_bounded_cleanup(Arc::clone(&inner));
                    }
                    // In-flight: the completion path observes cancellation
                    // and shuts down; the bounded cleanup task below is a
                    // second, outer-timeout-bounded chance to reap sooner.
                    Err(_) => Self::spawn_bounded_cleanup(Arc::clone(&inner)),
                }
            }
            // The in-flight starter detects its placeholder is gone and
            // kills its own child instead of publishing the lease; dropping
            // the receiver here wakes nobody wrongly (watch state persists).
            LeaseState::Starting { .. } => {}
        }
    }

    /// One BOUNDED one-shot cleanup task per terminated lease: strict
    /// outer timeout around (connection lock wait + graceful 250ms EOF +
    /// kill/1s reap). Finite input, no producer — not a long-lived task.
    /// If the outer timeout is exceeded (a call still in flight), the
    /// completion path sees the cancelled gate and shuts the child down;
    /// `kill_on_drop` on the final `Arc` drop remains the hard backstop.
    /// Without a runtime handle: sync close_stdin + start_kill, then drop.
    fn spawn_bounded_cleanup(inner: Arc<LeaseInner>) {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let _ = tokio::time::timeout(CLEANUP_TOTAL_TIMEOUT, async {
                        let mut conn = inner.connection.lock().await;
                        conn.finish_shutdown().await;
                    })
                    .await;
                });
            }
            Err(_) => {
                if let Ok(mut conn) = inner.connection.try_lock() {
                    conn.start_kill();
                }
            }
        }
    }

    /// Number of live (Ready or Starting) leases.
    pub fn lease_count(&self) -> usize {
        self.leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Terminate one session's runtime lease on one SERVER (the lease unit:
    /// one child process shared by that session's exact tools of that
    /// server).
    pub fn revoke_server_lease(&self, session: &SessionId, server: &str) {
        let removed = {
            let mut map = self
                .leases
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.remove(&(session.as_str().to_string(), server.to_string()))
        };
        if let Some(state) = removed {
            Self::cancel_state(state);
        }
    }

    /// Revoke one EXACT tool grant's runtime backing. Grant revocation
    /// itself is gate-level (the `SessionToolSet`); at the runtime layer the
    /// lease unit is the server child, so this conservatively terminates
    /// the shared server lease — the child loses all session access rather
    /// than retaining any after an exact revocation.
    pub fn revoke_exact_tool(&self, session: &SessionId, server: &str, server_tool_name: &str) {
        let _ = server_tool_name; // exactness lives in the gate; lease unit is the server child
        self.revoke_server_lease(session, server);
    }

    /// Terminate every lease held by one session. Returns how many.
    pub fn terminate_session(&self, session: &SessionId) -> usize {
        let removed: Vec<LeaseState> = {
            let mut map = self
                .leases
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let keys: Vec<(String, String)> = map
                .keys()
                .filter(|(s, _)| s == session.as_str())
                .cloned()
                .collect();
            keys.into_iter().filter_map(|k| map.remove(&k)).collect()
        };
        let count = removed.len();
        for state in removed {
            Self::cancel_state(state);
        }
        count
    }

    /// Terminate every lease (runtime shutdown).
    pub fn terminate_all(&self) {
        let removed: Vec<LeaseState> = {
            let mut map = self
                .leases
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.drain().map(|(_, v)| v).collect()
        };
        for state in removed {
            Self::cancel_state(state);
        }
    }

    /// Reap Ready leases idle past the configured bound. Work per pass is
    /// bounded by [`REAP_SCAN_MAX`] examined keys.
    pub fn reap_idle(&self) {
        let removed: Vec<LeaseState> = {
            let mut map = self
                .leases
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let idle: Vec<(String, String)> = map
                .iter()
                .take(REAP_SCAN_MAX)
                .filter(|(_, state)| match state {
                    LeaseState::Ready(inner) => {
                        inner
                            .last_used
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .elapsed()
                            >= self.idle_max
                    }
                    LeaseState::Starting { .. } => false,
                })
                .map(|(k, _)| k.clone())
                .collect();
            idle.into_iter().filter_map(|k| map.remove(&k)).collect()
        };
        for state in removed {
            Self::cancel_state(state);
        }
    }

    /// Execute one EXACT already-gate-authorized MCP tool call. See module
    /// docs for the lease/single-flight/validation/revocation contract.
    pub async fn call_exact(
        &self,
        session: &SessionId,
        server: &str,
        expected_fingerprint: &str,
        server_tool_name: &str,
        expected_digest: &SchemaDigest,
        params: Value,
    ) -> Result<String, McpLeaseError> {
        // 1. Current config + fingerprint (pure local; no lock held).
        let Some(config) = (self.config_source)(server) else {
            self.revoke_server_lease(session, server);
            return Err(McpLeaseError::ServerNotConfigured(server.to_string()));
        };
        if server_config_fingerprint(&config) != expected_fingerprint {
            self.revoke_server_lease(session, server);
            return Err(McpLeaseError::FingerprintDrift(server.to_string()));
        }
        self.reap_idle();

        let key = (session.as_str().to_string(), server.to_string());
        let inner: Arc<LeaseInner> = loop {
            // 2. Single-flight acquisition. The guard-scoped decision below
            // returns an owned action, so the map lock is NEVER held across
            // I/O or awaits (also keeps this future Send).
            enum Acquire {
                Use(Arc<LeaseInner>),
                CancelStale(LeaseState),
                Wait(tokio::sync::watch::Receiver<bool>),
                Start {
                    token: u64,
                    done_tx: tokio::sync::watch::Sender<bool>,
                },
                AtCapacity,
            }
            let action = {
                let mut map = self
                    .leases
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match map.get(&key) {
                    Some(LeaseState::Ready(inner)) => {
                        if inner.cancelled() || inner.fingerprint != expected_fingerprint {
                            match map.remove(&key) {
                                Some(state) => Acquire::CancelStale(state),
                                None => continue,
                            }
                        } else {
                            Acquire::Use(Arc::clone(inner))
                        }
                    }
                    Some(LeaseState::Starting { done, .. }) => Acquire::Wait(done.clone()),
                    None => {
                        if map.len() >= MAX_LIVE_LEASES {
                            Acquire::AtCapacity
                        } else {
                            let token = self
                                .next_token
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let (done_tx, done_rx) = tokio::sync::watch::channel(false);
                            map.insert(
                                key.clone(),
                                LeaseState::Starting {
                                    token,
                                    done: done_rx,
                                },
                            );
                            Acquire::Start { token, done_tx }
                        }
                    }
                }
            };
            match action {
                Acquire::Use(inner) => break inner,
                Acquire::CancelStale(state) => {
                    Self::cancel_state(state);
                    continue;
                }
                Acquire::AtCapacity => {
                    self.reap_idle();
                    let still_full = self
                        .leases
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .len()
                        >= MAX_LIVE_LEASES;
                    if still_full {
                        return Err(McpLeaseError::Capacity(MAX_LIVE_LEASES));
                    }
                    continue;
                }
                Acquire::Wait(mut follower) => {
                    // Lost-wakeup-proof: the watch channel stores state, so
                    // if the starter finished between our map release and
                    // this await, `changed()` resolves immediately; sender
                    // drop also wakes us.
                    if !*follower.borrow() {
                        let _ = follower.changed().await;
                    }
                    continue;
                }
                Acquire::Start { token, done_tx } => {
                    let started = self
                        .start_lease(expected_fingerprint, &config)
                        .await
                        .map_err(|e| McpLeaseError::Transport(server.to_string(), e));
                    let ours = {
                        let mut map = self
                            .leases
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let ours = matches!(
                            map.get(&key),
                            Some(LeaseState::Starting { token: t, .. }) if *t == token
                        );
                        match &started {
                            Ok(inner) if ours => {
                                map.insert(key.clone(), LeaseState::Ready(Arc::clone(inner)));
                            }
                            Err(_) if ours => {
                                map.remove(&key);
                            }
                            _ => {}
                        }
                        ours
                    };
                    match started {
                        Ok(inner) => {
                            if !ours {
                                // Terminated while starting (session end or
                                // revocation): never resurrect.
                                inner.set_cancelled();
                                if let Ok(mut conn) = inner.connection.try_lock() {
                                    conn.shutdown().await;
                                }
                                let _ = done_tx.send(true);
                                return Err(McpLeaseError::Revoked(server.to_string()));
                            }
                            let _ = done_tx.send(true);
                            break inner;
                        }
                        Err(err) => {
                            let _ = done_tx.send(true);
                            return Err(err);
                        }
                    }
                }
            }
        };

        // 3. Exact validation against the pinned listing BEFORE any call.
        match inner.listing.get(server_tool_name) {
            None => {
                self.revoke_server_lease(session, server);
                return Err(McpLeaseError::NameNotListed(
                    server.to_string(),
                    server_tool_name.to_string(),
                ));
            }
            Some(listed) if listed != expected_digest => {
                self.revoke_server_lease(session, server);
                return Err(McpLeaseError::SchemaMismatch(
                    server.to_string(),
                    server_tool_name.to_string(),
                ));
            }
            Some(_) => {}
        }

        // 4. Call under the connection lock; the revocation gate check is
        // taken while HOLDING that lock, so after `cancel()` returns no new
        // call can pass. A call that already passed the gate may complete;
        // if it then observes cancellation it kills the child before
        // releasing the connection.
        let mut conn = inner.connection.lock().await;
        if inner.cancelled() {
            return Err(McpLeaseError::Revoked(server.to_string()));
        }
        let result = conn.call_tool(server_tool_name, params).await;
        if inner.cancelled() {
            // Revoked while this already-started call ran: shut the child
            // down (graceful EOF, bounded kill fallback) before releasing.
            conn.shutdown().await;
        }
        drop(conn);
        *inner
            .last_used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
        result.map_err(|e| McpLeaseError::Transport(server.to_string(), e))
    }

    /// Spawn + initialize + single bounded tools/list for one server. No
    /// manager lock is held. Any failure kills the child before returning.
    async fn start_lease(
        &self,
        fingerprint: &str,
        config: &McpServerConfig,
    ) -> Result<Arc<LeaseInner>, String> {
        let mut conn = McpConnection::start(config).await?;
        let defs = match conn.list_tools().await {
            Ok(defs) => defs,
            Err(e) => {
                conn.start_kill();
                return Err(e);
            }
        };
        let mut listing = HashMap::new();
        for def in defs {
            listing.insert(def.name.clone(), SchemaDigest::of_schema(&def.input_schema));
        }
        Ok(Arc::new(LeaseInner {
            fingerprint: fingerprint.to_string(),
            connection: tokio::sync::Mutex::new(conn),
            listing,
            gate: std::sync::Mutex::new(false),
            last_used: std::sync::Mutex::new(Instant::now()),
            liveness: Arc::new(()),
        }))
    }

    /// Test seam: `Weak` ownership token of one lease. `upgrade() == None`
    /// proves the manager (and any bounded shutdown task) released it.
    #[doc(hidden)]
    pub fn lease_liveness_for_tests(
        &self,
        session: &SessionId,
        server: &str,
    ) -> Option<std::sync::Weak<()>> {
        let map = self
            .leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match map.get(&(session.as_str().to_string(), server.to_string())) {
            Some(LeaseState::Ready(inner)) => Some(Arc::downgrade(&inner.liveness)),
            _ => None,
        }
    }
}

impl Drop for McpRuntimeManager {
    fn drop(&mut self) {
        self.terminate_all();
    }
}

impl std::fmt::Debug for McpRuntimeManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpRuntimeManager")
            .field("leases", &self.lease_count())
            .finish_non_exhaustive()
    }
}

/// Typed per-session lease capability handed to tool contexts: the exact
/// session identity plus the shared manager. Never widens to a server
/// grant — every call re-validates the exact tool and fingerprint.
#[derive(Clone)]
pub struct McpLeaseCapability {
    session: SessionId,
    manager: Arc<McpRuntimeManager>,
}

impl McpLeaseCapability {
    pub fn new(session: SessionId, manager: Arc<McpRuntimeManager>) -> Self {
        Self { session, manager }
    }

    pub async fn call_exact(
        &self,
        server: &str,
        expected_fingerprint: &str,
        server_tool_name: &str,
        expected_digest: &SchemaDigest,
        params: Value,
    ) -> Result<String, McpLeaseError> {
        self.manager
            .call_exact(
                &self.session,
                server,
                expected_fingerprint,
                server_tool_name,
                expected_digest,
                params,
            )
            .await
    }
}

impl std::fmt::Debug for McpLeaseCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpLeaseCapability").finish_non_exhaustive()
    }
}

/// RAII session-end guard: dropping it terminates every lease of the
/// session on every stream exit path (success, error, cancellation).
pub struct McpSessionEndGuard {
    session: SessionId,
    manager: Arc<McpRuntimeManager>,
}

impl McpSessionEndGuard {
    pub fn new(session: SessionId, manager: Arc<McpRuntimeManager>) -> Self {
        Self { session, manager }
    }
}

impl Drop for McpSessionEndGuard {
    fn drop(&mut self) {
        let terminated = self.manager.terminate_session(&self.session);
        if terminated > 0 {
            tracing::debug!(
                leases = terminated,
                "MCP session leases terminated at stream end"
            );
        }
    }
}
