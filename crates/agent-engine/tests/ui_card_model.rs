//! The C1 card model against its fixture corpus (`tests/fixtures/ui/` in
//! this package; the desk's D1 copies it), and the measured worst-case
//! budget of §4.4 (R2-M3).

use std::path::{Path, PathBuf};

use agent_engine::ui::answer::{
    build_capsule, format_answer_message, parse_answer, parse_answer_message, task_excerpt,
    AnswerMessage,
};
use agent_engine::ui::sanitize::{fold_key, refused_char};
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
        revalidate(&doc).unwrap_or_else(|e| panic!("{name}: {e}"));
        revalidate(&stored).unwrap_or_else(|e| panic!("{name}: stored: {e}"));
        if let Some(n) = f.get("input_bytes") {
            assert_eq!(json!(bytes(&f["input"])), *n, "{name}: input bytes");
            assert_eq!(json!(bytes(&f["doc"])), f["doc_bytes"], "{name}: doc bytes");
            assert!(bytes(&f["doc"]) <= DOC_MAX_BYTES, "{name}");
        }
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
            // `q` reads back as its attribute projection; everything else as built.
            assert_eq!(json!(back.q), a["q"], "{name}: q as an attribute");
            assert_eq!(
                back,
                AnswerMessage {
                    q: back.q.clone(),
                    ..m
                },
                "{name}: round trip"
            );
        }
        for bad in f["bad_answers"].as_array().into_iter().flatten() {
            let e = parse_answer(&doc, &bad["values"])
                .expect_err(&name)
                .to_string();
            assert_eq!(json!(e), bad["error"], "{name}");
            assert!(one_safe_line(&e), "{name}: {e:?}");
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
        let e = parse_input(&f["input"]).expect_err(&name).to_string();
        assert_eq!(json!(e), f["error"], "{name}");
        assert!(one_safe_line(&e), "{name}: {e:?}");
    }
}

/// A refusal is one line, at most 322 characters, with nothing that can
/// hide or reorder text (Shady #439 M1).
fn one_safe_line(e: &str) -> bool {
    e.chars().count() <= ERROR_PATH_MAX + 2 + ERROR_MESSAGE_MAX
        && ui_sanitize(e, Lines::Single) == e
        && refused_char(e).is_none()
}

/// The raw-JSON lane (Shady #439 L2): `decode_json`, then the model.
#[test]
fn raw_fixtures() {
    let all = read_dir("raw");
    assert!(all.len() >= 10);
    for (name, f) in all {
        let decoded = decode_json(f["text"].as_str().unwrap());
        let v = match (decoded, f["json_error"].as_str()) {
            (Err(e), Some(prefix)) => {
                assert!(e.to_string().starts_with(prefix), "{name}: {e}");
                assert!(one_safe_line(&e.to_string()), "{name}: {e:?}");
                continue;
            }
            (Ok(v), None) => v,
            (other, _) => panic!("{name}: {other:?}"),
        };
        match f["what"].as_str().unwrap() {
            "input" => match parse_input(&v) {
                Ok(UiInput::Create { doc, .. }) => {
                    assert_eq!(serde_json::to_value(&doc).unwrap(), f["doc"], "{name}")
                }
                Ok(other) => panic!("{name}: {other:?}"),
                Err(e) => assert_eq!(json!(e.to_string()), f["error"], "{name}"),
            },
            "answer" => {
                let Ok(UiInput::Create { doc, .. }) = parse_input(&f["card"]) else {
                    panic!("{name}: card")
                };
                match parse_answer(&doc, &v) {
                    Ok(a) => assert_eq!(serde_json::to_value(&a).unwrap(), f["values"], "{name}"),
                    Err(e) => assert_eq!(json!(e.to_string()), f["error"], "{name}"),
                }
            }
            other => panic!("{name}: {other}"),
        }
    }
}

