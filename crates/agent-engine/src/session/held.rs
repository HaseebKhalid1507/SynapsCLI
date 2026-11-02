//! Focus hold mode (S1): the client-visible types.
//!
//! The contract is `pria-apps/.local/plans/2026-10-21-focus-side-chats-synaps-wire.md`
//! v1.1 (§1 the commands, §2 the queries, §3 the events, §4 `HeldItem`). Every
//! name and shape here is final there; the serde attributes below produce
//! exactly its JSON, and `wire_tests` pins each example line. All of it is
//! additive: `PROTOCOL_VERSION` stays 3, an older daemon answers the new
//! commands "malformed frame", and an older client decodes the new events as
//! `Unknown`.
//!
//! While a session's delivery mode is `hold`, every event that would reach
//! its context goes to the session's held store instead (one exception: a
//! completion of a subagent this session started, arriving while a turn
//! runs, still steers into that turn). Nothing here is a `SessionSetting`:
//! the mode and its lease live in the held store only, so a reload record
//! never carries them and an older image never sees them.

use serde::{Deserialize, Serialize};

/// `delivery` hold: the shortest lease a client may ask for.
pub const LEASE_SECS_MIN: u64 = 30;
/// `delivery` hold: the longest lease a client may ask for.
pub const LEASE_SECS_MAX: u64 = 3600;
/// `held_release.text`: at most this many UTF-8 bytes.
pub const RELEASE_TEXT_MAX_BYTES: usize = 16384;
/// Items per session in state held or join_next; past it the oldest below
/// high severity fold into one `held_fold` item.
pub const HELD_BOUND: usize = 1000;
/// `HeldItem.headline`: at most this many characters.
pub const HEADLINE_MAX_CHARS: usize = 140;
/// `held_show` for an id the store does not know.
pub const UNKNOWN_HELD_ID: &str = "unknown held id";
/// The `type` of a fold item.
pub const HELD_FOLD_TYPE: &str = "held_fold";
/// `content_type` of the digest's event (§3.1).
pub const HELD_DIGEST_TYPE: &str = "held_digest";
/// `content_type` of the join_next block's event (§3.1).
pub const HELD_JOIN_TYPE: &str = "held_join";

/// `delivery.mode`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    /// Today's delivery.
    #[default]
    Auto,
    /// Events go to the held store instead of the context.
    Hold,
}

impl DeliveryMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Hold => "hold",
        }
    }
}

/// `held_release.as`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseAs {
    /// ONE user-role message with every released item (or the app's text).
    Digest,
    /// In front of the next user message, as one block; no turn of its own.
    JoinNext,
    /// Back into the event queue in arrival order, delivered as today.
    Individual,
}

impl ReleaseAs {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Digest => "digest",
            Self::JoinNext => "join_next",
            Self::Individual => "individual",
        }
    }
}

/// `HeldItem.state`; `held_updated.state` (never `held`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeldState {
    Held,
    JoinNext,
    Released,
    Dismissed,
    HandledOnSide,
}

impl HeldState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Held => "held",
            Self::JoinNext => "join_next",
            Self::Released => "released",
            Self::Dismissed => "dismissed",
            Self::HandledOnSide => "handled_on_side",
        }
    }

    /// Counted against [`HELD_BOUND`].
    pub fn is_pending(self) -> bool {
        matches!(self, Self::Held | Self::JoinNext)
    }

    /// What `held_release` with `ids: null` takes.
    pub fn is_releasable_by_all(self) -> bool {
        matches!(self, Self::Held | Self::JoinNext | Self::HandledOnSide)
    }

    /// Left out of `held_list`.
    pub fn is_listed(self) -> bool {
        !matches!(self, Self::Released | Self::Dismissed)
    }
}

/// `delivery_changed.reason`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryReason {
    /// A client's `delivery` command changed the mode.
    Set,
    /// A client's `delivery` hold while already in hold: a new lease.
    Renewed,
    /// The lease ran out: the daemon set auto and released everything.
    LeaseExpired,
    /// A reload or restart brought hold back from the store.
    Restored,
}

