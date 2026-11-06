//! Owned sessions: a session whose lifetime is bound to the connection
//! that created it (`Attach::Create{owned}`), so a headless client's
//! session dies with it, as an in-process `synaps rpc` does.
//!
//! The owner leaving for any reason ends the session (`EndReason::
//! OwnerGone`; a running turn is cancelled as `Cancel` cancels it). Its
//! connection's `Detach` is the fast path; the liveness lease
//! ([`super::types::OwnerBind::alive`]) is the guarantee. The one exception
//! is a daemon reload: an owner that leaves while the session is fenced for
//! the exec, or a session the next image restores, waits
//! [`owner_rebind_grace`] for the owner to re-bind with its token, then ends
//! the same way. A restored session's grace starts only once the new image
//! accepts connections (`ArmOwnerRebind`), and it starts no autonomous work
//! before its owner is back. An owned session never parks, idle-ends or
//! pins, and only its owner drives it: everyone else watches.
//!
//! Forwarded secrets live in the runtime's tool env only, kept apart as a
//! [`SecretEnv`] (redacted `Debug`) and merged into a child's env at spawn:
//! never in `SessionConfig` (the journal's env and the reload record come
//! from it).

use std::time::Duration;

use super::actor::SessionActor;
use super::types::{
    ClientId, EndReason, OwnedConfig, OwnerBind, OwnerToken, SecretEnv, SessionCommand, SessionEventWire, ToolEnv,
};

/// `SYNAPS_DAEMON_OWNER_REBIND_SECS`: how long an owned session waits for
/// its owner to re-bind after a daemon reload (also: for the creating
/// connection to bind at all). Default 30 s.
pub fn owner_rebind_grace() -> Duration {
    std::env::var("SYNAPS_DAEMON_OWNER_REBIND_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(30))
}

/// A client other than the owner sent an owned session something that
/// would drive it.
pub const OWNED_INPUT_REFUSAL: &str = "this session is owned by the client that created it; only that client can \
     drive it (others may watch: attach with mirror or observe)";

/// A client other than the owner attached with `takeover`.
pub const OWNED_TAKEOVER_REFUSAL: &str = "this session is owned by the client that created it and cannot be taken \
     over; attach with mirror or observe to watch it";

pub const OWNED_PIN_REFUSAL: &str = "this session is owned by the client that created it and ends when that \
     client disconnects, so it cannot be pinned or kept warm";

/// The actor's ownership state.
pub(crate) struct OwnedState {
    token: OwnerToken,
    /// The bound owner; `None` until it binds, and while it re-binds.
    pub(crate) client: Option<ClientId>,
    /// Armed while unbound: the session ends `OwnerGone` when it passes.
    pub(crate) rebind_deadline: Option<tokio::time::Instant>,
    /// The bound owner's connection lease: cancelled when that connection
    /// task ends, however it ends.
    alive: Option<tokio_util::sync::CancellationToken>,
    /// Restored by a reload and not re-bound yet: autonomous work waits.
    pub(crate) awaiting_owner: bool,
}

impl OwnedState {
    /// Unbound. A fresh create's creator binds by attaching with the token
    /// within the grace (armed now). A restored session waits, unarmed,
    /// for `ArmOwnerRebind`.
    pub(crate) fn new(owned: OwnedConfig) -> Self {
        let rebind_deadline = (!owned.restored).then(|| tokio::time::Instant::now() + owner_rebind_grace());
        Self { token: owned.token, client: None, rebind_deadline, alive: None, awaiting_owner: owned.restored }
    }

    /// For the select loop: the bound owner's lease, if any.
    pub(crate) fn lease(&self) -> Option<tokio_util::sync::CancellationToken> {
        self.alive.clone()
    }
}

/// The rebind deadline; pends forever when there is none.
pub(crate) async fn rebind_timer(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

/// Resolves when the bound owner's connection is gone; pends forever when
/// nobody is bound.
pub(crate) async fn lease_closed(lease: Option<tokio_util::sync::CancellationToken>) {
    match lease {
        Some(t) => t.cancelled().await,
        None => std::future::pending().await,
    }
}

/// The process env as a session env: non-UTF-8 vars cannot be represented
/// there and are skipped (never a panic inside the actor).
fn process_env_utf8() -> super::types::SessionEnv {
    std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))).collect()
}

impl SessionActor {
    /// Whether `client` is this owned session's bound owner.
    pub(crate) fn is_session_owner(&self, client: ClientId) -> bool {
        self.owned.as_ref().is_some_and(|o| o.client == Some(client))
    }

    /// `client` just attached claiming ownership: bind it if the token
    /// matches (a re-bind replaces a stale binding and its lease). A wrong
    /// token, or a session that is not owned, leaves it a plain client; the
    /// daemon then tells the client it is not the owner.
    pub(crate) fn bind_owner(&mut self, client: ClientId, bind: OwnerBind) {
        let Some(owned) = self.owned.as_mut() else {
            tracing::warn!(session = %self.id, "owner claim on a session that is not owned; attached as a plain client");
            return;
        };
        if !owned.token.matches(&bind.claim.token) {
            tracing::warn!(session = %self.id, "owner claim with the wrong token; attached as a plain client");
            return;
        }
        owned.client = Some(client);
        owned.rebind_deadline = None;
        owned.alive = Some(bind.alive);
        owned.awaiting_owner = false;
        tracing::info!(session = %self.id, client = client.0, "owned session: owner bound");
        if let Some(secrets) = bind.claim.forward_secrets {
            self.apply_forwarded_secrets(secrets);
        }
    }

