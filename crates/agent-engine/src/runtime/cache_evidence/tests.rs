use super::*;
use serde_json::json;

const FIVE: Duration = FIVE_MINUTES;
const HOUR: Duration = ONE_HOUR;
const NS: &str = "ns-a";

fn msg(role: &str, content: Value) -> SharedMessage {
    Arc::new(json!({ "role": role, "content": content }))
}

fn cc(ttl: Duration) -> Value {
    if ttl == HOUR {
        json!({"type": "ephemeral", "ttl": "1h"})
    } else {
        json!({"type": "ephemeral"})
    }
}

/// A request's wire messages: `turns` user/assistant texts, the last user
/// text marked with `ttl`.
fn request(turns: &[&str], ttl: Duration) -> Vec<SharedMessage> {
    let mut out: Vec<SharedMessage> = turns
        .iter()
        .enumerate()
        .map(|(i, t)| msg(if i % 2 == 0 { "user" } else { "assistant" }, json!(t)))
        .collect();
    let last = out.pop().expect("a turn");
    out.push(msg(last["role"].as_str().unwrap(), json!([{"type": "text", "text": last["content"], "cache_control": cc(ttl)}])));
    out
}

/// The probe for the history `turns` (ending with a reply): the
/// placeholder carries the tail marker.
fn probe(ns: &str, stable: u64, turns: &[&str]) -> ProbeEvidence {
    let mut turns = turns.to_vec();
    turns.push("…");
    let messages = request(&turns, FIVE);
    ProbeEvidence { ns: ns.into(), stable, markers: history_markers(&messages), messages, rollover: false }
}

fn send(store: &CacheEvidence, ns: &str, stable: u64, turns: &[&str], ttl: Duration) -> CacheTicket {
    store.ticket(ns, &WireEvidence::of(stable, &request(turns, ttl), []))
}

fn answer(store: &CacheEvidence, p: &ProbeEvidence) -> CacheExpiry {
    CacheEval::Matched { selection: store.select(p), next_ping_at: None }.at(Instant::now())
}

fn says(c: &CacheExpiry) -> (CacheState, Option<CacheReason>) {
    (c.state, c.reason)
}

const WARM: (CacheState, Option<CacheReason>) = (CacheState::Warm, None);
const BAND: (CacheState, Option<CacheReason>) = (CacheState::Unknown, Some(CacheReason::MayHaveExpired));
const EXPIRED: (CacheState, Option<CacheReason>) = (CacheState::Cold, Some(CacheReason::Expired));

async fn advance(d: Duration) {
    tokio::time::advance(d).await;
}

fn mins(m: u64) -> Duration {
    Duration::from_secs(m * 60)
}

#[test]
fn a_string_content_and_its_marked_block_hash_alike() {
    let plain = vec![msg("user", json!("hello")), msg("assistant", json!("hi"))];
    let marked = request(&["hello"], HOUR);
    let at = BlockCoord { msg: 0, block: 0, ord: 0 };
    assert_eq!(hash_at(7, &plain, &[at]), hash_at(7, &marked, &[at]));
    assert_ne!(hash_at(7, &plain, &[at]), hash_at(8, &plain, &[at]), "stable is in the hash");
    let past = BlockCoord { msg: 5, block: 0, ord: 9 };
    assert_eq!(hash_at(7, &plain, &[past]), vec![None]);
}

#[test]
fn markers_are_read_with_their_ttls_and_ordinals() {
    let mut m = request(&["a", "b", "c"], FIVE);
    let first = Arc::make_mut(&mut m[0]);
    first["content"] = json!([{"type": "text", "text": "a", "cache_control": cc(HOUR)}]);
    let ev = WireEvidence::of(1, &m, [HOUR]);
    assert_eq!(ev.markers.len(), 2);
    assert_eq!(ev.markers[0], Marker { at: BlockCoord { msg: 0, block: 0, ord: 0 }, ttl: HOUR });
    assert_eq!(ev.markers[1], Marker { at: BlockCoord { msg: 2, block: 0, ord: 2 }, ttl: FIVE });
    assert_eq!(ev.tail.unwrap().0, ev.markers[1]);
    assert_eq!(ev.max_ttl, HOUR);
}

