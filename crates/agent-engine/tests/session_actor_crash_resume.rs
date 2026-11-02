//! A daemon crash must not end work: a fresh daemon's crash restore sends
//! each restored session `SessionCommand::ResumeAfterCrash`, and the actor
//! resumes the turn the crash cut ONCE, only when the turn draft it folded
//! in was written by the dead daemon and nothing answered or re-prompted
//! the turn since. A session that was idle while its workers ran gets one
//! note naming them instead.
//!
//! In-process actor, loopback Anthropic SSE stub, isolated HOME; no daemon.
//! The crash is played as the actor sees it (`crash_mid_turn`, as in
//! session_actor_checkpoint.rs): the disk is captured mid-turn and put back
//! after the actor is gone, so the next actor's load folds the draft in.
//! The "dead daemon" is this test process: its drafts carry this
//! process's key (`session::live::process_key`).

mod session_actor_common;
use session_actor_common::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_engine::core::session_draft::read_turn_draft;
use agent_engine::core::session_lock::sessions_dir;
use agent_engine::daemon::crash_record::resume_text;
use agent_engine::session::{
    ClientKind, ClientMeta, ClientTransport, Envelope, LocalTransport, SessionCommand,
    SessionConfig, SessionEventWire, SessionHandle, TurnTrigger,
};
use futures::StreamExt;
use serial_test::serial;

/// The requests numbered in `hang` stream one text delta ("hi") and never
/// end (a crash cuts them); every other one is a whole reply (`SSE_HI`).
async fn provider(hang: &'static [usize]) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_c = Arc::clone(&hits);
    let app = axum::Router::new().fallback(move || {
        let hits = Arc::clone(&hits_c);
        async move {
            let hit = hits.fetch_add(1, Ordering::SeqCst);
            let hangs = hang.contains(&hit);
            let stream = futures::stream::once(async move {
                Ok::<_, std::io::Error>(axum::body::Bytes::from(if hangs {
                    SSE_PREFIX
                } else {
                    SSE_HI
                }))
            })
            .chain(futures::stream::unfold(hangs, |hangs| async move {
                if !hangs {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                Some((Ok(axum::body::Bytes::from(": keep-alive\n\n")), hangs))
            }));
            axum::response::Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from_stream(stream))
                .unwrap()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), hits)
}

fn persist_cfg() -> SessionConfig {
    SessionConfig {
        persist: true,
        ..cfg()
    }
}

fn is_text(e: &SessionEventWire) -> bool {
    matches!(
        e,
        SessionEventWire::Stream(agent_engine::StreamEvent::Llm(agent_engine::LlmEvent::Text(_)))
    )
}

fn crashed_at() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-10-19T15:51:07Z")
        .unwrap()
        .with_timezone(&chrono::Utc)
}

/// This process's key: the writer of every draft this test's actors write.
fn this_process() -> String {
    agent_engine::session::live::process_key()
        .expect("a process key on Linux")
        .to_string()
}

/// Send the restore's command and return the actor's decision.
async fn resume(
    handle: &SessionHandle,
    crash: &str,
    turn_in_flight: bool,
    lost_workers: &[&str],
) -> &'static str {
    let (tx, rx) = tokio::sync::oneshot::channel();
    handle
        .send(SessionCommand::ResumeAfterCrash {
            crash: crash.to_string(),
            crashed_at: crashed_at(),
            turn_in_flight,
            lost_workers: lost_workers.iter().map(|w| w.to_string()).collect(),
            reply: Some(tx),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), rx)
        .await
        .expect("the actor answers in time")
        .expect("the actor answers")
}

/// The resume turns among `seen`: `TurnStarted` as a queued message.
fn resume_turns(seen: &[Envelope]) -> Vec<Option<String>> {
    seen.iter()
        .filter_map(|e| match &e.event {
            SessionEventWire::TurnStarted { trigger: TurnTrigger::QueuedAuto, user_text, .. } => {
                Some(user_text.clone())
            }
            _ => None,
        })
        .collect()
}

fn turns_started(seen: &[Envelope]) -> usize {
    seen.iter()
        .filter(|e| matches!(e.event, SessionEventWire::TurnStarted { .. }))
        .count()
}

