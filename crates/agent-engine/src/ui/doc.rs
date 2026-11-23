//! A `ui` input, validated and normalized into a [`CardDoc`] (§5.1).
//!
//! [`parse_input`] enforces the JSON Schema (`schema.json`) by hand, then
//! the rules beyond it, and reports the first failure as one teaching line
//! ([`UiError`]). The result is the stored, normalized doc:
//! - every string has been through `ui_sanitize` (single-line fields fold
//!   `\n` to a space);
//! - string options are `{label}`;
//! - `multi`, a field's `type`, `value` and `optional` are materialized;
//! - an empty optional string (`detail`, `hint`, `placeholder`) is absent;
//! - a `default` is a list with `multi` and one label without it, in the
//!   label's own spelling (v3.2 #4).
//!
//! Order of checks: depth, then bytes (counted, never built), so a value
//! of any size or depth is refused cheaply (Shady #439 M2). In a string,
//! `\r\n` and `\r` become `\n` before any check (v3.2 #1). A C0/C1 control
//! (other than `\n`/`\t`) or a bidi embedding, override or isolate is
//! refused, not rewritten (`sanitize::refused_char`). Length bounds apply to
//! the raw string (as JSON Schema does); "not empty", line counts and
//! duplicates to the sanitized one. Labels are compared by
//! `sanitize::fold_key`.
//!
//! Normalizing is a fixed point: [`revalidate`] checks that a stored doc is
//! exactly what normalizing makes of it. Create and revalidate measure the
//! same thing, the normalized doc against `DOC_MAX_BYTES`, so a doc that
//! was accepted always revalidates (Shady #439 H1).

use serde_json::{Map, Value};

pub use agent_core::core::ui_types::{
    CardDoc, CardField, CardOption, DefaultChoice, FieldKind, FieldValue, Mode,
};

use super::sanitize::{
    char_len, cut_chars, fold_key, looks_secret, normalize_newlines, refused_char, ui_sanitize,
    Lines,
};
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

/// The §4.7 footnote ("Don't type secrets here") applies: a field label
/// matches the advisory secret heuristic. Never a refusal (v3.2 #14).
pub fn secret_footnote(doc: &CardDoc) -> bool {
    doc.fields
        .iter()
        .any(|f| f.kind != FieldKind::Check && looks_secret(&f.label))
}

/// Check a doc read back from storage or the wire: at most
/// `DOC_MAX_BYTES`, and exactly what normalizing makes of it. The raw
/// input's 8 KiB cap is not applied: it bounds a call, not a doc.
pub fn revalidate(doc: &CardDoc) -> Result<(), UiError> {
    if json_len_within(doc, DOC_MAX_BYTES).is_none() {
        return Err(UiError::new("doc", format!("over {DOC_MAX_BYTES} bytes")));
    }
    let value = serde_json::to_value(doc).map_err(|e| UiError::new("doc", e.to_string()))?;
    let Some(obj) = value.as_object() else {
        return Err(UiError::new("doc", "must be an object"));
    };
    if normalize(obj)? == *doc {
        Ok(())
    } else {
        Err(UiError::new("doc", "is not normalized"))
    }
}

