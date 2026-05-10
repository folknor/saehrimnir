//! iCalendar (RFC 5545) projection + parsing for CalDAV.
//!
//! v0 emits the minimum VCALENDAR / VEVENT shape ratatoskr's
//! `parse_icalendar` accepts: `UID`, `SUMMARY`, `DESCRIPTION`,
//! `LOCATION`, `DTSTART`, `DTEND`, `ORGANIZER`, `ATTENDEE`. No
//! recurrence, alarms, or attachments yet (the fixture types
//! don't carry them).
//!
//! Filled in by the GET / REPORT / PUT wedges.

#![allow(dead_code)]

use chrono::{DateTime, Utc};

use crate::fixture::{Address, Event};

/// Format `dt` as an RFC 5545 UTC date-time:
/// `YYYYMMDDTHHMMSSZ`.
pub(crate) fn format_dt(dt: DateTime<Utc>) -> String {
    // chrono's `to_rfc3339` is `2026-01-15T09:00:00+00:00`. iCal
    // wants `20260115T090000Z`. Build from the components rather
    // than munging the rfc3339 string so daylight rounding stays
    // honest.
    dt.format("%Y%m%dT%H%M%SZ").to_string()
}

/// Parse an iCalendar UTC date-time as emitted by `format_dt`.
/// Tolerates both UTC (`Z` suffix) and floating-local times by
/// treating the latter as UTC. Returns None for any other shape.
pub(crate) fn parse_dt(s: &str) -> Option<DateTime<Utc>> {
    // RFC 5545 form: `YYYYMMDDTHHMMSS[Z]`. Try the Z form first.
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s.trim_end_matches('Z'), "%Y%m%dT%H%M%S") {
        return Some(dt.and_utc());
    }
    // RFC 3339 fallback (some clients send full timestamps).
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    None
}

/// Project an [`Event`] into a single-VEVENT VCALENDAR text body.
pub(crate) fn event_to_ical(event: &Event) -> String {
    let mut out = String::new();
    out.push_str("BEGIN:VCALENDAR\r\n");
    out.push_str("VERSION:2.0\r\n");
    out.push_str("PRODID:-//saehrimnir//mock CalDAV//EN\r\n");
    out.push_str("BEGIN:VEVENT\r\n");
    write_line(&mut out, "UID", &event.id);
    write_line(&mut out, "SUMMARY", &event.subject);
    if let Some(desc) = &event.body_text {
        write_line(&mut out, "DESCRIPTION", desc);
    }
    if let Some(loc) = &event.location {
        write_line(&mut out, "LOCATION", loc);
    }
    write_line(&mut out, "DTSTART", &format_dt(event.start));
    write_line(&mut out, "DTEND", &format_dt(event.end));
    if event.is_all_day {
        write_line(&mut out, "X-MICROSOFT-CDO-ALLDAYEVENT", "TRUE");
    }
    if let Some(org) = &event.organizer {
        write_address_line(&mut out, "ORGANIZER", org);
    }
    for attendee in &event.attendees {
        write_address_line(&mut out, "ATTENDEE", attendee);
    }
    out.push_str("END:VEVENT\r\n");
    out.push_str("END:VCALENDAR\r\n");
    out
}

fn write_line(out: &mut String, name: &str, value: &str) {
    out.push_str(name);
    out.push(':');
    out.push_str(&escape_text(value));
    out.push_str("\r\n");
}

fn write_address_line(out: &mut String, name: &str, addr: &Address) {
    out.push_str(name);
    if let Some(n) = &addr.name {
        out.push_str(";CN=");
        out.push_str(&escape_text(n));
    }
    out.push_str(":mailto:");
    out.push_str(&addr.email);
    out.push_str("\r\n");
}

/// Escape RFC 5545 TEXT values: backslash, semicolon, comma, and
/// CR/LF. Newlines inside a value become `\n`. Long lines are
/// emitted on a single line; ratatoskr's parser tolerates either,
/// and `event.subject` / `body_text` are typically short.
fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\r' => {} // dropped; CRLF pairs become \n via the next char
            _ => out.push(c),
        }
    }
    out
}

/// Inverse of `escape_text`. Used by the PUT path when reading the
/// VEVENT body the client sent.
pub(crate) fn unescape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n' | 'N') => out.push('\n'),
                Some('\\') => out.push('\\'),
                Some(';') => out.push(';'),
                Some(',') => out.push(','),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Parsed fields from an inbound VEVENT. Anything we don't care
/// about gets dropped; the v0 fixture doesn't carry recurrence,
/// alarms, or per-event timezones.
#[derive(Debug, Default)]
pub(crate) struct ParsedEvent {
    pub uid: Option<String>,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    pub is_all_day: bool,
    pub organizer: Option<Address>,
    pub attendees: Vec<Address>,
}

