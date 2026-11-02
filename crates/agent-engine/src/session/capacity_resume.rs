//! The capacity resume (A3 phase 2): a turn that ended on a reset-based
//! capacity stop (no Anthropic account had capacity, and a limited one comes
//! back at a known time) is owed ONE automatic resume at that time.
//!
//! The engine notes the stop on the turn's completion
//! (`runtime::CapacityStopAt`); the actor arms a [`CapacityStop`] from it,
//! publishes it in its crash-record entry (so a reload, a stop and start or
//! a crash carries it), holds a parked session from eviction while it is
//! armed, and at `resume_at` plus 0-60 s of jitter starts one turn with
//! [`resume_text`]. The resume is AT MOST ONCE: it is consumed by a durable,
//! acknowledged write of the record without it before it is dispatched, so
//! a crash after the consumption and before the dispatch loses that resume
//! rather than giving it twice.
//!
//! This module holds the pure rules: the attempt chain and its loop cap,
//! validating a restored stop, the history digest and the texts.

use std::time::Duration;

use chrono::{DateTime, Utc};

use super::types::CapacityStop;

/// The capacity resume's log target.
pub(crate) const LOG_TARGET: &str = "agent_engine::session::capacity_resume";

/// Capacity resumes one stop chain may dispatch: a resumed turn that stops
/// again arms again only while fewer than this many were dispatched.
pub(crate) const MAX_DISPATCHES: u8 = 3;

/// How far past its stop a resume may lie (a weekly limit resets within 7
/// days; a later reset is bad data).
pub(crate) fn horizon() -> chrono::Duration {
    chrono::Duration::days(8)
}

/// A restored stop's clock may run ahead of this daemon's by this much.
fn skew() -> chrono::Duration {
    chrono::Duration::minutes(2)
}

/// The most a resume waits past its reset (spreads sessions stopped on the
/// same reset).
pub(crate) const MAX_JITTER: Duration = Duration::from_secs(60);

/// How long a consumption or an owner clear waits for the crash record's
/// writer to acknowledge the write.
pub(crate) const ACK_WAIT: Duration = Duration::from_secs(5);

/// The longest the actor sleeps toward a resume before it reads the wall
/// clock again (a host suspend or a wall step can't make it fire early or
/// hours late). A consumption the writer did not acknowledge is retried
/// after this long (the next 10-minute look).
pub(crate) const TIMER_CAP: Duration = Duration::from_secs(10 * 60);

/// How soon a due resume looks again when the session was busy (a turn,
/// a compaction, a driver, queued commands or events). A pending
/// `on_session_start` is not busy: the resumed turn's own wait holds its
/// request until the hook ends, as for a queue wake.
pub(crate) const DEFER: Duration = Duration::from_secs(30);

/// The jitter's ceiling ([`MAX_JITTER`], or a test's).
pub(crate) fn max_jitter() -> Duration {
    #[cfg(any(test, feature = "testing"))]
    if let Some(d) = testing::get(&testing::MAX_JITTER_MS) {
        return d;
    }
    MAX_JITTER
}

/// How long a consumption or an owner clear waits for its acknowledgement
/// ([`ACK_WAIT`], or a test's).
pub(crate) fn ack_wait() -> Duration {
    #[cfg(any(test, feature = "testing"))]
    if let Some(d) = testing::get(&testing::ACK_WAIT_MS) {
        return d;
    }
    ACK_WAIT
}

/// When an unacknowledged consumption is tried again ([`TIMER_CAP`], or a
/// test's).
pub(crate) fn retry() -> Duration {
    #[cfg(any(test, feature = "testing"))]
    if let Some(d) = testing::get(&testing::RETRY_MS) {
        return d;
    }
    TIMER_CAP
}

/// When a busy session's due resume looks again ([`DEFER`], or a test's).
pub(crate) fn defer() -> Duration {
    #[cfg(any(test, feature = "testing"))]
    if let Some(d) = testing::get(&testing::RETRY_MS) {
        return d;
    }
    DEFER
}

