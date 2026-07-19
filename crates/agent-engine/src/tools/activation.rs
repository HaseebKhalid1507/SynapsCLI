//! `SessionToolSet` — per-session core + exact activated tools (Task 15,
//! spec §7.1, §4.2).
//!
//! This is bookkeeping only, not yet an execution gate: it records which
//! configured core tools and which exactly-activated deferred tools belong to
//! one session, pinned to the catalog generation and schema digests they were
//! granted against. It never issues model-facing activations, exposes
//! schemas, initializes implementations, or enforces execution.
//!
//! Isolation invariants:
//! - a new session's set starts with zero activations and inherits nothing
//!   from any other set;
//! - activation requires an exact [`SessionActivationGrant`] for this
//!   session, tool, current catalog generation, and current schema digest;
//! - every rejection is typed and leaves the set unchanged (no partial
//!   mutation);
//! - catalog drift is represented (`is_stale`), never silently absorbed.

use std::collections::BTreeMap;
use std::sync::Arc;

use agent_core::BoundedText;
use thiserror::Error;

use super::catalog::{
    CapabilityRecord, CapabilitySource, CatalogGeneration, SchemaDigest, SessionActivationGrant,
    SideEffectClass, ToolCatalog, ToolId, TrustProvenance,
};
use super::{Tool, ToolRegistry};

pub use agent_core::orchestration::capability::{SessionId, SessionIdError};

/// Runtime-lease placeholder metadata (spec §7.4 territory). Real lease
/// acquisition/expiry arrives with MCP/extension activation work; until then
/// every activation records that no runtime has been acquired.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeLease {
    NotAcquired,
}

/// One exact activation held by a session: the validated grant plus lease
/// placeholder metadata. The grant carries the pinned catalog generation and
/// schema digest.
#[derive(Clone, Debug)]
pub struct ActivatedTool {
    grant: SessionActivationGrant,
    lease: RuntimeLease,
}

impl ActivatedTool {
    pub fn grant(&self) -> &SessionActivationGrant {
        &self.grant
    }

    pub fn schema_digest(&self) -> &SchemaDigest {
        self.grant.schema_digest()
    }

    pub fn catalog_generation(&self) -> CatalogGeneration {
        self.grant.catalog_generation()
    }

    pub fn lease(&self) -> RuntimeLease {
        self.lease
    }
}

/// Typed failure for constructing a [`SessionToolSet`].
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum SessionToolSetError {
    #[error("configured core tool is not in the catalog: {0}")]
    UnknownCoreTool(ToolId),
}

/// Typed failure for activating a tool in a [`SessionToolSet`]. Every
/// variant fails closed without mutating the set.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ActivationError {
    #[error("activation grant was issued for another session")]
    SessionMismatch,
    #[error("activation grant names a tool the catalog does not know: {0}")]
    UnknownTool(ToolId),
    #[error(
        "activation grant generation {} does not match current catalog generation {}",
        grant.value(),
        catalog.value()
    )]
    StaleGeneration {
        grant: CatalogGeneration,
        catalog: CatalogGeneration,
    },
    #[error("activation grant schema digest does not match the catalog record for {0}")]
    DigestMismatch(ToolId),
    #[error("tool is already activated for this session: {0}")]
    AlreadyActivated(ToolId),
    #[error("tool is already in this session's configured core set: {0}")]
    AlreadyCore(ToolId),
    #[error(
        "session tool set snapshot at generation {} is stale against catalog generation {}; rebuild it",
        set.value(),
        catalog.value()
    )]
    StaleSnapshot {
        set: CatalogGeneration,
        catalog: CatalogGeneration,
    },
}

/// The small configured core set plus exact activated deferred tools for one
/// session, pinned to the catalog generation it was built against. Core
/// tools are pinned with the schema digest of their catalog record at build
/// time, so later drift is detectable per tool, not just per generation.
#[derive(Clone, Debug)]
pub struct SessionToolSet {
    session: SessionId,
    catalog_generation: CatalogGeneration,
    core: BTreeMap<ToolId, SchemaDigest>,
    activated: BTreeMap<ToolId, ActivatedTool>,
}

