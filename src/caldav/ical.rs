//! iCalendar (RFC 5545) projection + parsing for CalDAV.
//!
//! v0 emits the minimum VCALENDAR / VEVENT shape ratatoskr's
//! `parse_icalendar` accepts: `UID`, `SUMMARY`, `DESCRIPTION`,
//! `LOCATION`, `DTSTART`, `DTEND`, `RRULE`, `EXDATE`, `ORGANIZER`,
//! `ATTENDEE`. No alarms or attachments yet (the fixture types
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
    if let Some(rrule) = &event.recurrence_rule {
        // RRULE values are emitted unescaped: RFC 5545 §3.3.10 says
        // the value is a structured RECUR (semicolon-separated
        // pairs), not TEXT. Escaping `;` here would corrupt the
        // wire form. Sanitize CR/LF to keep an authored rule from
        // injecting a second property line.
        let safe = rrule
            .chars()
            .filter(|c| *c != '\r' && *c != '\n')
            .collect::<String>();
        out.push_str("RRULE:");
        out.push_str(&safe);
        out.push_str("\r\n");
    }
    for ex in &event.recurrence_exdates {
        write_line(&mut out, "EXDATE", &format_dt(*ex));
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
    out.push_str(&sanitize_address(&addr.email));
    out.push_str("\r\n");
}

/// Strip CR/LF/control bytes from an address before it lands in an
/// iCal property line. Real email addresses (per RFC 5321) cannot
/// contain those characters, but a hostile fixture could embed
/// `\r\nDESCRIPTION:injected` and inject a new property line into
/// every CalDAV GET / REPORT. Defensive sanitisation here keeps the
/// emit path infallible without forcing every caller to validate.
fn sanitize_address(email: &str) -> String {
    email
        .chars()
        .filter(|c| !c.is_control() && *c != '\r' && *c != '\n')
        .collect()
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
/// about gets dropped; the v0 fixture doesn't carry alarms or
/// per-event timezones.
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
    pub recurrence_rule: Option<String>,
    pub recurrence_exdates: Vec<DateTime<Utc>>,
}