/// Test timings for this process's capacity resumes (`cfg(test)` / the
/// `testing` feature): never in a release build.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    const UNSET: u64 = u64::MAX;
    pub(super) static MAX_JITTER_MS: AtomicU64 = AtomicU64::new(UNSET);
    pub(super) static ACK_WAIT_MS: AtomicU64 = AtomicU64::new(UNSET);
    pub(super) static RETRY_MS: AtomicU64 = AtomicU64::new(UNSET);

    pub(super) fn get(knob: &AtomicU64) -> Option<Duration> {
        match knob.load(Ordering::Relaxed) {
            UNSET => None,
            ms => Some(Duration::from_millis(ms)),
        }
    }

    /// The jitter's ceiling, the acknowledgement wait, and the look-again
    /// delay (both the retry after an unacknowledged consumption and the
    /// deferral of a busy session) for this process; dropping it restores
    /// the defaults.
    pub struct Timings(());

    impl Timings {
        pub fn set(max_jitter: Duration, ack_wait: Duration, look_again: Duration) -> Self {
            let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(UNSET - 1);
            MAX_JITTER_MS.store(ms(max_jitter), Ordering::Relaxed);
            ACK_WAIT_MS.store(ms(ack_wait), Ordering::Relaxed);
            RETRY_MS.store(ms(look_again), Ordering::Relaxed);
            Self(())
        }
    }

    impl Drop for Timings {
        fn drop(&mut self) {
            for knob in [&MAX_JITTER_MS, &ACK_WAIT_MS, &RETRY_MS] {
                knob.store(UNSET, Ordering::Relaxed);
            }
        }
    }

    static PAUSE: std::sync::Mutex<Option<std::sync::Arc<tokio::sync::Notify>>> =
        std::sync::Mutex::new(None);

    /// A crash-window seam: while alive, every capacity resume of this
    /// process stops for good right after its consumption is acknowledged,
    /// before anything is dispatched (the test then drops the actor, as a
    /// crash in that window would). Dropping it lets later resumes through.
    pub struct PauseAfterAck(std::sync::Arc<tokio::sync::Notify>);

    impl PauseAfterAck {
        pub fn set() -> Self {
            let reached = std::sync::Arc::new(tokio::sync::Notify::new());
            *PAUSE.lock().unwrap_or_else(|p| p.into_inner()) = Some(std::sync::Arc::clone(&reached));
            Self(reached)
        }

        /// Wait until a resume reached the pause.
        pub async fn reached(&self) {
            self.0.notified().await;
        }
    }

    impl Drop for PauseAfterAck {
        fn drop(&mut self) {
            *PAUSE.lock().unwrap_or_else(|p| p.into_inner()) = None;
        }
    }

    /// The seam's point (the actor, after the acknowledgement).
    pub(crate) async fn pause_after_ack() {
        let reached = PAUSE.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if let Some(reached) = reached {
            reached.notify_one();
            std::future::pending::<()>().await;
        }
    }
}

/// The failure kinds a stop can carry (`SelectionFailureKind::as_str`, or a
/// window limit with no broker reason). Anything else on disk is invalid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopKind {
    AllCoolingDown,
    CredentialRejected,
    ActionRequired,
    UsageReadFailed,
    QuotaUnknown,
    WindowLimit,
}

impl StopKind {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "all_cooling_down" => Self::AllCoolingDown,
            "credential_rejected" => Self::CredentialRejected,
            "action_required" => Self::ActionRequired,
            "usage_read_failed" => Self::UsageReadFailed,
            "quota_unknown" => Self::QuotaUnknown,
            "window_limit" => Self::WindowLimit,
            _ => return None,
        })
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::AllCoolingDown => "all_cooling_down",
            Self::CredentialRejected => "credential_rejected",
            Self::ActionRequired => "action_required",
            Self::UsageReadFailed => "usage_read_failed",
            Self::QuotaUnknown => "quota_unknown",
            Self::WindowLimit => "window_limit",
        }
    }
}

