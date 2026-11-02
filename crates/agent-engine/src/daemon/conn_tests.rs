//! The reload drain (`drain_on_reload`): a connection whose receiver fell
//! behind still forwards every envelope that survived in the ring.
use super::*;

const SID: &str = "s-drain";

fn env(seq: u64) -> Envelope {
    Envelope {
        session_id: SessionId(SID.into()),
        seq,
        ts: chrono::Utc::now(),
        event: SessionEventWire::SystemNotice(format!("n{seq}")),
    }
}

/// The frames the drain sent: (seq, notice text), in order.
fn forwarded(out: &mut mpsc::Receiver<DaemonFrame>) -> Vec<(u64, String)> {
    let mut got = Vec::new();
    while let Ok(frame) = out.try_recv() {
        match frame {
            DaemonFrame::Event(e) => {
                assert_eq!(e.session_id.as_str(), SID);
                match e.event {
                    WireSessionEvent::SystemNotice { text } => got.push((e.seq, text)),
                    other => panic!("unexpected event {other:?}"),
                }
            }
            other => panic!("unexpected frame {other:?}"),
        }
    }
    got
}

/// 40 envelopes into a ring of 16: the drain sends the lag notice (24
/// dropped), then the 16 that survived, in order. The old drain stopped at
/// the lag and forwarded none of them.
#[tokio::test]
async fn a_lagged_reload_drain_sends_the_notice_then_every_survivor() {
    let (events, mut rx) = broadcast::channel::<Envelope>(16);
    for seq in 0..40 {
        events.send(env(seq)).unwrap();
    }
    let (out, mut sent) = mpsc::channel::<DaemonFrame>(64);
    let open = drain_on_reload(&mut rx, &|_: &Envelope| false, &SessionId(SID.into()), &out).await;
    assert!(open, "the client is still there");
    let mut want = vec![(u64::MAX, "event stream lagged; 24 dropped".to_string())];
    want.extend((24..40).map(|seq| (seq, format!("n{seq}"))));
    assert_eq!(
        forwarded(&mut sent),
        want,
        "the lag notice, then every envelope still in the ring, in order"
    );
}

/// Without a lag, the drain forwards what is queued except what the
/// connection skips, and returns once the ring is empty.
#[tokio::test]
async fn a_reload_drain_skips_what_the_connection_skips() {
    let (events, mut rx) = broadcast::channel::<Envelope>(16);
    for seq in 0..5 {
        events.send(env(seq)).unwrap();
    }
    let (out, mut sent) = mpsc::channel::<DaemonFrame>(64);
    let skip = |e: &Envelope| e.seq % 2 == 1;
    assert!(drain_on_reload(&mut rx, &skip, &SessionId(SID.into()), &out).await);
    let seqs: Vec<u64> = forwarded(&mut sent).into_iter().map(|(seq, _)| seq).collect();
    assert_eq!(seqs, vec![0, 2, 4]);
}

/// A client that is gone ends the drain.
#[tokio::test]
async fn a_reload_drain_stops_once_the_client_is_gone() {
    let (events, mut rx) = broadcast::channel::<Envelope>(16);
    for seq in 0..3 {
        events.send(env(seq)).unwrap();
    }
    let (out, sent) = mpsc::channel::<DaemonFrame>(64);
    drop(sent);
    assert!(!drain_on_reload(&mut rx, &|_: &Envelope| false, &SessionId(SID.into()), &out).await);
}
