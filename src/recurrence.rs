//! Recurrence-rule parsing + cross-protocol translators.
//!
//! The fixture authors recurrence as a raw RRULE string (RFC 5545
//! `RRULE` value, without the leading `RRULE:` prefix - e.g.
//! `"FREQ=WEEKLY;BYDAY=MO,WE,FR;COUNT=10"`). CalDAV and Google
//! Calendar v3 carry that string verbatim on the wire; JMAP
//! (JSCalendar) and Microsoft Graph want structured shapes, so we
//! parse the RRULE into a [`ParsedRule`] and project each layer
//! accordingly.
//!
//! Scope of the parser is limited to what ratatoskr's recurrence
//! sync code actually exercises: FREQ (DAILY/WEEKLY/MONTHLY/YEARLY),
//! INTERVAL, COUNT, UNTIL, BYDAY, BYMONTHDAY, BYMONTH. Anything else
//! (BYSETPOS, BYHOUR, BYMINUTE, BYWEEKNO, WKST, ...) is parsed but
//! silently dropped from the structured translations; the raw
//! string keeps the data on CalDAV / gcal so a fixture can still
//! author them for those paths. Stage 2 of CalDAV recurrence (when
//! a fixture forces it) extends the structured translators.

use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frequency {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

impl Frequency {
    fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "DAILY" => Some(Self::Daily),
            "WEEKLY" => Some(Self::Weekly),
            "MONTHLY" => Some(Self::Monthly),
            "YEARLY" => Some(Self::Yearly),
            _ => None,
        }
    }

    pub fn jscalendar(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
            Self::Yearly => "yearly",
        }
    }
}

/// One BYDAY entry. RFC 5545 allows an optional ordinal prefix
/// (e.g. `2MO` = "the second Monday"); we keep it as a parsed
/// `(ordinal, weekday)` pair so the structured translators can
/// surface it where the target schema supports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByDay {
    pub ordinal: Option<i32>,
    pub weekday: Weekday,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weekday {
    Mo,
    Tu,
    We,
    Th,
    Fr,
    Sa,
    Su,
}

impl Weekday {
    fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "MO" => Some(Self::Mo),
            "TU" => Some(Self::Tu),
            "WE" => Some(Self::We),
            "TH" => Some(Self::Th),
            "FR" => Some(Self::Fr),
            "SA" => Some(Self::Sa),
            "SU" => Some(Self::Su),
            _ => None,
        }
    }

    /// Graph schema: full English lowercase ("monday", ...).
    pub fn graph(self) -> &'static str {
        match self {
            Self::Mo => "monday",
            Self::Tu => "tuesday",
            Self::We => "wednesday",
            Self::Th => "thursday",
            Self::Fr => "friday",
            Self::Sa => "saturday",
            Self::Su => "sunday",
        }
    }

    /// JSCalendar / RFC 8984: two-letter lowercase ("mo", ...).
    pub fn jscalendar(self) -> &'static str {
        match self {
            Self::Mo => "mo",
            Self::Tu => "tu",
            Self::We => "we",
            Self::Th => "th",
            Self::Fr => "fr",
            Self::Sa => "sa",
            Self::Su => "su",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedRule {
    pub freq: Option<Frequency>,
    pub interval: Option<u32>,
    pub count: Option<u32>,
    pub until: Option<DateTime<Utc>>,
    pub by_day: Vec<ByDay>,
    pub by_month_day: Vec<i8>,
    pub by_month: Vec<u8>,
}

impl ParsedRule {
    /// Parse an RRULE value (without the leading `RRULE:`).
    /// Tolerant: unknown keys are dropped, malformed values for a
    /// known key drop just that key. Returns the empty parse if
    /// `value` is empty or wholly unparseable.
    pub fn parse(value: &str) -> Self {
        let mut out = Self::default();
        for part in value.split(';') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let Some((key, val)) = part.split_once('=') else {
                continue;
            };
            match key.trim().to_ascii_uppercase().as_str() {
                "FREQ" => out.freq = Frequency::parse(val.trim()),
                "INTERVAL" => out.interval = val.trim().parse().ok(),
                "COUNT" => out.count = val.trim().parse().ok(),
                "UNTIL" => out.until = parse_until(val.trim()),
                "BYDAY" => {
                    out.by_day = val
                        .split(',')
                        .filter_map(|d| parse_by_day(d.trim()))
                        .collect();
                }
                "BYMONTHDAY" => {
                    out.by_month_day = val
                        .split(',')
                        .filter_map(|d| d.trim().parse::<i8>().ok())
                        .filter(|n| (-31..=31).contains(n) && *n != 0)
                        .collect();
                }
                "BYMONTH" => {
                    out.by_month = val
                        .split(',')
                        .filter_map(|d| d.trim().parse::<u8>().ok())
                        .filter(|n| (1..=12).contains(n))
                        .collect();
                }
                _ => {}
            }
        }
        out
    }
}

