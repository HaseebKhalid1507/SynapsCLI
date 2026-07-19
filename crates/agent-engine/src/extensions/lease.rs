//! Task 20 (Commit B) — session-scoped exact extension runtime leases
//! (spec §7.5, mirroring the §7.4 MCP lease discipline).
//!
//! Deferred manifest-declared extension tools execute ONLY through this
//! manager, strictly AFTER the `ExecutionGate` has authorized the exact
//! call. One `ExtensionRuntimeManager` is shared by the `ExtensionManager`
//! (which owns the retained internal launch records) and the `Runtime`
//! (which mints per-session capabilities and the durable session-end
//! scope):
//!
//! - a lease is keyed by (session, plugin) and PINNED to the launch-record
//!   fingerprint (manifest + cwd + resolved config) it was acquired under;
//!   per-key SINGLE-FLIGHT acquisition (a `Starting` placeholder holding a
//!   `watch` receiver whose stored state makes wakeups lost-proof)
//!   guarantees concurrent first calls never spawn duplicate children, and
//!   the manager map lock is NEVER held across process/pipe I/O;
//! - acquisition re-reads the retained launch record, RE-VALIDATES the
//!   manifest (permissions included), starts exactly one child
//!   (`kill_on_drop`), initializes ONCE, and requires the runtime's
//!   registered declarations to match the manifest's passive declarations
//!   EXACTLY (tool names, descriptions, canonical schema digests — and
//!   provider declarations when present) BEFORE any call; a mismatch shuts
//!   the child down and fails closed;
//! - every call validates the selected exact tool name and canonical
//!   schema digest against the pinned validated listing BEFORE `tool.call`;
//! - revocation/termination semantics are exact and honest: revocation
//!   marks the lease cancelled under the per-lease call permit; a call
//!   that already passed the gate check MAY complete and then shuts the
//!   child down; NO call passes after revocation returns. `kill_on_drop`
//!   on the child is the hard backstop once the last `Arc` drops;
//! - idle leases are reaped opportunistically with a CONSTANT per-pass
//!   scan cap; session end is the drop of the LAST owner of the shared
//!   durable [`ExtensionSessionEndGuard`] scope. No PID signalling, no
//!   long-lived task, no unbounded channel;
//! - lease failures carry ONLY static text, local identities, and numeric
//!   metadata — extension-reported error content is withheld (length
//!   only), and no launch-record (secret) config ever appears anywhere.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use thiserror::Error;

use super::lifecycle::{
    validate_runtime_provider_declarations, validate_runtime_tool_declarations,
};
use super::manager::{DeferredExtensionRecord, SharedDeferredRecords};
use super::runtime::process::ProcessExtension;
use super::runtime::ExtensionHandler;
use crate::tools::activation::SessionId;
use crate::tools::catalog::SchemaDigest;

/// Default idle bound for opportunistic reaping (parity with MCP leases).
pub const DEFAULT_IDLE_MAX: Duration = Duration::from_secs(300);
/// Hard cap on simultaneously live leases per manager.
pub const MAX_LIVE_LEASES: usize = 64;
/// Reap passes scan up to the full capped map so idle entries can never
/// starve behind an active HashMap iteration prefix.
pub const REAP_SCAN_MAX: usize = MAX_LIVE_LEASES;
/// Outer total bound on one lease cleanup task.
const CLEANUP_TOTAL_TIMEOUT: Duration = Duration::from_secs(5);
/// Bound on locally-generated transport detail strings surfaced in errors.
const TRANSPORT_DETAIL_MAX: usize = 200;
/// Stable prefix `ProcessExtension` puts on extension-REPORTED JSON-RPC
/// error messages; content behind it is extension-controlled and withheld.
const EXTENSION_REPORTED_PREFIX: &str = "Extension error:";

