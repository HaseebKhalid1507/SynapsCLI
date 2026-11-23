//! Agent cards ("surfaces"), catalog `synaps:card/1`: the card model.
//!
//! Phase C1 of `agent-ui/architecture.md` v3.2. **Inert:** nothing in a
//! running session reaches this module. There is no tool, no wire type, no
//! actor or daemon hook and no config; later phases (C2 storage, C3 wire,
//! C4a the `ui` tool) build on it.
//!
//! The pure data types and every bound live in
//! `agent_core::core::ui_types` (v3.2 #12) and are re-exported here, so
//! paths don't change. The logic is here:
//! - [`schema.json`](SCHEMA_JSON): the normative input schema (§5.1).
//! - [`doc`]: validate and normalize a `ui` input into a [`CardDoc`];
//!   [`revalidate`] a stored one.
//! - [`sanitize`]: `ui_sanitize`, the one sanitizer for every card string,
//!   and the case-fold key labels are compared by.
//! - [`contract`]: the contract digest over the normalized doc (§5.4).
//! - [`answer`]: answer values, the `<ui_answer>` message, the late-answer
//!   capsule (§3.6, §5.3).
//! - [`fold`]: folding host-proven evidence (`UiMark`) out of history
//!   (§2.5).
//!
//! The corpus at `crates/agent-engine/tests/fixtures/ui/` pins all of it;
//! the desk's D1 copies it for parity.

pub mod answer;
pub mod contract;
pub mod doc;
pub mod fold;
pub mod sanitize;

pub use agent_core::core::ui_types::*;
pub use answer::{AnswerMessage, Capsule};
pub use contract::contract;
pub use doc::{parse_input, revalidate, secret_footnote, UiInput};
pub use sanitize::{roster_label, ui_sanitize};

use sanitize::{cut_chars, Lines as L};

/// The normative input schema (§5.1). The `ui` tool's `input_schema` is
/// this file; [`doc::parse_input`] enforces it plus the rules beyond it.
pub const SCHEMA_JSON: &str = include_str!("schema.json");

/// [`SCHEMA_JSON`] parsed.
pub fn input_schema() -> serde_json::Value {
    serde_json::from_str(SCHEMA_JSON).expect("ui/schema.json is valid JSON")
}

/// The longest refusal path and message, in characters (Shady #439 M1).
pub const ERROR_PATH_MAX: usize = 80;
pub const ERROR_MESSAGE_MAX: usize = 240;

/// A refusal: the offending path and one teaching line. `Display` is the
/// tool error the agent reads, e.g.
/// `options: at most 9 (got 12); ask in plain text or narrow the choices`.
///
/// Built only by [`UiError::new`], which puts both parts through
/// `ui_sanitize` (single line) and bounds them, so no key or value the
/// agent wrote can carry a control, a bidi override or a newline into the
/// error, or make it long (§4.3). Untrusted keys are written with
/// [`key_path`] first, so they stay readable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiError {
    path: String,
    message: String,
}

impl UiError {
    pub(crate) fn new(path: impl AsRef<str>, message: impl AsRef<str>) -> Self {
        Self {
            path: cut_chars(&ui_sanitize(path.as_ref(), L::Single), ERROR_PATH_MAX),
            message: cut_chars(&ui_sanitize(message.as_ref(), L::Single), ERROR_MESSAGE_MAX),
        }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for UiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

impl std::error::Error for UiError {}

/// A key the agent or a client wrote, as a path segment: bare when it is a
/// plain identifier (`blocks`), else a JSON string with everything outside
/// printable ASCII escaped (`"\u001b[31m"`), cut to 24 characters first.
pub(crate) fn key_path(k: &str) -> String {
    let plain = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
    if !k.is_empty() && k.len() <= 24 && k.bytes().all(plain) {
        return k.to_string();
    }
    let mut out = String::from("\"");
    let n = k.chars().count();
    for c in k.chars().take(24) {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            ' '..='~' => out.push(c),
            c => {
                let mut units = [0u16; 2];
                for u in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{u:04x}"));
                }
            }
        }
    }
    if n > 24 {
        out.push('…');
    }
    out.push('"');
    out
}

/// Whether `v` nests deeper than `limit`, scalars counted as one level.
/// The recursion stops at `limit`, so a value of any depth is safe to ask
/// about (Shady #439 M2).
pub(crate) fn deeper_than(v: &serde_json::Value, limit: usize) -> bool {
    use serde_json::Value;
    if limit == 0 {
        return true;
    }
    match v {
        Value::Array(a) => a.iter().any(|v| deeper_than(v, limit - 1)),
        Value::Object(o) => o.values().any(|v| deeper_than(v, limit - 1)),
        _ => false,
    }
}

