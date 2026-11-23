//! The C2 exit gate (`agent-ui/architecture.md` v3.2 §7): crash
//! consistency of the agent-card sidecar, named as the design lists them.
//! Every test runs without a client, on `ModelFs`: a crash drops every
//! rename not followed by a directory sync (or keeps them all), and
//! failpoints fail, stall or kill the writer on either side of a rename.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use agent_core::core::session_surfaces::*;
use agent_core::core::ui_types::*;
use chrono::{DateTime, TimeZone, Utc};
use serde_json::{json, Value};

const SID: &str = "20261123-120000-a1b2";
const SID2: &str = "20261123-120500-c3d4";
const LINEAGE: &str = "0123456789abcdef0123456789abcdef";
const OTHER_LINEAGE: &str = "fedcba9876543210fedcba9876543210";
const HX: &str = "00112233445566778899aabbccddeeff";
const DIGEST: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
const LONG: Duration = Duration::from_secs(10);

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 23, 12, 0, 0).unwrap()
}

fn hours_ago(h: i64) -> String {
    (now() - chrono::Duration::hours(h)).to_rfc3339()
}

fn cfg() -> StoreConfig {
    StoreConfig {
        budget: Duration::from_millis(150),
        sync_retries: SYNC_RETRIES,
        sync_gap: Duration::from_millis(2),
    }
}

fn budget() -> Duration {
    cfg().budget
}

fn sid(n: u8) -> String {
    format!("s_{n:016x}")
}

fn aid(n: u8) -> String {
    format!("a_{n:016x}")
}

fn act(n: u8) -> String {
    format!("{n:032x}")
}

fn contract() -> String {
    format!("c1:{}", "0".repeat(32))
}

fn doc() -> CardDoc {
    serde_json::from_value(json!({
        "question": "Which approach?",
        "options": [{"label": "Refactor"}, {"label": "Rewrite"}],
    }))
    .unwrap()
}

fn values() -> Answer {
    Answer {
        choice: Some(Choice::One("Refactor".into())),
        note: None,
        fields: None,
    }
}

fn other_values() -> Answer {
    Answer {
        choice: Some(Choice::One("Rewrite".into())),
        note: None,
        fields: None,
    }
}

fn card_raised(id: &str, mode: WaitMode, raised_at: String) -> SurfaceState {
    SurfaceState {
        id: id.to_string(),
        call_id: format!("toolu_{id}"),
        rev: 1,
        contract: contract(),
        catalog: 1,
        mode,
        phase: Phase::Open,
        offers: vec![Verb::Answer, Verb::Skip],
        doc: doc(),
        origin: Origin {
            turn: None,
            task: "Cut the 0.13 release".into(),
            stopped: None,
        },
        epoch: 0,
        resume: false,
        actions: Vec::new(),
        raised_at,
        downgraded: None,
        answer: None,
    }
}

fn create(n: u8, mode: WaitMode) -> Op {
    Op::Create(Box::new(card_raised(&sid(n), mode, hours_ago(1))))
}

/// An accepted action `n` on card `card`: answer `aid(n)`, action `act(n)`.
#[allow(clippy::too_many_arguments)]
fn accept_with(
    card: u8,
    n: u8,
    verb: Verb,
    rev: u32,
    wake: bool,
    epoch: u32,
    vals: Answer,
    offers: Vec<Verb>,
) -> Op {
    let status = match verb {
        Verb::Answer => AnswerStatus::Answered,
        Verb::Skip => AnswerStatus::Skipped,
        Verb::Revise => AnswerStatus::Revised,
    };
    let hash = action_hash(verb, rev - 1, epoch, &vals);
    Op::Accept(Box::new(Accepted {
        record: LedgerRecord {
            answer_id: aid(n),
            card: sid(card),
            rev,
            status,
            values: vals,
            wake,
            epoch,
            delivery: Delivery::Queued,
            q: "Which approach?".into(),
            asked: None,
        },
        action: LogEntry {
            action_id: act(n),
            hash,
            rev,
        },
        by: "desk".into(),
        at: hours_ago(0),
        offers,
    }))
}

fn accept(card: u8, n: u8) -> Op {
    accept_with(card, n, Verb::Answer, 1, true, 0, values(), Vec::new())
}

fn request(card: u8, n: u8, verb: Verb, expect_rev: u32, epoch: u32) -> ActionRequest {
    ActionRequest {
        card: sid(card),
        action_id: act(n),
        hash: action_hash(verb, expect_rev, epoch, &values()),
        verb,
        contract: contract(),
        epoch,
        expect_rev,
    }
}

fn deliver(n: u8, to: Delivery) -> Op {
    Op::Deliver {
        answer_id: aid(n),
        to,
    }
}

fn base_image(session_id: &str) -> Sidecar {
    Sidecar::new(session_id, LINEAGE, ROOT_BASE)
}

struct World {
    fs: Arc<ModelFs>,
    store: UiStore<ModelFs>,
    img: Images,
}

fn world() -> World {
    let fs = ModelFs::new();
    let store = UiStore::open(fs.clone(), SID, cfg()).unwrap();
    World {
        fs,
        store,
        img: Images::new(base_image(SID)),
    }
}

impl World {
    fn commit(&mut self, op: Option<Op>) -> Poll {
        self.img.commit(&mut self.store, op).unwrap();
        self.img.poll(&mut self.store, LONG)
    }

    fn commit_ok(&mut self, op: Op) {
        assert_eq!(self.commit(Some(op)), Poll::Done(Outcome::Durable));
    }

    fn on_disk(&self, session_id: &str) -> Option<Sidecar> {
        self.fs
            .get(&sidecar_name(session_id))
            .map(|b| decode_sidecar(&b).unwrap())
    }

    fn recover(
        &self,
        session_id: &str,
        history: &HistoryFacts,
        parent: Option<&ParentFacts>,
    ) -> Plan {
        recover(
            &load(&*self.fs, session_id).unwrap(),
            history,
            parent,
            now(),
        )
    }
}

fn answer_msg(lineage: &str, card: u8, n: u8, rev: u32) -> Value {
    json!({
        "role": "user",
        "content": format!("<ui_answer card=\"{}\" answer=\"{}\">{{}}</ui_answer>", sid(card), aid(n)),
        MARK_KEY: {"l": lineage, "card": sid(card), "answer": aid(n), "rev": rev, "status": "answered"},
    })
}

fn history_of(session_id: &str, base: Option<&str>, messages: &[Value]) -> HistoryFacts {
    HistoryFacts::from_messages(session_id, base, None, messages.iter())
}

fn history(messages: &[Value]) -> HistoryFacts {
    history_of(SID, None, messages)
}

/// The facts of a saved session file (JSON mode or a journal's replayed
/// snapshot): its generation and mode are there, and never read.
fn facts_of(session: &Value) -> HistoryFacts {
    HistoryFacts::from_messages(
        session["id"].as_str().unwrap(),
        session["compaction"]["source_range_digest"].as_str(),
        session["compacted_into"].as_str(),
        session["api_messages"].as_array().unwrap().iter(),
    )
}

fn image(plan: &Plan) -> &Sidecar {
    plan.commit.as_ref().expect("a recovery commit")
}

fn card(s: &Sidecar, n: u8) -> &SurfaceState {
    s.card(&sid(n)).expect("the card")
}

// ── Store (R2-H1) ────────────────────────────────────────────────────────

/// The writer stalls past the budget: the action is pending, the write
/// lands, the outcome is Durable, and nothing overtook it.
#[test]
fn store_timeout_worker_continues() {
    let mut w = world();
    w.commit_ok(create(1, WaitMode::Live));
    w.fs.stall();
    let seq = w.img.commit(&mut w.store, Some(accept(1, 1))).unwrap();
    std::thread::sleep(budget());
    assert_eq!(w.img.poll(&mut w.store, budget()), Poll::Pending);
    assert_eq!(
        w.img.admit(&w.store, &request(1, 2, Verb::Answer, 0, 0)),
        Err(Admission::Busy)
    );
    assert_eq!(
        w.img.admit(&w.store, &request(1, 1, Verb::Answer, 0, 0)),
        Err(Admission::Pending)
    );
    assert!(
        card(&w.img.projection(), 1).answer.is_none(),
        "never accepted before Durable"
    );
    w.fs.release();
    assert_eq!(w.img.poll(&mut w.store, LONG), Poll::Done(Outcome::Durable));
    let disk = w.on_disk(SID).unwrap();
    assert_eq!(disk.seq, seq);
    assert!(card(&disk, 1).answer.is_some());
    let seqs: Vec<u64> = w.fs.renames().iter().filter_map(|r| r.after).collect();
    assert!(seqs.windows(2).all(|p| p[0] < p[1]), "{seqs:?}");
    assert_eq!(seqs.last(), Some(&seq), "nothing overtook it");
}

