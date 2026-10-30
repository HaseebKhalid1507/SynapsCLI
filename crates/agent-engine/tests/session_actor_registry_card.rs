//! A `--continue` that loses the journal lock must not touch the live
//! owner's registry card.
//!
//! Incident (2026-10-18): a daemon session stayed live (journal lock held,
//! per-session socket listening), but its `run/<id>.json` card was gone, so
//! `synaps send` and agent-comms could no longer see it. A TUI started with
//! `--continue <id>` built a second actor for the same id. That actor
//! published its own card over the owner's before it asked for the journal
//! lock. The lock was refused, the actor was dropped, and its endpoint
//! cleanup removed the card, which it now owned. A `--name` on that
//! refused continue also saved the owner's journal first.

mod session_actor_common;
use session_actor_common::*;

use agent_engine::events::registry::{registry_dir, SessionRegistration};
use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, EndReason, LocalTransport, SessionCommand,
    SessionConfig, SessionEventWire,
};
use serial_test::serial;

fn pcfg() -> SessionConfig {
    SessionConfig {
        persist: true,
        ..cfg()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn refused_continue_leaves_the_live_owners_card_alone() {
    let _h = Home::new();
    let (url, _) = stub(SSE_HI, false).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;

    // A session with a journal on disk, then ended so this host no longer
    // runs it (a `--continue` must go through `SessionActor::create`).
    let handle = host.create_session(pcfg()).await.unwrap();
    let id = handle.id.as_str().to_string();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Tui))
        .await
        .unwrap();
    a.send(submit("hi")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    handle
        .send(SessionCommand::End {
            reason: EndReason::HostShutdown,
        })
        .await
        .unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Ended { .. })).await;

    // Another process owns the session now: it holds the journal lock and
    // has published its card. `Ended` is emitted before the ended actor is
    // dropped, and the drop is what releases its lock, so wait for it.
    let sessions = agent_engine::core::session_lock::sessions_dir();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let _owner_lock = loop {
        match agent_engine::core::session_lock::SessionLock::try_acquire(
            &sessions,
            &id,
            agent_engine::core::session_lock::LockHolder {
                pid: std::process::id(),
                kind: "daemon".into(),
            },
        ) {
            Ok(lock) => break lock,
            Err(_) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(e) => panic!("the ended session never released its journal lock: {e:?}"),
        }
    };
    let run = registry_dir();
    let owner = SessionRegistration {
        kind: "session".into(),
        session_id: id.clone(),
        name: None,
        socket_path: run.join("owner.sock").to_string_lossy().into_owned(),
        pid: std::process::id(),
        started_at: chrono::Utc::now(),
    };
    let card = run.join(format!("{id}.json"));
    std::fs::write(&card, serde_json::to_string(&owner).unwrap()).unwrap();
    let before = std::fs::read_to_string(&card).unwrap();

    let refused = host
        .create_session(SessionConfig {
            continue_session: Some(Some(id.clone())),
            name: Some("intruder".into()),
            ..pcfg()
        })
        .await;
    let Err(err) = refused else {
        panic!("continue must be refused while another process holds the journal");
    };
    assert!(
        err.to_string().contains("live in another process"),
        "unexpected error: {err}"
    );

    // It may not write the owner's journal (`--name` used to save it first)…
    let journal = agent_engine::core::session::Session::load(&id).unwrap();
    assert_ne!(
        journal.name.as_deref(),
        Some("intruder"),
        "the refused continue renamed the owner's journal"
    );
    // …nor touch the owner's card.
    let after = std::fs::read_to_string(&card)
        .unwrap_or_else(|e| panic!("the owner's card was removed by the refused continue: {e}"));
    assert_eq!(
        after, before,
        "the refused continue rewrote the owner's card"
    );
    let stray: Vec<_> = std::fs::read_dir(&run)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("s-") && n.ends_with(".sock"))
        .collect();
    assert!(
        stray.is_empty(),
        "the refused continue left a socket behind: {stray:?}"
    );
}
