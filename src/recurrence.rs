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

    /// RRULE token: uppercase ASCII, e.g. `"WEEKLY"`. Inverse of
    /// [`Self::parse`], used by [`format_rrule`].
    pub fn rrule_token(self) -> &'static str {
        match self {
            Self::Daily => "DAILY",
            Self::Weekly => "WEEKLY",
            Self::Monthly => "MONTHLY",
            Self::Yearly => "YEARLY",
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

    /// RRULE token: two-letter uppercase ASCII ("MO", ...).
    pub fn rrule_token(self) -> &'static str {
        match self {
            Self::Mo => "MO",
            Self::Tu => "TU",
            Self::We => "WE",
            Self::Th => "TH",
            Self::Fr => "FR",
            Self::Sa => "SA",
            Self::Su => "SU",
        }
    }

    /// Case-insensitive parse of the two-letter RRULE token.
    /// Used by the write paths that synthesise an RRULE from a
    /// protocol-specific structured form (Graph's `daysOfWeek`
    /// in `"monday"` form goes through `from_long` first; the
    /// short tokens land here).
    pub fn parse_short(s: &str) -> Option<Self> {
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

    /// Case-insensitive parse of Graph's full-word day token
    /// (`"monday"`, `"tuesday"`, ...). Returns None for any
    /// other string.
    pub fn parse_long(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "monday" => Some(Self::Mo),
            "tuesday" => Some(Self::Tu),
            "wednesday" => Some(Self::We),
            "thursday" => Some(Self::Th),
            "friday" => Some(Self::Fr),
            "saturday" => Some(Self::Sa),
            "sunday" => Some(Self::Su),
            _ => None,
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

/// Emit an RFC 5545 RRULE value (without the `RRULE:` prefix) from
/// a parsed rule. Returns None when `freq` is unset - a recurrence
/// with no FREQ isn't a valid wire form and the caller should leave
/// the event single-instance instead of writing a malformed rule.
///
/// Used by the create / patch paths on gcal, Graph, and JMAP to
/// turn their protocol-specific structured forms (parsed into
/// `ParsedRule`) back into the canonical RRULE string the fixture
/// stores. `INTERVAL` is omitted when 1 (the default) so the emitted
/// rule round-trips through [`ParsedRule::parse`] with the same
/// minimal field set the fixture loader produces.
pub fn format_rrule(rule: &ParsedRule) -> Option<String> {
    let freq = rule.freq?;
    let mut parts: Vec<String> = vec![format!("FREQ={}", freq.rrule_token())];
    if let Some(n) = rule.interval
        && n > 1
    {
        parts.push(format!("INTERVAL={n}"));
    }
    if !rule.by_day.is_empty() {
        let tokens: Vec<String> = rule
            .by_day
            .iter()
            .map(|d| match d.ordinal {
                Some(n) => format!("{n}{}", d.weekday.rrule_token()),
                None => d.weekday.rrule_token().to_string(),
            })
            .collect();
        parts.push(format!("BYDAY={}", tokens.join(",")));
    }
    if !rule.by_month_day.is_empty() {
        let tokens: Vec<String> = rule.by_month_day.iter().map(i8::to_string).collect();
        parts.push(format!("BYMONTHDAY={}", tokens.join(",")));
    }
    if !rule.by_month.is_empty() {
        let tokens: Vec<String> = rule.by_month.iter().map(u8::to_string).collect();
        parts.push(format!("BYMONTH={}", tokens.join(",")));
    }
    if let Some(c) = rule.count {
        parts.push(format!("COUNT={c}"));
    }
    if let Some(u) = rule.until {
        parts.push(format!("UNTIL={}", u.format("%Y%m%dT%H%M%SZ")));
    }
    Some(parts.join(";"))
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

/// Recurrence-aware time-range match, shared by all three calendar
/// read surfaces: the CalDAV `time-range` REPORT filter, the JMAP
/// `CalendarEvent/query` `after` / `before` filter, and the Graph
/// `calendarView` range read. Real servers (JMAP, CalDAV RFC 4791,
/// Graph) expand a recurring event's occurrences before testing a
/// range; we approximate that expansion without materialising every
/// instance:
///
/// - A NON-recurring event matches iff its own `[start, end)` interval
///   overlaps `[range_start, range_end)` (half-open): `start < range_end
///   && end > range_start`.
/// - A RECURRING event (any non-`None` `recurrence_rule`) matches iff
///   `start < range_end` (the series' DTSTART falls before the window
///   closes) AND the recurrence is not terminated before `range_start`.
///   Termination is read from the RRULE `UNTIL` (the series spans
///   `[DTSTART, UNTIL]`): a rule with `UNTIL < range_start` has ended
///   and does not match. COUNT-bounded and unbounded rules are treated
///   as not-terminated (a documented approximation - we do not expand
///   COUNT to find its true last occurrence).
///
/// An absent bound is open on that side.
pub fn event_matches_range(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    recurrence_rule: Option<&str>,
    range_start: Option<DateTime<Utc>>,
    range_end: Option<DateTime<Utc>>,
) -> bool {
    // DTSTART must fall before the window closes, for both recurring and
    // non-recurring events (a series' first occurrence is its DTSTART).
    if let Some(re) = range_end
        && start >= re
    {
        return false;
    }
    match recurrence_rule {
        // Single instance: its own interval must reach past the window's
        // start.
        None => {
            if let Some(rs) = range_start
                && end <= rs
            {
                return false;
            }
            true
        }
        // Recurring: matches unless an explicit UNTIL ends the series
        // before the window opens. COUNT / unbounded rules never
        // terminate for this test.
        Some(rule) => {
            if let Some(rs) = range_start
                && let Some(until) = ParsedRule::parse(rule).until
                && until < rs
            {
                return false;
            }
            true
        }
    }
}

/// Parse an UNTIL value. RFC 5545 lets UNTIL be either a `DATE`
/// (`YYYYMMDD`) or a UTC `DATE-TIME` (`YYYYMMDDTHHMMSSZ`). Fixtures
/// today only need the UTC form, but accepting the date form too
/// keeps the parser permissive against well-formed real-world rules.
fn parse_until(s: &str) -> Option<DateTime<Utc>> {
    // UTC date-time, e.g. `20260315T100000Z`.
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s.trim_end_matches('Z'), "%Y%m%dT%H%M%S")
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
        assert_eq!(
            r.by_day[0],
            ByDay {
                ordinal: Some(2),
                weekday: Weekday::Mo
            }
        );
        assert_eq!(
            r.by_day[1],
            ByDay {
                ordinal: Some(-1),
                weekday: Weekday::Fr
            }
        );
    }

    #[test]
    fn parses_until_in_utc_form() {
        let r = ParsedRule::parse("FREQ=DAILY;UNTIL=20260315T100000Z");
        assert_eq!(
            r.until,
            Some(Utc.with_ymd_and_hms(2026, 3, 15, 10, 0, 0).unwrap())
        );
    }

    #[test]
    fn parses_until_date_only_form() {
        let r = ParsedRule::parse("FREQ=DAILY;UNTIL=20260315");
        assert_eq!(
            r.until,
            Some(Utc.with_ymd_and_hms(2026, 3, 15, 0, 0, 0).unwrap())
        );
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

    #[test]
    fn format_rrule_round_trips_weekly_byday_count() {
        let r = ParsedRule::parse("FREQ=WEEKLY;BYDAY=MO,WE,FR;COUNT=10");
        let s = format_rrule(&r).unwrap();
        assert_eq!(s, "FREQ=WEEKLY;BYDAY=MO,WE,FR;COUNT=10");
        // Round-trip: parse the emitted string back; the parsed
        // shape matches the original.
        assert_eq!(ParsedRule::parse(&s), r);
    }

    #[test]
    fn format_rrule_omits_default_interval() {
        let r = ParsedRule {
            freq: Some(Frequency::Daily),
            interval: Some(1),
            count: Some(5),
            ..Default::default()
        };
        assert_eq!(format_rrule(&r).unwrap(), "FREQ=DAILY;COUNT=5");
    }

    #[test]
    fn format_rrule_emits_explicit_interval_when_greater_than_one() {
        let r = ParsedRule {
            freq: Some(Frequency::Monthly),
            interval: Some(3),
            by_month_day: vec![15],
            ..Default::default()
        };
        assert_eq!(
            format_rrule(&r).unwrap(),
            "FREQ=MONTHLY;INTERVAL=3;BYMONTHDAY=15"
        );
    }

    #[test]
    fn format_rrule_emits_until_in_utc_form() {
        let r = ParsedRule::parse("FREQ=DAILY;UNTIL=20261215T170000Z");
        let s = format_rrule(&r).unwrap();
        assert!(s.ends_with("UNTIL=20261215T170000Z"), "got {s}");
    }

    #[test]
    fn format_rrule_emits_ordinal_byday() {
        let r = ParsedRule::parse("FREQ=MONTHLY;BYDAY=2MO,-1FR");
        let s = format_rrule(&r).unwrap();
        assert_eq!(s, "FREQ=MONTHLY;BYDAY=2MO,-1FR");
    }

    #[test]
    fn format_rrule_returns_none_without_freq() {
        let r = ParsedRule {
            freq: None,
            count: Some(5),
            ..Default::default()
        };
        assert_eq!(format_rrule(&r), None);
    }

    fn dt(y: i32, mo: u32, d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, 0, 0, 0).unwrap()
    }

    #[test]
    fn non_recurring_uses_exact_half_open_overlap() {
        // Event [Jan 15 09:00, Jan 15 09:30) vs a January window.
        let s = Utc.with_ymd_and_hms(2026, 1, 15, 9, 0, 0).unwrap();
        let e = Utc.with_ymd_and_hms(2026, 1, 15, 9, 30, 0).unwrap();
        assert!(event_matches_range(
            s,
            e,
            None,
            Some(dt(2026, 1, 1)),
            Some(dt(2026, 2, 1))
        ));
        // A February window does not overlap the single January instance.
        assert!(!event_matches_range(
            s,
            e,
            None,
            Some(dt(2026, 2, 1)),
            Some(dt(2026, 3, 1))
        ));
        // Touching at the exclusive end (event.end == range_start) is no
        // overlap.
        assert!(!event_matches_range(
            s,
            e,
            None,
            Some(e),
            Some(dt(2026, 2, 1))
        ));
    }

    #[test]
    fn recurring_unbounded_matches_window_after_dtstart() {
        // Weekly series starting Jan 2024, unbounded: matches a 2026
        // window even though its own [start, end) is years earlier.
        let s = Utc.with_ymd_and_hms(2024, 1, 1, 9, 0, 0).unwrap();
        let e = Utc.with_ymd_and_hms(2024, 1, 1, 9, 30, 0).unwrap();
        assert!(event_matches_range(
            s,
            e,
            Some("FREQ=WEEKLY;BYDAY=MO"),
            Some(dt(2026, 6, 1)),
            Some(dt(2026, 7, 1))
        ));
    }

    #[test]
    fn recurring_count_bounded_treated_as_not_terminated() {
        let s = Utc.with_ymd_and_hms(2024, 1, 1, 9, 0, 0).unwrap();
        let e = Utc.with_ymd_and_hms(2024, 1, 1, 9, 30, 0).unwrap();
        assert!(event_matches_range(
            s,
            e,
            Some("FREQ=WEEKLY;BYDAY=MO;COUNT=10"),
            Some(dt(2026, 6, 1)),
            Some(dt(2026, 7, 1))
        ));
    }

    #[test]
    fn recurring_until_in_past_drops_out() {
        // Series ends 2025-06; a 2026 window must not match it.
        let s = Utc.with_ymd_and_hms(2024, 1, 1, 9, 0, 0).unwrap();
        let e = Utc.with_ymd_and_hms(2024, 1, 1, 9, 30, 0).unwrap();
        assert!(!event_matches_range(
            s,
            e,
            Some("FREQ=WEEKLY;BYDAY=MO;UNTIL=20250601T090000Z"),
            Some(dt(2026, 6, 1)),
            Some(dt(2026, 7, 1))
        ));
    }

    #[test]
    fn recurring_until_after_window_start_matches() {
        let s = Utc.with_ymd_and_hms(2024, 1, 1, 9, 0, 0).unwrap();
        let e = Utc.with_ymd_and_hms(2024, 1, 1, 9, 30, 0).unwrap();
        assert!(event_matches_range(
            s,
            e,
            Some("FREQ=MONTHLY;BYMONTHDAY=15;UNTIL=20261215T170000Z"),
            Some(dt(2026, 11, 1)),
            Some(dt(2026, 12, 1))
        ));
    }

    #[test]
    fn recurring_dtstart_after_window_end_drops_out() {
        // A series that does not begin until after the window closes
        // cannot have an in-window occurrence.
        let s = Utc.with_ymd_and_hms(2027, 1, 1, 9, 0, 0).unwrap();
        let e = Utc.with_ymd_and_hms(2027, 1, 1, 9, 30, 0).unwrap();
        assert!(!event_matches_range(
            s,
            e,
            Some("FREQ=WEEKLY;BYDAY=MO"),
            Some(dt(2026, 6, 1)),
            Some(dt(2026, 7, 1))
        ));
    }

    #[test]
    fn open_bounds_are_permissive() {
        let s = Utc.with_ymd_and_hms(2024, 1, 1, 9, 0, 0).unwrap();
        let e = Utc.with_ymd_and_hms(2024, 1, 1, 9, 30, 0).unwrap();
        // No bounds at all: everything matches.
        assert!(event_matches_range(s, e, None, None, None));
        assert!(event_matches_range(
            s,
            e,
            Some("FREQ=WEEKLY;BYDAY=MO;UNTIL=20250101T000000Z"),
            None,
            None
        ));
    }
}
