//! Typed continuous-memory context domain model (task A1).
//!
//! Implements the closed domain types from the continuous-memory spec:
//! - §6.1 [`MemoryContextMode`] with exact automatic-recall / turn-capture /
//!   lifetime semantics,
//! - §6.2 [`MemoryContextLease`] — host-private construction, no `Deserialize`,
//! - §6.3 [`UserIntentProof`],
//! - §19  [`AuthorizedMemoryAction`] / [`apply_memory_context_action`] and the
//!   exhaustive [`SessionMemoryState`] transition table.
//!
//! Authority pattern: like `crate::orchestration::validate_user_authorizable_model`,
//! untrusted callers (plugins, models, extension JSON) can never mint authority.
//! A [`MemoryContextLease`] can only be produced by crate-internal host code via
//! the `pub(crate)` [`MemoryContextLease::grant`] gate, and the struct is
//! `#[non_exhaustive]` with **no** `Deserialize` impl, so neither struct-literal
//! construction nor wire deserialization can forge one outside this crate.
//!
//! Style (spec §19): typed boundaries and exhaustive enums — no `enabled: bool`
//! flags, no `serde_json::Value` past the boundary, no model-created lease IDs,
//! no content-bearing errors.

use agent_core::BoundedText;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// Byte ceiling for any single boundary-crossing identifier. Oversized or
/// empty identifiers fail closed at parse time (spec §5.4 bounds).
const MEMORY_IDENTIFIER_MAX_BYTES: usize = 128;

/// Parse one user/plugin-supplied identifier through the workspace-standard
/// [`BoundedText`] budgeting utility. Fail-closed: truncation (over budget),
/// emptiness, and control characters are all rejected rather than repaired.
fn parse_identifier(raw: &str, field: &'static str) -> Result<String, MemoryContextError> {
    let bounded = BoundedText::new(raw, MEMORY_IDENTIFIER_MAX_BYTES);
    if bounded.truncated
        || bounded.text.trim().is_empty()
        || bounded.text.chars().any(char::is_control)
    {
        return Err(MemoryContextError::InvalidIdentifier { field });
    }
    Ok(bounded.text)
}

/// Declare a bounded, parse-at-the-boundary identifier newtype. Construction
/// is `pub(crate)`: identifiers embedded in authority-bearing types (leases,
/// intent proofs) are minted by host code, never by plugin/model input
/// reaching this module as pre-built typed values.
macro_rules! bounded_identifier {
    ($(#[$meta:meta])* $name:ident, $field:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub struct $name(String);

        impl $name {
            /// Parse and bound a raw identifier; fails closed on oversized,
            /// empty, or control-character input.
            #[allow(dead_code)] // consumed by host wiring in tasks A3/A4
            pub(crate) fn parse(raw: &str) -> Result<Self, MemoryContextError> {
                Ok(Self(parse_identifier(raw, $field)?))
            }

            /// The validated identifier text.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

bounded_identifier!(
    /// Host-minted lease identity (spec §6.2). Model-created lease IDs are an
    /// explicitly unsafe pattern (spec §19); only [`MemoryContextLease::grant`]
    /// callers mint these.
    MemoryLeaseId,
    "lease_id"
);
bounded_identifier!(
    /// Chat-session identity a lease is scoped to.
    // TODO(task A3/A4): move to shared location — unify with the engine's
    // session identity when the control tool / context provider land.
    SessionId,
    "session_id"
);
bounded_identifier!(
    /// Project isolation boundary (spec §5.2).
    // TODO(task A3/A4): move to shared location
    ProjectId,
    "project_id"
);
bounded_identifier!(
    /// Manifest-declared context-provider identity (spec §7.1).
    // TODO(task A3/A4): move to shared location
    ContextProviderId,
    "provider_id"
);
bounded_identifier!(
    /// Identity of the exact frontend command request that authorized a
    /// transition (spec §6.3 `ExplicitCommand`).
    // TODO(task A3/A4): move to shared location
    RequestId,
    "command_id"
);
bounded_identifier!(
    /// Identity of a host-driven confirmation exchange (spec §6.3
    /// `ConfirmedPrompt`).
    // TODO(task A3/A4): move to shared location
    ConfirmationId,
    "confirmation_id"
);

/// Digest of the exact current user message proving it directly names the
/// requested memory transition (spec §6.3 `ExactCurrentRequest`).
// TODO(task A3/A4): move to shared location
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MessageDigest([u8; 32]);

impl MessageDigest {
    /// Wrap a host-computed digest of the current user message.
    #[allow(dead_code)] // consumed by the authorization policy in task A3/A4
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Capture policy carried by a lease (spec §6.2). Placeholder shape until the
/// capture flow lands.
// TODO(task A3/A4): move to shared location — full policy shape lands with
// the capture flow (spec §7.5, §8, §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CapturePolicy {
    /// Capture only host-classified eligible turns (spec §8.2).
    #[default]
    EligibleTurnsOnly,
}

/// Recall policy carried by a lease (spec §6.2). Placeholder shape until the
/// recall flow lands.
// TODO(task A3/A4): move to shared location — full policy shape lands with
// the per-prompt recall flow (spec §7.4, §9, §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecallPolicy {
    /// One bounded recall attempt per eligible prompt (spec §10.3 budget).
    #[default]
    BoundedPerPrompt,
}

// ---------------------------------------------------------------------------
// §6.1 Memory modes
// ---------------------------------------------------------------------------

/// Continuous-memory context mode (spec §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemoryContextMode {
    /// No automatic recall, no turn capture; persists until changed.
    Off,
    /// Automatic recall on the next eligible prompt only; consumed once.
    RecallOnce,
    /// Automatic recall on every eligible prompt; session lease.
    RecallEachPrompt,
    /// Turn capture only, no automatic recall; session lease.
    CaptureOnly,
    /// Automatic recall and turn capture; session lease.
    CaptureAndRecall,
}

/// Automatic-recall column of the spec §6.1 semantics table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomaticRecall {
    /// No automatic recall.
    Never,
    /// Recall on the next eligible prompt only.
    NextEligiblePromptOnly,
    /// Recall on every eligible prompt.
    EveryEligiblePrompt,
}

/// Turn-capture column of the spec §6.1 semantics table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnCapture {
    /// Turns are not captured.
    Disabled,
    /// Eligible turns are captured.
    Enabled,
}

/// Lifetime column of the spec §6.1 semantics table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeLifetime {
    /// Persists until the user changes it (`Off`).
    UntilChanged,
    /// Consumed by exactly one eligible prompt (`RecallOnce`).
    ConsumedOnce,
    /// Lives as a session lease until revoked or expired.
    SessionLease,
}

impl MemoryContextMode {
    /// Canonical spec §6.1 mode string — the exact vocabulary the
    /// `memory.default_mode` config surface (task A2) and the `/memory`
    /// frontend command status output (task A5) share.
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryContextMode::Off => "off",
            MemoryContextMode::RecallOnce => "recall_once",
            MemoryContextMode::RecallEachPrompt => "recall_each_prompt",
            MemoryContextMode::CaptureOnly => "capture_only",
            MemoryContextMode::CaptureAndRecall => "capture_and_recall",
        }
    }

    /// Automatic-recall semantics (spec §6.1 table, column 2). Exhaustive.
    pub fn automatic_recall(self) -> AutomaticRecall {
        match self {
            MemoryContextMode::Off | MemoryContextMode::CaptureOnly => AutomaticRecall::Never,
            MemoryContextMode::RecallOnce => AutomaticRecall::NextEligiblePromptOnly,
            MemoryContextMode::RecallEachPrompt | MemoryContextMode::CaptureAndRecall => {
                AutomaticRecall::EveryEligiblePrompt
            }
        }
    }

    /// Turn-capture semantics (spec §6.1 table, column 3). Exhaustive.
    pub fn turn_capture(self) -> TurnCapture {
        match self {
            MemoryContextMode::Off
            | MemoryContextMode::RecallOnce
            | MemoryContextMode::RecallEachPrompt => TurnCapture::Disabled,
            MemoryContextMode::CaptureOnly | MemoryContextMode::CaptureAndRecall => {
                TurnCapture::Enabled
            }
        }
    }

    /// Lifetime semantics (spec §6.1 table, column 4). Exhaustive.
    pub fn lifetime(self) -> ModeLifetime {
        match self {
            MemoryContextMode::Off => ModeLifetime::UntilChanged,
            MemoryContextMode::RecallOnce => ModeLifetime::ConsumedOnce,
            MemoryContextMode::RecallEachPrompt
            | MemoryContextMode::CaptureOnly
            | MemoryContextMode::CaptureAndRecall => ModeLifetime::SessionLease,
        }
    }
}