/// The capacity resume that started the running (or last) turn: kept from
/// its dispatch until the next turn starts, so a stop of that turn arms with
/// the chain's attempt and only for a strictly later reset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Provenance {
    /// Capacity resumes dispatched in this chain, this one included (1..=3).
    pub(crate) dispatched: u8,
    /// The reset the dispatched resume was armed for.
    pub(crate) prev_resume_at: DateTime<Utc>,
}

/// Why a stop is not armed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NoArm {
    /// The chain already dispatched [`MAX_DISPATCHES`] resumes.
    LoopCap,
    /// The reset is not later than the one the last resume was armed for.
    NotLater,
}

impl NoArm {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::LoopCap => "loop_cap",
            Self::NotLater => "not_later",
        }
    }
}

/// The attempt a stop resetting at `resume_at` arms with: 0 for a turn no
/// capacity resume started; for a resumed turn the resumes its chain
/// already dispatched, while that is below [`MAX_DISPATCHES`] and the reset
/// is strictly later than the previous one.
pub(crate) fn next_attempt(
    resume_at: DateTime<Utc>,
    chain: Option<Provenance>,
) -> Result<u8, NoArm> {
    match chain {
        None => Ok(0),
        Some(p) if p.dispatched >= MAX_DISPATCHES => Err(NoArm::LoopCap),
        Some(p) if resume_at <= p.prev_resume_at => Err(NoArm::NotLater),
        Some(p) => Ok(p.dispatched),
    }
}

/// Why a restored stop is dropped (a fixed class: the loaded text never
/// reaches a log).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Invalid {
    Kind,
    Digest,
    Attempt,
    Time,
    BeyondHorizon,
    Stale,
}

impl Invalid {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Kind => "invalid_kind",
            Self::Digest => "bad_digest",
            Self::Attempt => "bad_attempt",
            Self::Time => "bad_time",
            Self::BeyondHorizon => "beyond_horizon",
            Self::Stale => "stale",
        }
    }
}

/// A stop that passed [`validate`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Valid {
    pub(crate) kind: StopKind,
    pub(crate) stop: CapacityStop,
}

/// Check a stop loaded from a record before it earns anything (an arm, an
/// eviction hold): a known kind, a 64-hex digest, an attempt in 0..=2, a
/// stop not in the future (beyond a small skew) with its reset after it,
/// the reset within the horizon of the stop, and the stop itself not older
/// than the horizon. An overdue reset (`resume_at <= now`) is valid: it
/// fires right away, behind the same guards.
pub(crate) fn validate(stop: &CapacityStop, now: DateTime<Utc>) -> Result<Valid, Invalid> {
    let kind = StopKind::parse(&stop.kind).ok_or(Invalid::Kind)?;
    let hex = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !hex(&stop.last_digest) {
        return Err(Invalid::Digest);
    }
    if stop.attempt >= MAX_DISPATCHES {
        return Err(Invalid::Attempt);
    }
    if stop.stopped_at > now + skew() || stop.resume_at <= stop.stopped_at {
        return Err(Invalid::Time);
    }
    if stop.resume_at > stop.stopped_at + horizon() {
        return Err(Invalid::BeyondHorizon);
    }
    if now > stop.stopped_at + horizon() {
        return Err(Invalid::Stale);
    }
    Ok(Valid { kind, stop: stop.clone() })
}

