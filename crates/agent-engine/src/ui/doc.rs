//! A `ui` input, validated and normalized into a [`CardDoc`] (§5.1).
//!
//! [`parse_input`] enforces the JSON Schema (`schema.json`) by hand, then
//! the rules beyond it, and reports the first failure as one teaching line
//! ([`UiError`]). The result is the stored, normalized doc:
//! - every string has been through `ui_sanitize` (single-line fields fold
//!   `\n` to a space);
//! - string options are `{label}`;
//! - `multi`, a field's `type`, `value` and `optional` are materialized;
//! - an empty optional string (`detail`, `hint`, `placeholder`) is absent.
//!
//! Strings holding a C0/C1 control (other than `\n`/`\t`) or a bidi
//! embedding, override or isolate are refused, not rewritten
//! (`sanitize::refused_char`). Length bounds apply to the raw string (as
//! JSON Schema does); "not empty" applies to the sanitized one.
//!
//! Normalizing is a fixed point: a doc serialized and parsed again is the
//! same doc ([`CardDoc::revalidate`]).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::answer::FieldValue;
use super::sanitize::{char_len, cut_chars, looks_secret, refused_char, ui_sanitize, Lines};
use super::*;

/// The schema's top-level properties, sorted.
pub(crate) const TOP_KEYS_SORTED: [&str; 8] = [
    "close", "default", "detail", "fields", "multi", "options", "question", "wait",
];
const TOP_KEYS: &str = "question, detail, options, multi, default, fields, wait or close";
const OPTION_KEYS: [&str; 3] = ["label", "hint", "note"];
const FIELD_KEYS: [&str; 6] = ["id", "label", "type", "value", "placeholder", "optional"];

/// A parsed `ui` call: ask (create) or withdraw (close).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiInput {
    Create { doc: CardDoc, wait: bool },
    Close(String),
}

/// One normalized question (§5). Exactly the contract's preimage
/// (`contract::preimage`); `wait` is the call's mode, not part of the doc.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardDoc {
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<CardOption>,
    #[serde(default)]
    pub multi: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<DefaultChoice>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<CardField>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// The prompt of the free-text note this option asks for when picked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The agent's recommendation: one label, or (with `multi`) a list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DefaultChoice {
    One(String),
    Many(Vec<String>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardField {
    pub id: String,
    pub label: String,
    #[serde(rename = "type")]
    pub kind: FieldKind,
    /// The prefill: `""` / `false` when the agent gave none.
    pub value: FieldValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    pub optional: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldKind {
    Text,
    Long,
    Check,
}

impl FieldKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FieldKind::Text => "text",
            FieldKind::Long => "long",
            FieldKind::Check => "check",
        }
    }

    /// The bound on a `text` / `long` value; `None` for a check.
    pub fn max_chars(self) -> Option<usize> {
        match self {
            FieldKind::Text => Some(TEXT_MAX),
            FieldKind::Long => Some(LONG_MAX),
            FieldKind::Check => None,
        }
    }

    pub fn lines(self) -> Lines {
        if self == FieldKind::Long {
            Lines::Multi
        } else {
            Lines::Single
        }
    }
}

/// How a card is answered (§5.2), derived by the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// `options`, not `multi`, no `fields`: a click or digit answers at once.
    Quick,
    /// Anything else: a Send button.
    Form,
}

impl CardDoc {
    pub fn mode(&self) -> Mode {
        if !self.options.is_empty() && !self.multi && self.fields.is_empty() {
            Mode::Quick
        } else {
            Mode::Form
        }
    }

    pub fn option(&self, label: &str) -> Option<&CardOption> {
        self.options.iter().find(|o| o.label == label)
    }

    /// The §4.7 footnote ("Don't type secrets here") applies: a field label
    /// matches the advisory secret heuristic. Never a refusal.
    pub fn secret_footnote(&self) -> bool {
        self.fields
            .iter()
            .any(|f| f.kind != FieldKind::Check && looks_secret(&f.label))
    }

    /// Check a doc read back from storage or the wire: it must be exactly
    /// what [`parse_input`] makes of it.
    pub fn revalidate(&self) -> Result<(), UiError> {
        let value = serde_json::to_value(self).map_err(|e| UiError::new("doc", e.to_string()))?;
        match parse_input(&value)? {
            UiInput::Create { doc, .. } if doc == *self => Ok(()),
            _ => Err(UiError::new("doc", "is not normalized")),
        }
    }
}