// ---------------------------------------------------------------------------
// §6.3 User intent proof
// ---------------------------------------------------------------------------

/// Proof of exact user intent behind a memory transition (spec §6.3).
///
/// `ExactCurrentRequest` may be used only when the host's authorization
/// policy can prove that the current user request directly names the exact
/// memory transition; otherwise the frontend asks for confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserIntentProof {
    /// A deterministic frontend command (e.g. `/memory on`).
    ExplicitCommand {
        /// The exact command request identity.
        command_id: RequestId,
    },
    /// A host-driven confirmation exchange the user answered affirmatively.
    ConfirmedPrompt {
        /// The confirmation exchange identity.
        confirmation_id: ConfirmationId,
    },
    /// The current user message itself names the exact transition.
    ExactCurrentRequest {
        /// Digest of that exact user message.
        user_message_digest: MessageDigest,
    },
}

/// Mint one host-owned `ExplicitCommand` intent proof for a `/memory`
/// frontend command invocation (spec §6.3; spec assumption 5: a deterministic
/// slash command is authoritative). Host code only — the command identity is
/// generated here and never accepted from model or plugin text.
pub(crate) fn mint_explicit_command_proof() -> UserIntentProof {
    UserIntentProof::ExplicitCommand {
        command_id: RequestId::parse(&format!("memcmd-{}", uuid::Uuid::new_v4()))
            .expect("generated command id is always valid"),
    }
}

// ---------------------------------------------------------------------------
// §6.2 Lease
// ---------------------------------------------------------------------------

/// A host-granted continuous-memory lease (spec §6.2).
///
/// Construction is host-private (mirroring the authority discipline of
/// `crate::orchestration::validate_user_authorizable_model`): the only
/// constructor, [`MemoryContextLease::grant`], is `pub(crate)`, the struct is
/// `#[non_exhaustive]`, and there is deliberately **no** `Deserialize` impl —
/// deserialization from extension or model output is forbidden.
///
/// Struct-literal construction outside the engine crate does not compile:
///
/// ```compile_fail
/// use agent_engine::runtime::memory_context::MemoryContextLease;
///
/// fn forge(lease: MemoryContextLease) -> MemoryContextLease {
///     // E0639: `MemoryContextLease` is `#[non_exhaustive]` outside its crate.
///     MemoryContextLease { ..lease }
/// }
/// ```
///
/// The `grant` constructor is inaccessible outside the engine crate:
///
/// ```compile_fail
/// use agent_engine::runtime::memory_context::MemoryContextLease;
///
/// fn probe() {
///     // E0624: `grant` is `pub(crate)`.
///     let _ = MemoryContextLease::grant;
/// }
/// ```
///
/// And no `Deserialize` impl exists, so wire input cannot mint a lease:
///
/// ```compile_fail
/// fn requires_deserialize<T: for<'de> serde::Deserialize<'de>>() {}
///
/// fn probe() {
///     // E0277: `MemoryContextLease: Deserialize` is not satisfied.
///     requires_deserialize::<agent_engine::runtime::memory_context::MemoryContextLease>();
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct MemoryContextLease {
    /// Host-minted lease identity.
    pub lease_id: MemoryLeaseId,
    /// The session this lease is scoped to.
    pub session_id: SessionId,
    /// The project isolation boundary.
    pub project_id: ProjectId,
    /// The context provider this lease activates.
    pub provider_id: ContextProviderId,
    /// The granted memory mode.
    pub mode: MemoryContextMode,
    /// The capture policy in force under this lease.
    pub capture_policy: CapturePolicy,
    /// The recall policy in force under this lease.
    pub recall_policy: RecallPolicy,
    /// Proof of the exact user intent that authorized this lease.
    pub granted_by: UserIntentProof,
    /// When the host granted the lease.
    pub granted_at: SystemTime,
    /// Optional hard expiry; `None` means until revoked.
    pub expires_at: Option<SystemTime>,
}

impl MemoryContextLease {
    /// Host-private minting gate. Callers are crate-internal authorization
    /// paths that already hold a [`UserIntentProof`]; plugin/model/extension
    /// code can never reach this. Fails closed on an expiry that is not
    /// strictly after the grant instant.
    #[allow(clippy::too_many_arguments)] // field order mirrors spec §6.2 exactly
    #[allow(dead_code)] // called by the authorization policy in task A3/A4
    pub(crate) fn grant(
        lease_id: MemoryLeaseId,
        session_id: SessionId,
        project_id: ProjectId,
        provider_id: ContextProviderId,
        mode: MemoryContextMode,
        capture_policy: CapturePolicy,
        recall_policy: RecallPolicy,
        granted_by: UserIntentProof,
        granted_at: SystemTime,
        expires_at: Option<SystemTime>,
    ) -> Result<Self, MemoryContextError> {
        if let Some(expires_at) = expires_at {
            if expires_at <= granted_at {
                return Err(MemoryContextError::InvalidExpiry);
            }
        }
        Ok(Self {
            lease_id,
            session_id,
            project_id,
            provider_id,
            mode,
            capture_policy,
            recall_policy,
            granted_by,
            granted_at,
            expires_at,
        })
    }

    /// Whether the lease is expired at `now`. `None` never expires.
    fn is_expired_at(&self, now: SystemTime) -> bool {
        self.expires_at.is_some_and(|expires_at| expires_at <= now)
    }
}

// ---------------------------------------------------------------------------
// §19 Authorized actions and session state
// ---------------------------------------------------------------------------

/// A fully authorized memory transition (spec §19). Because every
/// lease-bearing variant embeds a [`MemoryContextLease`] — which only host
/// code can mint — an `AuthorizedMemoryAction` carrying authority cannot be
/// forged outside this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorizedMemoryAction {
    /// Install a session lease (`RecallEachPrompt`, `CaptureOnly`, or
    /// `CaptureAndRecall`).
    Enable {
        /// The host-granted lease to install.
        lease: MemoryContextLease,
    },
    /// Install a one-shot recall lease (`RecallOnce`).
    RecallOnce {
        /// The host-granted one-shot lease to install.
        lease: MemoryContextLease,
    },
    /// Immediately revoke this session's memory context.
    Disable {
        /// The session whose context is revoked.
        session: SessionId,
    },
}

/// Durable (session-lease) slot of [`SessionMemoryState`]. Exhaustive — no
/// `enabled: bool`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DurableSlot {
    /// No session lease installed — effective mode `Off`.
    Empty,
    /// An installed session lease.
    Active(MemoryContextLease),
}

/// One-shot (`RecallOnce`) slot of [`SessionMemoryState`]. The `Consumed`
/// marker makes one-shot consumption exact across retries (spec §20.1): a
/// replay of the same logical request cannot recall twice.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OneShotSlot {
    /// No one-shot recall pending or recorded.
    Empty,
    /// A granted one-shot recall awaiting its next eligible prompt.
    Pending(MemoryContextLease),
    /// A one-shot recall that has been consumed exactly once.
    Consumed(MemoryLeaseId),
}

/// Per-session memory-context state machine. All transitions are exhaustive
/// over (current slot state, action); there are no boolean mode flags and no
/// raw JSON past this boundary (spec §19).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMemoryState {
    session_id: SessionId,
    durable: DurableSlot,
    one_shot: OneShotSlot,
}

