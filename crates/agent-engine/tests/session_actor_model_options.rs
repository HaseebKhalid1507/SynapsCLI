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
    t.send_from_self(SessionCommand::Query {
        id: 5,
        query: SessionQuery::ModelOptions,
    })
    .await
    .unwrap();
    let seen = until(&mut t, |e| {
        matches!(e, SessionEventWire::QueryResult { id: 5, .. })
    })
    .await;
    let SessionEventWire::QueryResult { value, .. } = &seen.last().unwrap().event else {
        unreachable!()
    };
    let o: ModelOptions = serde_json::from_value(value.clone()).expect("the pinned shape");
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
