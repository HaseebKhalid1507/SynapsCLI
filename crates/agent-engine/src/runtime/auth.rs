use super::types::AuthState;
use crate::auth::{
    broker_from_source, is_expired_with_margin, AccountSelector, CredentialBroker, CredentialRef,
    CredentialSource, OAuthProviderId, PinnedToken, TokenCache, DEFAULT_MARGIN_MS,
};
use crate::{Result, RuntimeError};
use reqwest::Client;
use std::sync::Arc;
use tokio::sync::RwLock;

pub(super) struct AuthMethods;

impl AuthMethods {
    pub(super) async fn recover_local_rejection(
        auth: &Arc<RwLock<AuthState>>,
        client: &Client,
        source: &CredentialSource,
        cache: &TokenCache,
        binding: Option<&str>,
        rejected: &str,
    ) -> Result<()> {
        let credential = turn_pinned_credential(source, binding).ok_or_else(|| {
            RuntimeError::Auth(
                "Rejected OAuth token has no bound local account; run `synaps login`.".into(),
            )
        })?;
        let broker = broker_from_source(source, cache, client.clone());
        Self::recover_credential(auth, source, broker.as_ref(), credential, rejected).await
    }

    async fn recover_credential(
        auth: &Arc<RwLock<AuthState>>,
        source: &CredentialSource,
        broker: &dyn CredentialBroker,
        credential: CredentialRef,
        rejected: &str,
    ) -> Result<()> {
        let mtime = local_mtime_for(source);
        let token = broker.recover_rejected_token(&credential, rejected).await
            .map_err(|e| {
                turn_auth_failed(
                    broker_error_kind(&e),
                    Some(&credential),
                    recovery_error(&credential, &e),
                )
            })?;
        Self::bind_pinned_at(auth, source, PinnedToken { credential, token }, mtime).await;
        Ok(())
    }

    /// `/auth reload` (and the former dev-side 401 entry point), kept as a
    /// thin wrapper over the per-account recovery above — there is no second
    /// refresh path. Drops the in-memory token, then:
    ///
    /// * Remote: invalidate the client `TokenCache` and re-vend.
    /// * Local: recover the bound credential (or, if unbound, the explicitly
    ///   selected one) through `recover_rejected_token`, which forces a
    ///   refresh only while storage still holds the rejected token. Under
    ///   `Auto` with no binding there is no seat to recover: re-select.
    ///
    /// `sent` is the exact token a rejected request carried (`None` = "the
    /// token we hold now"). Local only: if shared state already holds a
    /// *different* non-empty token, a concurrent stream has recovered since
    /// that request was built — reuse it, spend nothing.
    pub(super) async fn reload_after_rejection(
        auth: Arc<RwLock<AuthState>>,
        client: &Client,
        source: &CredentialSource,
        cache: &TokenCache,
        sent: Option<&str>,
    ) -> Result<()> {
        let (rejected, bound) = {
            let mut g = auth.write().await;
            if !source.is_remote() && g.auth_type != "oauth" {
                return Err(RuntimeError::Auth(
                    "Anthropic auth is not OAuth-managed in this session; nothing to reload."
                        .to_string(),
                ));
            }
            if let (Some(sent), false) = (sent, source.is_remote()) {
                if !g.auth_token.is_empty() && g.auth_token != sent {
                    return Ok(());
                }
            }
            g.token_expires = None;
            g.auth_file_mtime = None;
            let held = std::mem::take(&mut g.auth_token);
            (sent.map_or(held, str::to_string), g.bound_credential.clone())
        };
        if source.is_remote() {
            cache.invalidate("anthropic");
            return Self::refresh_if_needed(auth, client, source, cache, None, RefreshPoint::TurnStart)
                .await;
        }
        let broker = broker_from_source(source, cache, client.clone());
        let credential = turn_pinned_credential(source, bound.as_deref()).or_else(|| {
            match broker.account_selector(OAuthProviderId::Anthropic) {
                AccountSelector::Account(account) => {
                    Some(CredentialRef::new(OAuthProviderId::Anthropic, account))
                }
                _ => None,
            }
        });
        match credential {
            Some(credential) if !rejected.is_empty() => {
                tracing::info!("Reloading Anthropic credentials after rejection");
                Self::recover_credential(&auth, source, broker.as_ref(), credential, &rejected)
                    .await
            }
            _ => {
                Self::refresh_with_broker(auth, source, broker.as_ref(), None, RefreshPoint::TurnStart)
                    .await
            }
        }
    }
}

/// True if `model` routes to the Anthropic path (not OpenAI/codex/local). Used
/// to skip the Anthropic pre-stream refresh for non-Anthropic models — they
/// resolve their own provider auth (incl. via the broker), so fetching an
/// Anthropic token first is wasteful and would FAIL on a codex-only Remote
/// broker. (#158 C4/#7)
pub(super) fn model_is_anthropic(model: &str) -> bool {
    crate::runtime::openai::resolve_route(model)
        .is_some_and(|route| route.wire == crate::runtime::openai::WireProtocol::AnthropicMessages)
}

/// The model key handed to the broker's capacity selector for an Anthropic
/// request. Anthropic usage reports model-scoped windows by FAMILY
/// (`seven_day_opus` → `opus`, `seven_day_sonnet` → `sonnet`) and the policy
/// matches a family entry against any `-`/`/`-separated token of the
/// requested id, so the full canonical id (`claude-opus-4-7`) must be passed
/// — never a truncated alias — with only the routing prefix (`anthropic/`)
/// removed. Non-Anthropic ids pass through unchanged.
pub(super) fn quota_model_key(model: &str) -> &str {
    model.strip_prefix("anthropic/").unwrap_or(model)
}

/// Opaque, log-safe fingerprint of a Remote machine principal: a 16-hex
/// prefix of SHA-256 over the machine token, or `anon` when none is
/// configured. Mirrors the recipe behind the core `TokenCache` scope so the
/// runtime binding and the broker cache key discriminate principals
/// identically (pinned by `binding_scope_matches_the_core_cache_scope`).
/// The raw token is never part of any binding, state, or log line.
fn principal_fingerprint(machine_token: &str) -> String {
    use sha2::{Digest, Sha256};
    if machine_token.is_empty() {
        return "anon".to_string();
    }
    let digest = Sha256::digest(machine_token.as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Identity of the credential SOURCE an in-memory Anthropic token came from:
/// `local`, or `remote:<endpoint>#<principal fingerprint>`. Two installations
/// can both hold `anthropic@work`, so the source kind and endpoint are part
/// of it; the machine principal is too, because the same endpoint presented
/// with a different machine token is a different broker identity whose
/// tokens must never be served from the previous principal's fast path.
pub(super) fn binding_scope(source: &CredentialSource) -> String {
    match source {
        CredentialSource::Local => "local".to_string(),
        CredentialSource::Remote {
            endpoint,
            machine_token,
        } => format!(
            "remote:{}#{}",
            endpoint.trim_end_matches('/'),
            principal_fingerprint(machine_token)
        ),
    }
}

/// Identity of the credential an in-memory Anthropic token was vended for:
/// `<binding_scope>|<storage_key>`. The Remote machine token itself is NOT
/// part of it (it is the client's own secret) — only its fingerprint via
/// the scope.
pub(super) fn anthropic_binding(source: &CredentialSource, cred: &CredentialRef) -> String {
    format!("{}|{}", binding_scope(source), cred.storage_key())
}

/// The Anthropic credential a recorded binding names, if — and only if — it
/// was vended by `source` (same kind, endpoint and machine principal). A
/// binding from any other source, a non-Anthropic key, or an unparsable
/// value yields `None`, so callers fall back to re-vending. Storage keys
/// never contain `|` (labels are `[a-z0-9._-]`), so the last separator is
/// the scope/key boundary even if an endpoint contained one.
pub(super) fn turn_pinned_credential(
    source: &CredentialSource,
    bound: Option<&str>,
) -> Option<CredentialRef> {
    let (scope, key) = bound?.rsplit_once('|')?;
    if scope != binding_scope(source) {
        return None;
    }
    CredentialRef::parse_storage_key(key).filter(|cred| cred.provider == OAuthProviderId::Anthropic)
}

/// Result of comparing the selected Anthropic account against the account
/// the in-memory token is bound to.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum BindingCheck {
    /// Same binding: the cached token may be served if still fresh.
    Matches,
    /// Different (or unknown) binding, or a fail-closed selector: the cached
    /// token must not be served.
    Switched,
    /// `Auto` selector at a turn boundary: capacity is re-checked by the
    /// broker; the cached token is never served across turns.
    AutoRepin,
}

/// Where in the request lifecycle the refresh runs. Under `Auto` a new turn
/// re-pins (fresh capacity check); rounds inside a turn keep the seat the
/// turn started on — served from memory while fresh, re-vended for the SAME
/// credential when stale — so one turn does not hop between accounts and
/// lose its prompt cache. The seat is only kept if it was pinned on the
/// current source; a source or principal change mid-turn re-pins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RefreshPoint {
    TurnStart,
    WithinTurn,
}

/// Pure binding comparison used by the fast path.
pub(super) fn check_binding(
    source: &CredentialSource,
    selector: &AccountSelector,
    bound: Option<&str>,
    point: RefreshPoint,
) -> BindingCheck {
    match selector {
        AccountSelector::Auto => match (point, bound) {
            (RefreshPoint::TurnStart, _) | (_, None) => BindingCheck::AutoRepin,
            // Mid-turn: keep the seat this turn pinned — but only a seat
            // vended by THIS source. A binding from another endpoint or
            // machine principal is a switched source, never a kept seat.
            (RefreshPoint::WithinTurn, Some(_)) => {
                if turn_pinned_credential(source, bound).is_some() {
                    BindingCheck::Matches
                } else {
                    BindingCheck::Switched
                }
            }
        },
        AccountSelector::Account(account) => {
            let expected = anthropic_binding(
                source,
                &CredentialRef::new(OAuthProviderId::Anthropic, account.clone()),
            );
            if bound == Some(expected.as_str()) {
                BindingCheck::Matches
            } else {
                BindingCheck::Switched
            }
        }
        // Fail closed: a misconfigured selector never serves any token.
        AccountSelector::Invalid { .. } => BindingCheck::Switched,
    }
}

impl AuthMethods {
    /// Reset `AuthState` so a Remote client never *uses* or *holds* a credential
    /// seeded from a local `auth.json`. Called from `apply_config` when the
    /// source is Remote: clears the access token, drops any refresh token
    /// (invariant 1), and forces the next `refresh_if_needed` to fetch from the
    /// broker. `auth_type` stays "oauth" so the Local early-return is not taken.
    pub(super) fn scrub_for_remote(s: &mut AuthState) {
        s.auth_token.clear();
        s.auth_type = "oauth".to_string();
        s.refresh_token = None;
        s.token_expires = None;
        s.bound_credential = None;
    }

    /// Drop the in-memory Anthropic token because the selected account or
    /// credential source changed (config/source switch). Keeps `auth_type`
    /// so an OAuth runtime refetches through the broker on the next turn;
    /// non-OAuth harnesses (`api_key`) are untouched by callers.
    pub(super) fn scrub_for_account_switch(s: &mut AuthState) {
        s.auth_token.clear();
        s.token_expires = None;
        s.bound_credential = None;
    }

    /// Check if the Anthropic access token is expired — or bound to an
    /// account other than the one now selected — and re-vend it through
    /// the credential broker if needed.
    ///
    /// One path for both sources: `broker_from_source` yields the in-process
    /// `LocalBroker` (single-flight refresh + atomic auth.json persistence
    /// behind the boundary) or the authenticated `RemoteBroker`. In BOTH cases
    /// this layer receives an access token + expiry only and never holds a
    /// refresh token — there is no direct-read fallback.
    ///
    /// Account binding: the broker's effective selector for Anthropic decides
    /// which credential is wanted. An explicit selector (default or named)
    /// serves the cached token only while `AuthState.bound_credential` still
    /// names that credential on this source (kind, endpoint AND machine
    /// principal); a switch (config `auth use`, env override, source or
    /// principal change) scrubs and refetches, never silently keeping the
    /// previous account. `Auto` never serves a cached token across turns:
    /// the broker re-selects at every turn start so a seat whose capacity
    /// was consumed since the last turn is not kept merely because its token
    /// is unexpired; within a turn the pinned seat is kept (see
    /// [`RefreshPoint`]). `model` lets the Auto selector require capacity
    /// for the model actually being requested.
    ///
    /// The binding check is self-sufficient — it does not rely on
    /// `apply_auth_config` having observed the switch — so a token vended
    /// under a different principal can never be served even if the config
    /// path was bypassed or its non-blocking lock attempt was skipped.
    pub(super) async fn refresh_if_needed(
        auth: Arc<RwLock<AuthState>>,
        client: &Client,
        source: &CredentialSource,
        cache: &TokenCache,
        model: Option<&str>,
        point: RefreshPoint,
    ) -> Result<()> {
        {
            let auth_guard = auth.read().await;
            // A non-OAuth local auth mode (e.g. a stubbed api_key harness)
            // never contacts the broker. Remote sources always resolve via
            // the broker regardless of the seeded auth_type.
            if !source.is_remote() && auth_guard.auth_type != "oauth" {
                return Ok(());
            }
        }
        let broker = broker_from_source(source, cache, client.clone());
        Self::refresh_with_broker(auth, source, broker.as_ref(), model, point).await
    }

    /// Broker-injected core of [`Self::refresh_if_needed`]; `source` is the
    /// identity the broker was built for and is what bindings are checked
    /// against. Separate so the selection semantics can be exercised with a
    /// scripted broker (no env, no config file, no network).
    pub(super) async fn refresh_with_broker(
        auth: Arc<RwLock<AuthState>>,
        source: &CredentialSource,
        broker: &dyn CredentialBroker,
        model: Option<&str>,
        point: RefreshPoint,
    ) -> Result<()> {
        let selector = broker.account_selector(OAuthProviderId::Anthropic);
        // Auto mid-turn: a seat this turn pinned that is now cooling down
        // (another session met its limit) is not kept; the request would
        // meet the same limit. Memory only, outside the auth lock.
        let cooling = match (&selector, point) {
            (AccountSelector::Auto, RefreshPoint::WithinTurn) => {
                let bound = auth.read().await.bound_credential.clone();
                turn_pinned_credential(source, bound.as_deref())
                    .is_some_and(|cred| broker.active_cooldown(&cred).is_some())
            }
            _ => false,
        };
        // Fast path: in-memory token still fresh AND still the selected account?
        // Local: also unchanged on disk (`recorded_mtime`, checked below).
        let recorded_mtime = {
            let auth_guard = auth.read().await;
            let binding = check_binding(
                source,
                &selector,
                auth_guard.bound_credential.as_deref(),
                point,
            );
            if let Some(exp) = auth_guard.token_expires {
                let still_fresh = if source.is_remote() {
                    // Must use the SAME predicate as the remote cache
                    // (is_expired_with_margin + DEFAULT_MARGIN_MS) so the
                    // fast-path and TokenCache agree on freshness (board #1).
                    !auth_guard.auth_token.is_empty()
                        && !is_expired_with_margin(exp, DEFAULT_MARGIN_MS)
                } else {
                    !auth_guard.auth_token.is_empty() && crate::epoch_millis() < exp
                };
                if still_fresh && binding == BindingCheck::Matches && !cooling {
                    if source.is_remote() {
                        return Ok(());
                    }
                    // Fresh by expiry and binding; the mtime check decides.
                    Some(auth_guard.auth_file_mtime)
                } else {
                    None
                }
            } else {
                None
            }
        };
        // Read lock dropped here — the stat(2) below never runs under it.
        // Local only: auth.json rewritten externally (`synaps login`, cron,
        // sync, another process's rotation) since this token was vended?
        // One stat per request; a change re-vends the SAME bound account
        // through the broker below (explicit selector: that credential;
        // Auto mid-turn: the pinned seat).
        if let Some(recorded) = recorded_mtime {
            if !auth_file_changed(recorded) {
                return Ok(());
            }
            tracing::info!("auth.json changed on disk; reloading the bound Anthropic account");
        }
        let bound: Option<String> = {
            let auth_guard = auth.read().await;
            let binding = check_binding(
                source,
                &selector,
                auth_guard.bound_credential.as_deref(),
                point,
            );
            match binding {
                BindingCheck::Switched if auth_guard.bound_credential.is_some() => {
                    tracing::info!(
                        "Anthropic account selection changed; discarding the previous account's token"
                    );
                }
                BindingCheck::AutoRepin => {
                    tracing::debug!("Anthropic auto account selection: re-pinning for this turn");
                }
                _ => {}
            }
            auth_guard.bound_credential.clone()
        };

        tracing::info!("Refreshing auth token via credential broker");
        let mtime = local_mtime_for(source);
        let pinned: PinnedToken = match &selector {
            AccountSelector::Account(account) => {
                // Explicit: exactly this credential, never a fallback.
                let cred = CredentialRef::new(OAuthProviderId::Anthropic, account.clone());
                let token = broker.access_token_for(&cred).await.map_err(|e| {
                    turn_auth_failed(broker_error_kind(&e), Some(&cred), vend_error(&cred, &e))
                })?;
                PinnedToken {
                    credential: cred,
                    token,
                }
            }
            AccountSelector::Auto => {
                Self::vend_auto(broker, source, bound.as_deref(), model, point).await?
            }
            AccountSelector::Invalid { source, reason } => {
                return Err(turn_auth_failed("invalid_selector", None, RuntimeError::Auth(format!(
                    "Anthropic account selector from {source} is invalid ({reason}); refusing to fall back to another account. Fix it or run `synaps auth use --provider anthropic --account <label|default>`."
                ))));
            }
        };
        // Update shared auth state so all clones (including spawned stream
        // tasks) immediately see the fresh token. Never a refresh token.
        Self::bind_pinned_at(&auth, source, pinned, mtime).await;

        Ok(())
    }