/// Validate and normalize one `ui` input.
pub fn parse_input(input: &Value) -> Result<UiInput, UiError> {
    let bytes = serde_json::to_vec(input).map_or(usize::MAX, |v| v.len());
    if bytes > INPUT_MAX_BYTES {
        return Err(UiError::new(
            "input",
            format!("at most {INPUT_MAX_BYTES} bytes (got {bytes}); shorten the detail or ask in plain text"),
        ));
    }
    if deeper_than(input, INPUT_MAX_DEPTH) {
        return Err(UiError::new(
            "input",
            format!("nested deeper than {INPUT_MAX_DEPTH} levels"),
        ));
    }
    let Some(obj) = input.as_object() else {
        return Err(UiError::new("input", "must be a JSON object"));
    };
    if let Some(k) = obj.keys().find(|k| !TOP_KEYS_SORTED.contains(&k.as_str())) {
        return Err(UiError::new(
            k.as_str(),
            format!("unknown parameter; use {TOP_KEYS}"),
        ));
    }
    if let Some(close) = obj.get("close") {
        if let Some(other) = obj.keys().find(|k| *k != "close") {
            return Err(UiError::new(
                "close",
                format!("give it alone, with no other parameter (got {other})"),
            ));
        }
        return match close.as_str() {
            Some(id) if is_host_id(id, "s_") => Ok(UiInput::Close(id.to_string())),
            _ => Err(UiError::new(
                "close",
                "must be the id of a card you created, like s_3f2a91c0d4e2a871",
            )),
        };
    }
    let wait = flag(obj, "wait")?;
    let multi = flag(obj, "multi")?;
    let Some(question) = obj.get("question") else {
        return Err(UiError::new(
            "question",
            "required: the one-line question to ask",
        ));
    };
    let question = string(
        question,
        "question",
        Lines::Single,
        1,
        QUESTION_MAX,
        "; put the rest in detail",
    )?;
    let detail = match obj.get("detail") {
        Some(v) => Some(string(v, "detail", Lines::Multi, 0, DETAIL_MAX, "")?),
        None => None,
    };
    if let Some(d) = &detail {
        let lines = d.split('\n').count();
        if lines > DETAIL_MAX_LINES {
            return Err(UiError::new(
                "detail",
                format!("at most {DETAIL_MAX_LINES} lines (got {lines})"),
            ));
        }
    }
    if !obj.contains_key("options") && !obj.contains_key("fields") {
        return Err(UiError::new(
            "options",
            "a card needs options or fields (or both)",
        ));
    }
    let options = match obj.get("options") {
        Some(v) => options(v, multi)?,
        None => Vec::new(),
    };
    let default = match obj.get("default") {
        Some(v) => Some(default(v, &options, multi)?),
        None => None,
    };
    let fields = match obj.get("fields") {
        Some(v) => fields(v)?,
        None => Vec::new(),
    };
    let doc = CardDoc {
        question,
        detail: non_empty(detail),
        options,
        multi,
        default,
        fields,
    };
    Ok(UiInput::Create { doc, wait })
}

/// Whether `v` nests deeper than `limit`, scalars counted as one level.
fn deeper_than(v: &Value, limit: usize) -> bool {
    if limit == 0 {
        return true;
    }
    match v {
        Value::Array(a) => a.iter().any(|v| deeper_than(v, limit - 1)),
        Value::Object(o) => o.values().any(|v| deeper_than(v, limit - 1)),
        _ => false,
    }
}

fn flag(obj: &Map<String, Value>, key: &str) -> Result<bool, UiError> {
    match obj.get(key) {
        None => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(UiError::new(key, "must be true or false")),
    }
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.is_empty())
}

/// A shown string: refused characters, the raw bound, then `ui_sanitize`
/// and the minimum on what is left.
fn string(
    v: &Value,
    path: &str,
    lines: Lines,
    min: usize,
    max: usize,
    advice: &str,
) -> Result<String, UiError> {
    let Some(s) = v.as_str() else {
        return Err(UiError::new(path, "must be a string"));
    };
    if let Some((c, why)) = refused_char(s) {
        return Err(UiError::new(
            path,
            format!("holds U+{:04X} ({why}); remove it", c as u32),
        ));
    }
    let n = char_len(s);
    if n > max {
        return Err(UiError::new(
            path,
            format!("at most {max} characters (got {n}){advice}"),
        ));
    }
    let clean = ui_sanitize(s, lines);
    if char_len(&clean) < min {
        let msg = if s.is_empty() {
            "must not be empty"
        } else {
            "must not be empty (it holds only invisible characters)"
        };
        return Err(UiError::new(path, msg));
    }
    Ok(clean)
}

/// A value quoted in an error line: sanitized and cut, so an error can't
/// carry what the card couldn't.
fn quote(s: &str) -> String {
    format!("\"{}\"", cut_chars(&ui_sanitize(s, Lines::Single), 40))
}

