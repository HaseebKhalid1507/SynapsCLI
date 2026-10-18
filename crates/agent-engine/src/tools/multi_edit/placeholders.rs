//! Opt-in clock and file-mtime placeholders for `multi_edit`.
//!
//! The clock is read once per call (by the caller) and every expansion uses
//! that one instant, so a timestamp written into two files is identical.
//! Each mtime path is stat-ed at most once per call. Every error names the
//! placeholder; an unknown `{{...}}` is never left in the text silently.

use chrono::{DateTime, Local};
use serde_json::Value;
use std::collections::HashMap;

/// `date -Iseconds` shape: `2026-10-18T11:23:45-05:00`.
pub(crate) fn iso(t: &DateTime<Local>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

/// Format with a user strftime string without ever panicking: chrono's
/// `format(...).to_string()` panics on an invalid specifier, so the items
/// are validated first and the write error is mapped, not unwrapped.
pub(crate) fn format_checked(t: &DateTime<Local>, fmt: &str) -> Result<String, String> {
    use chrono::format::{Item, StrftimeItems};
    use std::fmt::Write;
    if fmt.is_empty() {
        return Err("empty strftime format".into());
    }
    let items: Vec<Item<'_>> = StrftimeItems::new(fmt).collect();
    if items.iter().any(|item| matches!(item, Item::Error)) {
        return Err(format!("invalid strftime format {fmt:?}"));
    }
    let mut out = String::new();
    write!(out, "{}", t.format_with_items(items.into_iter()))
        .map_err(|_| format!("strftime format {fmt:?} cannot format this time"))?;
    Ok(out)
}

pub(crate) struct Expander {
    now: DateTime<Local>,
    mtimes: HashMap<String, DateTime<Local>>,
}

impl Expander {
    pub(crate) fn new(now: DateTime<Local>) -> Self {
        Self {
            now,
            mtimes: HashMap::new(),
        }
    }

    pub(crate) fn now_iso(&self) -> String {
        iso(&self.now)
    }

    /// Expand every `{{...}}` in `s`. `{{{{` is a literal `{{`; a `}}`
    /// without an opener is ordinary text.
    pub(crate) fn expand(&mut self, s: &str) -> Result<String, String> {
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        while let Some(i) = rest.find("{{") {
            out.push_str(&rest[..i]);
            let after = &rest[i + 2..];
            if let Some(tail) = after.strip_prefix("{{") {
                out.push_str("{{");
                rest = tail;
                continue;
            }
            let Some(j) = after.find("}}") else {
                return Err("unterminated placeholder: '{{' without a closing '}}' (write '{{{{' for a literal '{{')".into());
            };
            out.push_str(&self.resolve(&after[..j])?);
            rest = &after[j + 2..];
        }
        out.push_str(rest);
        Ok(out)
    }

    /// Expand the string leaves of a JSON value (keys are left alone). The
    /// value is serialised afterwards, so escaping is always correct.
    pub(crate) fn expand_value(&mut self, value: &mut Value) -> Result<(), String> {
        match value {
            Value::String(s) => *s = self.expand(s)?,
            Value::Array(items) => {
                for item in items {
                    self.expand_value(item)?;
                }
            }
            Value::Object(map) => {
                for item in map.values_mut() {
                    self.expand_value(item)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn resolve(&mut self, token: &str) -> Result<String, String> {
        if token == "now_iso" {
            return Ok(iso(&self.now));
        }
        if let Some(fmt) = token.strip_prefix("now:") {
            return format_checked(&self.now, fmt).map_err(|e| format!("{{{{{token}}}}}: {e}"));
        }
        if let Some(path) = token.strip_prefix("mtime_iso:") {
            let t = self.mtime(path).map_err(|e| format!("{{{{{token}}}}}: {e}"))?;
            return Ok(iso(&t));
        }
        if let Some(rest) = token.strip_prefix("mtime:") {
            // FMT runs up to the first ":/" (the path must be absolute).
            let Some(k) = rest.find(":/") else {
                return Err(format!(
                    "{{{{{token}}}}}: expected mtime:FMT:/absolute/path"
                ));
            };
            let (fmt, path) = (&rest[..k], &rest[k + 1..]);
            let t = self.mtime(path).map_err(|e| format!("{{{{{token}}}}}: {e}"))?;
            return format_checked(&t, fmt).map_err(|e| format!("{{{{{token}}}}}: {e}"));
        }
        Err(format!(
            "unknown placeholder {{{{{token}}}}} (known: now:FMT, now_iso, mtime:FMT:/path, mtime_iso:/path)"
        ))
    }

    fn mtime(&mut self, path: &str) -> Result<DateTime<Local>, String> {
        if !std::path::Path::new(path).is_absolute() {
            return Err(format!("mtime path {path:?} must be absolute"));
        }
        if let Some(t) = self.mtimes.get(path) {
            return Ok(*t);
        }
        let modified = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .map_err(|e| format!("cannot read the mtime of {path}: {e}"))?;
        let t = DateTime::<Local>::from(modified);
        self.mtimes.insert(path.to_string(), t);
        Ok(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn fixed() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 10, 18, 9, 5, 7).single().unwrap()
    }

    #[test]
    fn now_and_now_iso_use_the_one_clock_reading() {
        let mut e = Expander::new(fixed());
        let out = e.expand("## r2 ({{now:%H:%M}}) at {{now_iso}}").unwrap();
        assert_eq!(out, format!("## r2 (09:05) at {}", iso(&fixed())));
        assert!(iso(&fixed()).starts_with("2026-10-18T09:05:07"));
    }

    #[test]
    fn unknown_and_unterminated_placeholders_are_errors() {
        let mut e = Expander::new(fixed());
        let err = e.expand("a {{nowiso}} b").unwrap_err();
        assert!(err.contains("unknown placeholder {{nowiso}}"), "{err}");
        let err = e.expand("a {{now_iso b").unwrap_err();
        assert!(err.contains("unterminated"), "{err}");
    }

    #[test]
    fn bad_strftime_is_an_error_not_a_panic() {
        let mut e = Expander::new(fixed());
        for bad in ["{{now:%Q}}", "{{now:%}}", "{{now:}}"] {
            let err = e.expand(bad).unwrap_err();
            assert!(err.contains("strftime"), "{bad}: {err}");
        }
    }

    #[test]
    fn literal_braces_pass_through() {
        let mut e = Expander::new(fixed());
        assert_eq!(e.expand("{{{{now_iso}}").unwrap(), "{{now_iso}}");
        assert_eq!(e.expand("map[k]}} and }} end").unwrap(), "map[k]}} and }} end");
        assert_eq!(e.expand("no braces $(x) `y`").unwrap(), "no braces $(x) `y`");
    }

    #[test]
    fn mtime_is_read_once_per_path_and_must_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("gate.txt");
        std::fs::write(&f, "x").unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        std::fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let p = f.to_str().unwrap();
        let mut e = Expander::new(fixed());
        let first = e.expand(&format!("{{{{mtime_iso:{p}}}}}")).unwrap();
        assert_eq!(first, iso(&DateTime::<Local>::from(old)));
        // Touch the file: the same call still sees the first reading.
        std::fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(old + std::time::Duration::from_secs(3600))
            .unwrap();
        let again = e.expand(&format!("{{{{mtime:%Y-%m-%dT%H:%M:%S%:z:{p}}}}}")).unwrap();
        assert_eq!(again, first);

        let missing = tmp.path().join("missing").to_str().unwrap().to_string();
        let err = e.expand(&format!("{{{{mtime_iso:{missing}}}}}")).unwrap_err();
        assert!(err.contains("cannot read the mtime"), "{err}");
        let err = e.expand("{{mtime_iso:relative/path}}").unwrap_err();
        assert!(err.contains("must be absolute"), "{err}");
    }

    #[test]
    fn json_string_leaves_expand_and_stay_valid_json() {
        let mut e = Expander::new(fixed());
        let mut v = serde_json::json!({"ts": "{{now_iso}}", "note": "say \"{{now:%H}}\"\n", "n": 3,
                                       "{{now_iso}}": ["{{now:%M}}"]});
        e.expand_value(&mut v).unwrap();
        let row = serde_json::to_string(&v).unwrap();
        let back: Value = serde_json::from_str(&row).unwrap();
        assert_eq!(back["note"], "say \"09\"\n");
        assert_eq!(back["{{now_iso}}"][0], "05", "keys are never expanded");
    }
}
