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
    ToolCatalog, ToolEffect, ToolId, TrustProvenance,
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
/// schema digest; the trust provenance of the catalog record is pinned
/// engine-side at activation time (the grant type in agent-core is
/// deliberately untouched) so execution can detect a record that was
/// removed and re-added under a different — even internally coherent —
/// source/provenance.
#[derive(Clone, Debug)]
pub struct ActivatedTool {
    grant: SessionActivationGrant,
    provenance: TrustProvenance,
    lease: RuntimeLease,
}

impl ActivatedTool {
    pub fn grant(&self) -> &SessionActivationGrant {
        &self.grant
    }

    pub fn schema_digest(&self) -> &SchemaDigest {
        self.grant.schema_digest()
    }

    /// The trust provenance of the catalog record at activation time.
    pub fn provenance(&self) -> &TrustProvenance {
        &self.provenance
    }

    pub fn catalog_generation(&self) -> CatalogGeneration {
        self.grant.catalog_generation()
    }

    pub fn lease(&self) -> RuntimeLease {
        self.lease
    }
}

/// Typed failure for [`SessionToolSet::revoke_exact`].
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ExactRevocationError {
    #[error("cannot revoke a configured core tool: {0}")]
    CoreTool(ToolId),
    #[error("tool is not activated in this session: {0}")]
    NotActivated(ToolId),
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

/// Per-tool pins captured when a core tool enters a session's set: the
/// schema digest AND the trust provenance of the catalog record at build
/// time. Pinning provenance (not just the digest) closes the coherent-
/// imposter hole: a tool removed and re-added under the SAME `ToolId` with a
/// schema-identical body but a different, internally consistent
/// source/provenance must still be denied at execution time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CorePin {
    schema_digest: SchemaDigest,
    provenance: TrustProvenance,
}

impl CorePin {
    pub fn schema_digest(&self) -> &SchemaDigest {
        &self.schema_digest
    }

    pub fn provenance(&self) -> &TrustProvenance {
        &self.provenance
    }
}

/// Why an activation was not carried across a round-top rebuild.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DropReason {
    /// The tool is no longer in the catalog.
    Removed,
    /// The record's schema digest or trust provenance changed.
    Drifted,
}

/// One activation dropped by [`SessionToolSet::rebuilt_for_catalog`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DroppedActivation {
    pub id: ToolId,
    pub reason: DropReason,
}

/// Kill-switch for carry-forward: `SYNAPS_TOOLSET_CARRY_FORWARD=0` restores
/// the zero-inherit rebuild (every catalog mutation wipes activations).
pub fn carry_forward_enabled() -> bool {
    std::env::var("SYNAPS_TOOLSET_CARRY_FORWARD").as_deref() != Ok("0")
}

/// The small configured core set plus exact activated deferred tools for one
/// session, pinned to the catalog generation it was built against. Core
/// tools are pinned with the schema digest AND trust provenance of their
/// catalog record at build time, so later drift is detectable per tool, not
/// just per generation.
#[derive(Clone, Debug)]
pub struct SessionToolSet {
    session: SessionId,
    catalog_generation: CatalogGeneration,
    core: BTreeMap<ToolId, CorePin>,
    activated: BTreeMap<ToolId, ActivatedTool>,
    /// Session schema-generation counter (spec §7.7): advances by exactly
    /// one for every successful nonempty activation batch (a single
    /// activation is a batch of one). Deterministic bookkeeping only; it
    /// exposes nothing and grants nothing.
    schema_generation: u64,
}