    /// Bind the in-memory Anthropic token to `pinned` — the seat the broker
    /// just vended for THIS source — exactly as [`Self::refresh_with_broker`]
    /// does after a vend. This is the ONE sanctioned mid-turn re-pin: the
    /// request loop calls it after a gated account failover (recognized
    /// window exhaustion under `Auto`, nothing streamed yet), so the rest of
    /// the turn keeps the new seat through the normal `WithinTurn` fast path
    /// and the next turn boundary re-selects as usual. Never a refresh token.
    ///
    /// The auth.json mtime is sampled here, after the caller's vend; a write
    /// racing that vend is caught by 401 recovery instead.
    pub(super) async fn bind_pinned(
        auth: &Arc<RwLock<AuthState>>,
        source: &CredentialSource,
        pinned: PinnedToken,
    ) {
        Self::bind_pinned_at(auth, source, pinned, local_mtime_for(source)).await
    }

    /// [`Self::bind_pinned`] with the auth.json mtime the caller sampled
    /// BEFORE its vend (so a write racing the vend causes one extra cheap
    /// reload, never a missed one). Remote sources record `None`.
    async fn bind_pinned_at(
        auth: &Arc<RwLock<AuthState>>,
        source: &CredentialSource,
        pinned: PinnedToken,
        mtime: Option<std::time::SystemTime>,
    ) {
        let binding = anthropic_binding(source, &pinned.credential);
        let mut auth_guard = auth.write().await;
        auth_guard.auth_token = pinned.token.token;
        auth_guard.auth_type = "oauth".to_string();
        auth_guard.refresh_token = None;
        auth_guard.token_expires = Some(pinned.token.expires);
        auth_guard.auth_file_mtime = mtime;
        auth_guard.bound_credential = Some(binding);
    }

    /// `Auto` vend. At a turn boundary (or with no seat pinned on this
    /// source) the broker's capacity selector picks the seat. Mid-turn, the
    /// seat this turn already pinned is re-vended by explicit credential —
    /// a stale token is a refresh event, not a re-selection event — so a
    /// long agentic turn keeps one account (and its prompt cache) instead
    /// of hopping to whichever seat now has the lowest utilization, and no
    /// per-round usage fetches are spent. Only if THAT seat can no longer be
    /// vended, or is cooling down (another session met its limit), does
    /// Auto fall back to re-selection (Auto opted in to switching;
    /// the fallback is logged, never silent).
    async fn vend_auto(
        broker: &dyn CredentialBroker,
        source: &CredentialSource,
        bound: Option<&str>,
        model: Option<&str>,
        point: RefreshPoint,
    ) -> Result<PinnedToken> {
        if point == RefreshPoint::WithinTurn {
            if let Some(cred) = turn_pinned_credential(source, bound) {
                // A pinned seat that is cooling down is not re-vended: the
                // selection below never picks it while the cooldown lasts,
                // and with no other account it says until when.
                if let Some(until_ms) = broker.active_cooldown(&cred) {
                    tracing::info!(
                        credential = %cred,
                        until_ms,
                        "Anthropic auto selection: the seat pinned for this turn is cooling down; re-selecting"
                    );
                } else {
                    match broker.access_token_for(&cred).await {
                        Ok(token) => {
                            return Ok(PinnedToken {
                                credential: cred,
                                token,
                            })
                        }
                        Err(e) => tracing::warn!(
                            credential = %cred,
                            error = %e,
                            "Anthropic auto selection: the seat pinned for this turn could not be re-vended; re-selecting"
                        ),
                    }
                }
            }
        }
        broker
            .access_token_pinned_for(OAuthProviderId::Anthropic, model.map(quota_model_key))
            .await
            .map_err(|e| {
                turn_auth_failed(
                    auto_selection_kind(&e),
                    None,
                    RuntimeError::Auth(auto_selection_text(&e, crate::epoch_millis())),
                )
            })
    }
}

/// Fixed kind of a failed automatic selection. In automatic Anthropic
/// selection a `Credential` error comes only from listing the stored
/// accounts, so it is a credential-store failure.
fn auto_selection_kind(e: &crate::auth::BrokerError) -> &'static str {
    match e {
        crate::auth::BrokerError::Credential(_) => "credential_store",
        other => broker_error_kind(other),
    }
}

/// Log ONE WARN `turn_auth_failed` with a fixed kind (and the account
/// label when one is known; never a token or identity) for an Anthropic
/// auth error that ends the turn, and return it unchanged.
fn turn_auth_failed(
    kind: &str,
    account: Option<&CredentialRef>,
    err: RuntimeError,
) -> RuntimeError {
    tracing::warn!(
        provider = %"anthropic",
        kind = %kind,
        account = %account.map_or("-", |c| c.account.label_str()),
        "turn_auth_failed"
    );
    err
}

/// Fixed kind of a broker error for the `turn_auth_failed` WARN.
fn broker_error_kind(e: &crate::auth::BrokerError) -> &'static str {
    use crate::auth::{BrokerError, RefreshFailureKind as R};
    match e {
        BrokerError::NoAccountAvailable {
            failure: Some(f), ..
        } => f.kind.as_str(),
        BrokerError::NoAccountAvailable { .. } => "no_account_available",
        BrokerError::Refresh(f) => match f.kind {
            R::Transient => "refresh_transient",
            R::LockBusy => "refresh_lock_busy",
            R::Other => "refresh_other",
            R::Cooldown { .. } => "refresh_cooldown",
            R::Credential => "refresh_rejected",
            R::ActionRequired => "refresh_action_required",
        },
        BrokerError::NotConfigured(_) => "no_credentials",
        BrokerError::UnknownAccount { .. } => "unknown_account",
        BrokerError::InvalidAccount(_) => "invalid_account",
        BrokerError::Credential(_) => "credential_error",
        BrokerError::Unauthorized => "broker_unauthorized",
        BrokerError::Transport(_) => "broker_transport",
        _ => "broker_error",
    }
}

/// User text for a failed automatic selection. Every clause is something
/// the broker reported; login advice appears only for accounts whose
/// failure a login fixes.
fn auto_selection_text(e: &crate::auth::BrokerError, now_ms: u64) -> String {
    use crate::auth::BrokerError;
    match e {
        BrokerError::NoAccountAvailable {
            failure: Some(f), ..
        } => selection_failure_text(f, now_ms),
        BrokerError::Refresh(f) => chosen_seat_refresh_text(f),
        // The broker itself is unreachable, or refused this client's
        // machine auth (its text says what to set): a login fixes neither.
        BrokerError::Transport(_) => format!(
            "Automatic account selection failed: {e}. Check auth.remote_endpoint / broker \
             reachability, then send again; no login needed."
        ),
        BrokerError::Unauthorized => {
            format!("Automatic account selection failed: {}", sentence(&e.to_string()))
        }
        // Listing the stored accounts failed: no account was read, and
        // neither a login nor a retry fixes the store.
        BrokerError::Credential(msg) => format!(
            "Automatic account selection failed (credential_store): the credential store could \
             not be read ({msg}); repair or restore it."
        ),
        other => format!(
            "Automatic account selection failed: {other}. Run `synaps login`, pick an account with `synaps auth use --provider anthropic --account <label>`, or check auth.remote_endpoint / broker reachability."
        ),
    }
}

/// `synaps login` for exactly one Anthropic slot.
fn login_command(label: &str) -> String {
    if label == "default" {
        "`synaps login --provider anthropic`".to_string()
    } else {
        format!("`synaps login --provider anthropic --account {label}`")
    }
}

/// `label: kind (HTTP s, retry after Ns, until HH:MM UTC)`. A cooling
/// account whose kind is its last failed read or refresh says "cooling down
/// until" (or just "cooling down" when the end is unknown).
fn account_failure_part(a: &crate::auth::AccountFailure, now_ms: u64) -> String {
    let mut extra = Vec::new();
    if let Some(status) = a.http_status {
        extra.push(format!("HTTP {status}"));
    }
    if let Some(secs) = a.retry_after_s {
        extra.push(format!("retry after {secs}s"));
    }
    let cooling = a.cooling_down && a.kind != crate::auth::AccountFailureKind::CoolingDown;
    match a.until_ms.filter(|t| *t > now_ms) {
        Some(until) => extra.push(format!(
            "{}until {}",
            if cooling { "cooling down " } else { "" },
            super::anthropic_quota::describe_reset_utc(until, now_ms)
        )),
        None if cooling => extra.push("cooling down".to_string()),
        None => {}
    }
    if extra.is_empty() {
        format!("{}: {}", a.account, a.kind)
    } else {
        format!("{}: {} ({})", a.account, a.kind, extra.join(", "))
    }
}

fn selection_failure_text(f: &crate::auth::SelectionFailure, now_ms: u64) -> String {
    use crate::auth::SelectionFailureKind as K;
    let what = match f.kind {
        K::NoCredentials => "no Anthropic account is logged in",
        K::UsageReadFailed => "reading account usage failed, so capacity is unproven (not used up)",
        K::CredentialRejected => {
            "every Anthropic account that is not cooling down had its login refused"
        }
        K::AllCoolingDown => "every Anthropic account is cooling down or at a usage limit",
        K::QuotaUnknown => "no account's usage reading proved capacity",
        K::SeatChurn => {
            "the chosen account's stored login kept changing while its capacity was being read"
        }
        K::RemoteNoCapacity => "the credential broker found no account with proven capacity",
        K::ActionRequired => {
            "every Anthropic account that is not cooling down needs a new login or an action \
             outside synaps"
        }
        K::CredentialStore => "the credential store could not be read, so no account was checked",
        K::Unknown => "the daemon reported a reason this synaps build does not know",
    };
    let mut text = format!("Automatic account selection failed ({}): {what}", f.kind);
    if !f.accounts.is_empty() {
        let parts: Vec<_> = f
            .accounts
            .iter()
            .map(|a| account_failure_part(a, now_ms))
            .collect();
        text.push_str(&format!(" [{}]", parts.join("; ")));
    }
    let earliest = f
        .earliest_reset_ms
        .filter(|t| *t > now_ms)
        .map(|t| super::anthropic_quota::describe_reset_utc(t, now_ms));
    if let Some(at) = &earliest {
        text.push_str(&format!("; earliest reset {at}"));
    }
    text.push('.');
    if f.kind == K::NoCredentials {
        text.push_str(
            " Run `synaps login --provider anthropic` (add `--account <label>` for a named slot).",
        );
        return text;
    }
    let login = f.login_labels();
    if !login.is_empty() {
        let commands: Vec<_> = login.iter().map(|l| login_command(l)).collect();
        text.push_str(&format!(
            " {} need{} a new login: {}.",
            login.join(", "),
            if login.len() == 1 { "s" } else { "" },
            commands.join(", ")
        ));
    }
    for a in f.accounts.iter().filter(|a| a.kind.needs_action()) {
        text.push(' ');
        text.push_str(&action_remedy(a));
    }
    // "No login needed" only when no account needs a login or an action;
    // "send again" only when sending again can help.
    let nothing_to_fix = login.is_empty() && f.action_labels().is_empty();
    match f.kind {
        K::NoCredentials | K::CredentialRejected | K::ActionRequired | K::CredentialStore => {}
        K::RemoteNoCapacity => text.push_str(
            " The broker host's log has each account's reason (`auth_selection_failed`); send again to retry.",
        ),
        K::AllCoolingDown => match (&earliest, nothing_to_fix) {
            (Some(at), true) => text.push_str(&format!(" No login needed; send again after {at}.")),
            (None, true) => text.push_str(" No login needed; send again later."),
            (Some(at), false) => text.push_str(&format!(" Send again after {at}.")),
            (None, false) => {}
        },
        // Failed usage reads are held: the next selection reads usage again
        // only after the hold ends, so name that time instead of "retry".
        K::UsageReadFailed if f.usage_read_retry_ms.is_some_and(|t| t > now_ms) => {
            let at = super::anthropic_quota::describe_reset_utc(
                f.usage_read_retry_ms.unwrap_or(now_ms),
                now_ms,
            );
            text.push_str(&format!(" Usage is read again after {at}; send again then."));
            if nothing_to_fix {
                text.push_str(" No login needed.");
            }
        }
        // No hold recorded, but every live transient failure asked for a
        // wait: sending again sooner only repeats them. (A hold that has
        // already ended means its Retry-After has too.)
        K::UsageReadFailed
            if f.usage_read_retry_ms.is_none() && f.transient_retry_after_s().is_some() =>
        {
            let secs = f.transient_retry_after_s().unwrap_or_default();
            text.push_str(&format!(" Send again in {secs}s or later"));
            text.push_str(if nothing_to_fix { "; no login needed." } else { "." });
        }
        _ if nothing_to_fix => text.push_str(" Send again to retry; no login needed."),
        _ => text.push_str(" Send again to retry."),
    }
    text
}

/// The remedy for an account whose failure needs an action outside synaps.
/// The `usage read needs action` WARN in the synaps log has the detail.
fn action_remedy(a: &crate::auth::AccountFailure) -> String {
    use crate::auth::AccountFailureKind as K;
    match a.kind {
        K::CredentialStore => format!(
            "{}: its stored login could not be read; repair or restore the credential store \
             (the `usage read needs action` line in the synaps log names the file).",
            a.account
        ),
        _ => format!(
            "{}: the token refresh needs an action outside synaps; the `usage read needs \
             action` line in the synaps log says what to do.",
            a.account
        ),
    }
}

