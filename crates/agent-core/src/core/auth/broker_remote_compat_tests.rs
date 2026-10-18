//! `RemoteBroker::usage` against a broker that predates `/usage/snapshot`
//! (0.10.2). Its router has no such route, so axum answers an empty 404.

use super::*;
use crate::core::auth::{Account, CredentialRef, OAuthProviderId};

const LEGACY_USAGE: &str =
    r#"{"five_hour":{"utilization":12.5,"resets_at":"2025-09-20T22:00:00Z"}}"#;

/// `/capabilities` rows without account lists, as 0.10.2 serves them.
fn capabilities_without_accounts() -> String {
    serde_json::to_string(&[ProviderStatus {
        key: "anthropic".into(),
        name: "Anthropic".into(),
        kind: CredentialKind::OAuth,
        configured: true,
        accounts: Vec::new(),
    }])
    .unwrap()
}

/// 0.10.2's broker shape: `/usage` (default slot only) and `/capabilities`
/// without account rows; no `/usage/snapshot`.
async fn spawn_old_broker() -> String {
    use axum::routing::get;
    let caps = capabilities_without_accounts();
    let app = axum::Router::new()
        .route("/usage", get(|| async { LEGACY_USAGE }))
        .route("/capabilities", get(move || async move { caps }));
    spawn(app).await
}

async fn spawn(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn remote(endpoint: String) -> RemoteBroker {
    RemoteBroker::new(
        endpoint,
        "opaque-machine-token",
        reqwest::Client::new(),
        super::super::TokenCache::new(),
    )
}

#[tokio::test]
async fn default_account_falls_back_to_legacy_usage_on_an_old_broker() {
    let broker = remote(spawn_old_broker().await);
    let cred = CredentialRef::new(OAuthProviderId::Anthropic, Account::Default);
    let snapshot = broker.usage(&cred).await.expect("legacy /usage fallback");
    assert_eq!(snapshot.provider, "anthropic");
    assert_eq!(snapshot.account, "default");
    assert_eq!(
        snapshot.window("five_hour").unwrap().used_percent.valid(),
        Some(12.5)
    );
}

#[tokio::test]
async fn named_account_on_an_old_broker_says_the_broker_is_too_old() {
    let broker = remote(spawn_old_broker().await);
    let cred = CredentialRef::new(OAuthProviderId::Anthropic, Account::named("work").unwrap());
    let err = broker.usage(&cred).await.unwrap_err();
    assert!(matches!(err, BrokerError::BrokerTooOld(_)), "{err:?}");
    let text = err.to_string();
    assert!(
        text.contains("broker is older than this client")
            && text.contains("per-account usage unavailable"),
        "{text}"
    );
}

#[tokio::test]
async fn a_current_broker_404_is_still_an_unknown_account() {
    use axum::{http::StatusCode, routing::get, Json};
    let app = axum::Router::new()
        .route(
            "/usage/snapshot",
            get(|| async {
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "unknown account"})),
                )
            }),
        )
        .route("/usage", get(|| async { LEGACY_USAGE }));
    let broker = remote(spawn(app).await);
    for account in [Account::Default, Account::named("work").unwrap()] {
        let cred = CredentialRef::new(OAuthProviderId::Anthropic, account);
        let err = broker.usage(&cred).await.unwrap_err();
        assert!(matches!(err, BrokerError::UnknownAccount { .. }), "{err:?}");
    }
}

#[tokio::test]
async fn account_listing_on_an_old_broker_says_the_broker_is_too_old() {
    let broker = remote(spawn_old_broker().await);
    let err = broker.accounts(OAuthProviderId::Anthropic).await.unwrap_err();
    assert!(matches!(err, BrokerError::BrokerTooOld(_)), "{err:?}");
    assert!(err.to_string().contains("account listing unavailable"), "{err}");
}

#[tokio::test]
async fn an_empty_listing_on_a_current_broker_is_just_empty() {
    use axum::{http::StatusCode, routing::get, Json};
    let caps = capabilities_without_accounts();
    let app = axum::Router::new()
        .route("/capabilities", get(move || async move { caps }))
        .route(
            "/usage/snapshot",
            get(|| async {
                (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "invalid account"})),
                )
            }),
        );
    let broker = remote(spawn(app).await);
    assert!(broker.accounts(OAuthProviderId::Anthropic).await.unwrap().is_empty());
}

/// A proxy that blanks error bodies but keeps a JSON content type must not
/// be mistaken for a missing route.
#[tokio::test]
async fn an_empty_json_404_is_not_a_missing_route() {
    use axum::{http::StatusCode, routing::get};
    let app = axum::Router::new()
        .route(
            "/usage/snapshot",
            get(|| async {
                (StatusCode::NOT_FOUND, [("content-type", "application/json")], "")
            }),
        )
        .route("/usage", get(|| async { LEGACY_USAGE }));
    let broker = remote(spawn(app).await);
    let cred = CredentialRef::new(OAuthProviderId::Anthropic, Account::Default);
    let err = broker.usage(&cred).await.unwrap_err();
    assert!(matches!(err, BrokerError::UnknownAccount { .. }), "{err:?}");
}