/// Poll the draft until it holds the streamed "hi" written by this process.
async fn draft_written_here(id: &str) {
    let me = this_process();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let d = read_turn_draft(&sessions_dir(), id).expect("readable draft");
        if d.as_ref()
            .is_some_and(|d| d.partial_text == "hi" && d.writer.as_deref() == Some(me.as_str()))
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the draft never carried the reply and this process's key: {d:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The session's files exactly as they were at one instant.
struct DiskState(Vec<(std::path::PathBuf, Vec<u8>)>);

impl DiskState {
    fn capture(id: &str) -> Self {
        let dir = sessions_dir();
        DiskState(
            ["json", "journal", "turn"]
                .iter()
                .map(|ext| dir.join(format!("{id}.{ext}")))
                .filter_map(|p| std::fs::read(&p).ok().map(|b| (p, b)))
                .collect(),
        )
    }
    fn restore(&self, id: &str) {
        let dir = sessions_dir();
        for ext in ["json", "journal", "turn"] {
            let _ = std::fs::remove_file(dir.join(format!("{id}.{ext}")));
        }
        for (path, bytes) in &self.0 {
            std::fs::write(path, bytes).unwrap();
        }
    }
}

async fn gone(handle: &SessionHandle) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while handle.is_alive() {
        assert!(tokio::time::Instant::now() < deadline, "actor never exited");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Play `kill -9` mid-turn: capture the disk while the turn runs, end the
/// session, wait until its actor is gone, put the captured files back.
async fn crash_mid_turn(a: &mut LocalTransport, handle: &SessionHandle, id: &str) {
    let at_crash = DiskState::capture(id);
    assert!(
        at_crash
            .0
            .iter()
            .any(|(p, _)| p.extension().is_some_and(|e| e == "turn")),
        "a running turn has a draft"
    );
    end(a).await;
    gone(handle).await;
    at_crash.restore(id);
}

/// What the restore does for one session: load it from disk (`continue`).
async fn restore(
    host: &Arc<agent_engine::EngineHost>,
    id: &str,
) -> (SessionHandle, LocalTransport) {
    let handle = host
        .create_session(SessionConfig {
            continue_session: Some(Some(id.to_string())),
            ..persist_cfg()
        })
        .await
        .unwrap();
    let (client, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    (handle, client)
}

/// A session with a turn in flight past its first text, then "crashed".
async fn crashed_session(host: &Arc<agent_engine::EngineHost>) -> String {
    let handle = host.create_session(persist_cfg()).await.unwrap();
    let id = handle.journal_id();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    a.send(submit("never finishes")).await.unwrap();
    until(&mut a, is_text).await;
    draft_written_here(&id).await;
    crash_mid_turn(&mut a, &handle, &id).await;
    id
}

/// The turn the crash cut is resumed once: not for a crash whose daemon
/// did not write the draft, once for the one that did, never again for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_crash_cut_turn_is_resumed_once_and_only_for_the_crash_that_cut_it() {
    let _h = Home::new();
    let (url, hits) = provider(&[0]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let id = crashed_session(&host).await;
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    let (handle, mut client) = restore(&host, &id).await;
    // A record from some other daemon (not the draft's writer): nothing.
    assert_eq!(resume(&handle, "4242:1:another-boot", true, &[]).await, "skip");
    let seen = drain_for(&mut client, Duration::from_millis(500)).await;
    assert_eq!(turns_started(&seen), 0, "{seen:#?}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    // The dead daemon's own record: ONE resume turn, however often it comes.
    let me = this_process();
    assert_eq!(resume(&handle, &me, true, &[]).await, "resumed");
    assert_eq!(resume(&handle, &me, true, &[]).await, "skip", "already resumed");
    let seen = drain_for(&mut client, Duration::from_millis(2500)).await;
    let text = resume_text(crashed_at(), true, &[]);
    assert!(text.starts_with("resume: the daemon restarted after a crash mid-turn (15:51 UTC)"));
    assert_eq!(resume_turns(&seen), vec![Some(text)], "{seen:#?}");
    assert_eq!(turns_started(&seen), 1, "{seen:#?}");
    assert!(seen.iter().any(|e| matches!(e.event, SessionEventWire::Idle)), "the resumed turn ended");
    assert_eq!(hits.load(Ordering::SeqCst), 2, "one new provider request");

    // Answered now: a repeat of the same crash still queues nothing.
    assert_eq!(resume(&handle, &me, true, &[]).await, "skip");
    let seen = drain_for(&mut client, Duration::from_millis(500)).await;
    assert_eq!(turns_started(&seen), 0, "{seen:#?}");
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    end(&mut client).await;
}

/// Another sender (the app, a sweep) already reached the session after the
/// crash: a turn it re-prompted, answered or still running, is not resumed
/// a second time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_turn_another_sender_already_reprompted_is_not_resumed_again() {
    // Answered: the nudge's turn ran to its end.
    {
        let _h = Home::new();
        let (url, hits) = provider(&[0]).await;
        std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let host = host().await;
        let id = crashed_session(&host).await;
        let (handle, mut client) = restore(&host, &id).await;
        client.send(submit("nudge: please continue")).await.unwrap();
        until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await;
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert_eq!(resume(&handle, &this_process(), true, &[]).await, "skip");
        let seen = drain_for(&mut client, Duration::from_millis(500)).await;
        assert_eq!(turns_started(&seen), 0, "{seen:#?}");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        end(&mut client).await;
    }
    // Re-prompted, its turn still running: the latest marker is the crash's
    // and nothing answered it, but a user message follows it.
    {
        let _h = Home::new();
        let (url, hits) = provider(&[0, 1]).await;
        std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
        let host = host().await;
        let id = crashed_session(&host).await;
        let (handle, mut client) = restore(&host, &id).await;
        client.send(submit("nudge: please continue")).await.unwrap();
        until(&mut client, is_text).await;
        assert_eq!(resume(&handle, &this_process(), true, &[]).await, "skip");
        let seen = drain_for(&mut client, Duration::from_millis(500)).await;
        assert_eq!(turns_started(&seen), 0, "{seen:#?}");
        assert_eq!(hits.load(Ordering::SeqCst), 2, "only the nudge's request");
        client.send(SessionCommand::Cancel).await.unwrap();
        until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await;
        end(&mut client).await;
    }
}

/// A session that was idle while its workers ran: one note naming them,
/// not sent again by a later restore from the same record; an idle session
/// with no workers gets nothing, and a turn the record calls cut gets
/// nothing when no crashed draft was folded in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn an_idle_parent_gets_one_note_naming_the_workers_the_crash_stopped() {
    let _h = Home::new();
    let (url, hits) = provider(&[]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let handle = host.create_session(persist_cfg()).await.unwrap();
    let id = handle.journal_id();
    let (mut a, _) = LocalTransport::attach(handle.clone(), ClientMeta::new(ClientKind::Test))
        .await
        .unwrap();
    a.send(submit("start two workers")).await.unwrap();
    until(&mut a, |e| matches!(e, SessionEventWire::Idle)).await;
    end(&mut a).await;
    gone(&handle).await;
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    let crash = "4242:1:another-boot";
    let workers = ["sa_3", "sa_9"];
    let (handle, mut client) = restore(&host, &id).await;
    assert_eq!(resume(&handle, crash, false, &[]).await, "none");
    assert_eq!(resume(&handle, crash, false, &workers).await, "resumed");
    let seen = drain_for(&mut client, Duration::from_millis(2500)).await;
    let owned: Vec<String> = workers.iter().map(|w| w.to_string()).collect();
    let note = resume_text(crashed_at(), false, &owned);
    assert!(note.contains("(sa_3, sa_9)"), "{note}");
    assert_eq!(resume_turns(&seen), vec![Some(note)], "{seen:#?}");
    assert!(seen.iter().any(|e| matches!(e.event, SessionEventWire::Idle)));
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    end(&mut client).await;
    gone(&handle).await;

    // A later start restores from the same record: the note is in the
    // history already. A cut turn with no crashed draft is not resumed.
    let (handle, mut client) = restore(&host, &id).await;
    assert_eq!(resume(&handle, crash, false, &workers).await, "skip", "already noted");
    assert_eq!(resume(&handle, &this_process(), true, &[]).await, "skip", "no crashed draft");
    let seen = drain_for(&mut client, Duration::from_millis(500)).await;
    assert_eq!(turns_started(&seen), 0, "{seen:#?}");
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    end(&mut client).await;
}

/// The crash-loop mark (the record entry's `resumed_at`) is set when a
/// crash restore queues the resume, stays while the resumed turn runs and
/// when that turn is cut (a cancel stands in for the next death), and is
/// cleared once a turn ends with its reply. So only a death that cuts the
/// resumed turn again is taken for a crash loop, never one that cuts a
/// later, unrelated turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_crash_loop_mark_lasts_until_a_turn_ends_with_its_reply() {
    let _h = Home::new();
    // The crashed turn and the resumed turn both hang; the next one replies.
    let (url, hits) = provider(&[0, 1]).await;
    std::env::set_var("SYNAPS_ANTHROPIC_BASE_URL", &url);
    let host = host().await;
    let id = crashed_session(&host).await;
    let (handle, mut client) = restore(&host, &id).await;
    let marked = |h: &SessionHandle| h.live_entry().is_some_and(|e| e.resumed_at.is_some());
    assert!(!marked(&handle), "no mark before a resume");

    assert_eq!(resume(&handle, &this_process(), true, &[]).await, "resumed");
    until(&mut client, is_text).await;
    assert_eq!(hits.load(Ordering::SeqCst), 2, "the resumed turn's request");
    assert!(marked(&handle), "the mark is recorded while the resumed turn runs");

    client.send(SessionCommand::Cancel).await.unwrap();
    until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await;
    assert!(marked(&handle), "a cut resumed turn keeps the mark");

    client.send(submit("an unrelated task")).await.unwrap();
    until(&mut client, |e| matches!(e, SessionEventWire::Idle)).await;
    assert_eq!(hits.load(Ordering::SeqCst), 3, "the unrelated turn's request");
    assert!(!marked(&handle), "a turn that ended with its reply clears the mark");
    end(&mut client).await;
}
