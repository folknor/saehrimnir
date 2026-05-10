# Ratatoskr's CalDAV client surface

What the v0 CalDAV mock has to satisfy. Distilled from
`<ratatoskr>/crates/calendar/src/caldav/` (high-level wrapper) and
`<ratatoskr>/crates/core/src/caldav/client/` (the actual wire
client) on 2026-05-10. Source-of-truth lives there; this file is a
cheat sheet so we don't have to fan out every turn.

The CalDAV listener is wired in v0 (`src/caldav/`). It reuses the
same `[[calendar]]` / `[[event]]` fixture types the Graph calendar
surface already projects, so a single fixture exercises both
backends.

## Wire shape at a glance

| Verb       | Path                                            | Purpose |
|------------|-------------------------------------------------|---------|
| `PROPFIND` | `/`                                             | Discover principal (`current-user-principal`). |
| `PROPFIND` | `/.well-known/caldav`                           | Same as `/` (RFC 6764 fallback hint). |
| `PROPFIND` | `/principals/{user}/`                           | Discover `calendar-home-set`. |
| `PROPFIND` | `/calendars/{user}/` (Depth=1)                  | List calendar collections (displayname, ctag, color, privilege-set). |
| `PROPFIND` | `/calendars/{user}/{cal}/` (Depth=0)            | Calendar-level CTag (CalendarServer extension). |
| `PROPFIND` | `/calendars/{user}/{cal}/` (Depth=1)            | List event resources (getetag, getcontenttype). |
| `REPORT`   | `/calendars/{user}/{cal}/`                      | `calendar-multiget` (fetch by hrefs) or `calendar-query` (time-range filter). |
| `GET`      | `/calendars/{user}/{cal}/{event}.ics`           | Single iCalendar body + ETag. |
| `PUT`      | `/calendars/{user}/{cal}/{event}.ics`           | Create/update; honours `If-Match` for ETag conflict. |
| `DELETE`   | `/calendars/{user}/{cal}/{event}.ics`           | Remove; honours `If-Match`. |

`{user}` is the fixture's `account.id`. `{cal}` is the
`Calendar::id` from `[[calendar]]`. `{event}` is the `Event::id`;
the event resource always serves at `<id>.ics` regardless of
whether the fixture's id includes a UUID. (Real servers usually
use UUID-shaped names; v0 is happy with whatever the fixture
declared.)

## Discovery flow

1. PROPFIND on `/` (or `/.well-known/caldav` as fallback) with body:

   ```xml
   <propfind xmlns="DAV:"><prop><current-user-principal/></prop></propfind>
   ```

   v0 returns `207 Multi-Status` with `current-user-principal`
   resolving to `/principals/{user}/`. The fallback path
   responds identically; ratatoskr doesn't redirect so we don't
   either.

2. PROPFIND on `/principals/{user}/` with body asking for
   `caldav:calendar-home-set` (and the DAV namespace):

   ```xml
   <propfind xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
     <prop><C:calendar-home-set/></prop>
   </propfind>
   ```

   Returns `<C:calendar-home-set><href>/calendars/{user}/</href></C:calendar-home-set>`.

3. PROPFIND on `/calendars/{user}/` with Depth=1 listing
   `resourcetype`, `displayname`, `CS:getctag`,
   `IC:calendar-color`, `current-user-privilege-set`. Returns one
   `<response>` per calendar plus the home collection itself
   (with `resourcetype = collection` only - no `calendar` flag).
   Each calendar response carries `resourcetype =
   "collection calendar"`, the configured `displayname`, a CTag
   derived from `Fixture::state` (see "ETag / CTag" below), and
   a privilege-set of `read` / `write` / `write-properties` /
   `write-content` / `read-current-user-privilege-set`.

## Event listing and fetch

PROPFIND with Depth=1 on a calendar URL returns one
`<response>` per VEVENT plus the calendar collection itself.
Event responses carry `getetag` and `getcontenttype =
"text/calendar; component=vevent"`.

