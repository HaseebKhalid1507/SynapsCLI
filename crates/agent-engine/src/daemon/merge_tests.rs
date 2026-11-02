//! The two-channel merge (child spend §7.4): order, no holding, lag notices
//! and their placement, the reload drain, closing and the attach floor.
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};

use super::{Merge, Source};
use crate::session::wire::DaemonFrame;
use crate::session::{Envelope, SessionEventWire, SessionId};

const SID: &str = "s-merge";

fn env(seq: u64) -> Envelope {
    Envelope {
        session_id: SessionId(SID.into()),
        seq,
        ts: chrono::Utc::now(),
        event: SessionEventWire::SystemNotice(format!("n{seq}")),
    }
}

fn channels(main_cap: usize, opt_cap: usize) -> (
    broadcast::Sender<Envelope>,
    broadcast::Receiver<Envelope>,
    broadcast::Sender<Envelope>,
    broadcast::Receiver<Envelope>,
) {
    let (mtx, mrx) = broadcast::channel(main_cap);
    let (otx, orx) = broadcast::channel(opt_cap);
    (mtx, mrx, otx, orx)
}

/// What one popped envelope was, for assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Out {
    Main(u64),
    Opt(u64),
    MainLag(u64),
    SpendLag(u64),
}

fn out((env, source): (Envelope, Source)) -> Out {
    match (source, &env.event) {
        (Source::Main, _) => Out::Main(env.seq),
        (Source::Optional, _) => Out::Opt(env.seq),
        (Source::Notice, SessionEventWire::ChildSpendLagged { dropped }) => {
            assert_eq!(env.seq, u64::MAX, "a notice takes no actor seq");
            Out::SpendLag(*dropped)
        }
        (Source::Notice, SessionEventWire::SystemNotice(text)) => {
            assert_eq!(env.seq, u64::MAX, "a notice takes no actor seq");
            let n = text
                .strip_prefix("event stream lagged; ")
                .and_then(|t| t.strip_suffix(" dropped"))
                .and_then(|t| t.parse().ok())
                .unwrap_or_else(|| panic!("not a lag notice: {text}"));
            Out::MainLag(n)
        }
        (s, e) => panic!("{s:?} {e:?}"),
    }
}

/// Everything ready now, without waiting.
fn ready(m: &mut Merge, main: &mut broadcast::Receiver<Envelope>, opt: &mut broadcast::Receiver<Envelope>) -> Vec<Out> {
    let mut got = Vec::new();
    while let Some(x) = m.pop_ready(main, Some(&mut *opt)) {
        got.push(out(x));
    }
    got
}

/// The connection's seqs strictly increase; notices (u64::MAX) are outside
/// the order and are left out of the check.
fn assert_increasing(got: &[Out]) {
    let seqs: Vec<u64> = got
        .iter()
        .filter_map(|o| match o {
            Out::Main(s) | Out::Opt(s) => Some(*s),
            _ => None,
        })
        .collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "not strictly increasing: {got:?}");
}

#[test]
fn interleaved_channels_come_out_in_seq_order() {
    let (mtx, mut mrx, otx, mut orx) = channels(64, 64);
    for (s, on_opt) in [(1, false), (2, true), (3, false), (4, true), (5, true), (6, false)] {
        let _ = if on_opt { otx.send(env(s)) } else { mtx.send(env(s)) };
    }
    let mut m = Merge::new(SessionId(SID.into()), 0);
    let got = ready(&mut m, &mut mrx, &mut orx);
    use Out::*;
    assert_eq!(got, vec![Main(1), Opt(2), Main(3), Opt(4), Opt(5), Main(6)]);
}