#[tokio::test(start_paused = true)]
async fn a_measured_5m_request_is_warm_then_expired() {
    let s = CacheEvidence::default();
    let t = send(&s, NS, 1, &["hello"], FIVE);
    let p = probe(NS, 1, &["hello", "hi"]);
    assert_eq!(says(&answer(&s, &p)), (CacheState::Unknown, Some(CacheReason::Unmeasured)));
    s.observe(&t, 0, 2000, Some(2000), Some(0));
    let c = answer(&s, &p);
    assert_eq!(says(&c), WARM);
    assert_eq!((c.warm_for_secs, c.proof_ttl_secs, c.requested_ttl_secs, c.proof_cache_tokens), (Some(300), Some(300), Some(300), Some(2000)));
    advance(Duration::from_secs(299)).await;
    assert_eq!(says(&answer(&s, &p)), WARM);
    advance(Duration::from_secs(1)).await;
    assert_eq!(says(&answer(&s, &p)), EXPIRED);
}

#[tokio::test(start_paused = true)]
async fn the_proof_ttl_follows_the_write() {
    // 1h tail, 1h write: 1h. Then read-only: the previous proof (1h).
    let s = CacheEvidence::default();
    let p = probe(NS, 1, &["hello", "hi"]);
    let t = send(&s, NS, 1, &["hello"], HOUR);
    s.observe(&t, 0, 900, Some(0), Some(900));
    assert_eq!(answer(&s, &p).proof_ttl_secs, Some(3600));
    let t = send(&s, NS, 1, &["hello"], HOUR);
    s.observe(&t, 900, 0, Some(0), Some(0));
    assert_eq!(answer(&s, &p).proof_ttl_secs, Some(3600));
    // A 5m write on a 1h tail: downgraded, 5m.
    let t = send(&s, NS, 1, &["hello"], HOUR);
    s.observe(&t, 0, 900, Some(900), Some(0));
    assert_eq!(answer(&s, &p).proof_ttl_secs, Some(300));
    // A read-only first measurement on a 1h tail: 5m.
    let s = CacheEvidence::default();
    let t = send(&s, NS, 1, &["hello"], HOUR);
    s.observe(&t, 900, 0, None, None);
    assert_eq!(answer(&s, &p).proof_ttl_secs, Some(300));
}

#[tokio::test(start_paused = true)]
async fn the_latest_send_wins_and_a_late_older_usage_changes_nothing() {
    let s = CacheEvidence::default();
    let p = probe(NS, 1, &["hello", "hi"]);
    let old = send(&s, NS, 1, &["hello"], FIVE);
    let new = send(&s, NS, 1, &["hello"], FIVE);
    s.observe(&new, 0, 0, None, None);
    s.observe(&old, 0, 500, Some(500), None);
    assert_eq!(says(&answer(&s, &p)), (CacheState::Cold, Some(CacheReason::Uncached)));
}

#[tokio::test(start_paused = true)]
async fn another_namespace_or_prefix_never_matches() {
    let s = CacheEvidence::default();
    let t = send(&s, NS, 1, &["hello"], FIVE);
    s.observe(&t, 0, 500, Some(500), None);
    let no = (CacheState::Unknown, Some(CacheReason::NoMatchingRequest));
    assert_eq!(says(&answer(&s, &probe("ns-b", 1, &["hello", "hi"]))), no, "another token");
    assert_eq!(says(&answer(&s, &probe(NS, 2, &["hello", "hi"]))), no, "another stable");
    assert_eq!(says(&answer(&s, &probe(NS, 1, &["hullo", "hi"]))), no, "other bytes");
}

#[tokio::test(start_paused = true)]
async fn a_record_past_the_lookback_does_not_match() {
    let s = CacheEvidence::default();
    let t = send(&s, NS, 1, &["hello"], FIVE);
    s.observe(&t, 0, 500, Some(500), None);
    // 20 blocks after the record's tail: the probe's placeholder is at ord 21.
    let mut turns = vec!["hello"];
    turns.extend(std::iter::repeat_n("x", 20));
    assert_eq!(says(&answer(&s, &probe(NS, 1, &turns))).1, Some(CacheReason::NoMatchingRequest));
    turns.pop();
    turns.pop();
    assert_eq!(says(&answer(&s, &probe(NS, 1, &turns))), WARM, "19 after: inside");
}

