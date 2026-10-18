//! Opt-in clock and file-mtime placeholders for `multi_edit`.
//!
//! The clock is read once per call (by the caller) and every expansion uses
//! that one instant, so a timestamp written into two files is identical.
//! Each mtime path is stat-ed at most once per call. Every error names the
//! placeholder; an unknown `{{...}}` is never left in the text silently.

use chrono::{DateTime, Local};
use serde_json::Value;
use std::collections::HashMap;

pub(crate) fn iso(_t: &DateTime<Local>) -> String {
    String::new()
}

pub(crate) fn format_checked(_t: &DateTime<Local>, _fmt: &str) -> Result<String, String> {
    Err("not implemented".into())
}

pub(crate) struct Expander {
    #[allow(dead_code)]
    now: DateTime<Local>,
    #[allow(dead_code)]
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
        String::new()
    }

    pub(crate) fn expand(&mut self, _s: &str) -> Result<String, String> {
        Err("not implemented".into())
    }

    pub(crate) fn expand_value(&mut self, _value: &mut Value) -> Result<(), String> {
        Err("not implemented".into())
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
