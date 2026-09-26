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
        // Fast path: in-memory token still fresh AND still the selected account?
        let bound: Option<String> = {
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
                if still_fresh && binding == BindingCheck::Matches {
                    return Ok(());
                }
            }
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
        // Read lock dropped here

        tracing::info!("Refreshing auth token via credential broker");
        let pinned: PinnedToken = match &selector {
            AccountSelector::Account(account) => {
                // Explicit: exactly this credential, never a fallback.
                let cred = CredentialRef::new(OAuthProviderId::Anthropic, account.clone());
                let token = broker.access_token_for(&cred).await.map_err(|e| {
                    RuntimeError::Auth(format!(
                        "Token refresh failed for account '{}': {}. Run `synaps login` (or `synaps login --account {}`) to re-authenticate, or check auth.remote_endpoint / broker reachability.",
                        cred.account, e, cred.account
                    ))
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
                return Err(RuntimeError::Auth(format!(
                    "Anthropic account selector from {source} is invalid ({reason}); refusing to fall back to another account. Fix it or run `synaps auth use --provider anthropic --account <label|default>`."
                )));
            }
        };
        // Update shared auth state so all clones (including spawned stream
        // tasks) immediately see the fresh token. Never a refresh token.
        Self::bind_pinned(&auth, source, pinned).await;

        Ok(())
    }

    /// Bind the in-memory Anthropic token to `pinned` — the seat the broker
    /// just vended for THIS source — exactly as [`Self::refresh_with_broker`]
    /// does after a vend. This is the ONE sanctioned mid-turn re-pin: the
    /// request loop calls it after a gated account failover (recognized
    /// window exhaustion under `Auto`, nothing streamed yet), so the rest of
    /// the turn keeps the new seat through the normal `WithinTurn` fast path
    /// and the next turn boundary re-selects as usual. Never a refresh token.
    pub(super) async fn bind_pinned(
        auth: &Arc<RwLock<AuthState>>,
        source: &CredentialSource,
        pinned: PinnedToken,
    ) {
        let binding = anthropic_binding(source, &pinned.credential);
        let mut auth_guard = auth.write().await;
        auth_guard.auth_token = pinned.token.token;
        auth_guard.auth_type = "oauth".to_string();
        auth_guard.refresh_token = None;
        auth_guard.token_expires = Some(pinned.token.expires);
        auth_guard.bound_credential = Some(binding);
    }

    /// `Auto` vend. At a turn boundary (or with no seat pinned on this
    /// source) the broker's capacity selector picks the seat. Mid-turn, the
    /// seat this turn already pinned is re-vended by explicit credential —
    /// a stale token is a refresh event, not a re-selection event — so a
    /// long agentic turn keeps one account (and its prompt cache) instead
    /// of hopping to whichever seat now has the lowest utilization, and no
    /// per-round usage fetches are spent. Only if THAT seat can no longer be
    /// vended does Auto fall back to re-selection (Auto opted in to switching;
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
        broker
            .access_token_pinned_for(OAuthProviderId::Anthropic, model.map(quota_model_key))
            .await
            .map_err(|e| {
                RuntimeError::Auth(format!(
                    "Automatic account selection failed: {e}. Run `synaps login`, pick an account with `synaps auth use --provider anthropic --account <label>`, or check auth.remote_endpoint / broker reachability."
                ))
            })
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
            bound_credential: None,
        }))
    }

    #[tokio::test]
    async fn remote_source_fetches_and_populates_auth_state() {
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
    async fn remote_source_fast_path_when_token_still_fresh() {
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
    async fn remote_fast_path_refetches_a_token_inside_the_margin() {
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
    async fn switched_account_binding_is_never_served_from_the_fast_path() {
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
    async fn matching_binding_serves_cached_token() {
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
    async fn unbound_fresh_token_is_rebound_through_the_broker() {
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
    }

    impl ScriptedBroker {
        fn new(selector: AccountSelector) -> Self {
            Self {
                selector,
                explicit_calls: Mutex::new(Vec::new()),
                auto_calls: Mutex::new(Vec::new()),
                explicit_denied: Vec::new(),
                auto_pick: "auto-pick",
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

    #[test]
    fn scrub_for_account_switch_drops_token_and_binding_but_keeps_oauth_mode() {
        let mut s = AuthState {
            auth_token: "old".into(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: Some(123),
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
}