/// The account the broker chose could not be vended because its token
/// refresh failed. Only a refused login gets login advice.
fn chosen_seat_refresh_text(f: &crate::auth::RefreshFailure) -> String {
    use crate::auth::RefreshFailureKind as R;
    let msg = sentence(f.message());
    match f.kind {
        R::Transient => format!(
            "Token refresh failed for the automatically chosen Anthropic account (network or \
             provider error, not a login problem): {msg} Send again to retry; no login needed."
        ),
        R::LockBusy => format!(
            "Another synaps process is still refreshing the automatically chosen Anthropic \
             account: {msg} Send again to retry; no login needed."
        ),
        R::Other => format!(
            "Token refresh failed for the automatically chosen Anthropic account: {msg} Send \
             again to retry."
        ),
        R::Cooldown { retry_in_secs } => format!(
            "Token refresh of the automatically chosen Anthropic account is paused after a \
             rejected token: {msg} Send again in {retry_in_secs}s."
        ),
        R::ActionRequired => {
            format!("Can't refresh the token for the automatically chosen Anthropic account: {msg}")
        }
        // A broker that ends the selection on the chosen seat's refused
        // refresh (an older or remote one) names the account and, for a
        // local slot, the exact login command.
        R::Credential if has_login_advice(f.message()) => {
            format!("Anthropic refused the login of the automatically chosen account: {msg}")
        }
        R::Credential => format!(
            "Anthropic refused the login of the automatically chosen account: {msg} Find it with \
             `synaps auth list` and log it in again with `synaps login --provider anthropic \
             --account <label>`."
        ),
    }
}
/// A failed explicit-account vend.
fn vend_error(credential: &CredentialRef, e: &crate::auth::BrokerError) -> RuntimeError {
    if let Some(text) = action_required(credential, e) {
        return RuntimeError::Auth(text);
    }
    let reason = e.to_string();
    RuntimeError::Auth(if has_login_advice(&reason) {
        format!(
            "Token refresh failed for account '{}': {} Or check auth.remote_endpoint / broker reachability.",
            credential.account,
            sentence(&reason)
        )
    } else {
        format!(
            "Token refresh failed for account '{}': {}. Run `synaps login` (or `synaps login --account {}`) to re-authenticate, or check auth.remote_endpoint / broker reachability.",
            credential.account, reason, credential.account
        )
    })
}

/// A failure only the user can fix outside synaps (a corrupt credential
/// store, a provider billing requirement): show its own instructions, with
/// no login advice (login fails the same way) and no "retry".
fn action_required(credential: &CredentialRef, e: &crate::auth::BrokerError) -> Option<String> {
    match e {
        crate::auth::BrokerError::Refresh(f)
            if f.kind == crate::auth::RefreshFailureKind::ActionRequired =>
        {
            Some(format!(
                "Can't refresh the token for '{}': {}",
                credential.account,
                sentence(f.message())
            ))
        }
        _ => None,
    }
}

/// The reason already says which `synaps login` to run (core texts for a
/// missing or removed slot, provider labels such as Kimi's or Muse's).
fn has_login_advice(reason: &str) -> bool {
    reason.contains("synaps login")
}

/// `text` as a sentence: trimmed, ending in exactly one period.
fn sentence(text: &str) -> String {
    let text = text.trim_end();
    if text.ends_with('.') {
        text.to_string()
    } else {
        format!("{text}.")
    }
}

/// Terminal auth error for a failed 401 recovery that says WHY. Only an
/// unusable login tells the user to run `synaps login`; a cooldown, a busy
/// refresh lock or a network/provider failure says so instead. Safe to
/// show: broker errors never echo token values or raw upstream bodies —
/// Anthropic refresh failures go through `sanitize_oauth_error_body`,
/// remote broker errors omit the broker's body, and the rest are our own
/// fixed texts (cooldown, lock timeouts, missing slot with its path).
fn recovery_error(credential: &CredentialRef, e: &crate::auth::BrokerError) -> RuntimeError {
    use crate::auth::{BrokerError, RefreshFailureKind};
    if let Some(text) = action_required(credential, e) {
        return RuntimeError::Auth(text);
    }
    let text = match e {
        BrokerError::Refresh(f) => match f.kind {
            RefreshFailureKind::Cooldown { .. } => format!(
                "Anthropic rejected the token for '{}' again right after it was refreshed: {}. \
                 If this persists, check the account's access.",
                credential.account,
                f.message()
            ),
            RefreshFailureKind::LockBusy => format!(
                "Another synaps process is still refreshing '{}' ({}); retry shortly.",
                credential.account,
                f.message()
            ),
            RefreshFailureKind::Transient => format!(
                "Token refresh failed for '{}' (network or provider error, not a login \
                 problem): {}. Retry shortly.",
                credential.account,
                f.message()
            ),
            RefreshFailureKind::Other => format!(
                "Token refresh failed for '{}': {}. Retry shortly.",
                credential.account,
                f.message()
            ),
            // Returned above by `action_required`; kept total for the match.
            RefreshFailureKind::ActionRequired => sentence(f.message()),
            RefreshFailureKind::Credential if has_login_advice(f.message()) => format!(
                "OAuth recovery failed for '{}': {}",
                credential.account,
                sentence(f.message())
            ),
            RefreshFailureKind::Credential => format!(
                "OAuth recovery failed for '{}': {}. Run `synaps login --account {}` to \
                 re-authenticate.",
                credential.account,
                f.message(),
                credential.account
            ),
        },
        other if has_login_advice(&other.to_string()) => format!(
            "OAuth recovery failed for '{}': {} Or check auth.remote_endpoint / broker \
             reachability.",
            credential.account,
            sentence(&other.to_string())
        ),
        other => format!(
            "OAuth recovery failed for '{}': {}. Run `synaps login --account {}` to \
             re-authenticate, or check auth.remote_endpoint / broker reachability.",
            credential.account, other, credential.account
        ),
    };
    RuntimeError::Auth(text)
}

/// Current mtime of the local auth.json, if it can be stat'ed.
fn auth_file_mtime_now() -> Option<std::time::SystemTime> {
    std::fs::metadata(crate::auth::auth_file_path())
        .and_then(|m| m.modified())
        .ok()
}

/// Local source: stat auth.json BEFORE vending, so a write racing the vend
/// can only cause one extra (cheap) reload, never a missed one.
fn local_mtime_for(source: &CredentialSource) -> Option<std::time::SystemTime> {
    if source.is_remote() {
        None
    } else {
        auth_file_mtime_now()
    }
}

