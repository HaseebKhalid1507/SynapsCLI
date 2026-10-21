//! Process-wide table of Anthropic/OAuth usage reads, one entry per account
//! slot, shared by every [`LocalBroker`](super::LocalBroker) in the process
//! (a daemon can hold several brokers, one per runtime `TokenCache`).
//!
//! * **Single-flight.** One network read per slot at a time; a reader that
//!   waited for a read in flight takes that read's result (success or
//!   failure) and sends nothing. The per-slot gate is never removed, so
//!   invalidating a cached reading can never open a second flight.
//! * **Negative cache.** A transient failure (usage-endpoint 429, 5xx,
//!   timeout, transport) holds the slot: reads inside the hold return the
//!   same failure with no request. Holds back off 2, 4, 8, then 10 minutes;
//!   a longer `Retry-After` wins, capped at 10 minutes. A success clears the
//!   hold. Credential rejections are never held (their path is unchanged).
//! * **Last good.** The slot's last successful snapshot and the seat it was
//!   read from, which automatic selection may use in place of a read that
//!   failed transiently (see [`USAGE_READ_GRACE`],
//!   [`UsageReads::last_good_for`] and `quota_policy::SelectionRequest::grace`).
//!
//! Keys are the stable slot identity (credential file + storage key + usage
//! endpoint), never the seat digest, which changes on token rotation. Last
//! good is bound to the account it was read from: when the credential now in
//! the slot carries an account id, the reading counts only for that same id
//! (a refresh rotation keeps it; a re-login to another account drops it).
//! Credentials without an account id cannot be told apart across rotation;
//! for those a re-login of the slot to another subscription within the
//! grace window could be graced on the previous occupant's reading, bounded
//! by the 30-minute window and by the message path's own 429 cooldown.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use super::account::SeatIdentity;
use super::broker::BrokerError;
use super::quota_policy::ObservationFailure;
use super::usage::{UsageError, UsageSnapshot};

/// First hold after a transient usage-read failure.
pub const NEGATIVE_BACKOFF_START: Duration = Duration::from_secs(2 * 60);
/// Longest hold, including one asked for by `Retry-After`.
pub const NEGATIVE_BACKOFF_CAP: Duration = Duration::from_secs(10 * 60);
/// A last successful reading younger than this may stand in for a read that
/// failed transiently. Never for an exhausted window, an auth rejection or
/// an active cooldown.
pub const USAGE_READ_GRACE: Duration = Duration::from_secs(30 * 60);
/// Tracing target of the per-read INFO line (passes the daemon log filter).
pub const USAGE_READ_TARGET: &str = "agent_core::auth::usage_read";

/// A usage read that failed for a reason that says nothing about the
/// account: rate limit, server error, timeout or transport. Secret-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransientReadFailure {
    /// The usage error kind (`rate_limited`, `upstream_status`, `timeout`,
    /// `transport`).
    pub kind: &'static str,
    pub status: Option<u16>,
    pub retry_after_s: Option<u64>,
    /// The failed read itself, so a held read reports exactly what the
    /// network read did (broker error text and typed cause).
    error: UsageError,
}

impl TransientReadFailure {
    /// `Some` only for the failures the negative cache holds.
    pub fn from_usage_error(e: &UsageError) -> Option<Self> {
        let (status, retry_after_s) = match e {
            UsageError::RateLimited { retry_after_s } => (Some(429), *retry_after_s),
            UsageError::UpstreamStatus {
                status,
                retry_after_s,
            } if *status >= 500 => (Some(*status), *retry_after_s),
            UsageError::Timeout | UsageError::Transport { .. } => (None, None),
            _ => return None,
        };
        Some(Self {
            kind: e.kind(),
            status,
            retry_after_s,
            error: e.clone(),
        })
    }

    pub fn is_rate_limited(&self) -> bool {
        self.status == Some(429)
    }

    /// The broker error callers saw for this failure before the table.
    pub fn to_broker_error(&self) -> BrokerError {
        self.error.clone().into_broker_error()
    }

    /// The typed cause the selection report keeps for this failure.
    pub fn cause(&self) -> ObservationFailure {
        self.error.failure()
    }
}

