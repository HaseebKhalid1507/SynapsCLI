//! C5a RED → GREEN tests: protocol frames + reactor payload constructor.
//!
//! Tests that must fail before implementation, pass after:
//!
//!  1. `EventPayload` serde roundtrip (reactor.rs `event_payload_from_drained`)
//!  2. `RpcEvent::Event` round-trip (additive wire frame)
//!  3. `EventsConfig` default (auto_turn = true)
//!  4. `EventsConfig` parse from config key `events.auto_turn = true`
//!
//! RPC_PROTOCOL_VERSION must stay 1 (additive variant, no break).

use synaps_cli::core::config::{load_config_from_str, EventsConfig};
use synaps_cli::core::rpc_protocol::{RpcEvent, RPC_PROTOCOL_VERSION};
use synaps_cli::engine::reactor::EventPayload;

// ─── 1. EventPayload serde roundtrip ─────────────────────────────────────────

#[test]
fn event_payload_serde_roundtrip() {
    let p = EventPayload {
        id: "evt-1".into(),
        source: "cli".into(),
        severity: "high".into(),
        content_type: "message".into(),
        text: "hello".into(),
        timestamp: "2025-01-01T00:00:00Z".into(),
        formatted: "<event>hello</event>".into(),
    };
    let json = serde_json::to_string(&p).expect("serialize");
    let back: EventPayload = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.id, p.id);
    assert_eq!(back.source, p.source);
    assert_eq!(back.severity, p.severity);
    assert_eq!(back.content_type, p.content_type);
    assert_eq!(back.text, p.text);
    assert_eq!(back.timestamp, p.timestamp);
    assert_eq!(back.formatted, p.formatted);
}

// ─── 2. RpcEvent::Event round-trip ───────────────────────────────────────────

#[test]
fn rpc_event_event_round_trip() {
    let ev = RpcEvent::Event {
        payload: Box::new(synaps_cli::core::rpc_protocol::EventPayload {
            id: "e-1".into(),
            source: "uptime-kuma".into(),
            severity: "critical".into(),
            content_type: "alert".into(),
            text: "Jellyfin DOWN".into(),
            timestamp: "2025-01-01T00:00:00Z".into(),
            formatted: "<event>Jellyfin DOWN</event>".into(),
        }),
    };
    let json = serde_json::to_string(&ev).expect("serialize");
    let val: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(val["type"], "event");
    assert_eq!(val["payload"]["source"], "uptime-kuma");
    assert_eq!(val["payload"]["severity"], "critical");

    let back: RpcEvent = serde_json::from_str(&json).expect("deserialize");
    match back {
        RpcEvent::Event { payload } => {
            assert_eq!(payload.id, "e-1");
            assert_eq!(payload.text, "Jellyfin DOWN");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn rpc_event_event_golden_json_shape() {
    let ev = RpcEvent::Event {
        payload: Box::new(synaps_cli::core::rpc_protocol::EventPayload {
            id: "x".into(),
            source: "s".into(),
            severity: "low".into(),
            content_type: "message".into(),
            text: "t".into(),
            timestamp: "ts".into(),
            formatted: "f".into(),
        }),
    };
    let json = serde_json::to_string(&ev).unwrap();
    // Must have type="event" at top level
    assert!(json.contains(r#""type":"event""#));
    assert!(json.contains(r#""payload""#));
}

// ─── Protocol version unchanged ──────────────────────────────────────────────

#[test]
fn rpc_protocol_version_still_one_after_event_frame() {
    // Additive variant — no version bump.
    assert_eq!(RPC_PROTOCOL_VERSION, 1);
}

// ─── 4. EventsConfig default ─────────────────────────────────────────────────

#[test]
fn events_config_default_auto_turn_true() {
    let cfg = EventsConfig::default();
    assert!(
        cfg.auto_turn,
        "events.auto_turn must default to true; opt-out via events.auto_turn = false"
    );
}

// ─── 5. EventsConfig parses from config key ──────────────────────────────────

#[test]
fn events_config_parse_auto_turn_true() {
    let cfg = load_config_from_str("events.auto_turn = true\n");
    assert!(
        cfg.events.auto_turn,
        "events.auto_turn should parse to true"
    );
}

#[test]
fn events_config_parse_auto_turn_false_explicit() {
    let cfg = load_config_from_str("events.auto_turn = false\n");
    assert!(!cfg.events.auto_turn);
}

// ─── 6. reactor event_payload_from_drained ───────────────────────────────────

#[test]
fn event_payload_from_drained_populates_all_fields() {
    use agent_engine::engine::reactor::{
        event_payload_from_drained, DrainedEvent, EventDisposition,
    };
    use agent_engine::events::types::{Event, Severity};

    let ev = Event::simple("discord", "ping from discord", Some(Severity::High));
    let formatted = format!("<event id=\"{}\" type=\"message\" severity=\"high\" source=\"discord\">ping from discord</event>", ev.id);
    let drained = DrainedEvent {
        event: ev.clone(),
        formatted: formatted.clone(),
        disposition: EventDisposition::Injected,
    };

    let payload = event_payload_from_drained(&drained);
    assert_eq!(payload.id, ev.id);
    assert_eq!(payload.source, "discord");
    assert_eq!(payload.severity, "high");
    assert_eq!(payload.text, "ping from discord");
    assert_eq!(payload.formatted, formatted);
    assert!(!payload.timestamp.is_empty());
}
