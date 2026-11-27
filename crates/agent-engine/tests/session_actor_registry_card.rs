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
//!
//! Nor may a fresh session that draws a live session's id run on it.

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
    // `Ended` comes before the ended actor is gone, and the actor releases
    // the journal lock before `Ended` (so whoever waits for `closed` can take
    // it). Wait until it's gone: until then this host still counts it as
    // live, and the `--continue` below would attach to it instead of being
    // refused. (Waiting only for the lock was enough while the drop released
    // it; since the early release it fails about one run in five.)
    tokio::time::timeout(std::time::Duration::from_secs(10), handle.closed())
        .await
        .expect("the ended session's actor never went away");

    // Another process owns the session now: it holds the journal lock and
    // has published its card. The lock is free by now; the retry is a
    // backstop.
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

/// Two fresh sessions that draw the same id. The second used to run on it
/// without the journal lock: it wrote the first's journal, replaced its card
/// and socket, and replaced it in the session map, where the first kept
/// running unlisted. It now draws another id, as it does over an ended
/// session's journal. The forced ids have the old four-hex suffix, which
/// still reads as a session id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_fresh_session_that_draws_a_live_sessions_id_draws_another() {
    let _h = Home::new();
    let host = host().await;
    let taken = "20261117-015328-de08".to_string();
    agent_engine::session::fresh_id_testing::force_fresh_session_ids([
        taken.clone(),
        taken.clone(),
    ]);
    let named = |name: &str| SessionConfig {
        name: Some(name.into()),
        ..pcfg()
    };
    let first = host.create_session(named("first")).await;
    let second = host.create_session(named("second")).await;
    agent_engine::session::fresh_id_testing::force_fresh_session_ids([]);
    let (first, second) = (first.unwrap(), second.unwrap());

    assert_eq!(first.id.as_str(), taken);
    let other = second.id.as_str().to_string();
    assert_ne!(other, taken, "the second session runs on the first's id");
    assert!(
        agent_engine::core::session_catalog::is_session_id(&other),
        "{other}"
    );
    assert_eq!(second.journal_id(), other, "its journal is its new id");

    let mut listed: Vec<String> = host
        .session_handles()
        .iter()
        .map(|h| h.id.as_str().to_string())
        .collect();
    listed.sort();
    let mut both = vec![taken.clone(), other.clone()];
    both.sort();
    assert_eq!(listed, both, "both sessions are listed");

    let sessions = agent_engine::core::session_lock::sessions_dir();
    let run = registry_dir();
    for (id, name) in [(&taken, "first"), (&other, "second")] {
        let journal = agent_engine::core::session::Session::load(id).unwrap();
        assert_eq!(journal.name.as_deref(), Some(name), "journal {id}");
        let card: SessionRegistration =
            serde_json::from_str(&std::fs::read_to_string(run.join(format!("{id}.json"))).unwrap())
                .unwrap();
        assert_eq!(card.name.as_deref(), Some(name), "card {id}");
        assert_locked(id);
    }

    // An id whose journal is on disk but unlocked (its session ended) is
    // taken too: the fresh session would write over that journal.
    let ended = "20261117-015328-0e0d".to_string();
    let ended_journal = sessions.join(format!("{ended}.json"));
    std::fs::write(&ended_journal, "{}").unwrap();
    agent_engine::session::fresh_id_testing::force_fresh_session_ids([ended.clone()]);
    let third = host.create_session(cfg()).await;
    agent_engine::session::fresh_id_testing::force_fresh_session_ids([]);
    let third = third.unwrap();
    assert_ne!(
        third.id.as_str(),
        ended,
        "a fresh session took an ended session's id"
    );
    assert_eq!(std::fs::read_to_string(&ended_journal).unwrap(), "{}");
    assert_no_stray_lock(&[&taken, &other, third.id.as_str()]);

    for h in [first, second, third] {
        h.send(SessionCommand::End {
            reason: EndReason::HostShutdown,
        })
        .await
        .unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), h.closed()).await;
    }
}

/// Two unnamed fresh sessions that draw the same id, as two sessions opened
/// in the same second do. The first has nothing on disk before its first
/// turn, so only its journal lock says the id is taken: the second used to
/// log the held lock and run on the id without it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_fresh_session_that_draws_an_unjournaled_live_id_draws_another() {
    let _h = Home::new();
    let host = host().await;
    let taken = "20261117-015328-a11d".to_string();
    agent_engine::session::fresh_id_testing::force_fresh_session_ids([
        taken.clone(),
        taken.clone(),
    ]);
    let first = host.create_session(pcfg()).await;
    let second = host.create_session(pcfg()).await;
    agent_engine::session::fresh_id_testing::force_fresh_session_ids([]);
    let (first, second) = (first.unwrap(), second.unwrap());

    let sessions = agent_engine::core::session_lock::sessions_dir();
    for ext in ["json", "journal", "turn"] {
        let file = sessions.join(format!("{taken}.{ext}"));
        assert!(!file.exists(), "the first session wrote {}", file.display());
    }
    assert_eq!(first.id.as_str(), taken);
    let other = second.id.as_str().to_string();
    assert_ne!(other, taken, "the second session runs on the first's id");
    assert!(
        agent_engine::core::session_catalog::is_session_id(&other),
        "{other}"
    );
    assert_eq!(second.journal_id(), other, "its journal is its new id");
    let mut listed: Vec<String> = host
        .session_handles()
        .iter()
        .map(|h| h.id.as_str().to_string())
        .collect();
    listed.sort();
    let mut both = vec![taken.clone(), other.clone()];
    both.sort();
    assert_eq!(listed, both, "both sessions are listed");
    assert_locked(&taken);
    assert_locked(&other);
    assert_no_stray_lock(&[&taken, &other]);

    for h in [first, second] {
        h.send(SessionCommand::End {
            reason: EndReason::HostShutdown,
        })
        .await
        .unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), h.closed()).await;
    }
}

/// A live session holds `id`'s journal lock.
fn assert_locked(id: &str) {
    let lock = agent_engine::core::session_lock::SessionLock::try_acquire(
        &agent_engine::core::session_lock::sessions_dir(),
        id,
        agent_engine::core::session_lock::LockHolder {
            pid: std::process::id(),
            kind: "test".into(),
        },
    );
    assert!(
        matches!(
            lock,
            Err(agent_engine::core::session_lock::SessionLockError::Held { .. })
        ),
        "{id} runs without its journal lock: {lock:?}"
    );
}

/// Every lock file in the sessions dir is a live session's or sits beside a
/// journal: a fresh session's failed draws leave none of their own.
fn assert_no_stray_lock(live: &[&str]) {
    let dir = agent_engine::core::session_lock::sessions_dir();
    for entry in std::fs::read_dir(&dir).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = name.strip_suffix(".lock") else {
            continue;
        };
        let journaled = ["json", "journal", "turn"]
            .iter()
            .any(|ext| dir.join(format!("{id}.{ext}")).exists());
        assert!(
            live.contains(&id) || journaled,
            "stray lock file {name} (live: {live:?})"
        );
    }
}
