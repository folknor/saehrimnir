# Cross-protocol request log

`RequestLog` (in `src/request_log.rs`) is the process-scoped ring
of `RequestEntry` rows that every protocol layer appends to as it
dispatches commands. The log is exposed to harness scripts over
the JMAP HTTP listener:

- `GET /test/requests` returns a JSON array of every entry the
  harness has driven across all five protocols, oldest first.
- `DELETE /test/requests` clears the log.
- `POST /test/fixture/reset` also clears the log alongside the
  SMTP submission log, OAuth token store, and Lua dispatcher
  call counters.

The ring is capped at `REQUEST_LOG_CAP` (100k) entries; older
entries drop off the front when full. `snapshot()` `mem::take`s
under the mutex, releases it, then clones into the returned
`Vec`, so a slow read doesn't pin every protocol listener while
the snapshot materialises.

## Entry shape

Each row carries:

```
{
  "protocol":     "jmap" | "imap" | "smtp" | "graph" | "gmail" | "caldav" | "people",
  "command":      string  (protocol-native verb)
  "received_at":  RFC 3339 timestamp (wall clock; not byte-stable)
  "detail":       object  (per-protocol structured extras)
  "connection_id": optional u64  (per-TCP-connection id, omitted when
                                  the entry has no connection context)
}
```

`received_at` is a wall-clock timestamp, so rendered JSON is not
byte-identical across runs. Tests that need byte-stable output
should pass `?stable=true` (see "Stable projection" below) or
assert on `protocol` / `command` / `detail` and ignore
`received_at`.

### `connection_id`

A monotonic `u64` allocated once per accepted TCP connection (IMAP /
SMTP `serve_connection`, plus every axum HTTP connection via
`into_make_service_with_connect_info::<ConnInfo>`). Stamped onto
every entry the connection records, so harness scripts can group
the log by session without sæhrimnir locking in an aggregation
schema. Counter starts at 1 per process; resets only on restart.

Useful patterns:

- Phase 7 "one LOGIN + one SELECT per (account, folder) batch":
  group `protocol == "imap"` entries by `connection_id`, assert
  exactly one `LOGIN` per id and exactly one `SELECT <folder>`
  per (id, folder).
- HTTP keep-alive grouping: an `axum` connection that serves
  multiple Graph or Gmail requests under the same TCP socket
  produces multiple entries with the same `connection_id`; a
  fresh socket gets a fresh id.
- SMTP MAIL / RCPT / DATA per submission: every command on the
  same SMTP connection shares one id; one DATA per id is the
  per-submission count.

The field is omitted (Lua-side: `nil`) when no connection boundary
applies. Today that means test paths that use
`tower::ServiceExt::oneshot` against the HTTP routers (they bypass
`into_make_service_with_connect_info`) and direct
`RequestLog::record` calls outside any listener context.

### Stable projection

`GET /test/requests?stable=true` produces a byte-deterministic view
of the log for snapshot-style assertions:

- `received_at` is stripped from each entry.
- `connection_id` is rewritten to a 0-based first-seen dense index
  (raw ids are monotonic from process start, so absolute values
  aren't stable across restarts). Entries with no connection stay
  null and are omitted from the output. The rewriting is per
  `GET`, so two successive `?stable=true` reads of the same log
  produce identical output.

`command` is the protocol-native verb. For HTTP-based protocols
(JMAP / Graph / Gmail), the convention is `METHOD /path` with the
query string stripped; the parsed query lands inside `detail`.

## `command` and `detail` keys, per protocol

The `detail` payload is free-form JSON. Each protocol's
middleware or dispatcher picks its own key set; this is the
contract harness scripts can rely on.

### JMAP (`src/routes.rs::api`)

- `command`: the JMAP method name (e.g. `"Mailbox/get"`,
  `"Email/query"`). One entry per method-call inside a batched
  envelope; a 16-call envelope produces 16 rows that share a
  single `received_at` (entered via `RequestLog::extend` so the
  mutex is taken once).
- `detail.call_id`: the per-method `callId` from the envelope so
  tests can correlate multiple calls.
- `detail.account_id` (optional): the `accountId` argument when
  present.
- `detail.ids` (optional): the `ids[]` argument as a JSON array of
  strings, when the call carries a string-typed list (`Mailbox/get`,
  `Email/get`, future `*/set`). Absent when the field is missing
  or contains non-string entries.
- `detail.properties` (optional): the `properties[]` argument as a
  JSON array of strings. Lets a script distinguish a metadata-only
  `Email/get` (e.g. `properties=["id","keywords"]`) from a body
  fetch (`properties=[..., "bodyValues"]`) without re-deriving it
  from response shape.

### IMAP (`src/imap.rs::dispatch`)

- `command`: the verb in upper case. For `UID FETCH` /
  `UID SEARCH` etc. the sub-verb is included
  (`"UID FETCH"`); a bare `UID` with no sub-command records as
  `"UID"`.
- `detail.tag`: the IMAP tag (`a1`, `a2`, ...).
- `detail.args`: the rest of the line, with one carve-out:
  `LOGIN` records `""` and `AUTHENTICATE` records only the
  mechanism token, never the credential payload. The `+`
  continuation line for SASL is read inside the AUTH handler
  and never reaches the recorder.
- `detail.attrs` (UID FETCH only): the parsed FETCH item list as a
  JSON array of stable string labels (`"UID"`, `"FLAGS"`,
  `"INTERNALDATE"`, `"RFC822.SIZE"`, `"BODY[]"`, `"BODY[HEADER]"`,
  `"BODY[TEXT]"`, `"BODYSTRUCTURE"`, `"BODY[N]"`, `"BODY[N.MIME]"`).
  Empty array when the attribute list fails to parse; the raw line
  is still in `detail.args` for debugging.
- `detail.body` (UID FETCH only): `true` when any item in `attrs`
  asks for message bytes (`BODY[...]`, `RFC822*`, part / part-MIME
  fetches), `false` for metadata-only fetches (FLAGS / UID / MODSEQ
  / INTERNALDATE / BODYSTRUCTURE / RFC822.SIZE). Lets a steady-state
  script assert "no body refetch" while still permitting a flag-only
  reconciliation pass.

### SMTP (`src/smtp.rs::dispatch`)

- `command`: the verb in upper case (`"EHLO"`, `"MAIL"`,
  `"RCPT"`, `"DATA"`, ...). Empty input is recorded as
  `command: ""` so even malformed lines appear in the log.
- `detail.args`: the rest of the line. `AUTH` records only the
  mechanism token (e.g. `"PLAIN"`); the SASL initial-response is
  redacted. The 334 continuation line is read inside `cmd_auth`
  and never reaches the recorder.

### Microsoft Graph (`src/graph/mod.rs::log_request`)

Driven by an axum middleware that records every request matching
the Graph router (the catchall 404 included).

- `command`: `"<METHOD> <path>"` with query string stripped
  (e.g. `"GET /v1.0/me/mailFolders"`,
  `"PATCH /v1.0/me/events/ev-001"`).
- `detail.query`: the raw query string (or `null` if absent).

Mutating endpoints in `src/graph/calendar.rs` additionally append
their own row carrying the parsed body:

- `POST /v1.0/me/calendars/{calendar}/events`,
  `PATCH /v1.0/me/events/{event}`: `detail.body` is the parsed
  JSON body the client sent.
- `DELETE /v1.0/me/events/{event}`: `detail.id` is the path id
  the client targeted.
- `POST /v1.0/me/events/{event}/{accept|decline|tentativelyAccept}`
  (and the calendar-scoped
  `/v1.0/me/calendars/{cal}/events/{event}/{action}` form): the RSVP
  row carries `detail.event`, `detail.action`, and `detail.response`
  (the resolved Graph `responseStatus.response` value the mock durably
  recorded against the authenticated user's attendee).

The calendar-scoped GET / PATCH / DELETE
(`/v1.0/me/calendars/{cal}/events/{event}`) and their
`/v1.0/users/{user}/...` forms record the same `detail.body` /
`detail.id` rows as the mailbox-scoped variants.

A 404 from PATCH / DELETE on an unknown event short-circuits
before the body is parsed, so the second-row body / id detail is
absent in that case (the middleware envelope row is still
recorded).

### Gmail (`src/gmail/mod.rs::log_request`)

Same shape as the Graph middleware:

- `command`: `"<METHOD> <path>"` with query string stripped.
- `detail.query`: the raw query string (or `null` if absent).

Gmail's mutating endpoints are stubs in v0, so no
body-recording rows are emitted.

### Announce triggers (`src/announce.rs`)

A fixture `[[announce]]` trigger applies a change step before a
nominated request is served (see `notes/fixture-format.md`). Each fire
appends one row under the LISTENER's protocol tag, positioned
immediately AHEAD of the row for the request it preceded - so the log
itself shows the interleaving:

```
GET /gmail/v1/users/me/messages     <- page 1
ANNOUNCE arrive-new                 <- the change landed here
GET /gmail/v1/users/me/messages     <- page 2
```

- `command`: `"ANNOUNCE <step id>"`.
- `detail.announce`: always `true` - the key to filter on.
- `detail.step` / `detail.nth`: the step id and which matching request
  it fired before.
- `detail.applied`: `true` on success, with `detail.state` carrying
  the post-mutation primary state token.
- On failure `applied` is `false` and `detail.error` explains why
  (the script was exhausted, or the cursor was not at the named step).
  Both are authoring faults; they are recorded rather than swallowed
  because a trigger that quietly did nothing leaves a race gate
  passing while proving nothing.

### CalDAV (`src/caldav/mod.rs::dispatch`)

Every CalDAV request records a middleware-style row:

- `command`: `"<METHOD> <path>"` (the extension verbs `PROPFIND` /
  `REPORT` / `MKCALENDAR` included).
- `detail.query`: the raw query string (or `null`).

The RFC 6638 scheduling-outbox POST
(`POST /calendars/{user}/outbox/`, the iTIP REPLY bifrost submits
during an RSVP) additionally appends a second row:

- `detail.schedule_reply`: `true`.
- `detail.originator` / `detail.recipient`: the `Originator` /
  `Recipient` headers (the replying user and the organizer).
- `detail.itip`: the raw `text/calendar` iTIP REPLY body.

The durable per-attendee `PARTSTAT` change lands via the follow-up
PUT (a normal event write), not the outbox POST.

## Adding a new protocol or detail key

The contract is purely conventional - `RequestLog::record`
takes any `serde_json::Value`. When extending, prefer:

- One row per logical request the client made; if a single HTTP
  request fans into several internal recordings, that's allowed
  (Graph mutations do this) but document it here.
- Stable, lower-snake-case keys so harness scripts can grep
  without translating.
- Values that don't carry credentials. The log is exposed
  unauthenticated on `/test/requests`; SMTP / IMAP auth verbs
  redact mechanism + payload. Anything similarly sensitive
  (bearer tokens, attachment payloads) should follow the same
  pattern.