impl SessionToolSet {
    /// Build a fresh set for one session. Every configured core id must
    /// exist in the catalog (typed failure otherwise) and its schema digest
    /// AND trust provenance are pinned from the catalog record; the set
    /// starts with zero activations — nothing is inherited from any other
    /// session.
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
            validated.insert(
                id,
                CorePin {
                    schema_digest: record.schema_digest().clone(),
                    provenance: record.provenance().clone(),
                },
            );
        }
        Ok(Self {
            session,
            catalog_generation: catalog.generation(),
            core: validated,
            activated: BTreeMap::new(),
            schema_generation: 0,
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

    /// Deterministic minimal core for Task 18 progressive disclosure. Only
    /// essential local operations plus discovery/authorization gateways are
    /// eligible; missing/disabled gateways are skipped rather than invented.
    /// Specialized subagent lifecycle, extension, MCP, shell-session, and
    /// dynamically registered tools remain deferred until exact activation.
    pub fn progressive_core_for_catalog(session: SessionId, catalog: &ToolCatalog) -> Self {
        const ESSENTIAL_BUILTINS: &[&str] = &[
            "bash",
            "read",
            "write",
            "edit",
            "grep",
            "find",
            "ls",
            "search_tools",
            "activate_tools",
            "search_skills",
            "load_skill",
        ];
        let core = ESSENTIAL_BUILTINS
            .iter()
            .map(|name| ToolId::builtin(name))
            .filter(|id| catalog.get(id).is_some())
            .collect::<Vec<_>>();
        Self::new(session, core, catalog)
            .expect("progressive core ids are filtered through the catalog")
    }

    /// Round-top rebuild against a NEWER catalog (`runtime/stream.rs`):
    /// the core is re-derived exactly as a fresh build would derive it, and
    /// every prior exact activation whose CURRENT catalog record still
    /// matches its pinned (schema digest, trust provenance) is re-issued a
    /// grant at the new generation. Activations whose record is gone or
    /// drifted are dropped and reported.
    ///
    /// This is NOT a gate change: a carried activation is exactly what a
    /// user re-activation of the same tool would produce against this
    /// catalog, and `ExecutionGate::authorize` still checks digest and
    /// provenance against the current record on every call.
    pub fn rebuilt_for_catalog(
        &self,
        catalog: &ToolCatalog,
        progressive: bool,
    ) -> (Self, Vec<DroppedActivation>) {
        let mut next = if progressive {
            Self::progressive_core_for_catalog(self.session.clone(), catalog)
        } else {
            Self::default_core_for_catalog(self.session.clone(), catalog)
        };
        let mut dropped = Vec::new();
        for old in self.activated.values() {
            let id = old.grant().tool_id().clone();
            match catalog.get(&id) {
                // Promoted to core by the new catalog — nothing to carry.
                Some(_) if next.core.contains_key(&id) => {}
                Some(rec)
                    if rec.schema_digest() == old.schema_digest()
                        && rec.provenance() == old.provenance() =>
                {
                    let grant = SessionActivationGrant::new(
                        self.session.as_str(),
                        id.clone(),
                        catalog.generation(),
                        rec.schema_digest().clone(),
                    )
                    .expect("session id was valid when the set was built");
                    next.activated.insert(
                        id,
                        ActivatedTool {
                            grant,
                            provenance: rec.provenance().clone(),
                            lease: RuntimeLease::NotAcquired,
                        },
                    );
                }
                Some(_) => dropped.push(DroppedActivation {
                    id,
                    reason: DropReason::Drifted,
                }),
                None => dropped.push(DroppedActivation {
                    id,
                    reason: DropReason::Removed,
                }),
            }
        }
        // Session schema-generation is bookkeeping for "activation batches
        // applied"; carrying is not a batch: keep the old counter, +1 iff
        // anything was dropped (the exposed schema changed).
        next.schema_generation = self.schema_generation + u64::from(!dropped.is_empty());
        (next, dropped)
    }

    /// Typed EXACT revocation (Task 19 grant invalidation): remove one
    /// non-core exact activation from this session's set. The session
    /// schema generation advances by exactly one IFF an activation was
    /// removed; core tools and unknown/never-activated ids fail typed with
    /// zero mutation. Never touches the catalog or sibling activations.
    pub fn revoke_exact(&mut self, id: &ToolId) -> Result<(), ExactRevocationError> {
        if self.core.contains_key(id) {
            return Err(ExactRevocationError::CoreTool(id.clone()));
        }
        if self.activated.remove(id).is_none() {
            return Err(ExactRevocationError::NotActivated(id.clone()));
        }
        self.schema_generation += 1;
        Ok(())
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
        self.core.get(id).map(CorePin::schema_digest)
    }

    /// The full (digest, provenance) pin for a configured core tool, or
    /// `None` when the id is not in this session's core set.
    pub fn core_pin(&self, id: &ToolId) -> Option<&CorePin> {
        self.core.get(id)
    }

    /// Deterministic projection of exact activations (ToolId order).
    pub fn activated(&self) -> impl Iterator<Item = &ActivatedTool> {
        self.activated.values()
    }

    pub fn activation(&self, id: &ToolId) -> Option<&ActivatedTool> {
        self.activated.get(id)
    }

    /// Session schema-generation counter (spec §7.7): the number of
    /// successful nonempty activation batches applied to this set. A
    /// deterministic bulk `activate_many` advances it by exactly one.
    pub fn schema_generation(&self) -> u64 {
        self.schema_generation
    }

    /// Validate one grant against this set and the current catalog without
    /// mutating anything. Shared by [`Self::activate`] and
    /// [`Self::activate_many`] so single and bulk paths can never drift.
    fn validate_grant(
        &self,
        grant: &SessionActivationGrant,
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
        Ok(())
    }

    /// Record one exact activation. This is set/test/bootstrap plumbing, not
    /// a model-facing activation flow: the grant must already exist and must
    /// match this session, a cataloged tool, the CURRENT catalog generation,
    /// and the CURRENT schema digest exactly. Any drift — foreign session,
    /// unknown tool, stale generation, changed digest, stale set snapshot,
    /// duplicate activation, core-set membership — fails typed before any
    /// mutation, so a failed call leaves the set byte-for-byte unchanged. No
    /// implementation is constructed, no process started, no schema exposed.
    /// A successful call is a nonempty batch of one: the session
    /// schema-generation advances by exactly one.
    pub fn activate(
        &mut self,
        grant: SessionActivationGrant,
        catalog: &ToolCatalog,
    ) -> Result<(), ActivationError> {
        self.validate_grant(&grant, catalog)?;
        let provenance = catalog
            .get(grant.tool_id())
            .expect("validate_grant verified the record exists")
            .provenance()
            .clone();
        self.activated.insert(
            grant.tool_id().clone(),
            ActivatedTool {
                grant,
                provenance,
                lease: RuntimeLease::NotAcquired,
            },
        );
        self.schema_generation += 1;
        Ok(())
    }

    /// Deterministic atomic bulk activation (spec §7.7): validate EVERY
    /// requested grant first — duplicates within the batch, unknown ids,
    /// core-set ids, stale generations, digest mismatches, stale set
    /// snapshots, and untrusted/inconsistent source provenance all fail
    /// typed with ZERO partial mutation — then apply in stable `ToolId`
    /// order and advance the session schema-generation by exactly one. An
    /// empty batch is a no-op that advances nothing. Returns the number of
    /// activations applied.
    pub fn activate_many(
        &mut self,
        grants: Vec<SessionActivationGrant>,
        catalog: &ToolCatalog,
    ) -> Result<usize, BulkActivationError> {
        if grants.is_empty() {
            return Ok(0);
        }
        let mut seen: std::collections::BTreeSet<ToolId> = std::collections::BTreeSet::new();
        for grant in &grants {
            if !seen.insert(grant.tool_id().clone()) {
                return Err(BulkActivationError::DuplicateRequest(
                    grant.tool_id().clone(),
                ));
            }
            self.validate_grant(grant, catalog)
                .map_err(BulkActivationError::Grant)?;
            // Source trust re-check before ANY grant is applied: an
            // unverified or provenance-inconsistent capability rejects the
            // whole batch (validate_grant proved the record exists).
            let record = catalog
                .get(grant.tool_id())
                .expect("validate_grant verified the record exists");
            check_source_trust(record).map_err(|err| match err {
                ToolAuthorizationError::SourceProvenanceMismatch(id) => {
                    BulkActivationError::SourceProvenanceMismatch(id)
                }
                // check_source_trust only produces UntrustedSource or
                // SourceProvenanceMismatch; anything else maps to the
                // conservative untrusted denial for this id.
                _ => BulkActivationError::UntrustedSource(grant.tool_id().clone()),
            })?;
        }
        // All grants validated: apply in stable ToolId order.
        let mut ordered = grants;
        ordered.sort_by(|a, b| a.tool_id().cmp(b.tool_id()));
        let applied = ordered.len();
        for grant in ordered {
            let provenance = catalog
                .get(grant.tool_id())
                .expect("validate_grant verified the record exists")
                .provenance()
                .clone();
            self.activated.insert(
                grant.tool_id().clone(),
                ActivatedTool {
                    grant,
                    provenance,
                    lease: RuntimeLease::NotAcquired,
                },
            );
        }
        // Exactly ONE schema-generation advance for the whole batch.
        self.schema_generation += 1;
        Ok(applied)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivationBasis {
    Core,
    Exact {
        catalog_generation: CatalogGeneration,
    },
}

/// A fully authorized tool call: exact resolved identity plus the acquired
/// implementation. The implementation field is private and the ONLY
/// constructor is [`ExecutionGate::authorize`], which acquires it strictly
/// after every check passes — the safe path cannot reach an implementation
/// without passing the gate.
pub struct AuthorizedToolCall {
    resolved: ResolvedToolCall,
    implementation: Arc<dyn Tool>,
    activation_basis: ActivationBasis,
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

    pub fn activation_basis(&self) -> ActivationBasis {
        self.activation_basis
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
    /// Source/trust provenance failure: either the catalog record's source
    /// and trust provenance disagree (an internally inconsistent record must
    /// never authorize), or the record's CURRENT provenance differs from the
    /// provenance the session pinned at core-build/activation time (a tool
    /// removed and re-added under a different — even internally coherent —
    /// provenance must never authorize).
    #[error("Tool call denied: source/trust provenance is inconsistent or drifted from the session's pin: {0}")]
    SourceProvenanceMismatch(ToolId),
}

/// The spec §7.1 execution gate: a pure, reusable authorization component
/// evaluated immediately before tool implementation lookup/execution.
///
/// Order of checks (each fails typed, closed, without acquiring anything):
/// 1. resolve wire name → exact live `ToolId` (deterministic reverse
///    mapping; aliases cannot pick a different identity);
/// 2. the identity must be cataloged;
/// 3. the identity must be core (pinned digest intact) or hold an exact
///    session activation grant covering (session, tool, digest) — see
///    [`grant_covers_execution`]: catalog generation drift alone does NOT
///    deny at execution time; only a real change to the called tool's
///    record (digest, presence, provenance) does;
/// 4. the record's CURRENT trust provenance must equal the provenance the
///    session PINNED at core-build/activation time — this closes the
///    coherent-imposter hole (same `ToolId`, schema-identical body,
///    different but internally consistent source/provenance);
/// 5. source trust is re-evaluated conservatively — see the honesty note
///    on [`check_source_trust`]: this re-checks typed source/provenance
///    consistency and denies Unknown/Unverified, but does NOT yet consult
///    live manifest permission/revocation state (Task 20 integrates the
///    permission/revocation policy);
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
    ///
    /// SECURITY INVARIANT (per-tool digest + provenance validation): no
    /// tool executes unless its CURRENT catalog record — presence of the
    /// exact `ToolId`, schema digest, and source/trust provenance — exactly
    /// matches what the session pinned at core-build/activation time.
    /// Catalog generation inequality ALONE is no longer a denial reason at
    /// execution time: an unrelated background mutation (e.g. a plugin
    /// load mid-round) must not kill in-flight calls to schema-identical
    /// tools. Any REAL drift of the called tool's record — changed digest,
    /// removal, changed provenance (even to an internally coherent one) —
    /// still denies typed and closed.
    ///
    /// ACCEPTED RESIDUAL RISK: [`SchemaDigest`] hashes the schema JSON
    /// only, not the implementation factory. A record replaced in place
    /// with an identical schema AND identical pinned provenance but a
    /// different implementation passes this gate; the exposure is bounded
    /// by the deterministic round-top rebuild (runtime/stream.rs), which
    /// re-pins the set against the current catalog between provider rounds.
    pub fn authorize(
        catalog: &ToolCatalog,
        session: &SessionToolSet,
        resolved: ResolvedToolCall,
    ) -> Result<AuthorizedToolCall, ToolAuthorizationError> {
        let tool_id = resolved.tool_id().clone();
        let record = catalog
            .get(&tool_id)
            .ok_or_else(|| ToolAuthorizationError::NotCataloged(tool_id.clone()))?;

        // Generation drift is observed (for logging) but NOT a wholesale
        // denial: the per-tool checks below are unconditional and judge the
        // called tool's CURRENT record against the session's pins. The
        // round-top rebuild (runtime/stream.rs) still uses `is_stale` to
        // refresh the set deterministically between rounds.
        let generation_drift = session.is_stale(catalog);

        // Core status or exact activation grant, with pinned-digest
        // verification either way — unconditional, drift or not. The pinned
        // trust provenance is carried out of each branch for the
        // unconditional pinned-provenance check below.
        let activation_basis;
        let pinned_provenance: &TrustProvenance;
        if let Some(pin) = session.core_pin(&tool_id) {
            if pin.schema_digest() != record.schema_digest() {
                if generation_drift {
                    tracing::warn!(
                        tool = %tool_id,
                        set_generation = session.catalog_generation().value(),
                        catalog_generation = catalog.generation().value(),
                        "tool denied under catalog generation drift: pinned core schema digest \
                         no longer matches the current catalog record"
                    );
                }
                return Err(ToolAuthorizationError::SchemaDigestMismatch(tool_id));
            }
            pinned_provenance = pin.provenance();
            activation_basis = ActivationBasis::Core;
        } else if let Some(activated) = session.activation(&tool_id) {
            if activated.schema_digest() != record.schema_digest() {
                if generation_drift {
                    tracing::warn!(
                        tool = %tool_id,
                        set_generation = session.catalog_generation().value(),
                        catalog_generation = catalog.generation().value(),
                        "tool denied under catalog generation drift: activation grant schema \
                         digest no longer matches the current catalog record"
                    );
                }
                return Err(ToolAuthorizationError::SchemaDigestMismatch(tool_id));
            }
            // Execution-time grant re-check on the (session, tool, digest)
            // tuple — deliberately WITHOUT the generation term (see
            // `grant_covers_execution`). Grant ISSUANCE and activation stay
            // generation-strict (`validate_grant`, `covers()`).
            if !grant_covers_execution(
                activated.grant(),
                session.session().as_str(),
                &tool_id,
                record.schema_digest(),
            ) {
                return Err(ToolAuthorizationError::NotActivated(tool_id));
            }
            pinned_provenance = activated.provenance();
            activation_basis = ActivationBasis::Exact {
                catalog_generation: activated.catalog_generation(),
            };
        } else {
            return Err(ToolAuthorizationError::NotActivated(tool_id));
        }

        // Unconditional pinned-provenance equality (BLOCKER fix): the
        // record's CURRENT trust provenance must equal the provenance the
        // session pinned at core-build/activation time. This — not the
        // self-consistency check below — is what catches the coherent
        // imposter: a tool removed and re-added under the same `ToolId`
        // with a schema-identical body but a different, internally
        // consistent source/provenance pair.
        if pinned_provenance != record.provenance() {
            if generation_drift {
                tracing::warn!(
                    tool = %tool_id,
                    set_generation = session.catalog_generation().value(),
                    catalog_generation = catalog.generation().value(),
                    "tool denied under catalog generation drift: current record provenance \
                     no longer matches the provenance the session pinned"
                );
            }
            return Err(ToolAuthorizationError::SourceProvenanceMismatch(tool_id));
        }

        // Conservative source trust re-check immediately before
        // acquisition: typed source/provenance SELF-consistency plus the
        // deny-by-default for Unknown/Unverified (see check_source_trust) —
        // live manifest permission/revocation state is not consulted here
        // yet (Task 20). This complements (does not replace) the
        // pinned-provenance equality above.
        check_source_trust(record)?;

        if generation_drift {
            tracing::warn!(
                tool = %tool_id,
                set_generation = session.catalog_generation().value(),
                catalog_generation = catalog.generation().value(),
                "tool call survived catalog generation drift: current record digest, presence \
                 and provenance exactly match the session's pins"
            );
        }

        // Effect classes (Task 24) are execution-SCHEDULING policy, not
        // authorization policy: every class may execute once authorized —
        // the stream scheduler decides concurrency/ordering from the
        // recorded class. The exhaustive match keeps this decision loud if
        // a future class needs gate-level treatment.
        match record.effect() {
            ToolEffect::ReadOnly | ToolEffect::IdempotentWrite | ToolEffect::NonIdempotent => {}
        }

        // Acquisition happens strictly after authorization succeeds.
        let implementation = record.implementation();
        Ok(AuthorizedToolCall {
            resolved,
            implementation,
            activation_basis,
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

/// Execution-time grant coverage: (session, tool, digest) equality WITHOUT
/// the generation term of [`SessionActivationGrant::covers`]. Rationale: at
/// execution time the tool's identity and schema are already re-validated
/// against the CURRENT catalog record, so requiring the pinned generation to
/// equal the live one only makes unrelated catalog mutations (background
/// plugin loads) kill in-flight calls to schema-identical tools. `covers()`
/// itself (agent-core) stays exact-tuple and is still what grant ISSUANCE
/// and activation validation (`validate_grant`) enforce — this relaxation
/// applies ONLY to re-validation of an already-established authorization.
fn grant_covers_execution(
    grant: &SessionActivationGrant,
    session_id: &str,
    tool_id: &ToolId,
    schema_digest: &SchemaDigest,
) -> bool {
    grant.session_id() == session_id
        && grant.tool_id() == tool_id
        && grant.schema_digest() == schema_digest
}

/// Conservative per-source trust policy — CONSISTENCY check, not a live
/// permission lookup: `BuiltinRuntime` provenance is valid only for
/// builtin-sourced records; extension/MCP/plugin provenance must match the
/// catalog source identity exactly; `Unverified` (and any unknown source)
/// is denied by default even if configured core.
///
/// Honesty note (Task 16): this validates the typed provenance recorded at
/// catalog time against the record's source identity. It does NOT consult
/// live manifest permission state or revocation — a capability whose
/// extension permissions were revoked after cataloging still passes this
/// check until the catalog entry is removed/rebuilt. Live
/// permission/revocation integration is Task 20.
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

// ── Task 17: host grant issuance, typed confirmation authority, and the
//    retained shared session set (spec §7.2, §7.3, §7.7) ─────────────────────

/// One retained per-stream-session handle: `activate_tools` mutates the SAME
/// set the `ExecutionGate` and the next provider round consume. Interior
/// critical sections are short and never held across `.await`.
pub type SharedSessionToolSet = Arc<std::sync::RwLock<SessionToolSet>>;

/// Typed failure for deterministic bulk activation ([`SessionToolSet::activate_many`]).
/// Every variant fails the WHOLE batch with zero partial mutation.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum BulkActivationError {
    #[error("duplicate tool id in one activation batch: {0}")]
    DuplicateRequest(ToolId),
    #[error("activation batch rejected: {0}")]
    Grant(#[from] ActivationError),
    #[error("activation batch rejected: source provenance is unverified: {0}")]
    UntrustedSource(ToolId),
    #[error("activation batch rejected: catalog source and trust provenance disagree: {0}")]
    SourceProvenanceMismatch(ToolId),
}

/// Host-supplied activation authority for MODEL-INITIATED activation.
///
/// `ModelConfirmed` may be populated ONLY by host policy (explicit server
/// auto-approve or an interactive confirmation mechanism) — never from
/// model-authored JSON arguments. Explicit user-requested activation goes
/// through [`activate_exact_for_user`], a host Rust API that involves no
/// authority value at all (PR #63 ergonomics: a user asking for an exact
/// known tool needs no redundant prompt).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivationAuthority {
    /// No confirmation authority: model-initiated activation is denied
    /// before any grant issuance or set/schema mutation.
    Unauthorized,
    /// Host confirmation policy satisfied for this session/turn.
    ModelConfirmed,
}

/// Typed failure for host grant issuance. Issuance validates the exact
/// known `ToolId`, the current catalog generation/digest (pinned into the
/// grant), and source trust — before any grant exists.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum GrantIssuanceError {
    #[error("cannot issue activation grant: tool is not in the catalog: {0}")]
    UnknownTool(ToolId),
    #[error("cannot issue activation grant: source provenance is unverified: {0}")]
    UntrustedSource(ToolId),
    #[error("cannot issue activation grant: catalog source and trust provenance disagree: {0}")]
    SourceProvenanceMismatch(ToolId),
    #[error("cannot issue activation grant: {0}")]
    Grant(#[from] agent_core::orchestration::capability::ActivationGrantError),
}

/// Issue one exact session-scoped activation grant against the CURRENT
/// catalog state. Validates: the id is a known exact cataloged capability
/// and its source trust/provenance is consistent and verified. The grant
/// pins the current catalog generation and schema digest. Pure: no factory
/// invocation, no process start, no network, no set mutation.
pub fn issue_exact_grant(
    catalog: &ToolCatalog,
    session: &SessionId,
    tool_id: &ToolId,
) -> Result<SessionActivationGrant, GrantIssuanceError> {
    let record = catalog
        .get(tool_id)
        .ok_or_else(|| GrantIssuanceError::UnknownTool(tool_id.clone()))?;
    check_source_trust(record).map_err(|err| match err {
        ToolAuthorizationError::SourceProvenanceMismatch(id) => {
            GrantIssuanceError::SourceProvenanceMismatch(id)
        }
        _ => GrantIssuanceError::UntrustedSource(tool_id.clone()),
    })?;
    Ok(SessionActivationGrant::new(
        session.as_str(),
        tool_id.clone(),
        catalog.generation(),
        record.schema_digest().clone(),
    )?)
}

/// Typed failure for the host activation entry points.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum HostActivationError {
    /// Model-initiated activation without host confirmation authority.
    /// Denied BEFORE grant issuance and before any set/schema mutation.
    #[error(
        "tool activation requires host confirmation authority; \
         model-initiated activation was not confirmed"
    )]
    ConfirmationRequired,
    #[error(transparent)]
    Issuance(#[from] GrantIssuanceError),
    #[error(transparent)]
    Bulk(#[from] BulkActivationError),
}

/// Explicit USER-REQUESTED exact activation (spec §7.3 / PR #63 ergonomics):
/// the host may authorize one exact known local identity without a
/// redundant confirmation prompt. This is a host Rust API — it is never
/// reachable from model-authored JSON and takes no confirmation boolean.
/// Grants are session-scoped and never persisted.
pub fn activate_exact_for_user(
    set: &mut SessionToolSet,
    catalog: &ToolCatalog,
    tool_id: &ToolId,
) -> Result<(), HostActivationError> {
    let grant = issue_exact_grant(catalog, set.session(), tool_id)?;
    set.activate_many(vec![grant], catalog)?;
    Ok(())
}

/// MODEL-INITIATED exact activation: requires host-supplied
/// [`ActivationAuthority::ModelConfirmed`]. Without it, the request is
/// denied before any grant issuance, set mutation, or schema-generation
/// advance. All requested ids are validated and issued first; the batch
/// applies atomically in stable `ToolId` order with exactly one session
/// schema-generation advance. Grants are session-scoped, never persisted.
pub fn activate_model_initiated(
    authority: ActivationAuthority,
    set: &mut SessionToolSet,
    catalog: &ToolCatalog,
    tool_ids: &[ToolId],
) -> Result<usize, HostActivationError> {
    match authority {
        ActivationAuthority::ModelConfirmed => {}
        ActivationAuthority::Unauthorized => {
            return Err(HostActivationError::ConfirmationRequired);
        }
    }
    let session = set.session().clone();
    let mut grants = Vec::with_capacity(tool_ids.len());
    for tool_id in tool_ids {
        grants.push(issue_exact_grant(catalog, &session, tool_id)?);
    }
    Ok(set.activate_many(grants, catalog)?)
}

/// Resolve the session tool set for the extension-provider route (Task 17,
/// closing the Task 16 policy divergence): when the runtime threads its
/// RETAINED per-stream set, the route consumes THAT set's current state —
/// including exact activations — at its pinned generation. The route must
/// never silently mint a fresh default-core set for the same round. Only
/// callers with no retained handle at all (internal/sync helpers) fall back
/// to a fresh default-core set with zero activations under a locally minted
/// session.
///
/// Generation drift is deliberately SURVIVED here (per-tool digest
/// validation fix): the retained set is served as-is even when the catalog
/// generation moved past its snapshot, because every actual execution still
/// passes through [`ExecutionGate::authorize`], which unconditionally
/// re-validates each called tool's current record (presence, digest,
/// provenance) against the session's pins. Denying the whole retained set
/// wholesale would let an unrelated background catalog mutation kill an
/// entire in-flight round of schema-identical tools. Infallible: neither
/// path can be denied anymore, so the return type is the plain set.
pub fn route_session_set(
    retained: Option<&SharedSessionToolSet>,
    catalog: &ToolCatalog,
    fallback_session: impl FnOnce() -> SessionId,
) -> SessionToolSet {
    match retained {
        Some(shared) => {
            let guard = shared
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.is_stale(catalog) {
                tracing::warn!(
                    set_generation = guard.catalog_generation().value(),
                    catalog_generation = catalog.generation().value(),
                    "serving retained session tool set across catalog generation drift; \
                     per-call ExecutionGate::authorize still protects execution"
                );
            }
            guard.clone()
        }
        None => SessionToolSet::default_core_for_catalog(fallback_session(), catalog),
    }
}