/// A failure after `renameat` is Unsynced, never rolled back.
#[test]
fn store_error_after_rename() {
    let fs = ModelFs::new();
    fs.fail_after_rename(1);
    let out = write_durable(&*fs, "x.ui", br#"{"seq":1}"#);
    assert!(matches!(out, Outcome::Unsynced(_)), "{out:?}");
    assert_eq!(
        fs.get("x.ui").as_deref(),
        Some(&br#"{"seq":1}"#[..]),
        "renamed, kept"
    );

    // Through the store: the writer syncs again, and the change stands.
    let mut w = world();
    w.commit_ok(create(1, WaitMode::Live));
    w.fs.fail_after_rename(1);
    assert_eq!(w.commit(Some(accept(1, 1))), Poll::Done(Outcome::Durable));
    assert!(card(w.img.durable(), 1).answer.is_some());

    // And when the syncs keep failing too, the store fails with the change
    // kept, in memory and on disk.
    w.fs.fail_after_rename(1);
    w.fs.fail_sync(u32::MAX);
    let out = w.commit(Some(Op::Close { card: sid(1) }));
    assert!(matches!(out, Poll::Done(Outcome::Unsynced(_))), "{out:?}");
    assert!(w.store.failure().is_some());
    assert_eq!(card(&w.img.live(), 1).phase, Phase::Closed);
    assert_eq!(
        card(&w.on_disk(SID).unwrap(), 1).phase,
        Phase::Closed,
        "never rolled back"
    );
}

/// `sync_all` fails on every retry: the store fails closed, and the next
/// action is refused before any change. A sync that recovers within its
/// retries is Durable.
#[test]
fn store_dir_sync_fails() {
    let mut w = world();
    w.commit_ok(create(1, WaitMode::Live));
    w.commit_ok(create(2, WaitMode::Live));
    let before = w.fs.sync_calls();
    w.fs.fail_sync(u32::MAX);
    let out = w.commit(Some(accept(1, 1)));
    assert!(matches!(out, Poll::Done(Outcome::Unsynced(_))), "{out:?}");
    assert_eq!(
        w.fs.sync_calls() - before,
        1 + SYNC_RETRIES,
        "the sync and its retries"
    );
    let live = w.img.live();
    let refused = w.img.admit(&w.store, &request(2, 2, Verb::Answer, 0, 0));
    assert!(matches!(refused, Err(Admission::Storage(_))), "{refused:?}");
    let refused = w.img.commit(&mut w.store, Some(accept(2, 2)));
    assert!(matches!(refused, Err(Refusal::Storage(_))), "{refused:?}");
    assert_eq!(w.img.live(), live, "refused before any change");

    let mut w = world();
    w.commit_ok(create(1, WaitMode::Live));
    w.fs.fail_sync(SYNC_RETRIES);
    assert_eq!(w.commit(Some(accept(1, 1))), Poll::Done(Outcome::Durable));
    assert!(w.store.failure().is_none());
}

/// With the rename dropped, recovery sees the old image; with it kept, the
/// new one. Never a mix.
#[test]
fn store_crash_after_rename_before_dir_sync() {
    for keep in [false, true] {
        let mut w = world();
        w.commit_ok(create(1, WaitMode::Live));
        let old = w.fs.get(&sidecar_name(SID)).unwrap();
        w.fs.fail_sync(u32::MAX);
        let _ = w.commit(Some(accept(1, 1)));
        let new = w.fs.get(&sidecar_name(SID)).unwrap();
        assert_ne!(old, new);
        w.fs.crash(keep);
        let after = w.fs.get(&sidecar_name(SID)).unwrap();
        assert_eq!(after, if keep { new } else { old }, "keep {keep}");
        let plan = w.recover(SID, &history(&[]), None);
        assert_eq!(card(image(&plan), 1).answer.is_some(), keep, "keep {keep}");
    }
}

/// A second, different answer while the first is pending is `busy`; then
/// `resolved` if the first became durable, admitted if it was
/// NotPublished. A same-id retry and an attach before either outcome get
/// `pending` and a projection without the action.
#[test]
fn store_changed_answer_while_indeterminate() {
    for published in [true, false] {
        let mut w = world();
        w.commit_ok(create(1, WaitMode::Live));
        w.fs.stall();
        if !published {
            w.fs.fail_before_rename(1);
        }
        w.img.commit(&mut w.store, Some(accept(1, 1))).unwrap();
        assert_eq!(w.img.poll(&mut w.store, budget()), Poll::Pending);
        let changed = ActionRequest {
            action_id: act(2),
            hash: action_hash(Verb::Answer, 0, 0, &other_values()),
            ..request(1, 2, Verb::Answer, 0, 0)
        };
        assert_eq!(w.img.admit(&w.store, &changed), Err(Admission::Busy));
        assert_eq!(
            w.img.admit(&w.store, &request(1, 1, Verb::Answer, 0, 0)),
            Err(Admission::Pending)
        );
        let attach = w.img.projection();
        assert!(card(&attach, 1).answer.is_none() && attach.ledger.is_empty());
        w.fs.release();
        let out = w.img.poll(&mut w.store, LONG);
        if published {
            assert_eq!(out, Poll::Done(Outcome::Durable));
            assert_eq!(w.img.admit(&w.store, &changed), Err(Admission::Resolved));
            assert_eq!(
                w.img.admit(&w.store, &request(1, 1, Verb::Answer, 0, 0)),
                Err(Admission::Duplicate { rev: 1 })
            );
        } else {
            assert!(
                matches!(out, Poll::Done(Outcome::NotPublished(_))),
                "{out:?}"
            );
            assert_eq!(w.img.admit(&w.store, &changed), Ok(()));
        }
    }
}

fn tiny_rng(seed: u64) -> impl FnMut() -> u64 {
    let mut x = seed;
    move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    }
}

/// A property test: no interleaving of failures before and after the
/// rename, writer deaths, failed syncs and crashes (kept or dropped) lets
/// an older `seq` replace a newer one.
#[test]
fn store_never_overwrites_newer() {
    let mut next = tiny_rng(0x9e37_79b9_7f4a_7c15);
    for case in 0..24 {
        let fs = ModelFs::new();
        let mut store = UiStore::open(fs.clone(), SID, cfg()).unwrap();
        let mut img = Images::new(base_image(SID));
        for step in 0..ATTACHED_MAX_STATES as u8 {
            match next() % 8 {
                0 => fs.fail_before_rename(1),
                1 => fs.fail_after_rename(1),
                2 => fs.panic_next_write(next() & 1 == 0),
                3 => fs.fail_sync(1),
                4 => {
                    fs.crash(next() & 1 == 0);
                    store = UiStore::open(fs.clone(), SID, cfg()).unwrap();
                    img = Images::new(match load(&*fs, SID).unwrap() {
                        Loaded::Image(s) => *s,
                        _ => base_image(SID),
                    });
                }
                _ => {}
            }
            if img
                .commit(&mut store, Some(create(step, WaitMode::Live)))
                .is_ok()
            {
                let out = img.poll(&mut store, LONG);
                assert_ne!(out, Poll::Pending, "case {case} step {step}");
            }
        }
        for r in fs.renames() {
            if let (Some(before), Some(after)) = (r.before, r.after) {
                assert!(after > before, "case {case}: {r:?}");
            }
        }
    }
}

/// The writer dies (a panic) after or before its rename: the file's `seq`
/// is read back, and the commit is synced from a fresh writer or known
/// never published. A same-id retry and an attach before the outcome get
/// `pending` and a projection without the action.
#[test]
fn store_read_back_after_writer_panic() {
    for after_rename in [true, false] {
        let mut w = world();
        w.commit_ok(create(1, WaitMode::Live));
        w.fs.stall();
        w.fs.panic_next_write(after_rename);
        w.img.commit(&mut w.store, Some(accept(1, 1))).unwrap();
        assert_eq!(w.img.poll(&mut w.store, budget()), Poll::Pending);
        assert_eq!(
            w.img.admit(&w.store, &request(1, 1, Verb::Answer, 0, 0)),
            Err(Admission::Pending)
        );
        assert!(card(&w.img.projection(), 1).answer.is_none());
        w.fs.release();
        let out = w.img.poll(&mut w.store, LONG);
        if after_rename {
            assert_eq!(out, Poll::Done(Outcome::Durable));
            assert!(card(w.img.durable(), 1).answer.is_some());
        } else {
            assert!(
                matches!(out, Poll::Done(Outcome::NotPublished(_))),
                "{out:?}"
            );
            assert!(card(&w.img.live(), 1).answer.is_none());
        }
        // The fresh writer takes the next commit.
        w.commit_ok(Op::Close { card: sid(1) });
    }
}

// ── Projection (R3-H1) ───────────────────────────────────────────────────

/// An answer's commit is pending, the user Cancels, and the commit fails
/// before the rename: only the answer is removed; the epoch rise survives
/// and commits next.
#[test]
fn pending_commit_cancel_then_not_published() {
    let mut w = world();
    w.commit_ok(create(1, WaitMode::Live));
    w.fs.stall();
    w.fs.fail_before_rename(1);
    w.img.commit(&mut w.store, Some(accept(1, 1))).unwrap();
    w.img.note(Op::Stop(StopKind::User));
    assert_eq!(w.img.live().epoch, 1, "admission sees the stop");
    assert_eq!(w.img.projection().epoch, 0, "shown only once durable");
    assert!(card(&w.img.projection(), 1).answer.is_none());
    w.fs.release();
    let out = w.img.poll(&mut w.store, LONG);
    assert!(
        matches!(out, Poll::Done(Outcome::NotPublished(_))),
        "{out:?}"
    );
    let live = w.img.live();
    assert!(
        card(&live, 1).answer.is_none() && live.ledger.is_empty(),
        "the answer is gone"
    );
    assert_eq!(live.epoch, 1, "the stop is not");
    assert!(w.img.has_uncommitted());
    assert_eq!(w.commit(None), Poll::Done(Outcome::Durable));
    assert_eq!(w.img.durable().epoch, 1);
    assert!(card(w.img.durable(), 1).resume);
    assert_eq!(w.on_disk(SID).unwrap().epoch, 1);
}

/// `/new` joins the writer. Whatever the first outcome (and whether its
/// own clear lands or falls back to unlinking), no card or answer
/// reappears after `Cleared`.
#[test]
fn pending_commit_then_new() {
    for (first, clear) in [(true, true), (false, true), (true, false), (false, false)] {
        let mut w = world();
        w.commit_ok(create(1, WaitMode::Live));
        w.fs.stall();
        if !first {
            w.fs.fail_before_rename(1);
        }
        w.img.commit(&mut w.store, Some(accept(1, 1))).unwrap();
        assert_eq!(w.img.poll(&mut w.store, budget()), Poll::Pending);
        // `/new` joins the writer first.
        w.fs.release();
        let joined = w.img.poll(&mut w.store, LONG);
        assert!(matches!(joined, Poll::Done(_)), "{joined:?}");
        if !clear {
            w.fs.fail_before_rename(1);
        }
        match w.commit(Some(Op::Clear { at: hours_ago(0) })) {
            Poll::Done(Outcome::Durable) => assert!(w.img.projection().cards.is_empty()),
            Poll::Done(Outcome::NotPublished(_)) => {
                // The same writer unlinks `.ui`, then syncs the directory.
                w.store.remove(SID).unwrap();
                assert_eq!(w.store.poll(LONG), Poll::Done(Outcome::Durable));
                assert!(w.fs.get(&sidecar_name(SID)).is_none());
            }
            other => panic!("{other:?}"),
        }
        w.fs.crash(false);
        let plan = w.recover(SID, &history(&[]), None);
        let (cards, ledger) = plan
            .commit
            .as_ref()
            .map_or((0, 0), |s| (s.cards.len(), s.ledger.len()));
        assert_eq!((cards, ledger), (0, 0), "first {first}, clear {clear}");
        assert!(plan.deliver.is_empty());
    }
}

/// Create and close each resolve both ways, and an unknown outcome is
/// unconfirmed, never a refusal.
#[test]
fn create_and_close_unconfirmed() {
    for published in [true, false] {
        let mut w = world();
        w.fs.stall();
        if !published {
            w.fs.fail_before_rename(1);
        }
        w.img
            .commit(&mut w.store, Some(create(1, WaitMode::Live)))
            .unwrap();
        let p = w.img.poll(&mut w.store, budget());
        assert_eq!(Lifecycle::of(&p), Lifecycle::Unconfirmed);
        assert!(
            w.img.projection().card(&sid(1)).is_none(),
            "visible only once durable"
        );
        w.fs.release();
        let p = w.img.poll(&mut w.store, LONG);
        match Lifecycle::of(&p) {
            Lifecycle::Done => assert!(published),
            Lifecycle::Failed(_) => assert!(!published),
            Lifecycle::Unconfirmed => panic!("{p:?}"),
        }
        assert_eq!(w.img.projection().card(&sid(1)).is_some(), published);
    }
    for published in [true, false] {
        let mut w = world();
        w.commit_ok(create(1, WaitMode::Live));
        w.fs.stall();
        if !published {
            w.fs.fail_before_rename(1);
        }
        w.img
            .commit(&mut w.store, Some(Op::Close { card: sid(1) }))
            .unwrap();
        let p = w.img.poll(&mut w.store, budget());
        assert_eq!(Lifecycle::of(&p), Lifecycle::Unconfirmed);
        assert_eq!(
            card(&w.img.projection(), 1).phase,
            Phase::Open,
            "shown as open meanwhile"
        );
        w.fs.release();
        let p = w.img.poll(&mut w.store, LONG);
        let phase = card(&w.img.projection(), 1).phase;
        if published {
            assert_eq!(
                (Lifecycle::of(&p), phase),
                (Lifecycle::Done, Phase::Withdrawn)
            );
        } else {
            assert!(matches!(Lifecycle::of(&p), Lifecycle::Failed(_)));
            assert_eq!(phase, Phase::Open, "stays open, shown as open");
        }
    }
}

// ── Identity and migration (R2-H2) ───────────────────────────────────────

fn session_file(id: &str, generation: &str, messages: &[Value]) -> Value {
    json!({"id": id, "_journal_generation": generation, "api_messages": messages})
}

/// Snapshot rotations change the generation: the sidecar stays Related and
/// its open cards stay open.
#[test]
fn rotation_with_open_cards() {
    let mut w = world();
    w.commit_ok(create(1, WaitMode::Waiting));
    w.commit_ok(create(2, WaitMode::Live));
    let msgs = [json!({"role": "user", "content": "go"})];
    for generation in ["g-1", "g-2", "g-3"] {
        let plan = w.recover(SID, &facts_of(&session_file(SID, generation, &msgs)), None);
        assert_eq!(plan.class, Class::Related, "{generation}");
        let s = image(&plan);
        assert_eq!(card(s, 1).phase, Phase::Open);
        assert_eq!(card(s, 1).mode, WaitMode::Live);
        assert_eq!(card(s, 1).downgraded, Some(Downgrade::Crash));
        assert_eq!(card(s, 2).phase, Phase::Open);
        assert!(card(s, 2).resume);
    }
}

/// Undelivered answers across rotations: each is included once.
#[test]
fn rotation_with_undelivered_answers() {
    let mut w = world();
    w.commit_ok(create(1, WaitMode::Live));
    w.commit_ok(create(2, WaitMode::Live));
    w.commit_ok(accept(1, 1));
    w.commit_ok(accept(2, 2));
    w.commit_ok(deliver(2, Delivery::InContext));
    let plan = w.recover(SID, &facts_of(&session_file(SID, "g-1", &[])), None);
    assert_eq!(plan.class, Class::Related);
    assert_eq!(plan.deliver, [aid(1), aid(2)]);
    assert!(!plan.starts_turn());
    // Delivered, saved under a rotated generation: nothing more to deliver.
    w.fs.put(&sidecar_name(SID), &encode_sidecar(image(&plan)).unwrap());
    let msgs = [answer_msg(LINEAGE, 1, 1, 1), answer_msg(LINEAGE, 2, 2, 1)];
    let again = w.recover(SID, &facts_of(&session_file(SID, "g-2", &msgs)), None);
    assert_eq!(again.class, Class::Related);
    assert!(again.deliver.is_empty());
    assert_eq!(again.recorded, [aid(1), aid(2)]);
}

/// The history was saved with the answer, the sidecar's next commit was
/// not: the answer is Recorded, not delivered again.
#[test]
fn crash_between_files_history_first() {
    let mut w = world();
    w.commit_ok(create(1, WaitMode::Live));
    w.commit_ok(accept(1, 1));
    w.img.note(deliver(1, Delivery::InContext));
    w.fs.crash(false);
    let plan = w.recover(SID, &history(&[answer_msg(LINEAGE, 1, 1, 1)]), None);
    assert_eq!(plan.class, Class::Related);
    assert!(plan.deliver.is_empty());
    assert_eq!(plan.recorded, [aid(1)]);
    assert_eq!(
        image(&plan).record(&aid(1)).unwrap().delivery,
        Delivery::Recorded
    );
}

/// The sidecar knew the answer was in context, the history save never
/// came: it goes back into context once, with no wake.
#[test]
fn crash_between_files_sidecar_first() {
    let mut w = world();
    w.commit_ok(create(1, WaitMode::Live));
    w.commit_ok(accept(1, 1));
    w.commit_ok(deliver(1, Delivery::InContext));
    w.fs.crash(false);
    let plan = w.recover(SID, &history(&[]), None);
    assert_eq!(plan.class, Class::Related);
    assert_eq!(plan.deliver, [aid(1)]);
    assert!(!image(&plan).record(&aid(1)).unwrap().wake);
    assert!(!plan.starts_turn());
}

/// Switching persistence mode, both ways: the classification and the plan
/// don't change.
#[test]
fn json_and_journal_modes() {
    let mut w = world();
    w.commit_ok(create(1, WaitMode::Live));
    w.commit_ok(accept(1, 1));
    w.commit_ok(create(2, WaitMode::Live));
    w.commit_ok(accept(2, 2));
    let msgs = [answer_msg(LINEAGE, 1, 1, 1)];
    let json_mode = session_file(SID, "json-1", &msgs);
    let mut journal = session_file(SID, "journal-7", &msgs);
    journal["_journal"] = json!({"records": 12});
    let back_to_json = session_file(SID, "json-2", &msgs);
    let plans: Vec<Plan> = [json_mode, journal, back_to_json]
        .iter()
        .map(|s| w.recover(SID, &facts_of(s), None))
        .collect();
    for p in &plans {
        assert_eq!(p.class, Class::Related);
        assert_eq!(p.recorded, [aid(1)]);
        assert_eq!(p.deliver, [aid(2)]);
    }
    assert_eq!(plans[0], plans[1]);
    assert_eq!(plans[1], plans[2]);
}

/// A parent with one settled answer and one queued (carried), and the
/// history before and after its compaction.
fn compaction_world() -> World {
    let mut w = world();
    w.commit_ok(create(1, WaitMode::Live));
    w.commit_ok(accept(1, 1));
    w.commit_ok(deliver(1, Delivery::InContext));
    w.commit_ok(deliver(1, Delivery::Recorded));
    w.commit_ok(Op::Settle { answer_id: aid(1) });
    w.commit_ok(create(2, WaitMode::Live));
    w.commit_ok(accept(2, 2));
    w
}

fn before_compaction() -> Vec<Value> {
    vec![
        json!({"role": "user", "content": "go"}),
        answer_msg(LINEAGE, 1, 1, 1),
    ]
}

fn after_compaction() -> Vec<Value> {
    vec![json!({"role": "user", "content": "[summary]"})]
}

/// A crash before prepare, after prepare, after the history save and after
/// finish: one authority at every cut, the settled answer never comes
/// back, the queued one is delivered once.
#[test]
fn compaction_in_place_every_boundary() {
    let mut w = compaction_world();
    let mut cuts: Vec<(&str, Vec<u8>, HistoryFacts)> = Vec::new();
    let grab = |w: &World| w.fs.get(&sidecar_name(SID)).unwrap();
    cuts.push(("before prepare", grab(&w), history(&before_compaction())));
    let to = HandoffTo {
        session_id: SID.into(),
        base: DIGEST.into(),
    };
    let (op, successor) = prepare(w.img.durable(), to, HX).unwrap();
    assert!(successor.is_none());
    w.commit_ok(op);
    cuts.push(("after prepare", grab(&w), history(&before_compaction())));
    // The compaction is applied and saved: the history starts at DIGEST.
    let applied = history_of(SID, Some(DIGEST), &after_compaction());
    cuts.push(("after the history save", grab(&w), applied.clone()));
    w.commit_ok(finish_in_place(w.img.durable()));
    cuts.push(("after finish", grab(&w), applied));
    for (what, bytes, h) in &cuts {
        let loaded = Loaded::Image(Box::new(decode_sidecar(bytes).unwrap()));
        let plan = recover(&loaded, h, None, now());
        assert!(plan.is_authority(), "{what}: {plan:?}");
        let s = image(&plan);
        assert!(s.compacting.is_none(), "{what}");
        assert_eq!(s.base, h.base, "{what}");
        assert_eq!(plan.deliver, [aid(2)], "{what}: the queued answer, once");
        assert!(plan.recorded.is_empty(), "{what}");
    }
}

fn parent_facts(w: &World, compacted_into: Option<&str>) -> ParentFacts {
    ParentFacts::Read {
        compacted_into: compacted_into.map(str::to_string),
        sidecar: w.on_disk(SID).map(Box::new),
    }
}

/// The successor handoff, cut before prepare, after prepare, after the
/// successor's save, after the parent's save and after finish. Both ids
/// load at every cut; at most one is the active card authority, and
/// exactly one is.
#[test]
fn compaction_successor_every_boundary() {
    let mut w = compaction_world();
    let parent_before = history(&before_compaction());
    let successor_history = history_of(SID2, Some(DIGEST), &after_compaction());
    let linked = HistoryFacts {
        compacted_into: Some(SID2.into()),
        ..parent_before.clone()
    };
    let check = |w: &World, what: &str, parent_h: &HistoryFacts, linked_now: bool| {
        let p = w.recover(SID, parent_h, None);
        let pf = parent_facts(w, linked_now.then_some(SID2));
        let s = w.recover(SID2, &successor_history, Some(&pf));
        let authorities = [p.is_authority(), s.is_authority()];
        assert_eq!(
            authorities.iter().filter(|a| **a).count(),
            1,
            "{what}: {p:?} {s:?}"
        );
        (p, s)
    };
    let (p, s) = check(&w, "before prepare", &parent_before, false);
    assert!(p.is_authority() && s.class == Class::Missing);

    let to = HandoffTo {
        session_id: SID2.into(),
        base: DIGEST.into(),
    };
    let (op, successor) = prepare(w.img.durable(), to, HX).unwrap();
    w.commit_ok(op);
    let mut successor = successor.unwrap();
    w.store.commit(&mut successor).unwrap();
    assert_eq!(w.store.poll(LONG), Poll::Done(Outcome::Durable));
    let (p, s) = check(&w, "after prepare", &parent_before, false);
    assert!(p.is_authority());
    assert_eq!(
        p.files,
        [FileAction::RemoveIfPrepared {
            session_id: SID2.into(),
            hx: HX.into()
        }]
    );
    assert_eq!(
        s.files,
        [FileAction::Remove(sidecar_name(SID2))],
        "not active: removed"
    );

    // The successor's history is saved; the parent isn't linked yet.
    let (p, _) = check(&w, "after the successor save", &parent_before, false);
    assert!(p.is_authority());

    // The parent's save gains `compacted_into`: the activation.
    let (p, s) = check(&w, "after the parent save", &linked, true);
    assert_eq!(p.class, Class::Moved);
    assert!(s.activated && s.is_authority());
    assert_eq!(s.deliver, [aid(2)], "the carried answer, once");

    // Finish: the successor's `prepared` cleared, then `moved_to` on the parent.
    w.store.rebind(SID2);
    let mut succ = Images::new(w.on_disk(SID2).unwrap());
    succ.commit(&mut w.store, Some(finish_successor(succ.durable())))
        .unwrap();
    assert_eq!(succ.poll(&mut w.store, LONG), Poll::Done(Outcome::Durable));
    w.commit_ok(moved(w.img.durable()));
    let (p, s) = check(&w, "after finish", &linked, true);
    assert_eq!(p.class, Class::Moved);
    assert!(s.is_authority() && !s.activated, "already active");
}

/// An answer accepted just before Prepare is in the prepared image; one
/// sent just after is `busy`, and lands on the active owner after Finish
/// (or on the parent after a rollback).
#[test]
fn compaction_answer_around_prepare() {
    for success in [true, false] {
        let mut w = world();
        w.commit_ok(create(1, WaitMode::Live));
        w.commit_ok(create(2, WaitMode::Live));
        w.commit_ok(accept(1, 1));
        let to = HandoffTo {
            session_id: SID2.into(),
            base: DIGEST.into(),
        };
        let (op, successor) = prepare(w.img.durable(), to, HX).unwrap();
        w.commit_ok(op);
        let mut successor = successor.unwrap();
        assert!(successor.record(&aid(1)).is_some(), "in the cut");
        w.store.commit(&mut successor).unwrap();
        assert_eq!(w.store.poll(LONG), Poll::Done(Outcome::Durable));
        let late = request(2, 2, Verb::Answer, 0, 0);
        assert_eq!(w.img.admit(&w.store, &late), Err(Admission::Busy), "fenced");
        if success {
            w.store.rebind(SID2);
            let mut succ = Images::new(w.on_disk(SID2).unwrap());
            succ.commit(&mut w.store, Some(finish_successor(succ.durable())))
                .unwrap();
            assert_eq!(succ.poll(&mut w.store, LONG), Poll::Done(Outcome::Durable));
            w.commit_ok(moved(w.img.durable()));
            assert_eq!(succ.admit(&w.store, &late), Ok(()));
            succ.commit(&mut w.store, Some(accept(2, 2))).unwrap();
            assert_eq!(succ.poll(&mut w.store, LONG), Poll::Done(Outcome::Durable));
            assert!(w.on_disk(SID2).unwrap().record(&aid(2)).is_some());
            assert!(w.on_disk(SID).unwrap().record(&aid(2)).is_none());
        } else {
            w.store.remove(SID2).unwrap();
            assert_eq!(w.store.poll(LONG), Poll::Done(Outcome::Durable));
            w.commit_ok(rollback(w.img.durable()));
            assert_eq!(w.img.admit(&w.store, &late), Ok(()));
            w.commit_ok(accept(2, 2));
            assert!(w.on_disk(SID).unwrap().record(&aid(2)).is_some());
        }
    }
}

/// A failure before the parent link: the parent's writer removes the
/// successor's sidecar, then commits the cleared `compacting`. A crash at
/// either point leaves the parent the only authority, and no orphan.
#[test]
fn compaction_successor_rollback() {
    let mut w = compaction_world();
    let parent_h = history(&before_compaction());
    let successor_h = history_of(SID2, Some(DIGEST), &after_compaction());
    let to = HandoffTo {
        session_id: SID2.into(),
        base: DIGEST.into(),
    };
    let (op, successor) = prepare(w.img.durable(), to, HX).unwrap();
    w.commit_ok(op);
    let mut successor = successor.unwrap();
    w.store.commit(&mut successor).unwrap();
    assert_eq!(w.store.poll(LONG), Poll::Done(Outcome::Durable));

    // A crash before the rollback: recovery removes the prepared orphan.
    let plan = w.recover(SID, &parent_h, None);
    assert!(plan.is_authority());
    apply_files(&*w.fs, &plan.files).unwrap();
    assert!(w.fs.get(&sidecar_name(SID2)).is_none());
    w.fs.put(&sidecar_name(SID2), &encode_sidecar(&successor).unwrap());

    // The rollback: the successor goes first, then `compacting`.
    w.store.remove(SID2).unwrap();
    assert_eq!(w.store.poll(LONG), Poll::Done(Outcome::Durable));
    let mid = w.recover(SID, &parent_h, None);
    assert!(
        mid.is_authority(),
        "a crash between the two: still the parent"
    );
    w.commit_ok(rollback(w.img.durable()));
    assert!(w.img.durable().compacting.is_none());
    assert!(w.fs.get(&sidecar_name(SID2)).is_none());
    let p = w.recover(SID, &parent_h, None);
    let s = w.recover(SID2, &successor_h, Some(&parent_facts(&w, None)));
    assert!(p.is_authority() && !s.is_authority());
    assert_eq!(s.class, Class::Missing);
    assert!(p.files.is_empty());
}

/// Another lineage (or stem) is Foreign; an older backup without the
/// `newest_settled` mark (or another base) is Restored. Neither delivers
/// anything, and the fresh sidecar is Related on the next load.
#[test]
fn foreign_and_restored_history() {
    let mut w = compaction_world();
    w.commit_ok(create(3, WaitMode::Waiting));
    let cases = [
        (
            "foreign lineage",
            history(&[answer_msg(OTHER_LINEAGE, 1, 1, 1)]),
            Class::Foreign,
        ),
        (
            "foreign stem",
            history_of(SID2, None, &before_compaction()),
            Class::Foreign,
        ),
        (
            "restored: no newest_settled mark",
            history(&[]),
            Class::Restored,
        ),
        (
            "restored: another base",
            history_of(SID, Some(DIGEST), &before_compaction()),
            Class::Restored,
        ),
    ];
    for (what, h, class) in cases {
        let loaded = load(&*w.fs, SID).unwrap();
        let plan = recover(&loaded, &h, None, now());
        assert_eq!(plan.class, class, "{what}");
        assert!(
            plan.deliver.is_empty() && plan.recorded.is_empty(),
            "{what}"
        );
        assert!(!plan.starts_turn());
        let s = image(&plan);
        assert!(s.ledger.is_empty(), "{what}");
        assert_eq!(s.session_id, h.session_id);
        assert!(
            s.cards
                .iter()
                .all(|c| c.state.phase == Phase::Unknown && c.state.offers.is_empty()),
            "{what}"
        );
        assert!(s.card(&sid(3)).is_some(), "{what}: the open card, unknown");
        assert!(
            s.card(&sid(2)).is_some(),
            "{what}: the unsettled answer, unknown"
        );
        if let Some(m) = h.marks.first() {
            assert_eq!(s.lineage, m.lineage(), "{what}");
        }
        let next = recover(&Loaded::Image(Box::new(s.clone())), &h, None, now());
        assert_eq!(next.class, Class::Related, "{what}: the next load");
    }
}

// ── Settling (R2-H3) ─────────────────────────────────────────────────────

/// The history was saved, the settle commit failed: the record stays
/// Recorded, compaction refuses, and the mark stays.
#[test]
fn saved_history_failed_settle() {
    let mut w = world();
    let mut receipts = SaveReceipts::default();
    w.commit_ok(create(1, WaitMode::Live));
    w.commit_ok(accept(1, 1));
    w.commit_ok(deliver(1, Delivery::InContext));
    let ticket = receipts.request(SID);
    receipts.applied(SID, SaveKind::History, receipts.issued(), true);
    assert_eq!(receipts.receipt(&ticket), Receipt::Saved);
    w.img.note(deliver(1, Delivery::Recorded));
    w.fs.fail_before_rename(1);
    let out = w.commit(Some(Op::Settle { answer_id: aid(1) }));
    assert!(
        matches!(out, Poll::Done(Outcome::NotPublished(_))),
        "{out:?}"
    );
    let live = w.img.live();
    assert_eq!(live.record(&aid(1)).unwrap().delivery, Delivery::Recorded);
    let to = HandoffTo {
        session_id: SID.into(),
        base: DIGEST.into(),
    };
    assert_eq!(prepare(&live, to, HX).unwrap_err(), HandoffError::Unsettled);
    assert_eq!(
        HandoffError::Unsettled.to_string(),
        "card answers not yet saved"
    );
    // The mark stays: the next load reads it and records, never re-inserts.
    let plan = w.recover(SID, &history(&[answer_msg(LINEAGE, 1, 1, 1)]), None);
    assert_eq!(plan.recorded, [aid(1)]);
    assert!(plan.deliver.is_empty());
}

/// One successful save of the session, started after the tickets, covers
/// all of them.
#[test]
fn coalesced_saves_satisfy_tickets() {
    let mut r = SaveReceipts::default();
    let tickets: Vec<Ticket> = (0..3).map(|_| r.request(SID)).collect();
    assert!(tickets.iter().all(|t| r.receipt(t) == Receipt::NotYet));
    r.applied(SID, SaveKind::History, r.issued(), true);
    assert!(tickets.iter().all(|t| r.receipt(t) == Receipt::Saved));
    let later = r.request(SID);
    assert_eq!(
        r.receipt(&later),
        Receipt::NotYet,
        "a save never covers a later ticket"
    );
    // Through the ledger: two records, one save, both Recorded.
    let mut w = world();
    w.commit_ok(create(1, WaitMode::Live));
    w.commit_ok(create(2, WaitMode::Live));
    w.commit_ok(accept(1, 1));
    w.commit_ok(accept(2, 2));
    let mut by_answer = HashMap::new();
    for n in [1, 2] {
        w.img.note(deliver(n, Delivery::InContext));
        by_answer.insert(aid(n), r.request(SID));
    }
    r.applied(SID, SaveKind::History, r.issued(), true);
    for (answer, t) in &by_answer {
        assert_eq!(r.receipt(t), Receipt::Saved);
        w.img.note(Op::Deliver {
            answer_id: answer.clone(),
            to: Delivery::Recorded,
        });
    }
    assert_eq!(w.commit(None), Poll::Done(Outcome::Durable));
    assert!(w
        .img
        .durable()
        .ledger
        .iter()
        .all(|x| x.delivery == Delivery::Recorded));
}

/// A failed save, a draft save, another session's save and a save that
/// started before the ticket never satisfy it.
#[test]
fn failed_save_never_satisfies_ticket() {
    let mut r = SaveReceipts::default();
    let early = r.issued();
    let t = r.request(SID);
    r.applied(SID, SaveKind::History, r.issued(), false);
    assert_eq!(r.receipt(&t), Receipt::NotYet, "a failed save");
    r.applied(SID, SaveKind::Draft, r.issued(), true);
    assert_eq!(r.receipt(&t), Receipt::NotYet, "a draft save");
    r.applied(SID2, SaveKind::History, r.issued(), true);
    assert_eq!(r.receipt(&t), Receipt::NotYet, "another session's save");
    r.applied(SID, SaveKind::History, early, true);
    assert_eq!(
        r.receipt(&t),
        Receipt::NotYet,
        "a save from before the ticket"
    );
    r.applied(SID, SaveKind::History, r.issued(), true);
    assert_eq!(r.receipt(&t), Receipt::Saved);
}

/// A settle queued behind a fresh answer's commit can't lose the answer,
/// and a failed progress commit doesn't either.
#[test]
fn progress_commit_never_overwrites_admission() {
    let mut w = world();
    w.commit_ok(create(1, WaitMode::Live));
    w.commit_ok(create(2, WaitMode::Live));
    w.commit_ok(accept(1, 1));
    w.commit_ok(deliver(1, Delivery::Recorded));
    w.fs.stall();
    w.img.commit(&mut w.store, Some(accept(2, 2))).unwrap();
    w.img.note(Op::Settle { answer_id: aid(1) });
    w.fs.release();
    assert_eq!(w.img.poll(&mut w.store, LONG), Poll::Done(Outcome::Durable));
    assert_eq!(w.commit(None), Poll::Done(Outcome::Durable));
    let d = w.img.durable();
    assert_eq!(d.record(&aid(1)).unwrap().delivery, Delivery::Settled);
    assert!(
        d.record(&aid(2)).is_some() && card(d, 2).answer.is_some(),
        "the answer stands"
    );
    assert_eq!(d.newest_settled.as_deref(), Some(aid(1).as_str()));
    w.img.note(deliver(2, Delivery::InContext));
    w.fs.fail_before_rename(1);
    let out = w.commit(None);
    assert!(
        matches!(out, Poll::Done(Outcome::NotPublished(_))),
        "{out:?}"
    );
    assert!(card(&w.img.live(), 2).answer.is_some(), "still there");
    assert!(card(&w.on_disk(SID).unwrap(), 2).answer.is_some());
}

/// A waiter took the answer and the history wasn't saved: recovery
/// includes it once, with no wake.
#[test]
fn consumed_before_history_save() {
    let mut w = world();
    w.commit_ok(create(1, WaitMode::Waiting));
    w.commit_ok(accept(1, 1));
    w.commit_ok(deliver(1, Delivery::Handed));
    w.fs.crash(false);
    let plan = w.recover(SID, &history(&[]), None);
    assert_eq!(plan.deliver, [aid(1)]);
    assert!(!plan.starts_turn());
    let s = image(&plan);
    assert!(!s.record(&aid(1)).unwrap().wake);
    assert!(!card(s, 1).answer.as_ref().unwrap().wake);
    // Its message saved with the mark: never inserted a second time.
    w.fs.put(&sidecar_name(SID), &encode_sidecar(s).unwrap());
    let again = w.recover(SID, &history(&[answer_msg(LINEAGE, 1, 1, 1)]), None);
    assert!(again.deliver.is_empty());
    assert_eq!(again.recorded, [aid(1)]);
}

/// A compaction drops history marks only for settled records: with one in
/// context it refuses; once settled, no cut (prepared, applied, finished)
/// brings the answer back.
#[test]
fn mark_removal_at_every_compaction_boundary() {
    let mut w = world();
    let mut receipts = SaveReceipts::default();
    w.commit_ok(create(1, WaitMode::Live));
    w.commit_ok(accept(1, 1));
    w.commit_ok(deliver(1, Delivery::InContext));
    let to = HandoffTo {
        session_id: SID.into(),
        base: DIGEST.into(),
    };
    assert_eq!(
        prepare(w.img.durable(), to.clone(), HX).unwrap_err(),
        HandoffError::Unsettled
    );
    let t = receipts.request(SID);
    receipts.applied(SID, SaveKind::History, receipts.issued(), true);
    assert_eq!(receipts.receipt(&t), Receipt::Saved);
    w.commit_ok(deliver(1, Delivery::Recorded));
    w.commit_ok(Op::Settle { answer_id: aid(1) });
    let (op, _) = prepare(w.img.durable(), to, HX).unwrap();
    let marked = history(&[answer_msg(LINEAGE, 1, 1, 1)]);
    let unmarked = history_of(SID, Some(DIGEST), &after_compaction());
    w.commit_ok(op);
    let prepared = w.on_disk(SID).unwrap();
    w.commit_ok(finish_in_place(w.img.durable()));
    let finished = w.on_disk(SID).unwrap();
    for (what, s, h) in [
        ("prepared, not applied", &prepared, &marked),
        ("applied: the mark is gone", &prepared, &unmarked),
        ("finished", &finished, &unmarked),
    ] {
        let plan = recover(&Loaded::Image(Box::new(s.clone())), h, None, now());
        assert_eq!(plan.class, Class::Related, "{what}");
        assert!(plan.deliver.is_empty(), "{what}: never re-inserted");
    }
}

/// Revision 2 is accepted while revision 1 is in context; both settle by
/// their own marks.
#[test]
fn revision_keeps_unsettled_proof() {
    let mut w = world();
    w.commit_ok(create(1, WaitMode::Live));
    w.commit_ok(accept_with(
        1,
        1,
        Verb::Answer,
        1,
        true,
        0,
        values(),
        vec![Verb::Revise],
    ));
    w.commit_ok(deliver(1, Delivery::InContext));
    let revise = ActionRequest {
        hash: action_hash(Verb::Revise, 1, 0, &other_values()),
        ..request(1, 2, Verb::Revise, 1, 0)
    };
    assert_eq!(w.img.admit(&w.store, &revise), Ok(()));
    w.commit_ok(accept_with(
        1,
        2,
        Verb::Revise,
        2,
        true,
        0,
        other_values(),
        vec![Verb::Revise],
    ));
    let d = w.img.durable();
    assert_eq!(
        d.record(&aid(1)).unwrap().delivery,
        Delivery::InContext,
        "kept, not replaced"
    );
    assert_eq!(card(d, 1).answer.as_ref().unwrap().answer_id, aid(2));
    let one = w.recover(SID, &history(&[answer_msg(LINEAGE, 1, 1, 1)]), None);
    assert_eq!(
        (one.recorded.clone(), one.deliver.clone()),
        (vec![aid(1)], vec![aid(2)])
    );
    let both = w.recover(
        SID,
        &history(&[answer_msg(LINEAGE, 1, 1, 1), answer_msg(LINEAGE, 1, 2, 2)]),
        None,
    );
    assert_eq!(both.recorded, [aid(1), aid(2)]);
    for n in [1, 2] {
        w.commit_ok(deliver(n, Delivery::Recorded));
        w.commit_ok(Op::Settle { answer_id: aid(n) });
        assert_eq!(
            w.img.durable().record(&aid(n)).unwrap().delivery,
            Delivery::Settled
        );
    }
    assert!(w.img.durable().all_settled());
}

// ── Lifecycle (R2-H4), the recovery half ─────────────────────────────────

/// A durable close survives a crash, twice.
#[test]
fn close_durable_then_crash() {
    let mut w = world();
    w.commit_ok(create(1, WaitMode::Live));
    w.commit_ok(Op::Close { card: sid(1) });
    w.fs.crash(false);
    let plan = w.recover(SID, &history(&[]), None);
    assert_eq!(card(image(&plan), 1).phase, Phase::Withdrawn);
    let again = recover(
        &Loaded::Image(Box::new(image(&plan).clone())),
        &history(&[]),
        None,
        now(),
    );
    assert_eq!(card(image(&again), 1).phase, Phase::Withdrawn);
    assert!(card(image(&again), 1).offers.is_empty());
}

/// `/new` then a crash: nothing comes back. The delete fallback (the clear
/// commit failed, `.ui` unlinked) loads as Missing, "not recorded", never
/// as open.
#[test]
fn new_then_crash() {
    let mut w = world();
    w.commit_ok(create(1, WaitMode::Live));
    w.commit_ok(accept(1, 1));
    w.commit_ok(Op::Clear { at: hours_ago(0) });
    w.fs.crash(false);
    let plan = w.recover(SID, &history(&[]), None);
    assert!(image(&plan).cards.is_empty() && image(&plan).ledger.is_empty());

    let mut w = world();
    w.commit_ok(create(1, WaitMode::Live));
    w.commit_ok(accept(1, 1));
    w.fs.fail_before_rename(1);
    assert!(matches!(
        w.commit(Some(Op::Clear { at: hours_ago(0) })),
        Poll::Done(Outcome::NotPublished(_))
    ));
    w.store.remove(SID).unwrap();
    assert_eq!(w.store.poll(LONG), Poll::Done(Outcome::Durable));
    w.fs.crash(false);
    let h = history(&[answer_msg(LINEAGE, 1, 1, 1)]);
    let plan = w.recover(SID, &h, None);
    assert_eq!(plan.class, Class::Missing);
    assert!(plan.deliver.is_empty());
    assert!(
        plan.commit.as_ref().is_none_or(|s| s.cards.is_empty()),
        "never open"
    );
}

/// An expired card stays expired; an open card past 24 h expires at
/// recovery and stays so.
#[test]
fn expiry_then_restart() {
    let mut w = world();
    w.commit_ok(create(1, WaitMode::Live));
    w.commit_ok(Op::Create(Box::new(card_raised(
        &sid(2),
        WaitMode::Live,
        hours_ago(25),
    ))));
    w.commit_ok(Op::Expire { card: sid(1) });
    w.fs.crash(false);
    let plan = w.recover(SID, &history(&[]), None);
    assert_eq!(card(image(&plan), 1).phase, Phase::Expired);
    assert_eq!(card(image(&plan), 2).phase, Phase::Expired, "past 24 h");
    let again = recover(
        &Loaded::Image(Box::new(image(&plan).clone())),
        &history(&[]),
        None,
        now(),
    );
    assert_eq!(card(image(&again), 1).phase, Phase::Expired);
    assert_eq!(card(image(&again), 2).phase, Phase::Expired);
}

/// A crash with a waiting card gives the state a committed downgrade
/// gives, so a timeout's `pending`, sent before a failed write, stays
/// consistent.
#[test]
fn downgrade_is_recovery_equivalent() {
    let mut a = world();
    a.commit_ok(create(1, WaitMode::Waiting));
    a.commit_ok(Op::Downgrade(Downgrade::Timeout));
    a.fs.crash(false);
    let committed = a.recover(SID, &history(&[]), None);

    let mut b = world();
    b.commit_ok(create(1, WaitMode::Waiting));
    b.img.note(Op::Downgrade(Downgrade::Timeout));
    assert_eq!(
        card(&b.img.projection(), 1).mode,
        WaitMode::Live,
        "shown before it is durable"
    );
    b.fs.fail_before_rename(1);
    assert!(matches!(
        b.commit(None),
        Poll::Done(Outcome::NotPublished(_))
    ));
    b.fs.crash(false);
    let crashed = b.recover(SID, &history(&[]), None);

    let (x, y) = (card(image(&committed), 1), card(image(&crashed), 1));
    assert_eq!(
        (x.mode, x.phase, &x.offers, x.resume),
        (y.mode, y.phase, &y.offers, y.resume)
    );
    assert_eq!(x.mode, WaitMode::Live);
    assert_eq!(image(&committed).epoch, image(&crashed).epoch);
}

/// The reload barrier fails when the store or the history receipt fails,
/// or the in-flight outcome is unknown at its budget; else its image is
/// durable and flagged `checkpointed`.
#[test]
fn checkpoint_barrier_reports_failure() {
    let setup = || {
        let mut w = world();
        w.commit_ok(create(1, WaitMode::Live));
        w.commit_ok(accept(1, 1));
        w.commit_ok(deliver(1, Delivery::InContext));
        w
    };
    let mut receipts = SaveReceipts::default();
    let mut tickets = HashMap::new();
    tickets.insert(aid(1), receipts.request(SID));

    // The history receipt is missing.
    let mut w = setup();
    let budget = budget();
    let r = checkpoint_barrier(&mut w.img, &mut w.store, &tickets, &receipts, budget);
    assert_eq!(r, Err(BarrierFailure::History));

    // The store fails.
    receipts.applied(SID, SaveKind::History, receipts.issued(), true);
    let mut w = setup();
    w.fs.fail_before_rename(1);
    let r = checkpoint_barrier(&mut w.img, &mut w.store, &tickets, &receipts, budget);
    assert!(matches!(r, Err(BarrierFailure::UiStore(_))), "{r:?}");

    // The in-flight commit is still unknown at the budget.
    let mut w = setup();
    w.fs.stall();
    w.img
        .commit(&mut w.store, Some(create(2, WaitMode::Live)))
        .unwrap();
    let r = checkpoint_barrier(&mut w.img, &mut w.store, &tickets, &receipts, budget);
    assert!(matches!(r, Err(BarrierFailure::UiStore(_))), "{r:?}");
    w.fs.release();
    let _ = w.img.poll(&mut w.store, LONG);

    // Both hold.
    let mut w = setup();
    assert_eq!(
        checkpoint_barrier(&mut w.img, &mut w.store, &tickets, &receipts, budget),
        Ok(())
    );
    let d = w.img.durable();
    assert!(d.checkpointed);
    assert_eq!(d.record(&aid(1)).unwrap().delivery, Delivery::Recorded);
    // A checkpointed image recovers without a second epoch rise.
    let plan = w.recover(SID, &history(&[answer_msg(LINEAGE, 1, 1, 1)]), None);
    assert_eq!(image(&plan).epoch, d.epoch);
}

// ── #440 review, round 2 ─────────────────────────────────────────────────

/// H1: the writer dies and its read-back fails (EIO), for a write after or
/// before its rename and a removal after or before its unlink. A failed
/// read proves nothing: the outcome is unknown (`Unsynced`, the store
/// failed closed), never `NotPublished` and never a durable removal. The
/// change stays pending: no rollback, and no new admission.
#[test]
fn store_read_back_error_is_not_evidence() {
    for after in [true, false] {
        let mut w = world();
        w.commit_ok(create(1, WaitMode::Live));
        w.fs.panic_next_write(after);
        w.fs.fail_read(1);
        w.img.commit(&mut w.store, Some(accept(1, 1))).unwrap();
        let out = w.img.poll(&mut w.store, LONG);
        assert!(
            matches!(out, Poll::Done(Outcome::Unsynced(_))),
            "write, after {after}: {out:?}"
        );
        assert_eq!(Lifecycle::of(&out), Lifecycle::Unconfirmed);
        assert!(w.store.failure().is_some(), "failed closed");
        assert!(
            matches!(w.img.pending_op(), Some(Op::Accept(_))),
            "kept pending, never rolled back"
        );
        assert!(card(&w.img.live(), 1).answer.is_some());
        assert!(card(&w.img.projection(), 1).answer.is_none());
        assert_eq!(
            w.img.admit(&w.store, &request(1, 1, Verb::Answer, 0, 0)),
            Err(Admission::Pending)
        );
        let other = w.img.admit(&w.store, &request(1, 2, Verb::Answer, 0, 0));
        assert!(matches!(other, Err(Admission::Storage(_))), "{other:?}");
        let next = w.img.commit(&mut w.store, Some(Op::Close { card: sid(1) }));
        assert!(matches!(next, Err(Refusal::Storage(_))), "{next:?}");

        let mut w = world();
        w.commit_ok(create(1, WaitMode::Live));
        w.fs.panic_next_remove(after);
        w.fs.fail_read(1);
        w.store.remove(SID).unwrap();
        let out = w.store.poll(LONG);
        assert!(
            matches!(out, Poll::Done(Outcome::Unsynced(_))),
            "remove, after {after}: {out:?}"
        );
        assert_eq!(Lifecycle::of(&out), Lifecycle::Unconfirmed);
        assert!(w.store.failure().is_some(), "failed closed");
    }
    // A read-back that does read decides, as before: a removal the file
    // outlived never happened; one it didn't, did.
    for after in [true, false] {
        let mut w = world();
        w.commit_ok(create(1, WaitMode::Live));
        w.fs.panic_next_remove(after);
        w.store.remove(SID).unwrap();
        let out = w.store.poll(LONG);
        if after {
            assert_eq!(out, Poll::Done(Outcome::Durable));
            assert!(w.fs.get(&sidecar_name(SID)).is_none());
        } else {
            assert!(
                matches!(out, Poll::Done(Outcome::NotPublished(_))),
                "{out:?}"
            );
            assert!(w.fs.get(&sidecar_name(SID)).is_some());
        }
        assert!(w.store.failure().is_none());
    }
}

/// R2-M1: a read-back that reads but proves nothing about the in-flight
/// change (a file over the read bound, bytes without a `seq`, a `seq` past
/// a write's, or at or past a removal's own, which no file ever holds)
/// leaves the outcome unknown, for both kinds of job: `Unsynced`, the
/// store failed closed, nothing admitted after it. The file was put there
/// behind the writer's back, as by another process.
#[test]
fn store_read_back_without_evidence_fails_closed() {
    let at = |seq: u64| json!({"v": 1, "seq": seq}).to_string().into_bytes();
    for remove in [false, true] {
        let mut cases = vec!["oversized", "no seq", "newer seq"];
        if remove {
            cases.push("its own seq");
        }
        for what in cases {
            let mut w = world();
            w.commit_ok(create(1, WaitMode::Live));
            let next = w.store.last_seq() + 1;
            let bytes = match what {
                "oversized" => vec![b' '; SIDECAR_READ_MAX_BYTES + 1],
                "no seq" => b"{\"v\":1}".to_vec(),
                "newer seq" => at(next + 1),
                _ => at(next),
            };
            w.fs.put(&sidecar_name(SID), &bytes);
            let out = if remove {
                w.fs.panic_next_remove(false);
                w.store.remove(SID).unwrap();
                w.store.poll(LONG)
            } else {
                w.fs.panic_next_write(false);
                w.img.commit(&mut w.store, Some(accept(1, 1))).unwrap();
                w.img.poll(&mut w.store, LONG)
            };
            let case = format!("remove {remove}, {what}");
            assert!(
                matches!(out, Poll::Done(Outcome::Unsynced(_))),
                "{case}: {out:?}"
            );
            assert_eq!(Lifecycle::of(&out), Lifecycle::Unconfirmed, "{case}");
            assert!(w.store.failure().is_some(), "{case}: failed closed");
            assert!(
                matches!(w.store.admit(), Err(Refusal::Storage(_))),
                "{case}: nothing admitted"
            );
            assert_eq!(w.store.last_seq(), next, "{case}: seq kept");
            assert_eq!(w.fs.get(&sidecar_name(SID)), Some(bytes), "{case}");
            if !remove {
                assert!(
                    matches!(w.img.pending_op(), Some(Op::Accept(_))),
                    "{case}: kept pending, never rolled back"
                );
                assert_eq!(
                    w.img.admit(&w.store, &request(1, 1, Verb::Answer, 0, 0)),
                    Err(Admission::Pending),
                    "{case}"
                );
            }
        }
    }
    // An older seq still decides: the removal never happened.
    let mut w = world();
    w.commit_ok(create(1, WaitMode::Live));
    w.fs.put(&sidecar_name(SID), &at(w.store.last_seq() - 1));
    w.fs.panic_next_remove(false);
    w.store.remove(SID).unwrap();
    let out = w.store.poll(LONG);
    assert!(
        matches!(out, Poll::Done(Outcome::NotPublished(_))),
        "{out:?}"
    );
    assert!(w.store.failure().is_none());
}

/// v3.3 #2a: an image over its bounds (a create that would make 13 cards)
/// is refused as `Bounds`: nothing is written, the store isn't failed, and
/// the next valid commit is Durable.
#[test]
fn over_bounds_image_is_refused_as_bounds() {
    let mut w = world();
    for n in 0..ATTACHED_MAX_STATES as u8 {
        w.commit_ok(create(n, WaitMode::Live));
    }
    let writes = w.fs.renames().len();
    let seq = w.store.last_seq();
    let r = w.img.commit(&mut w.store, Some(create(99, WaitMode::Live)));
    assert!(matches!(r, Err(Refusal::Bounds(_))), "{r:?}");
    assert_eq!(w.fs.renames().len(), writes, "nothing written");
    assert_eq!(w.store.last_seq(), seq);
    assert!(w.store.failure().is_none(), "not a failed store");
    assert!(w.img.live().card(&sid(99)).is_none());
    w.commit_ok(Op::Close { card: sid(0) });
    assert_eq!(w.on_disk(SID).unwrap().seq, seq + 1);
}

/// M3 / v3.3 #1: a successful `/new` keeps every card's action log as a
/// tombstone stamped with the clear's time. A retry of an accepted action,
/// before and after a restart, gets its original outcome (`duplicate`);
/// no card is open or deliverable; the tombstones already there stay.
#[test]
fn new_keeps_the_retry_logs() {
    let mut w = world();
    w.commit_ok(create(1, WaitMode::Live));
    w.commit_ok(accept(1, 1));
    w.commit_ok(create(2, WaitMode::Live));
    let old = Tombstone {
        id: sid(7),
        at: hours_ago(2),
        log: Vec::new(),
    };
    let stale = Tombstone {
        id: sid(8),
        at: hours_ago(8 * 24),
        log: Vec::new(),
    };
    let mut seeded = w.img.durable().clone();
    seeded.tombstones = vec![stale, old.clone()];
    w.img = Images::new(seeded);
    w.commit_ok(Op::Clear { at: hours_ago(0) });
    let retry = request(1, 1, Verb::Answer, 0, 0);
    let check = |s: &Sidecar| {
        assert!(s.cards.is_empty() && s.ledger.is_empty(), "{s:?}");
        let ids: Vec<&str> = s.tombstones.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            ids,
            [sid(7), sid(1), sid(2)],
            "kept, then the cleared cards; past 7 days gone"
        );
        assert_eq!(s.tombstones[1].at, hours_ago(0));
        assert_eq!(s.tombstones[1].log.len(), 1);
    };
    check(w.img.durable());
    assert_eq!(
        w.img.admit(&w.store, &retry),
        Err(Admission::Duplicate { rev: 1 })
    );
    let open = w.img.admit(&w.store, &request(2, 2, Verb::Answer, 0, 0));
    assert_eq!(open, Err(Admission::Resolved), "no card is open");

    w.fs.crash(false);
    let plan = w.recover(SID, &history(&[]), None);
    assert!(plan.deliver.is_empty());
    check(image(&plan));
    let store = UiStore::open(w.fs.clone(), SID, cfg()).unwrap();
    let img = Images::new(image(&plan).clone());
    assert_eq!(
        img.admit(&store, &retry),
        Err(Admission::Duplicate { rev: 1 })
    );
}