/// Typed lease failures. Static text, local identities, and numeric
/// metadata only — never extension-controlled content, never config.
#[derive(Debug, Error)]
pub enum ExtensionLeaseError {
    #[error("extension '{0}' has no retained deferred launch record; lease revoked")]
    NotDeferred(String),
    #[error("extension '{0}' manifest failed re-validation at lease acquisition; lease revoked")]
    ManifestInvalid(String),
    #[error(
        "extension '{0}' launch record changed since the lease was acquired; lease invalidated"
    )]
    LaunchDrift(String),
    #[error("extension '{0}' no longer declares tool '{1}' with the pinned schema; lease revoked")]
    DeclarationDrift(String, String),
    #[error("extension '{0}' runtime declarations do not match the manifest ({1}); child terminated without any call")]
    DeclarationMismatch(String, &'static str),
    #[error("extension '{0}' did not register expected tool '{1}'; poisoned lease terminated without calling")]
    NameNotListed(String, String),
    #[error("extension '{0}' registered a schema for tool '{1}' that does not match the pinned digest; poisoned lease terminated without calling")]
    SchemaMismatch(String, String),
    #[error("extension lease for '{0}' was revoked before the call could start")]
    Revoked(String),
    #[error("extension lease capacity of {0} live leases reached; try again after idle leases are reaped")]
    Capacity(usize),
    #[error("extension '{0}' transport failure: {1}")]
    Transport(String, String),
    #[error("extension '{plugin}' reported a tool error ({length} bytes, content withheld)")]
    ExtensionReported { plugin: String, length: usize },
}

impl ExtensionLeaseError {
    /// Whether this failure class poisons the pinned declaration/grant
    /// (spec §7.5): record removal, permission/config/catalog drift, and
    /// runtime declaration mismatches invalidate the EXACT session grant.
    /// Transport/capacity/revocation-race and extension-reported tool
    /// errors are transient and must NOT revoke.
    pub fn revokes_exact_grant(&self) -> bool {
        matches!(
            self,
            Self::NotDeferred(_)
                | Self::ManifestInvalid(_)
                | Self::LaunchDrift(_)
                | Self::DeclarationDrift(_, _)
                | Self::DeclarationMismatch(_, _)
                | Self::NameNotListed(_, _)
                | Self::SchemaMismatch(_, _)
        )
    }
}

/// Truncate a locally-generated detail string to a hard byte bound (valid
/// UTF-8 preserved). Extension-REPORTED content never reaches this — it is
/// withheld entirely by the caller.
fn bound_detail(detail: &str) -> String {
    if detail.len() <= TRANSPORT_DETAIL_MAX {
        return detail.to_string();
    }
    let mut cut = TRANSPORT_DETAIL_MAX;
    while cut > 0 && !detail.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}… ({} bytes total)", &detail[..cut], detail.len())
}

/// Canonical fingerprint of a retained launch record: manifest identity,
/// process cwd, and RESOLVED config. Computed locally; never logged.
fn record_fingerprint(record: &DeferredExtensionRecord) -> SchemaDigest {
    SchemaDigest::of_schema(&serde_json::json!({
        "manifest": serde_json::to_value(&record.manifest).unwrap_or(Value::Null),
        "cwd": record.cwd.as_ref().map(|p| p.display().to_string()),
        "config": record.config,
    }))
}

struct LeaseInner {
    fingerprint: SchemaDigest,
    handler: Arc<ProcessExtension>,
    /// Per-lease call/revocation permit: the cancellation gate is read
    /// while HOLDING this, so no new call passes after `cancel` returns.
    call_permit: tokio::sync::Mutex<()>,
    /// Pinned validated listing from the single initialize (tool name →
    /// canonical schema digest).
    listing: HashMap<String, SchemaDigest>,
    /// `true` = cancelled. Held only for flag reads/writes — never I/O.
    gate: std::sync::Mutex<bool>,
    last_used: std::sync::Mutex<Instant>,
    /// Test-observable ownership token.
    liveness: Arc<()>,
}

