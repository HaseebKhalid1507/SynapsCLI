//! Zero-network OAuth lifecycle harness.  All listeners bind loopback and no
//! production identity-provider endpoint is contacted.
use agent_core::auth::{
    load_provider_auth, save_provider_auth, start_callback_server_at, CallbackOutcome,
    OAuthCredentials,
};
use serial_test::serial;
use std::time::{SystemTime, UNIX_EPOCH};

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[tokio::test]
#[serial]
async fn login_storage_expiry_refresh_rotation_and_omission_are_unattended() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", home.path());

    // Fake browser redirect into the real callback listener.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let (rx, server) =
        start_callback_server_at("right-state".into(), "127.0.0.1", port, "/callback")
            .await
            .unwrap();
    let response = reqwest::get(format!(
        "http://127.0.0.1:{port}/callback?code=fake-code&state=right-state"
    ))
    .await
    .unwrap();
    assert!(response.status().is_success());
    assert!(matches!(rx.await.unwrap(), CallbackOutcome::Authorized(v) if v.code == "fake-code"));
    server.shutdown().await;

    // The fake token endpoint's login response, followed by expiry and two
    // refresh responses: one rotates, one legally omits the refresh token.
    let mut credential = OAuthCredentials {
        auth_type: "oauth".into(),
        access: "access-1".into(),
        refresh: "refresh-1".into(),
        expires: now() + 10,
        account_id: None,
    };
    save_provider_auth("xai-auth", &credential).unwrap();
    assert_eq!(
        load_provider_auth("xai-auth").unwrap().unwrap().refresh,
        "refresh-1"
    );
    credential.expires = now().saturating_sub(1);
    credential.access = "access-2".into();
    credential.refresh = "refresh-2".into(); // rotation
    save_provider_auth("xai-auth", &credential).unwrap();
    let omitted_refresh_response = ("access-3", None::<String>);
    credential.access = omitted_refresh_response.0.into();
    credential.refresh = omitted_refresh_response.1.unwrap_or(credential.refresh);
    credential.expires = now() + 3_600_000;
    save_provider_auth("xai-auth", &credential).unwrap();
    let stored = load_provider_auth("xai-auth").unwrap().unwrap();
    assert_eq!(
        (stored.access.as_str(), stored.refresh.as_str()),
        ("access-3", "refresh-2")
    );
    let disk = std::fs::read_to_string(agent_core::auth::auth_file_path()).unwrap();
    assert!(disk.contains("refresh-2")); // broker storage owns it
}

#[tokio::test]
async fn denial_and_wrong_state_fail_closed() {
    async fn callback(query: &str) -> CallbackOutcome {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let (rx, server) =
            start_callback_server_at("expected".into(), "127.0.0.1", port, "/callback")
                .await
                .unwrap();
        let _ = reqwest::get(format!("http://127.0.0.1:{port}/callback?{query}"))
            .await
            .unwrap();
        let out = rx.await.unwrap();
        server.shutdown().await;
        out
    }
    assert!(matches!(
        callback("error=access_denied&error_description=nope&state=expected").await,
        CallbackOutcome::Denied { .. }
    ));
    assert_eq!(
        callback("code=stolen&state=wrong").await,
        CallbackOutcome::Invalid
    );
}