impl SessionToolSet {
    /// Build a fresh set for one session. Every configured core id must
    /// exist in the catalog (typed failure otherwise) and its schema digest
    /// is pinned from the catalog record; the set starts with zero
    /// activations — nothing is inherited from any other session.
    pub fn new(
        session: SessionId,
        core: impl IntoIterator<Item = ToolId>,
        catalog: &ToolCatalog,
    ) -> Result<Self, SessionToolSetError> {
        let mut validated = BTreeMap::new();
        for id in core {
            let Some(record) = catalog.get(&id) else {
                return Err(SessionToolSetError::UnknownCoreTool(id));
            };
            validated.insert(id, record.schema_digest().clone());
        }
        Ok(Self {
            session,
            catalog_generation: catalog.generation(),
            core: validated,
            activated: BTreeMap::new(),
        })
    }

    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// Default per-stream-session set (Task 16 behavior preservation): the
    /// core set is EXACTLY every currently cataloged capability with
    /// verified provenance — builtins plus already-loaded trusted
    /// extension/MCP/plugin tools. `Unverified` capabilities are excluded
    /// (denied-by-default at the gate), and the set carries zero
    /// activations. Deterministic: ids come straight from the catalog in
    /// `ToolId` order, so rebuilding after a catalog mutation yields the
    /// same set for the same catalog state.
    pub fn default_core_for_catalog(session: SessionId, catalog: &ToolCatalog) -> Self {
        let core: Vec<ToolId> = catalog
            .iter()
            .filter(|record| record.provenance() != &TrustProvenance::Unverified)
            .map(|record| record.id().clone())
            .collect();
        Self::new(session, core, catalog)
            .expect("core ids are drawn from the catalog itself and must all resolve")
    }

    /// The catalog generation this set snapshot was built against.
    pub fn catalog_generation(&self) -> CatalogGeneration {
        self.catalog_generation
    }

    /// True when the catalog has mutated since this set was built. A stale
    /// set must be rebuilt/revalidated, not silently used.
    pub fn is_stale(&self, catalog: &ToolCatalog) -> bool {
        self.catalog_generation != catalog.generation()
    }

    /// Deterministic projection of configured core ids (ToolId order).
    pub fn core_ids(&self) -> impl Iterator<Item = &ToolId> {
        self.core.keys()
    }

    pub fn is_core(&self, id: &ToolId) -> bool {
        self.core.contains_key(id)
    }

    /// The schema digest pinned for a configured core tool at build time,
    /// or `None` when the id is not in this session's core set.
    pub fn core_schema_digest(&self, id: &ToolId) -> Option<&SchemaDigest> {
        self.core.get(id)
    }

    /// Deterministic projection of exact activations (ToolId order).
    pub fn activated(&self) -> impl Iterator<Item = &ActivatedTool> {
        self.activated.values()
    }

    pub fn activation(&self, id: &ToolId) -> Option<&ActivatedTool> {
        self.activated.get(id)
    }

