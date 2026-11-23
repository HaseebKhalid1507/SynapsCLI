//! Agent cards on the wire (agent-ui C3: §2.1, §2.3–§2.5, §2.8; v3.3's
//! lenient elements). Inert: no daemon sends these frames before C4a.

use serde_json::json;

use super::*;
use crate::ui::{SurfaceState, Verb};

fn env(event: SessionEventWire) -> Envelope {
    Envelope { session_id: SessionId::from("wire-1"), seq: 9, ts: chrono::Utc::now(), event }
}

fn meta() -> SessionMeta {
    crate::session::handle::echo::meta_for(&SessionId::from("wire-1"))
}

fn snapshot(surfaces: Vec<SurfaceState>) -> AttachSnapshot {
    AttachSnapshot {
        meta: meta(),
        view: crate::session::handle::echo::view_for(),
        conversation: Default::default(),
        streaming: false,
        replay: vec![],
        pending_prompts: vec![],
        clients: vec![],
        roster: Vec::new(),
        client_input: ClientInput::wire_default(),
        input_owner: None,
        display_tail: None,
        surfaces,
    }
}

/// An event through the socket and back, as a client reads it.
fn through_the_wire(event: SessionEventWire) -> (serde_json::Value, SessionEventWire) {
    let line = encode_line(&DaemonFrame::Event(env(event).into())).unwrap();
    let v: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
    match decode_line::<DaemonFrame>(line.trim_end()).unwrap() {
        DaemonFrame::Event(w) => (v["event"].clone(), Envelope::from(w).event),
        other => panic!("{other:?}"),
    }
}

fn every_outcome() -> Vec<SurfaceOutcome> {
    use SurfaceOutcome as O;
    let all = vec![O::Accepted, O::Pending, O::Duplicate, O::Refused];
    // A new outcome must join the list (and the round trip below).
    for o in &all {
        match o {
            O::Accepted | O::Pending | O::Duplicate | O::Refused => {}
            O::Unknown => unreachable!(),
        }
    }
    all
}

fn every_refusal() -> Vec<SurfaceRefusal> {
    use SurfaceRefusal as R;
    let all = vec![
        R::RateLimited,
        R::Forbidden,
        R::Reloading,
        R::Invalid,
        R::Busy,
        R::Storage,
        R::Resolved,
        R::Gone,
        R::Stale,
        R::Stopped,
        R::Conflict,
        R::Unavailable,
    ];
    for r in &all {
        match r {
            R::RateLimited
            | R::Forbidden
            | R::Reloading
            | R::Invalid
            | R::Busy
            | R::Storage
            | R::Resolved
            | R::Gone
            | R::Stale
            | R::Stopped
            | R::Conflict
            | R::Unavailable => {}
            R::Unknown => unreachable!(),
        }
    }
    all
}

/// `{"ev":"surface","surface":SurfaceState}` (§2.3): the card crosses
/// whole and comes back equal.
#[test]
fn a_surface_event_round_trips() {
    let card = test_card(1);
    let (v, back) = through_the_wire(SessionEventWire::Surface(Box::new(card.clone())));
    assert_eq!(v, json!({"ev": "surface", "surface": serde_json::to_value(&card).unwrap()}));
    match back {
        SessionEventWire::Surface(c) => assert_eq!(*c, card),
        other => panic!("{other:?}"),
    }
}

/// `surface_result` (§2.3, §2.8) in every outcome and every refusal
/// reason, with and without its optional keys, in §2.8's shape.
#[test]
fn every_surface_result_round_trips() {
    let refused = SurfaceResult {
        client: ClientId(3),
        action_id: "5b0c".repeat(8),
        outcome: SurfaceOutcome::Refused,
        reason: Some(SurfaceRefusal::RateLimited),
        retry: Some(true),
        answer_rev: None,
    };
    let (v, _) = through_the_wire(SessionEventWire::SurfaceResult(refused.clone()));
    assert_eq!(
        v,
        json!({"ev": "surface_result", "client": 3, "action_id": "5b0c".repeat(8),
               "outcome": "refused", "reason": "rate_limited", "retry": true})
    );
    let mut cases = vec![SurfaceResult {
        outcome: SurfaceOutcome::Accepted,
        reason: None,
        retry: None,
        answer_rev: Some(2),
        ..refused.clone()
    }];
    for outcome in every_outcome() {
        cases.push(SurfaceResult { outcome, reason: None, retry: None, answer_rev: None, ..refused.clone() });
    }
    for reason in every_refusal() {
        cases.push(SurfaceResult { reason: Some(reason), retry: Some(false), ..refused.clone() });
    }
    for r in cases {
        match through_the_wire(SessionEventWire::SurfaceResult(r.clone())) {
            (v, SessionEventWire::SurfaceResult(back)) => {
                assert_eq!(back, r, "{v}");
                for (key, present) in
                    [("reason", r.reason.is_some()), ("retry", r.retry.is_some()), ("answer_rev", r.answer_rev.is_some())]
                {
                    assert_eq!(v.get(key).is_some(), present, "{key}: {v}");
                }
            }
            (_, other) => panic!("{other:?}"),
        }
    }
}

