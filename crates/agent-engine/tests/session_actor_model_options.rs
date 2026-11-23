//! `SessionQuery::ModelOptions` (feature `model_options`): the session's
//! current model and level, the models a client may `Set`, and the levels
//! the runtime would accept for the current model; a level it lists applies.

mod session_actor_common;
use session_actor_common::*;

use agent_engine::session::model_options::ModelOptions;
use agent_engine::session::{
    AttachMode, ClientKind, ClientMeta, ClientTransport, LocalTransport, SessionCommand,
    SessionEventWire, SessionQuery, SessionSetting,
};
use serial_test::serial;

struct EnvGuard(&'static str);
impl EnvGuard {
    fn set(k: &'static str, v: &str) -> Self {
        std::env::set_var(k, v);
        Self(k)
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        std::env::remove_var(self.0);
    }
}

async fn options(t: &mut LocalTransport, id: u64) -> ModelOptions {
    t.send_from_self(SessionCommand::Query {
        id,
        query: SessionQuery::ModelOptions,
    })
    .await
    .unwrap();
    let seen = until(t, |e| matches!(e, SessionEventWire::QueryResult { id: i, .. } if *i == id)).await;
    let SessionEventWire::QueryResult { value, .. } = &seen.last().unwrap().event else {
        unreachable!()
    };
    serde_json::from_value(value.clone()).expect("the pinned shape")
}

/// A remote credential broker that serves OpenAI Codex only.
async fn broker_serving_codex() -> String {
    let app = axum::Router::new().route(
        "/capabilities",
        axum::routing::get(|| async {
            axum::Json(serde_json::json!([
                {"key": "anthropic", "name": "Anthropic", "kind": "o_auth", "configured": false},
                {"key": "openai-codex", "name": "OpenAI Codex", "kind": "o_auth", "configured": true}
            ]))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// The rows are what THIS session's credential source serves: bound to a
/// remote broker that serves OpenAI Codex, the session lists Codex models,
/// and never Anthropic's on the strength of a local `ANTHROPIC_API_KEY` a
/// remote session never uses. The current model keeps its own section.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn model_options_come_from_the_sessions_credential_source() {
    let _h = Home::new();
    let _key = EnvGuard::set("ANTHROPIC_API_KEY", "sk-ant-local-only");
    let _ep = EnvGuard::set("SYNAPS_AUTH_ENDPOINT", &broker_serving_codex().await);
    let _tok = EnvGuard::set("SYNAPS_MACHINE_TOKEN", "machine-token");
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let (mut t, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui)).await.unwrap();
    let o = options(&mut t, 5).await;
    assert!(o.models.iter().any(|m| m.provider_key == "openai-codex"), "{:?}", o.models);
    let anthropic: Vec<_> = o.models.iter().filter(|m| m.provider_key == "anthropic").collect();
    assert_eq!(anthropic.len(), 1, "only the current model: {anthropic:?}");
    assert_eq!((anthropic[0].id.as_str(), anthropic[0].provider.as_str()), (MODEL, "Anthropic"));
    end(&mut t).await;
}

/// As `catalog_page`'s reply reaches only the connection that asked, the
/// model options reach only the client that asked: another client of the
/// same session (an older one never asks) never receives them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn model_options_reach_only_the_client_that_asked() {
    let _h = Home::new();
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let (mut asker, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui)).await.unwrap();
    let (mut other, _) =
        LocalTransport::attach_with(handle.clone(), ClientMeta::new(ClientKind::Tui), AttachMode::Mirror)
            .await
            .unwrap();
    let o = options(&mut asker, 5).await;
    assert_eq!(o.model, MODEL);
    // The other client's own query is answered after it: everything the
    // session sent before that answer has reached it, and none was ours.
    other
        .send_from_self(SessionCommand::Query { id: 6, query: SessionQuery::View })
        .await
        .unwrap();
    let seen = until(&mut other, |e| matches!(e, SessionEventWire::QueryResult { id: 6, .. })).await;
    assert!(
        !seen.iter().any(|e| matches!(e.event, SessionEventWire::QueryResult { id: 5, .. } | SessionEventWire::QueryReply { .. })),
        "another client received the options"
    );
    end(&mut asker).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn model_options_lists_what_set_takes() {
    let _h = Home::new();
    let host = host().await;
    let handle = host.create_session(cfg()).await.unwrap();
    let (mut t, snap) = LocalTransport::attach_with(
        handle.clone(),
        ClientMeta::new(ClientKind::Tui),
        AttachMode::Mirror,
    )
    .await
    .unwrap();
    let o = options(&mut t, 5).await;
    assert_eq!(o.model, snap.view.model);
    assert_eq!(o.thinking_level, snap.view.thinking_level);
    // The current model is always a row, even with no credential in this home.
    assert!(
        o.models
            .iter()
            .any(|m| m.id == o.model || m.id.ends_with(&format!("/{}", o.model))),
        "{:?}",
        o.models
    );
    assert!(!o.thinking.is_empty(), "some level applies");
    // Every listed level applies through `Set`.
    for (k, level) in o.thinking.iter().enumerate() {
        if level == "ultracode" {
            continue; // needs orchestration tools this bare host may not have
        }
        let id = 100 + k as u64;
        let level = agent_core::reasoning::ReasoningLevel::parse(level).unwrap();
        t.send_from_self(SessionCommand::Set {
            id,
            setting: SessionSetting::ReasoningLevel { level },
        })
        .await
        .unwrap();
        let seen = until(
            &mut t,
            |e| matches!(e, SessionEventWire::SettingChanged(a) if a.id == id),
        )
        .await;
        let SessionEventWire::SettingChanged(a) = &seen.last().unwrap().event else {
            unreachable!()
        };
        assert!(a.ok, "{level:?}: {:?}", a.message);
    }
    end(&mut t).await;
}