    /// Record one exact activation. This is set/test/bootstrap plumbing, not
    /// a model-facing activation flow: the grant must already exist and must
    /// match this session, a cataloged tool, the CURRENT catalog generation,
    /// and the CURRENT schema digest exactly. Any drift — foreign session,
    /// unknown tool, stale generation, changed digest, stale set snapshot,
    /// duplicate activation, core-set membership — fails typed before any
    /// mutation, so a failed call leaves the set byte-for-byte unchanged. No
    /// implementation is constructed, no process started, no schema exposed.
    pub fn activate(
        &mut self,
        grant: SessionActivationGrant,
        catalog: &ToolCatalog,
    ) -> Result<(), ActivationError> {
        if grant.session_id() != self.session.as_str() {
            return Err(ActivationError::SessionMismatch);
        }
        let tool_id = grant.tool_id().clone();
        let record = catalog
            .get(&tool_id)
            .ok_or_else(|| ActivationError::UnknownTool(tool_id.clone()))?;
        if grant.catalog_generation() != catalog.generation() {
            return Err(ActivationError::StaleGeneration {
                grant: grant.catalog_generation(),
                catalog: catalog.generation(),
            });
        }
        if grant.schema_digest() != record.schema_digest() {
            return Err(ActivationError::DigestMismatch(tool_id));
        }
        // A fresh grant cannot rescue a stale set snapshot: the set itself
        // must be rebuilt against the current catalog first.
        if self.catalog_generation != catalog.generation() {
            return Err(ActivationError::StaleSnapshot {
                set: self.catalog_generation,
                catalog: catalog.generation(),
            });
        }
        // Exact-tuple re-check through the grant's own covers() so the two
        // validation paths can never drift apart.
        debug_assert!(grant.covers(
            self.session.as_str(),
            &tool_id,
            catalog.generation(),
            record.schema_digest(),
        ));
        if self.activated.contains_key(&tool_id) {
            return Err(ActivationError::AlreadyActivated(tool_id));
        }
        // Core tools are always available to the session; re-recording one
        // as a deferred activation would create two conflicting bookkeeping
        // entries for the same capability.
        if self.core.contains_key(&tool_id) {
            return Err(ActivationError::AlreadyCore(tool_id));
        }
        self.activated.insert(
            tool_id,
            ActivatedTool {
                grant,
                lease: RuntimeLease::NotAcquired,
            },
        );
        Ok(())
    }
}

// ── ExecutionGate (Task 16, spec §7.1 / §4.2) ───────────────────────────────

/// Byte budget for wire/tool names echoed inside typed denials. Denials are
/// static, bounded, metadata-only strings: hostile oversize wire names must
/// never be reflected verbatim.
pub const WIRE_NAME_MAX_BYTES: usize = 128;

fn bounded_wire_name(raw: &str) -> String {
    BoundedText::new(raw, WIRE_NAME_MAX_BYTES).text
}

/// Typed resolution of an incoming API/wire tool name to the exact live
/// runtime tool identity: the wire name as received (bounded), the runtime
/// name it deterministically maps to (via the registry's sanitized-name
/// reverse mapping), and the exact catalog [`ToolId`] derived from the live
/// tool instance itself — never from the alias. Aliases and collision
/// suffixes therefore cannot select a different identity than the tool they
/// deterministically resolve to.
///
/// This is identity data only; it grants nothing. Authorization happens in
/// [`ExecutionGate::authorize`], which is the only constructor of
/// [`AuthorizedToolCall`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedToolCall {
    wire_name: String,
    runtime_name: String,
    tool_id: ToolId,
}

impl ResolvedToolCall {
    pub fn new(wire_name: &str, runtime_name: &str, tool_id: ToolId) -> Self {
        Self {
            wire_name: bounded_wire_name(wire_name),
            runtime_name: runtime_name.to_string(),
            tool_id,
        }
    }

    /// The API/wire name as received (bounded for echo safety).
    pub fn wire_name(&self) -> &str {
        &self.wire_name
    }

    /// The exact runtime tool name the wire name resolves to.
    pub fn runtime_name(&self) -> &str {
        &self.runtime_name
    }

    /// The exact catalog identity of the resolved live tool.
    pub fn tool_id(&self) -> &ToolId {
        &self.tool_id
    }
}

/// A fully authorized tool call: exact resolved identity plus the acquired
/// implementation. The implementation field is private and the ONLY
/// constructor is [`ExecutionGate::authorize`], which acquires it strictly
/// after every check passes — the safe path cannot reach an implementation
/// without passing the gate.
pub struct AuthorizedToolCall {
    resolved: ResolvedToolCall,
    implementation: Arc<dyn Tool>,
}

impl AuthorizedToolCall {
    pub fn resolved(&self) -> &ResolvedToolCall {
        &self.resolved
    }

    pub fn wire_name(&self) -> &str {
        self.resolved.wire_name()
    }

    pub fn runtime_name(&self) -> &str {
        self.resolved.runtime_name()
    }

    pub fn tool_id(&self) -> &ToolId {
        self.resolved.tool_id()
    }

    /// The authorized implementation handle.
    pub fn implementation(&self) -> Arc<dyn Tool> {
        Arc::clone(&self.implementation)
    }
}

