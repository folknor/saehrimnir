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
  "protocol":   "jmap" | "imap" | "smtp" | "graph" | "gmail",
  "command":    string  (protocol-native verb)
  "received_at": RFC 3339 timestamp (wall clock; not byte-stable)
  "detail":     object  (per-protocol structured extras)
}
```

`received_at` is a wall-clock timestamp, so rendered JSON is not
byte-identical across runs. Tests that need byte-stable output
should assert on `protocol` / `command` / `detail` and ignore
`received_at`.

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