/// `HeldItem.severity`: the event's severity, lowercase (an event without
/// one renders `medium` in its envelope, and is `medium` here too).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeldSeverity {
    Low,
    Medium,
    High,
    Critical,
}

impl HeldSeverity {
    pub fn of(severity: Option<&crate::events::types::Severity>) -> Self {
        use crate::events::types::Severity;
        match severity {
            Some(Severity::Low) => Self::Low,
            None | Some(Severity::Medium) => Self::Medium,
            Some(Severity::High) => Self::High,
            Some(Severity::Critical) => Self::Critical,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }

    /// High and critical items are never folded.
    pub fn is_high(self) -> bool {
        self >= Self::High
    }
}

/// One held event (§4). The full text is not part of it: `held_show` and
/// the store add it ([`HeldItemWithText`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldItem {
    /// `"h-"` + 16 lowercase hex, random ([`new_held_id`]).
    pub id: String,
    /// Arrival, ms since the epoch.
    pub ts_ms: u64,
    /// The event's `content_type` (`message`, `subagent_completion`, ...),
    /// or [`HELD_FOLD_TYPE`].
    #[serde(rename = "type")]
    pub kind: String,
    pub source: String,
    pub severity: HeldSeverity,
    /// A completion of a subagent THIS session started.
    pub own_subagent: bool,
    /// The first line of the text, at most [`HEADLINE_MAX_CHARS`]
    /// characters, sanitized like the event envelope.
    pub headline: String,
    pub state: HeldState,
    /// Set by `held_mark`.
    pub side_session: Option<String>,
}

/// `HeldItem` plus its full text: `held_show`'s value and the store's item
/// line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldItemWithText {
    #[serde(flatten)]
    pub item: HeldItem,
    pub text: String,
}

/// `held_list`'s value: the mode, the lease, and every item not released
/// or dismissed (no text).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldList {
    pub mode: DeliveryMode,
    pub lease_until_ms: Option<u64>,
    pub items: Vec<HeldItem>,
}

/// A fresh held id: `"h-"` + 16 lowercase hex digits from the OS RNG.
pub fn new_held_id() -> String {
    format!("h-{:016x}", rand::random::<u64>())
}