impl std::fmt::Debug for AuthorizedToolCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthorizedToolCall")
            .field("resolved", &self.resolved)
            .finish_non_exhaustive()
    }
}

/// Typed execution-gate denial. Every variant is static, bounded, and
/// metadata-only: no tool input, no schema bytes, no provider body content —
/// only bounded identities and generation counters.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ToolAuthorizationError {
    /// The wire name resolves to no live registered tool. Message shape kept
    /// compatible with the legacy tool-loop denial; the echoed name is
    /// bounded at resolution time.
    #[error("Unknown tool: {wire_name}")]
    UnknownTool { wire_name: String },
    /// The live tool exists but the catalog holds no record for its
    /// identity — the registry/catalog invariant is broken; fail closed.
    #[error("Tool call denied: capability is not cataloged: {0}")]
    NotCataloged(ToolId),
    /// Known capability, but neither core for this session nor covered by an
    /// exact activation grant (the forged deferred-call case).
    #[error("Tool call denied: tool is not activated for this session: {0}")]
    NotActivated(ToolId),
    /// The session tool set snapshot predates the current catalog
    /// generation; it must be rebuilt deterministically, never silently
    /// reused.
    #[error(
        "Tool call denied: session tool set generation {} is stale against catalog generation {}",
        set.value(),
        catalog.value()
    )]
    StaleSessionSet {
        set: CatalogGeneration,
        catalog: CatalogGeneration,
    },
    /// The capability's current schema digest differs from the digest the
    /// session pinned at core-build/activation time — a changed tool is
    /// never silently blessed.
    #[error("Tool call denied: schema digest changed since authorization was pinned: {0}")]
    SchemaDigestMismatch(ToolId),
    /// Unverified/unknown provenance is denied by default, even if the
    /// capability was (mis)configured as core, until an explicit later
    /// policy exists.
    #[error("Tool call denied: source provenance is unverified: {0}")]
    UntrustedSource(ToolId),
    /// The catalog record's source and trust provenance disagree — an
    /// internally inconsistent record must never authorize.
    #[error("Tool call denied: catalog source and trust provenance disagree: {0}")]
    SourceProvenanceMismatch(ToolId),
}

/// The spec §7.1 execution gate: a pure, reusable authorization component
/// evaluated immediately before tool implementation lookup/execution.
///
/// Order of checks (each fails typed, closed, without acquiring anything):
/// 1. resolve wire name → exact live `ToolId` (deterministic reverse
///    mapping; aliases cannot pick a different identity);
/// 2. the identity must be cataloged;
/// 3. the session tool set snapshot must match the catalog generation;
/// 4. the identity must be core (pinned digest intact) or hold an exact
///    session activation grant (session, tool, generation, digest);
/// 5. source permission/trust is re-evaluated conservatively;
/// 6. side-effect/confirmation policy (interim: `Unclassified` is allowed
///    only for verified-provenance capabilities — Task 24 adds real effect
///    classes);
/// 7. only then is the implementation acquired.
///
/// TOCTOU: callers must resolve, authorize, and acquire against ONE
/// consistent registry borrow/lock guard — [`ExecutionGate::authorize_wire_call`]
/// does exactly that for a `&ToolRegistry` held under the caller's read lock.
pub struct ExecutionGate;

impl ExecutionGate {
    /// Resolve an incoming API/wire name against one registry snapshot.
    pub fn resolve(
        registry: &ToolRegistry,
        wire_name: &str,
    ) -> Result<ResolvedToolCall, ToolAuthorizationError> {
        registry
            .resolve_wire_call(wire_name)
            .ok_or_else(|| ToolAuthorizationError::UnknownTool {
                wire_name: bounded_wire_name(wire_name),
            })
    }