/// Validate and normalize one `ui` input.
pub fn parse_input(input: &Value) -> Result<UiInput, UiError> {
    if deeper_than(input, INPUT_MAX_DEPTH) {
        return Err(UiError::new(
            "input",
            format!("nested deeper than {INPUT_MAX_DEPTH} levels"),
        ));
    }
    if json_len_within(input, INPUT_MAX_BYTES).is_none() {
        return Err(UiError::new(
            "input",
            format!("over {INPUT_MAX_BYTES} bytes; shorten the detail or ask in plain text"),
        ));
    }
    let Some(obj) = input.as_object() else {
        return Err(UiError::new("input", "must be a JSON object"));
    };
    if let Some(k) = obj.keys().find(|k| !TOP_KEYS_SORTED.contains(&k.as_str())) {
        return Err(UiError::new(
            key_path(k),
            format!("unknown parameter; use {TOP_KEYS}"),
        ));
    }
    if let Some(close) = obj.get("close") {
        // v3.2 #5: a key at its schema default counts as absent.
        let at_default = |k: &str, v: &Value| matches!(k, "wait" | "multi") && *v == false;
        if let Some((other, _)) = obj.iter().find(|(k, v)| *k != "close" && !at_default(k, v)) {
            return Err(UiError::new(
                "close",
                format!(
                    "give it alone, with no other parameter (got {})",
                    key_path(other)
                ),
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
    let doc = normalize(obj)?;
    if json_len_within(&doc, DOC_MAX_BYTES).is_none() {
        // Unreachable for an input within its cap (`NORMALIZE_GROWTH_MAX`);
        // kept so create and revalidate can never disagree.
        return Err(UiError::new(
            "input",
            format!("normalized, the card is over {DOC_MAX_BYTES} bytes; shorten the detail or ask in plain text"),
        ));
    }
    Ok(UiInput::Create { doc, wait })
}

/// The doc an object with only known top-level keys describes.
fn normalize(obj: &Map<String, Value>) -> Result<CardDoc, UiError> {
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
    Ok(CardDoc {
        question,
        detail: non_empty(detail),
        options,
        multi,
        default,
        fields,
    })
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

/// A shown string: line endings, refused characters, the raw bound, then
/// `ui_sanitize` and the minimum on what is left.
fn string(
    v: &Value,
    path: &str,
    lines: Lines,
    min: usize,
    max: usize,
    advice: &str,
) -> Result<String, UiError> {
    let Some(raw) = v.as_str() else {
        return Err(UiError::new(path, "must be a string"));
    };
    let s = normalize_newlines(raw);
    if let Some((c, why)) = refused_char(&s) {
        return Err(UiError::new(
            path,
            format!("holds U+{:04X} ({why}); remove it", c as u32),
        ));
    }
    let n = char_len(&s);
    if n > max {
        return Err(UiError::new(
            path,
            format!("at most {max} characters (got {n}){advice}"),
        ));
    }
    let clean = ui_sanitize(&s, lines);
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
    let mut keys: Vec<(String, String)> = Vec::with_capacity(n);
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
                        format!("{path}.{}", key_path(k)),
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
        let key = fold_key(&option.label);
        if let Some((_, first)) = keys.iter().find(|(k, _)| *k == key) {
            return Err(UiError::new(
                label_path,
                format!(
                    "{} duplicates {first}; labels must differ (ignoring case)",
                    quote(&option.label),
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
        keys.push((key, label_path));
    }
    Ok(out)
}

/// v3.2 #4: matched to a label by `fold_key` and stored in the label's
/// spelling; a string with `multi` becomes `[label]`, a one-item list
/// without it the label.
fn default(v: &Value, options: &[CardOption], multi: bool) -> Result<DefaultChoice, UiError> {
    if options.is_empty() {
        return Err(UiError::new(
            "default",
            "needs options; it names the option you recommend",
        ));
    }
    let label = |v: &Value, path: &str| -> Result<String, UiError> {
        let given = string(v, path, Lines::Single, 0, usize::MAX, "")?;
        let key = fold_key(&given);
        match options.iter().find(|o| fold_key(&o.label) == key) {
            Some(o) => Ok(o.label.clone()),
            None => Err(UiError::new(
                path,
                format!("{} is not one of the option labels", quote(&given)),
            )),
        }
    };
    match (v, multi) {
        (Value::String(_), false) => Ok(DefaultChoice::One(label(v, "default")?)),
        (Value::String(_), true) => Ok(DefaultChoice::Many(vec![label(v, "default")?])),
        (Value::Array(items), false) => match items.as_slice() {
            [one] => Ok(DefaultChoice::One(label(one, "default[0]")?)),
            _ => Err(UiError::new(
                "default",
                "one option label (a list of more than one only when multi is true)",
            )),
        },
        (Value::Array(items), true) => {
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
                format!("{path}.{}", key_path(k)),
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
        let optional = flag(o, "optional")
            .map_err(|e| UiError::new(format!("{path}.optional"), e.message()))?;
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
            "input: over 8192 bytes; shorten the detail or ask in plain text"
        );

        let deep4 = json!({"question": "q", "options": ["a", {"label": "b"}]});
        assert!(!deeper_than(&deep4, 4) && deeper_than(&deep4, 3));
        create(deep4);
        let deep5 = json!({"question": "q", "fields": [{"id": "a", "label": "L", "value": ["x"]}]});
        assert_eq!(err(deep5), "input: nested deeper than 4 levels");
    }

    /// Shady #439 M2: depth and size are checked before anything walks or
    /// copies the whole value. Before, `serde_json::to_vec` ran first and
    /// overflowed the stack on this value.
    #[test]
    fn a_deep_or_huge_value_is_refused_before_it_is_serialized() {
        let mut deep = json!("x");
        for _ in 0..200_000 {
            deep = Value::Array(vec![deep]);
        }
        // Never cloned, dropped whole or put through `json!`: all recurse.
        let mut input = Map::new();
        input.insert("question".into(), json!("q"));
        input.insert("options".into(), deep);
        let input = Value::Object(input);
        assert_eq!(
            parse_input(&input).unwrap_err().to_string(),
            "input: nested deeper than 4 levels"
        );
        let mut v = input;
        loop {
            match v {
                Value::Object(mut o) => match o.remove("options") {
                    Some(inner) => v = inner,
                    None => break,
                },
                Value::Array(mut a) => match a.pop() {
                    Some(inner) => v = inner,
                    None => break,
                },
                _ => break,
            }
        }
        let huge = json!({"question": "q", "detail": "x".repeat(64 << 20), "options": ["a", "b"]});
        assert_eq!(
            err(huge),
            "input: over 8192 bytes; shorten the detail or ask in plain text"
        );
    }

    /// Shady #439 H1: the largest doc an accepted input normalizes to is
    /// over 8 KiB, and still revalidates, serialized and read back too.
    #[test]
    fn an_accepted_doc_always_revalidates() {
        for input in [worst_input(false), worst_input(true)] {
            assert_eq!(serde_json::to_vec(&input).unwrap().len(), INPUT_MAX_BYTES);
            let doc = create(input);
            let bytes = serde_json::to_vec(&doc).unwrap().len();
            assert!(bytes > INPUT_MAX_BYTES && bytes <= DOC_MAX_BYTES, "{bytes}");
            revalidate(&doc).unwrap();
            let back: CardDoc =
                serde_json::from_str(&serde_json::to_string(&doc).unwrap()).unwrap();
            assert_eq!(back, doc);
            revalidate(&back).unwrap();
        }
    }

    /// An input of exactly 8 KiB that grows the most when normalized: every
    /// option a string and every field bare, `multi` omitted and (with
    /// `kelvin`) a default of nine labels spelled in KELVIN SIGN, given in
    /// ASCII `k`.
    fn worst_input(kelvin: bool) -> Value {
        let k = "\u{212A}";
        let mut input = json!({
            "question": "q",
            "detail": "",
            "options": (0..OPTIONS_MAX).map(|i| i.to_string()).collect::<Vec<_>>(),
            "fields": (0..FIELDS_MAX).map(|i| json!({"id": format!("f{i}"), "label": "L"})).collect::<Vec<_>>(),
        });
        if kelvin {
            input["multi"] = json!(true);
            for i in 0..OPTIONS_MAX {
                input["options"][i] = json!(format!("{i}{}", k.repeat(OPTION_LABEL_MAX - 1)));
            }
            input["default"] = (0..DEFAULT_MAX)
                .map(|i| format!("{i}{}", "k".repeat(OPTION_LABEL_MAX - 1)))
                .collect();
        }
        let size = |v: &Value| serde_json::to_vec(v).unwrap().len();
        let mut left = INPUT_MAX_BYTES - size(&input);
        let mut slots: Vec<(String, &str, usize)> = vec![
            ("/detail".into(), "", DETAIL_MAX),
            ("/question".into(), "q", QUESTION_MAX),
        ];
        let digits = ["0", "1", "2", "3", "4", "5", "6", "7", "8"];
        if !kelvin {
            slots.extend(
                (0..OPTIONS_MAX).map(|i| (format!("/options/{i}"), digits[i], OPTION_LABEL_MAX)),
            );
        }
        slots.extend((0..FIELDS_MAX).map(|i| (format!("/fields/{i}/label"), "L", FIELD_LABEL_MAX)));
        for (ptr, prefix, cap) in &slots {
            let n = (left / 4).min(cap - prefix.chars().count());
            *input.pointer_mut(ptr).unwrap() = json!(format!("{prefix}{}", "\u{10FFFD}".repeat(n)));
            left -= 4 * n;
        }
        // The last bytes in ASCII, in the first slot with room, so the
        // input is exactly at its cap.
        let chars = |v: &Value| v.as_str().unwrap().chars().count();
        let (ptr, _, _) = slots
            .iter()
            .find(|(ptr, _, cap)| cap - chars(input.pointer(ptr).unwrap()) >= left)
            .unwrap();
        let slot = input.pointer_mut(ptr).unwrap();
        *slot = json!(format!("{}{}", slot.as_str().unwrap(), "x".repeat(left)));
        input
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

    /// Shady #439 M1: a hostile key at any level, or beside `close`, is
    /// written escaped and cut; the refusal stays one safe, short line.
    #[test]
    fn hostile_keys_in_refusals() {
        let k = "\u{1b}[31m\n\u{202E}forged\u{200B}";
        let shown = r#""\u001b[31m\u000a\u202eforged\u200b""#;
        let long = "k".repeat(7000);
        let cut = format!("\"{}…\"", "k".repeat(24));
        for (key, shown) in [(k, shown.to_string()), (long.as_str(), cut)] {
            let mut top = json!({"question": "q", "options": ["a", "b"]});
            top[key] = json!(1);
            let mut opt = json!({"question": "q", "options": ["a", {"label": "b"}]});
            opt["options"][1][key] = json!(1);
            let mut field = json!({"question": "q", "fields": [{"id": "a", "label": "L"}]});
            field["fields"][0][key] = json!(1);
            let mut close = json!({"close": "s_3f2a91c0d4e2a871"});
            close[key] = json!(1);
            let want = [
                format!("{shown}: unknown parameter; use {TOP_KEYS}"),
                format!("options[1].{shown}: unknown key; an option takes label, hint and note"),
                format!("fields[0].{shown}: unknown key; a field takes id, label, type, value, placeholder and optional"),
                format!("{shown}: unknown parameter; use {TOP_KEYS}"),
            ];
            for (v, want) in [top, opt, field, close].into_iter().zip(want) {
                let e = err(v);
                assert_eq!(e, want);
                assert!(crate::ui::tests::safe_line(&e), "{e:?}");
            }
        }
    }

    #[test]
    fn duplicates() {
        assert_eq!(
            err(json!({"question": "q", "options": ["Yes", "no", "YES"]})),
            "options[2]: \"YES\" duplicates options[0]; labels must differ (ignoring case)"
        );
        // Unicode case folding, not lowercase (Shady #439 M3).
        assert_eq!(
            err(json!({"question": "q", "options": ["\u{3A3}", "\u{3C2}"]})),
            "options[1]: \"\u{3C2}\" duplicates options[0]; labels must differ (ignoring case)"
        );
        assert_eq!(
            err(json!({"question": "q", "options": ["Stra\u{DF}e", "STRASSE"]})),
            "options[1]: \"STRASSE\" duplicates options[0]; labels must differ (ignoring case)"
        );
        // Not canonical equivalence: é and e + U+0301 are two labels.
        create(json!({"question": "q", "options": ["\u{E9}", "e\u{301}"]}));
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
            e(json!({"close": "s_3f2a91c0d4e2a871", "wait": true})),
            "close: give it alone, with no other parameter (got wait)"
        );
        assert_eq!(
            e(json!({"close": "s_3f2a91c0d4e2a871", "multi": "false"})),
            "close: give it alone, with no other parameter (got multi)"
        );
        assert_eq!(
            e(json!({"close": "s_3f2a91c0d4e2a871", "detail": ""})),
            "close: give it alone, with no other parameter (got detail)"
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
            e(json!({"question": "q", "options": ["a", "b"], "default": ["a", "b"]})),
            "default: one option label (a list of more than one only when multi is true)"
        );
        assert_eq!(
            e(json!({"question": "q", "options": ["a", "b"], "default": []})),
            "default: one option label (a list of more than one only when multi is true)"
        );
        assert_eq!(
            e(json!({"question": "q", "options": ["a", "b"], "default": ["c"]})),
            "default[0]: \"c\" is not one of the option labels"
        );
        assert_eq!(
            e(
                json!({"question": "q", "options": ["a", "b"], "multi": true, "default": ["a", "A"]})
            ),
            "default[1]: \"a\" is listed twice"
        );
        assert_eq!(
            e(json!({"question": "q", "options": ["a", "b"], "default": 1})),
            "default: must be an option label"
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

    /// v3.2 #4: `default` follows `multi`, matched ignoring case, stored in
    /// the label's spelling.
    #[test]
    fn default_is_coerced_and_matched_ignoring_case() {
        let d = |v: Value| create(v).default;
        let one = |s: &str| Some(DefaultChoice::One(s.into()));
        let many = |s: &[&str]| {
            Some(DefaultChoice::Many(
                s.iter().map(|s| s.to_string()).collect(),
            ))
        };
        let opts = json!(["Yes", "No", "Stra\u{DF}e"]);
        assert_eq!(
            d(json!({"question": "q", "options": opts, "default": "yes"})),
            one("Yes")
        );
        assert_eq!(
            d(json!({"question": "q", "options": opts, "default": ["NO"]})),
            one("No")
        );
        assert_eq!(
            d(json!({"question": "q", "options": opts, "default": "STRASSE"})),
            one("Stra\u{DF}e")
        );
        assert_eq!(
            d(json!({"question": "q", "options": opts, "multi": true, "default": "yes"})),
            many(&["Yes"])
        );
        assert_eq!(
            d(json!({"question": "q", "options": opts, "multi": true, "default": ["no", "YES"]})),
            many(&["No", "Yes"])
        );
        assert_eq!(
            d(json!({"question": "q", "options": opts, "multi": true, "default": []})),
            many(&[])
        );
        // The stored form is a fixed point.
        let doc =
            create(json!({"question": "q", "options": opts, "multi": true, "default": "yes"}));
        revalidate(&doc).unwrap();
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
        revalidate(&doc).unwrap();
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
            '\u{0}', '\u{7}', '\u{1B}', '\u{7F}', '\u{85}', '\u{9B}', '\u{202A}', '\u{202B}',
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
            let expect = if multi_line {
                format!("{prefix}\nx")
            } else {
                format!("{prefix} x")
            };
            // v3.2 #1: CRLF and a lone CR are line endings, never refused.
            for nl in ["\n", "\r\n", "\r"] {
                let doc = create(inject(&format!("{nl}x")));
                assert_eq!(read(&doc, site), expect, "{site} {nl:?}");
            }
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
        // v3.2 #5: beside close, wait:false and multi:false count as absent.
        for v in [
            json!({"close": "s_3f2a91c0d4e2a871"}),
            json!({"close": "s_3f2a91c0d4e2a871", "wait": false}),
            json!({"close": "s_3f2a91c0d4e2a871", "multi": false, "wait": false}),
        ] {
            assert_eq!(
                parse_input(&v).unwrap(),
                UiInput::Close("s_3f2a91c0d4e2a871".into()),
                "{v}"
            );
        }
    }

    #[test]
    fn secret_footnote_is_advisory() {
        let doc =
            create(json!({"question": "q", "fields": [{"id": "t", "label": "GitHub token"}]}));
        assert!(secret_footnote(&doc), "flagged, and still a valid card");
        assert!(!secret_footnote(&create(
            json!({"question": "q", "fields": [{"id": "v", "label": "Version"}]})
        )));
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
            revalidate(&doc).unwrap_err().to_string(),
            "doc: is not normalized"
        );
        // Over the doc cap: refused before anything else.
        let mut big = create(json!({"question": "q", "options": opts(2)}));
        big.detail = Some("x".repeat(DOC_MAX_BYTES));
        assert_eq!(
            revalidate(&big).unwrap_err().to_string(),
            format!("doc: over {DOC_MAX_BYTES} bytes")
        );
    }
}