fn options(v: &Value, multi: bool) -> Result<Vec<CardOption>, UiError> {
    let Some(items) = v.as_array() else {
        return Err(UiError::new(
            "options",
            format!("must be a list of {OPTIONS_MIN} to {OPTIONS_MAX} options"),
        ));
    };
    let n = items.len();
    if n < OPTIONS_MIN {
        return Err(UiError::new(
            "options",
            format!("at least {OPTIONS_MIN} (got {n}); one option is not a choice"),
        ));
    }
    if n > OPTIONS_MAX {
        return Err(UiError::new(
            "options",
            format!("at most {OPTIONS_MAX} (got {n}); ask in plain text or narrow the choices"),
        ));
    }
    let mut out: Vec<CardOption> = Vec::with_capacity(n);
    let mut label_paths: Vec<String> = Vec::with_capacity(n);
    let mut note_at: Option<usize> = None;
    for (i, item) in items.iter().enumerate() {
        let path = format!("options[{i}]");
        let (option, label_path) = match item {
            Value::String(_) => {
                let label = string(
                    item,
                    &path,
                    Lines::Single,
                    1,
                    OPTION_LABEL_MAX,
                    "; move the rest into a hint",
                )?;
                (
                    CardOption {
                        label,
                        hint: None,
                        note: None,
                    },
                    path.clone(),
                )
            }
            Value::Object(o) => {
                if let Some(k) = o.keys().find(|k| !OPTION_KEYS.contains(&k.as_str())) {
                    return Err(UiError::new(
                        format!("{path}.{k}"),
                        "unknown key; an option takes label, hint and note",
                    ));
                }
                let label_path = format!("{path}.label");
                let Some(label) = o.get("label") else {
                    return Err(UiError::new(label_path, "required"));
                };
                let label = string(
                    label,
                    &label_path,
                    Lines::Single,
                    1,
                    OPTION_LABEL_MAX,
                    "; move the rest into a hint",
                )?;
                let hint = match o.get("hint") {
                    Some(h) => non_empty(Some(string(
                        h,
                        &format!("{path}.hint"),
                        Lines::Single,
                        0,
                        HINT_MAX,
                        "",
                    )?)),
                    None => None,
                };
                let note = match o.get("note") {
                    Some(p) => Some(string(
                        p,
                        &format!("{path}.note"),
                        Lines::Single,
                        1,
                        NOTE_PROMPT_MAX,
                        "",
                    )?),
                    None => None,
                };
                (CardOption { label, hint, note }, label_path)
            }
            _ => {
                return Err(UiError::new(
                    path,
                    "must be a label or {label, hint?, note?}",
                ))
            }
        };
        let folded = option.label.to_lowercase();
        if let Some(j) = out.iter().position(|o| o.label.to_lowercase() == folded) {
            return Err(UiError::new(
                label_path,
                format!(
                    "{} duplicates {}; labels must differ (ignoring case)",
                    quote(&option.label),
                    label_paths[j]
                ),
            ));
        }
        if option.note.is_some() {
            if multi {
                return Err(UiError::new(
                    format!("{path}.note"),
                    "not allowed with multi; use a field for free text",
                ));
            }
            if let Some(j) = note_at {
                return Err(UiError::new(
                    format!("{path}.note"),
                    format!("only one option may ask for a note (options[{j}] already does)"),
                ));
            }
            note_at = Some(i);
        }
        out.push(option);
        label_paths.push(label_path);
    }
    Ok(out)
}

fn default(v: &Value, options: &[CardOption], multi: bool) -> Result<DefaultChoice, UiError> {
    if options.is_empty() {
        return Err(UiError::new(
            "default",
            "needs options; it names the option you recommend",
        ));
    }
    let label = |v: &Value, path: &str| -> Result<String, UiError> {
        let l = string(v, path, Lines::Single, 0, usize::MAX, "")?;
        if options.iter().any(|o| o.label == l) {
            Ok(l)
        } else {
            Err(UiError::new(
                path,
                format!("{} is not one of the option labels", quote(&l)),
            ))
        }
    };
    match v {
        Value::String(_) if multi => Err(UiError::new(
            "default",
            "a list of option labels when multi is true",
        )),
        Value::String(_) => Ok(DefaultChoice::One(label(v, "default")?)),
        Value::Array(_) if !multi => Err(UiError::new(
            "default",
            "one option label (a list only when multi is true)",
        )),
        Value::Array(items) => {
            if items.len() > DEFAULT_MAX {
                return Err(UiError::new(
                    "default",
                    format!("at most {DEFAULT_MAX} labels (got {})", items.len()),
                ));
            }
            let mut out: Vec<String> = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                let path = format!("default[{i}]");
                let l = label(item, &path)?;
                if out.contains(&l) {
                    return Err(UiError::new(path, format!("{} is listed twice", quote(&l))));
                }
                out.push(l);
            }
            Ok(DefaultChoice::Many(out))
        }
        _ => Err(UiError::new("default", "must be an option label")),
    }
}