#[tokio::test(start_paused = true)]
async fn warm_outranks_unknown_outranks_cold_then_the_longest_prefix() {
    let s = CacheEvidence::default();
    let p = probe(NS, 1, &["hello", "hi", "more", "ok"]);
    // The shorter prefix measured warm, the longer one inactive.
    let short = send(&s, NS, 1, &["hello"], FIVE);
    s.observe(&short, 0, 400, Some(400), None);
    let long = send(&s, NS, 1, &["hello", "hi", "more"], FIVE);
    s.observe(&long, 0, 0, None, None);
    assert_eq!(says(&answer(&s, &p)), WARM);
    // Both warm: the longer one.
    let long = send(&s, NS, 1, &["hello", "hi", "more"], FIVE);
    s.observe(&long, 300, 200, Some(200), None);
    assert_eq!(answer(&s, &p).proof_cache_tokens, Some(500));
}

// ── v3.1 §1 (R3-M1): the upper bound never shrinks ───────────────────────

/// History-only 1h→5m on one key: at the 5m deadline the band, not cold,
/// until the 1h send's horizon.
#[tokio::test(start_paused = true)]
async fn a_history_only_ttl_cut_keeps_the_1h_horizon() {
    let s = CacheEvidence::default();
    let p = probe(NS, 1, &["hello", "hi"]);
    let t = send(&s, NS, 1, &["hello"], HOUR);
    s.observe(&t, 0, 900, Some(0), Some(900));
    advance(mins(1)).await;
    let t = send(&s, NS, 1, &["hello"], FIVE);
    s.observe(&t, 900, 0, Some(0), Some(0));
    assert_eq!(answer(&s, &p).proof_ttl_secs, Some(300));
    advance(mins(6)).await;
    assert_eq!(says(&answer(&s, &p)), BAND);
    advance(mins(54)).await;
    assert_eq!(says(&answer(&s, &p)), EXPIRED, "past the 1h send's horizon");
}

/// The 1h record evicted while a 5m one of its group survives: the group
/// horizon remembers the hour.
#[tokio::test(start_paused = true)]
async fn an_evicted_1h_record_leaves_its_horizon() {
    let s = CacheEvidence::default();
    let t = send(&s, NS, 1, &["long ago"], HOUR);
    s.observe(&t, 0, 900, Some(0), Some(900));
    let texts: Vec<String> = (0..RECORDS).map(|i| format!("turn {i}")).collect();
    for text in &texts {
        let t = send(&s, NS, 1, &[text], FIVE);
        s.observe(&t, 0, 100, Some(100), None);
    }
    assert_eq!(says(&answer(&s, &probe(NS, 1, &["long ago", "hi"]))).1, Some(CacheReason::NoMatchingRequest), "evicted");
    let p = probe(NS, 1, &[&texts[RECORDS - 1], "hi"]);
    advance(mins(6)).await;
    assert_eq!(says(&answer(&s, &p)), BAND);
}

/// Sends rewriting one record's last send; then an unmeasured (in flight
/// or failed) send after the last measurement: each stretches the band.
#[tokio::test(start_paused = true)]
async fn repeated_and_unmeasured_sends_stretch_the_band() {
    let s = CacheEvidence::default();
    let p = probe(NS, 1, &["hello", "hi"]);
    let t = send(&s, NS, 1, &["hello"], FIVE);
    s.observe(&t, 0, 100, Some(100), None);
    for _ in 0..3 {
        advance(mins(2)).await;
        send(&s, NS, 1, &["hello"], FIVE);
    }
    // Proof at 0 (warm to 5m); the last send at 6m (horizon 11m).
    advance(mins(4)).await;
    assert_eq!(says(&answer(&s, &p)), BAND, "10m: inside the last send's horizon");
    advance(mins(1)).await;
    assert_eq!(says(&answer(&s, &p)), EXPIRED);
}

/// A group eviction holds every answer in the band until `floor`: here a
/// 5m group that predates the evicted 1h one (so its own cap is 5m).
#[tokio::test(start_paused = true)]
async fn an_evicted_group_holds_cold_back_until_its_floor() {
    let s = CacheEvidence::default();
    let t = send(&s, NS, 1, &["hello"], FIVE);
    s.observe(&t, 0, 100, Some(100), None);
    let t = send(&s, "ns-old", 9, &["old"], HOUR);
    s.observe(&t, 0, 900, Some(0), Some(900));
    let t = send(&s, NS, 1, &["hello"], FIVE);
    s.observe(&t, 0, 100, Some(100), None);
    // Groups without history markers: they evict groups, not records.
    for g in 0..GROUPS as u64 - 1 {
        s.ticket("ns-filler", &WireEvidence { stable: 100 + g, markers: Vec::new(), tail: None, max_ttl: FIVE });
    }
    let p = probe(NS, 1, &["hello", "hi"]);
    assert_eq!(says(&answer(&s, &probe("ns-old", 9, &["old", "hi"]))).1, Some(CacheReason::NoMatchingRequest), "evicted");
    advance(mins(6)).await;
    assert_eq!(says(&answer(&s, &p)), BAND, "the evicted 1h group's floor");
    advance(mins(55)).await;
    assert_eq!(says(&answer(&s, &p)), EXPIRED);
}