/// What one network read produced, before the table classifies it.
// One value per network read, moved straight into the table: boxing the
// snapshot would buy nothing.
#[allow(clippy::large_enum_variant)]
pub enum Fetched {
    Ok(UsageSnapshot, SeatIdentity),
    /// The usage request itself failed.
    Usage(UsageError),
    /// The read failed before a usage request (slot check, token refresh),
    /// with its typed cause.
    Failed {
        error: BrokerError,
        cause: ObservationFailure,
    },
}

#[derive(Debug, Clone)]
pub enum ReadError {
    /// Held: no request for this slot before `retry_at_ms` (epoch ms).
    Transient {
        failure: TransientReadFailure,
        retry_at_ms: u64,
    },
    Other {
        error: BrokerError,
        cause: ObservationFailure,
    },
}

impl ReadError {
    pub fn to_broker_error(&self) -> BrokerError {
        match self {
            Self::Transient { failure, .. } => failure.to_broker_error(),
            Self::Other { error, .. } => error.clone(),
        }
    }

    /// The typed cause the selection report keeps for this failure.
    pub fn cause(&self) -> ObservationFailure {
        match self {
            Self::Transient { failure, .. } => failure.cause(),
            Self::Other { cause, .. } => cause.clone(),
        }
    }
}

pub type ReadResult = Result<(UsageSnapshot, SeatIdentity), ReadError>;

/// How a read was answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadPath {
    Network,
    Joined,
    NegativeCacheHit,
}

#[derive(Debug, Clone)]
struct Hold {
    failure: TransientReadFailure,
    until_ms: u64,
    backoff_ms: u64,
}

#[derive(Default)]
struct Entry {
    /// Bumped by every recorded network read.
    generation: u64,
    last: Option<ReadResult>,
    hold: Option<Hold>,
    /// Last successful snapshot and the seat whose token read it.
    last_good: Option<(UsageSnapshot, SeatIdentity)>,
}

/// Whether a reading taken from seat `read_from` may stand for the
/// credential now stored in the slot (`now`): the same account id when
/// either carries one; without ids on both sides, yes (see the module docs).
fn same_account(read_from: &SeatIdentity, now: &SeatIdentity) -> bool {
    match (read_from.seat_fingerprint(), now.seat_fingerprint()) {
        (Some(a), Some(b)) => a == b,
        (None, None) => true,
        _ => false,
    }
}

