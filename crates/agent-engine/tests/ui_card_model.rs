//! The C1 card model against its fixture corpus (`tests/fixtures/ui/` in
//! this package; the desk's D1 copies it), and the measured worst-case
//! budget of §4.4 (R2-M3).

use std::path::{Path, PathBuf};

use agent_engine::ui::answer::{
    build_capsule, format_answer_message, parse_answer, parse_answer_message, task_excerpt,
    AnswerMessage,
};
use agent_engine::ui::doc::parse_input;
use agent_engine::ui::sanitize::refused_char;
use agent_engine::ui::*;
use serde_json::{json, Value};

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ui")
}

fn read_dir(kind: &str) -> Vec<(String, Value)> {
    let mut out: Vec<(String, Value)> = std::fs::read_dir(corpus().join(kind))
        .unwrap_or_else(|e| panic!("{kind}: {e}"))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .map(|p| {
            let name = p.file_stem().unwrap().to_string_lossy().into_owned();
            let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap())
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            (name, v)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn status(s: &str) -> AnswerStatus {
    serde_json::from_value(json!(s)).unwrap()
}

#[test]
fn every_appendix_a_example_is_in_the_corpus() {
    let valid: Vec<String> = read_dir("valid").into_iter().map(|(n, _)| n).collect();
    for a in ["a1_", "a2_", "a3_", "a4_", "a5_"] {
        assert!(valid.iter().any(|n| n.starts_with(a)), "{a}");
    }
    let invalid: Vec<String> = read_dir("invalid").into_iter().map(|(n, _)| n).collect();
    for a in [
        "a_ten_options",
        "a_duplicate_label",
        "a_default_not_an_option",
        "a_note_with_multi",
        "a_field_id_uppercase",
        "a_check_value_string",
        "a_close_with_question",
        "a_unknown_key_blocks",
        "a_question_121_chars",
        "a_question_bidi_override",
    ] {
        assert!(invalid.iter().any(|n| n == a), "{a}");
    }
}

#[test]
fn valid_fixtures() {
    let all = read_dir("valid");
    assert!(all.len() >= 9);
    for (name, f) in all {
        let parsed = parse_input(&f["input"]).unwrap_or_else(|e| panic!("{name}: {e}"));
        if let Some(close) = f["close"].as_str() {
            assert_eq!(parsed, UiInput::Close(close.into()), "{name}");
            assert_eq!(
                f["result"],
                json!({"status": "closed", "id": close}),
                "{name}"
            );
            continue;
        }
        let UiInput::Create { doc, wait } = parsed else {
            panic!("{name}: not a create")
        };
        assert_eq!(
            serde_json::to_value(&doc).unwrap(),
            f["doc"],
            "{name}: normalized doc"
        );
        let stored: CardDoc = serde_json::from_value(f["doc"].clone()).unwrap();
        assert_eq!(stored, doc, "{name}: the stored doc reads back");
        doc.revalidate().unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(json!(wait), f["wait"], "{name}");
        assert_eq!(
            serde_json::to_value(doc.mode()).unwrap(),
            f["mode"],
            "{name}"
        );
        assert_eq!(json!(contract(&doc)), f["contract"], "{name}: contract");

        for a in f["answers"].as_array().unwrap() {
            let st = status(a["status"].as_str().unwrap());
            let values = if st == AnswerStatus::Skipped {
                assert_eq!(a["values"], json!({}), "{name}");
                Answer::skip()
            } else {
                parse_answer(&doc, &a["values"]).unwrap_or_else(|e| panic!("{name}: {e}"))
            };
            assert_eq!(
                serde_json::to_value(&values).unwrap(),
                a["values"],
                "{name}"
            );
            let asked = a.get("late").map(|late| {
                let task = task_excerpt(late["turn_message"].as_str().unwrap());
                assert_eq!(json!(task), late["task"], "{name}: task excerpt");
                let asked = build_capsule(&doc, &task, &values);
                assert_eq!(json!(asked), late["asked"], "{name}: capsule");
                assert!(asked.len() <= CAPSULE_MAX_BYTES);
                asked
            });
            let m = AnswerMessage {
                card: a["card"].as_str().unwrap().into(),
                answer_id: a["answer_id"].as_str().unwrap().into(),
                q: doc.question.clone(),
                status: st,
                rev: a["rev"].as_u64().unwrap() as u32,
                values,
                asked,
            };
            let text = format_answer_message(&m);
            assert_eq!(json!(text), a["message"], "{name}: message");
            let back = parse_answer_message(&text).unwrap_or_else(|| panic!("{name}: reads back"));
            assert_eq!(format_answer_message(&back), text, "{name}: round trip");
            assert_eq!(
                AnswerMessage {
                    q: m.q.clone(),
                    ..back
                },
                m,
                "{name}: round trip"
            );
        }
        for bad in f["bad_answers"].as_array().into_iter().flatten() {
            let e = parse_answer(&doc, &bad["values"]).expect_err(&name);
            assert_eq!(json!(e.to_string()), bad["error"], "{name}");
        }
        if let Some(r) = f.get("wait_result") {
            let a = &f["answers"][0];
            assert_eq!(r["status"], "answered");
            assert_eq!(r["answer_id"], a["answer_id"]);
            assert_eq!(r["id"], a["card"]);
            assert_eq!(
                r["answer"], a["values"],
                "{name}: a waited result carries the same answer object"
            );
        }
    }
}

#[test]
fn invalid_fixtures() {
    let all = read_dir("invalid");
    assert!(all.len() >= 10);
    for (name, f) in all {
        let e = parse_input(&f["input"]).expect_err(&name);
        assert_eq!(json!(e.to_string()), f["error"], "{name}");
    }
}

#[test]
fn sanitize_fixtures() {
    let f: Value =
        serde_json::from_str(&std::fs::read_to_string(corpus().join("sanitize.json")).unwrap())
            .unwrap();
    let cases = f["cases"].as_array().unwrap();
    assert!(cases.len() >= 20);
    for c in cases {
        let s = c["in"].as_str().unwrap();
        assert_eq!(json!(ui_sanitize(s, Lines::Single)), c["single"], "{s:?}");
        assert_eq!(json!(ui_sanitize(s, Lines::Multi)), c["multi"], "{s:?}");
        let refused = refused_char(s)
            .map(|(ch, why)| json!({"char": format!("U+{:04X}", ch as u32), "why": why}));
        assert_eq!(refused.unwrap_or(Value::Null), c["refused"], "{s:?}");
    }
}

// ── The measured budget (§4.4, R2-M3) ────────────────────────────────────
//
// Every cap full at once, in the encoding that costs most. Strings that
// sanitize can't shrink take 4 bytes per character (a supplementary-plane
// scalar is one character for every bound and 4 bytes in JSON; `"` and `\`
// cost 2, controls are refused). Numbers are at their type's maximum and
// enums at their longest spelling, so the fixed-shape transitions (epoch,
// expiry, final receipts, settle, downgrade) can't grow an image: they only
// change those fields. C2 and C3 re-run this against their real types.

const WIDE: &str = "\u{10FFFD}";

fn wide(n: usize) -> String {
    WIDE.repeat(n)
}

fn hex(n: usize) -> String {
    "f".repeat(n)
}

fn bytes(v: &Value) -> usize {
    serde_json::to_vec(v).unwrap().len()
}

/// The largest doc a valid input can normalize to: the input at its 8 KiB
/// cap, with the shape that materializes the most (string options become
/// `{label}`, fields omit `type`, `value` and `optional`, `multi` omitted),
/// the rest of the budget in 4-byte characters.
fn worst_doc() -> CardDoc {
    let mut input = json!({
        "question": "q",
        "detail": "",
        "options": (0..OPTIONS_MAX).map(|i| i.to_string()).collect::<Vec<_>>(),
        "fields": (0..FIELDS_MAX).map(|i| json!({"id": format!("f{i}"), "label": "L"})).collect::<Vec<_>>(),
    });
    let mut left = INPUT_MAX_BYTES - bytes(&input);
    let mut fill = |slot: &mut Value, prefix: &str, cap: usize| {
        let room = cap - prefix.chars().count();
        let n = (left / 4).min(room);
        *slot = json!(format!("{prefix}{}", wide(n)));
        left -= 4 * n;
    };
    fill(&mut input["detail"], "", DETAIL_MAX);
    fill(&mut input["question"], "q", QUESTION_MAX);
    for i in 0..OPTIONS_MAX {
        fill(&mut input["options"][i], &i.to_string(), OPTION_LABEL_MAX);
    }
    for i in 0..FIELDS_MAX {
        fill(&mut input["fields"][i]["label"], "L", FIELD_LABEL_MAX);
    }
    assert!(left < 4, "the input budget is spent ({left} left)");
    assert!(bytes(&input) <= INPUT_MAX_BYTES);
    match parse_input(&input).unwrap() {
        UiInput::Create { doc, .. } => doc,
        UiInput::Close(_) => unreachable!(),
    }
}

/// Answer values at their 8 KiB cap.
fn worst_values() -> Value {
    let mut v = json!({"choice": "x", "fields": {"f0": ""}});
    let pad = ANSWER_MAX_BYTES - bytes(&v);
    v["fields"]["f0"] = json!("x".repeat(pad));
    assert_eq!(bytes(&v), ANSWER_MAX_BYTES);
    v
}

/// A provider's tool_use id, a `_synaps_turn` id, a session id: not bounded
/// by the design; modelled at 128 bytes.
fn opaque_id() -> String {
    "i".repeat(128)
}

fn rfc3339() -> &'static str {
    "2026-11-23T05:42:00.123456789+00:00"
}

/// §2.2 `SurfaceState`, every field at its worst.
fn worst_state(doc: &CardDoc) -> Value {
    json!({
        "id": format!("s_{}", hex(16)),
        "call_id": opaque_id(),
        "rev": u64::MAX,
        "contract": contract(doc),
        "catalog": u32::MAX,
        "mode": "waiting",
        "phase": "withdrawn",
        "offers": ["answer", "skip", "revise", "forget"],
        "doc": doc,
        "origin": {"turn": opaque_id(), "task": wide(TASK_EXCERPT_MAX), "stopped": "driver_revoke_long"},
        "epoch": u32::MAX,
        "resume": false,
        "actions": (0..ACTION_LOG_MAX).map(|_| hex(32)).collect::<Vec<_>>(),
        "raised_at": rfc3339(),
        "downgraded": "abandoned",
        "answer": {
            "rev": u32::MAX,
            "action_id": hex(32),
            "answer_id": format!("a_{}", hex(16)),
            "status": "answered",
            "values": worst_values(),
            "delivery": "in_context",
            "wake": false,
            // A roster label: a ≤32-character client name, `-N`, ` (#id)`.
            "by": "n".repeat(96),
            "at": rfc3339(),
        },
    })
}

/// §3.7: one action log entry. `rev` is an answer revision, at most 4.
fn log_entry() -> Value {
    json!({"action_id": hex(32), "hash": hex(64), "verb": "revise", "rev": 4, "epoch": u32::MAX, "outcome": "accepted"})
}

fn log() -> Vec<Value> {
    (0..ACTION_LOG_MAX).map(|_| log_entry()).collect()
}

/// A tombstone: what a retry still needs (§3.7): the id, when it was made
/// (for the 7-day limit) and the full action log.
fn tombstone() -> Value {
    json!({"id": format!("s_{}", hex(16)), "at": rfc3339(), "log": log()})
}

/// The worst encoded capsule: ≤1 KiB, with as many characters as JSON
/// escapes again (`\u003c` → `\\u003c`, `"` → `\"`) as fit.
fn worst_asked() -> String {
    let mut s = String::from("<asked during=\"\">");
    while s.len() + 6 + "</asked>".len() <= CAPSULE_MAX_BYTES {
        s.push_str("\\u003c");
    }
    s.push_str("</asked>");
    assert!(s.len() <= CAPSULE_MAX_BYTES);
    s
}

#[test]
fn worst_case_budget_is_measured() {
    let doc = worst_doc();
    let doc_bytes = bytes(&serde_json::to_value(&doc).unwrap());

    let state = worst_state(&doc);
    let state_bytes = bytes(&state);
    let attached = Value::Array(vec![state.clone(); ATTACHED_MAX_STATES]);
    let attached_bytes = bytes(&attached);
    let tomb_bytes = bytes(&tombstone());

    let per_record = LEDGER_MAX_VALUE_BYTES / LEDGER_MAX_RECORDS;
    let record_values = {
        let mut v = json!({"fields": {"f0": ""}});
        v["fields"]["f0"] = json!("x".repeat(per_record - bytes(&v)));
        assert_eq!(bytes(&v), per_record);
        v
    };
    let record = json!({
        "answer_id": format!("a_{}", hex(16)), "card": format!("s_{}", hex(16)), "rev": u32::MAX,
        "status": "answered", "values": record_values, "wake": false, "epoch": u32::MAX,
        "delivery": "in_context", "q": wide(QUESTION_MAX), "asked": worst_asked(),
    });
    let binding = json!({"session_id": opaque_id(), "base": hex(64), "seq": u64::MAX});
    let sidecar = json!({
        "v": 1,
        "session_id": opaque_id(),
        "lineage": hex(32),
        "base": hex(64),
        "seq": u64::MAX,
        "epoch": u32::MAX,
        "checkpointed": true,
        "compacting": {"hx": hex(32), "from": binding, "to": binding},
        "prepared": {"hx": hex(32), "parent": opaque_id(), "from_seq": u64::MAX},
        "moved_to": {"to": opaque_id(), "hx": hex(32)},
        "cards": (0..ATTACHED_MAX_STATES).map(|_| json!({"state": state, "log": log()})).collect::<Vec<_>>(),
        "ledger": vec![record; LEDGER_MAX_RECORDS],
        "tombstones": vec![tombstone(); TOMBSTONES_MAX],
        "newest_settled": format!("a_{}", hex(16)),
    });
    let sidecar_bytes = bytes(&sidecar);

    eprintln!(
        "ui budget (measured): doc {doc_bytes} B; SurfaceState {state_bytes} B (cap {SURFACE_STATE_MAX_BYTES}); \
         Attached.surfaces {attached_bytes} B (cap {ATTACHED_MAX_BYTES}); tombstone {tomb_bytes} B (cap {TOMBSTONE_MAX_BYTES}); \
         sidecar {sidecar_bytes} B (cap {SIDECAR_MAX_BYTES})"
    );
    assert!(
        doc_bytes > INPUT_MAX_BYTES,
        "the doc is the worst case: materializing grew it ({doc_bytes} B)"
    );
    assert!(
        state_bytes <= SURFACE_STATE_MAX_BYTES,
        "SurfaceState {state_bytes} B"
    );
    assert!(
        attached_bytes <= ATTACHED_MAX_BYTES,
        "Attached.surfaces {attached_bytes} B"
    );
    assert!(
        tomb_bytes <= TOMBSTONE_MAX_BYTES,
        "tombstone {tomb_bytes} B"
    );
    assert!(
        sidecar_bytes <= SIDECAR_MAX_BYTES,
        "sidecar {sidecar_bytes} B"
    );
}

#[test]
fn the_tool_input_schema_is_the_shipped_file() {
    let s = input_schema();
    assert_eq!(s["type"], "object");
    assert_eq!(
        serde_json::to_string(&s).unwrap(),
        serde_json::to_string(&serde_json::from_str::<Value>(SCHEMA_JSON).unwrap()).unwrap()
    );
}