fn parse_by_day(s: &str) -> Option<ByDay> {
    // RFC 5545: `[+/-]<N><DAY>` where N is optional. Examples: `MO`,
    // `2MO`, `-1FR` ("last Friday").
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && matches!(bytes[i], b'+' | b'-' | b'0'..=b'9') {
        i += 1;
    }
    let (ord, day) = s.split_at(i);
    let ordinal = if ord.is_empty() {
        None
    } else {
        ord.parse::<i32>().ok()
    };
    let weekday = Weekday::parse(day)?;
    Some(ByDay { ordinal, weekday })
}

/// Parse an UNTIL value. RFC 5545 lets UNTIL be either a `DATE`
/// (`YYYYMMDD`) or a UTC `DATE-TIME` (`YYYYMMDDTHHMMSSZ`). Fixtures
/// today only need the UTC form, but accepting the date form too
/// keeps the parser permissive against well-formed real-world rules.
fn parse_until(s: &str) -> Option<DateTime<Utc>> {
    // UTC date-time, e.g. `20260315T100000Z`.
    if let Ok(dt) =
        chrono::NaiveDateTime::parse_from_str(s.trim_end_matches('Z'), "%Y%m%dT%H%M%S")
    {
        return Some(dt.and_utc());
    }
    // Date-only form, e.g. `20260315`: midnight UTC.
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y%m%d") {
        return Some(d.and_hms_opt(0, 0, 0)?.and_utc());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn parses_weekly_with_byday_and_count() {
        let r = ParsedRule::parse("FREQ=WEEKLY;BYDAY=MO,WE,FR;COUNT=10");
        assert_eq!(r.freq, Some(Frequency::Weekly));
        assert_eq!(r.count, Some(10));
        assert_eq!(r.interval, None);
        assert_eq!(r.until, None);
        assert_eq!(r.by_day.len(), 3);
        assert_eq!(r.by_day[0].weekday, Weekday::Mo);
        assert_eq!(r.by_day[2].weekday, Weekday::Fr);
        assert!(r.by_day.iter().all(|d| d.ordinal.is_none()));
    }

    #[test]
    fn parses_monthly_with_byday_ordinal() {
        let r = ParsedRule::parse("FREQ=MONTHLY;BYDAY=2MO,-1FR");
        assert_eq!(r.freq, Some(Frequency::Monthly));
        assert_eq!(r.by_day[0], ByDay { ordinal: Some(2), weekday: Weekday::Mo });
        assert_eq!(r.by_day[1], ByDay { ordinal: Some(-1), weekday: Weekday::Fr });
    }

    #[test]
    fn parses_until_in_utc_form() {
        let r = ParsedRule::parse("FREQ=DAILY;UNTIL=20260315T100000Z");
        assert_eq!(r.until, Some(Utc.with_ymd_and_hms(2026, 3, 15, 10, 0, 0).unwrap()));
    }

    #[test]
    fn parses_until_date_only_form() {
        let r = ParsedRule::parse("FREQ=DAILY;UNTIL=20260315");
        assert_eq!(r.until, Some(Utc.with_ymd_and_hms(2026, 3, 15, 0, 0, 0).unwrap()));
    }

    #[test]
    fn parses_interval_and_bymonthday() {
        let r = ParsedRule::parse("FREQ=MONTHLY;INTERVAL=2;BYMONTHDAY=15,-1");
        assert_eq!(r.interval, Some(2));
        assert_eq!(r.by_month_day, vec![15, -1]);
    }

    #[test]
    fn drops_unknown_keys_quietly() {
        // BYSETPOS / WKST not modeled; rule still parses the rest.
        let r = ParsedRule::parse("FREQ=WEEKLY;BYSETPOS=2;WKST=MO;COUNT=5");
        assert_eq!(r.freq, Some(Frequency::Weekly));
        assert_eq!(r.count, Some(5));
    }

    #[test]
    fn drops_malformed_count_but_keeps_rest() {
        let r = ParsedRule::parse("FREQ=DAILY;COUNT=banana");
        assert_eq!(r.freq, Some(Frequency::Daily));
        assert_eq!(r.count, None);
    }
}