    /// The host's `ArmOwnerRebind`: the daemon accepts connections, so a
    /// restored session still waiting for its owner starts the grace now.
    pub(crate) fn arm_owner_rebind(&mut self) {
        if let Some(owned) = self.owned.as_mut() {
            if owned.client.is_none() && owned.rebind_deadline.is_none() {
                owned.rebind_deadline = Some(tokio::time::Instant::now() + owner_rebind_grace());
                tracing::info!(session = %self.id, "owned session: waiting for the owner to re-bind");
            }
        }
    }

    /// Whether `c` attached to an owned session it does not own: it may
    /// watch, never drive.
    pub(crate) fn watches_owned(&self, c: ClientId) -> bool {
        self.owned.is_some() && !self.is_session_owner(c)
    }

    /// Owned sessions: what a client other than the owner may send. Reads,
    /// its own detach and a save; nothing that drives the session (input,
    /// cancel, prompt answers, settings, commands such as `/close`, taking
    /// input, ending it).
    pub(crate) fn owned_refusal(&self, c: ClientId, cmd: &SessionCommand) -> Option<String> {
        let read_only = matches!(
            cmd,
            SessionCommand::Query { .. } | SessionCommand::Detach { .. } | SessionCommand::Resync { .. } | SessionCommand::Save
        );
        (self.watches_owned(c) && !read_only).then(|| OWNED_INPUT_REFUSAL.to_string())
    }

    /// The tools' env = the session env, plus the forwarded secrets kept
    /// apart (merged into a child's env only at spawn); the forwarded names
    /// drop out of the stripped list (they are not stripped any more).
    /// `self.config` is untouched.
    fn apply_forwarded_secrets(&mut self, secrets: SecretEnv) {
        if !self.runtime.is_live() {
            return;
        }
        let vars = self.config.env.clone().unwrap_or_else(process_env_utf8);
        let stripped: Vec<String> = self
            .config
            .env_stripped
            .iter()
            .filter(|n| !secrets.0.iter().any(|(s, _)| s == *n))
            .cloned()
            .collect();
        let forwarded = secrets.0.len();
        self.runtime.set_tool_env(Some(ToolEnv::with_secrets(vars, secrets)));
        self.runtime.set_env_stripped(stripped);
        tracing::info!(session = %self.id, forwarded, "owned session: forwarded secrets applied to the tool env");
    }

    /// `client` detaches. If it was the owner the session ends, its turn
    /// cancelled as `Cancel` would (the user's interruption), unless a
    /// reload fenced it (the re-bind grace instead).
    pub(crate) fn detach_client(&mut self, client: ClientId) -> Option<EndReason> {
        let owner_left = self.is_session_owner(client);
        self.detach(client);
        if !owner_left {
            return None;
        }
        let reason = self.owner_left()?;
        self.note_cancel_cause(crate::engine::interrupt::InterruptReason::User);
        if self.driver.is_some() {
            self.driver_revoke("session ending");
        }
        Some(reason)
    }

    /// The bound owner's connection lease closed without its `Detach`
    /// arriving (a full queue, a connection task that panicked, an attach
    /// that timed out after it was queued): the same as that detach.
    pub(crate) fn owner_lease_closed(&mut self) -> Option<EndReason> {
        let owned = self.owned.as_mut()?;
        owned.alive = None;
        let client = owned.client?;
        tracing::info!(session = %self.id, client = client.0, "owned session: the owner's connection is gone");
        self.detach_client(client)
    }

    /// The bound owner just detached. During a reload (the session is
    /// fenced for the exec) it gets the re-bind grace; otherwise the
    /// session ends.
    pub(crate) fn owner_left(&mut self) -> Option<EndReason> {
        let fenced = self.reload_fence.is_some();
        let owned = self.owned.as_mut()?;
        owned.client = None;
        owned.alive = None;
        if fenced {
            owned.rebind_deadline = Some(tokio::time::Instant::now() + owner_rebind_grace());
            tracing::info!(session = %self.id, "owned session: owner left for a reload; waiting for it to re-bind");
            self.publish_presence();
            return None;
        }
        tracing::info!(session = %self.id, "owned session: owner gone; ending the session");
        Some(EndReason::OwnerGone)
    }

    /// The re-bind grace passed with no owner. Mid-reload (fenced) the next
    /// image restores the session with a fresh grace; this one waits.
    pub(crate) fn owner_rebind_expired(&mut self) -> Option<EndReason> {
        let fenced = self.reload_fence.is_some();
        let owned = self.owned.as_mut()?;
        if owned.client.is_some() {
            owned.rebind_deadline = None;
            return None;
        }
        if fenced {
            owned.rebind_deadline = Some(tokio::time::Instant::now() + owner_rebind_grace());
            return None;
        }
        owned.rebind_deadline = None;
        tracing::info!(session = %self.id, "owned session: owner did not bind within the grace; ending the session");
        Some(EndReason::OwnerGone)
    }

    /// `KeepWarm{on: true}` on an owned session: refused, to the sender.
    pub(crate) fn refuse_owned_pin(&mut self, from: Option<ClientId>) {
        match from {
            Some(c) => self.emit(SessionEventWire::Refused {
                client: c,
                command: super::actor::command_name(&SessionCommand::KeepWarm { on: true }).to_string(),
                reason: OWNED_PIN_REFUSAL.to_string(),
            }),
            None => self.emit(SessionEventWire::SystemNotice(OWNED_PIN_REFUSAL.to_string())),
        }
    }
}