/// §2.4's command, as its example spells it, through a client frame:
/// every verb, with and without `values`; `Debug` never shows the values,
/// nor their length.
#[test]
fn a_surface_action_round_trips_and_debug_redacts_its_values() {
    let line = r#"{"type":"cmd","session_id":"s1","cmd":{"cmd":"surface_action","surface_id":"s_3f2a91c0d4e2a871","contract":"c1:9d00000000000000000000000000000f","action_id":"5b0c5b0c5b0c5b0c5b0c5b0c5b0c5b0c","expect_rev":0,"epoch":4,"verb":"answer","values":{"choice":"Refactor the parser"}}}"#;
    let frame: ClientFrame = decode_line(line).unwrap();
    match &frame {
        ClientFrame::Cmd {
            cmd: SessionCommand::SurfaceAction { surface_id, contract, action_id, expect_rev, epoch, verb, values },
            ..
        } => {
            assert_eq!(surface_id, "s_3f2a91c0d4e2a871");
            assert_eq!(contract, "c1:9d00000000000000000000000000000f");
            assert_eq!(action_id, &"5b0c".repeat(8));
            assert_eq!((*expect_rev, *epoch, *verb), (0, 4, Verb::Answer));
            assert_eq!(values.as_ref(), Some(&json!({"choice": "Refactor the parser"})));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        serde_json::to_value(&frame).unwrap(),
        serde_json::from_str::<serde_json::Value>(line).unwrap(),
        "re-encodes to the same frame"
    );
    let dbg = format!("{frame:?}");
    assert!(dbg.contains("SurfaceAction") && dbg.contains("<redacted>"), "{dbg}");
    assert!(!dbg.contains("Refactor") && !dbg.contains("choice"), "{dbg}");

    for verb in Verb::ALL {
        for values in [None, Some(json!({}))] {
            let cmd = SessionCommand::SurfaceAction {
                surface_id: "s_3f2a91c0d4e2a871".into(),
                contract: "c1:9d00000000000000000000000000000f".into(),
                action_id: "5b0c".repeat(8),
                expect_rev: 1,
                epoch: 4,
                verb,
                values: values.clone(),
            };
            let v = serde_json::to_value(&cmd).unwrap();
            assert_eq!(v.get("values").is_some(), values.is_some(), "{v}");
            let back: SessionCommand = serde_json::from_value(v.clone()).unwrap();
            assert_eq!(serde_json::to_value(&back).unwrap(), v);
        }
    }
}

/// The additive fields (§2.1): `capabilities.surfaces` and the two
/// presence counts are omitted at 0 and read as 0 when absent (an older
/// client's or daemon's), and round-trip when set. This build declares 0:
/// it renders no cards (the TUI's are C5).
#[test]
fn the_capability_and_the_presence_counts_are_additive() {
    let this = ClientCapabilities::current();
    assert_eq!(this.surfaces, 0);
    assert!(serde_json::to_value(this).unwrap().get("surfaces").is_none());
    let old: ClientCapabilities = serde_json::from_str(r#"{"tool_queue":true,"owner_gone":true}"#).unwrap();
    assert_eq!(old.surfaces, 0);
    let capable = ClientCapabilities { surfaces: 1, ..this };
    let v = serde_json::to_value(capable).unwrap();
    assert_eq!(v["surfaces"], 1);
    assert_eq!(serde_json::from_value::<ClientCapabilities>(v).unwrap(), capable);

    let v = serde_json::to_value(meta()).unwrap();
    assert!(v.get("surfaces_waiting").is_none() && v.get("surfaces_open").is_none(), "{v}");
    let m = SessionMeta { surfaces_waiting: 1, surfaces_open: 2, ..meta() };
    let v = serde_json::to_value(&m).unwrap();
    assert_eq!((v["surfaces_waiting"].as_u64(), v["surfaces_open"].as_u64()), (Some(1), Some(2)));
    assert_eq!(serde_json::from_value::<SessionMeta>(v).unwrap(), m);
}

/// `Attached.surfaces` (§2.5) crosses whole, and is omitted when empty.
#[test]
fn attached_surfaces_round_trip() {
    let card = test_card(1);
    let frame = |s| encode_line(&DaemonFrame::Attached(AttachedWire::new(ClientId(1), s))).unwrap();
    let line = frame(snapshot(vec![card.clone()]));
    match decode_line::<DaemonFrame>(line.trim_end()).unwrap() {
        DaemonFrame::Attached(a) => assert_eq!(a.into_snapshot().1.surfaces, vec![card]),
        other => panic!("{other:?}"),
    }
    let empty: serde_json::Value = serde_json::from_str(frame(snapshot(vec![])).trim_end()).unwrap();
    assert!(empty.get("surfaces").is_none(), "{empty}");
}

/// v3.3's lenient elements, for cards: a card a newer daemon shaped
/// differently (an unknown phase, an extra field) never fails a frame. In
/// `Attached` it is dropped and the rest stay; as an event it becomes the
/// notice an unknown event gets; a `surface_result` with an outcome or
/// reason this build doesn't know reads them as `Unknown`. A failed frame
/// reads as a dead socket, and `Attached` is what every reconnect needs.
#[test]
fn a_card_this_build_cannot_read_never_fails_the_frame() {
    let card = test_card(1);
    let mut phase = serde_json::to_value(&card).unwrap();
    phase["phase"] = json!("from_the_future");
    let mut field = serde_json::to_value(&card).unwrap();
    field["from_the_future"] = json!(1);
    let mut verb = serde_json::to_value(&card).unwrap();
    verb["offers"] = json!(["revise", "forget"]);

    for bad in [&phase, &field, &verb] {
        let line = json!({"type": "event", "session_id": "wire-1", "seq": 9, "ts": chrono::Utc::now(),
                          "event": {"ev": "surface", "surface": bad}})
        .to_string();
        match decode_line::<DaemonFrame>(&line).unwrap() {
            DaemonFrame::Event(w) => match Envelope::from(w).event {
                SessionEventWire::SystemNotice(t) => assert!(t.contains("newer daemon"), "{t}"),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    let line = encode_line(&DaemonFrame::Attached(AttachedWire::new(ClientId(1), snapshot(vec![card.clone()])))).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
    v["surfaces"].as_array_mut().unwrap().extend([phase, field, verb, json!(7)]);
    match decode_line::<DaemonFrame>(&v.to_string()).unwrap() {
        DaemonFrame::Attached(a) => assert_eq!(a.into_snapshot().1.surfaces, vec![card.clone()]),
        other => panic!("{other:?}"),
    }
    v["surfaces"] = json!({"not": "a list"});
    match decode_line::<DaemonFrame>(&v.to_string()).unwrap() {
        DaemonFrame::Attached(a) => assert!(a.into_snapshot().1.surfaces.is_empty()),
        other => panic!("{other:?}"),
    }

    let newer = json!({"ev": "surface_result", "client": 1, "action_id": "x", "outcome": "deferred",
                       "reason": "cosmic_rays", "from_the_future": true});
    match SessionEventWire::from(serde_json::from_value::<WireSessionEvent>(newer).unwrap()) {
        SessionEventWire::SurfaceResult(r) => {
            assert_eq!((r.outcome, r.reason), (SurfaceOutcome::Unknown, Some(SurfaceRefusal::Unknown)));
        }
        other => panic!("{other:?}"),
    }
}

/// What a reader from before C3 does with the new top-level events, were
/// one ever sent to it (`client_decodes` sends none): its
/// `WireSessionEvent` lacks the tags, and `#[serde(other)]` reads them as
/// `Unknown`. Simulated the way the driver-event test does, by renaming
/// the tag to one this build lacks. The real check against `main`'s reader
/// (every frame below, decoded by that build) is in the PR.
#[test]
fn an_older_reader_reads_the_new_events_as_unknown() {
    let card = test_card(1);
    let result = SurfaceResult {
        client: ClientId(3),
        action_id: "5b0c".repeat(8),
        outcome: SurfaceOutcome::Accepted,
        reason: None,
        retry: None,
        answer_rev: Some(1),
    };
    for (tag, event) in [
        ("surface", SessionEventWire::Surface(Box::new(card.clone()))),
        ("surface_result", SessionEventWire::SurfaceResult(result.clone())),
    ] {
        let line = encode_line(&DaemonFrame::Event(env(event).into())).unwrap();
        let as_old = line.replace(&format!(r#""ev":"{tag}""#), r#""ev":"zz_before_c3""#);
        match decode_line::<DaemonFrame>(as_old.trim_end()).unwrap() {
            DaemonFrame::Event(w) => assert!(matches!(w.event, WireSessionEvent::Unknown), "{tag}"),
            other => panic!("{other:?}"),
        }
        // For the check against `main`: `cargo test ... -- --nocapture`.
        eprintln!("C3FRAME {}", line.trim_end());
    }
    let m = SessionMeta { surfaces_waiting: 1, surfaces_open: 2, ..meta() };
    let mut s = snapshot(vec![card]);
    s.meta = m.clone();
    for frame in [
        DaemonFrame::Attached(AttachedWire::new(ClientId(1), s)),
        DaemonFrame::SessionList { sessions: vec![m] },
        DaemonFrame::Event(
            env(SessionEventWire::Refused {
                client: ClientId(1),
                command: "surface_action".into(),
                reason: SURFACES_UNAVAILABLE.into(),
            })
            .into(),
        ),
    ] {
        eprintln!("C3FRAME {}", encode_line(&frame).unwrap().trim_end());
    }
}
