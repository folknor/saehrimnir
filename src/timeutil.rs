//! UTC time helpers shared across the protocol layers.
//!
//! Every timestamp in the fixture and on every wire is UTC, so the
//! conversions between `jiff::Timestamp` (an instant) and
//! `jiff::civil::DateTime` (a wall-clock reading) all go through the
//! fixed UTC offset. Going through `tz::Offset::UTC` rather than a
//! named `TimeZone` keeps the conversion independent of a tzdb being
//! present, which matters because the mock has to behave identically
//! on hosts that ship no zoneinfo.

use jiff::Timestamp;
use jiff::civil;
use jiff::tz::Offset;

/// Interpret a civil datetime as UTC. `None` when the instant falls
/// outside `Timestamp`'s representable range.
pub fn utc(dt: civil::DateTime) -> Option<Timestamp> {
    Offset::UTC.to_timestamp(dt).ok()
}

/// Midnight UTC on the given civil date.
pub fn utc_midnight(d: civil::Date) -> Option<Timestamp> {
    utc(d.at(0, 0, 0, 0))
}

/// The UTC wall-clock reading of an instant.
pub fn civil_utc(ts: Timestamp) -> civil::DateTime {
    Offset::UTC.to_datetime(ts)
}

/// Render an instant as an RFC 2822 date-time, the form a mail `Date:`
/// header takes: `Thu, 15 Jan 2026 10:00:00 +0000`. Spelled out via
/// `strftime` rather than `jiff::fmt::rfc2822` because that printer
/// emits an unpadded day and the `-0000` unknown-offset marker, and
/// both of those are visible in the byte-stable wire transcripts the
/// IMAP and Graph tests pin.
pub fn rfc2822(ts: Timestamp) -> String {
    ts.strftime("%a, %d %b %Y %H:%M:%S +0000").to_string()
}

/// A UTC instant from its calendar/clock components. `None` on an
/// invalid date (e.g. February 30) or an out-of-range instant.
pub fn ymd_hms(year: i16, month: i8, day: i8, hour: i8, min: i8, sec: i8) -> Option<Timestamp> {
    let d = civil::Date::new(year, month, day).ok()?;
    utc(d.at(hour, min, sec, 0))
}
