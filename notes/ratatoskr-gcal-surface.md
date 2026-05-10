# Ratatoskr's Google Calendar client surface

What the v0 mock has to satisfy. Distilled from
`<ratatoskr>/crates/calendar/src/google.rs` on 2026-05-10.

## Connection

- Real Google Calendar host:
  `https://www.googleapis.com/calendar/v3`. Shares
  `googleapis.com` with several other Google services but the
  `/calendar/v3` prefix is what differentiates the surface. We
  host it on a dedicated listener (`--gcal-port`) so the
  eventual `RATATOSKR_TEST_GCAL_ENDPOINT` override can point at
  exactly this port.
- Bearer auth via the same Google OAuth token Gmail uses.

## Endpoints invoked

In order:

1. `GET /calendar/v3/users/me/calendarList` - first call. Reads
   each entry's `id`, `summary`, `backgroundColor`, `primary`,
   `accessRole` (one of `"owner"`, `"writer"`, `"reader"`,
   `"freeBusyReader"`; only owner / writer permit mutation).
   Source: `google.rs:95-122`.

2. `GET /calendar/v3/calendars/{id}/events` - paged events.
   Query params:
   - `maxResults=250` (default).
   - First call: `timeMin=<now-90d>` / `timeMax=<now+365d>` /
     `singleEvents=true`.
   - Subsequent calls: `syncToken=<previous>` instead of
     `timeMin`/`timeMax`.
   - All calls: optional `pageToken` for mid-list paging.
   Sync-token recovery: `google.rs:175-186` matches on `"410"`
   or `"sync token"` (case-insensitive) substrings, returns an
   empty result + `new_sync_token: None` so the caller drops
   the saved token and re-bootstraps.

3. `POST /calendar/v3/calendars/{id}/events` - create. Echoes
   the created event in the 200 response.
4. `PATCH /calendar/v3/calendars/{id}/events/{eid}` - update.
5. `DELETE /calendar/v3/calendars/{id}/events/{eid}` - 204 / no
   body.

## Event JSON shape

Per `google.rs:33-94` (`GoogleCalendarEvent` deserialiser):

```
{
  "id": "<remote id>",
  "iCalUID": "<uid>",
  "etag": "<etag>",
  "status": "confirmed" | "cancelled" | ...,
  "summary": "<title>",
  "description": "<body>",
  "location": "<plain string>",
  "start": { "dateTime": "<rfc3339>", "timeZone": "..." }
        | { "date": "<yyyy-mm-dd>" },
  "end":   <same shape>,
  "organizer": { "email": "...", "displayName": "..." },
  "attendees": [<opaque values; pass-through>],
  "htmlLink": "<url?>",
  "recurrence": ["RRULE:..."],
  "visibility": "default" | "public" | "private" | "confidential",
  "transparency": "opaque" | "transparent"
}
```

ratatoskr's projection drops a lot of these (recurrence is
flattened to the first `RRULE:` line; transparency maps to a
busy/free string). The mock omits anything ratatoskr doesn't
read.

## Sync-token contract

- Token unknown / not the seed and not the current `state` →
  HTTP 410 with `error.errors[0].reason = "fullSyncRequired"`.
  ratatoskr's recovery branch matches on the substring
  `"sync token"` (case-insensitive) and the `"410"` substring.
- Token equals current `state` → empty `items[]`, same token
  echoed as `nextSyncToken`.
- No token → full event list, paged via `nextPageToken`,
  terminating page carries `nextSyncToken`. Mid-pages must NOT
  emit `nextSyncToken` (ratatoskr would persist it before
  reading the next page).

## Tombstones

Event deletes surface as `{ "id": "...", "status": "cancelled" }`
in the events list. ratatoskr's `:189-200` branch routes
`status == "cancelled"` to `deleted_remote_ids`. The mock uses
the `event_destroyed` change-log entries (recorded by the same
`Fixture::mutate` path Graph / CalDAV / JMAP write) to surface
deletes once delta-sync lands here too; today the events list
returns live events only.

## What v0 doesn't surface

- Recurrence-instance expansion. The mock returns master events
  even when the client passes `singleEvents=true` (ratatoskr's
  fixtures don't drive RRULE today).
- Calendar-level mutation (`POST /calendarList`, etc.).
- Free/busy queries.
- `eventTypes`, `attachments`, `conferenceData`. Reserved for
  fixture-format growth.
