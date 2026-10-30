//! Past-tab query language. Never errors: any token that does not parse as
//! a filter is kept as free text.
//!
//! * free words — case-insensitive, ALL must appear in title / name / id /
//!   cwd / first + last user message. `"two words"` is one term.
//! * `cwd:x` `model:x` `name:x` — case-insensitive substring
//!   (`model:` matches the provider-stripped key, so `model:opus-4-6`
//!   matches both `anthropic/claude-opus-4-6` and `claude-opus-4-6`).
//! * `is:named`
//! * `$>5` `$<1` — cost; `msgs>20` `msgs<5` — message count (sessions with
//!   an unknown count never match a `msgs` bound).
//! * `after:D` — active on/after D (`updated >= start of D`);
//!   `before:D` — started before D (`created < start of D`). Together they
//!   mean "overlaps the window". D: `oct-20`, `2026-10-20`, `3d`, `1w`,
//!   `today`, `yesterday`. A month-day in the future means last year.

use super::time::Clock;
use super::{model_key, CatalogEntry};
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Query {
    /// Lower-cased free terms (all must match).
    pub terms: Vec<String>,
    pub cwd: Vec<String>,
    pub model: Vec<String>,
    pub name: Vec<String>,
    pub named: bool,
    pub cost_gt: Option<f64>,
    pub cost_lt: Option<f64>,
    pub msgs_gt: Option<usize>,
    pub msgs_lt: Option<usize>,
    pub after: Option<DateBound>,
    pub before: Option<DateBound>,
}

/// A resolved date token: the local day and its UTC start instant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DateBound {
    pub date: NaiveDate,
    pub start: DateTime<Utc>,
}

impl Query {
    pub fn is_empty(&self) -> bool {
        *self == Query::default()
    }

    /// Does `e` satisfy every term and filter?
    pub fn matches(&self, e: &CatalogEntry) -> bool {
        let blob = e.search_blob();
        if !self.terms.iter().all(|t| blob.contains(t.as_str())) {
            return false;
        }
        if !self.cwd.is_empty() {
            let Some(cwd) = &e.cwd else { return false };
            let raw = cwd.to_string_lossy().to_lowercase();
            let tl = super::tilde(cwd).to_lowercase();
            if !self
                .cwd
                .iter()
                .all(|c| raw.contains(c.as_str()) || tl.contains(c.as_str()))
            {
                return false;
            }
        }
        if !self.model.is_empty() {
            let m = model_key(&e.model).to_lowercase();
            if !self.model.iter().all(|q| m.contains(model_key(q))) {
                return false;
            }
        }
        if !self.name.is_empty() {
            let Some(n) = &e.name else { return false };
            let n = n.to_lowercase();
            if !self.name.iter().all(|q| n.contains(q.as_str())) {
                return false;
            }
        }
        if self.named && e.name.is_none() {
            return false;
        }
        if self.cost_gt.is_some_and(|v| e.cost <= v) || self.cost_lt.is_some_and(|v| e.cost >= v) {
            return false;
        }
        if let Some(v) = self.msgs_gt {
            if !matches!(e.message_count, Some(n) if n > v) {
                return false;
            }
        }
        if let Some(v) = self.msgs_lt {
            if !matches!(e.message_count, Some(n) if n < v) {
                return false;
            }
        }
        if self.after.is_some_and(|b| e.updated_at < b.start) {
            return false;
        }
        if self.before.is_some_and(|b| e.created_at >= b.start) {
            return false;
        }
        true
    }
}

/// Parse a query string. Total: bad tokens become free text.
pub fn parse_query(input: &str, clock: &Clock) -> Query {
    let mut q = Query::default();
    for tok in tokenize(input) {
        if tok.quoted {
            push_term(&mut q, &tok.text);
            continue;
        }
        if !apply_token(&mut q, &tok.text, clock) {
            push_term(&mut q, &tok.text);
        }
    }
    q
}

