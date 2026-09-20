use super::types::AuthState;
use crate::auth::{
    broker_from_source, is_expired_with_margin, AccountSelector, CredentialRef, CredentialSource,
    OAuthProviderId, PinnedToken, TokenCache, DEFAULT_MARGIN_MS,
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

/// Identity of the credential an in-memory Anthropic token was vended for:
/// the credential source kind plus the credential's storage key. Two
/// installations can both hold `anthropic@work`, so the source is part of
/// the binding; the Remote machine token is NOT (it is the client's own
/// secret and must never appear in state or logs) — the endpoint URL alone
/// distinguishes brokers.
pub(super) fn anthropic_binding(source: &CredentialSource, cred: &CredentialRef) -> String {
    match source {
        CredentialSource::Local => format!("local|{}", cred.storage_key()),
        CredentialSource::Remote { endpoint, .. } => {
            format!("remote:{endpoint}|{}", cred.storage_key())
        }
    }
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
/// turn started on while its token is fresh, so one turn does not hop
/// between accounts and lose its prompt cache.
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
            // Mid-turn: keep whatever seat this turn pinned, if still fresh.
            (RefreshPoint::WithinTurn, Some(_)) => BindingCheck::Matches,
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
    /// names that credential on this source; a switch (config `auth use`,
    /// env override, source change) scrubs and refetches, never silently
    /// keeping the previous account. `Auto` never serves a cached token
    /// across turns: the broker re-selects on every call so a seat whose
    /// capacity was consumed since the last turn is not kept merely because
    /// its token is unexpired. `model` lets the Auto selector require capacity
    /// for the model actually being requested.
    pub(super) async fn refresh_if_needed(
        auth: Arc<RwLock<AuthState>>,
        client: &Client,
        source: &CredentialSource,
        cache: &TokenCache,
        model: Option<&str>,
        point: RefreshPoint,
    ) -> Result<()> {
        // Fast path: in-memory token still fresh AND still the selected account?
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
        let selector = broker.account_selector(OAuthProviderId::Anthropic);
        {
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
        }
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
            AccountSelector::Auto => broker
                .access_token_pinned_for(OAuthProviderId::Anthropic, model.map(quota_model_key))
                .await
                .map_err(|e| {
                    RuntimeError::Auth(format!(
                        "Automatic account selection failed: {e}. Run `synaps login`, pick an account with `synaps auth use --provider anthropic --account <label>`, or check auth.remote_endpoint / broker reachability."
                    ))
                })?,
            AccountSelector::Invalid { source, reason } => {
                return Err(RuntimeError::Auth(format!(
                    "Anthropic account selector from {source} is invalid ({reason}); refusing to fall back to another account. Fix it or run `synaps auth use --provider anthropic --account <label|default>`."
                )));
            }
        };
        let binding = anthropic_binding(source, &pinned.credential);

        // Update shared auth state so all clones (including spawned stream
        // tasks) immediately see the fresh token. Never a refresh token.
        {
            let mut auth_guard = auth.write().await;
            auth_guard.auth_token = pinned.token.token;
            auth_guard.auth_type = "oauth".to_string();
            auth_guard.refresh_token = None;
            auth_guard.token_expires = Some(pinned.token.expires);
            auth_guard.bound_credential = Some(binding);
        }

        Ok(())
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
        let auth = Arc::new(RwLock::new(AuthState {
            auth_token: "sk-OLD-ACCOUNT".into(),
            auth_type: "oauth".into(),
            refresh_token: None,
            token_expires: Some(9_999_999_999_999),
            bound_credential: Some(format!("remote:{url}|anthropic@old")),
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
            Some(format!("remote:{url}|anthropic").as_str()),
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
            bound_credential: Some(format!("remote:{url}|anthropic")),
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
            assert!(!opus_window.applies_to(Some(key)), "{id} must not hit the opus window");
        }
        // Codex slugs are exact and untouched.
        assert_eq!(quota_model_key("gpt-5.6-sol"), "gpt-5.6-sol");
        assert!(model_matches("gpt-5.6-sol", quota_model_key("gpt-5.6-sol")));
        assert!(!model_matches("gpt-5.6-luna", quota_model_key("gpt-5.6-sol")));
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
        assert_eq!(b_remote, "remote:https://broker.example:8181|anthropic@work");
        assert!(!b_remote.contains("MACHINE-SECRET"));
        let b_local = anthropic_binding(&CredentialSource::Local, &cred);
        assert_eq!(b_local, "local|anthropic@work");
        assert_ne!(b_local, b_remote, "same slot on different sources is a different credential");
        let other_broker = CredentialSource::Remote {
            endpoint: "https://other.example".into(),
            machine_token: "MACHINE-SECRET".into(),
        };
        assert_ne!(anthropic_binding(&other_broker, &cred), b_remote);
    }

    #[test]
    fn check_binding_matrix() {
        use crate::auth::Account;
        let local = CredentialSource::Local;
        let explicit_default = AccountSelector::Account(Account::Default);
        let explicit_work = AccountSelector::Account(Account::parse("work").unwrap());
        use RefreshPoint::{TurnStart, WithinTurn};
        assert_eq!(
            check_binding(&local, &explicit_default, Some("local|anthropic"), TurnStart),
            BindingCheck::Matches
        );
        assert_eq!(
            check_binding(&local, &explicit_work, Some("local|anthropic"), TurnStart),
            BindingCheck::Switched
        );
        assert_eq!(
            check_binding(&local, &explicit_work, Some("local|anthropic@work"), WithinTurn),
            BindingCheck::Matches
        );
        assert_eq!(
            check_binding(&local, &explicit_default, None, TurnStart),
            BindingCheck::Switched
        );
        // Auto never trusts a cross-turn binding, even a matching one …
        assert_eq!(
            check_binding(&local, &AccountSelector::Auto, Some("local|anthropic"), TurnStart),
            BindingCheck::AutoRepin
        );
        // … but keeps the seat a turn started on for that turn's later rounds.
        assert_eq!(
            check_binding(&local, &AccountSelector::Auto, Some("local|anthropic"), WithinTurn),
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
            check_binding(&remote, &explicit_default, Some("local|anthropic"), TurnStart),
            BindingCheck::Switched
        );
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