`REPORT calendar-multiget` accepts an XML body listing
`<href>` elements and returns each one's `getetag` plus
`<C:calendar-data>` containing the VCALENDAR/VEVENT iCalendar
text. Hrefs may be relative or absolute on the same origin -
v0 normalises both.

`REPORT calendar-query` filters by `<C:time-range
start="20260101T000000Z" end="20260131T235959Z"/>` inside the
nested `<C:comp-filter name="VEVENT">`. v0 honours UTC-shaped
RFC 5545 timestamps (`YYYYMMDDTHHMMSSZ`) and returns events
whose `[start, end)` overlaps the query range.

`GET` on an event URL returns the iCalendar body verbatim with
the `ETag` header set.

## iCalendar projection

Each fixture event projects as a single VCALENDAR with one
VEVENT. v0 emits:

- `UID` - the fixture's `event.id`.
- `SUMMARY` - `event.subject`.
- `DESCRIPTION` - `event.body_text` if set.
- `LOCATION` - `event.location` if set.
- `DTSTART` / `DTEND` - UTC formatted as
  `YYYYMMDDTHHMMSSZ`.
- `ORGANIZER` - `mailto:{email}` plus `CN` parameter if the
  fixture's `organizer` carries a name.
- `ATTENDEE` - one line per attendee; `mailto:{email}` plus
  `CN` parameter for the name. v0 does not yet emit
  `PARTSTAT` / `ROLE` (ratatoskr's parser is tolerant of their
  absence).

Recurrence, alarms, and per-event timezones are not in scope
for v0; the fixture types don't carry them, and ratatoskr's
parser handles their absence cleanly.

## Mutations

`PUT` parses the request body as iCalendar and either creates
a new event (when no event with that id exists) or updates the
matching one. The mutation runs through `Fixture::mutate` so
the `event_created` / `event_updated` id sets land on the
change_log. Subsequent Graph `calendarView/delta` walks
observe the same mutation - the fixture image is the single
source of truth across both surfaces.

PUT honours `If-Match`: when the header is present and its
ETag does not match the current event's ETag (or the event
doesn't exist), v0 returns `412 Precondition Failed`. New
events sent with `If-Match: *` must not exist (POST-style
"create-or-fail"); a mismatch returns 412.

DELETE removes the event from the fixture and records
`event_destroyed`. `If-Match` is checked the same way.

Both mutating verbs return the freshly computed ETag in the
response headers so the client can update its local cache
without an extra GET.

## ETag / CTag derivation

ETags and CTags are deterministic, derived from the fixture
state token plus the resource id. The format is documented
inline in `src/caldav/mod.rs`; the only thing tests should rely
on is that the value changes when the underlying resource
changes and stays stable when nothing has touched it.

Real CalDAV servers use opaque hashes; ratatoskr's client only
checks for byte-equality, so the format doesn't matter as long
as it cycles correctly.

## Authentication

v0 accepts any `Authorization` header (Basic or Bearer) and
also accepts requests without one. Matches the JMAP / Graph /
Gmail "no auth in v0" baseline. The fixture-level `[oauth]`
block does not gate this listener; bearer enforcement will land
when a fixture forces it (mirrors the JMAP path).

## Out of scope for v0

- `MKCALENDAR` (creating new calendar collections from the
  client). ratatoskr never sends one.
- `MKCOL` (creating calendar home, principal, etc).
- `ACL` modifications.
- Calendar color / display name updates via `PROPPATCH`.
- Delegation (`calendar-proxy-read-for` / `-write-for`).
- Free-busy queries.
- VEVENT recurrence (RRULE / EXDATE), alarms (VALARM),
  attachments, scheduling (iTIP / iMIP), or per-event VTIMEZONE
  blocks.

These all wait on a fixture that needs them. The module
structure is flat enough to grow them in place without a
re-organisation.