/// H2: a session's deletion is sidecar first across an OS crash, not only
/// in syscall order. The order is: card files, a directory sync, history,
/// a closing sync; a failed barrier touches no history file; and at every
/// cut (the process dies after k operations) an OS crash that keeps any
/// subset of the unsynced unlinks, each independently, never leaves card
/// files without all of their history.
#[test]
fn session_deleted_sidecar_first_across_crashes() {
    let history: Vec<String> = ["json", "journal", "turn"]
        .iter()
        .map(|e| format!("{SID}.{e}"))
        .collect();
    let setup = || {
        let fs = ModelFs::new();
        for name in sidecar_files(SID).iter().chain(&history) {
            fs.put(name, b"{}");
        }
        fs
    };
    let delete = |fs: &ModelFs| {
        delete_sidecar_first(SID, &history, &mut |n| fs.remove(n), &mut || fs.sync_dir())
    };

    let fs = setup();
    delete(&fs).unwrap();
    let mut order: Vec<String> = sidecar_files(SID)
        .iter()
        .map(|n| format!("remove {n}"))
        .collect();
    order.push("sync".into());
    order.extend(history.iter().map(|n| format!("remove {n}")));
    order.push("sync".into());
    assert_eq!(fs.log(), order);
    assert!(fs.names().is_empty());

    let fs = setup();
    fs.fail_sync(1);
    assert!(delete(&fs).is_err());
    for name in &history {
        assert!(fs.get(name).is_some(), "{name} survives a failed barrier");
        assert!(!fs.log().contains(&format!("remove {name}")));
    }

    for cut in 0..=order.len() as u32 {
        let probe = setup();
        probe.die_after(cut);
        let _ = delete(&probe);
        for kept in 0..(1u32 << probe.unsynced()) {
            let fs = setup();
            fs.die_after(cut);
            let _ = delete(&fs);
            fs.crash_keeping(&|i| kept & (1 << i) != 0);
            let names = fs.names();
            let cards = sidecar_files(SID).iter().any(|c| names.contains(c));
            let all_history = history.iter().all(|h| names.contains(h));
            assert!(!cards || all_history, "cut {cut}, kept {kept:b}: {names:?}");
        }
    }
}