impl SessionMemoryState {
    /// Fresh state for a session: memory is off by default (spec §21) — no
    /// durable lease, no one-shot recall. Crate-private like the lease gate.
    #[allow(dead_code)] // constructed by engine session wiring in task A3/A4
    pub(crate) fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            durable: DurableSlot::Empty,
            one_shot: OneShotSlot::Empty,
        }
    }

    /// The session identity this state machine is scoped to.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The effective durable mode (`Off` when no session lease is installed).
    pub fn active_mode(&self) -> MemoryContextMode {
        match &self.durable {
            DurableSlot::Empty => MemoryContextMode::Off,
            DurableSlot::Active(lease) => lease.mode,
        }
    }

    /// Install a session lease (spec §19 `Enable`). Only session-lease modes
    /// are installable here: `Off` is reached via [`Self::revoke`] and
    /// `RecallOnce` via [`Self::install_one_shot`] — both rejections fail
    /// closed. An already-active lease is replaced: each install carries its
    /// own fresh user authorization proof.
    pub fn install(&mut self, lease: MemoryContextLease) -> Result<(), MemoryContextError> {
        self.install_at(lease, SystemTime::now())
    }

    fn install_at(
        &mut self,
        lease: MemoryContextLease,
        now: SystemTime,
    ) -> Result<(), MemoryContextError> {
        if lease.session_id != self.session_id {
            return Err(MemoryContextError::SessionMismatch);
        }
        if lease.is_expired_at(now) {
            return Err(MemoryContextError::LeaseExpired {
                lease_id: lease.lease_id,
            });
        }
        match lease.mode {
            mode @ (MemoryContextMode::Off | MemoryContextMode::RecallOnce) => {
                Err(MemoryContextError::NotSessionLeaseMode { mode })
            }
            MemoryContextMode::RecallEachPrompt
            | MemoryContextMode::CaptureOnly
            | MemoryContextMode::CaptureAndRecall => {
                // Exhaustive over the current slot: both states accept the
                // freshly re-authorized replacement.
                self.durable = match std::mem::replace(&mut self.durable, DurableSlot::Empty) {
                    DurableSlot::Empty | DurableSlot::Active(_) => DurableSlot::Active(lease),
                };
                Ok(())
            }
        }
    }

    /// Install a one-shot recall lease (spec §19 `RecallOnce`). Exactness
    /// guarantees (spec §20.1):
    /// - only one pending one-shot at a time (`OneShotAlreadyPending`);
    /// - re-installing a consumed lease ID is rejected
    ///   (`OneShotAlreadyConsumed`) so retries cannot recall twice;
    /// - a *different* lease ID after consumption is a fresh grant and
    ///   installs normally.
    pub fn install_one_shot(
        &mut self,
        lease: MemoryContextLease,
    ) -> Result<(), MemoryContextError> {
        self.install_one_shot_at(lease, SystemTime::now())
    }

    fn install_one_shot_at(
        &mut self,
        lease: MemoryContextLease,
        now: SystemTime,
    ) -> Result<(), MemoryContextError> {
        if lease.session_id != self.session_id {
            return Err(MemoryContextError::SessionMismatch);
        }
        if lease.is_expired_at(now) {
            return Err(MemoryContextError::LeaseExpired {
                lease_id: lease.lease_id,
            });
        }
        match lease.mode {
            MemoryContextMode::RecallOnce => match &self.one_shot {
                OneShotSlot::Empty => {
                    self.one_shot = OneShotSlot::Pending(lease);
                    Ok(())
                }
                OneShotSlot::Pending(existing) => Err(MemoryContextError::OneShotAlreadyPending {
                    lease_id: existing.lease_id.clone(),
                }),
                OneShotSlot::Consumed(consumed) if *consumed == lease.lease_id => {
                    Err(MemoryContextError::OneShotAlreadyConsumed {
                        lease_id: consumed.clone(),
                    })
                }
                OneShotSlot::Consumed(_) => {
                    self.one_shot = OneShotSlot::Pending(lease);
                    Ok(())
                }
            },
            mode @ (MemoryContextMode::Off
            | MemoryContextMode::RecallEachPrompt
            | MemoryContextMode::CaptureOnly
            | MemoryContextMode::CaptureAndRecall) => {
                Err(MemoryContextError::NotOneShotMode { mode })
            }
        }
    }

    /// Consume the pending one-shot recall for an eligible prompt. Transitions
    /// `Pending → Consumed` exactly once and returns the lease; every repeat
    /// fails closed. An expired pending lease is dropped unused.
    pub fn consume_one_shot(&mut self) -> Result<MemoryContextLease, MemoryContextError> {
        self.consume_one_shot_at(SystemTime::now())
    }

    fn consume_one_shot_at(
        &mut self,
        now: SystemTime,
    ) -> Result<MemoryContextLease, MemoryContextError> {
        match std::mem::replace(&mut self.one_shot, OneShotSlot::Empty) {
            OneShotSlot::Empty => Err(MemoryContextError::NoPendingRecall),
            OneShotSlot::Pending(lease) => {
                if lease.is_expired_at(now) {
                    // Never used: the slot stays Empty and the caller gets a
                    // typed, content-free failure.
                    Err(MemoryContextError::LeaseExpired {
                        lease_id: lease.lease_id,
                    })
                } else {
                    self.one_shot = OneShotSlot::Consumed(lease.lease_id.clone());
                    Ok(lease)
                }
            }
            OneShotSlot::Consumed(lease_id) => {
                self.one_shot = OneShotSlot::Consumed(lease_id.clone());
                Err(MemoryContextError::OneShotAlreadyConsumed { lease_id })
            }
        }
    }

    /// Immediately revoke this session's memory context (spec §19 `Disable`).
    /// A foreign session identity is a fail-closed no-op. The `Consumed`
    /// replay marker survives revocation: disabling memory must not re-arm a
    /// consumed logical request; a genuinely new grant uses a fresh lease ID.
    pub fn revoke(&mut self, session: &SessionId) {
        if *session != self.session_id {
            return;
        }
        self.durable = DurableSlot::Empty;
        self.one_shot = match std::mem::replace(&mut self.one_shot, OneShotSlot::Empty) {
            OneShotSlot::Empty | OneShotSlot::Pending(_) => OneShotSlot::Empty,
            consumed @ OneShotSlot::Consumed(_) => consumed,
        };
    }

    /// Crate-private (task A6): provider identities bound to currently
    /// granted leases — the installed durable session lease plus any
    /// pending one-shot recall. The host disable/session-end revocation
    /// path uses these to defensively revoke the backing extension runtime
    /// lease for each bound `extension:<plugin>:<id>` address (an
    /// idempotent no-op while nothing has ever spawned, exact reap once
    /// Phase B routes real calls). A consumed one-shot marker carries no
    /// live authority and is deliberately excluded.
    pub(crate) fn bound_provider_ids(&self) -> Vec<ContextProviderId> {
        let mut bound = Vec::new();
        if let DurableSlot::Active(lease) = &self.durable {
            bound.push(lease.provider_id.clone());
        }
        if let OneShotSlot::Pending(lease) = &self.one_shot {
            bound.push(lease.provider_id.clone());
        }
        bound
    }

    /// Typed status snapshot — safe to surface without spawning any provider
    /// process (spec §7.2 "status does not spawn").
    pub fn status(&self) -> MemoryContextStatus {
        MemoryContextStatus {
            session_id: self.session_id.clone(),
            durable: match &self.durable {
                DurableSlot::Empty => DurableStatus::Off,
                DurableSlot::Active(lease) => DurableStatus::Active {
                    mode: lease.mode,
                    lease_id: lease.lease_id.clone(),
                    expires_at: lease.expires_at,
                },
            },
            one_shot: match &self.one_shot {
                OneShotSlot::Empty => OneShotStatus::Idle,
                OneShotSlot::Pending(lease) => OneShotStatus::Pending {
                    lease_id: lease.lease_id.clone(),
                },
                OneShotSlot::Consumed(lease_id) => OneShotStatus::Consumed {
                    lease_id: lease_id.clone(),
                },
            },
        }
    }

    /// Test-only introspection: the intent proof recorded on the installed
    /// durable session lease, if any. Lets crate-internal tests prove the
    /// `/memory` command path (task A5) always grants under
    /// [`UserIntentProof::ExplicitCommand`] without exposing lease internals
    /// beyond tests.
    #[cfg(test)]
    pub(crate) fn durable_proof(&self) -> Option<&UserIntentProof> {
        match &self.durable {
            DurableSlot::Empty => None,
            DurableSlot::Active(lease) => Some(&lease.granted_by),
        }
    }

    /// Test-only introspection: the intent proof recorded on the pending
    /// one-shot recall lease, if any.
    #[cfg(test)]
    pub(crate) fn one_shot_pending_proof(&self) -> Option<&UserIntentProof> {
        match &self.one_shot {
            OneShotSlot::Empty | OneShotSlot::Consumed(_) => None,
            OneShotSlot::Pending(lease) => Some(&lease.granted_by),
        }
    }
}

/// Durable-slot component of [`MemoryContextStatus`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DurableStatus {
    /// No session lease — memory is off.
    Off,
    /// A session lease is active.
    Active {
        /// The active mode.
        mode: MemoryContextMode,
        /// The active lease identity.
        lease_id: MemoryLeaseId,
        /// Optional hard expiry of the active lease.
        expires_at: Option<SystemTime>,
    },
}

/// One-shot-slot component of [`MemoryContextStatus`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OneShotStatus {
    /// No one-shot recall pending or recorded.
    Idle,
    /// A one-shot recall awaits the next eligible prompt.
    Pending {
        /// The pending lease identity.
        lease_id: MemoryLeaseId,
    },
    /// The recorded one-shot recall was consumed exactly once.
    Consumed {
        /// The consumed lease identity.
        lease_id: MemoryLeaseId,
    },
}

/// Typed, content-free status of a session's memory context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryContextStatus {
    /// The session this status describes.
    pub session_id: SessionId,
    /// Durable session-lease state.
    pub durable: DurableStatus,
    /// One-shot recall state.
    pub one_shot: OneShotStatus,
}