impl LeaseInner {
    fn cancelled(&self) -> bool {
        *self
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn set_cancelled(&self) {
        *self
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    }
}

enum LeaseState {
    /// Single-flight placeholder; the `watch` channel stores completion
    /// state so followers can never miss the wakeup.
    Starting {
        token: u64,
        done: tokio::sync::watch::Receiver<bool>,
    },
    Ready(Arc<LeaseInner>),
}

/// Session-scoped exact extension runtime lease manager. See module docs.
pub struct ExtensionRuntimeManager {
    leases: std::sync::Mutex<HashMap<(String, String), LeaseState>>,
    /// The `ExtensionManager`'s retained internal launch records (shared
    /// handle; sync lock held only for map operations, never across I/O).
    records: SharedDeferredRecords,
    idle_max: Duration,
    next_token: std::sync::atomic::AtomicU64,
    /// Host-scope session identity for HANDLER-based acquisition (hook
    /// events, provider routing, user sidecar/command APIs). Bound at
    /// engine boot to the Runtime's durable tool session, so a MIXED
    /// extension's tool leases and handler leases share ONE key — one
    /// shared child process per plugin. Unbound (tests/manual) falls back
    /// to a fixed private scope.
    host_scope: std::sync::OnceLock<SessionId>,
}

impl ExtensionRuntimeManager {
    pub(crate) fn new(records: SharedDeferredRecords, idle_max: Duration) -> Self {
        Self {
            leases: std::sync::Mutex::new(HashMap::new()),
            records,
            idle_max,
            next_token: std::sync::atomic::AtomicU64::new(1),
            host_scope: std::sync::OnceLock::new(),
        }
    }

    /// Bind the host-scope session identity used by handler-based
    /// acquisition (first binder wins; the engine binds the Runtime's
    /// durable tool session at boot so Mixed extensions share one child).
    pub fn bind_host_scope(&self, session: SessionId) {
        let _ = self.host_scope.set(session);
    }

    /// The host-scope session identity (bound, or the private fallback).
    pub fn host_scope(&self) -> SessionId {
        self.host_scope
            .get_or_init(|| {
                SessionId::parse("extension-host-scope")
                    .expect("static host scope identity is valid")
            })
            .clone()
    }

    fn current_record(&self, plugin: &str) -> Option<DeferredExtensionRecord> {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(plugin)
            .cloned()
    }

    /// Mark cancelled and hand the lease to ONE bounded cleanup task
    /// (graceful `shutdown` RPC + kill under an outer timeout). Without a
    /// runtime handle the last `Arc` drop triggers `kill_on_drop` — the
    /// hard backstop in every path.
    fn cancel_state(state: LeaseState) {
        match state {
            LeaseState::Ready(inner) => {
                inner.set_cancelled();
                Self::spawn_bounded_cleanup(inner);
            }
            // The in-flight starter detects its placeholder is gone and
            // shuts its own child down instead of publishing the lease.
            LeaseState::Starting { .. } => {}
        }
    }