fn fields(v: &Value) -> Result<Vec<CardField>, UiError> {
    let Some(items) = v.as_array() else {
        return Err(UiError::new(
            "fields",
            format!("must be a list of {FIELDS_MIN} to {FIELDS_MAX} fields"),
        ));
    };
    let n = items.len();
    if n < FIELDS_MIN {
        return Err(UiError::new(
            "fields",
            format!("at least {FIELDS_MIN} (got {n}); leave fields out instead"),
        ));
    }
    if n > FIELDS_MAX {
        return Err(UiError::new(
            "fields",
            format!("at most {FIELDS_MAX} (got {n}); ask fewer things at once"),
        ));
    }
    let mut out: Vec<CardField> = Vec::with_capacity(n);
    for (i, item) in items.iter().enumerate() {
        let path = format!("fields[{i}]");
        let Some(o) = item.as_object() else {
            return Err(UiError::new(
                path,
                "must be an object {id, label, type?, value?, placeholder?, optional?}",
            ));
        };
        if let Some(k) = o.keys().find(|k| !FIELD_KEYS.contains(&k.as_str())) {
            return Err(UiError::new(
                format!("{path}.{k}"),
                "unknown key; a field takes id, label, type, value, placeholder and optional",
            ));
        }
        let id = field_id(o.get("id"), &format!("{path}.id"))?;
        if let Some(j) = out.iter().position(|f| f.id == id) {
            return Err(UiError::new(
                format!("{path}.id"),
                format!("\"{id}\" duplicates fields[{j}].id"),
            ));
        }
        let Some(label) = o.get("label") else {
            return Err(UiError::new(format!("{path}.label"), "required"));
        };
        let label = string(
            label,
            &format!("{path}.label"),
            Lines::Single,
            1,
            FIELD_LABEL_MAX,
            "",
        )?;
        let kind = match o.get("type") {
            None => FieldKind::Text,
            Some(Value::String(t)) if t == "text" => FieldKind::Text,
            Some(Value::String(t)) if t == "long" => FieldKind::Long,
            Some(Value::String(t)) if t == "check" => FieldKind::Check,
            Some(_) => {
                return Err(UiError::new(
                    format!("{path}.type"),
                    "must be text, long or check",
                ))
            }
        };
        let value_path = format!("{path}.value");
        let value = match (kind, o.get("value")) {
            (FieldKind::Check, None) => FieldValue::Check(false),
            (FieldKind::Check, Some(Value::Bool(b))) => FieldValue::Check(*b),
            (FieldKind::Check, Some(other)) => {
                return Err(UiError::new(
                    value_path,
                    format!("a check takes true or false (got {})", json_quote(other)),
                ))
            }
            (_, None) => FieldValue::Text(String::new()),
            (_, Some(v @ Value::String(_))) => {
                let max = kind.max_chars().unwrap_or(TEXT_MAX);
                let advice = format!(" for a {} field", kind.as_str());
                FieldValue::Text(string(v, &value_path, kind.lines(), 0, max, &advice)?)
            }
            (_, Some(other)) => {
                return Err(UiError::new(
                    value_path,
                    format!(
                        "a {} field takes a string (got {})",
                        kind.as_str(),
                        json_quote(other)
                    ),
                ))
            }
        };
        let placeholder = match o.get("placeholder") {
            Some(p) => non_empty(Some(string(
                p,
                &format!("{path}.placeholder"),
                Lines::Single,
                0,
                PLACEHOLDER_MAX,
                "",
            )?)),
            None => None,
        };
        let optional =
            flag(o, "optional").map_err(|e| UiError::new(format!("{path}.optional"), e.message))?;
        if optional && kind == FieldKind::Check {
            return Err(UiError::new(
                format!("{path}.optional"),
                "a check is always answered; only text and long fields can be optional",
            ));
        }
        out.push(CardField {
            id,
            label,
            kind,
            value,
            placeholder,
            optional,
        });
    }
    Ok(out)
}

/// `^[a-z][a-z0-9_]{0,23}$`
fn field_id(v: Option<&Value>, path: &str) -> Result<String, UiError> {
    let Some(v) = v else {
        return Err(UiError::new(path, "required"));
    };
    let Some(id) = v.as_str() else {
        return Err(UiError::new(path, "must be a string"));
    };
    let mut bytes = id.bytes();
    let ok = bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && id.len() <= FIELD_ID_MAX;
    if ok {
        Ok(id.to_string())
    } else {
        Err(UiError::new(
            path,
            format!(
                "{} must match ^[a-z][a-z0-9_]{{0,23}}$: a lowercase letter, then up to 23 lowercase letters, digits or _",
                quote(id)
            ),
        ))
    }
}