fn push_term(q: &mut Query, t: &str) {
    let t = t.trim().to_lowercase();
    if !t.is_empty() {
        q.terms.push(t);
    }
}

struct Tok {
    text: String,
    quoted: bool,
}

/// Whitespace split with `"..."` grouping; an unterminated quote runs to
/// the end of input.
fn tokenize(s: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        if c == '"' {
            chars.next();
            let text: String = chars.by_ref().take_while(|c| *c != '"').collect();
            out.push(Tok { text, quoted: true });
            continue;
        }
        let mut text = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() {
                break;
            }
            text.push(c);
            chars.next();
        }
        out.push(Tok {
            text,
            quoted: false,
        });
    }
    out
}

/// Try `tok` as a filter; `false` = not a (valid) filter.
fn apply_token(q: &mut Query, tok: &str, clock: &Clock) -> bool {
    let lower = tok.to_lowercase();
    if let Some((key, val)) = lower.split_once(':') {
        if val.is_empty() {
            return false;
        }
        match key {
            "cwd" => q.cwd.push(val.to_string()),
            "model" => q.model.push(val.to_string()),
            "name" => q.name.push(val.to_string()),
            "is" if val == "named" => q.named = true,
            "after" => match parse_date(val, clock) {
                Some(d) => q.after = Some(d),
                None => return false,
            },
            "before" => match parse_date(val, clock) {
                Some(d) => q.before = Some(d),
                None => return false,
            },
            _ => return false,
        }
        return true;
    }
    if let Some(rest) = lower.strip_prefix('$') {
        return match cmp(rest).and_then(|(op, v)| Some((op, v.parse::<f64>().ok()?))) {
            Some(('>', v)) if v.is_finite() => {
                q.cost_gt = Some(v);
                true
            }
            Some(('<', v)) if v.is_finite() => {
                q.cost_lt = Some(v);
                true
            }
            _ => false,
        };
    }
    if let Some(rest) = lower.strip_prefix("msgs") {
        return match cmp(rest).and_then(|(op, v)| Some((op, v.parse::<usize>().ok()?))) {
            Some(('>', v)) => {
                q.msgs_gt = Some(v);
                true
            }
            Some(('<', v)) => {
                q.msgs_lt = Some(v);
                true
            }
            _ => false,
        };
    }
    false
}

fn cmp(s: &str) -> Option<(char, &str)> {
    let mut it = s.chars();
    let op = it.next()?;
    matches!(op, '>' | '<')
        .then(|| (op, it.as_str()))
        .filter(|(_, v)| !v.is_empty())
}

/// `oct-20` · `2026-10-20` · `3d` · `1w` · `today` · `yesterday`.
pub(crate) fn parse_date(s: &str, clock: &Clock) -> Option<DateBound> {
    let today = clock.today();
    let date = match s {
        "today" => today,
        "yesterday" => today - Duration::days(1),
        _ => {
            if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                d
            } else if let Some(d) = relative(s, today) {
                d
            } else {
                month_day(s, today)?
            }
        }
    };
    Some(DateBound {
        date,
        start: clock.start_of(date),
    })
}

fn relative(s: &str, today: NaiveDate) -> Option<NaiveDate> {
    let (num, unit) = s.split_at(s.len().checked_sub(1)?);
    let n: i64 = num.parse().ok().filter(|n| (0..=36_500).contains(n))?;
    let days = match unit {
        "d" => n,
        "w" => n * 7,
        _ => return None,
    };
    today.checked_sub_signed(Duration::days(days))
}

fn month_day(s: &str, today: NaiveDate) -> Option<NaiveDate> {
    let (m, d) = s.split_once(['-', '/', ' '])?;
    let month = match m.get(..3)? {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };
    let day: u32 = d.parse().ok()?;
    let this_year = NaiveDate::from_ymd_opt(today.year(), month, day)?;
    if this_year > today {
        NaiveDate::from_ymd_opt(today.year() - 1, month, day)
    } else {
        Some(this_year)
    }
}
