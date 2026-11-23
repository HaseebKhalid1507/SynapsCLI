//! The contract digest: a card's semantic identity (§5.4).
//!
//! `contract = "c1:" + hex128(sha256(canonical_json({catalog:1, normalizer:1,
//! question, detail, options, multi, default, fields})))` over the
//! **normalized** doc. It covers every byte the user reads and every
//! constraint on the answer. The stored doc is its preimage, so a newer
//! daemon keeps it stable: it is never recomputed with a different
//! normalizer, only compared.
//!
//! The preimage is built here field by field, not from `CardDoc`'s serde
//! form, so a change to how a doc is stored can't move a contract. Every
//! key is present; an absent optional is `null`. Canonical JSON: object keys
//! sorted (all are ASCII, so byte order is code-unit order), no whitespace,
//! strings escaped as `serde_json` escapes them.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::answer::FieldValue;
use super::doc::{CardDoc, DefaultChoice};
use super::{CATALOG, NORMALIZER};

/// `"c1:"` + 32 lowercase hex.
pub fn contract(doc: &CardDoc) -> String {
    let digest = Sha256::digest(canonical_json(&preimage(doc)).as_bytes());
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("c1:{hex}")
}

/// Whether `s` has the shape of a contract.
pub fn is_contract(s: &str) -> bool {
    s.strip_prefix("c1:")
        .is_some_and(|h| h.len() == 32 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// The digest's preimage.
pub fn preimage(doc: &CardDoc) -> Value {
    let options: Vec<Value> = doc
        .options
        .iter()
        .map(|o| json!({"label": o.label, "hint": o.hint, "note": o.note}))
        .collect();
    let default = match &doc.default {
        None => Value::Null,
        Some(DefaultChoice::One(l)) => json!(l),
        Some(DefaultChoice::Many(ls)) => json!(ls),
    };
    let fields: Vec<Value> = doc
        .fields
        .iter()
        .map(|f| {
            let value = match &f.value {
                FieldValue::Text(t) => json!(t),
                FieldValue::Check(b) => json!(b),
            };
            json!({"id": f.id, "label": f.label, "type": f.kind.as_str(), "value": value,
                   "placeholder": f.placeholder, "optional": f.optional})
        })
        .collect();
    json!({
        "catalog": CATALOG,
        "normalizer": NORMALIZER,
        "question": doc.question,
        "detail": doc.detail,
        "options": options,
        "multi": doc.multi,
        "default": default,
        "fields": fields,
    })
}

/// Compact JSON with object keys sorted, independent of whether
/// `serde_json` was built with `preserve_order`.
pub fn canonical_json(v: &Value) -> String {
    let mut out = String::new();
    write_canonical(v, &mut out);
    out
}

fn write_canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                write_canonical(&o[k], out);
            }
            out.push('}');
        }
        Value::Array(a) => {
            out.push('[');
            for (i, v) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(v, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::doc::{parse_input, UiInput};
    use super::*;

    fn doc(v: Value) -> CardDoc {
        match parse_input(&v).unwrap() {
            UiInput::Create { doc, .. } => doc,
            UiInput::Close(_) => unreachable!(),
        }
    }

    #[test]
    fn canonical_sorts_keys_without_whitespace() {
        let v = json!({"b": [1, {"z": null, "a": "x\n\"<"}], "a": true});
        assert_eq!(
            canonical_json(&v),
            r#"{"a":true,"b":[1,{"a":"x\n\"<","z":null}]}"#
        );
    }

    #[test]
    fn preimage_has_every_key() {
        let d = doc(json!({"question": "q", "fields": [{"id": "a", "label": "L"}]}));
        assert_eq!(
            canonical_json(&preimage(&d)),
            r#"{"catalog":1,"default":null,"detail":null,"fields":[{"id":"a","label":"L","optional":false,"placeholder":null,"type":"text","value":""}],"multi":false,"normalizer":1,"options":[],"question":"q"}"#
        );
    }

    #[test]
    fn shape_and_stability() {
        let a = doc(
            json!({"question": "Which approach?", "options": ["Refactor", "Rewrite"], "default": "Refactor"}),
        );
        let c = contract(&a);
        assert!(is_contract(&c), "{c}");
        // Inputs that normalize to the same doc share a contract.
        let b = doc(
            json!({"question": "Which\u{200B} approach?", "options": [{"label": "Refactor"}, "Rewrite"],
            "default": "Refactor", "multi": false, "detail": ""}),
        );
        assert_eq!(contract(&b), c);
        assert!(!is_contract("c1:XYZ") && !is_contract(&c[..34]));
    }

    /// Every byte the user reads and every constraint on the answer moves
    /// the contract (§5.4 coverage).
    #[test]
    fn covers_everything_shown_or_constraining() {
        let base = json!({"question": "q", "detail": "d", "options": ["a", {"label": "b", "hint": "h", "note": "n"}],
            "default": "a", "fields": [{"id": "f", "label": "L", "value": "v", "placeholder": "p", "optional": true},
                                       {"id": "g", "label": "G", "type": "check", "value": true}]});
        let c0 = contract(&doc(base.clone()));
        type Edit = (&'static str, fn(&mut Value));
        let edits: &[Edit] = &[
            ("question", |v| v["question"] = json!("Q")),
            ("detail", |v| v["detail"] = json!("D")),
            ("label", |v| v["options"][1]["label"] = json!("B")),
            ("hint", |v| v["options"][1]["hint"] = json!("H")),
            ("note prompt", |v| v["options"][1]["note"] = json!("N")),
            ("option order", |v| {
                v["options"] = json!([{"label": "b", "hint": "h", "note": "n"}, "a"])
            }),
            ("default", |v| v["default"] = json!("b")),
            ("field id", |v| v["fields"][0]["id"] = json!("x")),
            ("field label", |v| v["fields"][0]["label"] = json!("X")),
            ("field type", |v| v["fields"][0]["type"] = json!("long")),
            ("prefill", |v| v["fields"][0]["value"] = json!("w")),
            ("check prefill", |v| v["fields"][1]["value"] = json!(false)),
            ("placeholder", |v| {
                v["fields"][0]["placeholder"] = json!("P")
            }),
            ("optional", |v| v["fields"][0]["optional"] = json!(false)),
        ];
        for (what, edit) in edits {
            let mut v = base.clone();
            edit(&mut v);
            assert_ne!(contract(&doc(v)), c0, "{what} must change the contract");
        }
        let single = doc(json!({"question": "q", "options": ["a", "b"]}));
        let multi = doc(json!({"question": "q", "options": ["a", "b"], "multi": true}));
        assert_ne!(
            contract(&single),
            contract(&multi),
            "multi must change the contract"
        );
        // `wait` is the call's mode, not the card's meaning.
        let mut v = base.clone();
        v["wait"] = json!(true);
        assert_eq!(contract(&doc(v)), c0);
    }
}
