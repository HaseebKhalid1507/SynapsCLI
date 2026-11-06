//! Owned sessions: a session whose lifetime is bound to the connection
//! that created it (`Attach::Create{owned}`), so a headless client's
//! session dies with it, as an in-process `synaps rpc` does.
//!
//! The owner leaving for any reason ends the session (`EndReason::
//! OwnerGone`; a running turn is cancelled as `Cancel` cancels it). The one
//! exception is a daemon reload: an owner that leaves while the session is
//! fenced for the exec, or a session the next image restores, waits
//! [`owner_rebind_grace`] for the owner to re-bind with its token, then
//! ends the same way. An owned session never parks, idle-ends or pins.
//!
//! Forwarded secrets live in the runtime's tool env only: never in
//! `SessionConfig` (the journal's env and the reload record come from it).

use std::time::Duration;

use super::actor::SessionActor;
use super::types::{ClientId, EndReason, OwnerClaim, OwnerToken, SecretEnv, SessionCommand, SessionEventWire};

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

pub const OWNED_PIN_REFUSAL: &str = "this session is owned by the client that created it and ends when that \
     client disconnects, so it cannot be pinned or kept warm";

/// The actor's ownership state.
pub(crate) struct OwnedState {
    token: OwnerToken,
    /// The bound owner; `None` until it binds, and while it re-binds.
    pub(crate) client: Option<ClientId>,
    /// Armed while unbound: the session ends `OwnerGone` when it passes.
    pub(crate) rebind_deadline: Option<tokio::time::Instant>,
}

impl OwnedState {
    /// Unbound: the creator (or, after a reload, the returning owner) binds
    /// by attaching with the token within the grace.
    pub(crate) fn new(token: OwnerToken) -> Self {
        Self { token, client: None, rebind_deadline: Some(tokio::time::Instant::now() + owner_rebind_grace()) }
    }
}

/// The rebind deadline; pends forever when there is none.
pub(crate) async fn rebind_timer(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

impl SessionActor {
    /// Whether `client` is this owned session's bound owner.
    pub(crate) fn is_session_owner(&self, client: ClientId) -> bool {
        self.owned.as_ref().is_some_and(|o| o.client == Some(client))
    }

    /// `client` just attached claiming ownership: bind it if the token
    /// matches (a re-bind replaces a stale binding). A wrong token, or a
    /// session that is not owned, leaves it a plain client; the daemon then
    /// tells the client it is not the owner.
    pub(crate) fn bind_owner(&mut self, client: ClientId, claim: OwnerClaim) {
        let Some(owned) = self.owned.as_mut() else {
            tracing::warn!(session = %self.id, "owner claim on a session that is not owned; attached as a plain client");
            return;
        };
        if owned.token != claim.token {
            tracing::warn!(session = %self.id, "owner claim with the wrong token; attached as a plain client");
            return;
        }
        owned.client = Some(client);
        owned.rebind_deadline = None;
        tracing::info!(session = %self.id, client = client.0, "owned session: owner bound");
        if let Some(secrets) = claim.forward_secrets {
            self.apply_forwarded_secrets(&secrets);
        }
    }

    /// The tools' env = the session env plus the forwarded secrets; the
    /// forwarded names drop out of the stripped list (they are not
    /// stripped any more). `self.config` is untouched.
    fn apply_forwarded_secrets(&mut self, secrets: &SecretEnv) {
        if !self.runtime.is_live() {
            return;
        }
        let mut env = self.config.env.clone().unwrap_or_else(|| std::env::vars().collect());
        env.retain(|(k, _)| !secrets.0.iter().any(|(s, _)| s == k));
        env.extend(secrets.0.iter().cloned());
        env.sort_by(|(a, _), (b, _)| a.cmp(b));
        let stripped: Vec<String> = self
            .config
            .env_stripped
            .iter()
            .filter(|n| !secrets.0.iter().any(|(s, _)| s == *n))
            .cloned()
            .collect();
        self.runtime.set_env(Some(env));
        self.runtime.set_env_stripped(stripped);
        tracing::info!(session = %self.id, forwarded = secrets.0.len(), "owned session: forwarded secrets applied to the tool env");
    }

    /// The bound owner just detached. During a reload (the session is
    /// fenced for the exec) it gets the re-bind grace; otherwise the
    /// session ends.
    pub(crate) fn owner_left(&mut self) -> Option<EndReason> {
        let fenced = self.reload_fence.is_some();
        let owned = self.owned.as_mut()?;
        owned.client = None;
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