impl MemoryContextStatus {
    /// Human-readable `/memory status` text (task A5, spec §7.3).
    /// Metadata-only, mirroring `TraceStatusReport::render`: mode, lease
    /// identity, lease expiry, and one-shot slot — never memory content.
    pub fn render(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        match &self.durable {
            DurableStatus::Off => {
                let _ = writeln!(out, "memory: mode off (no session lease)");
            }
            DurableStatus::Active {
                mode,
                lease_id,
                expires_at,
            } => {
                let _ = writeln!(out, "memory: mode {} (session lease)", mode.as_str());
                let _ = writeln!(
                    out,
                    "  lease: {} — {}",
                    lease_id.as_str(),
                    render_lease_expiry(*expires_at),
                );
            }
        }
        match &self.one_shot {
            OneShotStatus::Idle => {
                let _ = writeln!(out, "  one-shot recall: none pending");
            }
            OneShotStatus::Pending { lease_id } => {
                let _ = writeln!(
                    out,
                    "  one-shot recall: pending (lease {})",
                    lease_id.as_str()
                );
            }
            OneShotStatus::Consumed { lease_id } => {
                let _ = writeln!(
                    out,
                    "  one-shot recall: consumed (lease {})",
                    lease_id.as_str()
                );
            }
        }
        out.trim_end().to_string()
    }
}

/// Render a lease's expiry column: relative seconds remaining, `expired`,
/// or the no-expiry (until revoked) wording. Always mentions "expiry" so
/// status text is self-describing.
fn render_lease_expiry(expires_at: Option<SystemTime>) -> String {
    match expires_at {
        None => "no expiry (until revoked or session end)".to_string(),
        Some(at) => match at.duration_since(SystemTime::now()) {
            Ok(left) => format!("expiry in {}s", left.as_secs()),
            Err(_) => "expiry passed (lease expired)".to_string(),
        },
    }
}

/// Typed memory-context failure. Deliberately content-free (spec §19: no
/// content-bearing errors): only host-minted identities and static field
/// names appear.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryContextError {
    /// The lease or revocation targets a different session.
    SessionMismatch,
    /// `install` requires a session-lease mode (`RecallEachPrompt`,
    /// `CaptureOnly`, `CaptureAndRecall`).
    NotSessionLeaseMode {
        /// The rejected mode.
        mode: MemoryContextMode,
    },
    /// `install_one_shot` requires `RecallOnce`.
    NotOneShotMode {
        /// The rejected mode.
        mode: MemoryContextMode,
    },
    /// A one-shot recall is already pending; consume or revoke it first.
    OneShotAlreadyPending {
        /// The pending lease identity.
        lease_id: MemoryLeaseId,
    },
    /// This one-shot lease was already consumed; retries cannot recall twice.
    OneShotAlreadyConsumed {
        /// The consumed lease identity.
        lease_id: MemoryLeaseId,
    },
    /// No one-shot recall is pending.
    NoPendingRecall,
    /// The lease is expired.
    LeaseExpired {
        /// The expired lease identity.
        lease_id: MemoryLeaseId,
    },
    /// `expires_at` is not strictly after `granted_at`.
    InvalidExpiry,
    /// A boundary identifier was empty, oversized, or contained control
    /// characters.
    InvalidIdentifier {
        /// Static name of the offending field.
        field: &'static str,
    },
    /// The requested transition can only be committed under deterministic,
    /// host-owned proof of user intent (spec §6.3 `ExplicitCommand` from the
    /// `/memory` frontend command, task A5). A model tool call is a proposal
    /// (spec §7.2 rules) and cannot carry such proof through JSON parameters,
    /// so it is refused without installing any lease.
    RequiresHostConfirmation,
    /// No memory-context capability is wired into this execution context;
    /// lease-granting actions are unavailable and nothing was mutated.
    CapabilityUnavailable,
    /// Enable-time provider validation failed (task A6, spec §7.1): no
    /// loaded extension declares the requested context provider as a
    /// validated `deferred.context_providers` capability. Fail closed —
    /// nothing was granted.
    ProviderNotRegistered,
    /// Enable-time provider validation failed (task A6): more than one
    /// installed extension declares an overlapping context-provider
    /// capability and no explicit provider id disambiguates the request.
    /// Fail closed — the host never picks one arbitrarily, and nothing
    /// was granted.
    ProviderAmbiguous,
    /// A recall contribution's project identity does not match the host's
    /// expected project (spec §5.2 isolation) — rejected fail-closed, the
    /// contribution never influences budgeting or request assembly.
    ContributionProjectMismatch,
    /// A recall contribution exceeded a spec §10.3 bound (record count,
    /// per-record size, or engine-provided rendered budget). Content-free:
    /// only the static field name is carried.
    ContributionOutOfBounds {
        /// Static name of the offending field.
        field: &'static str,
    },
    /// The computed §10.3 recall budget is below the minimum useful floor —
    /// recall is skipped rather than reducing core reserves; nothing was
    /// held.
    RecallBudgetBelowMinimum,
}

impl std::fmt::Display for MemoryContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoryContextError::SessionMismatch => {
                write!(f, "memory context action targets a different session")
            }
            MemoryContextError::NotSessionLeaseMode { mode } => {
                write!(f, "mode {mode:?} cannot be installed as a session lease")
            }
            MemoryContextError::NotOneShotMode { mode } => {
                write!(f, "mode {mode:?} cannot be installed as a one-shot recall")
            }
            MemoryContextError::OneShotAlreadyPending { lease_id } => {
                write!(f, "a one-shot recall is already pending: {}", lease_id.as_str())
            }
            MemoryContextError::OneShotAlreadyConsumed { lease_id } => {
                write!(f, "one-shot recall already consumed: {}", lease_id.as_str())
            }
            MemoryContextError::NoPendingRecall => write!(f, "no one-shot recall is pending"),
            MemoryContextError::LeaseExpired { lease_id } => {
                write!(f, "memory lease expired: {}", lease_id.as_str())
            }
            MemoryContextError::InvalidExpiry => {
                write!(f, "lease expiry must be strictly after the grant instant")
            }
            MemoryContextError::InvalidIdentifier { field } => {
                write!(f, "invalid memory-context identifier: {field}")
            }
            MemoryContextError::RequiresHostConfirmation => write!(
                f,
                "memory transition requires deterministic host confirmation (run /memory); \
                 a model tool call is a proposal and cannot commit it"
            ),
            MemoryContextError::CapabilityUnavailable => write!(
                f,
                "memory-context capability is unavailable in this context"
            ),
            MemoryContextError::ProviderNotRegistered => write!(
                f,
                "no installed extension declares the requested memory context provider \
                 (nothing was granted)"
            ),
            MemoryContextError::ProviderAmbiguous => write!(
                f,
                "multiple installed extensions declare overlapping memory context providers; \
                 an exact provider id is required (nothing was granted)"
            ),
            MemoryContextError::ContributionProjectMismatch => write!(
                f,
                "memory contribution targets a different project (rejected, nothing admitted)"
            ),
            MemoryContextError::ContributionOutOfBounds { field } => {
                write!(f, "memory contribution exceeds its bound: {field}")
            }
            MemoryContextError::RecallBudgetBelowMinimum => write!(
                f,
                "memory recall budget is below the minimum useful floor; recall skipped"
            ),
        }
    }
}

impl std::error::Error for MemoryContextError {}

/// Apply one fully authorized memory transition to a session's state — the
/// single engine transition every frontend shares (spec §19, verbatim shape).
pub fn apply_memory_context_action(
    state: &mut SessionMemoryState,
    action: AuthorizedMemoryAction,
) -> Result<MemoryContextStatus, MemoryContextError> {
    match action {
        AuthorizedMemoryAction::Enable { lease } => {
            state.install(lease)?;
            Ok(state.status())
        }
        AuthorizedMemoryAction::RecallOnce { lease } => {
            state.install_one_shot(lease)?;
            Ok(state.status())
        }
        AuthorizedMemoryAction::Disable { session } => {
            state.revoke(&session);
            Ok(state.status())
        }
    }
}

// ---------------------------------------------------------------------------
// §7.2 Session-scoped capability handle (task A4)
// ---------------------------------------------------------------------------