/// Reads a record's `capacity_stop` leniently (its `deserialize_with`, in
/// the crash record, the reload state and a checkpoint's record): a recorded
/// resume that does not parse as a [`CapacityStop`] (a wrong type, a missing
/// field, an attempt out of range, a kind that is no string, a number past
/// what a typed read takes, nesting past the reader's depth limit) is
/// dropped with one WARN of a fixed class, and the rest of the record reads
/// as usual. Neither the stored value nor serde's message (it quotes the
/// value) reaches the log. A resume that parses is still validated before it
/// earns anything ([`validate`]).
///
/// Only the resume's raw text is taken from the record (a syntax-only skip:
/// no number range, no depth limit) and decoded apart, so a resume that is
/// valid JSON is isolated from typed errors (a number out of range, nesting
/// past the depth limit, a wrong type or shape): it drops only itself.
/// Invalid JSON syntax inside it still fails the whole record, as anywhere
/// else in the record, and running out of memory while reading it is not
/// isolated either (an allocation failure aborts the process). Taking the raw
/// text needs serde_json's own reader (or a `Value`):
/// serde's buffered content (a `flatten`, an untagged enum) cannot hand out
/// raw text, and would decode the resume first anyway. So a record that
/// flattens a [`ReloadSession`](crate::daemon::reload::ReloadSession) takes
/// `capacity_stop` as its own field (the crash record's entry does).
pub(crate) fn lenient_stop<'de, D>(d: D) -> Result<Option<CapacityStop>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let Some(raw) = Option::<Box<serde_json::value::RawValue>>::deserialize(d)? else {
        return Ok(None);
    };
    match serde_json::from_str::<CapacityStop>(raw.get()) {
        Ok(stop) => Ok(Some(stop)),
        Err(_) => {
            tracing::warn!(
                target: LOG_TARGET,
                event = "capacity_resume",
                phase = "dropped",
                class = "malformed",
                "capacity resume: a recorded resume does not parse; dropped"
            );
            Ok(None)
        }
    }
}

/// Whether a crash-record entry recorded Parked past its eviction age is
/// restored anyway: it carries a capacity resume that validates while the
/// switch is on (the resume unparks it at its time). Anything else is left
/// on disk, as before.
pub(crate) fn spares_parked(
    stop: Option<&CapacityStop>,
    switch_on: bool,
    now: DateTime<Utc>,
) -> bool {
    switch_on && stop.is_some_and(|s| validate(s, now).is_ok())
}

/// A restore gave `session` up (no live actor carries it): its capacity
/// resume goes with it, and a later load never arms it. One INFO line (the
/// kind as validated: loaded text never reaches the log).
pub(crate) fn note_given_up(session: &str, stop: &CapacityStop) {
    let kind = StopKind::parse(&stop.kind).map_or("invalid", StopKind::as_str);
    tracing::info!(
        target: LOG_TARGET,
        session = %session,
        event = "capacity_resume",
        phase = "dropped",
        why = "given_up",
        kind,
        "capacity resume: dropped; the restore gave the session up"
    );
}