/// Parse a VCALENDAR body, returning the first VEVENT's fields.
/// Returns None if no VEVENT is present.
pub(crate) fn parse_vevent(body: &str) -> Option<ParsedEvent> {
    let mut in_event = false;
    let mut parsed = ParsedEvent::default();
    let mut found = false;
    for raw_line in unfold_lines(body) {
        let line = raw_line.trim_end_matches(['\r', '\n']);
        if line == "BEGIN:VEVENT" {
            in_event = true;
            found = true;
            continue;
        }
        if line == "END:VEVENT" {
            break;
        }
        if !in_event {
            continue;
        }
        let (name, value) = match split_property(line) {
            Some(nv) => nv,
            None => continue,
        };
        match name.as_str() {
            "UID" => parsed.uid = Some(unescape_text(value)),
            "SUMMARY" => parsed.summary = Some(unescape_text(value)),
            "DESCRIPTION" => parsed.description = Some(unescape_text(value)),
            "LOCATION" => parsed.location = Some(unescape_text(value)),
            "DTSTART" => parsed.start = parse_dt(value),
            "DTEND" => parsed.end = parse_dt(value),
            "ORGANIZER" => parsed.organizer = parse_address(line),
            "ATTENDEE" => {
                if let Some(a) = parse_address(line) {
                    parsed.attendees.push(a);
                }
            }
            "X-MICROSOFT-CDO-ALLDAYEVENT" if value.eq_ignore_ascii_case("TRUE") => {
                parsed.is_all_day = true;
            }
            _ => {}
        }
    }
    if found { Some(parsed) } else { None }
}

/// Unfold RFC 5545 line continuations: a line starting with a
/// space or tab is a continuation of the previous line. Returns
/// owned strings since the unfold may need to concatenate.
fn unfold_lines(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in body.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if matches!(line.chars().next(), Some(' ' | '\t'))
            && let Some(last) = out.last_mut()
        {
            last.push_str(&line[1..]);
            continue;
        }
        out.push(line.to_string());
    }
    out
}

/// Split `NAME[;PARAM=VAL...]:VALUE`. Returns `(name, value)`. The
/// parameter portion is dropped here - the only callers that need
/// it (`parse_address`) handle the full line themselves.
fn split_property(line: &str) -> Option<(String, &str)> {
    let colon = line.find(':')?;
    let head = &line[..colon];
    let value = &line[colon + 1..];
    let semi = head.find(';');
    let name_end = semi.unwrap_or(head.len());
    let name = head[..name_end].to_uppercase();
    Some((name, value))
}

/// Parse an `ORGANIZER` / `ATTENDEE` line: `NAME[;CN="..."]:mailto:address`.
fn parse_address(line: &str) -> Option<Address> {
    let colon = line.find(':')?;
    let params_segment = &line[..colon];
    let value = &line[colon + 1..];
    let email = value.strip_prefix("mailto:").unwrap_or(value).to_string();
    let mut name = None;
    for param in params_segment.split(';').skip(1) {
        if let Some(rest) = param.strip_prefix("CN=") {
            // CN values can be quoted; strip surrounding quotes.
            let trimmed = rest.trim_matches('"');
            if !trimmed.is_empty() {
                name = Some(unescape_text(trimmed));
            }
        }
    }
    Some(Address { email, name })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn dt(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    #[test]
    fn round_trip_minimal_event() {
        let event = Event {
            id: "ev-1".into(),
            calendar_id: "cal".into(),
            subject: "Standup".into(),
            body_preview: None,
            body_text: Some("Daily; 15 minutes".into()),
            start: dt(2026, 1, 15, 9, 0, 0),
            end: dt(2026, 1, 15, 9, 15, 0),
            location: Some("Room A".into()),
            organizer: Some(Address {
                email: "alice@example.com".into(),
                name: Some("Alice".into()),
            }),
            attendees: vec![Address {
                email: "bob@example.com".into(),
                name: None,
            }],
            is_all_day: false,
        };
        let ical = event_to_ical(&event);
        let parsed = parse_vevent(&ical).expect("parse");
        assert_eq!(parsed.uid.as_deref(), Some("ev-1"));
        assert_eq!(parsed.summary.as_deref(), Some("Standup"));
        assert_eq!(parsed.description.as_deref(), Some("Daily; 15 minutes"));
        assert_eq!(parsed.location.as_deref(), Some("Room A"));
        assert_eq!(parsed.start, Some(dt(2026, 1, 15, 9, 0, 0)));
        assert_eq!(parsed.end, Some(dt(2026, 1, 15, 9, 15, 0)));
        let org = parsed.organizer.as_ref().expect("organizer");
        assert_eq!(org.email, "alice@example.com");
        assert_eq!(org.name.as_deref(), Some("Alice"));
        assert_eq!(parsed.attendees.len(), 1);
        assert_eq!(parsed.attendees[0].email, "bob@example.com");
    }

    #[test]
    fn semicolons_in_summary_are_escaped_and_round_trip() {
        let event = Event {
            id: "ev".into(),
            calendar_id: "cal".into(),
            subject: "Q1; budget review".into(),
            body_preview: None,
            body_text: None,
            start: dt(2026, 2, 1, 14, 0, 0),
            end: dt(2026, 2, 1, 15, 0, 0),
            location: None,
            organizer: None,
            attendees: vec![],
            is_all_day: false,
        };
        let ical = event_to_ical(&event);
        assert!(ical.contains("Q1\\; budget review"));
        let parsed = parse_vevent(&ical).expect("parse");
        assert_eq!(parsed.summary.as_deref(), Some("Q1; budget review"));
    }
}
