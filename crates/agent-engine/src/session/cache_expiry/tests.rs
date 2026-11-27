use super::*;
use crate::runtime::keepwarm::{CacheUsage, CachedPrefix};

const FIVE_MINUTES: Duration = Duration::from_secs(300);
const ONE_HOUR: Duration = Duration::from_secs(3600);

/// A moment `secs` after `t0` on both clocks.
fn at(t0: CacheAnchor, secs: u64) -> CacheAnchor {
    CacheAnchor {
        mono: t0.mono + Duration::from_secs(secs),
        wall: t0.wall + Duration::from_secs(secs),
    }
}

/// The cell after one request accepted at `t0` that read `tokens`.
fn cell(t0: CacheAnchor, tokens: Option<u64>) -> CacheAnchorState {
    CacheAnchorState {
        anchor: Some(t0),
        prefix: Some(CachedPrefix::of(1, &[])),
        usage: tokens.map(|read| CacheUsage {
            read,
            ..CacheUsage::default()
        }),
        dispatched: Some(t0),
    }
}

fn read(cell: CacheAnchorState, honoured: Duration, requested: Duration) -> CacheEval {
    CacheEval::Read {
        cell,
        reads_cache: true,
        honoured,
        requested,
        next_ping_at: None,
    }
}

fn says(c: &CacheExpiry) -> (CacheState, Option<CacheReason>) {
    (c.state, c.reason)
}

#[test]
fn warm_then_the_band_then_cold() {
    let t0 = CacheAnchor::now();
    let e = read(cell(t0, Some(20_000)), ONE_HOUR, ONE_HOUR);
    let c = e.at(at(t0, 600));
    assert_eq!(says(&c), (CacheState::Warm, None));
    assert_eq!((c.warm_for_secs, c.lapse_by_secs), (Some(3000), Some(3000)));
    assert_eq!(
        (c.proof_ttl_secs, c.requested_ttl_secs, c.proof_cache_tokens),
        (Some(3600), Some(3600), Some(20_000))
    );
    let c = e.at(at(t0, 3600));
    assert_eq!(says(&c), (CacheState::Cold, Some(CacheReason::Expired)));
    assert_eq!((c.warm_for_secs, c.lapse_by_secs), (None, None));
}

/// 1h asked, 5m known to be honoured (no 1h write seen, or a downgrade):
/// warm for the 5m, then the band until the hour asked for.
#[test]
fn an_unproven_hour_is_the_band() {
    let t0 = CacheAnchor::now();
    let e = read(cell(t0, Some(5)), FIVE_MINUTES, ONE_HOUR);
    assert_eq!(e.at(at(t0, 299)).warm_for_secs, Some(1));
    let c = e.at(at(t0, 300));
    assert_eq!(
        says(&c),
        (CacheState::Unknown, Some(CacheReason::MayHaveExpired))
    );
    assert_eq!((c.warm_for_secs, c.lapse_by_secs), (None, Some(3300)));
    assert_eq!(
        says(&e.at(at(t0, 3600))),
        (CacheState::Cold, Some(CacheReason::Expired))
    );
}

/// A request dispatched after the anchor, never answered (a ping in
/// flight, a dropped connection): it may have refreshed the entry, so its
/// dispatch bounds the band, never the warm time.
#[test]
fn a_later_dispatch_is_the_band_not_cold() {
    let t0 = CacheAnchor::now();
    let mut c = cell(t0, Some(5));
    c.dispatched = Some(at(t0, 290));
    let e = read(c, FIVE_MINUTES, FIVE_MINUTES);
    let c = e.at(at(t0, 301));
    assert_eq!(
        says(&c),
        (CacheState::Unknown, Some(CacheReason::MayHaveExpired))
    );
    assert_eq!(c.lapse_by_secs, Some(289));
    assert_eq!(
        says(&e.at(at(t0, 590))),
        (CacheState::Cold, Some(CacheReason::Expired))
    );
}

#[test]
fn unmeasured_uncached_and_unmatched() {
    let t0 = CacheAnchor::now();
    let ask = |tokens, reads_cache| {
        CacheEval::Read {
            cell: cell(t0, tokens),
            reads_cache,
            honoured: ONE_HOUR,
            requested: ONE_HOUR,
            next_ping_at: None,
        }
        .at(at(t0, 1))
    };
    assert_eq!(
        says(&ask(None, true)),
        (CacheState::Unknown, Some(CacheReason::Unmeasured))
    );
    let c = ask(Some(0), true);
    assert_eq!(says(&c), (CacheState::Cold, Some(CacheReason::Uncached)));
    assert_eq!((c.proof_cache_tokens, c.lapse_by_secs), (None, None));
    assert_eq!(
        says(&ask(Some(9), false)),
        (CacheState::Unknown, Some(CacheReason::NoMatchingRequest))
    );
}

/// The larger of the two clocks: a machine that slept wakes to the time
/// the provider counted.
#[test]
fn a_sleep_ages_the_answer() {
    let t0 = CacheAnchor::now();
    let e = read(cell(t0, Some(5)), FIVE_MINUTES, FIVE_MINUTES);
    let slept = CacheAnchor {
        mono: t0.mono + Duration::from_secs(10),
        wall: t0.wall + Duration::from_secs(400),
    };
    assert_eq!(
        says(&e.at(slept)),
        (CacheState::Cold, Some(CacheReason::Expired))
    );
}

#[test]
fn the_next_ping_is_named_and_a_fixed_reason_is_bare() {
    let t0 = CacheAnchor::now();
    let e = CacheEval::Read {
        cell: cell(t0, Some(5)),
        reads_cache: true,
        honoured: FIVE_MINUTES,
        requested: FIVE_MINUTES,
        next_ping_at: Some(t0.mono + Duration::from_secs(240)),
    };
    assert_eq!(e.at(at(t0, 40)).next_ping_in_secs, Some(200));
    assert_eq!(
        CacheEval::Fixed(CacheReason::RolloverPending).at(t0),
        CacheExpiry::unknown(CacheReason::RolloverPending)
    );
}

#[test]
fn the_wire_is_append_only_and_renders_as_documented() {
    let v: CacheExpiry =
        serde_json::from_value(serde_json::json!({"state": "lukewarm", "reason": "meteor"}))
            .unwrap();
    assert_eq!(says(&v), (CacheState::Unknown, Some(CacheReason::Unknown)));
    let t0 = CacheAnchor::now();
    let warm = read(cell(t0, Some(20_000)), ONE_HOUR, ONE_HOUR).at(at(t0, 600));
    assert_eq!(
        serde_json::to_value(&warm).unwrap(),
        serde_json::json!({"state": "warm", "warm_for_secs": 3000, "lapse_by_secs": 3000,
            "proof_ttl_secs": 3600, "requested_ttl_secs": 3600, "proof_cache_tokens": 20000})
    );
    assert_eq!(
        render_cache_expiry(&warm),
        "Recent cache activity · about 50m left · 20000 tokens read or written by the last measured request"
    );
    let band = read(cell(t0, Some(5)), FIVE_MINUTES, ONE_HOUR).at(at(t0, 301));
    assert_eq!(
        render_cache_expiry(&band),
        "May have expired · this session's last possible refresh ends in about 55m"
    );
    assert_eq!(
        render_cache_expiry(&CacheExpiry::unknown(CacheReason::Parked)),
        "parked"
    );
}