    fn spawn_bounded_cleanup(inner: Arc<LeaseInner>) {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = tokio::time::timeout(CLEANUP_TOTAL_TIMEOUT, async {
                    // Wait for any in-flight call to release the permit so
                    // shutdown never races an active tool.call frame.
                    let _permit = inner.call_permit.lock().await;
                    inner.handler.shutdown().await;
                })
                .await;
            });
        }
        // No runtime: dropping the Arc (here or when bailing callers
        // release theirs) kills the child via kill_on_drop.
    }

    /// Number of live (Ready or Starting) leases.
    pub fn lease_count(&self) -> usize {
        self.leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Terminate one session's runtime lease for one PLUGIN (the lease
    /// unit: one child shared by that session's exact tools of the plugin).
    pub fn revoke_plugin_lease(&self, session: &SessionId, plugin: &str) {
        let removed = {
            let mut map = self
                .leases
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.remove(&(session.as_str().to_string(), plugin.to_string()))
        };
        if let Some(state) = removed {
            Self::cancel_state(state);
        }
    }

    /// Revoke one EXACT tool grant's runtime backing. Grant revocation
    /// itself is gate-level; at the runtime layer the lease unit is the
    /// plugin child, so this conservatively terminates the shared lease.
    pub fn revoke_exact_tool(&self, session: &SessionId, plugin: &str, tool_name: &str) {
        let _ = tool_name; // exactness lives in the gate; lease unit is the plugin child
        self.revoke_plugin_lease(session, plugin);
    }

    /// Terminate every session's lease for one plugin (manager unload/
    /// reload path). Returns how many were terminated.
    pub fn revoke_plugin_all_sessions(&self, plugin: &str) -> usize {
        let removed: Vec<LeaseState> = {
            let mut map = self
                .leases
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let keys: Vec<(String, String)> =
                map.keys().filter(|(_, p)| p == plugin).cloned().collect();
            keys.into_iter().filter_map(|k| map.remove(&k)).collect()
        };
        let count = removed.len();
        for state in removed {
            Self::cancel_state(state);
        }
        count
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

    /// Single-flight acquisition of the validated Ready lease for one
    /// (session, plugin) key. Shared by exact tool calls and handler-based
    /// (hook/provider/user) acquisition — one child per key. The map lock
    /// is never held across I/O; a live lease whose pinned launch
    /// fingerprint no longer matches the CURRENT record is launch drift
    /// and fails closed after terminating the stale child.
    async fn acquire_ready(
        &self,
        session: &SessionId,
        plugin: &str,
        record: &DeferredExtensionRecord,
        validated: &super::manifest::ValidatedExtensionManifest,
    ) -> Result<Arc<LeaseInner>, ExtensionLeaseError> {
        let expected_fingerprint = record_fingerprint(record);
        let key = (session.as_str().to_string(), plugin.to_string());
        let inner: Arc<LeaseInner> = loop {
            // 4. Single-flight acquisition: the guard-scoped decision
            // returns an owned action, so the map lock is NEVER held
            // across I/O or awaits.
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
                    // A live lease whose pinned fingerprint no longer
                    // matches the current record is launch drift: the
                    // pinned grant falls with it (spec §7.5).
                    let was_drift = matches!(
                        &state,
                        LeaseState::Ready(inner) if inner.fingerprint != expected_fingerprint
                            && !inner.cancelled()
                    );
                    Self::cancel_state(state);
                    if was_drift {
                        return Err(ExtensionLeaseError::LaunchDrift(plugin.to_string()));
                    }
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
                        return Err(ExtensionLeaseError::Capacity(MAX_LIVE_LEASES));
                    }
                    continue;
                }
                Acquire::Wait(mut follower) => {
                    // Lost-wakeup-proof: the watch channel stores state.
                    if !*follower.borrow() {
                        let _ = follower.changed().await;
                    }
                    continue;
                }
                Acquire::Start { token, done_tx } => {
                    let started = self
                        .start_lease(plugin, record, validated, expected_fingerprint.clone())
                        .await;
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
                                inner.handler.shutdown().await;
                                let _ = done_tx.send(true);
                                return Err(ExtensionLeaseError::Revoked(plugin.to_string()));
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

        Ok(inner)
    }

    /// Acquire the shared validated per-plugin lease under the HOST scope
    /// for handler-based capabilities (hook events, provider routing, user
    /// sidecar/command/settings APIs). Same record re-validation, exact
    /// runtime declaration matching, single-flight, and teardown rules as
    /// exact tool calls — and, when the host scope is bound to the
    /// Runtime's tool session (engine boot), the SAME lease key: a Mixed
    /// extension runs ONE shared child.
    async fn acquire_handler_scope(
        &self,
        plugin: &str,
    ) -> Result<Arc<LeaseInner>, ExtensionLeaseError> {
        let session = self.host_scope();
        let (record, validated) = self.record_and_validate(&session, plugin)?;
        self.reap_idle();
        self.acquire_ready(&session, plugin, &record, &validated)
            .await
    }

    /// Whether a Ready (non-cancelled) lease exists for one key.
    fn has_ready_lease(&self, session: &SessionId, plugin: &str) -> bool {
        let map = self
            .leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        matches!(
            map.get(&(session.as_str().to_string(), plugin.to_string())),
            Some(LeaseState::Ready(inner)) if !inner.cancelled()
        )
    }

    /// Steps shared by every acquisition path: re-read the retained
    /// internal launch record and RE-VALIDATE the manifest (permissions
    /// included) before any spawn decision. Failure revokes the session's
    /// plugin lease and fails closed.
    fn record_and_validate(
        &self,
        session: &SessionId,
        plugin: &str,
    ) -> Result<
        (
            DeferredExtensionRecord,
            super::manifest::ValidatedExtensionManifest,
        ),
        ExtensionLeaseError,
    > {
        let Some(record) = self.current_record(plugin) else {
            self.revoke_plugin_lease(session, plugin);
            return Err(ExtensionLeaseError::NotDeferred(plugin.to_string()));
        };
        let Ok(validated) = record.manifest.validate(plugin) else {
            self.revoke_plugin_lease(session, plugin);
            return Err(ExtensionLeaseError::ManifestInvalid(plugin.to_string()));
        };
        Ok((record, validated))
    }

    /// Execute one EXACT already-gate-authorized extension tool call. See
    /// module docs for the lease/single-flight/validation contract.
    pub async fn call_exact(
        &self,
        session: &SessionId,
        plugin: &str,
        tool_name: &str,
        expected_digest: &SchemaDigest,
        params: Value,
    ) -> Result<Value, ExtensionLeaseError> {
        // 1.-2. Record re-read + manifest re-validation (pure local).
        let (record, validated) = self.record_and_validate(session, plugin)?;
        // 3. Catalog-drift check: the CURRENT record must still declare
        // this exact tool with the digest pinned in the catalog entry.
        let declared_tools = record
            .manifest
            .deferred
            .as_ref()
            .map(|d| d.tools.as_slice())
            .unwrap_or(&[]);
        let still_declared = declared_tools.iter().any(|t| {
            t.name == tool_name && &SchemaDigest::of_schema(&t.input_schema) == expected_digest
        });
        if !still_declared {
            self.revoke_plugin_lease(session, plugin);
            return Err(ExtensionLeaseError::DeclarationDrift(
                plugin.to_string(),
                tool_name.to_string(),
            ));
        }
        self.reap_idle();

        // 4. Single-flight acquisition of the validated per-plugin lease.
        let inner = self
            .acquire_ready(session, plugin, &record, &validated)
            .await?;
        // 5. Exact validation against the pinned validated listing BEFORE
        // any call.
        match inner.listing.get(tool_name) {
            None => {
                self.revoke_plugin_lease(session, plugin);
                return Err(ExtensionLeaseError::NameNotListed(
                    plugin.to_string(),
                    tool_name.to_string(),
                ));
            }
            Some(listed) if listed != expected_digest => {
                self.revoke_plugin_lease(session, plugin);
                return Err(ExtensionLeaseError::SchemaMismatch(
                    plugin.to_string(),
                    tool_name.to_string(),
                ));
            }
            Some(_) => {}
        }

        // 6. Call under the per-lease permit; the revocation gate check is
        // taken while HOLDING it, so after cancellation no new call passes.
        let permit = inner.call_permit.lock().await;
        if inner.cancelled() {
            return Err(ExtensionLeaseError::Revoked(plugin.to_string()));
        }
        let result = inner
            .handler
            .call_tool_once_no_spawn(tool_name, params)
            .await;
        if inner.cancelled() {
            // Revoked while this already-started call ran: shut the child
            // down before releasing the permit.
            inner.handler.shutdown().await;
        }
        drop(permit);
        *inner
            .last_used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
        result.map_err(|e| {
            if let Some(rest) = e.strip_prefix(EXTENSION_REPORTED_PREFIX) {
                // Extension-controlled content: withheld, length only.
                ExtensionLeaseError::ExtensionReported {
                    plugin: plugin.to_string(),
                    length: rest.trim().len(),
                }
            } else {
                ExtensionLeaseError::Transport(plugin.to_string(), bound_detail(&e))
            }
        })
    }

    /// Spawn + set permissions + initialize ONCE for one plugin, then
    /// require the runtime's declarations to match the manifest EXACTLY.
    /// No manager lock is held. Any failure shuts the child down before
    /// returning (`kill_on_drop` backs the local paths).
    async fn start_lease(
        &self,
        plugin: &str,
        record: &DeferredExtensionRecord,
        validated: &super::manifest::ValidatedExtensionManifest,
        fingerprint: SchemaDigest,
    ) -> Result<Arc<LeaseInner>, ExtensionLeaseError> {
        let manifest = &record.manifest;
        let process = ProcessExtension::spawn_with_cwd(
            plugin,
            &manifest.command,
            &manifest.args,
            record.cwd.clone(),
        )
        .await
        .map_err(|e| ExtensionLeaseError::Transport(plugin.to_string(), bound_detail(&e)))?;
        process.set_permissions(validated.permissions.clone()).await;
        let caps = match process
            .initialize(record.cwd.clone(), record.config.clone())
            .await
        {
            Ok(caps) => caps,
            Err(e) => {
                process.shutdown().await;
                let err = if let Some(rest) = e.strip_prefix(EXTENSION_REPORTED_PREFIX) {
                    ExtensionLeaseError::ExtensionReported {
                        plugin: plugin.to_string(),
                        length: rest.trim().len(),
                    }
                } else {
                    ExtensionLeaseError::Transport(plugin.to_string(), bound_detail(&e))
                };
                return Err(err);
            }
        };
        let deferred = manifest.deferred.as_ref();
        let declared_tools = deferred.map(|d| d.tools.as_slice()).unwrap_or(&[]);
        let declared_providers = deferred.map(|d| d.providers.as_slice()).unwrap_or(&[]);
        if let Err(reason) = validate_runtime_tool_declarations(declared_tools, &caps.tools) {
            process.shutdown().await;
            return Err(ExtensionLeaseError::DeclarationMismatch(
                plugin.to_string(),
                reason,
            ));
        }
        if let Err(reason) =
            validate_runtime_provider_declarations(declared_providers, &caps.providers)
        {
            process.shutdown().await;
            return Err(ExtensionLeaseError::DeclarationMismatch(
                plugin.to_string(),
                reason,
            ));
        }
        for decl in &caps.capabilities {
            if super::runtime::process::validate_capability(decl, &validated.permissions).is_err() {
                process.shutdown().await;
                return Err(ExtensionLeaseError::DeclarationMismatch(
                    plugin.to_string(),
                    "capability_declaration_rejected",
                ));
            }
        }
        let mut listing = HashMap::new();
        for spec in &caps.tools {
            listing.insert(
                spec.name.clone(),
                SchemaDigest::of_schema(&spec.input_schema),
            );
        }
        Ok(Arc::new(LeaseInner {
            fingerprint,
            handler: Arc::new(process),
            call_permit: tokio::sync::Mutex::new(()),
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
        plugin: &str,
    ) -> Option<std::sync::Weak<()>> {
        let map = self
            .leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match map.get(&(session.as_str().to_string(), plugin.to_string())) {
            Some(LeaseState::Ready(inner)) => Some(Arc::downgrade(&inner.liveness)),
            _ => None,
        }
    }
}

impl Drop for ExtensionRuntimeManager {
    fn drop(&mut self) {
        self.terminate_all();
    }
}

impl std::fmt::Debug for ExtensionRuntimeManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionRuntimeManager")
            .field("leases", &self.lease_count())
            .finish_non_exhaustive()
    }
}

/// Typed per-session lease capability handed to tool contexts: the exact
/// session identity plus the shared manager. Never widens to a plugin
/// grant — every call re-validates the exact tool and launch record.
#[derive(Clone)]
pub struct ExtensionLeaseCapability {
    session: SessionId,
    manager: Arc<ExtensionRuntimeManager>,
}

impl ExtensionLeaseCapability {
    pub fn new(session: SessionId, manager: Arc<ExtensionRuntimeManager>) -> Self {
        Self { session, manager }
    }

    /// The exact session identity this capability is scoped to.
    pub fn session(&self) -> &SessionId {
        &self.session
    }

    pub async fn call_exact(
        &self,
        plugin: &str,
        tool_name: &str,
        expected_digest: &SchemaDigest,
        params: Value,
    ) -> Result<Value, ExtensionLeaseError> {
        self.manager
            .call_exact(&self.session, plugin, tool_name, expected_digest, params)
            .await
    }
}

impl std::fmt::Debug for ExtensionLeaseCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionLeaseCapability")
            .finish_non_exhaustive()
    }
}