/// `"h-"` + exactly 16 lowercase hex digits.
pub fn is_held_id(id: &str) -> bool {
    id.strip_prefix("h-").is_some_and(|hex| {
        hex.len() == 16 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

#[cfg(test)]
mod wire_tests {
    //! Every example line of the wire contract v1.1, decoded and encoded
    //! back to the SAME bytes (field order included), plus the old-client
    //! view of the new events.

    use super::*;
    use crate::session::wire::WireSessionEvent;
    use crate::session::{SessionCommand, SessionEventWire, SessionQuery};

    const ID: &str = "h-0123456789abcdef";

    fn same_bytes<T: Serialize + for<'de> Deserialize<'de>>(line: &str) -> T {
        let v: T = serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}"));
        assert_eq!(serde_json::to_string(&v).unwrap(), line, "round trip changed the bytes");
        v
    }

    fn item() -> HeldItem {
        HeldItem {
            id: ID.into(),
            ts_ms: 1761026400000,
            kind: "message".into(),
            source: "master".into(),
            severity: HeldSeverity::Low,
            own_subagent: false,
            headline: "Master -> SC (00:2x ...): ...".into(),
            state: HeldState::Held,
            side_session: None,
        }
    }

    const ITEM_JSON: &str = concat!(
        r#"{"id":"h-0123456789abcdef","ts_ms":1761026400000,"type":"message","source":"master","#,
        r#""severity":"low","own_subagent":false,"headline":"Master -> SC (00:2x ...): ...","#,
        r#""state":"held","side_session":null}"#
    );

    #[test]
    fn delivery_hold_and_auto() {
        match same_bytes::<SessionCommand>(r#"{"cmd":"delivery","mode":"hold","lease_secs":300}"#) {
            SessionCommand::Delivery { mode: DeliveryMode::Hold, lease_secs: Some(300) } => {}
            other => panic!("{other:?}"),
        }
        match same_bytes::<SessionCommand>(r#"{"cmd":"delivery","mode":"auto"}"#) {
            SessionCommand::Delivery { mode: DeliveryMode::Auto, lease_secs: None } => {}
            other => panic!("{other:?}"),
        }
        // A client that sends a lease with auto: decoded (it is ignored).
        let v: SessionCommand =
            serde_json::from_str(r#"{"cmd":"delivery","mode":"auto","lease_secs":60}"#).unwrap();
        assert!(matches!(v, SessionCommand::Delivery { mode: DeliveryMode::Auto, lease_secs: Some(60) }));
        assert!(serde_json::from_str::<SessionCommand>(r#"{"cmd":"delivery","mode":"later"}"#).is_err());
    }

    #[test]
    fn held_release_every_form() {
        match same_bytes::<SessionCommand>(r#"{"cmd":"held_release","ids":null,"as":"digest","text":null}"#) {
            SessionCommand::HeldRelease { ids: None, release_as: ReleaseAs::Digest, text: None } => {}
            other => panic!("{other:?}"),
        }
        match same_bytes::<SessionCommand>(
            r#"{"cmd":"held_release","ids":["h-0123456789abcdef"],"as":"join_next","text":null}"#,
        ) {
            SessionCommand::HeldRelease { ids: Some(ids), release_as: ReleaseAs::JoinNext, text: None } => {
                assert_eq!(ids, vec![ID.to_string()])
            }
            other => panic!("{other:?}"),
        }
        match same_bytes::<SessionCommand>(
            r#"{"cmd":"held_release","ids":[],"as":"join_next","text":"[side Fix tray] Summary for the main chat: ..."}"#,
        ) {
            SessionCommand::HeldRelease { ids: Some(ids), release_as: ReleaseAs::JoinNext, text: Some(t) } => {
                assert!(ids.is_empty());
                assert_eq!(t, "[side Fix tray] Summary for the main chat: ...");
            }
            other => panic!("{other:?}"),
        }
        match same_bytes::<SessionCommand>(
            r#"{"cmd":"held_release","ids":["h-0123456789abcdef"],"as":"individual","text":null}"#,
        ) {
            SessionCommand::HeldRelease { release_as: ReleaseAs::Individual, .. } => {}
            other => panic!("{other:?}"),
        }
        // Absent ids/text read as null.
        let v: SessionCommand = serde_json::from_str(r#"{"cmd":"held_release","as":"digest"}"#).unwrap();
        assert!(matches!(v, SessionCommand::HeldRelease { ids: None, text: None, .. }));
    }

    #[test]
    fn held_dismiss_and_mark() {
        match same_bytes::<SessionCommand>(r#"{"cmd":"held_dismiss","ids":["h-0123456789abcdef"]}"#) {
            SessionCommand::HeldDismiss { ids } => assert_eq!(ids, vec![ID.to_string()]),
            other => panic!("{other:?}"),
        }
        match same_bytes::<SessionCommand>(
            r#"{"cmd":"held_mark","id":"h-0123456789abcdef","side_session":"20261021-013000-ab12"}"#,
        ) {
            SessionCommand::HeldMark { id, side_session } => {
                assert_eq!(id, ID);
                assert_eq!(side_session, "20261021-013000-ab12");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn held_queries() {
        match same_bytes::<SessionCommand>(r#"{"cmd":"query","id":7,"query":{"query":"held_list"}}"#) {
            SessionCommand::Query { id: 7, query: SessionQuery::HeldList } => {}
            other => panic!("{other:?}"),
        }
        match same_bytes::<SessionCommand>(
            r#"{"cmd":"query","id":8,"query":{"query":"held_show","held_id":"h-0123456789abcdef"}}"#,
        ) {
            SessionCommand::Query { id: 8, query: SessionQuery::HeldShow { held_id } } => assert_eq!(held_id, ID),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn held_item_and_query_values() {
        assert_eq!(serde_json::to_string(&item()).unwrap(), ITEM_JSON);
        assert_eq!(same_bytes::<HeldItem>(ITEM_JSON), item());
        let marked = HeldItem {
            kind: "subagent_completion".into(),
            severity: HeldSeverity::Critical,
            own_subagent: true,
            state: HeldState::HandledOnSide,
            side_session: Some("20261021-013000-ab12".into()),
            ..item()
        };
        let back = same_bytes::<HeldItem>(&serde_json::to_string(&marked).unwrap());
        assert_eq!(back, marked);
        let json = serde_json::to_value(&marked).unwrap();
        assert_eq!(json["type"], "subagent_completion");
        assert_eq!(json["severity"], "critical");
        assert_eq!(json["state"], "handled_on_side");

        let list = HeldList { mode: DeliveryMode::Hold, lease_until_ms: Some(1761027000000), items: vec![item()] };
        let line = format!(r#"{{"mode":"hold","lease_until_ms":1761027000000,"items":[{ITEM_JSON}]}}"#);
        assert_eq!(serde_json::to_string(&list).unwrap(), line);
        assert_eq!(same_bytes::<HeldList>(&line), list);
        assert_eq!(
            serde_json::to_string(&HeldList { mode: DeliveryMode::Auto, lease_until_ms: None, items: vec![] })
                .unwrap(),
            r#"{"mode":"auto","lease_until_ms":null,"items":[]}"#
        );

        let shown = HeldItemWithText { item: item(), text: "line one\nline two".into() };
        let line = format!(r#"{},"text":"line one\nline two"}}"#, &ITEM_JSON[..ITEM_JSON.len() - 1]);
        assert_eq!(serde_json::to_string(&shown).unwrap(), line);
        assert_eq!(same_bytes::<HeldItemWithText>(&line), shown);
        assert_eq!(serde_json::json!({ "error": UNKNOWN_HELD_ID }).to_string(), r#"{"error":"unknown held id"}"#);
    }

    #[test]
    fn held_events() {
        let line = format!(r#"{{"ev":"event_held","item":{ITEM_JSON}}}"#);
        match same_bytes::<WireSessionEvent>(&line) {
            WireSessionEvent::EventHeld { item: got } => assert_eq!(got, item()),
            other => panic!("{other:?}"),
        }
        match same_bytes::<WireSessionEvent>(
            r#"{"ev":"delivery_changed","mode":"hold","lease_until_ms":1761027000000,"reason":"set"}"#,
        ) {
            WireSessionEvent::DeliveryChanged {
                mode: DeliveryMode::Hold,
                lease_until_ms: Some(1761027000000),
                reason: DeliveryReason::Set,
            } => {}
            other => panic!("{other:?}"),
        }
        for (reason, json) in [
            (DeliveryReason::Renewed, "renewed"),
            (DeliveryReason::LeaseExpired, "lease_expired"),
            (DeliveryReason::Restored, "restored"),
        ] {
            let line = format!(r#"{{"ev":"delivery_changed","mode":"auto","lease_until_ms":null,"reason":"{json}"}}"#);
            match same_bytes::<WireSessionEvent>(&line) {
                WireSessionEvent::DeliveryChanged { mode: DeliveryMode::Auto, lease_until_ms: None, reason: r } => {
                    assert_eq!(r, reason)
                }
                other => panic!("{other:?}"),
            }
        }
        for (state, json) in [
            (HeldState::JoinNext, "join_next"),
            (HeldState::Released, "released"),
            (HeldState::Dismissed, "dismissed"),
            (HeldState::HandledOnSide, "handled_on_side"),
        ] {
            let line = format!(r#"{{"ev":"held_updated","ids":["h-0123456789abcdef"],"state":"{json}"}}"#);
            match same_bytes::<WireSessionEvent>(&line) {
                WireSessionEvent::HeldUpdated { ids, state: s } => {
                    assert_eq!(ids, vec![ID.to_string()]);
                    assert_eq!(s, state);
                }
                other => panic!("{other:?}"),
            }
        }
    }

    /// In-process and on the wire, the three events convert both ways.
    #[test]
    fn held_events_cross_the_socket_boundary() {
        let cases = [
            SessionEventWire::EventHeld { item: item() },
            SessionEventWire::DeliveryChanged {
                mode: DeliveryMode::Hold,
                lease_until_ms: Some(1761027000000),
                reason: DeliveryReason::Renewed,
            },
            SessionEventWire::HeldUpdated { ids: vec![ID.into()], state: HeldState::Dismissed },
        ];
        for ev in cases {
            let before = format!("{ev:?}");
            let wire = WireSessionEvent::for_client(ev).expect("always sent");
            let line = serde_json::to_string(&wire).unwrap();
            let back: WireSessionEvent = serde_json::from_str(&line).unwrap();
            assert_eq!(format!("{:?}", SessionEventWire::from(back)), before, "{line}");
        }
        // Always sent: no opt-in name.
        assert_eq!(
            crate::session::wire::optional_event_name(&SessionEventWire::HeldUpdated {
                ids: vec![],
                state: HeldState::Released
            }),
            None
        );
    }

    /// An older client (no hold variants) reads all three as `Unknown`.
    #[test]
    fn old_clients_drop_the_new_events() {
        #[derive(Deserialize)]
        #[serde(tag = "ev", rename_all = "snake_case")]
        #[allow(dead_code)]
        enum OldEvent {
            Idle,
            #[serde(other)]
            Unknown,
        }
        for line in [
            format!(r#"{{"ev":"event_held","item":{ITEM_JSON}}}"#),
            r#"{"ev":"delivery_changed","mode":"auto","lease_until_ms":null,"reason":"lease_expired"}"#.to_string(),
            r#"{"ev":"held_updated","ids":[],"state":"released"}"#.to_string(),
        ] {
            assert!(matches!(serde_json::from_str::<OldEvent>(&line).unwrap(), OldEvent::Unknown), "{line}");
        }
    }

    #[test]
    fn held_ids() {
        for _ in 0..64 {
            let id = new_held_id();
            assert!(is_held_id(&id), "{id}");
        }
        assert!(is_held_id(ID));
        for bad in ["h-0123456789ABCDEF", "h-0123456789abcde", "h-0123456789abcdef0", "x-0123456789abcdef", ""] {
            assert!(!is_held_id(bad), "{bad}");
        }
    }

    #[test]
    fn severity_and_state_rules() {
        use crate::events::types::Severity;
        assert_eq!(HeldSeverity::of(None), HeldSeverity::Medium);
        assert_eq!(HeldSeverity::of(Some(&Severity::Low)), HeldSeverity::Low);
        assert_eq!(HeldSeverity::of(Some(&Severity::Critical)), HeldSeverity::Critical);
        assert!(HeldSeverity::High.is_high() && HeldSeverity::Critical.is_high());
        assert!(!HeldSeverity::Medium.is_high());
        assert!(HeldState::Held.is_pending() && HeldState::JoinNext.is_pending());
        assert!(!HeldState::HandledOnSide.is_pending());
        assert!(HeldState::HandledOnSide.is_releasable_by_all());
        assert!(!HeldState::Dismissed.is_releasable_by_all());
        assert!(!HeldState::Released.is_listed() && !HeldState::Dismissed.is_listed());
        assert!(HeldState::HandledOnSide.is_listed());
    }
}