/// The compact JSON size of `v` (a value already within a depth bound),
/// counted without building it; `None` once it passes `cap`.
pub(crate) fn json_len_within(v: &impl serde::Serialize, cap: usize) -> Option<usize> {
    struct Count {
        n: usize,
        cap: usize,
    }
    impl std::io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.n += buf.len();
            if self.n > self.cap {
                return Err(std::io::Error::other("over the cap"));
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut c = Count { n: 0, cap };
    serde_json::to_writer(&mut c, v).ok().map(|()| c.n)
}

/// Decode JSON text that will become a `ui` input or answer values: the
/// shared ingress (Shady #439 L2). Stricter than `serde_json::from_str`:
/// a repeated key at any level is refused, not resolved last-wins. Like
/// `serde_json` it refuses lone surrogates, numbers out of `f64` range and
/// nesting past 128; the model's own depth and byte caps apply after.
pub fn decode_json(text: &str) -> Result<serde_json::Value, UiError> {
    let mut de = serde_json::Deserializer::from_str(text);
    let v = <Strict as serde::Deserialize>::deserialize(&mut de)
        .and_then(|v| de.end().map(|()| v.0))
        .map_err(|e| UiError::new("json", e.to_string()))?;
    Ok(v)
}

/// A `Value` decoded with repeated keys refused.
struct Strict(serde_json::Value);

impl<'de> serde::Deserialize<'de> for Strict {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(StrictVisitor).map(Strict)
    }
}

struct StrictVisitor;