/// Durable last-owner session scope. The Runtime mints ONE of these per
/// tool session (wrapped in an `Arc` shared by every runtime clone and
/// in-flight stream); streams HOLD it, never construct or drop their own.
/// Leases therefore survive across provider turns, and only the drop of
/// the LAST owner — true session end — terminates the session's leases.
pub struct ExtensionSessionEndGuard {
    session: SessionId,
    manager: Arc<ExtensionRuntimeManager>,
}

impl ExtensionSessionEndGuard {
    pub fn new(session: SessionId, manager: Arc<ExtensionRuntimeManager>) -> Self {
        Self { session, manager }
    }
}

impl Drop for ExtensionSessionEndGuard {
    fn drop(&mut self) {
        self.manager.terminate_session(&self.session);
    }
}

impl std::fmt::Debug for ExtensionSessionEndGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionSessionEndGuard")
            .finish_non_exhaustive()
    }
}

/// Lazy [`ExtensionHandler`] for deferred hook/provider/user capabilities
/// (Task 20 Commit C). Registered/subscribed at load WITHOUT a process;
/// the first AUTHORIZED use (a matching hook event delivered by the
/// permission-checked `HookBus` subscription, a selected provider
/// complete/stream, or an explicit user sidecar/command/settings action)
/// single-flights through the SAME per-plugin lease manager — starting
/// and exact-validating the child once. Discovery, load, search, and
/// diagnostics never acquire.
pub struct LazyExtensionHandler {
    plugin: String,
    manager: Arc<ExtensionRuntimeManager>,
}