/// The digest a stop pins the history with: sha256 of the last message's
/// JSON, 64 lowercase hex (of nothing for an empty history).
pub(crate) fn last_digest(messages: &[crate::SharedMessage]) -> String {
    use sha2::{Digest, Sha256};
    let body = messages
        .last()
        .map(|m| serde_json::to_vec(m.as_ref()).unwrap_or_default())
        .unwrap_or_default();
    Sha256::digest(&body)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A random jitter in `0..=max_jitter()`.
pub(crate) fn random_jitter() -> Duration {
    let max = max_jitter().as_millis() as u64;
    Duration::from_millis(rand::random::<u64>() % (max + 1))
}

/// `HH:MM UTC`, the notices' time format.
fn hhmm(t: DateTime<Utc>) -> String {
    t.format("%H:%M UTC").to_string()
}

/// The notice when a stop is armed: the kind's words and UTC times only
/// (no account, email, id or seat hash).
pub fn armed_notice(resume_at: DateTime<Utc>) -> String {
    format!(
        "Stopped: no Anthropic account has capacity until {}. This session continues by itself \
         then; send a message to continue sooner, or Esc to cancel the resume.",
        hhmm(resume_at)
    )
}

/// The notice when the owner's Esc cancels an armed resume.
pub fn canceled_notice() -> String {
    "The automatic resume is canceled; send a message to continue.".to_string()
}

/// The resume turn's text, in the existing `resume:` form.
pub fn resume_text(stopped_at: DateTime<Utc>, resume_at: DateTime<Utc>) -> String {
    format!(
        "resume: your last turn stopped at {} because no Anthropic account had capacity; one \
         was due back at {}, so continue where you stopped.",
        hhmm(stopped_at),
        hhmm(resume_at)
    )
}

/// Fixtures for the records that carry a capacity resume (the crash record,
/// the reload state, a checkpoint's record).
#[cfg(test)]
pub(crate) mod fixtures {
    /// Placed in each malformed resume wherever a stored value could be
    /// quoted: it must never reach a log line or an error text.
    pub(crate) const SENTINEL: &str = "SENTINEL-a1-5d29";

    /// Recorded resumes that do not parse as a `CapacityStop`, by the
    /// session id that carries each: a `resume_at` of the wrong type (a
    /// string, an object), a missing field, an attempt of 256, -1 or a
    /// string, a kind that is no string, and a resume that is no object.
    /// Each holds [`SENTINEL`].
    pub(crate) fn malformed_stops() -> Vec<(&'static str, serde_json::Value)> {
        let now = chrono::Utc::now();
        // Well typed (it would parse) but carrying the sentinel; each case
        // below breaks one thing.
        let base = serde_json::json!({
            "resume_at": (now + chrono::Duration::hours(1)).to_rfc3339(),
            "stopped_at": now.to_rfc3339(),
            "kind": SENTINEL,
            "history_len": 3,
            "last_digest": SENTINEL,
            "attempt": 0
        });
        let with = |field: &str, v: serde_json::Value| {
            let mut m = base.clone();
            m[field] = v;
            m
        };
        let mut missing = base.clone();
        missing.as_object_mut().expect("an object").remove("history_len");
        vec![
            ("s-time-string", with("resume_at", serde_json::json!(SENTINEL))),
            ("s-time-object", with("resume_at", serde_json::json!({ "at": SENTINEL }))),
            ("s-missing-field", missing),
            ("s-attempt-256", with("attempt", serde_json::json!(256))),
            ("s-attempt-negative", with("attempt", serde_json::json!(-1))),
            ("s-attempt-string", with("attempt", serde_json::json!(SENTINEL))),
            ("s-kind-number", with("kind", serde_json::json!(5))),
            ("s-kind-list", with("kind", serde_json::json!([SENTINEL]))),
            ("s-not-an-object", serde_json::json!(SENTINEL)),
        ]
    }

    /// The fixtures are what they claim: the base parses, each case does
    /// not.
    #[test]
    fn each_malformed_stop_fails_the_typed_parse() {
        for (id, v) in malformed_stops() {
            assert!(
                serde_json::from_value::<super::CapacityStop>(v).is_err(),
                "{id} must not parse"
            );
        }
        let mut base = malformed_stops().remove(0).1;
        base["resume_at"] = serde_json::json!(chrono::Utc::now().to_rfc3339());
        assert!(serde_json::from_value::<super::CapacityStop>(base).is_ok(), "the base parses");
    }

    /// Recorded resumes past what the JSON reader takes as a typed value,
    /// as raw record text, by the session id that carries each: a number out
    /// of range (`1e400`) in `attempt` and in `resume_at`, and a resume
    /// nested deeper than the reader's 128 levels (arrays, objects). Each is
    /// valid JSON and holds [`SENTINEL`]. No `serde_json::Value` can hold
    /// the first two, and every typed read (a `Value` too) refuses all four.
    pub(crate) fn raw_malformed_stops() -> Vec<(&'static str, String)> {
        let now = chrono::Utc::now();
        let stop = |resume_at: &str, attempt: &str| {
            format!(
                "{{\"resume_at\":{resume_at},\"stopped_at\":\"{}\",\"kind\":\"{SENTINEL}\",\
                 \"history_len\":3,\"last_digest\":\"{SENTINEL}\",\"attempt\":{attempt}}}",
                now.to_rfc3339()
            )
        };
        let resume_at = format!("\"{}\"", (now + chrono::Duration::hours(1)).to_rfc3339());
        let deep = 200;
        vec![
            ("s-attempt-1e400", stop(&resume_at, "1e400")),
            ("s-time-1e400", stop("1e400", "0")),
            ("s-nested-arrays", format!("{}\"{SENTINEL}\"{}", "[".repeat(deep), "]".repeat(deep))),
            (
                "s-nested-objects",
                format!("{}\"{SENTINEL}\"{}", "{\"kind\":".repeat(deep), "}".repeat(deep)),
            ),
        ]
    }

    /// A record's bytes as a reader finds them on disk: `doc` (the record
    /// as a `Value`) with each of `stops` written, as raw text, as the
    /// `capacity_stop` of the entry under `sessions` with that id.
    pub(crate) fn with_raw_stops(mut doc: serde_json::Value, stops: &[(&str, String)]) -> Vec<u8> {
        let placeholder = |n: usize| format!("capres-raw-stop-{n}");
        for (n, (id, _)) in stops.iter().enumerate() {
            let entry = doc["sessions"]
                .as_array_mut()
                .expect("sessions")
                .iter_mut()
                .find(|s| s["id"] == *id)
                .expect("an entry with that id");
            entry["capacity_stop"] = serde_json::json!(placeholder(n));
        }
        let mut text = serde_json::to_string(&doc).expect("the record serializes");
        for (n, (_, raw)) in stops.iter().enumerate() {
            let quoted = format!("\"{}\"", placeholder(n));
            assert_eq!(text.matches(&quoted).count(), 1, "one place for {quoted}");
            text = text.replace(&quoted, raw);
        }
        text.into_bytes()
    }

    /// The raw fixtures are what they claim: valid JSON (a syntax-only skip
    /// takes each), refused by every typed read, and the same text with an
    /// in-range number parses.
    #[test]
    fn each_raw_malformed_stop_is_valid_json_past_the_typed_reader() {
        for (id, raw) in raw_malformed_stops() {
            assert!(serde_json::from_str::<serde::de::IgnoredAny>(&raw).is_ok(), "{id}: valid JSON");
            assert!(serde_json::from_str::<serde_json::Value>(&raw).is_err(), "{id}: no Value reads it");
            assert!(serde_json::from_str::<super::CapacityStop>(&raw).is_err(), "{id}: no typed read");
            assert!(raw.contains(SENTINEL), "{id}");
        }
        let in_range = raw_malformed_stops().remove(0).1.replace("1e400", "0");
        assert!(serde_json::from_str::<super::CapacityStop>(&in_range).is_ok(), "the base parses");
    }

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

    fn captured<T>(subscriber_for: impl FnOnce(Capture) -> tracing::Dispatch, f: impl FnOnce() -> T) -> (T, String) {
        let out = Capture::default();
        let result = tracing::dispatcher::with_default(&subscriber_for(out.clone()), f);
        let text = String::from_utf8(out.0.lock().unwrap_or_else(|p| p.into_inner()).clone()).expect("utf-8");
        (result, text)
    }

    /// `f`'s log lines through the daemon's own filter
    /// (`with_crate_directives` over a `RUST_LOG`-unset base).
    pub(crate) fn daemon_log<T>(f: impl FnOnce() -> T) -> (T, String) {
        use tracing_subscriber::filter::{EnvFilter, LevelFilter};
        captured(
            |out| {
                let unset = EnvFilter::builder()
                    .with_default_directive(LevelFilter::ERROR.into())
                    .parse_lossy("");
                tracing_subscriber::fmt()
                    .with_env_filter(agent_core::logging::with_crate_directives(unset))
                    .with_writer(out)
                    .with_ansi(false)
                    .finish()
                    .into()
            },
            f,
        )
    }

    /// The control line [`all_levels_log`] emits at TRACE before `f` runs.
    pub(crate) const TRACE_CONTROL: &str = "capres test control: a trace line";

    /// `f`'s log lines at EVERY level and target (TRACE included), so a
    /// test can show that a stored value reaches no line at all. The
    /// capture holds [`TRACE_CONTROL`] (checked here), so it does reach
    /// TRACE.
    pub(crate) fn all_levels_log<T>(f: impl FnOnce() -> T) -> (T, String) {
        let (value, text) = captured(
            |out| {
                tracing_subscriber::fmt()
                    .with_max_level(tracing::Level::TRACE)
                    .with_writer(out)
                    .with_ansi(false)
                    .finish()
                    .into()
            },
            || {
                tracing::trace!(target: "agent_engine::session::capacity_resume", "{TRACE_CONTROL}");
                f()
            },
        );
        assert!(text.contains(TRACE_CONTROL), "the capture reaches TRACE: {text:?}");
        (value, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(h: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(1_800_000_000, 0).unwrap() + chrono::Duration::hours(h)
    }

    fn stop(attempt: u8) -> CapacityStop {
        CapacityStop {
            resume_at: at(30),
            stopped_at: at(0),
            kind: "all_cooling_down".into(),
            history_len: 3,
            last_digest: "0f".repeat(32),
            attempt,
        }
    }

    /// A turn no capacity resume started arms with attempt 0; a resumed
    /// turn arms with the resumes its chain dispatched, up to the third,
    /// and never a fourth.
    #[test]
    fn the_attempt_counts_dispatched_resumes_and_stops_at_three() {
        assert_eq!(next_attempt(at(30), None), Ok(0), "a first stop");
        let chain = |dispatched| Some(Provenance { dispatched, prev_resume_at: at(10) });
        assert_eq!(next_attempt(at(30), chain(1)), Ok(1));
        assert_eq!(next_attempt(at(30), chain(2)), Ok(2), "the third arm");
        assert_eq!(next_attempt(at(30), chain(3)), Err(NoArm::LoopCap), "no fourth");
    }

    /// A resumed turn that stops again arms only for a strictly later reset.
    #[test]
    fn a_resumed_turn_arms_again_only_for_a_later_reset() {
        let chain = Some(Provenance { dispatched: 1, prev_resume_at: at(10) });
        assert_eq!(next_attempt(at(10), chain), Err(NoArm::NotLater), "the same reset");
        assert_eq!(next_attempt(at(9), chain), Err(NoArm::NotLater), "an earlier one");
        assert_eq!(next_attempt(at(10) + chrono::Duration::seconds(1), chain), Ok(1));
    }

    /// Every kind the engine notes is valid; anything else is not.
    #[test]
    fn the_stop_kinds_are_a_closed_set() {
        for k in [
            "all_cooling_down",
            "credential_rejected",
            "action_required",
            "usage_read_failed",
            "quota_unknown",
            "window_limit",
        ] {
            assert_eq!(StopKind::parse(k).map(StopKind::as_str), Some(k));
        }
        for k in ["", "no_credentials", "seat_churn", "ALL_COOLING_DOWN", "all_cooling_down "] {
            assert_eq!(StopKind::parse(k), None, "{k:?}");
        }
    }

    /// A well-formed stop is valid, an overdue one too; each malformed
    /// field is refused with its own fixed class.
    #[test]
    fn a_restored_stop_is_validated_field_by_field() {
        let now = at(1);
        assert_eq!(validate(&stop(0), now).map(|v| v.kind), Ok(StopKind::AllCoolingDown));
        assert!(validate(&stop(2), now).is_ok(), "attempt 2");
        assert!(validate(&stop(0), at(40)).is_ok(), "overdue but inside the horizon");

        let bad = |f: &dyn Fn(&mut CapacityStop)| {
            let mut s = stop(0);
            f(&mut s);
            validate(&s, now).err()
        };
        assert_eq!(bad(&|s| s.kind = "<script>".into()), Some(Invalid::Kind));
        assert_eq!(bad(&|s| s.last_digest = "0F".repeat(32)), Some(Invalid::Digest), "upper case");
        assert_eq!(bad(&|s| s.last_digest = "0f".repeat(8)), Some(Invalid::Digest), "16 hex");
        assert_eq!(bad(&|s| s.last_digest = "zz".repeat(32)), Some(Invalid::Digest));
        assert_eq!(bad(&|s| s.attempt = 3), Some(Invalid::Attempt));
        assert_eq!(bad(&|s| s.attempt = 255), Some(Invalid::Attempt));
        assert_eq!(bad(&|s| s.resume_at = s.stopped_at), Some(Invalid::Time), "reset at the stop");
        assert_eq!(bad(&|s| s.stopped_at = at(1) + chrono::Duration::minutes(3)), Some(Invalid::Time), "a stop in the future");
        assert!(bad(&|s| s.stopped_at = at(1) + chrono::Duration::minutes(1)).is_none(), "inside the skew");
        assert_eq!(bad(&|s| s.resume_at = at(0) + horizon() + chrono::Duration::seconds(1)), Some(Invalid::BeyondHorizon));
        assert!(bad(&|s| s.resume_at = at(0) + horizon()).is_none(), "the horizon's edge");
        assert_eq!(validate(&stop(0), at(0) + horizon() + chrono::Duration::seconds(1)).err(), Some(Invalid::Stale));
    }

    /// The digest is 64 lowercase hex, pins the last message only, and is
    /// stable across a JSON round trip (what a reload or a restore loads).
    #[test]
    fn the_digest_pins_the_last_message() {
        let m = |t: &str| -> crate::SharedMessage {
            std::sync::Arc::new(serde_json::json!({"role": "user", "content": [{"type": "text", "text": t}]}))
        };
        let a = last_digest(&[m("one"), m("two")]);
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert_eq!(a, last_digest(&[m("zero"), m("two")]), "only the last message counts");
        assert_ne!(a, last_digest(&[m("one"), m("three")]));
        let reparsed: serde_json::Value = serde_json::from_slice(&serde_json::to_vec(m("two").as_ref()).unwrap()).unwrap();
        assert_eq!(a, last_digest(&[std::sync::Arc::new(reparsed)]));
        assert_eq!(last_digest(&[]).len(), 64);
    }

    /// A parked entry past its eviction age is restored only for a resume
    /// that validates with the switch on; an invalid, stale or switched-off
    /// one, or none, is left on disk as before (it can't pin anything).
    #[test]
    fn only_a_valid_resume_spares_a_parked_entry_past_its_eviction() {
        let now = at(1);
        assert!(spares_parked(Some(&stop(0)), true, now));
        assert!(spares_parked(Some(&stop(0)), true, at(40)), "overdue: it fires once restored");
        assert!(!spares_parked(None, true, now), "no resume");
        assert!(!spares_parked(Some(&stop(0)), false, now), "the switch is off");
        let mut bad = stop(0);
        bad.kind = "seat_churn".into();
        assert!(!spares_parked(Some(&bad), true, now), "an unknown kind");
        assert!(!spares_parked(Some(&stop(3)), true, now), "a fourth attempt");
        assert!(!spares_parked(Some(&stop(0)), true, at(0) + horizon() + chrono::Duration::seconds(1)), "stale");
    }

    /// The texts name the kind's situation and UTC times only.
    #[test]
    fn the_texts_name_utc_times_only() {
        let text = resume_text(at(0), at(30));
        assert!(text.starts_with("resume: "), "{text}");
        assert!(text.contains(&at(0).format("%H:%M UTC").to_string()), "{text}");
        assert!(text.contains(&at(30).format("%H:%M UTC").to_string()), "{text}");
        assert!(!text.contains('@') && !text.contains("claude"), "{text}");
        assert!(armed_notice(at(30)).contains(&at(30).format("%H:%M UTC").to_string()));
        assert!(random_jitter() <= MAX_JITTER);
    }
}