/// The merge never holds a frame for a later main event: frames that are
/// the last events before an idle stretch go out at once.
#[tokio::test]
async fn frames_last_before_idle_go_out_without_a_later_main_event() {
    let (mtx, mut mrx, otx, mut orx) = channels(64, 64);
    mtx.send(env(1)).unwrap();
    otx.send(env(2)).unwrap();
    otx.send(env(3)).unwrap();
    let mut m = Merge::new(SessionId(SID.into()), 0);
    let mut got = Vec::new();
    for _ in 0..3 {
        let x = tokio::time::timeout(Duration::from_secs(5), m.next(&mut mrx, Some(&mut orx)))
            .await
            .expect("delivered without a later main event")
            .expect("open");
        got.push(out(x));
    }
    assert_eq!(got, vec![Out::Main(1), Out::Opt(2), Out::Opt(3)]);
    // Idle now: the next call waits rather than returning anything.
    assert!(tokio::time::timeout(Duration::from_millis(50), m.next(&mut mrx, Some(&mut orx))).await.is_err());
    // A frame published while it waits wakes it, with no main event.
    let waiting = tokio::spawn(async move {
        let x = m.next(&mut mrx, Some(&mut orx)).await;
        (x.map(out), mtx)
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    otx.send(env(4)).unwrap();
    let (x, _mtx) = tokio::time::timeout(Duration::from_secs(5), waiting).await.expect("woken").unwrap();
    assert_eq!(x, Some(Out::Opt(4)));
}

/// §7.4: an optional lag gives `child_spend_lagged {dropped}` (frames only)
/// right before the first frame that survived; ordinary events are not in
/// the count and keep their order.
#[test]
fn an_optional_lag_puts_the_notice_right_before_the_first_survivor() {
    let (mtx, mut mrx, otx, mut orx) = channels(64, 4);
    mtx.send(env(1)).unwrap();
    for s in 2..=9 {
        otx.send(env(s)).unwrap();
    }
    mtx.send(env(10)).unwrap();
    let mut m = Merge::new(SessionId(SID.into()), 0);
    let got = ready(&mut m, &mut mrx, &mut orx);
    use Out::*;
    assert_eq!(got, vec![Main(1), SpendLag(4), Opt(6), Opt(7), Opt(8), Opt(9), Main(10)]);
    assert_increasing(&got);
}

/// A main-channel lag gives the main lag notice; the optional stream is
/// untouched and no child_spend_lagged is sent.
#[test]
fn a_main_lag_leaves_the_optional_stream_alone() {
    let (mtx, mut mrx, otx, mut orx) = channels(4, 64);
    for s in 1..=8 {
        mtx.send(env(s)).unwrap();
    }
    otx.send(env(9)).unwrap();
    otx.send(env(10)).unwrap();
    let mut m = Merge::new(SessionId(SID.into()), 0);
    let got = ready(&mut m, &mut mrx, &mut orx);
    use Out::*;
    assert_eq!(got, vec![MainLag(4), Main(5), Main(6), Main(7), Main(8), Opt(9), Opt(10)]);
}

/// Lag on both channels at once: each gets its own notice, the frames'
/// notice right before the first surviving frame.
#[test]
fn both_channels_lag_at_once() {
    let (mtx, mut mrx, otx, mut orx) = channels(4, 4);
    for s in 1..=16 {
        let _ = if s % 2 == 0 { otx.send(env(s)) } else { mtx.send(env(s)) };
    }
    let mut m = Merge::new(SessionId(SID.into()), 0);
    let got = ready(&mut m, &mut mrx, &mut orx);
    use Out::*;
    assert_eq!(
        got,
        vec![MainLag(4), Main(9), SpendLag(4), Opt(10), Main(11), Opt(12), Main(13), Opt(14), Main(15), Opt(16)]
    );
    assert_increasing(&got);
}

/// Repeated lag while the connection's output is blocked (nothing popped):
/// one notice per gap, each right before its survivor, and every frame is
/// either delivered or counted once.
#[test]
fn repeated_lag_while_output_is_blocked() {
    let (_mtx, mut mrx, otx, mut orx) = channels(64, 4);
    for s in 1..=8 {
        otx.send(env(s)).unwrap();
    }
    let mut m = Merge::new(SessionId(SID.into()), 0);
    let first = out(m.pop_ready(&mut mrx, Some(&mut orx)).unwrap());
    let second = out(m.pop_ready(&mut mrx, Some(&mut orx)).unwrap());
    assert_eq!((first, second), (Out::SpendLag(4), Out::Opt(5)));
    // Blocked: more frames arrive than the ring holds.
    for s in 9..=16 {
        otx.send(env(s)).unwrap();
    }
    let rest = ready(&mut m, &mut mrx, &mut orx);
    use Out::*;
    assert_eq!(rest, vec![SpendLag(7), Opt(13), Opt(14), Opt(15), Opt(16)]);
    let delivered = 1 + rest.iter().filter(|o| matches!(o, Opt(_))).count() as u64;
    assert_eq!(delivered + 4 + 7, 16, "every frame delivered or counted once");
}

/// The reload drain forwards what both rings hold, in seq order, applies
/// the connection's skip to main envelopes only, and places the notice.
#[tokio::test]
async fn the_reload_drain_takes_both_channels_in_order() {
    let (mtx, mut mrx, otx, mut orx) = channels(64, 4);
    mtx.send(env(1)).unwrap();
    for s in 2..=7 {
        otx.send(env(s)).unwrap();
    }
    mtx.send(env(8)).unwrap();
    mtx.send(env(9)).unwrap();
    let (tx, mut rx) = mpsc::channel(64);
    let mut m = Merge::new(SessionId(SID.into()), 0);
    let skip = |e: &Envelope| e.seq == 8 || e.seq == 5;
    assert!(m.drain(&mut mrx, Some(&mut orx), &skip, &tx).await);
    let mut got = Vec::new();
    while let Ok(DaemonFrame::Event(w)) = rx.try_recv() {
        got.push(w.seq);
    }
    // 2 and 3 were overwritten; 5 is a frame, which the skip never drops.
    assert_eq!(got, vec![1, u64::MAX, 4, 5, 6, 7, 9]);
    drop(rx);
    mtx.send(env(10)).unwrap();
    assert!(!m.drain(&mut mrx, Some(&mut orx), &skip, &tx).await, "the client is gone");
}

/// After the main channel closes (the session ended), frames already in the
/// optional ring still go out, then the merge ends.
#[tokio::test]
async fn after_the_main_channel_closes_the_ready_frames_still_go_out() {
    let (mtx, mut mrx, otx, mut orx) = channels(64, 64);
    mtx.send(env(1)).unwrap();
    otx.send(env(2)).unwrap();
    otx.send(env(3)).unwrap();
    drop(mtx);
    drop(otx);
    let mut m = Merge::new(SessionId(SID.into()), 0);
    let mut got = Vec::new();
    while let Some(x) = tokio::time::timeout(Duration::from_secs(5), m.next(&mut mrx, Some(&mut orx))).await.unwrap() {
        got.push(out(x));
    }
    assert_eq!(got, vec![Out::Main(1), Out::Opt(2), Out::Opt(3)]);
}

/// Frames published before the connection's attach snapshot (seq below its
/// floor) are dropped: the snapshot's totals already include them.
#[test]
fn frames_from_before_the_attach_snapshot_are_dropped() {
    let (mtx, mut mrx, otx, mut orx) = channels(64, 64);
    otx.send(env(2)).unwrap();
    otx.send(env(4)).unwrap();
    mtx.send(env(5)).unwrap();
    otx.send(env(6)).unwrap();
    let mut m = Merge::new(SessionId(SID.into()), 5);
    assert_eq!(ready(&mut m, &mut mrx, &mut orx), vec![Out::Main(5), Out::Opt(6)]);
}

/// A connection without the opt-in has no optional receiver: the merge is
/// the plain main pump.
#[test]
fn without_the_opt_in_the_main_channel_is_unchanged() {
    let (mtx, mut mrx, otx, _orx) = channels(64, 64);
    mtx.send(env(1)).unwrap();
    otx.send(env(2)).unwrap();
    mtx.send(env(3)).unwrap();
    let mut m = Merge::new(SessionId(SID.into()), 0);
    let mut got = Vec::new();
    while let Some(x) = m.pop_ready(&mut mrx, None) {
        got.push(out(x));
    }
    assert_eq!(got, vec![Out::Main(1), Out::Main(3)]);
}

/// The race between the two polls (port review r1, SHOULD-FIX 1): main
/// answers Empty, then the actor, on another thread in the daemon, publishes
/// main 1 and optional 2, and the optional poll returns 2 alone. The seam
/// stages exactly that publish between the two polls. Main is polled again
/// before the lone optional head is judged, so 1 goes out first.
#[test]
fn a_main_envelope_published_between_the_two_polls_still_goes_first() {
    let (mtx, mut mrx, otx, mut orx) = channels(64, 64);
    let mut m = Merge::new(SessionId(SID.into()), 0);
    let (main_tx, opt_tx) = (mtx.clone(), otx.clone());
    m.between_polls.0 = Some(Box::new(move || {
        main_tx.send(env(1)).unwrap();
        opt_tx.send(env(2)).unwrap();
    }));
    let got = ready(&mut m, &mut mrx, &mut orx);
    assert!(m.between_polls.0.is_none(), "the staged publish ran");
    assert_eq!(got, vec![Out::Main(1), Out::Opt(2)]);
    // The channels stay open throughout (no Closed path in the race).
    drop((mtx, otx));
}

/// The same race in the reload drain, which pops through the same fill.
#[tokio::test]
async fn the_reload_drain_keeps_the_order_across_the_two_polls() {
    let (mtx, mut mrx, otx, mut orx) = channels(64, 64);
    let mut m = Merge::new(SessionId(SID.into()), 0);
    let (main_tx, opt_tx) = (mtx.clone(), otx.clone());
    m.between_polls.0 = Some(Box::new(move || {
        main_tx.send(env(1)).unwrap();
        opt_tx.send(env(2)).unwrap();
        main_tx.send(env(3)).unwrap();
    }));
    let (tx, mut rx) = mpsc::channel(64);
    assert!(m.drain(&mut mrx, Some(&mut orx), &|_: &Envelope| false, &tx).await);
    let mut got = Vec::new();
    while let Ok(DaemonFrame::Event(w)) = rx.try_recv() {
        got.push(w.seq);
    }
    assert_eq!(got, vec![1, 2, 3]);
    drop((mtx, otx));
}

/// A lag in front of the attach floor (port review r1, LOW 3). The optional
/// receiver subscribes before Attach and is not read during the attach wait,
/// so it can lag on frames the attach snapshot already holds.
/// - Every survivor is below the floor: the gap is lower still, so nothing
///   was lost to the client and no child_spend_lagged is sent.
/// - The survivors straddle the floor, the gap below it: the same.
/// - The gap itself straddles the floor: `dropped` is the whole gap, an
///   upper bound (§7.4), which only marks the breakdown incomplete.
#[test]
fn a_lag_in_front_of_the_attach_floor_is_reported_only_when_it_reaches_past_it() {
    use Out::*;
    // Survivors 5..=8, all below the floor 10.
    let (mtx, mut mrx, otx, mut orx) = channels(64, 4);
    for s in 1..=8 {
        otx.send(env(s)).unwrap();
    }
    mtx.send(env(10)).unwrap();
    otx.send(env(11)).unwrap();
    let mut m = Merge::new(SessionId(SID.into()), 10);
    assert_eq!(ready(&mut m, &mut mrx, &mut orx), vec![Main(10), Opt(11)]);

    // Survivors 5..=8 straddle the floor 7; the gap 1..=4 is below it.
    let (_mtx, mut mrx, otx, mut orx) = channels(64, 4);
    for s in 1..=8 {
        otx.send(env(s)).unwrap();
    }
    let mut m = Merge::new(SessionId(SID.into()), 7);
    assert_eq!(ready(&mut m, &mut mrx, &mut orx), vec![Opt(7), Opt(8)]);

    // The gap 1..=4 straddles the floor 3: reported whole.
    let (_mtx, mut mrx, otx, mut orx) = channels(64, 4);
    for s in 1..=8 {
        otx.send(env(s)).unwrap();
    }
    let mut m = Merge::new(SessionId(SID.into()), 3);
    assert_eq!(ready(&mut m, &mut mrx, &mut orx), vec![SpendLag(4), Opt(5), Opt(6), Opt(7), Opt(8)]);
}

/// A writer for a test subscriber that keeps everything written to it.
#[derive(Clone, Default)]
struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// The main lag's log line (port review r1, LOW 5), through the daemon's own
/// filter (`with_crate_directives` over an unset RUST_LOG): the reload drain
/// says "(reload drain)" as it did before the merge; the pump's line does
/// not (the control).
#[tokio::test]
async fn the_reload_drain_names_itself_in_the_lag_log_line() {
    use tracing_subscriber::filter::{EnvFilter, LevelFilter};
    let unset = EnvFilter::builder().with_default_directive(LevelFilter::ERROR.into()).parse_lossy("");
    let log = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(agent_core::logging::with_crate_directives(unset))
        .with_writer(log.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let text = || String::from_utf8(log.0.lock().unwrap_or_else(|p| p.into_inner()).clone()).expect("utf-8 log");

    // The pump: a lagged main ring, read through pop_ready.
    let (mtx, mut mrx, _otx, mut orx) = channels(4, 4);
    for s in 1..=8 {
        mtx.send(env(s)).unwrap();
    }
    let mut m = Merge::new(SessionId(SID.into()), 0);
    assert_eq!(out(m.pop_ready(&mut mrx, Some(&mut orx)).unwrap()), Out::MainLag(4));
    let pump = text();
    assert!(pump.contains("daemon: client lagged"), "{pump:?}");
    assert!(!pump.contains("(reload drain)"), "{pump:?}");

    // The reload drain: the same lag, read through drain.
    let (mtx, mut mrx, _otx, mut orx) = channels(4, 4);
    for s in 1..=8 {
        mtx.send(env(s)).unwrap();
    }
    let (tx, _rx) = mpsc::channel(64);
    let mut m = Merge::new(SessionId(SID.into()), 0);
    assert!(m.drain(&mut mrx, Some(&mut orx), &|_: &Envelope| false, &tx).await);
    let drained = text();
    assert!(drained.contains("daemon: client lagged (reload drain)"), "{drained:?}");
    assert!(drained.contains("WARN"), "{drained:?}");
}