// ── R4-M1: a reintroduced group keeps the longest TTL ever ticketed ──────

/// Evict a 1h group, bring it back before the floor with a 5m matching
/// send, and evaluate past the old floor inside the refreshed hour: the
/// band, never cold. Twice, so a one-time patch cannot pass.
#[tokio::test(start_paused = true)]
async fn a_reintroduced_group_keeps_the_hour_twice() {
    let s = CacheEvidence::default();
    let p = probe(NS, 1, &["hello", "hi"]);
    let t = send(&s, NS, 1, &["hello"], HOUR);
    s.observe(&t, 0, 900, Some(0), Some(900));
    let mut filler = 1000;
    for round in 0..2 {
        // Every other group pushes G out.
        for _ in 0..GROUPS {
            filler += 1;
            send(&s, "ns-filler", filler, &["x"], FIVE);
        }
        assert_eq!(says(&answer(&s, &p)).1, Some(CacheReason::NoMatchingRequest), "round {round}: evicted");
        advance(mins(50)).await;
        // Back, history-only 5m, matching the old entry (and refreshing it).
        let t = send(&s, NS, 1, &["hello"], FIVE);
        s.observe(&t, 900, 0, Some(0), Some(0));
        advance(mins(11)).await;
        assert_eq!(says(&answer(&s, &p)), BAND, "round {round}: past the old floor, inside the refreshed hour");
    }
}

// ── R4-M2: the store's version ───────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn the_version_moves_on_ticket_observe_and_eviction() {
    let s = CacheEvidence::default();
    let v0 = s.version();
    let t = send(&s, NS, 1, &["hello"], FIVE);
    let v1 = s.version();
    assert!(v1 > v0);
    s.observe(&t, 0, 100, Some(100), None);
    let v2 = s.version();
    assert!(v2 > v1);
    let sel = s.select(&probe(NS, 1, &["hello", "hi"]));
    assert_eq!(sel.version, v2, "captured with the candidates");
}

/// R4-M2's regression: a job read a 5m proof (its group's horizon five
/// minutes on), its answer waits past that horizon while a producer
/// tickets, then separately observes, with nothing else changed. `finish`
/// never ages the old reading into `cold`: the store moved, so `stale`; a
/// fresh read is warm again. With no change it ages as before.
#[tokio::test(start_paused = true)]
async fn an_answer_read_before_a_newer_send_is_stale_never_cold() {
    let s = CacheEvidence::default();
    let t = send(&s, NS, 1, &["hello"], FIVE);
    s.observe(&t, 0, 900, Some(900), None);
    let p = probe(NS, 1, &["hello", "hi"]);
    for observe_too in [false, true] {
        let held = CacheEval::Matched { selection: s.select(&p), next_ping_at: None };
        advance(mins(4)).await;
        // An idle ping of the same request is dispatched meanwhile...
        let ping = send(&s, NS, 1, &["hello"], FIVE);
        let after_ticket = CacheEval::Matched { selection: s.select(&p), next_ping_at: None };
        if observe_too {
            // ... and, separately, its usage arrives.
            s.observe(&ping, 900, 0, None, None);
        }
        advance(mins(2)).await;
        let now = Instant::now();
        assert_eq!(says(&held.at(now)), EXPIRED, "the bug: the old reading aged alone");
        assert_eq!(held.finalize(&s, now), None, "observe_too={observe_too}: stale");
        let fresh = CacheEval::Matched { selection: s.select(&p), next_ping_at: None };
        let fresh = fresh.finalize(&s, now).expect("current");
        let want = if observe_too { WARM } else { BAND };
        assert_eq!(says(&fresh), want, "observe_too={observe_too}: {fresh:?}");
        if observe_too {
            assert_eq!(after_ticket.finalize(&s, now), None, "an observe alone moves it too");
        }
    }
    // No store change: the reading ages, and `finalize` is `at`.
    let quiet = CacheEval::Matched { selection: s.select(&p), next_ping_at: None };
    advance(mins(6)).await;
    let now = Instant::now();
    assert_eq!(quiet.finalize(&s, now), Some(quiet.at(now)));
    assert_eq!(says(&quiet.at(now)), EXPIRED);
}