/// Session-scoped memory-context capability handed to tool contexts (task
/// A4), mirroring the shape of [`crate::mcp::McpLeaseCapability`] and
/// [`crate::extensions::lease::ExtensionLeaseCapability`]: a small `Clone`
/// handle over the shared per-session [`SessionMemoryState`] that the
/// `memory_context` builtin uses to read and mutate memory-context state.
///
/// Authority discipline: construction is `pub(crate)` — only host wiring can
/// mint one — and every mutation routes through the exhaustive
/// [`apply_memory_context_action`] transition table. The capability can only
/// commit actions that are always locally safe/revocable (spec §7.2 rules:
/// `disable`, `status`, one-shot `recall_once`); durable `enable` and
/// `index_history` need deterministic host-owned proof (`ExplicitCommand`
/// from `/memory`, task A5) that never flows through this handle.
#[derive(Clone)]
pub struct MemoryContextCapability {
    /// Shared session state — the same `Arc` the host's `/memory` command
    /// path mutates (task A5), so tool and frontend observe one truth.
    state: Arc<Mutex<SessionMemoryState>>,
    /// Project isolation boundary leases minted through this handle are
    /// scoped to (spec §5.2).
    project_id: ProjectId,
    /// Exact context provider one-shot leases activate (spec §7.1).
    provider_id: ContextProviderId,
    /// Host-attributed provenance recorded on locally-safe one-shot grants.
    /// Supplied by host wiring at construction — never by model JSON. Task
    /// A5 replaces this with per-request `ExplicitCommand` /
    /// `ExactCurrentRequest` proof plumbing.
    one_shot_proof: UserIntentProof,
}

impl MemoryContextCapability {
    /// Host-private construction (mirrors the [`MemoryContextLease::grant`]
    /// gate): untrusted code cannot mint a capability, so a `None` slot in
    /// `ToolCapabilities` can never be filled from outside the engine crate.
    #[allow(dead_code)] // called by host session wiring in task A5
    pub(crate) fn new(
        state: Arc<Mutex<SessionMemoryState>>,
        project_id: ProjectId,
        provider_id: ContextProviderId,
        one_shot_proof: UserIntentProof,
    ) -> Self {
        Self {
            state,
            project_id,
            provider_id,
            one_shot_proof,
        }
    }

    /// Lock the shared state. Poison recovery is sound here: every
    /// `SessionMemoryState` transition is a check-then-single-assignment with
    /// no panicking code between, so a poisoned lock still holds a consistent
    /// state and fail-open recovery cannot observe a half-applied transition.
    fn lock(&self) -> std::sync::MutexGuard<'_, SessionMemoryState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Typed status snapshot — metadata-only, never spawns a provider
    /// (spec §7.2 "status does not spawn").
    pub fn status(&self) -> MemoryContextStatus {
        self.lock().status()
    }

    /// Revoke this session's memory context (spec §7.2 "`disable` is always
    /// locally allowed"). Idempotent: revoking an already-off session is a
    /// no-op that reports `Off`.
    pub fn disable(&self) -> MemoryContextStatus {
        let mut state = self.lock();
        let session = state.session_id.clone();
        // Infallible by construction: the session identity is read from the
        // state itself, and `Disable` is total over the transition table.
        match apply_memory_context_action(
            &mut state,
            AuthorizedMemoryAction::Disable { session },
        ) {
            Ok(status) => status,
            Err(_) => state.status(),
        }
    }

    /// Grant and install exactly one one-shot recall lease (spec §7.2
    /// "`recall_once` grants a one-shot lease"). Locally safe: consumed by at
    /// most one eligible prompt and revocable via [`Self::disable`]. Fails
    /// typed when a one-shot is already pending or the expiry is invalid —
    /// on failure no lease is installed.
    pub(crate) fn recall_once(
        &self,
        expires_minutes: Option<u32>,
    ) -> Result<MemoryContextStatus, MemoryContextError> {
        let granted_at = SystemTime::now();
        let expires_at =
            expires_minutes.map(|m| granted_at + Duration::from_secs(u64::from(m) * 60));
        let mut state = self.lock();
        let lease = MemoryContextLease::grant(
            MemoryLeaseId::parse(&format!("memctx-once-{}", uuid::Uuid::new_v4()))?,
            state.session_id.clone(),
            self.project_id.clone(),
            self.provider_id.clone(),
            MemoryContextMode::RecallOnce,
            CapturePolicy::default(),
            RecallPolicy::default(),
            self.one_shot_proof.clone(),
            granted_at,
            expires_at,
        )?;
        apply_memory_context_action(&mut state, AuthorizedMemoryAction::RecallOnce { lease })
    }
}

impl std::fmt::Debug for MemoryContextCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryContextCapability")
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// §6.5 / §10.1 Recall contribution — typed segment, never raw text
// ---------------------------------------------------------------------------

bounded_identifier!(
    /// Version of the contribution schema a provider produced (spec §6.5).
    /// Parse-at-the-boundary: empty/oversized/control input fails closed, so
    /// a constructed value is never empty.
    ContributionSchemaVersion,
    "contribution_schema"
);
bounded_identifier!(
    /// Identity of one stored memory record (spec §6.5).
    MemoryId,
    "memory_id"
);

/// Source class of a recalled record (spec §6.5). Minimal closed set for the
/// Phase A typed boundary; the retrieval pipeline (task B1) extends it to the
/// full spec §9 vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MemorySource {
    /// Consolidated from captured chat history (spec §8).
    #[default]
    ChatHistory,
    /// Explicitly stated by the user.
    UserStated,
}

/// Why the provider ranked a record into the contribution (spec §6.5, §10.4
/// explainability). Minimal closed set until task B1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankReason {
    /// Exact topical match against the recall query.
    ExactTopic,
    /// Recency-weighted selection.
    Recency,
}

/// Sensitivity class of a recalled record (spec §6.5). Mirrors the workspace
/// memory-store vocabulary; disclosure enforcement lands in task B1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sensitivity {
    /// Ordinary project data.
    #[default]
    Normal,
    /// Secret-class content — bodies are never disclosed in rendered output.
    Secret,
}

/// Retention class of a recalled record (spec §6.5). Placeholder single
/// variant until the consolidation/retention flow lands (spec §11, Phase B).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RetentionClass {
    /// Standard project retention.
    #[default]
    Standard,
}

/// Provider-reported recall accounting (spec §6.5, §10.4): bounded counters
/// only — never content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContributionAccounting {
    /// Candidate records the provider considered before selection.
    pub candidates_considered: u32,
    /// Records withheld by disclosure policy (counted, never named).
    pub withheld: u32,
    /// Records truncated to fit their per-record bound.
    pub truncated: u32,
}

/// One record inside a recall contribution (spec §6.5, verbatim shape).
/// Content fields are [`BoundedText`] — byte-bounded at construction, exact
/// truncation accounting preserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryContributionRecord {
    /// Identity of the stored memory this record was recalled from.
    pub memory_id: MemoryId,
    /// Source class of the record.
    pub source: MemorySource,
    /// When the underlying memory was recorded.
    pub timestamp: SystemTime,
    /// Why the provider ranked this record in (explainability, spec §10.4).
    pub rank_reason: Vec<RankReason>,
    /// Sensitivity class of the record.
    pub sensitivity: Sensitivity,
    /// Retention class of the record.
    pub retention: RetentionClass,
    /// Bounded record body.
    pub content: BoundedText,
    /// Whether the provider truncated the body to fit its bound.
    pub truncated: bool,
    /// The memory this record supersedes, if any.
    pub supersedes: Option<MemoryId>,
}

/// A provider's recall contribution for one turn (spec §6.5, verbatim
/// shape). The host accepts one only through [`validate_contribution`] —
/// project identity, schema, and bounds are checked before it may influence
/// budgeting or (in task B4) request assembly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryContextContribution {
    /// Contribution schema version the provider produced.
    pub schema: ContributionSchemaVersion,
    /// The provider that produced this contribution.
    pub provider_id: ContextProviderId,
    /// Project isolation boundary the records belong to (spec §5.2).
    pub project_id: ProjectId,
    /// The selected records (≤ [`MEMORY_MAX_SELECTED_RECORDS`]).
    pub records: Vec<MemoryContributionRecord>,
    /// The bounded rendered text the engine budgets (and, in task B4,
    /// injects as a typed segment — never a system message, spec §5.3).
    pub rendered: BoundedText,
    /// Bounded recall accounting counters.
    pub accounting: ContributionAccounting,
}

/// Typed context segment admitted to provider request assembly (spec §10.1):
/// accepted memory enters as `ContextSegment::Memory(...)`, never as raw
/// text, never appended to the user message, and never as system policy.
/// Deliberately a single-variant enum today — it exists to make the "typed
/// segment, not raw text" boundary explicit and extensible. Per-provider
/// wire translation of the segment is task B4; in Phase A the segment only
/// feeds the T29 context-budget lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextSegment {
    /// An accepted, validated memory recall contribution.
    Memory(MemoryContextContribution),
}