impl LazyExtensionHandler {
    pub fn new(plugin: &str, manager: Arc<ExtensionRuntimeManager>) -> Self {
        Self {
            plugin: plugin.to_string(),
            manager,
        }
    }

    /// Acquire the shared validated lease (host scope) and hand back the
    /// live inner. Static-only failure text.
    async fn live(&self) -> Result<Arc<LeaseInner>, String> {
        self.manager
            .acquire_handler_scope(&self.plugin)
            .await
            .map_err(|e| e.to_string())
    }

    fn touch(inner: &LeaseInner) {
        *inner
            .last_used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
    }
}

#[async_trait::async_trait]
impl ExtensionHandler for LazyExtensionHandler {
    fn id(&self) -> &str {
        &self.plugin
    }

    async fn handle(
        &self,
        event: &crate::extensions::hooks::events::HookEvent,
    ) -> crate::extensions::hooks::events::HookResult {
        // First AUTHORIZED matching event (the HookBus checked the
        // permission at subscribe time and matched the filter) starts the
        // child. Acquisition failure follows the documented eager hook
        // transport policy: warn + Continue (never a silent block).
        match self.live().await {
            Ok(inner) => {
                let result = inner.handler.handle(event).await;
                Self::touch(&inner);
                result
            }
            Err(error) => {
                tracing::warn!(
                    extension = %self.plugin,
                    error = %error,
                    "Deferred extension could not start for hook event — continuing",
                );
                crate::extensions::hooks::events::HookResult::Continue
            }
        }
    }

