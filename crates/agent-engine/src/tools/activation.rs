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

use thiserror::Error;

use super::catalog::{
    CatalogGeneration, SchemaDigest, SessionActivationGrant, ToolCatalog, ToolId,
};

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