/// True when we recorded an mtime at vend time and the file's current mtime
/// differs. An unreadable/missing file does NOT invalidate a working token.
fn auth_file_changed(recorded: Option<std::time::SystemTime>) -> bool {
    // `recorded.and_then` short-circuits: no stat when nothing was recorded.
    match (recorded, recorded.and_then(|_| auth_file_mtime_now())) {
        (Some(then), Some(now)) => then != now,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::TokenCache;

    /// Tiny in-test broker that always returns `token_json` at GET /token.
    async fn spawn_broker(token_json: &'static str) -> String {
        use axum::{routing::get, Router};
        let app = Router::new().route("/token", get(move || async move { token_json.to_string() }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    fn empty_auth() -> Arc<RwLock<AuthState>> {
        Arc::new(RwLock::new(AuthState {
            auth_token: String::new(),
            auth_type: "none".into(),
            refresh_token: Some("SHOULD-BE-CLEARED".into()),
            token_expires: None,
            auth_file_mtime: None,
            bound_credential: None,
        }))
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn remote_source_fetches_and_populates_auth_state() {
        // The remote broker reads the account selector from config; keep the
        // developer's own `auth.account.anthropic` out of this test.
        let _config = crate::test_env::BaseDirGuard::new();
        let url = spawn_broker(r#"{"access_token":"sk-broker-xyz","expires":9999999999999}"#).await;
        let source = CredentialSource::Remote {
            endpoint: url,
            machine_token: "m".into(),
        };
        let cache = TokenCache::new();
        let auth = empty_auth();
        AuthMethods::refresh_if_needed(
            Arc::clone(&auth),
            &Client::new(),
            &source,
            &cache,
            None,
            RefreshPoint::TurnStart,
        )
        .await
        .unwrap();
        let g = auth.read().await;
        assert_eq!(g.auth_token, "sk-broker-xyz");
        assert_eq!(g.auth_type, "oauth");
        assert_eq!(
            g.refresh_token, None,
            "Remote must clear any refresh token (invariant)"
        );
        assert_eq!(g.token_expires, Some(9_999_999_999_999));
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn remote_source_fast_path_when_token_still_fresh() {
        // The remote broker reads the account selector from config; keep the
        // developer's own `auth.account.anthropic` out of this test.
        let _config = crate::test_env::BaseDirGuard::new();
        let url = spawn_broker(r#"{"access_token":"sk-1","expires":9999999999999}"#).await;
        let source = CredentialSource::Remote {
            endpoint: url,
            machine_token: "m".into(),
        };
        let cache = TokenCache::new();
        let auth = empty_auth();
        // First call fetches + sets a far-future expiry.
        AuthMethods::refresh_if_needed(
            Arc::clone(&auth),
            &Client::new(),
            &source,
            &cache,
            None,
            RefreshPoint::TurnStart,
        )
        .await
        .unwrap();
        // Second call: in-memory token is fresh -> fast path returns without refetch.
        AuthMethods::refresh_if_needed(
            Arc::clone(&auth),
            &Client::new(),
            &source,
            &cache,
            None,
            RefreshPoint::TurnStart,
        )
        .await
        .unwrap();
        assert_eq!(auth.read().await.auth_token, "sk-1");
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn remote_fast_path_refetches_a_token_inside_the_margin() {
        // The remote broker reads the account selector from config; keep the
        // developer's own `auth.account.anthropic` out of this test.
        let _config = crate::test_env::BaseDirGuard::new();
        // AuthState holds a token that is valid but within the 5-min refetch
        // margin. The fast path must NOT serve it — it must refetch (board #1).
        let url = spawn_broker(r#"{"access_token":"sk-FRESH","expires":9999999999999}"#).await;
        let source = CredentialSource::Remote {
            endpoint: url,
            machine_token: "m".into(),
        };
        let cache = TokenCache::new();
        let near = crate::epoch_millis() + 2 * 60 * 1000; // 2 min — inside the 5-min margin
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "sk-STALE".into(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: Some(near),
            auth_file_mtime: None,
            bound_credential: None,
        }));
        AuthMethods::refresh_if_needed(
            Arc::clone(&auth),
            &Client::new(),
            &source,
            &cache,
            None,
            RefreshPoint::TurnStart,
        )
        .await
        .unwrap();
        assert_eq!(
            auth.read().await.auth_token,
            "sk-FRESH",
            "a token inside the refetch margin must be refetched, not served stale"
        );
    }

    #[tokio::test]
    async fn local_source_api_key_is_noop_never_contacts_broker() {
        // Local + non-oauth auth_type hits the original early-return: no broker,
        // no error, state untouched. (Local path byte-for-byte unchanged.)
        let source = CredentialSource::Local;
        let cache = TokenCache::new();
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "key".into(),
            auth_type: "api_key".into(),
            refresh_token: None,
            token_expires: None,
            auth_file_mtime: None,
            bound_credential: None,
        }));
        AuthMethods::refresh_if_needed(
            Arc::clone(&auth),
            &Client::new(),
            &source,
            &cache,
            None,
            RefreshPoint::TurnStart,
        )
        .await
        .unwrap();
        let g = auth.read().await;
        assert_eq!(g.auth_token, "key");
        assert_eq!(g.auth_type, "api_key");
    }

    /// Explicit selector, token fresh, but bound to a DIFFERENT account on
    /// the same broker: the fast path must not serve it — refetch for the
    /// selected account (config/source switch never keeps the old account).
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn switched_account_binding_is_never_served_from_the_fast_path() {
        // The remote broker reads the account selector from config; keep the
        // developer's own `auth.account.anthropic` out of this test.
        let _config = crate::test_env::BaseDirGuard::new();
        let url = spawn_broker(r#"{"access_token":"sk-SELECTED","expires":9999999999999}"#).await;
        let source = CredentialSource::Remote {
            endpoint: url.clone(),
            machine_token: "m".into(),
        };
        let cache = TokenCache::new();
        let old_cred = CredentialRef::new(
            OAuthProviderId::Anthropic,
            crate::auth::Account::parse("old").unwrap(),
        );
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "sk-OLD-ACCOUNT".into(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: Some(9_999_999_999_999),
            auth_file_mtime: None,
            bound_credential: Some(anthropic_binding(&source, &old_cred)),
        }));
        AuthMethods::refresh_if_needed(
            Arc::clone(&auth),
            &Client::new(),
            &source,
            &cache,
            None,
            RefreshPoint::TurnStart,
        )
        .await
        .unwrap();
        let g = auth.read().await;
        assert_eq!(g.auth_token, "sk-SELECTED");
        assert_eq!(
            g.bound_credential.as_deref(),
            Some(
                anthropic_binding(
                    &source,
                    &CredentialRef::default_for(OAuthProviderId::Anthropic)
                )
                .as_str()
            ),
            "binding must name the credential actually vended"
        );
    }

    /// Same broker, same account, fresh token: served without a refetch.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn matching_binding_serves_cached_token() {
        // The remote broker reads the account selector from config; keep the
        // developer's own `auth.account.anthropic` out of this test.
        let _config = crate::test_env::BaseDirGuard::new();
        let url = spawn_broker(r#"{"access_token":"sk-REFETCHED","expires":9999999999999}"#).await;
        let source = CredentialSource::Remote {
            endpoint: url.clone(),
            machine_token: "m".into(),
        };
        let cache = TokenCache::new();
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "sk-CACHED".into(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: Some(9_999_999_999_999),
            auth_file_mtime: None,
            bound_credential: Some(anthropic_binding(
                &source,
                &CredentialRef::default_for(OAuthProviderId::Anthropic),
            )),
        }));
        AuthMethods::refresh_if_needed(
            Arc::clone(&auth),
            &Client::new(),
            &source,
            &cache,
            None,
            RefreshPoint::TurnStart,
        )
        .await
        .unwrap();
        assert_eq!(auth.read().await.auth_token, "sk-CACHED");
    }

    /// A legacy state with no binding (e.g. seeded from auth.json before the
    /// account-aware refresh existed) is treated as unknown: refetch once, then
    /// bound.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn unbound_fresh_token_is_rebound_through_the_broker() {
        // The remote broker reads the account selector from config; keep the
        // developer's own `auth.account.anthropic` out of this test.
        let _config = crate::test_env::BaseDirGuard::new();
        let url = spawn_broker(r#"{"access_token":"sk-BOUND","expires":9999999999999}"#).await;
        let source = CredentialSource::Remote {
            endpoint: url.clone(),
            machine_token: "m".into(),
        };
        let cache = TokenCache::new();
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "sk-LEGACY".into(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: Some(9_999_999_999_999),
            auth_file_mtime: None,
            bound_credential: None,
        }));
        AuthMethods::refresh_if_needed(
            Arc::clone(&auth),
            &Client::new(),
            &source,
            &cache,
            None,
            RefreshPoint::TurnStart,
        )
        .await
        .unwrap();
        let g = auth.read().await;
        assert_eq!(g.auth_token, "sk-BOUND");
        assert!(g.bound_credential.is_some());
    }

    /// Pins the runtime↔policy contract for Claude family scopes: the key the
    /// runtime hands to the selector must let an `opus`/`sonnet` model-scoped
    /// window (as the Anthropic usage adapter reports them) apply to the
    /// matching family and ONLY that family.
    #[test]
    fn quota_model_key_matches_claude_family_scopes_not_exact_only() {
        use crate::auth::quota_policy::{model_matches, WindowLimit};
        let opus_window = WindowLimit {
            id: "seven_day_opus".into(),
            duration_ms: Some(7 * 24 * 3_600_000),
            used_percent: Some(100.0),
            limit_reached: None,
            resets_at_ms: None,
            models: Some(vec!["opus".into()]),
        };
        for id in [
            "claude-opus-4-7",
            "anthropic/claude-opus-4-7",
            "claude-3-opus-20240229",
        ] {
            let key = quota_model_key(id);
            assert!(!key.contains('/'), "routing prefix stripped: {key}");
            assert!(model_matches("opus", key), "{id}");
            assert!(opus_window.applies_to(Some(key)), "{id}");
            assert!(!model_matches("sonnet", key), "{id} is not sonnet");
        }
        for id in ["claude-sonnet-4-6", "anthropic/claude-sonnet-4-6"] {
            let key = quota_model_key(id);
            assert!(model_matches("sonnet", key), "{id}");
            assert!(
                !opus_window.applies_to(Some(key)),
                "{id} must not hit the opus window"
            );
        }
        // Codex slugs are exact and untouched.
        assert_eq!(quota_model_key("gpt-5.6-sol"), "gpt-5.6-sol");
        assert!(model_matches("gpt-5.6-sol", quota_model_key("gpt-5.6-sol")));
        assert!(!model_matches(
            "gpt-5.6-luna",
            quota_model_key("gpt-5.6-sol")
        ));
    }

    #[test]
    fn binding_never_contains_the_machine_token_and_distinguishes_sources() {
        let cred = CredentialRef::new(
            OAuthProviderId::Anthropic,
            crate::auth::Account::parse("work").unwrap(),
        );
        let remote = CredentialSource::Remote {
            endpoint: "https://broker.example:8181".into(),
            machine_token: "MACHINE-SECRET".into(),
        };
        let b_remote = anthropic_binding(&remote, &cred);
        assert!(
            b_remote.starts_with("remote:https://broker.example:8181#")
                && b_remote.ends_with("|anthropic@work"),
            "{b_remote}"
        );
        assert!(!b_remote.contains("MACHINE-SECRET"));
        // The principal is a fixed-width hex fingerprint, not any token bytes.
        let fingerprint = b_remote
            .strip_prefix("remote:https://broker.example:8181#")
            .and_then(|rest| rest.strip_suffix("|anthropic@work"))
            .unwrap();
        assert_eq!(fingerprint.len(), 16);
        assert!(fingerprint.chars().all(|c| c.is_ascii_hexdigit()));
        let b_local = anthropic_binding(&CredentialSource::Local, &cred);
        assert_eq!(b_local, "local|anthropic@work");
        assert_ne!(
            b_local, b_remote,
            "same slot on different sources is a different credential"
        );
        let other_broker = CredentialSource::Remote {
            endpoint: "https://other.example".into(),
            machine_token: "MACHINE-SECRET".into(),
        };
        assert_ne!(anthropic_binding(&other_broker, &cred), b_remote);
        // Same endpoint, different machine principal: a different identity.
        let rekeyed = CredentialSource::Remote {
            endpoint: "https://broker.example:8181".into(),
            machine_token: "OTHER-MACHINE".into(),
        };
        let b_rekeyed = anthropic_binding(&rekeyed, &cred);
        assert_ne!(b_rekeyed, b_remote);
        assert!(!b_rekeyed.contains("OTHER-MACHINE"));
        // Trailing slash on the endpoint does not fork the identity.
        let slashed = CredentialSource::Remote {
            endpoint: "https://broker.example:8181/".into(),
            machine_token: "MACHINE-SECRET".into(),
        };
        assert_eq!(anthropic_binding(&slashed, &cred), b_remote);
    }

    /// The binding's source scope must discriminate exactly like the core
    /// `TokenCache` scope (endpoint + principal digest): if the cache would
    /// key two tokens apart, so does the runtime binding — and vice versa.
    #[test]
    fn binding_scope_matches_the_core_cache_scope() {
        use crate::auth::BrokerClient;
        for token in ["MACHINE-SECRET", ""] {
            let remote = CredentialSource::Remote {
                endpoint: "https://broker.example:8181".into(),
                machine_token: token.into(),
            };
            let core_scope = BrokerClient::from_source(&remote).unwrap().scope();
            assert_eq!(binding_scope(&remote), format!("remote:{core_scope}"));
            assert!(!core_scope.contains(token) || token.is_empty());
        }
        assert!(binding_scope(&CredentialSource::Remote {
            endpoint: "https://broker.example:8181".into(),
            machine_token: String::new(),
        })
        .ends_with("#anon"));
        assert_eq!(binding_scope(&CredentialSource::Local), "local");
    }

    #[test]
    fn turn_pinned_credential_requires_this_source_and_anthropic() {
        let remote = CredentialSource::Remote {
            endpoint: "https://b".into(),
            machine_token: "t1".into(),
        };
        let rekeyed = CredentialSource::Remote {
            endpoint: "https://b".into(),
            machine_token: "t2".into(),
        };
        let work = CredentialRef::new(
            OAuthProviderId::Anthropic,
            crate::auth::Account::parse("work").unwrap(),
        );
        let bound = anthropic_binding(&remote, &work);
        assert_eq!(
            turn_pinned_credential(&remote, Some(&bound)),
            Some(work.clone())
        );
        assert_eq!(turn_pinned_credential(&rekeyed, Some(&bound)), None);
        assert_eq!(
            turn_pinned_credential(&CredentialSource::Local, Some(&bound)),
            None
        );
        assert_eq!(turn_pinned_credential(&remote, None), None);
        assert_eq!(turn_pinned_credential(&remote, Some("garbage")), None);
        // A binding for another provider on this source is not an Anthropic seat.
        let codex = CredentialRef::default_for(OAuthProviderId::OpenAiCodex);
        let codex_bound = format!("{}|{}", binding_scope(&remote), codex.storage_key());
        assert_eq!(turn_pinned_credential(&remote, Some(&codex_bound)), None);
    }

    #[test]
    fn check_binding_matrix() {
        use crate::auth::Account;
        let local = CredentialSource::Local;
        let explicit_default = AccountSelector::Account(Account::Default);
        let explicit_work = AccountSelector::Account(Account::parse("work").unwrap());
        use RefreshPoint::{TurnStart, WithinTurn};
        assert_eq!(
            check_binding(
                &local,
                &explicit_default,
                Some("local|anthropic"),
                TurnStart
            ),
            BindingCheck::Matches
        );
        assert_eq!(
            check_binding(&local, &explicit_work, Some("local|anthropic"), TurnStart),
            BindingCheck::Switched
        );
        assert_eq!(
            check_binding(
                &local,
                &explicit_work,
                Some("local|anthropic@work"),
                WithinTurn
            ),
            BindingCheck::Matches
        );
        assert_eq!(
            check_binding(&local, &explicit_default, None, TurnStart),
            BindingCheck::Switched
        );
        // Auto never trusts a cross-turn binding, even a matching one …
        assert_eq!(
            check_binding(
                &local,
                &AccountSelector::Auto,
                Some("local|anthropic"),
                TurnStart
            ),
            BindingCheck::AutoRepin
        );
        // … but keeps the seat a turn started on for that turn's later rounds.
        assert_eq!(
            check_binding(
                &local,
                &AccountSelector::Auto,
                Some("local|anthropic"),
                WithinTurn
            ),
            BindingCheck::Matches
        );
        assert_eq!(
            check_binding(&local, &AccountSelector::Auto, None, WithinTurn),
            BindingCheck::AutoRepin
        );
        // A fail-closed selector never serves a cached token.
        let invalid = AccountSelector::Invalid {
            source: "config".into(),
            reason: "bad label".into(),
        };
        assert_eq!(
            check_binding(&local, &invalid, Some("local|anthropic"), WithinTurn),
            BindingCheck::Switched
        );
        // Source switch with the same slot is a switch.
        let remote = CredentialSource::Remote {
            endpoint: "https://b".into(),
            machine_token: "t".into(),
        };
        assert_eq!(
            check_binding(
                &remote,
                &explicit_default,
                Some("local|anthropic"),
                TurnStart
            ),
            BindingCheck::Switched
        );
        // Same endpoint, different machine principal: explicit and Auto
        // mid-turn both refuse the previous principal's token.
        let rekeyed = CredentialSource::Remote {
            endpoint: "https://b".into(),
            machine_token: "t-other".into(),
        };
        let default_cred = CredentialRef::default_for(OAuthProviderId::Anthropic);
        let bound_remote = anthropic_binding(&remote, &default_cred);
        assert_eq!(
            check_binding(&remote, &explicit_default, Some(&bound_remote), WithinTurn),
            BindingCheck::Matches
        );
        assert_eq!(
            check_binding(&rekeyed, &explicit_default, Some(&bound_remote), WithinTurn),
            BindingCheck::Switched
        );
        assert_eq!(
            check_binding(
                &remote,
                &AccountSelector::Auto,
                Some(&bound_remote),
                WithinTurn
            ),
            BindingCheck::Matches
        );
        assert_eq!(
            check_binding(
                &rekeyed,
                &AccountSelector::Auto,
                Some(&bound_remote),
                WithinTurn
            ),
            BindingCheck::Switched
        );
        // Auto mid-turn also refuses a seat from another endpoint or from
        // Local, and any binding it cannot parse as an Anthropic seat.
        assert_eq!(
            check_binding(
                &remote,
                &AccountSelector::Auto,
                Some("local|anthropic"),
                WithinTurn
            ),
            BindingCheck::Switched
        );
        assert_eq!(
            check_binding(
                &local,
                &AccountSelector::Auto,
                Some(&bound_remote),
                WithinTurn
            ),
            BindingCheck::Switched
        );
        assert_eq!(
            check_binding(
                &local,
                &AccountSelector::Auto,
                Some("local|openai-codex"),
                WithinTurn
            ),
            BindingCheck::Switched
        );
    }

    // ── Scripted-broker tests: selection semantics without env/config/network ──

    use agent_core::auth::{
        AccessToken, BrokerError, ProxyByteStream, ProxyRequest, ProxyResponse,
    };
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Broker with a fixed selector that records which vend path the runtime
    /// took: explicit (`access_token_for`, by storage key) or automatic
    /// (`access_token_pinned_for`). Tokens name their path so the state
    /// reveals which one populated it.
    struct ScriptedBroker {
        selector: AccountSelector,
        explicit_calls: Mutex<Vec<String>>,
        auto_calls: Mutex<Vec<Option<String>>>,
        /// Storage keys whose explicit vend fails (revoked seat).
        explicit_denied: Vec<String>,
        /// The seat the automatic selector picks.
        auto_pick: &'static str,
        /// When set, the automatic selector fails with this error instead.
        auto_error: Mutex<Option<BrokerError>>,
        /// Storage keys of seats that are cooling down.
        cooling: Vec<String>,
    }

    impl ScriptedBroker {
        fn new(selector: AccountSelector) -> Self {
            Self {
                selector,
                explicit_calls: Mutex::new(Vec::new()),
                auto_calls: Mutex::new(Vec::new()),
                explicit_denied: Vec::new(),
                auto_pick: "auto-pick",
                auto_error: Mutex::new(None),
                cooling: Vec::new(),
            }
        }
        fn explicit(&self) -> Vec<String> {
            self.explicit_calls.lock().unwrap().clone()
        }
        fn auto(&self) -> Vec<Option<String>> {
            self.auto_calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl CredentialBroker for ScriptedBroker {
        async fn access_token(
            &self,
            _p: OAuthProviderId,
        ) -> std::result::Result<AccessToken, BrokerError> {
            Err(BrokerError::Denied("legacy path must not be used".into()))
        }
        fn account_selector(&self, _p: OAuthProviderId) -> AccountSelector {
            self.selector.clone()
        }
        fn active_cooldown(&self, cred: &CredentialRef) -> Option<u64> {
            self.cooling
                .contains(&cred.storage_key())
                .then_some(FRESH)
        }
        async fn access_token_for(
            &self,
            cred: &CredentialRef,
        ) -> std::result::Result<AccessToken, BrokerError> {
            let key = cred.storage_key();
            self.explicit_calls.lock().unwrap().push(key.clone());
            if self.explicit_denied.contains(&key) {
                return Err(BrokerError::Denied(format!("{key} revoked")));
            }
            Ok(AccessToken {
                token: format!("tok-explicit-{key}"),
                expires: u64::MAX,
            })
        }
        async fn access_token_pinned_for(
            &self,
            provider: OAuthProviderId,
            model: Option<&str>,
        ) -> std::result::Result<PinnedToken, BrokerError> {
            self.auto_calls
                .lock()
                .unwrap()
                .push(model.map(str::to_string));
            if let Some(e) = self.auto_error.lock().unwrap().clone() {
                return Err(e);
            }
            let credential = CredentialRef::new(
                provider,
                crate::auth::Account::parse(self.auto_pick).unwrap(),
            );
            Ok(PinnedToken {
                credential,
                token: AccessToken {
                    token: format!("tok-auto-{}", self.auto_pick),
                    expires: u64::MAX,
                },
            })
        }
        async fn proxy(&self, _r: ProxyRequest) -> std::result::Result<ProxyResponse, BrokerError> {
            Err(BrokerError::Denied("not implemented in stub".into()))
        }
        async fn proxy_stream(
            &self,
            _r: ProxyRequest,
        ) -> std::result::Result<ProxyByteStream, BrokerError> {
            Err(BrokerError::Denied("not implemented in stub".into()))
        }
        async fn anthropic_usage(&self) -> std::result::Result<serde_json::Value, BrokerError> {
            Err(BrokerError::Denied("not implemented in stub".into()))
        }
        async fn capabilities(
            &self,
        ) -> std::result::Result<Vec<agent_core::auth::ProviderStatus>, BrokerError> {
            Ok(vec![])
        }
    }

    fn remote(machine_token: &str) -> CredentialSource {
        CredentialSource::Remote {
            endpoint: "https://broker.example:8181".into(),
            machine_token: machine_token.into(),
        }
    }

    fn work_cred() -> CredentialRef {
        CredentialRef::new(
            OAuthProviderId::Anthropic,
            crate::auth::Account::parse("work").unwrap(),
        )
    }

    fn bound_state(
        source: &CredentialSource,
        cred: &CredentialRef,
        expires: u64,
    ) -> Arc<RwLock<AuthState>> {
        Arc::new(RwLock::new(AuthState {
            auth_token: "sk-HELD".into(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: Some(expires),
            auth_file_mtime: None,
            bound_credential: Some(anthropic_binding(source, cred)),
        }))
    }

    const FRESH: u64 = 9_999_999_999_999;
    const STALE: u64 = 1;

    /// Reviewer finding: the binding must carry the machine principal. A
    /// token vended under principal A, still fresh, must NOT be served when
    /// the same endpoint is now presented with principal B — for an explicit
    /// selector AND for Auto mid-turn (the two fast-path shapes).
    #[tokio::test]
    async fn same_endpoint_different_principal_never_serves_the_held_token() {
        for selector in [
            AccountSelector::Account(crate::auth::Account::parse("work").unwrap()),
            AccountSelector::Auto,
        ] {
            let vended_under = remote("principal-A");
            let now_presented = remote("principal-B");
            let auth = bound_state(&vended_under, &work_cred(), FRESH);
            let broker = ScriptedBroker::new(selector.clone());
            AuthMethods::refresh_with_broker(
                Arc::clone(&auth),
                &now_presented,
                &broker,
                Some("claude-opus-4-7"),
                RefreshPoint::WithinTurn,
            )
            .await
            .unwrap();
            let g = auth.read().await;
            assert_ne!(
                g.auth_token, "sk-HELD",
                "{selector:?}: previous principal's token served"
            );
            let bound = g.bound_credential.clone().unwrap();
            assert!(
                bound.starts_with(&binding_scope(&now_presented)),
                "{selector:?}: rebound to the presented principal, got {bound}"
            );
            assert!(
                !bound.contains("principal-"),
                "raw machine token in binding: {bound}"
            );
            match selector {
                AccountSelector::Account(_) => {
                    assert_eq!(broker.explicit(), vec!["anthropic@work"]);
                    assert!(broker.auto().is_empty());
                }
                _ => {
                    // The old binding is not a seat on THIS source, so Auto
                    // re-pins rather than re-vending the foreign seat.
                    assert!(
                        broker.explicit().is_empty(),
                        "must not re-vend a foreign seat"
                    );
                    assert_eq!(broker.auto(), vec![Some("claude-opus-4-7".to_string())]);
                }
            }
        }
    }

    /// Same principal, same seat, fresh: the fast path serves it for both
    /// selector shapes mid-turn — no broker traffic at all.
    #[tokio::test]
    async fn same_principal_fresh_seat_is_served_mid_turn_without_broker_calls() {
        for selector in [
            AccountSelector::Account(crate::auth::Account::parse("work").unwrap()),
            AccountSelector::Auto,
        ] {
            let source = remote("principal-A");
            let auth = bound_state(&source, &work_cred(), FRESH);
            let broker = ScriptedBroker::new(selector);
            AuthMethods::refresh_with_broker(
                Arc::clone(&auth),
                &source,
                &broker,
                Some("claude-opus-4-7"),
                RefreshPoint::WithinTurn,
            )
            .await
            .unwrap();
            assert_eq!(auth.read().await.auth_token, "sk-HELD");
            assert!(broker.explicit().is_empty() && broker.auto().is_empty());
        }
    }

    /// Auto at a turn boundary never serves the held token, even when it is
    /// fresh and on this source: capacity is re-checked by the selector.
    #[tokio::test]
    async fn auto_turn_start_always_repins_through_the_selector() {
        let source = remote("principal-A");
        let auth = bound_state(&source, &work_cred(), FRESH);
        let broker = ScriptedBroker::new(AccountSelector::Auto);
        AuthMethods::refresh_with_broker(
            Arc::clone(&auth),
            &source,
            &broker,
            Some("anthropic/claude-opus-4-7"),
            RefreshPoint::TurnStart,
        )
        .await
        .unwrap();
        let g = auth.read().await;
        assert_eq!(g.auth_token, "tok-auto-auto-pick");
        assert_eq!(
            g.bound_credential.as_deref(),
            Some(
                anthropic_binding(
                    &source,
                    &CredentialRef::new(
                        OAuthProviderId::Anthropic,
                        crate::auth::Account::parse("auto-pick").unwrap()
                    )
                )
                .as_str()
            )
        );
        assert!(
            broker.explicit().is_empty(),
            "turn start must not pin by explicit credential"
        );
        // Routing prefix stripped; full canonical id kept for family matching.
        assert_eq!(broker.auto(), vec![Some("claude-opus-4-7".to_string())]);
    }

    /// Auto mid-turn with a STALE token: a refresh event, not a re-selection
    /// event. The seat this turn pinned is re-vended by explicit credential;
    /// the capacity selector is not consulted, so the turn keeps its account
    /// (and prompt cache) and spends no usage fetches.
    #[tokio::test]
    async fn auto_within_turn_stale_token_revends_the_pinned_seat_not_a_reselection() {
        let source = remote("principal-A");
        let auth = bound_state(&source, &work_cred(), STALE);
        let broker = ScriptedBroker::new(AccountSelector::Auto);
        AuthMethods::refresh_with_broker(
            Arc::clone(&auth),
            &source,
            &broker,
            Some("claude-opus-4-7"),
            RefreshPoint::WithinTurn,
        )
        .await
        .unwrap();
        let g = auth.read().await;
        assert_eq!(g.auth_token, "tok-explicit-anthropic@work");
        assert_eq!(
            g.bound_credential.as_deref(),
            Some(anthropic_binding(&source, &work_cred()).as_str()),
            "seat unchanged"
        );
        assert_eq!(broker.explicit(), vec!["anthropic@work"]);
        assert!(broker.auto().is_empty(), "no re-selection mid-turn");
    }

    /// Only when the pinned seat can no longer be vended does Auto fall back
    /// to re-selection mid-turn — Auto opted in to switching; the fallback is
    /// logged, and the binding then names the seat actually vended.
    #[tokio::test]
    async fn auto_within_turn_falls_back_to_reselection_only_if_the_pinned_seat_is_gone() {
        let source = remote("principal-A");
        let auth = bound_state(&source, &work_cred(), STALE);
        let mut broker = ScriptedBroker::new(AccountSelector::Auto);
        broker.explicit_denied = vec!["anthropic@work".into()];
        AuthMethods::refresh_with_broker(
            Arc::clone(&auth),
            &source,
            &broker,
            Some("claude-opus-4-7"),
            RefreshPoint::WithinTurn,
        )
        .await
        .unwrap();
        let g = auth.read().await;
        assert_eq!(g.auth_token, "tok-auto-auto-pick");
        assert!(g
            .bound_credential
            .as_deref()
            .unwrap()
            .ends_with("|anthropic@auto-pick"));
        assert_eq!(
            broker.explicit(),
            vec!["anthropic@work"],
            "pinned seat tried first"
        );
        assert_eq!(broker.auto().len(), 1);
    }

    /// S4: a seat this turn pinned that is now cooling down (another
    /// session met its limit) is not kept mid-turn, fresh or stale: Auto
    /// re-selects without re-vending it, so the turn's next request does
    /// not meet the same limit. Without a cooldown the fresh seat is kept
    /// with no broker call, and an explicit selector keeps its account
    /// whatever its cooldown.
    #[tokio::test]
    async fn auto_within_turn_leaves_a_pinned_seat_that_is_cooling_down() {
        let source = remote("principal-A");
        for expires in [FRESH, STALE] {
            let auth = bound_state(&source, &work_cred(), expires);
            let mut broker = ScriptedBroker::new(AccountSelector::Auto);
            broker.cooling = vec!["anthropic@work".into()];
            AuthMethods::refresh_with_broker(
                Arc::clone(&auth),
                &source,
                &broker,
                Some("claude-opus-4-7"),
                RefreshPoint::WithinTurn,
            )
            .await
            .unwrap();
            let g = auth.read().await;
            assert_eq!(g.auth_token, "tok-auto-auto-pick", "expires {expires}");
            assert!(g
                .bound_credential
                .as_deref()
                .unwrap()
                .ends_with("|anthropic@auto-pick"));
            assert!(broker.explicit().is_empty(), "the cooling seat is not re-vended");
            assert_eq!(broker.auto(), vec![Some("claude-opus-4-7".to_string())]);
        }

        let auth = bound_state(&source, &work_cred(), FRESH);
        let mut broker = ScriptedBroker::new(AccountSelector::Auto);
        broker.cooling = vec!["anthropic@other".into()];
        AuthMethods::refresh_with_broker(
            Arc::clone(&auth),
            &source,
            &broker,
            Some("claude-opus-4-7"),
            RefreshPoint::WithinTurn,
        )
        .await
        .unwrap();
        assert_eq!(auth.read().await.auth_token, "sk-HELD");
        assert!(broker.explicit().is_empty() && broker.auto().is_empty());

        let auth = bound_state(&source, &work_cred(), FRESH);
        let mut broker = ScriptedBroker::new(AccountSelector::Account(
            crate::auth::Account::parse("work").unwrap(),
        ));
        broker.cooling = vec!["anthropic@work".into()];
        AuthMethods::refresh_with_broker(
            Arc::clone(&auth),
            &source,
            &broker,
            Some("claude-opus-4-7"),
            RefreshPoint::WithinTurn,
        )
        .await
        .unwrap();
        assert_eq!(auth.read().await.auth_token, "sk-HELD");
        assert!(broker.explicit().is_empty() && broker.auto().is_empty());
    }

    /// An explicit selector with a stale token re-vends exactly that
    /// credential — never the selector's automatic pick — and a denied
    /// explicit vend is an error, not a silent switch.
    #[tokio::test]
    async fn explicit_selector_never_falls_back_to_auto() {
        let source = remote("principal-A");
        let auth = bound_state(&source, &work_cred(), STALE);
        let mut broker = ScriptedBroker::new(AccountSelector::Account(
            crate::auth::Account::parse("work").unwrap(),
        ));
        broker.explicit_denied = vec!["anthropic@work".into()];
        let err = AuthMethods::refresh_with_broker(
            Arc::clone(&auth),
            &source,
            &broker,
            Some("claude-opus-4-7"),
            RefreshPoint::WithinTurn,
        )
        .await
        .expect_err("denied explicit vend must surface");
        assert!(err.to_string().contains("account 'work'"), "{err}");
        assert!(
            broker.auto().is_empty(),
            "explicit must never consult the selector"
        );
        // State untouched: the stale token is not replaced by anything else.
        let g = auth.read().await;
        assert_eq!(g.auth_token, "sk-HELD");
        assert_eq!(g.token_expires, Some(STALE));
    }

    /// Invalid selector: fail closed even with a fresh, correctly bound token.
    #[tokio::test]
    async fn invalid_selector_fails_closed_without_broker_calls() {
        let source = remote("principal-A");
        let auth = bound_state(&source, &work_cred(), FRESH);
        let broker = ScriptedBroker::new(AccountSelector::Invalid {
            source: "config".into(),
            reason: "bad label".into(),
        });
        let err = AuthMethods::refresh_with_broker(
            Arc::clone(&auth),
            &source,
            &broker,
            None,
            RefreshPoint::WithinTurn,
        )
        .await
        .expect_err("invalid selector must not serve any token");
        assert!(err.to_string().contains("invalid"), "{err}");
        assert!(broker.explicit().is_empty() && broker.auto().is_empty());
    }

    // ── Typed automatic-selection failures (user text + one WARN) ────────

    use agent_core::auth::{
        AccountFailure, AccountFailureKind as AK, SelectionFailure, SelectionFailureKind as SK,
    };

    /// In-process sink for formatted tracing output.
    #[derive(Clone, Default)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    struct LogWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl LogCapture {
        /// Formats into the capture through the daemon log file's own
        /// filter: `with_crate_directives` over the ERROR default an unset
        /// `RUST_LOG` gives, exactly what `init_logging` builds. A line
        /// these tests find is a line synaps.log keeps.
        fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync {
            use tracing_subscriber::filter::{EnvFilter, LevelFilter};
            let unset = EnvFilter::builder()
                .with_default_directive(LevelFilter::ERROR.into())
                .parse_lossy("");
            let sink = self.0.clone();
            tracing_subscriber::fmt()
                .with_env_filter(crate::logging::with_crate_directives(unset))
                .with_writer(move || LogWriter(sink.clone()))
                .with_ansi(false)
                .finish()
        }
        fn take(&self) -> String {
            String::from_utf8(std::mem::take(&mut *self.0.lock().unwrap())).unwrap()
        }
    }

    fn account(label: &str, kind: AK) -> AccountFailure {
        AccountFailure {
            account: label.into(),
            kind,
            http_status: None,
            retry_after_s: None,
            until_ms: None,
            cooling_down: false,
        }
    }

    fn no_account(failure: SelectionFailure) -> BrokerError {
        BrokerError::NoAccountAvailable {
            provider: "anthropic".into(),
            reason: "scripted".into(),
            failure: Some(failure),
        }
    }

    /// Run one turn-start Auto refresh against a selector failing with
    /// `error`; returns the user text and the captured log.
    async fn failed_turn_start(error: BrokerError) -> (String, String) {
        let source = remote("principal-A");
        let auth = bound_state(&source, &work_cred(), FRESH);
        let broker = ScriptedBroker::new(AccountSelector::Auto);
        *broker.auto_error.lock().unwrap() = Some(error);
        let capture = LogCapture::default();
        let err = {
            let _guard = tracing::subscriber::set_default(capture.subscriber());
            AuthMethods::refresh_with_broker(
                Arc::clone(&auth),
                &source,
                &broker,
                Some("claude-opus-4-7"),
                RefreshPoint::TurnStart,
            )
            .await
            .expect_err("a failed selection ends the turn")
        };
        assert!(matches!(err, RuntimeError::Auth(_)), "{err:?}");
        assert_eq!(
            auth.read().await.auth_token,
            "sk-HELD",
            "no token may be bound"
        );
        (err.to_string(), capture.take())
    }

    /// The one `turn_auth_failed` WARN line in `log`.
    fn turn_warn(log: &str) -> String {
        let lines: Vec<_> = log
            .lines()
            .filter(|l| l.contains("turn_auth_failed"))
            .collect();
        assert_eq!(lines.len(), 1, "exactly one turn_auth_failed WARN:\n{log}");
        assert!(lines[0].contains("WARN"), "{}", lines[0]);
        lines[0].to_string()
    }

    /// The turn_auth_failed WARN reaches the daemon log, and the capture
    /// is the real filter, not a pass-through: in the same subscriber a
    /// WARN at a target outside the crate directives is dropped while an
    /// ERROR there is kept (ERROR is the unset-`RUST_LOG` default).
    #[tokio::test]
    async fn turn_auth_failed_passes_the_daemon_log_filter_and_a_foreign_warn_does_not() {
        let mut c2 = account("claude2", AK::UsageRateLimited);
        c2.http_status = Some(429);
        let failure = SelectionFailure {
            kind: SK::UsageReadFailed,
            accounts: vec![c2],
            earliest_reset_ms: None,
            usage_read_retry_ms: None,
        };
        let (_, log) = failed_turn_start(no_account(failure)).await;
        assert!(turn_warn(&log).contains("kind=usage_read_failed"), "{log}");

        let capture = LogCapture::default();
        {
            let _guard = tracing::subscriber::set_default(capture.subscriber());
            tracing::warn!(target: "unrelated_crate::x", "foreign_warn_control");
            tracing::error!(target: "unrelated_crate::x", "foreign_error_control");
        }
        let control = capture.take();
        assert!(!control.contains("foreign_warn_control"), "{control}");
        assert!(control.contains("foreign_error_control"), "{control}");
    }

    /// JR's incident: usage reads failed (endpoint 429/503) while one account
    /// was cooling down. The text names the kind, each account and the
    /// earliest reset, says what the user can do, and gives NO login advice:
    /// a login would not have helped.
    #[tokio::test]
    async fn usage_read_failure_text_names_kind_accounts_and_reset_without_login_advice() {
        let now = crate::epoch_millis();
        let reset = now + 30 * 60 * 1000;
        let mut c2 = account("claude2", AK::UsageRateLimited);
        c2.http_status = Some(429);
        c2.retry_after_s = Some(120);
        let mut c5 = account("claude5", AK::CoolingDown);
        c5.until_ms = Some(reset);
        let mut c6 = account("claude6", AK::UsageUpstream);
        c6.http_status = Some(503);
        let failure = SelectionFailure {
            kind: SK::UsageReadFailed,
            accounts: vec![c2, c5, c6],
            earliest_reset_ms: Some(reset),
            usage_read_retry_ms: None,
        };
        let (text, log) = failed_turn_start(no_account(failure)).await;
        let reset_text = super::super::anthropic_quota::describe_reset_utc(reset, now);
        for needle in [
            "usage_read_failed",
            "claude2: usage_rate_limited (HTTP 429, retry after 120s)",
            "claude5: cooling_down",
            "claude6: usage_upstream (HTTP 503)",
            reset_text.as_str(),
            "Send again to retry; no login needed.",
        ] {
            assert!(text.contains(needle), "{needle:?} missing from: {text}");
        }
        assert!(
            !text.contains("synaps login"),
            "no login advice for a read failure: {text}"
        );
        let warn = turn_warn(&log);
        assert!(warn.contains("kind=usage_read_failed"), "{warn}");
    }

    /// PR266 review SF3: when every live transient failure carries a
    /// Retry-After, sending again right away only repeats the 429; the text
    /// says to wait the shortest of them. One without a Retry-After keeps
    /// the plain "send again" text. Limited and excluded accounts don't count.
    #[tokio::test]
    async fn usage_read_failure_text_honours_retry_after() {
        let mut c2 = account("claude2", AK::UsageRateLimited);
        c2.http_status = Some(429);
        c2.retry_after_s = Some(120);
        let mut c3 = account("claude3", AK::RefreshOther);
        c3.retry_after_s = Some(45);
        let mut c5 = account("claude5", AK::UsageRateLimited);
        c5.cooling_down = true;
        let c7 = account("claude7", AK::Excluded);
        let failure = SelectionFailure {
            kind: SK::UsageReadFailed,
            accounts: vec![c2.clone(), c3, c5, c7],
            earliest_reset_ms: None,
            usage_read_retry_ms: None,
        };
        let (text, _) = failed_turn_start(no_account(failure)).await;
        assert!(
            text.contains("Send again in 45s or later; no login needed."),
            "{text}"
        );
        assert!(!text.contains("Send again to retry"), "{text}");

        let mut c6 = account("claude6", AK::UsageUpstream);
        c6.http_status = Some(503);
        let failure = SelectionFailure {
            kind: SK::UsageReadFailed,
            accounts: vec![c2.clone(), c6],
            earliest_reset_ms: None,
            usage_read_retry_ms: None,
        };
        let (text, _) = failed_turn_start(no_account(failure)).await;
        assert!(
            text.contains("Send again to retry; no login needed."),
            "{text}"
        );

        // A login problem elsewhere drops "no login needed" but keeps the wait.
        let failure = SelectionFailure {
            kind: SK::UsageReadFailed,
            accounts: vec![account("claude4", AK::RefreshRejected), c2],
            earliest_reset_ms: None,
            usage_read_retry_ms: None,
        };
        let (text, _) = failed_turn_start(no_account(failure)).await;
        assert!(text.contains("Send again in 120s or later."), "{text}");
        assert!(!text.contains("no login needed"), "{text}");
    }

    /// Held usage reads: the text names when usage is read again (the next
    /// selection reads only after the hold), not a bare "retry", and says no
    /// login is needed unless an account needs one.
    #[tokio::test]
    async fn held_usage_read_text_names_the_read_time_and_no_login() {
        let now = crate::epoch_millis();
        let retry = now + 4 * 60 * 1000;
        let mut c2 = account("claude2", AK::UsageRateLimited);
        c2.http_status = Some(429);
        let failure = SelectionFailure {
            kind: SK::UsageReadFailed,
            accounts: vec![c2.clone()],
            earliest_reset_ms: None,
            usage_read_retry_ms: Some(retry),
        };
        let (text, _) = failed_turn_start(no_account(failure)).await;
        let at = super::super::anthropic_quota::describe_reset_utc(retry, now);
        let want = format!("Usage is read again after {at}; send again then. No login needed.");
        assert!(text.contains(&want), "{want:?} missing from: {text}");
        assert!(text.contains("capacity is unproven (not used up)"), "{text}");
        assert!(!text.contains("Send again to retry"), "{text}");
        assert!(!text.contains("synaps login"), "{text}");

        // An account that needs a login keeps its advice and drops "No login needed".
        let failure = SelectionFailure {
            kind: SK::UsageReadFailed,
            accounts: vec![account("claude3", AK::RefreshRejected), c2.clone()],
            earliest_reset_ms: None,
            usage_read_retry_ms: Some(retry),
        };
        let (text, _) = failed_turn_start(no_account(failure)).await;
        let held = format!("Usage is read again after {at}; send again then.");
        assert!(text.contains(&held), "{text}");
        assert!(text.contains("--account claude3"), "{text}");
        assert!(!text.contains("No login needed"), "{text}");

        // A read time already past falls back to S1's text.
        let failure = SelectionFailure {
            kind: SK::UsageReadFailed,
            accounts: vec![c2],
            earliest_reset_ms: None,
            usage_read_retry_ms: Some(now.saturating_sub(1000)),
        };
        let (text, _) = failed_turn_start(no_account(failure)).await;
        assert!(text.contains("Send again to retry; no login needed."), "{text}");
        assert!(!text.contains("Usage is read again"), "{text}");
    }

    /// Login advice names exactly the accounts with a credential kind, even
    /// when the top-level kind is not a credential kind.
    #[tokio::test]
    async fn login_advice_names_exactly_the_accounts_whose_login_was_refused() {
        let mut c2 = account("claude2", AK::UsageUnauthorized);
        c2.http_status = Some(401);
        let failure = SelectionFailure {
            kind: SK::CredentialRejected,
            accounts: vec![
                c2,
                account("claude3", AK::RefreshRejected),
                account("default", AK::CredentialMissing),
                account("claude5", AK::CoolingDown),
            ],
            earliest_reset_ms: None,
            usage_read_retry_ms: None,
        };
        let (text, log) = failed_turn_start(no_account(failure)).await;
        for needle in [
            "credential_rejected",
            "`synaps login --provider anthropic --account claude2`",
            "`synaps login --provider anthropic --account claude3`",
            "`synaps login --provider anthropic`",
        ] {
            assert!(text.contains(needle), "{needle:?} missing from: {text}");
        }
        assert!(!text.contains("--account claude5"), "{text}");
        assert!(!text.contains("--account default"), "{text}");
        assert!(!text.contains("no login needed"), "{text}");
        assert!(turn_warn(&log).contains("kind=credential_rejected"));

        // Mixed: a read failure elsewhere keeps "send again", and only the
        // refused account gets login advice.
        let mut c6 = account("claude6", AK::UsageTimeout);
        c6.http_status = None;
        let failure = SelectionFailure {
            kind: SK::UsageReadFailed,
            accounts: vec![account("claude3", AK::RefreshRejected), c6],
            earliest_reset_ms: None,
            usage_read_retry_ms: None,
        };
        let (text, _) = failed_turn_start(no_account(failure)).await;
        assert!(text.contains("Send again to retry"), "{text}");
        assert!(text.contains("--account claude3"), "{text}");
        assert!(!text.contains("--account claude6"), "{text}");
        assert!(!text.contains("no login needed"), "{text}");
    }

    /// Every account limited: the earliest reset, no login advice.
    #[tokio::test]
    async fn all_cooling_down_text_gives_the_earliest_reset_and_no_login_advice() {
        let now = crate::epoch_millis();
        let soon = now + 20 * 60 * 1000;
        let mut c5 = account("claude5", AK::Exhausted);
        c5.until_ms = Some(now + 3 * 60 * 60 * 1000);
        let mut c6 = account("claude6", AK::CoolingDown);
        c6.until_ms = Some(soon);
        let failure = SelectionFailure {
            kind: SK::AllCoolingDown,
            accounts: vec![c5, c6],
            earliest_reset_ms: Some(soon),
            usage_read_retry_ms: None,
        };
        let (text, log) = failed_turn_start(no_account(failure)).await;
        let soon_text = super::super::anthropic_quota::describe_reset_utc(soon, now);
        assert!(text.contains("all_cooling_down"), "{text}");
        assert!(
            text.contains(&format!("No login needed; send again after {soon_text}.")),
            "{text}"
        );
        assert!(!text.contains("synaps login"), "{text}");
        assert!(turn_warn(&log).contains("kind=all_cooling_down"));
    }

    /// S4 review: the seat this turn pinned is cooling down and no other
    /// account can serve. The mid-turn re-selection ends the turn with the
    /// usual all-cooling text (the earliest time, no login advice); the
    /// cooling seat is not re-vended and no token is bound.
    #[tokio::test]
    async fn auto_within_turn_cooling_seat_with_no_other_account_gets_the_cooling_text() {
        let now = crate::epoch_millis();
        let until = now + 20 * 60 * 1000;
        let mut work = account("work", AK::CoolingDown);
        work.until_ms = Some(until);
        work.cooling_down = true;
        let failure = SelectionFailure {
            kind: SK::AllCoolingDown,
            accounts: vec![work],
            earliest_reset_ms: Some(until),
            usage_read_retry_ms: None,
        };
        let source = remote("principal-A");
        let auth = bound_state(&source, &work_cred(), FRESH);
        let mut broker = ScriptedBroker::new(AccountSelector::Auto);
        broker.cooling = vec!["anthropic@work".into()];
        *broker.auto_error.lock().unwrap() = Some(no_account(failure));
        let capture = LogCapture::default();
        let err = {
            let _guard = tracing::subscriber::set_default(capture.subscriber());
            AuthMethods::refresh_with_broker(
                Arc::clone(&auth),
                &source,
                &broker,
                Some("claude-opus-4-7"),
                RefreshPoint::WithinTurn,
            )
            .await
            .expect_err("no account can serve")
        };
        let text = err.to_string();
        let until_text = super::super::anthropic_quota::describe_reset_utc(until, now);
        assert!(text.contains("all_cooling_down"), "{text}");
        assert!(
            text.contains(&format!("No login needed; send again after {until_text}.")),
            "{text}"
        );
        assert!(!text.contains("synaps login"), "{text}");
        assert!(turn_warn(&capture.take()).contains("kind=all_cooling_down"));
        assert!(broker.explicit().is_empty(), "the cooling seat is not re-vended");
        assert_eq!(broker.auto(), vec![Some("claude-opus-4-7".to_string())]);
        assert_eq!(auth.read().await.auth_token, "sk-HELD", "no token is bound");
    }

    /// r2 blocker 1: every account cooling, one of them with a refused
    /// login. The kind stays all_cooling_down and the earliest time stays,
    /// but that label's exact login command is named and the text never
    /// says "No login needed".
    #[tokio::test]
    async fn all_cooling_with_one_refused_login_names_its_command_and_never_no_login_needed() {
        let now = crate::epoch_millis();
        let soon = now + 20 * 60 * 1000;
        let later = now + 2 * 60 * 60 * 1000;
        let mut c2 = account("claude2", AK::UsageUnauthorized);
        c2.http_status = Some(401);
        c2.cooling_down = true;
        c2.until_ms = Some(later);
        let mut c6 = account("claude6", AK::CoolingDown);
        c6.cooling_down = true;
        c6.until_ms = Some(soon);
        let failure = SelectionFailure {
            kind: SK::AllCoolingDown,
            accounts: vec![c2, c6],
            earliest_reset_ms: Some(soon),
            usage_read_retry_ms: None,
        };
        let (text, log) = failed_turn_start(no_account(failure)).await;
        let soon_text = super::super::anthropic_quota::describe_reset_utc(soon, now);
        let later_text = super::super::anthropic_quota::describe_reset_utc(later, now);
        for needle in [
            "all_cooling_down".to_string(),
            format!("claude2: usage_unauthorized (HTTP 401, cooling down until {later_text})"),
            "claude2 needs a new login: `synaps login --provider anthropic --account claude2`"
                .to_string(),
            format!("Send again after {soon_text}."),
        ] {
            assert!(text.contains(&needle), "{needle:?} missing from: {text}");
        }
        assert!(!text.to_lowercase().contains("no login needed"), "{text}");
        assert!(!text.contains("--account claude6"), "{text}");
        assert!(turn_warn(&log).contains("kind=all_cooling_down"));
    }

    /// r2 blocker 1: a cooling account whose usage read was rate limited
    /// shows the 429 and its Retry-After beside the cooldown; no login.
    #[tokio::test]
    async fn a_cooling_account_with_a_usage_429_shows_status_retry_after_and_cooldown() {
        let now = crate::epoch_millis();
        let until = now + 40 * 60 * 1000;
        let mut c5 = account("claude5", AK::UsageRateLimited);
        c5.http_status = Some(429);
        c5.retry_after_s = Some(90);
        c5.cooling_down = true;
        c5.until_ms = Some(until);
        let failure = SelectionFailure {
            kind: SK::AllCoolingDown,
            accounts: vec![c5],
            earliest_reset_ms: Some(until),
            usage_read_retry_ms: None,
        };
        let (text, _) = failed_turn_start(no_account(failure)).await;
        let until_text = super::super::anthropic_quota::describe_reset_utc(until, now);
        for needle in [
            format!(
                "claude5: usage_rate_limited (HTTP 429, retry after 90s, cooling down until {until_text})"
            ),
            format!("No login needed; send again after {until_text}."),
        ] {
            assert!(text.contains(&needle), "{needle:?} missing from: {text}");
        }
        assert!(!text.contains("synaps login"), "{text}");
    }

    // ── The same texts from a CLASSIFIED failure (r3) ────────────────────
    //
    // `select` + `SelectionFailure::from_rejections` over scripted account
    // capacities: the headline kind is computed, never injected.

    fn anthropic_ref(label: &str) -> CredentialRef {
        CredentialRef::new(
            OAuthProviderId::Anthropic,
            crate::auth::Account::named(label).unwrap(),
        )
    }

    /// An account whose last read or refresh failed with `failure`, cooling
    /// down until `until` when given.
    fn failed_capacity(
        label: &str,
        failure: agent_core::auth::ObservationFailure,
        until: Option<u64>,
    ) -> crate::auth::quota_policy::AccountCapacity {
        let mut cap =
            crate::auth::quota_policy::AccountCapacity::failed(anthropic_ref(label), failure);
        cap.cooldown_until_ms = until;
        cap
    }

    /// An account with no reading that is cooling down until `until`.
    fn cooling_capacity(label: &str, until: u64) -> crate::auth::quota_policy::AccountCapacity {
        let mut cap = crate::auth::quota_policy::AccountCapacity::unobserved(anthropic_ref(label));
        cap.cooldown_until_ms = Some(until);
        cap
    }

    /// Run the real selection over `candidates` and classify its failure.
    fn classified(
        candidates: &[crate::auth::quota_policy::AccountCapacity],
        now: u64,
    ) -> SelectionFailure {
        use crate::auth::quota_policy::{select, Selection, SelectionRequest};
        let req = SelectionRequest::new(OAuthProviderId::Anthropic, now, 60_000);
        match select(&req, candidates) {
            Selection::NoCapacity { rejections, .. } => {
                SelectionFailure::from_rejections(&rejections, now)
            }
            other => panic!("expected no capacity, got {other:?}"),
        }
    }

    fn throttled_429() -> agent_core::auth::ObservationFailure {
        agent_core::auth::ObservationFailure::new(AK::UsageRateLimited)
            .with_status(429)
            .with_retry_after(Some(90))
    }

    /// r3, the B1 headline rule: the headline is what the user can act on
    /// now, classified over the accounts that aren't cooling down; a
    /// cooling account's read failure is shown per account. A: claude5
    /// cools with a usage 429, claude6 cools plainly. B: claude5 cools with
    /// a usage 429, claude2 is not cooling and its login was refused.
    /// Neither text may claim capacity is "not used up" or that sending
    /// again now retries anything.
    #[tokio::test]
    async fn classified_cooling_429_headlines_tell_what_can_be_done_now() {
        let now = crate::epoch_millis();
        let t5 = now + 30 * 60 * 1000;
        let t6 = now + 2 * 60 * 60 * 1000;
        let t5_text = super::super::anthropic_quota::describe_reset_utc(t5, now);
        let c5_part = format!(
            "claude5: usage_rate_limited (HTTP 429, retry after 90s, cooling down until {t5_text})"
        );

        let a = classified(
            &[
                failed_capacity("claude5", throttled_429(), Some(t5)),
                cooling_capacity("claude6", t6),
            ],
            now,
        );
        assert_eq!(a.kind, SK::AllCoolingDown, "{a:?}");
        let (text, log) = failed_turn_start(no_account(a)).await;
        for needle in [
            c5_part.clone(),
            format!("earliest reset {t5_text}"),
            format!("send again after {t5_text}."),
        ] {
            assert!(text.contains(&needle), "{needle:?} missing from: {text}");
        }
        let lower = text.to_lowercase();
        assert!(!lower.contains("send again to retry"), "{text}");
        assert!(!lower.contains("not used up"), "{text}");
        assert!(turn_warn(&log).contains("kind=all_cooling_down"), "{log}");

        let b = classified(
            &[
                failed_capacity("claude5", throttled_429(), Some(t5)),
                failed_capacity(
                    "claude2",
                    agent_core::auth::ObservationFailure::new(AK::UsageUnauthorized)
                        .with_status(401),
                    None,
                ),
            ],
            now,
        );
        assert_eq!(b.kind, SK::CredentialRejected, "{b:?}");
        let (text, log) = failed_turn_start(no_account(b)).await;
        for needle in [
            c5_part,
            format!("earliest reset {t5_text}"),
            "claude2 needs a new login: `synaps login --provider anthropic --account claude2`"
                .to_string(),
        ] {
            assert!(text.contains(&needle), "{needle:?} missing from: {text}");
        }
        let lower = text.to_lowercase();
        assert!(!lower.contains("send again to retry"), "{text}");
        assert!(!lower.contains("not used up"), "{text}");
        assert!(!lower.contains("no login needed"), "{text}");
        assert!(
            turn_warn(&log).contains("kind=credential_rejected"),
            "{log}"
        );
    }

    /// r3: the classified form of
    /// `all_cooling_with_one_refused_login_names_its_command_and_never_no_login_needed`.
    #[tokio::test]
    async fn classified_all_cooling_with_one_refused_login_names_its_command_and_never_no_login_needed(
    ) {
        let now = crate::epoch_millis();
        let soon = now + 20 * 60 * 1000;
        let later = now + 2 * 60 * 60 * 1000;
        let f = classified(
            &[
                failed_capacity(
                    "claude2",
                    agent_core::auth::ObservationFailure::new(AK::UsageUnauthorized)
                        .with_status(401),
                    Some(later),
                ),
                cooling_capacity("claude6", soon),
            ],
            now,
        );
        assert_eq!(f.kind, SK::AllCoolingDown, "{f:?}");
        let (text, log) = failed_turn_start(no_account(f)).await;
        let soon_text = super::super::anthropic_quota::describe_reset_utc(soon, now);
        let later_text = super::super::anthropic_quota::describe_reset_utc(later, now);
        for needle in [
            format!("claude2: usage_unauthorized (HTTP 401, cooling down until {later_text})"),
            "claude2 needs a new login: `synaps login --provider anthropic --account claude2`"
                .to_string(),
            format!("Send again after {soon_text}."),
        ] {
            assert!(text.contains(&needle), "{needle:?} missing from: {text}");
        }
        assert!(!text.to_lowercase().contains("no login needed"), "{text}");
        assert!(!text.contains("--account claude6"), "{text}");
        assert!(turn_warn(&log).contains("kind=all_cooling_down"), "{log}");
    }

    /// r2 item 7, the case the reviewer found untested: every account is
    /// cooling AND needs a login or an action, so cooldowns ending fixes
    /// nothing. The text names each fix and gives no time to send again,
    /// no retry or "later" advice and no "no login needed".
    #[tokio::test]
    async fn classified_all_cooling_and_all_needing_a_fix_names_each_fix_and_no_time_to_retry() {
        let now = crate::epoch_millis();
        let hour = 60 * 60 * 1000;
        let f = classified(
            &[
                failed_capacity(
                    "claude3",
                    agent_core::auth::ObservationFailure::new(AK::UsageUnauthorized)
                        .with_status(401),
                    Some(now + hour),
                ),
                failed_capacity(
                    "claude4",
                    agent_core::auth::ObservationFailure::new(AK::CredentialStore),
                    Some(now + 2 * hour),
                ),
                failed_capacity(
                    "claude5",
                    agent_core::auth::ObservationFailure::new(AK::RefreshActionRequired),
                    Some(now + 3 * hour),
                ),
            ],
            now,
        );
        assert_eq!(f.kind, SK::AllCoolingDown, "{f:?}");
        assert_eq!(f.earliest_reset_ms, None, "{f:?}");
        let (text, log) = failed_turn_start(no_account(f)).await;
        for needle in [
            "claude3 needs a new login: `synaps login --provider anthropic --account claude3`",
            "claude4: its stored login could not be read; repair or restore the credential store",
            "claude5: the token refresh needs an action outside synaps",
        ] {
            assert!(text.contains(needle), "{needle:?} missing from: {text}");
        }
        let lower = text.to_lowercase();
        for banned in [
            "send again",
            "later",
            "retry",
            "no login needed",
            "earliest reset",
        ] {
            assert!(!lower.contains(banned), "{banned:?} in: {text}");
        }
        assert!(turn_warn(&log).contains("kind=all_cooling_down"), "{log}");
    }

    /// S2 c3 condition 1, rendered: every account's chosen-seat refresh was
    /// refused in one selection (the broker carries each refusal as
    /// `refresh_rejected`/400). The classified failure names every login
    /// command, gives no time to send again, no retry or "later" advice and
    /// no "no login needed". A guard: S1's renderer already does this; the
    /// core test `every_chosen_accounts_refused_refresh_ends_the_selection_naming_each_login`
    /// pins the failure the broker builds.
    #[tokio::test]
    async fn classified_every_refused_refresh_names_each_login_and_no_time_to_retry() {
        let now = crate::epoch_millis();
        let refused = || {
            agent_core::auth::ObservationFailure::new(AK::RefreshRejected).with_status(400)
        };
        let f = classified(
            &[
                failed_capacity("claude2", refused(), None),
                failed_capacity("claude3", refused(), None),
            ],
            now,
        );
        assert_eq!(f.kind, SK::CredentialRejected, "{f:?}");
        assert_eq!(f.earliest_reset_ms, None, "{f:?}");
        let (text, log) = failed_turn_start(no_account(f)).await;
        for needle in [
            "claude2: refresh_rejected (HTTP 400)",
            "claude3: refresh_rejected (HTTP 400)",
            "claude2, claude3 need a new login: \
             `synaps login --provider anthropic --account claude2`, \
             `synaps login --provider anthropic --account claude3`.",
        ] {
            assert!(text.contains(needle), "{needle:?} missing from: {text}");
        }
        let lower = text.to_lowercase();
        for banned in [
            "send again",
            "later",
            "retry",
            "no login needed",
            "earliest reset",
        ] {
            assert!(!lower.contains(banned), "{banned:?} in: {text}");
        }
        assert!(
            turn_warn(&log).contains("kind=credential_rejected"),
            "{log}"
        );
    }

    /// r2 blocker 2: a refresh that needs an action outside synaps, or an
    /// unreadable store, is named with its remedy, with no "send again"
    /// retry advice, no "no login needed" and no login command.
    #[tokio::test]
    async fn action_required_selection_names_condition_and_remedy_without_retry_or_login_claims() {
        let mut c5 = account("claude5", AK::CoolingDown);
        c5.cooling_down = true;
        let failure = SelectionFailure {
            kind: SK::ActionRequired,
            accounts: vec![
                account("claude3", AK::RefreshActionRequired),
                account("claude4", AK::CredentialStore),
                c5,
            ],
            earliest_reset_ms: None,
            usage_read_retry_ms: None,
        };
        let (text, log) = failed_turn_start(no_account(failure)).await;
        for needle in [
            "action_required",
            "claude3: the token refresh needs an action outside synaps",
            "usage read needs action",
            "claude4: its stored login could not be read; repair or restore the credential store",
        ] {
            assert!(text.contains(needle), "{needle:?} missing from: {text}");
        }
        let lower = text.to_lowercase();
        assert!(!lower.contains("send again"), "{text}");
        assert!(!lower.contains("no login needed"), "{text}");
        assert!(!text.contains("synaps login"), "{text}");
        assert!(turn_warn(&log).contains("kind=action_required"));
    }

    /// r2 blocker 2 / item 5: automatic selection that could not even list
    /// the accounts (a corrupt or unreadable store) says so, with the store
    /// remedy instead of blanket login advice.
    #[tokio::test]
    async fn an_unreadable_store_in_automatic_selection_gets_store_advice_not_login_advice() {
        let (text, log) = failed_turn_start(BrokerError::Credential(
            "Failed to parse /scratch/auth.json: expected value at line 1 column 1".into(),
        ))
        .await;
        for needle in [
            "credential_store",
            "Failed to parse /scratch/auth.json",
            "repair or restore",
        ] {
            assert!(text.contains(needle), "{needle:?} missing from: {text}");
        }
        let lower = text.to_lowercase();
        assert!(!text.contains("synaps login"), "{text}");
        assert!(!lower.contains("send again"), "{text}");
        assert!(!lower.contains("no login needed"), "{text}");
        assert!(turn_warn(&log).contains("kind=credential_store"));
    }

    /// A remote broker's 503 carries no per-account detail: the text points
    /// at the broker host's log and makes no claim about logins.
    #[tokio::test]
    async fn remote_no_capacity_points_at_the_broker_log_without_login_claims() {
        let (text, log) =
            failed_turn_start(no_account(SelectionFailure::new(SK::RemoteNoCapacity))).await;
        assert!(text.contains("remote_no_capacity"), "{text}");
        assert!(text.contains("broker host"), "{text}");
        assert!(
            !text.contains("synaps login") && !text.contains("no login needed"),
            "{text}"
        );
        assert!(turn_warn(&log).contains("kind=remote_no_capacity"));
    }

    /// The automatically chosen seat's refresh failed transiently: not a
    /// login problem, and the text says so.
    #[tokio::test]
    async fn chosen_seat_transient_refresh_failure_is_not_login_advice() {
        use agent_core::auth::{RefreshFailure, RefreshFailureKind};
        let error = BrokerError::Refresh(RefreshFailure::new(
            RefreshFailureKind::Transient,
            "token endpoint returned HTTP 503",
        ));
        let (text, log) = failed_turn_start(error).await;
        assert!(text.contains("not a login problem"), "{text}");
        assert!(
            text.contains("Send again to retry; no login needed."),
            "{text}"
        );
        assert!(!text.contains("synaps login"), "{text}");
        assert!(turn_warn(&log).contains("kind=refresh_transient"));
    }

    /// The chosen seat's login was refused: the text keeps the broker's
    /// account name and exact login command, once, and never says "no
    /// login needed".
    #[tokio::test]
    async fn chosen_seat_refused_login_keeps_the_brokers_exact_login_command() {
        use agent_core::auth::{RefreshFailure, RefreshFailureKind};
        let error = BrokerError::Refresh(RefreshFailure::new(
            RefreshFailureKind::Credential,
            "account 'claude3': invalid_grant; log it in again with \
             `synaps login --provider anthropic --account claude3`",
        ));
        let (text, log) = failed_turn_start(error).await;
        assert!(text.contains("account 'claude3'"), "{text}");
        assert_eq!(text.matches("synaps login").count(), 1, "{text}");
        assert!(
            text.contains("`synaps login --provider anthropic --account claude3`"),
            "{text}"
        );
        assert!(
            !text.contains("no login needed") && !text.contains("<label>"),
            "{text}"
        );
        assert!(turn_warn(&log).contains("kind=refresh_rejected"));
    }

    /// An unreachable broker is not a login problem.
    #[tokio::test]
    async fn unreachable_broker_gives_no_login_advice() {
        let (text, log) =
            failed_turn_start(BrokerError::Transport("connection refused".into())).await;
        assert!(text.contains("broker reachability"), "{text}");
        assert!(text.contains("no login needed"), "{text}");
        assert!(!text.contains("synaps login"), "{text}");
        assert!(turn_warn(&log).contains("kind=broker_transport"));
    }

    #[test]
    fn scrub_for_account_switch_drops_token_and_binding_but_keeps_oauth_mode() {
        let mut s = AuthState {
            auth_token: "old".into(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: Some(123),
            auth_file_mtime: None,
            bound_credential: Some("local|anthropic@old".into()),
        };
        AuthMethods::scrub_for_account_switch(&mut s);
        assert!(s.auth_token.is_empty());
        assert_eq!(s.token_expires, None);
        assert_eq!(s.bound_credential, None);
        assert_eq!(s.auth_type, "oauth");
    }

    #[test]
    fn scrub_for_remote_clears_local_credential_and_refresh_token() {
        let mut s = AuthState {
            auth_token: "local-token".into(),
            auth_type: "oauth".into(),
            refresh_token: Some("LEAKED-REFRESH".into()),
            token_expires: Some(123),
            auth_file_mtime: None,
            bound_credential: Some("local|anthropic".into()),
        };
        AuthMethods::scrub_for_remote(&mut s);
        assert!(s.auth_token.is_empty(), "access token must be cleared");
        assert_eq!(s.bound_credential, None, "binding must be cleared");
        assert_eq!(
            s.refresh_token, None,
            "refresh token must be dropped (invariant 1)"
        );
        assert_eq!(
            s.token_expires, None,
            "expiry must be cleared to force a broker fetch"
        );
        assert_eq!(
            s.auth_type, "oauth",
            "auth_type stays oauth so Local early-return is skipped"
        );
    }

    // ── Local source: auth.json rotation + 401 recovery ─────────────────────

    fn write_creds_with_mtime(base: &std::path::Path, access: &str, mtime_secs: u64) {
        let path = base.join("auth.json");
        let body = serde_json::json!({
            "anthropic": {
                "type": "oauth",
                "refresh": "rt-local",
                "access": access,
                "expires": 9_999_999_999_999u64,
            }
        });
        std::fs::write(&path, body.to_string()).unwrap();
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime_secs);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    fn unloaded_local_oauth() -> Arc<RwLock<AuthState>> {
        // Mirrors `Runtime::from_parts`: empty token, expiry 0 → first call loads.
        Arc::new(RwLock::new(AuthState {
            auth_token: String::new(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: Some(0),
            auth_file_mtime: None,
            bound_credential: None,
        }))
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn local_fast_path_reloads_when_auth_json_mtime_changes() {
        let base = crate::test_env::BaseDirGuard::new();
        let (src, cache, client) = (CredentialSource::Local, TokenCache::new(), Client::new());
        let auth = unloaded_local_oauth();

        write_creds_with_mtime(base.path(), "sk-A", 1_000_000);
        AuthMethods::refresh_if_needed(Arc::clone(&auth), &client, &src, &cache, None, RefreshPoint::TurnStart)
            .await
            .unwrap();
        assert_eq!(auth.read().await.auth_token, "sk-A");
        assert!(auth.read().await.auth_file_mtime.is_some());

        // Same mtime, different content: fast path trusts memory (no re-read).
        write_creds_with_mtime(base.path(), "sk-B", 1_000_000);
        AuthMethods::refresh_if_needed(Arc::clone(&auth), &client, &src, &cache, None, RefreshPoint::TurnStart)
            .await
            .unwrap();
        assert_eq!(
            auth.read().await.auth_token,
            "sk-A",
            "unchanged mtime must keep the one-stat fast path"
        );

        // External rotation bumps mtime → next request reloads.
        write_creds_with_mtime(base.path(), "sk-C", 2_000_000);
        AuthMethods::refresh_if_needed(Arc::clone(&auth), &client, &src, &cache, None, RefreshPoint::TurnStart)
            .await
            .unwrap();
        assert_eq!(auth.read().await.auth_token, "sk-C");
    }

    /// The mtime reload is for the BOUND account: a named seat bound by the
    /// explicit selector is re-vended by that exact credential when
    /// auth.json changes, and served from memory while it does not.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn auth_json_change_revends_the_bound_named_account() {
        let base = crate::test_env::BaseDirGuard::new();
        write_creds_with_mtime(base.path(), "unused", 1_000_000);
        let source = CredentialSource::Local;
        let auth = bound_state(&source, &work_cred(), FRESH);
        auth.write().await.auth_file_mtime = auth_file_mtime_now();
        let broker = ScriptedBroker::new(AccountSelector::Account(
            crate::auth::Account::parse("work").unwrap(),
        ));
        for _ in 0..2 {
            AuthMethods::refresh_with_broker(
                Arc::clone(&auth),
                &source,
                &broker,
                None,
                RefreshPoint::TurnStart,
            )
            .await
            .unwrap();
        }
        assert!(broker.explicit().is_empty(), "unchanged file: fast path");
        assert_eq!(auth.read().await.auth_token, "sk-HELD");

        write_creds_with_mtime(base.path(), "unused", 2_000_000);
        AuthMethods::refresh_with_broker(
            Arc::clone(&auth),
            &source,
            &broker,
            None,
            RefreshPoint::TurnStart,
        )
        .await
        .unwrap();
        assert_eq!(broker.explicit(), vec!["anthropic@work"]);
        let g = auth.read().await;
        assert_eq!(g.auth_token, "tok-explicit-anthropic@work");
        assert_eq!(g.auth_file_mtime, auth_file_mtime_now());
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn local_fast_path_keeps_token_when_auth_json_disappears() {
        let base = crate::test_env::BaseDirGuard::new();
        let (src, cache, client) = (CredentialSource::Local, TokenCache::new(), Client::new());
        let auth = unloaded_local_oauth();
        write_creds_with_mtime(base.path(), "sk-A", 1_000_000);
        AuthMethods::refresh_if_needed(
            Arc::clone(&auth),
            &client,
            &src,
            &cache,
            None,
            RefreshPoint::TurnStart,
        )
            .await
            .unwrap();
        std::fs::remove_file(base.path().join("auth.json")).unwrap();
        AuthMethods::refresh_if_needed(
            Arc::clone(&auth),
            &client,
            &src,
            &cache,
            None,
            RefreshPoint::TurnStart,
        )
            .await
            .expect("a vanished auth.json must not break a working session");
        assert_eq!(auth.read().await.auth_token, "sk-A");
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn reload_after_rejection_uses_rotated_disk_token_without_refresh() {
        let base = crate::test_env::BaseDirGuard::new();
        write_creds_with_mtime(base.path(), "sk-new", 1_000_000);
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "sk-old".into(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: Some(9_999_999_999_999),
            auth_file_mtime: None,
            bound_credential: None,
        }));
        // A client that cannot reach anything: proves no network refresh ran.
        let offline = Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:9").unwrap())
            .build()
            .unwrap();
        AuthMethods::reload_after_rejection(
            Arc::clone(&auth),
            &offline,
            &CredentialSource::Local,
            &TokenCache::new(),
            None,
        )
        .await
        .unwrap();
        let g = auth.read().await;
        assert_eq!(g.auth_token, "sk-new");
        assert_eq!(g.refresh_token, None, "engine never holds a refresh token");
        assert!(g.auth_file_mtime.is_some());
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn reload_after_rejection_forces_refresh_when_disk_token_was_rejected() {
        // Revoked-but-unexpired: auth.json still holds the exact token the
        // provider just rejected. The broker must NOT hand it back; it must
        // spend the refresh token. The client is offline, so the forced
        // refresh fails — which is the observable proof it was attempted —
        // and the rejected token is not reinstated.
        let base = crate::test_env::BaseDirGuard::new();
        write_creds_with_mtime(base.path(), "sk-revoked", 1_000_000);
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "sk-revoked".into(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: Some(9_999_999_999_999),
            auth_file_mtime: None,
            bound_credential: None,
        }));
        let offline = Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:9").unwrap())
            .build()
            .unwrap();
        let err = AuthMethods::reload_after_rejection(
            Arc::clone(&auth),
            &offline,
            &CredentialSource::Local,
            &TokenCache::new(),
            None,
        )
        .await
        .expect_err("forced refresh must be attempted (and fail offline)");
        let msg = err.to_string();
        assert!(msg.contains("Token refresh failed"), "{msg}");
        // Attempted (and failed offline), not refused by the cooldown and not
        // blamed on the login.
        assert!(!msg.contains("not refreshing again"), "{msg}");
        assert!(!msg.contains("synaps login"), "{msg}");
        for secret in ["sk-revoked", "rt-local"] {
            assert!(!msg.contains(secret), "error leaked a credential: {msg}");
        }
        let g = auth.read().await;
        assert!(
            g.auth_token.is_empty(),
            "rejected token must not be reinstated"
        );
        assert_eq!(g.token_expires, None);
        // Failed refresh persists nothing.
        let disk = std::fs::read_to_string(base.path().join("auth.json")).unwrap();
        assert!(disk.contains("sk-revoked"));
    }

    /// Only an unusable login says "run `synaps login`"; cooldown, busy lock
    /// and network/provider failures each say what actually happened.
    #[test]
    fn recovery_error_only_blames_the_login_for_credential_failures() {
        use crate::auth::{BrokerError, RefreshFailure, RefreshFailureKind as K};
        let cred = work_cred();
        let msg = |kind, text: &str| {
            recovery_error(&cred, &BrokerError::Refresh(RefreshFailure::new(kind, text))).to_string()
        };
        let cooldown = msg(K::Cooldown { retry_in_secs: 30 }, "not refreshing again for 30s");
        assert!(cooldown.contains("not refreshing again for 30s"), "{cooldown}");
        let busy = msg(K::LockBusy, "timed out waiting for another process");
        assert!(busy.contains("still refreshing"), "{busy}");
        let net = msg(K::Transient, "Token refresh request failed: connect");
        assert!(net.contains("network or provider error"), "{net}");
        for m in [&cooldown, &busy, &net] {
            assert!(!m.contains("synaps login"), "{m}");
        }
        let cred_fail = msg(K::Credential, "invalid_grant: revoked");
        assert!(cred_fail.contains("synaps login --account work"), "{cred_fail}");
        assert!(cred_fail.contains("invalid_grant"), "{cred_fail}");
    }

    /// m8: core texts for a missing / removed slot (and provider labels such
    /// as Kimi's or Muse's) already say which `synaps login` to run; the
    /// runtime must not append a second copy.
    #[test]
    fn login_advice_is_printed_once() {
        use crate::auth::{BrokerError, RefreshFailure, RefreshFailureKind as K};
        let cred = work_cred();
        let once = |m: &str| {
            assert_eq!(m.matches("synaps login").count(), 1, "{m}");
            assert!(!m.contains(".."), "{m}");
        };
        let missing = "No credentials for anthropic@work at /x/auth.json. Run `synaps login \
                       --provider anthropic --account work`.";
        for e in [
            BrokerError::Refresh(RefreshFailure::new(K::Credential, missing)),
            BrokerError::Refresh(RefreshFailure::new(
                K::Credential,
                "credential was removed while its token was being refreshed; run `synaps login`",
            )),
            BrokerError::Credential(missing.into()),
        ] {
            once(&recovery_error(&cred, &e).to_string());
            once(&vend_error(&cred, &e).to_string());
        }
        // Without advice in the reason, the runtime still adds it.
        let plain = BrokerError::Refresh(RefreshFailure::new(K::Credential, "invalid_grant: revoked"));
        let m = recovery_error(&cred, &plain).to_string();
        assert!(m.contains("synaps login --account work"), "{m}");
        let m = vend_error(&cred, &BrokerError::Credential("invalid_grant".into())).to_string();
        assert!(m.contains("synaps login --account work"), "{m}");
        assert!(m.contains("broker reachability"), "{m}");
    }

    /// Round-2 I2: when the store itself is unusable (corrupt), login would
    /// fail too, so neither recovery nor vend tells the user to log in.
    #[test]
    fn action_required_failures_never_send_the_user_to_login() {
        use crate::auth::{BrokerError, RefreshFailure, RefreshFailureKind as K};
        let cred = work_cred();
        let text = "/x/auth.json is not valid JSON (eof). Move it aside with \
                    `mv '/x/auth.json' '/x/auth.json.broken-1'`, then log in again.";
        let e = BrokerError::Refresh(RefreshFailure::new(K::ActionRequired, text));
        for m in [recovery_error(&cred, &e).to_string(), vend_error(&cred, &e).to_string()] {
            assert!(m.contains("mv '/x/auth.json'"), "{m}");
            assert!(!m.contains("synaps login"), "{m}");
            assert!(!m.contains("retry"), "{m}");
        }
    }

    /// A refused recovery is still a terminal auth error, but it says WHY:
    /// another process refreshed this account seconds ago (stamp in its
    /// lock file), so the cooldown's "not refreshing again" reason is shown
    /// — with no token values.
    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn recovery_error_carries_the_cooldown_reason() {
        let base = crate::test_env::BaseDirGuard::new();
        write_creds_with_mtime(base.path(), "sk-revoked", 1_000_000);
        std::fs::write(
            base.path().join("auth.json.anthropic.refresh.lock"),
            crate::epoch_millis().to_string(),
        )
        .unwrap();
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "sk-revoked".into(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: Some(9_999_999_999_999),
            auth_file_mtime: None,
            bound_credential: Some(anthropic_binding(
                &CredentialSource::Local,
                &CredentialRef::default_for(OAuthProviderId::Anthropic),
            )),
        }));
        let err = AuthMethods::recover_local_rejection(
            &auth,
            &offline_client(),
            &CredentialSource::Local,
            &TokenCache::new(),
            auth.read().await.bound_credential.clone().as_deref(),
            "sk-revoked",
        )
        .await
        .expect_err("cooldown refuses the forced refresh");
        let msg = err.to_string();
        assert!(msg.contains("not refreshing again"), "{msg}");
        assert!(!msg.contains("synaps login"), "a cooldown is not a login problem: {msg}");
        for secret in ["sk-revoked", "rt-local"] {
            assert!(!msg.contains(secret), "error leaked a credential: {msg}");
        }
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn late_401_for_old_token_reuses_concurrent_reload_and_spends_nothing() {
        // Stream A already reloaded to sk-new (memory + disk). Stream B's 401
        // was for sk-old. B must reuse sk-new — NOT report it as rejected,
        // which would force a second refresh-token spend.
        let base = crate::test_env::BaseDirGuard::new();
        write_creds_with_mtime(base.path(), "sk-new", 1_000_000);
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "sk-new".into(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: Some(9_999_999_999_999),
            auth_file_mtime: None,
            bound_credential: None,
        }));
        let offline = Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:9").unwrap())
            .build()
            .unwrap();
        AuthMethods::reload_after_rejection(
            Arc::clone(&auth),
            &offline,
            &CredentialSource::Local,
            &TokenCache::new(),
            Some("sk-old"),
        )
        .await
        .expect("a concurrent reload already fixed it: no refresh attempted");
        assert_eq!(auth.read().await.auth_token, "sk-new");
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn late_401_while_other_reload_in_flight_uses_sent_token_as_rejected() {
        // Memory is empty (A's reload in flight) but disk already has sk-new.
        // B reports the token it SENT (sk-old): disk differs → reuse, no refresh.
        let base = crate::test_env::BaseDirGuard::new();
        write_creds_with_mtime(base.path(), "sk-new", 1_000_000);
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: String::new(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: None,
            auth_file_mtime: None,
            bound_credential: None,
        }));
        let offline = Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:9").unwrap())
            .build()
            .unwrap();
        AuthMethods::reload_after_rejection(
            Arc::clone(&auth),
            &offline,
            &CredentialSource::Local,
            &TokenCache::new(),
            Some("sk-old"),
        )
        .await
        .unwrap();
        assert_eq!(auth.read().await.auth_token, "sk-new");
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn failed_forced_refresh_then_user_relogin_recovers_next_request() {
        let base = crate::test_env::BaseDirGuard::new();
        write_creds_with_mtime(base.path(), "sk-revoked", 1_000_000);
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "sk-revoked".into(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: Some(9_999_999_999_999),
            auth_file_mtime: None,
            bound_credential: None,
        }));
        let (src, cache) = (CredentialSource::Local, TokenCache::new());
        let offline = Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:9").unwrap())
            .build()
            .unwrap();
        assert!(AuthMethods::reload_after_rejection(
            Arc::clone(&auth),
            &offline,
            &src,
            &cache,
            Some("sk-revoked")
        )
        .await
        .is_err());
        write_creds_with_mtime(base.path(), "sk-fixed", 2_000_000); // `synaps login`
        AuthMethods::refresh_if_needed(
            Arc::clone(&auth),
            &offline,
            &src,
            &cache,
            None,
            RefreshPoint::TurnStart,
        )
            .await
            .unwrap();
        assert_eq!(auth.read().await.auth_token, "sk-fixed");
    }

    #[test]
    fn auth_state_debug_redacts_tokens() {
        let s = AuthState {
            auth_token: "sk-SECRET".into(),
            auth_type: "oauth".into(),
            refresh_token: Some("rt-SECRET".into()),
            token_expires: Some(1),
            auth_file_mtime: None,
            bound_credential: None,
        };
        let d = format!("{s:?}");
        assert!(!d.contains("SECRET"), "{d}");
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn reload_after_rejection_remote_invalidates_cache_and_refetches() {
        let _config = crate::test_env::BaseDirGuard::new();
        let url = spawn_broker(r#"{"access_token":"sk-remote-new","expires":9999999999999}"#).await;
        let source = CredentialSource::Remote {
            endpoint: url,
            machine_token: "m".into(),
        };
        let cache = TokenCache::new();
        cache.put(
            "anthropic",
            crate::auth::BrokerToken {
                access_token: "sk-remote-old".into(),
                expires: 9_999_999_999_999,
                ttl_ms: None,
            },
        );
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "sk-remote-old".into(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: Some(9_999_999_999_999),
            auth_file_mtime: None,
            bound_credential: None,
        }));
        AuthMethods::reload_after_rejection(
            Arc::clone(&auth),
            &Client::new(),
            &source,
            &cache,
            None,
        )
        .await
        .unwrap();
        let g = auth.read().await;
        assert_eq!(g.auth_token, "sk-remote-new");
        assert_eq!(g.auth_file_mtime, None, "remote never tracks a local mtime");
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn reload_after_rejection_remote_ignores_sent_shortcut_and_refetches() {
        let _config = crate::test_env::BaseDirGuard::new();
        // Remote behaviour is unchanged from the pre-existing C1 path: even if
        // shared state already holds a different token, invalidate + refetch.
        let url = spawn_broker(r#"{"access_token":"sk-remote-new","expires":9999999999999}"#).await;
        let source = CredentialSource::Remote {
            endpoint: url,
            machine_token: "m".into(),
        };
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "sk-other".into(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: Some(9_999_999_999_999),
            auth_file_mtime: None,
            bound_credential: None,
        }));
        AuthMethods::reload_after_rejection(
            Arc::clone(&auth),
            &Client::new(),
            &source,
            &TokenCache::new(),
            Some("sk-sent"),
        )
        .await
        .unwrap();
        assert_eq!(auth.read().await.auth_token, "sk-remote-new");
    }

    #[tokio::test]
    async fn reload_after_rejection_local_api_key_is_rejected_not_reloaded() {
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "key".into(),
            auth_type: "api_key".into(),
            refresh_token: None,
            token_expires: None,
            auth_file_mtime: None,
            bound_credential: None,
        }));
        let err = AuthMethods::reload_after_rejection(
            Arc::clone(&auth),
            &Client::new(),
            &CredentialSource::Local,
            &TokenCache::new(),
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("nothing to reload"), "{err}");
        assert_eq!(auth.read().await.auth_token, "key", "state untouched");
    }

    /// Write Anthropic OAuth creds with an explicit expiry (ms since epoch).
    fn write_creds_expiring(base: &std::path::Path, access: &str, expires_ms: u64) {
        let body = serde_json::json!({
            "anthropic": {
                "type": "oauth",
                "refresh": "rt-local",
                "access": access,
                "expires": expires_ms,
            }
        });
        std::fs::write(base.join("auth.json"), body.to_string()).unwrap();
    }

    /// Stand-in for ANOTHER synaps process mid-refresh: hold the per-provider
    /// cross-process refresh flock (flock is per open file description, so
    /// this contends exactly like a foreign process would).
    fn hold_foreign_refresh_lock(base: &std::path::Path) -> std::fs::File {
        use fs4::fs_std::FileExt;
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(base.join("auth.json.anthropic.refresh.lock"))
            .unwrap();
        assert!(FileExt::try_lock_exclusive(&f).unwrap());
        f
    }

    fn offline_client() -> Client {
        Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:9").unwrap())
            .build()
            .unwrap()
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn forced_refresh_waits_on_cross_process_lock_and_adopts_rotated_token() {
        // Revocation hits two processes at once. The other one holds the
        // refresh lock and rotates auth.json; we must wait for it and adopt
        // its token instead of spending the same refresh token. The client
        // is offline, so success proves no network refresh happened here.
        let base = crate::test_env::BaseDirGuard::new();
        write_creds_expiring(base.path(), "sk-revoked", 9_999_999_999_999);
        let lock = hold_foreign_refresh_lock(base.path());
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "sk-revoked".into(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: Some(9_999_999_999_999),
            auth_file_mtime: None,
            bound_credential: None,
        }));
        let task = {
            let auth = Arc::clone(&auth);
            tokio::spawn(async move {
                AuthMethods::reload_after_rejection(
                    auth,
                    &offline_client(),
                    &CredentialSource::Local,
                    &TokenCache::new(),
                    Some("sk-revoked"),
                )
                .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !task.is_finished(),
            "must wait for the foreign refresh lock"
        );
        write_creds_expiring(base.path(), "sk-rotated", 9_999_999_999_999);
        drop(lock);
        task.await.unwrap().expect("adopts the rotated token");
        assert_eq!(auth.read().await.auth_token, "sk-rotated");
    }

    #[tokio::test]
    #[serial_test::serial(synaps_base_dir)]
    async fn expiry_refresh_waits_on_cross_process_lock_and_adopts_rotated_token() {
        // Two processes whose token expires together: the other one holds
        // the refresh lock and rotates auth.json. The normal expiry path must
        // wait and adopt its token rather than spend the same refresh token.
        // Offline client: success proves no network refresh happened here.
        let base = crate::test_env::BaseDirGuard::new();
        write_creds_expiring(base.path(), "sk-expired", 1_000);
        let lock = hold_foreign_refresh_lock(base.path());
        let auth = unloaded_local_oauth();
        let task = {
            let auth = Arc::clone(&auth);
            tokio::spawn(async move {
                AuthMethods::refresh_if_needed(
                    auth,
                    &offline_client(),
                    &CredentialSource::Local,
                    &TokenCache::new(),
                    None,
                    RefreshPoint::TurnStart,
                )
                .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !task.is_finished(),
            "expiry refresh must wait for the foreign refresh lock"
        );
        write_creds_expiring(base.path(), "sk-rotated", 9_999_999_999_999);
        drop(lock);
        task.await.unwrap().expect("adopts the rotated token");
        assert_eq!(auth.read().await.auth_token, "sk-rotated");
    }
}
