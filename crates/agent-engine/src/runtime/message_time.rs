//! When each message happened: the host-only `_synaps_at` key.
//!
//! API history (role/content JSON) has no per-message time, so a transcript
//! rebuilt from it (resume, attach, `/resync`, compaction) could only show
//! "now". The host stamps every NEW message with its own time, in memory,
//! under [`AT_KEY`] (RFC 3339 UTC). Like every `_synaps_*` key it never
//! reaches a provider (`continuation::WireMessages`), and persistence keeps
//! it beside the message, never inside (`session_journal::MESSAGE_AT_KEY`).
//!
//! Stamping never re-stamps and never touches settled history: a
//! [`Stamper`] only looks at messages appended since its last pass (its
//! floor), from the tail backward, and stops at the first message that
//! already has a time. Messages loaded without a time (legacy sessions)
//! stay without one and render with an empty time, never "now".

use std::sync::Arc;

use chrono::{DateTime, SecondsFormat, Utc};

use crate::SharedMessage;

/// The in-memory message time key.
pub const AT_KEY: &str = agent_core::core::session_journal::MESSAGE_AT_KEY;

/// Where message times come from. Defaults to the wall clock; tests inject
/// their own to "advance time" between a turn and a rebuild.
/// One thin pointer: it rides in `SessionConfig` (an `Attach` payload).
#[derive(Clone)]
pub struct MessageClock(Arc<ClockFn>);

type ClockFn = Box<dyn Fn() -> DateTime<Utc> + Send + Sync>;

impl MessageClock {
    pub fn new(now: impl Fn() -> DateTime<Utc> + Send + Sync + 'static) -> Self {
        Self(Arc::new(Box::new(now)))
    }

    pub fn now(&self) -> DateTime<Utc> {
        (self.0)()
    }
}

impl Default for MessageClock {
    fn default() -> Self {
        Self::new(Utc::now)
    }
}

impl std::fmt::Debug for MessageClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MessageClock")
    }
}

/// The stored form of a time: RFC 3339, UTC, whole seconds.
pub fn format_time(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// The recorded time of a message, if any.
pub fn message_time(message: &serde_json::Value) -> Option<&str> {
    agent_core::core::session_journal::message_time(message)
}

/// Stamp `messages[floor..]` that lack a time with `at`, walking back from
/// the tail and stopping at the first message that has one. Returns how many
/// were stamped. O(new messages).
pub fn stamp_new(messages: &mut [SharedMessage], floor: usize, at: DateTime<Utc>) -> usize {
    let mut stamp = None;
    let mut stamped = 0;
    let floor = floor.min(messages.len());
    for message in messages[floor..].iter_mut().rev() {
        if message.get(AT_KEY).is_some() || !message.is_object() {
            break;
        }
        let ts = stamp.get_or_insert_with(|| format_time(at)).clone();
        if let Some(object) = Arc::make_mut(message).as_object_mut() {
            object.insert(AT_KEY.into(), serde_json::Value::String(ts));
            stamped += 1;
        }
    }
    stamped
}

/// Stamps what was appended to one history since the previous pass.
///
/// The floor starts at the history's length when the stamper is created
/// (everything already there is settled) and moves to the end after every
/// pass. A history replaced by a shorter one (rollover, repair) clamps it.
#[derive(Debug, Clone, Default)]
pub struct Stamper {
    floor: usize,
}

impl Stamper {
    pub fn new(settled: usize) -> Self {
        Self { floor: settled }
    }

    /// Stamp every new message of `messages` with `clock`'s time; returns
    /// how many.
    pub fn stamp(&mut self, messages: &mut [SharedMessage], clock: &MessageClock) -> usize {
        let floor = self.floor.min(messages.len());
        self.floor = messages.len();
        if floor == messages.len() {
            return 0;
        }
        stamp_new(messages, floor, clock.now())
    }

    /// Treat `settled` messages as already handled (a history loaded or
    /// replaced wholesale).
    pub fn settle(&mut self, settled: usize) {
        self.floor = settled;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at(min: u32) -> DateTime<Utc> {
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 10, 25, 5, min, 0).unwrap()
    }

    fn msgs(n: usize) -> Vec<SharedMessage> {
        (0..n)
            .map(|i| Arc::new(json!({"role": "user", "content": format!("m{i}")})))
            .collect()
    }

    fn times(m: &[SharedMessage]) -> Vec<Option<&str>> {
        m.iter().map(|m| message_time(m)).collect()
    }

    #[test]
    fn stamping_is_idempotent_and_only_touches_new_messages() {
        // Three legacy messages: settled, never stamped.
        let mut m = msgs(3);
        let clock_min = Arc::new(std::sync::atomic::AtomicU32::new(47));
        let c = clock_min.clone();
        let clock = MessageClock::new(move || at(c.load(std::sync::atomic::Ordering::SeqCst)));
        let mut stamper = Stamper::new(m.len());
        assert_eq!(stamper.stamp(&mut m, &clock), 0);
        assert_eq!(times(&m), vec![None, None, None]);

        m.extend(msgs(2));
        assert_eq!(stamper.stamp(&mut m, &clock), 2);
        let first = "2026-10-25T05:47:00Z";
        assert_eq!(times(&m), vec![None, None, None, Some(first), Some(first)]);
        let ptrs: Vec<_> = m.iter().map(Arc::as_ptr).collect();

        // Later passes never re-stamp, even with the clock moved on.
        clock_min.store(58, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(stamper.stamp(&mut m, &clock), 0);
        assert_eq!(stamp_new(&mut m, 0, at(58)), 0, "stops at the stamped tail");
        assert_eq!(m.iter().map(Arc::as_ptr).collect::<Vec<_>>(), ptrs, "untouched");

        m.extend(msgs(1));
        assert_eq!(stamper.stamp(&mut m, &clock), 1);
        assert_eq!(message_time(&m[5]), Some("2026-10-25T05:58:00Z"));
        assert_eq!(message_time(&m[3]), Some(first));
    }

    #[test]
    fn a_shorter_replacement_history_clamps_the_floor() {
        let clock = MessageClock::new(|| at(1));
        let mut stamper = Stamper::new(4);
        // Rollover: a fresh, shorter head; its survivors keep their times.
        let mut head = msgs(2);
        assert_eq!(stamper.stamp(&mut head, &clock), 0);
        head.extend(msgs(1));
        assert_eq!(stamper.stamp(&mut head, &clock), 1);
        assert_eq!(times(&head), vec![None, None, Some("2026-10-25T05:01:00Z")]);
    }
}