/// See the module docs. [`process_table`] is the one production instance.
#[derive(Default)]
pub struct UsageReads {
    entries: Mutex<HashMap<String, Entry>>,
    gates: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

/// The table shared by every broker in this process.
pub fn process_table() -> Arc<UsageReads> {
    static TABLE: OnceLock<Arc<UsageReads>> = OnceLock::new();
    TABLE.get_or_init(|| Arc::new(UsageReads::default())).clone()
}

fn ms(d: Duration) -> u64 {
    d.as_millis() as u64
}

impl UsageReads {
    fn entries(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The active hold for `key`: its failure and when it ends.
    pub fn held_at(&self, key: &str, now_ms: u64) -> Option<(TransientReadFailure, u64)> {
        self.entries()
            .get(key)
            .and_then(|e| e.hold.as_ref())
            .filter(|h| h.until_ms > now_ms)
            .map(|h| (h.failure.clone(), h.until_ms))
    }

    /// Record one network read for `key` at `now_ms` and classify it.
    // ReadError carries the broker's own error (as `usage()` returns it);
    // one per read, so its size is no cost worth a box.
    #[allow(clippy::result_large_err)]
    pub fn record_at(&self, key: &str, fetched: Fetched, now_ms: u64) -> ReadResult {
        let mut entries = self.entries();
        let entry = entries.entry(key.to_string()).or_default();
        let result = match fetched {
            Fetched::Ok(snapshot, seat) => {
                entry.hold = None;
                entry.last_good = Some((snapshot.clone(), seat.clone()));
                Ok((snapshot, seat))
            }
            Fetched::Usage(e) => match TransientReadFailure::from_usage_error(&e) {
                Some(failure) => {
                    let cap = ms(NEGATIVE_BACKOFF_CAP);
                    let backoff_ms = match &entry.hold {
                        Some(h) => h.backoff_ms.saturating_mul(2).min(cap),
                        None => ms(NEGATIVE_BACKOFF_START),
                    };
                    let asked = failure.retry_after_s.map_or(0, |s| s.saturating_mul(1000));
                    let until_ms = now_ms.saturating_add(backoff_ms.max(asked).min(cap));
                    entry.hold = Some(Hold {
                        failure: failure.clone(),
                        until_ms,
                        backoff_ms,
                    });
                    Err(ReadError::Transient {
                        failure,
                        retry_at_ms: until_ms,
                    })
                }
                None => {
                    if e.is_auth_failure() {
                        entry.last_good = None;
                    }
                    entry.hold = None;
                    Err(ReadError::Other {
                        cause: e.failure(),
                        error: e.into_broker_error(),
                    })
                }
            },
            Fetched::Failed { error, cause } => {
                // A refused or missing login ends any grace for the slot.
                if cause.kind.needs_login() {
                    entry.last_good = None;
                }
                entry.hold = None;
                Err(ReadError::Other { error, cause })
            }
        };
        entry.generation = entry.generation.wrapping_add(1);
        entry.last = Some(result.clone());
        result
    }

    /// The slot's last successful snapshot, if it was read from the account
    /// now stored in the slot (`current`): never another account's reading.
    pub fn last_good_for(&self, key: &str, current: &SeatIdentity) -> Option<UsageSnapshot> {
        self.entries()
            .get(key)
            .and_then(|e| e.last_good.as_ref())
            .filter(|(_, read_from)| same_account(read_from, current))
            .map(|(snapshot, _)| snapshot.clone())
    }

    /// Drop the slot's last good reading (e.g. after a reported limit).
    pub fn forget_last_good(&self, key: &str) {
        if let Some(e) = self.entries().get_mut(key) {
            e.last_good = None;
        }
    }

    /// Drop the slot's hold and last good reading (`synaps auth state
    /// --clear`): the next read goes to the network. The gate stays, so a
    /// read in flight keeps its single flight; if one is in flight, its
    /// result is still recorded when it lands.
    pub fn forget(&self, key: &str) {
        if let Some(e) = self.entries().get_mut(key) {
            e.hold = None;
            e.last_good = None;
        }
    }

    fn generation(&self, key: &str) -> u64 {
        self.entries().get(key).map_or(0, |e| e.generation)
    }

    fn gate(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.gates
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Read `key`: a held failure is returned without a request; otherwise
    /// one flight per key, and a reader that waited for a flight takes its
    /// result.
    pub async fn read<F, Fut>(&self, key: &str, fetch: F) -> (ReadResult, ReadPath)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Fetched>,
    {
        if let Some((failure, retry_at_ms)) = self.held_at(key, crate::epoch_millis()) {
            return (
                Err(ReadError::Transient {
                    failure,
                    retry_at_ms,
                }),
                ReadPath::NegativeCacheHit,
            );
        }
        let seen = self.generation(key);
        let gate = self.gate(key);
        let _flight = gate.lock().await;
        let joined = self
            .entries()
            .get(key)
            .filter(|e| e.generation != seen)
            .and_then(|e| e.last.clone());
        if let Some(result) = joined {
            return (result, ReadPath::Joined);
        }
        let fetched = fetch().await;
        (
            self.record_at(key, fetched, crate::epoch_millis()),
            ReadPath::Network,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60_000;

    fn limited(retry_after_s: Option<u64>) -> Fetched {
        Fetched::Usage(UsageError::RateLimited { retry_after_s })
    }

    fn held_for(t: &UsageReads, fetched: Fetched, now: u64) -> u64 {
        match t.record_at("k", fetched, now) {
            Err(ReadError::Transient { retry_at_ms, .. }) => {
                assert_eq!(
                    t.held_at("k", now + 1).map(|(_, until)| until),
                    Some(retry_at_ms),
                    "a read inside the hold is answered from the table"
                );
                assert!(t.held_at("k", retry_at_ms).is_none(), "the hold ends on time");
                retry_at_ms - now
            }
            other => panic!("expected a held failure, got {other:?}"),
        }
    }

    fn seat(refresh: &str, account_id: Option<&str>) -> SeatIdentity {
        let creds = super::super::OAuthCredentials {
            auth_type: "oauth".into(),
            refresh: refresh.into(),
            access: "a".into(),
            expires: 0,
            account_id: account_id.map(Into::into),
        };
        SeatIdentity::of(super::super::OAuthProviderId::Anthropic, &creds)
    }

    fn good_from(read_from: SeatIdentity) -> Fetched {
        let snapshot = super::super::usage::parse_anthropic_usage(
            r#"{"seven_day":{"utilization":10,"resets_at":"2099-01-01T00:00:00Z"}}"#,
            "a",
            1,
        )
        .unwrap();
        Fetched::Ok(snapshot, read_from)
    }

    fn good() -> Fetched {
        good_from(seat("r", None))
    }

    #[test]
    fn failed_reads_back_off_two_four_eight_ten_minutes_and_a_success_clears() {
        let t = UsageReads::default();
        let mut now = 1_000_000;
        let mut holds = Vec::new();
        for _ in 0..5 {
            let held = held_for(&t, limited(None), now);
            holds.push(held / MIN);
            now += held;
        }
        assert_eq!(holds, vec![2, 4, 8, 10, 10]);
        assert!(t.record_at("k", good(), now).is_ok());
        assert!(t.held_at("k", now).is_none(), "a success clears the hold");
        assert!(t.last_good_for("k", &seat("r", None)).is_some());
        assert_eq!(held_for(&t, limited(None), now), 2 * MIN, "and restarts the backoff");

        // A longer Retry-After wins, capped at 10 minutes.
        let t = UsageReads::default();
        assert_eq!(held_for(&t, limited(Some(300)), now), 5 * MIN);
        assert_eq!(held_for(&t, limited(Some(3600)), now + 5 * MIN), 10 * MIN);
        let t = UsageReads::default();
        assert_eq!(held_for(&t, limited(Some(30)), now), 2 * MIN, "a shorter one does not");

        // 5xx, timeouts and transport failures are held too; a 5xx keeps its
        // Retry-After like a 429.
        for e in [
            UsageError::UpstreamStatus {
                status: 503,
                retry_after_s: None,
            },
            UsageError::Timeout,
            UsageError::Transport { detail: "connect".into() },
        ] {
            let t = UsageReads::default();
            assert_eq!(held_for(&t, Fetched::Usage(e), now), 2 * MIN);
        }
        let t = UsageReads::default();
        let slow = UsageError::UpstreamStatus {
            status: 503,
            retry_after_s: Some(300),
        };
        assert_eq!(held_for(&t, Fetched::Usage(slow), now), 5 * MIN);
        // Credential rejections and other answers are not, and a refused
        // login drops the last good reading.
        let t = UsageReads::default();
        assert!(t.record_at("k", good(), now).is_ok());
        for e in [
            UsageError::Unauthorized { status: 401 },
            UsageError::UpstreamStatus {
                status: 404,
                retry_after_s: None,
            },
            UsageError::Malformed { detail: "x".into() },
        ] {
            assert!(matches!(
                t.record_at("k", Fetched::Usage(e), now),
                Err(ReadError::Other { .. })
            ));
            assert!(t.held_at("k", now).is_none());
        }
        assert!(
            t.last_good_for("k", &seat("r", None)).is_none(),
            "the 401 dropped the last good reading"
        );
    }

    /// Last good is bound to the account it was read from: the same account
    /// id after a token rotation keeps it, another account id (or a switch
    /// between id and no id) never sees it. Without ids the slot's reading
    /// stands (bounded by the grace window; see the module docs).
    #[test]
    fn last_good_counts_only_for_the_account_it_was_read_from() {
        let t = UsageReads::default();
        assert!(t.record_at("k", good_from(seat("r1", Some("acct-1"))), 1).is_ok());
        assert!(t.last_good_for("k", &seat("r1", Some("acct-1"))).is_some());
        assert!(
            t.last_good_for("k", &seat("r2-rotated", Some("acct-1"))).is_some(),
            "a refresh rotation keeps the account"
        );
        assert!(
            t.last_good_for("k", &seat("r1", Some("acct-2"))).is_none(),
            "another account in the slot is never graced on this reading"
        );
        assert!(t.last_good_for("k", &seat("r1", None)).is_none());
        assert!(t.last_good_for("other", &seat("r1", Some("acct-1"))).is_none());

        let t = UsageReads::default();
        assert!(t.record_at("k", good_from(seat("r1", None)), 1).is_ok());
        assert!(t.last_good_for("k", &seat("r2", None)).is_some());
        assert!(t.last_good_for("k", &seat("r1", Some("acct-1"))).is_none());
    }
}
