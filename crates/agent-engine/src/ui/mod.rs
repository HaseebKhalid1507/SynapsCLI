//! Agent cards ("surfaces"), catalog `synaps:card/1`: the card model.
//!
//! Phase C1 of `agent-ui/architecture.md` v3.1. **Inert:** nothing in a
//! running session reaches this module. There is no tool, no wire type, no
//! actor or daemon hook and no config; later phases (C2 storage, C3 wire,
//! C4a the `ui` tool) build on it.
//!
//! - [`schema.json`](SCHEMA_JSON): the normative input schema (§5.1).
//! - [`doc`]: validate and normalize a `ui` input into a [`CardDoc`].
//! - [`sanitize`]: `ui_sanitize`, the one sanitizer for every card string.
//! - [`contract`]: the contract digest over the normalized doc (§5.4).
//! - [`answer`]: answer values, the `<ui_answer>` message, the late-answer
//!   capsule (§3.6, §5.3).
//! - [`fold`]: `UiMark` and folding host-proven evidence out of history
//!   (§2.5).
//!
//! The corpus at `tests/fixtures/ui/` (repository root) pins all of it; the
//! desk's D1 copies it for parity.

pub mod answer;
pub mod contract;
pub mod doc;
pub mod fold;
pub mod sanitize;

pub use answer::{Answer, AnswerStatus, Choice, FieldValue};
pub use contract::contract;
pub use doc::{CardDoc, CardField, CardOption, DefaultChoice, FieldKind, Mode, UiInput};
pub use fold::UiMark;
pub use sanitize::{ui_sanitize, Lines};

/// The normative input schema (§5.1). The `ui` tool's `input_schema` is
/// this file; [`doc::parse_input`] enforces it plus the rules beyond it.
pub const SCHEMA_JSON: &str = include_str!("schema.json");

/// [`SCHEMA_JSON`] parsed.
pub fn input_schema() -> serde_json::Value {
    serde_json::from_str(SCHEMA_JSON).expect("ui/schema.json is valid JSON")
}

/// The catalog major this module implements (`synaps:card/1`).
pub const CATALOG: u32 = 1;
/// The normalizer version folded into every contract (§5.4).
pub const NORMALIZER: u32 = 1;

// ── Bounds (§4.4, §5.1). Every one is tested at N and N+1. ───────────────
// Character bounds count Unicode scalar values, as JSON Schema does.

/// A `ui` input, serialized compactly. Refused, never truncated.
pub const INPUT_MAX_BYTES: usize = 8 * 1024;
/// Nesting of a `ui` input, scalars counted: `{"options":[{"label":"x"}]}`
/// is 4.
pub const INPUT_MAX_DEPTH: usize = 4;
pub const QUESTION_MAX: usize = 120;
pub const DETAIL_MAX: usize = 1500;
pub const DETAIL_MAX_LINES: usize = 30;
pub const OPTIONS_MIN: usize = 2;
pub const OPTIONS_MAX: usize = 9;
pub const OPTION_LABEL_MAX: usize = 60;
pub const HINT_MAX: usize = 120;
pub const NOTE_PROMPT_MAX: usize = 80;
pub const DEFAULT_MAX: usize = 9;
pub const FIELDS_MIN: usize = 1;
pub const FIELDS_MAX: usize = 5;
/// `^[a-z][a-z0-9_]{0,23}$`
pub const FIELD_ID_MAX: usize = 24;
pub const FIELD_LABEL_MAX: usize = 60;
pub const PLACEHOLDER_MAX: usize = 80;
/// A `text` value: the agent's prefill and the user's answer.
pub const TEXT_MAX: usize = 200;
/// A `long` value: the agent's prefill and the user's answer.
pub const LONG_MAX: usize = 2000;
/// A note typed for a `note` option.
pub const NOTE_MIN: usize = 1;
pub const NOTE_MAX: usize = 500;
/// Answer values, serialized compactly (§3.3 #11).
pub const ANSWER_MAX_BYTES: usize = 8 * 1024;
/// A late answer's `<asked>` capsule, encoded (§3.6).
pub const CAPSULE_MAX_BYTES: usize = 1024;
/// The capsule's copy of the detail.
pub const CAPSULE_DETAIL_MAX: usize = 400;
/// `origin.task`: the excerpt of the message that started the card's turn.
pub const TASK_EXCERPT_MAX: usize = 80;

// ── System budget (§4.4, R2-M3). C1 measures these on worst-case images;
// C2 (sidecar) and C3 (wire) enforce them. ────────────────────────────────

pub const SURFACE_STATE_MAX_BYTES: usize = 24 * 1024;
pub const ATTACHED_MAX_STATES: usize = 12;
pub const ATTACHED_MAX_BYTES: usize = 288 * 1024;
pub const LEDGER_MAX_RECORDS: usize = 16;
pub const LEDGER_MAX_VALUE_BYTES: usize = 64 * 1024;
pub const TOMBSTONES_MAX: usize = 128;
pub const TOMBSTONE_MAX_BYTES: usize = 1024;
pub const ACTION_LOG_MAX: usize = 5;
pub const SIDECAR_MAX_BYTES: usize = 512 * 1024;

/// A refusal: the offending path and one teaching line. `Display` is the
/// tool error the agent reads, e.g.
/// `options: at most 9 (got 12); ask in plain text or narrow the choices`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiError {
    pub path: String,
    pub message: String,
}

impl UiError {
    pub(crate) fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for UiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

impl std::error::Error for UiError {}

/// `true` when `s` is `<prefix>` + 16 lowercase hex: a surface id (`s_`) or
/// an answer id (`a_`).
pub fn is_host_id(s: &str, prefix: &str) -> bool {
    s.strip_prefix(prefix).is_some_and(|hex| {
        hex.len() == 16 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
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

    #[test]
    fn host_ids() {
        assert!(is_host_id("s_3f2a91c0d4e2a871", "s_"));
        assert!(is_host_id("a_6c1d2e3f4a5b6c7d", "a_"));
        for bad in [
            "s_3f2a91c0d4e2a87",
            "s_3f2a91c0d4e2a8711",
            "s_3F2A91C0D4E2A871",
            "a_3f2a91c0d4e2a871",
            "s_3f2a91c0d4e2a87g",
        ] {
            assert!(!is_host_id(bad, "s_"), "{bad}");
        }
    }
}