impl<'de> serde::de::Visitor<'de> for StrictVisitor {
    type Value = serde_json::Value;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("JSON")
    }
    fn visit_bool<E>(self, b: bool) -> Result<Self::Value, E> {
        Ok(b.into())
    }
    fn visit_i64<E>(self, n: i64) -> Result<Self::Value, E> {
        Ok(n.into())
    }
    fn visit_u64<E>(self, n: u64) -> Result<Self::Value, E> {
        Ok(n.into())
    }
    fn visit_f64<E: serde::de::Error>(self, n: f64) -> Result<Self::Value, E> {
        serde_json::Number::from_f64(n)
            .map(Into::into)
            .ok_or_else(|| E::custom("number out of range"))
    }
    fn visit_str<E>(self, s: &str) -> Result<Self::Value, E> {
        Ok(s.into())
    }
    fn visit_string<E>(self, s: String) -> Result<Self::Value, E> {
        Ok(s.into())
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
        let mut out = Vec::new();
        while let Some(Strict(v)) = a.next_element()? {
            out.push(v);
        }
        Ok(out.into())
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut m: A) -> Result<Self::Value, A::Error> {
        let mut out = serde_json::Map::new();
        while let Some(k) = m.next_key::<String>()? {
            let Strict(v) = m.next_value()?;
            if out.contains_key(&k) {
                return Err(serde::de::Error::custom(format!(
                    "the key {} is given twice",
                    key_path(&k)
                )));
            }
            out.insert(k, v);
        }
        Ok(out.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn at<'a>(v: &'a Value, path: &[&str]) -> &'a Value {
        path.iter().fold(v, |v, k| &v[*k])
    }

    /// The shipped schema is §5.1 and agrees with every constant the
    /// validator uses.
    #[test]
    fn schema_json_matches_the_constants() {
        let s = input_schema();
        let p = &s["properties"];
        let n = |v: &Value| v.as_u64().unwrap() as usize;
        assert_eq!(s["additionalProperties"], false);
        assert_eq!(n(&p["question"]["minLength"]), 1);
        assert_eq!(n(&p["question"]["maxLength"]), QUESTION_MAX);
        assert_eq!(n(&p["detail"]["maxLength"]), DETAIL_MAX);
        assert_eq!(n(&p["options"]["minItems"]), OPTIONS_MIN);
        assert_eq!(n(&p["options"]["maxItems"]), OPTIONS_MAX);
        let [as_str, as_obj] = p["options"]["items"]["oneOf"]
            .as_array()
            .unwrap()
            .as_slice()
        else {
            panic!()
        };
        assert_eq!(n(&as_str["maxLength"]), OPTION_LABEL_MAX);
        assert_eq!(as_obj["additionalProperties"], false);
        assert_eq!(
            n(at(as_obj, &["properties", "label", "maxLength"])),
            OPTION_LABEL_MAX
        );
        assert_eq!(
            n(at(as_obj, &["properties", "hint", "maxLength"])),
            HINT_MAX
        );
        assert_eq!(
            n(at(as_obj, &["properties", "note", "maxLength"])),
            NOTE_PROMPT_MAX
        );
        assert_eq!(n(&p["default"]["oneOf"][1]["maxItems"]), DEFAULT_MAX);
        assert_eq!(n(&p["fields"]["minItems"]), FIELDS_MIN);
        assert_eq!(n(&p["fields"]["maxItems"]), FIELDS_MAX);
        let f = &p["fields"]["items"];
        assert_eq!(f["additionalProperties"], false);
        assert_eq!(f["properties"]["id"]["pattern"], "^[a-z][a-z0-9_]{0,23}$");
        assert_eq!(n(&f["properties"]["label"]["maxLength"]), FIELD_LABEL_MAX);
        assert_eq!(
            n(&f["properties"]["placeholder"]["maxLength"]),
            PLACEHOLDER_MAX
        );
        assert_eq!(
            f["properties"]["type"]["enum"],
            serde_json::json!(["text", "long", "check"])
        );
        assert_eq!(p["close"]["pattern"], "^s_[0-9a-f]{16}$");
        let keys: Vec<&str> = p.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, doc::TOP_KEYS_SORTED);
    }

    /// Shady #439 M1: whatever a key holds, a refusal is one short line
    /// with no control, bidi control, invisible character or newline.
    #[test]
    fn errors_are_one_safe_bounded_line() {
        let hostile = "\u{1b}[31m\n\u{202E}forged\u{200B}\u{85}";
        assert_eq!(
            key_path(hostile),
            r#""\u001b[31m\u000a\u202eforged\u200b\u0085""#
        );
        assert_eq!(key_path("blocks"), "blocks");
        assert_eq!(key_path("wait-for"), "wait-for");
        assert_eq!(key_path(""), r#""""#);
        assert_eq!(key_path("a\"b\\c"), r#""a\"b\\c""#);
        assert_eq!(key_path("\u{1F600}"), r#""\ud83d\ude00""#);
        assert_eq!(
            key_path(&"k".repeat(25)),
            format!("\"{}…\"", "k".repeat(24))
        );
        let e = UiError::new(
            format!("x{hostile}"),
            format!("m{hostile}\r\n{}", "y".repeat(500)),
        );
        let shown = e.to_string();
        assert!(safe_line(&shown), "{shown:?}");
        assert_eq!(e.path(), "x[31m forged");
        assert!(shown.chars().count() <= ERROR_PATH_MAX + 2 + ERROR_MESSAGE_MAX);
    }

    /// One line, bounded, nothing that can hide or reorder text.
    pub(crate) fn safe_line(s: &str) -> bool {
        s.chars().count() <= ERROR_PATH_MAX + 2 + ERROR_MESSAGE_MAX
            && s.chars().all(|c| {
                !c.is_control()
                    && !crate::events::format::is_invisible(c)
                    && !matches!(c, '\u{2028}' | '\u{2029}')
            })
    }

    #[test]
    fn json_len_stops_at_the_cap() {
        let v = serde_json::json!({"a": "x".repeat(100)});
        let n = serde_json::to_vec(&v).unwrap().len();
        assert_eq!(json_len_within(&v, n), Some(n));
        assert_eq!(json_len_within(&v, n - 1), None);
    }

    /// Shady #439 L2: the raw-JSON ingress.
    #[test]
    fn decode_json_is_strict() {
        assert_eq!(
            decode_json(r#"{"a":[1,{"b":null}],"c":"\ud83d\ude00"}"#).unwrap(),
            serde_json::json!({"a": [1, {"b": null}], "c": "\u{1F600}"})
        );
        for (text, want) in [
            (r#"{"a":1,"a":2}"#, "json: the key a is given twice"),
            (r#"{"x":{"b":1,"b":1}}"#, "json: the key b is given twice"),
            (
                r#"[{"\u001b":1,"\u001b":2}]"#,
                r#"json: the key "\u001b" is given twice"#,
            ),
        ] {
            assert!(
                decode_json(text).unwrap_err().to_string().starts_with(want),
                "{text}"
            );
        }
        for bad in [
            r#""\ud83d""#,
            r#""\ude00""#,
            r#""\ud83dx""#,
            "1e400",
            "-1e400",
            "{} {}",
            "{\"a\":1,}",
        ] {
            let e = decode_json(bad).expect_err(bad);
            assert!(e.to_string().starts_with("json: "), "{e}");
        }
        assert!(decode_json(&format!("{}1{}", "[".repeat(128), "]".repeat(128))).is_err());
        assert!(decode_json(&format!("{}1{}", "[".repeat(127), "]".repeat(127))).is_ok());
        // Big integers stay numbers (u64, then f64); the model refuses them by type.
        assert!(decode_json("18446744073709551616").unwrap().is_f64());
        assert!(decode_json("-9223372036854775809").unwrap().is_f64());
    }
}