    /// Authorize a resolved identity against one catalog snapshot and one
    /// session tool set. On success — and only then — the implementation is
    /// acquired from the catalog record's factory and returned inside the
    /// typed [`AuthorizedToolCall`]. Failure acquires nothing and leaves the
    /// session set untouched (the gate never mutates it).
    pub fn authorize(
        catalog: &ToolCatalog,
        session: &SessionToolSet,
        resolved: ResolvedToolCall,
    ) -> Result<AuthorizedToolCall, ToolAuthorizationError> {
        let tool_id = resolved.tool_id().clone();
        let record = catalog
            .get(&tool_id)
            .ok_or_else(|| ToolAuthorizationError::NotCataloged(tool_id.clone()))?;

        // Snapshot-generation check first: a stale session set must be
        // rebuilt deterministically, never consulted for grants.
        if session.is_stale(catalog) {
            return Err(ToolAuthorizationError::StaleSessionSet {
                set: session.catalog_generation(),
                catalog: catalog.generation(),
            });
        }

        // Core status or exact activation grant, with pinned-digest
        // verification either way.
        if let Some(pinned) = session.core_schema_digest(&tool_id) {
            if pinned != record.schema_digest() {
                return Err(ToolAuthorizationError::SchemaDigestMismatch(tool_id));
            }
        } else if let Some(activated) = session.activation(&tool_id) {
            if activated.schema_digest() != record.schema_digest() {
                return Err(ToolAuthorizationError::SchemaDigestMismatch(tool_id));
            }
            // Exact-tuple grant re-check (session, tool, generation,
            // digest); any drift invalidates the grant.
            if !activated.grant().covers(
                session.session().as_str(),
                &tool_id,
                catalog.generation(),
                record.schema_digest(),
            ) {
                return Err(ToolAuthorizationError::NotActivated(tool_id));
            }
        } else {
            return Err(ToolAuthorizationError::NotActivated(tool_id));
        }

        // Conservative source permission/trust re-evaluation immediately
        // before acquisition.
        check_source_trust(record)?;

        // Side-effect/confirmation interim policy (until Task 24): the only
        // classification today is `Unclassified`, which is permitted solely
        // because the capability already passed the verified-provenance
        // check above — Unknown/Unverified capabilities were denied there.
        match record.side_effect() {
            SideEffectClass::Unclassified => {}
        }

        // Acquisition happens strictly after authorization succeeds.
        let implementation = record.implementation();
        Ok(AuthorizedToolCall {
            resolved,
            implementation,
        })
    }

    /// Resolve + authorize + acquire against ONE registry borrow (one
    /// consistent snapshot under the caller's read lock — no TOCTOU between
    /// resolution and implementation acquisition).
    pub fn authorize_wire_call(
        registry: &ToolRegistry,
        session: &SessionToolSet,
        wire_name: &str,
    ) -> Result<AuthorizedToolCall, ToolAuthorizationError> {
        let resolved = Self::resolve(registry, wire_name)?;
        Self::authorize(registry.catalog(), session, resolved)
    }
}

/// Conservative per-source trust policy: `BuiltinRuntime` provenance is
/// valid only for builtin-sourced records; extension/MCP/plugin provenance
/// must match the catalog source identity exactly; `Unverified` (and any
/// unknown source) is denied by default even if configured core.
fn check_source_trust(record: &CapabilityRecord) -> Result<(), ToolAuthorizationError> {
    let deny_untrusted = || ToolAuthorizationError::UntrustedSource(record.id().clone());
    let deny_mismatch = || ToolAuthorizationError::SourceProvenanceMismatch(record.id().clone());
    match (record.provenance(), record.source()) {
        (TrustProvenance::Unverified, _) => Err(deny_untrusted()),
        (_, CapabilitySource::Unknown { .. }) => Err(deny_untrusted()),
        (TrustProvenance::BuiltinRuntime, CapabilitySource::Builtin) => Ok(()),
        (
            TrustProvenance::ExtensionManifest { extension_id },
            CapabilitySource::Extension {
                extension_id: source_extension_id,
                ..
            },
        ) if extension_id == source_extension_id => Ok(()),
        (
            TrustProvenance::McpConfig { server_id },
            CapabilitySource::Mcp {
                server_id: source_server_id,
                ..
            },
        ) if server_id == source_server_id => Ok(()),
        (
            TrustProvenance::PluginConfig { plugin_id },
            CapabilitySource::Plugin {
                plugin_id: source_plugin_id,
                ..
            },
        ) if plugin_id == source_plugin_id => Ok(()),
        _ => Err(deny_mismatch()),
    }
}