/// Parse a VCALENDAR body, returning the first VEVENT's fields.
/// Returns `Err` if no VEVENT is present, or if more than one is
/// present. v0 only models one event per body; silently dropping
/// the second VEVENT would let a hostile / broken client supply
/// two with different UIDs and we'd take whichever the parser saw
/// first - particularly bad given the URL/UID divergence concern
/// where the URL says one id but the body could ship another.
pub(crate) fn parse_vevent(body: &str) -> Result<ParsedEvent, &'static str> {
    let mut in_event = false;
    let mut parsed = ParsedEvent::default();
    let mut found = 0usize;
    for raw_line in unfold_lines(body) {
        let line = raw_line.trim_end_matches(['\r', '\n']);
        if line == "BEGIN:VEVENT" {
            found += 1;
            if found > 1 {
                return Err("body contains multiple VEVENTs; v0 accepts at most one");
            }
            in_event = true;
            continue;
        }
        if line == "END:VEVENT" {
            in_event = false;
            // Don't break: keep scanning so a second BEGIN:VEVENT
            // later in the body trips the multi-VEVENT guard above.
            continue;
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
            // RRULE values are not TEXT-escaped (see emit), so keep
            // the raw value. A client that round-trips through us
            // gets the same bytes back even for rules we don't
            // structurally model.
            "RRULE" => parsed.recurrence_rule = Some(value.to_string()),
            "EXDATE" => {
                // RFC 5545 lets a single EXDATE line carry a
                // comma-separated list of dates. Split and parse
                // each independently; unparseable entries are
                // dropped (a strict server would reject; v0 is
                // permissive to avoid masking client bugs as 400s).
                for part in value.split(',') {
                    if let Some(dt) = parse_dt(part.trim()) {
                        parsed.recurrence_exdates.push(dt);
                    }
                }
            }
            _ => {}
        }
    }
    if found == 0 {
        Err("body must contain a VEVENT")
    } else {
        Ok(parsed)
    }
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
/// `mailto:` strip is case-insensitive: RFC 5545 examples and Apple
/// Calendar emit `MAILTO:` uppercase, lowercase clients emit
/// `mailto:`, both must round-trip without leaving the prefix
/// embedded in `address.email`.
fn parse_address(line: &str) -> Option<Address> {
    let colon = line.find(':')?;
    let params_segment = &line[..colon];
    let value = &line[colon + 1..];
    let email = strip_mailto_prefix(value).to_string();
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

/// Case-insensitive `mailto:` prefix strip. Apple Calendar emits
/// `MAILTO:` uppercase per the RFC 5545 examples; lowercase clients
/// emit `mailto:`. Both must produce the same `address.email` so a
/// PUT round-trip doesn't surface `mailto:MAILTO:bob@x` (the
/// re-emit unconditionally adds the lowercase prefix).
fn strip_mailto_prefix(value: &str) -> &str {
    if value.len() >= 7 && value[..7].eq_ignore_ascii_case("mailto:") {
        &value[7..]
    } else {
        value
    }
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
            account_id: "acct".into(),
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
            recurrence_rule: None,
            recurrence_exdates: vec![],
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
            account_id: "acct".into(),
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
            recurrence_rule: None,
            recurrence_exdates: vec![],
        };
        let ical = event_to_ical(&event);
        assert!(ical.contains("Q1\\; budget review"));
        let parsed = parse_vevent(&ical).expect("parse");
        assert_eq!(parsed.summary.as_deref(), Some("Q1; budget review"));
    }

    #[test]
    fn organizer_mailto_prefix_strip_is_case_insensitive() {
        // Apple Calendar emits MAILTO: uppercase; lowercase clients
        // emit mailto:. Both must produce email = "alice@example.com"
        // (without the prefix), so a round-trip doesn't surface
        // mailto:MAILTO:alice@example.com.
        for mailto in ["mailto:", "MAILTO:", "MailTo:"] {
            let body = format!(
                "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:ev-1\r\nSUMMARY:X\r\nDTSTART:20260115T090000Z\r\nDTEND:20260115T091500Z\r\nORGANIZER:{mailto}alice@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
            );
            let parsed = parse_vevent(&body).expect("parse");
            let org = parsed.organizer.expect("organizer");
            assert_eq!(
                org.email, "alice@example.com",
                "{mailto} prefix not stripped"
            );
        }
    }

    #[test]
    fn parse_vevent_rejects_multi_vevent_body() {
        let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
                    BEGIN:VEVENT\r\nUID:a\r\nSUMMARY:X\r\nDTSTART:20260115T090000Z\r\nDTEND:20260115T091500Z\r\nEND:VEVENT\r\n\
                    BEGIN:VEVENT\r\nUID:b\r\nSUMMARY:Y\r\nDTSTART:20260115T100000Z\r\nDTEND:20260115T110000Z\r\nEND:VEVENT\r\n\
                    END:VCALENDAR\r\n";
        let err = parse_vevent(body).unwrap_err();
        assert!(err.contains("multiple VEVENT"), "wrong error: {err}");
    }

    #[test]
    fn parse_vevent_rejects_empty_body() {
        let err = parse_vevent("BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n").unwrap_err();
        assert!(err.contains("must contain a VEVENT"), "wrong error: {err}");
    }

    #[test]
    fn address_email_with_crlf_is_sanitised_no_property_injection() {
        // A hostile email value containing \r\n followed by a fake
        // iCal property would, without sanitisation, inject a new
        // line into every CalDAV GET. Sanitisation strips CR/LF/
        // control bytes; the result is the original email with the
        // dangerous bytes removed (no escaping, since email
        // addresses per RFC 5321 cannot contain those characters
        // legitimately).
        let event = Event {
            id: "ev".into(),
            account_id: "acct".into(),
            calendar_id: "cal".into(),
            subject: "x".into(),
            body_preview: None,
            body_text: None,
            start: dt(2026, 2, 1, 14, 0, 0),
            end: dt(2026, 2, 1, 15, 0, 0),
            location: None,
            organizer: Some(Address {
                email: "evil@x\r\nDESCRIPTION:injected".into(),
                name: None,
            }),
            attendees: vec![],
            is_all_day: false,
            recurrence_rule: None,
            recurrence_exdates: vec![],
        };
        let ical = event_to_ical(&event);
        // The injected payload is concatenated into the ORGANIZER
        // line as a single contiguous value (which a parser will
        // reject as a malformed address) rather than appearing on
        // its own line. The defence is "no new line emitted", not
        // "the malformed address is hidden".
        let line_starts_with_description = ical
            .split("\r\n")
            .any(|l| l.starts_with("DESCRIPTION:injected"));
        assert!(
            !line_starts_with_description,
            "DESCRIPTION:injected appeared as its own property line: {ical}"
        );
        let organizer_line = ical
            .split("\r\n")
            .find(|l| l.starts_with("ORGANIZER"))
            .expect("ORGANIZER line");
        assert!(!organizer_line.contains('\r'));
        assert!(!organizer_line.contains('\n'));
        assert_eq!(organizer_line, "ORGANIZER:mailto:evil@xDESCRIPTION:injected");
    }
}