    async fn call_tool(&self, _name: &str, _input: Value) -> Result<Value, String> {
        // FAIL CLOSED: deferred extension tool execution flows ONLY
        // through the gate-authorized exact lease path
        // (`ExtensionRuntimeManager::call_exact`, which pins and checks
        // the declared name + canonical schema digest). A generic handler
        // call with an arbitrary tool name would bypass those checks.
        Err(format!(
            "extension '{}' is deferred: tool calls are only served through the \
             gate-authorized exact activation path",
            self.plugin
        ))
    }

    async fn provider_complete(
        &self,
        params: super::runtime::process::ProviderCompleteParams,
    ) -> Result<super::runtime::process::ProviderCompleteResult, String> {
        let inner = self.live().await?;
        let result = inner.handler.provider_complete(params).await;
        Self::touch(&inner);
        result
    }

    async fn provider_stream(
        &self,
        params: super::runtime::process::ProviderCompleteParams,
        sink: tokio::sync::mpsc::Sender<super::runtime::process::ProviderStreamEvent>,
    ) -> Result<super::runtime::process::ProviderCompleteResult, String> {
        let inner = self.live().await?;
        let result = inner.handler.provider_stream(params, sink).await;
        Self::touch(&inner);
        result
    }

    async fn invoke_command(
        &self,
        command: &str,
        args: Vec<String>,
        request_id: &str,
        sink: tokio::sync::mpsc::UnboundedSender<super::runtime::InvokeCommandEvent>,
    ) -> Result<Value, String> {
        let inner = self.live().await?;
        let result = inner
            .handler
            .invoke_command(command, args, request_id, sink)
            .await;
        Self::touch(&inner);
        result
    }