impl ContextSegment {
    /// The bounded rendered text this segment contributes to context
    /// budgeting (T29 `memory_contents` lane).
    pub fn rendered_text(&self) -> &str {
        match self {
            ContextSegment::Memory(contribution) => &contribution.rendered.text,
        }
    }
}

// ---------------------------------------------------------------------------
// §10.3 Budget policy
// ---------------------------------------------------------------------------

/// Spec §10.3 hard ceiling on the memory recall budget, in estimated tokens.
pub const MEMORY_BUDGET_MAX_TOKENS: u64 = 4096;

/// Spec §10.3: memory may use at most this percentage of the effective
/// provider input capacity.
pub const MEMORY_BUDGET_CAPACITY_PERCENT: u64 = 10;

/// Spec §10.3 minimum useful recall budget in estimated tokens. Below this,
/// recall is skipped entirely — core safety/output/tool-result reserves are
/// NEVER reduced to make room for memory.
pub const MEMORY_BUDGET_MIN_TOKENS: u64 = 512;

/// Spec §10.3: maximum selected records per contribution.
pub const MEMORY_MAX_SELECTED_RECORDS: usize = 8;

/// Spec §10.3: maximum rendered bytes for one individual record (2 KiB).
pub const MEMORY_MAX_RENDERED_RECORD_BYTES: usize = 2048;

/// The memory recall budget for a request, per spec §10.3 exactly:
/// `min(4096, 10% of effective_provider_input_capacity)` estimated tokens,
/// or `None` (skip recall) when the computed value falls below the
/// [`MEMORY_BUDGET_MIN_TOKENS`] floor. Pure — no engine state is read.
pub fn memory_budget_tokens(effective_provider_input_capacity: u64) -> Option<u64> {
    let capacity_share = effective_provider_input_capacity
        .saturating_mul(MEMORY_BUDGET_CAPACITY_PERCENT)
        / 100;
    let budget = MEMORY_BUDGET_MAX_TOKENS.min(capacity_share);
    (budget >= MEMORY_BUDGET_MIN_TOKENS).then_some(budget)
}