/// The label comparison key is Unicode full case folding (Shady #439 M3),
/// pinned for every scalar value against Python's `str.casefold`.
#[test]
fn casefold_fixture() {
    let f: Value =
        serde_json::from_str(&std::fs::read_to_string(corpus().join("casefold.json")).unwrap())
            .unwrap();
    let table = f["differs_from_lowercase"].as_object().unwrap();
    assert!(table.len() > 200);
    let (major, minor, update) = char::UNICODE_VERSION;
    let mut wrong = Vec::new();
    for c in (0..=0x10FFFFu32).filter_map(char::from_u32) {
        let expected = match table.get(&format!("U+{:04X}", c as u32)) {
            Some(v) => v.as_str().unwrap().to_string(),
            None => c.to_lowercase().collect(),
        };
        if fold_key(&c.to_string()) != expected {
            wrong.push(format!("U+{:04X}", c as u32));
        }
    }
    assert!(
        wrong.is_empty(),
        "fold_key differs from casefold.json (Unicode {}; this toolchain {major}.{minor}.{update}) at {wrong:?}",
        f["unicode"]
    );
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
// scalar is one character for every bound and 4 bytes in JSON), or 2 per
// byte where the bound is in bytes and `\` or `"` may appear (JSON writes
// them escaped). Numbers are at their type's maximum and enums at their
// longest spelling, so the fixed-shape transitions (epoch, expiry, final
// receipts, settle, downgrade) can't grow an image: they only change those
// fields. Identifiers are at their v3.2 #11 bounds. C2 and C3 re-run this
// against their real types.
//
// A part bounded in bytes (the doc, a stored capsule) is a filler at its
// allowance, not an example: an example only shows the bound is nearly
// reached (Shady #439 round 2). The examples are still built and checked
// against their allowance.

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

/// A JSON string of exactly `n` encoded bytes: a part at its allowance.
fn filler(n: usize) -> Value {
    let v = json!("x".repeat(n - 2));
    assert_eq!(bytes(&v), n);
    v
}

/// A doc near its bound: the max-size fixture, whose default is spelled in
/// KELVIN SIGN and given in ASCII. Its 9,554 bytes are an example; the
/// budget allows `DOC_MAX_BYTES`, the bound `revalidate` enforces.
fn doc_example() -> CardDoc {
    let f: Value = serde_json::from_str(
        &std::fs::read_to_string(corpus().join("valid/max_size_kelvin_default.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(bytes(&f["input"]), INPUT_MAX_BYTES);
    match parse_input(&f["input"]).unwrap() {
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

/// A `ui` call id at its bound, every byte one JSON escapes.
fn call_id() -> String {
    let id = "\\".repeat(CALL_ID_MAX_BYTES);
    assert!(is_call_id(&id));
    id
}

fn session_id() -> String {
    let id = "s".repeat(SESSION_ID_MAX_BYTES);
    assert!(is_session_id(&id));
    id
}

fn turn_id() -> String {
    let id = "ffffffff-ffff-ffff-ffff-ffffffffffff".to_string();
    assert!(is_turn_id(&id));
    id
}

/// A roster label at its bound, every byte one JSON escapes.
fn by() -> String {
    let by = roster_label(&"\\".repeat(500));
    assert!(by.len() <= BY_MAX_BYTES);
    by
}

fn rfc3339() -> &'static str {
    "2026-11-23T05:42:00.123456789+00:00"
}

/// The longest `StopKind`.
fn stop_kind() -> Value {
    [
        StopKind::User,
        StopKind::System,
        StopKind::Reload,
        StopKind::Crash,
    ]
    .iter()
    .map(|k| serde_json::to_value(k).unwrap())
    .max_by_key(bytes)
    .unwrap()
}

/// §2.2 `SurfaceState`, every field at its worst. `doc` gives the contract
/// (fixed length); the doc itself is `DOC_MAX_BYTES` of filler.
fn worst_state(doc: &CardDoc) -> Value {
    json!({
        "id": format!("s_{}", hex(16)),
        "call_id": call_id(),
        "rev": u64::MAX,
        "contract": contract(doc),
        "catalog": u32::MAX,
        "mode": "waiting",
        "phase": "withdrawn",
        "offers": ["answer", "skip", "revise", "forget"],
        "doc": filler(DOC_MAX_BYTES),
        "origin": {"turn": turn_id(), "task": wide(TASK_EXCERPT_MAX), "stopped": stop_kind()},
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
            "by": by(),
            "at": rfc3339(),
        },
    })
}

/// §3.7 (v3.2 #10): one action log entry, `{action_id, hash, rev}`. `hash`
/// is hex128 like the contract; `rev` is the answer revision it produced
/// (≤4).
fn log_entry() -> Value {
    json!({"action_id": hex(32), "hash": hex(32), "rev": 4})
}

fn log() -> Vec<Value> {
    (0..ACTION_LOG_MAX).map(|_| log_entry()).collect()
}

/// A tombstone (v3.2 #10): `{id, at, log}`, what a retry still needs.
fn tombstone() -> Value {
    json!({"id": format!("s_{}", hex(16)), "at": rfc3339(), "log": log()})
}

/// What a stored capsule may cost: any `Capsule` at its 1 KiB cap, every
/// byte at serde_json's widest escape (`\u00XX`, 6 bytes), plus the quotes.
/// `Capsule::parse` refuses `<` but not C0 controls, so a capsule read back
/// from disk can hold them; `build_capsule` never writes one.
const CAPSULE_STORED_ALLOWANCE: usize = 6 * CAPSULE_MAX_BYTES + 2;

/// The costliest capsule `build_capsule` writes that we know of (Shady #439
/// round 2): every part `\`, which JSON doubles, and a chosen label of `b`
/// and 43 of them, so the line reaches 1 KiB with nothing omitted. An
/// example; the budget uses [`CAPSULE_STORED_ALLOWANCE`].
fn capsule_example() -> Capsule {
    let b = "\\";
    let label = format!("b{}", b.repeat(43));
    let fields: Vec<Value> = (0..FIELDS_MAX)
        .map(|i| json!({"id": format!("f{i}"), "label": b.repeat(FIELD_LABEL_MAX)}))
        .collect();
    let d = match parse_input(
        &json!({"question": "q", "detail": b.repeat(CAPSULE_DETAIL_MAX),
        "options": ["a", {"label": label, "hint": b.repeat(HINT_MAX)}], "fields": fields}),
    )
    .unwrap()
    {
        UiInput::Create { doc, .. } => doc,
        UiInput::Close(_) => unreachable!(),
    };
    let values = json!({"choice": label, "fields": (0..FIELDS_MAX).map(|i| (format!("f{i}"), json!("v"))).collect::<serde_json::Map<String, Value>>()});
    let a = parse_answer(&d, &values).unwrap();
    // The longest task that keeps every part, every byte of it `\`.
    (0..=TASK_EXCERPT_MAX)
        .rev()
        .map(|n| build_capsule(&d, &b.repeat(n), &a))
        .find(|c| !c.contains("omitted"))
        .unwrap()
}

#[test]
fn worst_case_budget_is_measured() {
    let doc = doc_example();
    let doc_bytes = bytes(&serde_json::to_value(&doc).unwrap());
    revalidate(&doc).unwrap();
    let asked = capsule_example();
    let asked_bytes = bytes(&json!(asked));
    assert_eq!(
        asked.len(),
        CAPSULE_MAX_BYTES,
        "the example reaches the cap"
    );
    assert!(Capsule::parse("<asked during=\"\">\u{1}</asked>").is_some());
    assert_eq!(
        bytes(&json!("\u{1}")),
        6 + 2,
        "a C0 control stores as \\u00XX"
    );

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
        "delivery": "in_context", "q": wide(QUESTION_MAX),
        "asked": filler(CAPSULE_STORED_ALLOWANCE),
    });
    let binding = json!({"session_id": session_id(), "base": hex(64), "seq": u64::MAX});
    let sidecar = json!({
        "v": 1,
        "session_id": session_id(),
        "lineage": hex(32),
        "base": hex(64),
        "seq": u64::MAX,
        "epoch": u32::MAX,
        "checkpointed": true,
        "compacting": {"hx": hex(32), "from": binding, "to": binding},
        "prepared": {"hx": hex(32), "parent": session_id(), "from_seq": u64::MAX},
        "moved_to": {"to": session_id(), "hx": hex(32)},
        "cards": (0..ATTACHED_MAX_STATES).map(|_| json!({"state": state, "log": log()})).collect::<Vec<_>>(),
        "ledger": vec![record; LEDGER_MAX_RECORDS],
        "tombstones": vec![tombstone(); TOMBSTONES_MAX],
        "newest_settled": format!("a_{}", hex(16)),
    });
    let sidecar_bytes = bytes(&sidecar);

    eprintln!(
        "ui budget (measured): doc allowed {DOC_MAX_BYTES} B (example {doc_bytes} B); \
         capsule stored allowed {CAPSULE_STORED_ALLOWANCE} B (example {} B, stored {asked_bytes} B); \
         SurfaceState {state_bytes} B (cap {SURFACE_STATE_MAX_BYTES}); \
         Attached.surfaces {attached_bytes} B (cap {ATTACHED_MAX_BYTES}); tombstone {tomb_bytes} B (cap {TOMBSTONE_MAX_BYTES}); \
         sidecar {sidecar_bytes} B (cap {SIDECAR_MAX_BYTES})",
        asked.len()
    );
    assert!(
        doc_bytes > INPUT_MAX_BYTES && doc_bytes <= DOC_MAX_BYTES,
        "materializing grew the example past the input cap ({doc_bytes} B)"
    );
    assert!(
        asked_bytes <= CAPSULE_STORED_ALLOWANCE,
        "capsule stored {asked_bytes} B"
    );
    assert_eq!(tomb_bytes, 573, "v3.2 #10: the slim tombstone at its worst");
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