    async fn get_info(&self) -> Result<crate::extensions::info::PluginInfo, String> {
        // Diagnostics must never spawn a deferred extension.
        Err("deferred extension info is unavailable without activation".to_string())
    }

    async fn sidecar_spawn_args(&self) -> Result<crate::sidecar::spawn::SidecarSpawnArgs, String> {
        // Explicit user sidecar action: a legitimate acquisition trigger.
        let inner = self.live().await?;
        let result = inner.handler.sidecar_spawn_args().await;
        Self::touch(&inner);
        result
    }

    async fn settings_editor_open(&self, category: &str, field: &str) -> Result<Value, String> {
        let inner = self.live().await?;
        let result = inner.handler.settings_editor_open(category, field).await;
        Self::touch(&inner);
        result
    }

    async fn settings_editor_key(
        &self,
        category: &str,
        field: &str,
        key: &str,
    ) -> Result<Value, String> {
        let inner = self.live().await?;
        let result = inner
            .handler
            .settings_editor_key(category, field, key)
            .await;
        Self::touch(&inner);
        result
    }

    async fn settings_editor_commit(
        &self,
        category: &str,
        field: &str,
        value: Value,
    ) -> Result<Value, String> {
        let inner = self.live().await?;
        let result = inner
            .handler
            .settings_editor_commit(category, field, value)
            .await;
        Self::touch(&inner);
        result
    }

    async fn shutdown(&self) {
        // Terminates every session's lease for this plugin; never spawns.
        self.manager.revoke_plugin_all_sessions(&self.plugin);
    }

    async fn health(&self) -> super::runtime::ExtensionHealth {
        // Dormant reporting only — health checks never spawn.
        if self
            .manager
            .has_ready_lease(&self.manager.host_scope(), &self.plugin)
        {
            super::runtime::ExtensionHealth::Running
        } else {
            super::runtime::ExtensionHealth::Loaded
        }
    }
}

impl std::fmt::Debug for LazyExtensionHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazyExtensionHandler")
            .field("plugin", &self.plugin)
            .finish_non_exhaustive()
    }
}