/// A JSON value quoted in an error line, cut.
fn json_quote(v: &Value) -> String {
    match v {
        Value::String(s) => quote(s),
        other => cut_chars(&ui_sanitize(&other.to_string(), Lines::Single), 40),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn create(v: Value) -> CardDoc {
        match parse_input(&v) {
            Ok(UiInput::Create { doc, .. }) => doc,
            other => panic!("{v}: {other:?}"),
        }
    }

    fn err(v: Value) -> String {
        match parse_input(&v) {
            Err(e) => e.to_string(),
            Ok(ok) => panic!("{v} was accepted: {ok:?}"),
        }
    }

    fn opts(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("o{i}")).collect()
    }

    /// `build(n)` is accepted and `build(n + 1)` refused at `path`.
    #[track_caller]
    fn bound(n: usize, path: &str, build: impl Fn(usize) -> Value) {
        create(build(n));
        let e = err(build(n + 1));
        assert!(e.starts_with(&format!("{path}: ")), "N+1 = {}: {e}", n + 1);
    }

    /// `build(n)` is accepted and `build(n - 1)` refused at `path`.
    #[track_caller]
    fn floor(n: usize, path: &str, build: impl Fn(usize) -> Value) {
        create(build(n));
        let e = err(build(n - 1));
        assert!(e.starts_with(&format!("{path}: ")), "N-1 = {}: {e}", n - 1);
    }

    fn s(n: usize) -> String {
        "x".repeat(n)
    }

    #[test]
    fn question_bounds() {
        bound(
            QUESTION_MAX,
            "question",
            |n| json!({"question": s(n), "options": opts(2)}),
        );
        floor(
            1,
            "question",
            |n| json!({"question": s(n), "options": opts(2)}),
        );
        // Counted in characters, not bytes.
        bound(
            QUESTION_MAX,
            "question",
            |n| json!({"question": "\u{1F600}".repeat(n), "options": opts(2)}),
        );
    }

    #[test]
    fn detail_bounds() {
        bound(
            DETAIL_MAX,
            "detail",
            |n| json!({"question": "q", "detail": s(n), "options": opts(2)}),
        );
        bound(
            DETAIL_MAX_LINES,
            "detail",
            |n| json!({"question": "q", "detail": vec!["l"; n].join("\n"), "options": opts(2)}),
        );
        assert_eq!(
            err(json!({"question": "q", "detail": "\n".repeat(30), "options": opts(2)})),
            "detail: at most 30 lines (got 31)"
        );
    }

    #[test]
    fn option_bounds() {
        bound(
            OPTIONS_MAX,
            "options",
            |n| json!({"question": "q", "options": opts(n)}),
        );
        floor(
            OPTIONS_MIN,
            "options",
            |n| json!({"question": "q", "options": opts(n)}),
        );
        bound(
            OPTION_LABEL_MAX,
            "options[1]",
            |n| json!({"question": "q", "options": ["a", s(n)]}),
        );
        bound(
            OPTION_LABEL_MAX,
            "options[1].label",
            |n| json!({"question": "q", "options": ["a", {"label": s(n)}]}),
        );
        floor(
            1,
            "options[1].label",
            |n| json!({"question": "q", "options": ["a", {"label": s(n)}]}),
        );
        floor(
            1,
            "options[1]",
            |n| json!({"question": "q", "options": ["a", s(n)]}),
        );
        bound(
            HINT_MAX,
            "options[1].hint",
            |n| json!({"question": "q", "options": ["a", {"label": "b", "hint": s(n)}]}),
        );
        bound(
            NOTE_PROMPT_MAX,
            "options[1].note",
            |n| json!({"question": "q", "options": ["a", {"label": "b", "note": s(n)}]}),
        );
        floor(
            1,
            "options[1].note",
            |n| json!({"question": "q", "options": ["a", {"label": "b", "note": s(n)}]}),
        );
    }

    #[test]
    fn default_bounds() {
        bound(DEFAULT_MAX, "default", |n| {
            let o = opts(9);
            let d: Vec<String> = (0..n).map(|i| o[i % 9].clone()).collect();
            json!({"question": "q", "options": o, "multi": true, "default": d})
        });
        create(json!({"question": "q", "options": opts(2), "multi": true, "default": []}));
    }

    #[test]
    fn field_bounds() {
        let f = |id: String| json!({"id": id, "label": "L"});
        bound(
            FIELDS_MAX,
            "fields",
            |n| json!({"question": "q", "fields": (0..n).map(|i| f(format!("f{i}"))).collect::<Vec<_>>()}),
        );
        floor(
            FIELDS_MIN,
            "fields",
            |n| json!({"question": "q", "fields": (0..n).map(|i| f(format!("f{i}"))).collect::<Vec<_>>()}),
        );
        bound(
            FIELD_ID_MAX,
            "fields[0].id",
            |n| json!({"question": "q", "fields": [f(s(n))]}),
        );
        floor(
            1,
            "fields[0].id",
            |n| json!({"question": "q", "fields": [f(s(n))]}),
        );
        bound(
            FIELD_LABEL_MAX,
            "fields[0].label",
            |n| json!({"question": "q", "fields": [{"id": "a", "label": s(n)}]}),
        );
        floor(
            1,
            "fields[0].label",
            |n| json!({"question": "q", "fields": [{"id": "a", "label": s(n)}]}),
        );
        bound(
            PLACEHOLDER_MAX,
            "fields[0].placeholder",
            |n| json!({"question": "q", "fields": [{"id": "a", "label": "L", "placeholder": s(n)}]}),
        );
        bound(
            TEXT_MAX,
            "fields[0].value",
            |n| json!({"question": "q", "fields": [{"id": "a", "label": "L", "value": s(n)}]}),
        );
        bound(
            LONG_MAX,
            "fields[0].value",
            |n| json!({"question": "q", "fields": [{"id": "a", "label": "L", "type": "long", "value": s(n)}]}),
        );
    }

    #[test]
    fn field_id_pattern() {
        for ok in ["a", "a1", "a_b", "abcdefghijklmnopqrstuvwx"] {
            create(json!({"question": "q", "fields": [{"id": ok, "label": "L"}]}));
        }
        for bad in [
            "Version",
            "1a",
            "_a",
            "a-b",
            "a b",
            "é",
            "",
            "abcdefghijklmnopqrstuvwxy",
        ] {
            let e = err(json!({"question": "q", "fields": [{"id": bad, "label": "L"}]}));
            assert!(e.starts_with("fields[0].id: "), "{bad}: {e}");
        }
    }

    /// The input as a whole: 8 KiB at 8192 / 8193 bytes, depth 4 / 5.
    #[test]
    fn input_bytes_and_depth() {
        let at = |bytes: usize| {
            let mut v = json!({"question": "q", "options": ["a", "b"], "fields": [
                {"id": "a", "label": "L", "type": "long", "value": ""},
                {"id": "b", "label": "L", "type": "long", "value": ""},
                {"id": "c", "label": "L", "type": "long", "value": ""},
                {"id": "d", "label": "L", "type": "long", "value": ""},
                {"id": "e", "label": "L", "type": "long", "value": ""}]});
            let mut left = bytes - serde_json::to_vec(&v).unwrap().len();
            for i in 0..5 {
                let take = left.min(LONG_MAX);
                v["fields"][i]["value"] = json!(s(take));
                left -= take;
            }
            assert_eq!(left, 0);
            assert_eq!(serde_json::to_vec(&v).unwrap().len(), bytes);
            v
        };
        bound(INPUT_MAX_BYTES, "input", at);
        assert_eq!(
            err(at(8193)),
            "input: at most 8192 bytes (got 8193); shorten the detail or ask in plain text"
        );

        let deep4 = json!({"question": "q", "options": ["a", {"label": "b"}]});
        assert!(!deeper_than(&deep4, 4) && deeper_than(&deep4, 3));
        create(deep4);
        let deep5 = json!({"question": "q", "fields": [{"id": "a", "label": "L", "value": ["x"]}]});
        assert_eq!(err(deep5), "input: nested deeper than 4 levels");
    }

    #[test]
    fn unknown_keys_at_every_level() {
        assert_eq!(
            err(json!({"question": "q", "options": opts(2), "blocks": []})),
            "blocks: unknown parameter; use question, detail, options, multi, default, fields, wait or close"
        );
        assert_eq!(
            err(json!({"question": "q", "options": ["a", {"label": "b", "id": "x"}]})),
            "options[1].id: unknown key; an option takes label, hint and note"
        );
        assert_eq!(
            err(json!({"question": "q", "fields": [{"id": "a", "label": "L", "kind": "text"}]})),
            "fields[0].kind: unknown key; a field takes id, label, type, value, placeholder and optional"
        );
        // Case matters: `Question` is not `question`.
        assert!(err(json!({"Question": "q", "options": opts(2)}))
            .starts_with("Question: unknown parameter"));
    }

    #[test]
    fn duplicates() {
        assert_eq!(
            err(json!({"question": "q", "options": ["Yes", "no", "YES"]})),
            "options[2]: \"YES\" duplicates options[0]; labels must differ (ignoring case)"
        );
        // After ui_sanitize: an invisible character doesn't make a label new.
        assert_eq!(
            err(json!({"question": "q", "options": [{"label": "Approve"}, "Ap\u{200B}prove"]})),
            "options[1]: \"Approve\" duplicates options[0].label; labels must differ (ignoring case)"
        );
        assert_eq!(
            err(
                json!({"question": "q", "fields": [{"id": "a", "label": "A"}, {"id": "a", "label": "B"}]})
            ),
            "fields[1].id: \"a\" duplicates fields[0].id"
        );
        assert_eq!(
            err(
                json!({"question": "q", "options": ["a", "b"], "multi": true, "default": ["a", "a"]})
            ),
            "default[1]: \"a\" is listed twice"
        );
    }

    #[test]
    fn rules_beyond_the_schema() {
        let e = |v| err(v);
        assert_eq!(
            e(json!({"options": opts(2)})),
            "question: required: the one-line question to ask"
        );
        assert_eq!(
            e(json!({"question": "q"})),
            "options: a card needs options or fields (or both)"
        );
        assert_eq!(
            e(json!({"question": "q", "multi": true})),
            "options: a card needs options or fields (or both)"
        );
        assert_eq!(
            e(json!({"close": "s_3f2a91c0d4e2a871", "wait": false})),
            "close: give it alone, with no other parameter (got wait)"
        );
        assert_eq!(
            e(json!({"close": "s_3F2A"})),
            "close: must be the id of a card you created, like s_3f2a91c0d4e2a871"
        );
        assert_eq!(
            e(
                json!({"question": "q", "options": ["a", {"label": "b", "note": "Why?"}, {"label": "c", "note": "How?"}]})
            ),
            "options[2].note: only one option may ask for a note (options[1] already does)"
        );
        assert_eq!(
            e(
                json!({"question": "q", "multi": true, "options": ["a", {"label": "b", "note": "Why?"}]})
            ),
            "options[1].note: not allowed with multi; use a field for free text"
        );
        assert_eq!(
            e(json!({"question": "q", "options": ["a", "b"], "default": "c"})),
            "default: \"c\" is not one of the option labels"
        );
        assert_eq!(
            e(json!({"question": "q", "options": ["a", "b"], "default": "A"})),
            "default: \"A\" is not one of the option labels"
        );
        assert_eq!(
            e(json!({"question": "q", "options": ["a", "b"], "default": ["a"]})),
            "default: one option label (a list only when multi is true)"
        );
        assert_eq!(
            e(json!({"question": "q", "options": ["a", "b"], "multi": true, "default": "a"})),
            "default: a list of option labels when multi is true"
        );
        assert_eq!(
            e(json!({"question": "q", "fields": [{"id": "a", "label": "L"}], "default": "a"})),
            "default: needs options; it names the option you recommend"
        );
        assert_eq!(
            e(
                json!({"question": "q", "fields": [{"id": "a", "label": "L", "type": "check", "value": "yes"}]})
            ),
            "fields[0].value: a check takes true or false (got \"yes\")"
        );
        assert_eq!(
            e(json!({"question": "q", "fields": [{"id": "a", "label": "L", "value": true}]})),
            "fields[0].value: a text field takes a string (got true)"
        );
        assert_eq!(e(json!({"question": "q", "fields": [{"id": "a", "label": "L", "type": "check", "optional": true}]})), "fields[0].optional: a check is always answered; only text and long fields can be optional");
        assert_eq!(
            e(json!({"question": "q", "fields": [{"id": "a", "label": "L", "type": "date"}]})),
            "fields[0].type: must be text, long or check"
        );
        assert_eq!(
            e(json!({"question": "q", "options": opts(2), "wait": "yes"})),
            "wait: must be true or false"
        );
        assert_eq!(
            e(json!({"question": "q", "options": opts(2), "multi": 1})),
            "multi: must be true or false"
        );
        assert_eq!(
            e(json!({"question": 7, "options": opts(2)})),
            "question: must be a string"
        );
        assert_eq!(e(json!(["q"])), "input: must be a JSON object");
    }

    #[test]
    fn normalization_materializes_defaults() {
        let doc = create(
            json!({"question": "Release details", "options": ["a", {"label": "b", "hint": ""}],
            "fields": [{"id": "v", "label": "Version"}, {"id": "p", "label": "Publish", "type": "check", "placeholder": ""}],
            "detail": ""}),
        );
        assert_eq!(
            serde_json::to_value(&doc).unwrap(),
            json!({"question": "Release details", "options": [{"label": "a"}, {"label": "b"}], "multi": false,
                "fields": [{"id": "v", "label": "Version", "type": "text", "value": "", "optional": false},
                           {"id": "p", "label": "Publish", "type": "check", "value": false, "optional": false}]})
        );
        assert_eq!(doc.mode(), Mode::Form);
        doc.revalidate().unwrap();
    }

    /// Every string field: controls and bidi overrides are refused there,
    /// default-ignorables are dropped, ZWJ/ZWNJ kept, `\n` folded where the
    /// field is single-line.
    #[test]
    fn hostile_characters_in_every_string_field() {
        type Inject = fn(&str) -> Value;
        // (path, the text the injector puts before `x`, injector, multi-line)
        let sites: [(&str, &str, Inject, bool); 11] = [
            (
                "question",
                "Q",
                |x| json!({"question": format!("Q{x}"), "options": ["a", "b"]}),
                false,
            ),
            (
                "detail",
                "D",
                |x| json!({"question": "q", "detail": format!("D{x}"), "options": ["a", "b"]}),
                true,
            ),
            (
                "options[1]",
                "B",
                |x| json!({"question": "q", "options": ["a", format!("B{x}")]}),
                false,
            ),
            (
                "options[1].label",
                "B",
                |x| json!({"question": "q", "options": ["a", {"label": format!("B{x}")}]}),
                false,
            ),
            (
                "options[1].hint",
                "H",
                |x| json!({"question": "q", "options": ["a", {"label": "b", "hint": format!("H{x}")}]}),
                false,
            ),
            (
                "options[1].note",
                "N",
                |x| json!({"question": "q", "options": ["a", {"label": "b", "note": format!("N{x}")}]}),
                false,
            ),
            // The label is the default's clean form, so only the default holds the character.
            (
                "default",
                "b",
                |x| {
                    let label = ui_sanitize(&format!("b{x}"), Lines::Single);
                    json!({"question": "q", "options": ["a", label], "default": format!("b{x}")})
                },
                false,
            ),
            (
                "fields[0].label",
                "L",
                |x| json!({"question": "q", "fields": [{"id": "f", "label": format!("L{x}")}]}),
                false,
            ),
            (
                "fields[0].placeholder",
                "P",
                |x| json!({"question": "q", "fields": [{"id": "f", "label": "L", "placeholder": format!("P{x}")}]}),
                false,
            ),
            (
                "fields[0].value",
                "V",
                |x| json!({"question": "q", "fields": [{"id": "f", "label": "L", "value": format!("V{x}")}]}),
                false,
            ),
            (
                "fields[0].value",
                "V",
                |x| json!({"question": "q", "fields": [{"id": "f", "label": "L", "type": "long", "value": format!("V{x}")}]}),
                true,
            ),
        ];
        let refused = [
            '\u{0}', '\u{7}', '\u{1B}', '\r', '\u{7F}', '\u{85}', '\u{9B}', '\u{202A}', '\u{202B}',
            '\u{202C}', '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
        ];
        let dropped = [
            '\u{200B}',
            '\u{200E}',
            '\u{200F}',
            '\u{061C}',
            '\u{2028}',
            '\u{2029}',
            '\u{2060}',
            '\u{FEFF}',
            '\u{AD}',
            '\u{FE0F}',
            '\u{E0100}',
            '\u{E0041}',
            '\u{E007F}',
            '\u{180E}',
            '\u{3164}',
        ];
        // The string each site normalizes to, read back out of the doc.
        let read = |doc: &CardDoc, site: &str| -> String {
            match site {
                "question" => doc.question.clone(),
                "detail" => doc.detail.clone().unwrap_or_default(),
                "options[1]" | "options[1].label" => doc.options[1].label.clone(),
                "options[1].hint" => doc.options[1].hint.clone().unwrap_or_default(),
                "options[1].note" => doc.options[1].note.clone().unwrap_or_default(),
                "default" => match &doc.default {
                    Some(DefaultChoice::One(d)) => d.clone(),
                    _ => String::new(),
                },
                "fields[0].label" => doc.fields[0].label.clone(),
                "fields[0].placeholder" => doc.fields[0].placeholder.clone().unwrap_or_default(),
                "fields[0].value" => match &doc.fields[0].value {
                    FieldValue::Text(t) => t.clone(),
                    FieldValue::Check(_) => String::new(),
                },
                _ => unreachable!(),
            }
        };
        for (site, prefix, inject, multi_line) in sites {
            for c in refused {
                let e = err(inject(&format!("{c}x")));
                assert!(
                    e.starts_with(&format!("{site}: holds U+{:04X} (", c as u32)),
                    "{site} U+{:04X}: {e}",
                    c as u32
                );
            }
            for c in dropped {
                let doc = create(inject(&format!("{c}x")));
                assert_eq!(
                    read(&doc, site),
                    format!("{prefix}x"),
                    "{site} U+{:04X}",
                    c as u32
                );
            }
            let doc = create(inject("\u{200D}\u{200C}x"));
            assert_eq!(
                read(&doc, site),
                format!("{prefix}\u{200D}\u{200C}x"),
                "{site} keeps ZWJ/ZWNJ"
            );
            let doc = create(inject("\tx"));
            assert_eq!(read(&doc, site), format!("{prefix} x"), "{site} tab");
            let doc = create(inject("\nx"));
            let expect = if multi_line {
                format!("{prefix}\nx")
            } else {
                format!("{prefix} x")
            };
            assert_eq!(read(&doc, site), expect, "{site} newline");
            let url = "[x](https://e.test/?a=<b>)";
            assert_eq!(
                read(&create(inject(url)), site),
                format!("{prefix}{url}"),
                "{site}: URL-like text is kept as text"
            );
        }
    }

    #[test]
    fn only_invisible_is_empty() {
        assert_eq!(
            err(json!({"question": "\u{200B}\u{FEFF}", "options": opts(2)})),
            "question: must not be empty (it holds only invisible characters)"
        );
        assert_eq!(
            err(json!({"question": "", "options": opts(2)})),
            "question: must not be empty"
        );
    }

    #[test]
    fn modes() {
        assert_eq!(
            create(json!({"question": "q", "options": opts(3)})).mode(),
            Mode::Quick
        );
        assert_eq!(
            create(json!({"question": "q", "options": ["a", {"label": "b", "note": "?"}]})).mode(),
            Mode::Quick
        );
        assert_eq!(
            create(json!({"question": "q", "options": opts(3), "multi": true})).mode(),
            Mode::Form
        );
        assert_eq!(
            create(json!({"question": "q", "fields": [{"id": "a", "label": "L"}]})).mode(),
            Mode::Form
        );
        assert_eq!(
            create(
                json!({"question": "q", "options": opts(2), "fields": [{"id": "a", "label": "L"}]})
            )
            .mode(),
            Mode::Form
        );
    }

    #[test]
    fn wait_and_close() {
        assert_eq!(
            parse_input(&json!({"question": "q", "options": opts(2), "wait": true})).unwrap(),
            UiInput::Create {
                doc: create(json!({"question": "q", "options": opts(2)})),
                wait: true
            }
        );
        assert_eq!(
            parse_input(&json!({"close": "s_3f2a91c0d4e2a871"})).unwrap(),
            UiInput::Close("s_3f2a91c0d4e2a871".into())
        );
    }

    #[test]
    fn secret_footnote_is_advisory() {
        let doc =
            create(json!({"question": "q", "fields": [{"id": "t", "label": "GitHub token"}]}));
        assert!(doc.secret_footnote(), "flagged, and still a valid card");
        assert!(
            !create(json!({"question": "q", "fields": [{"id": "v", "label": "Version"}]}))
                .secret_footnote()
        );
    }

    #[test]
    fn stored_docs_refuse_unknown_keys_and_unnormalized_forms() {
        let bad: Result<CardDoc, _> = serde_json::from_value(
            json!({"question": "q", "options": [{"label": "a"}], "blocks": 1}),
        );
        assert!(bad.is_err());
        let doc: CardDoc = serde_json::from_value(
            json!({"question": "q\u{200B}", "options": [{"label": "a"}, {"label": "b"}]}),
        )
        .unwrap();
        assert_eq!(
            doc.revalidate().unwrap_err().to_string(),
            "doc: is not normalized"
        );
    }
}
