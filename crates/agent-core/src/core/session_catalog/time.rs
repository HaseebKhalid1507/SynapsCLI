//! Local-time context. Injected (never read ad hoc) so facets, day groups and
//! relative query dates are deterministic in tests.

use chrono::{DateTime, Datelike, Duration, FixedOffset, Local, NaiveDate, TimeZone, Utc};

/// "Now" plus the zone used for calendar maths.
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    pub now: DateTime<Utc>,
    /// `None` = the system local zone (DST-aware).
    fixed: Option<FixedOffset>,
}

impl Clock {
    /// The real clock in the system local zone.
    pub fn local_now() -> Self {
        Self {
            now: Utc::now(),
            fixed: None,
        }
    }

    /// A fixed instant in a fixed zone (tests).
    pub fn fixed(now: DateTime<Utc>, offset: FixedOffset) -> Self {
        Self {
            now,
            fixed: Some(offset),
        }
    }

    /// Calendar date of `t` in this zone.
    pub fn date_of(&self, t: DateTime<Utc>) -> NaiveDate {
        match self.fixed {
            Some(off) => t.with_timezone(&off).date_naive(),
            None => t.with_timezone(&Local).date_naive(),
        }
    }

    pub fn today(&self) -> NaiveDate {
        self.date_of(self.now)
    }

    /// UTC instant of local midnight starting `d` (DST gaps: earliest valid).
    pub fn start_of(&self, d: NaiveDate) -> DateTime<Utc> {
        let naive = d.and_hms_opt(0, 0, 0).expect("midnight is valid");
        match self.fixed {
            Some(off) => off
                .from_local_datetime(&naive)
                .earliest()
                .map(|t| t.with_timezone(&Utc)),
            None => Local
                .from_local_datetime(&naive)
                .earliest()
                .map(|t| t.with_timezone(&Utc)),
        }
        .unwrap_or_else(|| Utc.from_utc_datetime(&naive))
    }

    /// Bucket of `t`: rolling, exclusive windows by local calendar day —
    /// today, the 6 days before (`This week`), the 23 before that
    /// (`This month`, 30 days total), everything else. Future timestamps
    /// (clock skew) count as today.
    pub fn when(&self, t: DateTime<Utc>) -> When {
        let days = (self.today() - self.date_of(t)).num_days();
        match days {
            i64::MIN..=0 => When::Today,
            1..=6 => When::ThisWeek,
            7..=29 => When::ThisMonth,
            _ => When::Older,
        }
    }

    /// Day-header label: `Today`, `Yesterday`, `Oct 21`, or `Oct 21 2025`
    /// when the year differs.
    pub fn day_label(&self, d: NaiveDate) -> String {
        let today = self.today();
        if d >= today {
            "Today".into()
        } else if d == today - Duration::days(1) {
            "Yesterday".into()
        } else if d.year() == today.year() {
            d.format("%b %-d").to_string()
        } else {
            d.format("%b %-d %Y").to_string()
        }
    }
}

/// The When facet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum When {
    Today,
    ThisWeek,
    ThisMonth,
    Older,
}

impl When {
    pub const ALL: [When; 4] = [When::Today, When::ThisWeek, When::ThisMonth, When::Older];

    pub fn label(self) -> &'static str {
        match self {
            When::Today => "Today",
            When::ThisWeek => "This week",
            When::ThisMonth => "This month",
            When::Older => "Older",
        }
    }
}