/// Host-side contribution acceptance gate (spec §6.5: "The host validates
/// project ID, record count, record sizes, total size, disclosure classes,
/// and schema version"). Phase A scope — minimal but real:
///
/// - project identity mismatch fails closed (spec §5.2 isolation is a
///   Phase A invariant, never deferred);
/// - schema version must be non-empty (already guaranteed by the
///   [`ContributionSchemaVersion`] parse gate; re-checked here so the
///   validator stays the single acceptance authority when task B1 adds
///   boundary deserialization);
/// - record count, per-record size, and total rendered size are bounded
///   (`max_rendered_tokens` is the ENGINE-provided budget from
///   [`memory_budget_tokens`] — never plugin-chosen).
///
/// Disclosure-class and duplicate-ID checks belong to task B1.
pub fn validate_contribution(
    contribution: &MemoryContextContribution,
    expected_project: &ProjectId,
    max_rendered_tokens: u64,
) -> Result<(), MemoryContextError> {
    if contribution.project_id != *expected_project {
        return Err(MemoryContextError::ContributionProjectMismatch);
    }
    if contribution.schema.as_str().trim().is_empty() {
        return Err(MemoryContextError::ContributionOutOfBounds { field: "schema" });
    }
    if contribution.records.len() > MEMORY_MAX_SELECTED_RECORDS {
        return Err(MemoryContextError::ContributionOutOfBounds { field: "records" });
    }
    for record in &contribution.records {
        if record.content.retained_bytes > MEMORY_MAX_RENDERED_RECORD_BYTES {
            return Err(MemoryContextError::ContributionOutOfBounds {
                field: "record_content",
            });
        }
    }
    let rendered_tokens =
        crate::runtime::context::conservative_token_estimate(&contribution.rendered.text);
    if rendered_tokens > max_rendered_tokens {
        return Err(MemoryContextError::ContributionOutOfBounds { field: "rendered" });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const ALL_MODES: [MemoryContextMode; 5] = [
        MemoryContextMode::Off,
        MemoryContextMode::RecallOnce,
        MemoryContextMode::RecallEachPrompt,
        MemoryContextMode::CaptureOnly,
        MemoryContextMode::CaptureAndRecall,
    ];

    /// Durable modes a session can actually be in (RecallOnce never occupies
    /// the durable slot).
    const DURABLE_START_MODES: [MemoryContextMode; 4] = [
        MemoryContextMode::Off,
        MemoryContextMode::RecallEachPrompt,
        MemoryContextMode::CaptureOnly,
        MemoryContextMode::CaptureAndRecall,
    ];

    fn sid(raw: &str) -> SessionId {
        SessionId::parse(raw).expect("valid session id")
    }

    fn mint_at(
        session: &str,
        mode: MemoryContextMode,
        lease_id: &str,
        granted_at: SystemTime,
        expires_at: Option<SystemTime>,
    ) -> MemoryContextLease {
        MemoryContextLease::grant(
            MemoryLeaseId::parse(lease_id).expect("valid lease id"),
            sid(session),
            ProjectId::parse("proj-1").expect("valid project id"),
            ContextProviderId::parse("axel-memory").expect("valid provider id"),
            mode,
            CapturePolicy::default(),
            RecallPolicy::default(),
            UserIntentProof::ExplicitCommand {
                command_id: RequestId::parse("req-1").expect("valid request id"),
            },
            granted_at,
            expires_at,
        )
        .expect("grant succeeds")
    }

    fn mint(session: &str, mode: MemoryContextMode, lease_id: &str) -> MemoryContextLease {
        mint_at(session, mode, lease_id, SystemTime::now(), None)
    }

    /// A session state with the given durable mode installed (Off = fresh).
    fn state_in(mode: MemoryContextMode) -> SessionMemoryState {
        let mut state = SessionMemoryState::new(sid("sess-1"));
        match mode {
            MemoryContextMode::Off => {}
            MemoryContextMode::RecallOnce => unreachable!("not a durable mode"),
            durable => state
                .install(mint("sess-1", durable, "lease-seed"))
                .expect("seed install succeeds"),
        }
        state
    }

    // -- §6.1 semantics table -------------------------------------------------

    #[test]
    fn mode_semantics_match_spec_table_6_1_exactly() {
        use AutomaticRecall as Ar;
        use MemoryContextMode as M;
        use ModeLifetime as Lt;
        use TurnCapture as Tc;
        for mode in ALL_MODES {
            let (recall, capture, lifetime) = match mode {
                M::Off => (Ar::Never, Tc::Disabled, Lt::UntilChanged),
                M::RecallOnce => (Ar::NextEligiblePromptOnly, Tc::Disabled, Lt::ConsumedOnce),
                M::RecallEachPrompt => (Ar::EveryEligiblePrompt, Tc::Disabled, Lt::SessionLease),
                M::CaptureOnly => (Ar::Never, Tc::Enabled, Lt::SessionLease),
                M::CaptureAndRecall => (Ar::EveryEligiblePrompt, Tc::Enabled, Lt::SessionLease),
            };
            assert_eq!(mode.automatic_recall(), recall, "{mode:?} recall column");
            assert_eq!(mode.turn_capture(), capture, "{mode:?} capture column");
            assert_eq!(mode.lifetime(), lifetime, "{mode:?} lifetime column");
        }
    }

    // -- Exhaustive (mode × action) transitions ------------------------------

    #[test]
    fn every_durable_mode_by_enable_action_transition() {
        for start in DURABLE_START_MODES {
            for requested in ALL_MODES {
                let mut state = state_in(start);
                let action = AuthorizedMemoryAction::Enable {
                    lease: mint("sess-1", requested, "lease-next"),
                };
                let result = apply_memory_context_action(&mut state, action);
                match requested {
                    MemoryContextMode::Off | MemoryContextMode::RecallOnce => {
                        assert_eq!(
                            result,
                            Err(MemoryContextError::NotSessionLeaseMode { mode: requested }),
                            "Enable({requested:?}) from {start:?} must fail closed"
                        );
                        // Failed installs never mutate the durable slot.
                        assert_eq!(state.active_mode(), start);
                    }
                    MemoryContextMode::RecallEachPrompt
                    | MemoryContextMode::CaptureOnly
                    | MemoryContextMode::CaptureAndRecall => {
                        let status = result.expect("session-lease install succeeds");
                        assert_eq!(state.active_mode(), requested);
                        match status.durable {
                            DurableStatus::Active { mode, .. } => assert_eq!(mode, requested),
                            DurableStatus::Off => panic!("expected active durable status"),
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn every_durable_mode_by_recall_once_action_transition() {
        for start in DURABLE_START_MODES {
            let mut state = state_in(start);
            let action = AuthorizedMemoryAction::RecallOnce {
                lease: mint("sess-1", MemoryContextMode::RecallOnce, "lease-once"),
            };
            let status =
                apply_memory_context_action(&mut state, action).expect("one-shot install succeeds");
            // Durable slot is untouched; one-shot becomes pending.
            assert_eq!(state.active_mode(), start, "from {start:?}");
            assert_eq!(
                status.one_shot,
                OneShotStatus::Pending {
                    lease_id: MemoryLeaseId::parse("lease-once").unwrap()
                }
            );
        }
    }

    #[test]
    fn every_durable_mode_by_disable_action_transition() {
        for start in DURABLE_START_MODES {
            let mut state = state_in(start);
            let status = apply_memory_context_action(
                &mut state,
                AuthorizedMemoryAction::Disable {
                    session: sid("sess-1"),
                },
            )
            .expect("disable always succeeds for the owning session");
            assert_eq!(state.active_mode(), MemoryContextMode::Off, "from {start:?}");
            assert_eq!(status.durable, DurableStatus::Off);
            assert_eq!(status.one_shot, OneShotStatus::Idle);
        }
    }

    #[test]
    fn install_one_shot_rejects_every_non_recall_once_mode() {
        for mode in ALL_MODES {
            if mode == MemoryContextMode::RecallOnce {
                continue;
            }
            let mut state = state_in(MemoryContextMode::Off);
            assert_eq!(
                state.install_one_shot(mint("sess-1", mode, "lease-x")),
                Err(MemoryContextError::NotOneShotMode { mode }),
                "install_one_shot({mode:?}) must fail closed"
            );
            assert_eq!(state.status().one_shot, OneShotStatus::Idle);
        }
    }

    // -- Session isolation ---------------------------------------------------

    #[test]
    fn foreign_session_lease_fails_closed_and_foreign_revoke_is_noop() {
        let mut state = state_in(MemoryContextMode::CaptureAndRecall);
        assert_eq!(
            state.install(mint("sess-OTHER", MemoryContextMode::CaptureOnly, "lease-f")),
            Err(MemoryContextError::SessionMismatch)
        );
        assert_eq!(
            state.install_one_shot(mint("sess-OTHER", MemoryContextMode::RecallOnce, "lease-f")),
            Err(MemoryContextError::SessionMismatch)
        );
        state.revoke(&sid("sess-OTHER"));
        assert_eq!(
            state.active_mode(),
            MemoryContextMode::CaptureAndRecall,
            "foreign revoke must not disable the session"
        );
    }

    // -- One-shot exactness across repeated attempts -------------------------

    #[test]
    fn one_shot_install_consume_revoke_is_exact_across_repeated_attempts() {
        let mut state = state_in(MemoryContextMode::Off);
        let pending_id = MemoryLeaseId::parse("lease-once").unwrap();

        // First install: pending.
        state
            .install_one_shot(mint("sess-1", MemoryContextMode::RecallOnce, "lease-once"))
            .expect("first install succeeds");

        // Repeat install (same id) and a different grant while pending: both fail.
        assert_eq!(
            state.install_one_shot(mint("sess-1", MemoryContextMode::RecallOnce, "lease-once")),
            Err(MemoryContextError::OneShotAlreadyPending {
                lease_id: pending_id.clone()
            })
        );
        assert_eq!(
            state.install_one_shot(mint("sess-1", MemoryContextMode::RecallOnce, "lease-other")),
            Err(MemoryContextError::OneShotAlreadyPending {
                lease_id: pending_id.clone()
            })
        );

        // Consume exactly once.
        let consumed = state.consume_one_shot().expect("first consume succeeds");
        assert_eq!(consumed.lease_id, pending_id);
        for _ in 0..3 {
            assert_eq!(
                state.consume_one_shot(),
                Err(MemoryContextError::OneShotAlreadyConsumed {
                    lease_id: pending_id.clone()
                }),
                "every repeated consume fails closed"
            );
        }

        // Replaying the consumed lease id cannot re-arm the recall.
        assert_eq!(
            state.install_one_shot(mint("sess-1", MemoryContextMode::RecallOnce, "lease-once")),
            Err(MemoryContextError::OneShotAlreadyConsumed {
                lease_id: pending_id.clone()
            })
        );

        // A genuinely new grant (fresh id) installs normally.
        state
            .install_one_shot(mint("sess-1", MemoryContextMode::RecallOnce, "lease-two"))
            .expect("fresh grant installs");

        // Revoke clears the pending recall; consume then finds nothing.
        state.revoke(&sid("sess-1"));
        assert_eq!(
            state.consume_one_shot(),
            Err(MemoryContextError::NoPendingRecall)
        );
    }

    #[test]
    fn consumed_marker_survives_revoke_for_replay_protection() {
        let mut state = state_in(MemoryContextMode::Off);
        state
            .install_one_shot(mint("sess-1", MemoryContextMode::RecallOnce, "lease-once"))
            .unwrap();
        state.consume_one_shot().unwrap();
        state.revoke(&sid("sess-1"));
        // Disable must not re-arm the consumed logical request.
        assert_eq!(
            state.install_one_shot(mint("sess-1", MemoryContextMode::RecallOnce, "lease-once")),
            Err(MemoryContextError::OneShotAlreadyConsumed {
                lease_id: MemoryLeaseId::parse("lease-once").unwrap()
            })
        );
        assert_eq!(
            state.status().one_shot,
            OneShotStatus::Consumed {
                lease_id: MemoryLeaseId::parse("lease-once").unwrap()
            }
        );
    }

    // -- Expiry --------------------------------------------------------------

    #[test]
    fn expired_leases_fail_closed_on_install_and_consume() {
        let granted_at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let expires_at = granted_at + Duration::from_secs(60);
        let after_expiry = expires_at + Duration::from_secs(1);
        let expired_id = MemoryLeaseId::parse("lease-exp").unwrap();

        // Durable install of an already-expired lease.
        let mut state = state_in(MemoryContextMode::Off);
        let lease = mint_at(
            "sess-1",
            MemoryContextMode::CaptureAndRecall,
            "lease-exp",
            granted_at,
            Some(expires_at),
        );
        assert_eq!(
            state.install_at(lease, after_expiry),
            Err(MemoryContextError::LeaseExpired {
                lease_id: expired_id.clone()
            })
        );
        assert_eq!(state.active_mode(), MemoryContextMode::Off);

        // One-shot install of an already-expired lease.
        let lease = mint_at(
            "sess-1",
            MemoryContextMode::RecallOnce,
            "lease-exp",
            granted_at,
            Some(expires_at),
        );
        assert_eq!(
            state.install_one_shot_at(lease, after_expiry),
            Err(MemoryContextError::LeaseExpired {
                lease_id: expired_id.clone()
            })
        );

        // A pending one-shot that expires before its eligible prompt is
        // dropped unused — not marked consumed.
        let lease = mint_at(
            "sess-1",
            MemoryContextMode::RecallOnce,
            "lease-exp",
            granted_at,
            Some(expires_at),
        );
        state
            .install_one_shot_at(lease, granted_at)
            .expect("installs before expiry");
        assert_eq!(
            state.consume_one_shot_at(after_expiry),
            Err(MemoryContextError::LeaseExpired {
                lease_id: expired_id
            })
        );
        assert_eq!(state.status().one_shot, OneShotStatus::Idle);
    }

    #[test]
    fn grant_rejects_expiry_not_after_grant_instant() {
        let granted_at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        for expires_at in [granted_at, granted_at - Duration::from_secs(1)] {
            let result = MemoryContextLease::grant(
                MemoryLeaseId::parse("lease-1").unwrap(),
                sid("sess-1"),
                ProjectId::parse("proj-1").unwrap(),
                ContextProviderId::parse("axel-memory").unwrap(),
                MemoryContextMode::CaptureAndRecall,
                CapturePolicy::default(),
                RecallPolicy::default(),
                UserIntentProof::ExplicitCommand {
                    command_id: RequestId::parse("req-1").unwrap(),
                },
                granted_at,
                Some(expires_at),
            );
            assert_eq!(result, Err(MemoryContextError::InvalidExpiry));
        }
    }

    // -- Boundary identifier parsing ------------------------------------------

    #[test]
    fn boundary_identifiers_fail_closed_on_empty_oversized_and_control_input() {
        assert_eq!(
            SessionId::parse(""),
            Err(MemoryContextError::InvalidIdentifier { field: "session_id" })
        );
        assert_eq!(
            SessionId::parse("   "),
            Err(MemoryContextError::InvalidIdentifier { field: "session_id" })
        );
        assert_eq!(
            ProjectId::parse(&"x".repeat(MEMORY_IDENTIFIER_MAX_BYTES + 1)),
            Err(MemoryContextError::InvalidIdentifier { field: "project_id" })
        );
        assert_eq!(
            MemoryLeaseId::parse("lease\n1"),
            Err(MemoryContextError::InvalidIdentifier { field: "lease_id" })
        );
        // Exactly at the budget is accepted.
        let max = "x".repeat(MEMORY_IDENTIFIER_MAX_BYTES);
        assert_eq!(ProjectId::parse(&max).unwrap().as_str(), max);
    }

    // -- Visibility / authority proof -----------------------------------------

    /// Positive control for the host-private minting gate: crate-internal code
    /// (this module) can grant a lease through `MemoryContextLease::grant`.
    ///
    /// The negative half of the proof is compile-time, via the three
    /// `compile_fail` doctests on [`MemoryContextLease`]: outside the engine
    /// crate (1) struct-literal construction fails (`#[non_exhaustive]`),
    /// (2) `MemoryContextLease::grant` is unnameable (`pub(crate)`), and
    /// (3) no `Deserialize` impl exists, so plugin/model/extension wire input
    /// can never mint a lease. Those doctests run in the same
    /// `cargo test -p synaps-engine memory_context` invocation.
    #[test]
    fn lease_minting_is_gated_through_the_crate_private_grant() {
        let lease = mint("sess-1", MemoryContextMode::CaptureAndRecall, "lease-1");
        assert_eq!(lease.lease_id.as_str(), "lease-1");
        assert_eq!(lease.mode, MemoryContextMode::CaptureAndRecall);
        assert_eq!(
            lease.granted_by,
            UserIntentProof::ExplicitCommand {
                command_id: RequestId::parse("req-1").unwrap(),
            }
        );
        assert_eq!(lease.expires_at, None);
    }

    // -- §19 apply example shape ----------------------------------------------

    #[test]
    fn apply_memory_context_action_matches_spec_section_19_flow() {
        let mut state = SessionMemoryState::new(sid("sess-1"));

        let status = apply_memory_context_action(
            &mut state,
            AuthorizedMemoryAction::Enable {
                lease: mint("sess-1", MemoryContextMode::CaptureAndRecall, "lease-a"),
            },
        )
        .expect("enable succeeds");
        assert!(matches!(
            status.durable,
            DurableStatus::Active {
                mode: MemoryContextMode::CaptureAndRecall,
                ..
            }
        ));

        let status = apply_memory_context_action(
            &mut state,
            AuthorizedMemoryAction::RecallOnce {
                lease: mint("sess-1", MemoryContextMode::RecallOnce, "lease-b"),
            },
        )
        .expect("recall-once succeeds");
        assert!(matches!(status.one_shot, OneShotStatus::Pending { .. }));

        let status = apply_memory_context_action(
            &mut state,
            AuthorizedMemoryAction::Disable {
                session: sid("sess-1"),
            },
        )
        .expect("disable succeeds");
        assert_eq!(status.durable, DurableStatus::Off);
        assert_eq!(status.one_shot, OneShotStatus::Idle);
    }

    // -----------------------------------------------------------------------
    // Task A7 — §10.3 budget policy and §6.5 contribution validation
    // -----------------------------------------------------------------------

    /// Build a synthetic, in-bounds contribution for validator tests.
    pub(crate) fn synthetic_contribution(
        project: &ProjectId,
        rendered: &str,
    ) -> MemoryContextContribution {
        MemoryContextContribution {
            schema: ContributionSchemaVersion::parse("contribution/1").expect("valid schema"),
            provider_id: ContextProviderId::parse("axel-memory").expect("valid provider"),
            project_id: project.clone(),
            records: vec![MemoryContributionRecord {
                memory_id: MemoryId::parse("mem-0001").expect("valid memory id"),
                source: MemorySource::ChatHistory,
                timestamp: SystemTime::UNIX_EPOCH,
                rank_reason: vec![RankReason::ExactTopic, RankReason::Recency],
                sensitivity: Sensitivity::Normal,
                retention: RetentionClass::Standard,
                content: BoundedText::new("session-scoped authorization decision", 2048),
                truncated: false,
                supersedes: None,
            }],
            rendered: BoundedText::new(rendered, MEMORY_MAX_RENDERED_RECORD_BYTES * 8),
            accounting: ContributionAccounting {
                candidates_considered: 1,
                withheld: 0,
                truncated: 0,
            },
        }
    }

    fn pid(raw: &str) -> ProjectId {
        ProjectId::parse(raw).expect("valid project id")
    }

    /// Spec §10.3 exactly: `min(4096, 10% of capacity)` at several window
    /// sizes, including the exact ceiling and floor crossover points.
    #[test]
    fn memory_budget_matches_the_min_4096_ten_percent_formula() {
        // 10% dominates the 4096 ceiling.
        assert_eq!(memory_budget_tokens(200_000), Some(4_096));
        assert_eq!(memory_budget_tokens(1_000_000), Some(4_096));
        // Exact crossover: 10% of 40_960 is exactly 4_096.
        assert_eq!(memory_budget_tokens(40_960), Some(4_096));
        // Below the crossover the 10% share governs.
        assert_eq!(memory_budget_tokens(32_768), Some(3_276));
        assert_eq!(memory_budget_tokens(8_192), Some(819));
        // Exact floor: 10% of 5_120 is exactly the 512 minimum.
        assert_eq!(memory_budget_tokens(5_120), Some(512));
        // Saturating arithmetic never wraps at absurd capacities.
        assert_eq!(memory_budget_tokens(u64::MAX), Some(4_096));
    }

    /// Spec §10.3: below the 512-token minimum, recall is skipped (`None`) —
    /// reserves are never shrunk to make room.
    #[test]
    fn memory_budget_returns_none_below_the_minimum_floor() {
        assert_eq!(memory_budget_tokens(5_110), None); // 511 < 512
        assert_eq!(memory_budget_tokens(4_000), None);
        assert_eq!(memory_budget_tokens(512), None); // 10% of 512 is 51
        assert_eq!(memory_budget_tokens(0), None);
    }

    /// Project isolation (spec §5.2) is a Phase A invariant: a contribution
    /// for a different project fails closed; a matching one is accepted.
    #[test]
    fn validate_contribution_enforces_project_identity() {
        let project = pid("project-a");
        let contribution = synthetic_contribution(&project, "rendered memory text");
        assert_eq!(
            validate_contribution(&contribution, &project, 4_096),
            Ok(())
        );
        assert_eq!(
            validate_contribution(&contribution, &pid("project-b"), 4_096),
            Err(MemoryContextError::ContributionProjectMismatch)
        );
    }

    /// Spec §10.3 bounds fail closed: record count above 8, an oversized
    /// individual record, and rendered text above the ENGINE-provided budget
    /// are each rejected with a content-free error.
    #[test]
    fn validate_contribution_enforces_spec_10_3_bounds() {
        let project = pid("project-a");

        let mut too_many = synthetic_contribution(&project, "rendered");
        let record = too_many.records[0].clone();
        too_many.records = vec![record; MEMORY_MAX_SELECTED_RECORDS + 1];
        assert_eq!(
            validate_contribution(&too_many, &project, 4_096),
            Err(MemoryContextError::ContributionOutOfBounds { field: "records" })
        );

        let mut oversized_record = synthetic_contribution(&project, "rendered");
        oversized_record.records[0].content = BoundedText::new(
            &"x".repeat(MEMORY_MAX_RENDERED_RECORD_BYTES + 1),
            MEMORY_MAX_RENDERED_RECORD_BYTES * 2,
        );
        assert_eq!(
            validate_contribution(&oversized_record, &project, 4_096),
            Err(MemoryContextError::ContributionOutOfBounds {
                field: "record_content"
            })
        );

        // 4_000 ASCII chars ⇒ 1_600 estimated tokens > a 512-token budget.
        let oversized_rendered = synthetic_contribution(&project, &"y".repeat(4_000));
        assert_eq!(
            validate_contribution(&oversized_rendered, &project, 512),
            Err(MemoryContextError::ContributionOutOfBounds { field: "rendered" })
        );
        // The same contribution passes under a budget that covers it.
        assert_eq!(
            validate_contribution(&oversized_rendered, &project, 4_096),
            Ok(())
        );
    }

    /// The typed segment (spec §10.1) exposes exactly the bounded rendered
    /// text for budgeting — the single-variant enum is the explicit
    /// "typed segment, not raw text" boundary.
    #[test]
    fn context_segment_memory_exposes_the_bounded_rendered_text() {
        let contribution = synthetic_contribution(&pid("project-a"), "rendered memory text");
        let segment = ContextSegment::Memory(contribution.clone());
        assert_eq!(segment.rendered_text(), "rendered memory text");
        assert_eq!(segment.rendered_text(), contribution.rendered.text);
    }
}