// ── v3.1 §2: aged and classified at emit ─────────────────────────────────

#[tokio::test(start_paused = true)]
async fn an_answer_held_past_its_proof_goes_out_aged() {
    let s = CacheEvidence::default();
    let t = send(&s, "ns", 1, &["hello"], HOUR);
    s.observe(&t, 0, 900, Some(0), Some(900));
    // Downgrade the proof (5m) with the horizon at an hour.
    let t = send(&s, "ns", 1, &["hello"], HOUR);
    s.observe(&t, 0, 900, Some(900), Some(0));
    let eval = CacheEval::Matched { selection: s.select(&probe("ns", 1, &["hello", "hi"])), next_ping_at: Some(Instant::now() + mins(3)) };
    let now = Instant::now();
    let fresh = eval.at(now);
    assert_eq!((says(&fresh), fresh.warm_for_secs, fresh.next_ping_in_secs), (WARM, Some(300), Some(180)));
    let late = eval.at(now + mins(6));
    assert_eq!((says(&late), late.warm_for_secs, late.next_ping_in_secs), (BAND, None, Some(0)));
    assert_eq!(late.lapse_by_secs, Some(54 * 60));
    assert_eq!(says(&eval.at(now + mins(61))), EXPIRED);
}

#[test]
fn the_wording_is_evidence_not_a_countdown() {
    let warm = CacheExpiry {
        state: CacheState::Warm,
        reason: None,
        warm_for_secs: Some(299),
        lapse_by_secs: Some(3600),
        proof_ttl_secs: Some(300),
        requested_ttl_secs: Some(300),
        proof_cache_tokens: Some(1200),
        next_ping_in_secs: Some(200),
    };
    assert_eq!(
        render_cache_expiry(&warm),
        "Recent cache activity · about 4m left · 1200 tokens read or written by the last measured request · next keep-warm attempt in about 3m"
    );
    let band = CacheExpiry { state: CacheState::Unknown, reason: Some(CacheReason::MayHaveExpired), lapse_by_secs: Some(2341), ..CacheExpiry::unknown(CacheReason::MayHaveExpired) };
    assert_eq!(render_cache_expiry(&band), "May have expired · this session's last possible refresh ends in about 40m");
    let cold = CacheExpiry { state: CacheState::Cold, ..CacheExpiry::unknown(CacheReason::Expired) };
    assert_eq!(render_cache_expiry(&cold), "No unexpired evidence from this session");
    let uncached = CacheExpiry { state: CacheState::Cold, ..CacheExpiry::unknown(CacheReason::Uncached) };
    assert_eq!(render_cache_expiry(&uncached), "Last measured request reported no cache activity");
    for (reason, say) in [
        (CacheReason::Parked, "parked"),
        (CacheReason::Busy, "a turn is running"),
        (CacheReason::HistoryUnsettled, "last turn didn't finish"),
        (CacheReason::NoMatchingRequest, "no matching request yet"),
        (CacheReason::Unmeasured, "last request not measured yet"),
        (CacheReason::RolloverPending, "next turn starts a new context"),
        (CacheReason::ProbeFailed, "couldn't check"),
        (CacheReason::NotAnthropic, "not tracked for this provider"),
    ] {
        assert_eq!(render_cache_expiry(&CacheExpiry::unknown(reason)), say);
    }
}

#[test]
fn an_unknown_state_or_reason_from_a_newer_daemon_reads_as_unknown() {
    let c: CacheExpiry = serde_json::from_value(json!({"state": "lukewarm", "reason": "solar_flare", "extra": 1})).unwrap();
    assert_eq!(says(&c), (CacheState::Unknown, Some(CacheReason::Unknown)));
    let c: CacheExpiry = serde_json::from_value(json!({"state": "warm"})).unwrap();
    assert_eq!(c, CacheExpiry { state: CacheState::Warm, reason: None, ..CacheExpiry::unknown(CacheReason::Unknown) });
    assert_eq!(serde_json::to_value(CacheExpiry::unknown(CacheReason::MayHaveExpired)).unwrap(), json!({"state": "unknown", "reason": "may_have_expired"}));
}
