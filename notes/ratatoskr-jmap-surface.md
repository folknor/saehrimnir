# Ratatoskr's JMAP client surface

What the v0 mock has to satisfy. Distilled from `<ratatoskr>/crates/jmap/`
on 2026-05-06. Source-of-truth lives there; this file is a cheat sheet
so we don't have to fan out every turn.

Each entry below names the file/line where the behavior is observable,
so the next person can re-verify after the client drifts.

## Connection

- The client opens the session by calling `Client::connect(&jmap_url)`
  directly with whatever URL is in the account row's `jmap_url` column.
  No `.well-known/jmap` probe in the ratatoskr path. **v0 mock does
  not need a `.well-known` route**; the configured endpoint is hit as
  the session URL.
  Source: `crates/jmap/src/client.rs:319-326`.
- Auth is bearer (OAuth) or basic. v0 ignores credentials entirely;
  the listener accepts any header.

## Session resource

What ratatoskr reads off the session object:

- `session.accounts()` - iterated; one account is enough.
- `account.is_personal()` - must be `true` for the only account, or
  the shared-account branches fire.
  Source: `crates/jmap/src/sync/mod.rs:611-625`.
- `account.name()` - used as a fallback email when principals lookup
  fails. Should be the account's email address.
- `session.has_capability("urn:ietf:params:jmap:principals")` - gates
  `Principal/get` and `ShareNotification/changes` paths. **Do NOT
  advertise this capability in v0.**
  Source: `crates/jmap/src/sync/mod.rs:699`, `:842`.
- The default account id (used when method calls don't specify an
  override) comes from `request.default_account_id()`, populated from
  the session's `primaryAccounts["urn:ietf:params:jmap:mail"]`.

Capabilities the session MUST advertise:

- `urn:ietf:params:jmap:core`
- `urn:ietf:params:jmap:mail`
- `urn:ietf:params:jmap:calendars` - advertised iff the fixture
  carries any `[[calendar]]` entry. Pulls the client into the
  calendar sync flow at `<ratatoskr>/crates/jmap/src/calendar_sync/
  mod.rs:sync_calendars`.

Capabilities to NOT advertise (each pulls the client into work the
mock can't satisfy in v0):

- `urn:ietf:params:jmap:principals` - triggers `Principal/get` and
  `ShareNotification/changes` polling.
- `urn:ietf:params:jmap:submission` - only matters once we send.
- `urn:ietf:params:jmap:websocket` - only matters once we push.

The client tolerates missing capabilities; it just won't take those
code paths.

## Method calls invoked during initial sync

In order, every method `jmap_initial_sync` reaches:

1. `Mailbox/get` (no ids = list all). Reads list + `state` token.
   Source: `crates/jmap/src/sync/mailbox.rs:281-287`.
2. `Mailbox/get` again (also no ids), this time for the state token
   only - called by `get_mailbox_state` after the list is persisted.
   Same call, just discards the list. Source: `:217`.
3. `Email/query` in a loop until a page returns fewer than 50 ids.
   Per call: filter `{ "after": <UTCDate> }` (RFC3339 string per RFC
   8621, integer unix seconds also accepted by the mock), sort
   `[{"property": "receivedAt"}]`, `position`, `limit: 50`,
   `calculateTotal: true` on first page only.
   Source: `crates/jmap/src/sync/mod.rs:241-269`.
4. `Email/get` per batch of 50 ids, with the property list below,
   `fetchTextBodyValues: true`, `fetchHtmlBodyValues: true`.
   Source: `crates/jmap/src/sync/mod.rs:444-472`.
5. `Email/get` once more with empty ids - called by `get_email_state`
   purely to read a state token. **Mock must return a `state` field
   even when no ids were requested.** Source: `:236-258`.

Then unconditionally (but they won't fire if the session is shaped
right):

6. `discover_shared_accounts` - iterates `session.accounts()`, skips
   `is_personal=true`. With one personal account, nothing happens.
7. `resolve_shared_account_identities` - gated on principals
   capability. Won't fire.
8. `jmap_contacts_initial_sync` - fails soft, just logs a warning.
   Plan 2 can ignore.
9. Calendar sync flows through a separate `CalendarRuntime` (the
   JMAP arm calls `<ratatoskr>/crates/jmap/src/calendar_sync/mod.rs::
   sync_calendars`); see "Calendar surface" below.

## `Email/get` property list

Exactly what `email_get_properties()` returns
(`crates/jmap/src/parse.rs:35-63`):

```
id, blobId, threadId, mailboxIds, keywords, size, receivedAt,
messageId, inReplyTo, references, from, to, cc, bcc, replyTo,
subject, sentAt, hasAttachment, preview, textBody, htmlBody,
attachments,
header:List-Unsubscribe:asText,
header:List-Unsubscribe-Post:asText,
header:Disposition-Notification-To:asText
```

The three header property names round-trip as keys in the response -
returning `null` is fine, but the keys must be present (or absent
without erroring). The client is tolerant of `null`.

`fetchTextBodyValues` / `fetchHtmlBodyValues` are sent as arguments
on the `Email/get` call (see RFC 8621 §4.2.2). Both are `true` for
ratatoskr's path.

## What ratatoskr reads off `Email/get` responses

Per email (`crates/jmap/src/parse.rs:72-197`):

- `id` - required, errors if missing.
- `threadId` - required, errors if missing.
- `from` - array of `{name, email}`; ratatoskr uses only the first.
- `to`, `cc`, `bcc`, `replyTo` - arrays of `{name, email}`.
- `subject`, `preview` - strings, both optional.
- `sentAt`, `receivedAt` - RFC3339 strings per RFC 8621 §4.1.1
  (`receivedAt` is `UTCDate`, "Z"-suffixed; `sentAt` is `Date`, which
  may carry an offset but the mock emits the "Z"-suffixed UTC form
  since fixture timestamps are UTC). `date` defaults to `sentAt`,
  falls back to `receivedAt`. `internalDate` is `receivedAt`.
- `keywords` - map; `$seen` -> read, `$flagged` -> starred. Non-`$`
  keys become user category labels.
- `mailboxIds` - map of mailbox id to `true`.
- `hasAttachment`, `size` - direct.
- `messageId`, `references`, `inReplyTo` - arrays of strings; joined
  with spaces for DB storage.
- `textBody`, `htmlBody` - arrays of `EmailBodyPart`. Each has
  `partId` and `type`. Parts with `type: "text/x-amp-html"` are
  **skipped** by the client (`parse.rs:231`).
- `bodyValues` - map keyed by `partId`, value
  `{ "value": "<body string>" }`. Client looks up by `partId` from
  the `textBody`/`htmlBody` arrays.
- `attachments` - array of `EmailBodyPart`. Reads `blobId` (required),
  `name`, `type`, `size`, `cid`, `disposition`. `disposition: "inline"`
  marks inline attachments.

## What ratatoskr reads off `Mailbox/get` responses

Per mailbox (`crates/jmap/src/sync/mailbox.rs:36-95`):

- `id` - required.
- `name` - string, falls back to `"(unnamed)"`.
- `role` - enum. Recognized values:
  `inbox, archive, drafts, sent, trash, junk, important`.
  Anything else stringifies to `"other"` and is treated as a generic
  user folder. Source: `crates/jmap/src/sync/mailbox.rs:294-306`.
- `parentId` - optional string; resolved against the mailbox list to
  build the parent label id.
- `myRights` - object with these booleans:
  `mayReadItems, mayAddItems, mayRemoveItems, maySetSeen,
   maySetKeywords, mayCreateChild, mayRename, mayDelete, maySubmit`.
  Source: `crates/jmap/src/sync/mailbox.rs:323-334`.
- `isSubscribed` - boolean.
- `state` - top-level on the response, NOT per-mailbox. Returned by
  the wrapping `Mailbox/get` response.

Counts (`totalEmails`, `unreadEmails`, etc.) and `sortOrder` are not
read by the parser, but the mock still emits them. Either way works.

## `Email/query` shape

What the client sends (`crates/jmap/src/sync/mod.rs:248-258`,
`helpers.rs:11-21`):

- `accountId` - defaulted to the session's primary mail account.
- `filter` - for initial sync: `{ "after": <UTCDate> }`. Per RFC 8621
  §4.4.1, `after`/`before` are `UTCDate` strings (RFC3339, "Z"-suffixed,
  e.g. `"2026-01-15T11:00:00Z"`). The mock parser also accepts a
  unix-seconds integer for legacy callers. `after` is inclusive
  (`receivedAt >= after`); `before` is exclusive (`receivedAt < before`).
  For thread-scoped lookups (used outside initial sync):
  `{ "inThread": "<thread_id>" }`.
  v0 needs `after`/`before` only; `inThread` can be a v1 concern.
- `sort` - `[{ "property": "receivedAt" }]`. Direction defaults to
  ascending in jmap-client; ratatoskr does not pass `isAscending`,
  but reads results in the order returned. **For determinism, sort
  by `receivedAt` descending and break ties by `id` lexicographic.**
  (Plan 2 explicitly decides this; the client doesn't care about
  direction at the query level since it persists everything.)
- `position` - int offset (0 on first page, then accumulates).
- `limit` - `50` (`BATCH_SIZE` in `sync/mod.rs:22`).
- `calculateTotal` - `true` on first page, `false` after.

What the client reads off the response:

- `ids` - array of email ids.
- `total` - only consumed on first page (`query_result.total()`).
- Loop terminates when `ids.len() < BATCH_SIZE` (i.e., the last
  partial page).

So the mock must:
- Honor `position` and `limit`.
- Return `total` on every response (jmap-client always includes it
  in the deserialized `QueryResponse`); the client just only uses
  the first one.
- Return fewer than 50 ids on the final page so the loop exits.

## State tokens

The initial-sync code persists state tokens at the end:

- `get_mailbox_state` reads `state` off a bare `Mailbox/get` response.
- `get_email_state` reads `state` off an empty `Email/get` response
  (ids = `[]`).

Both must be present and non-empty strings. Any stable value is
fine - `"v0"`, `"fixture-state"`, the SHA of the fixture file -
the mock just has to return the same string consistently within a
process lifetime.

## `Mailbox/changes` and `Email/changes`

Wired against the real per-state change log
(`Fixture::change_log`). The seed state is set at fixture-load
time; each successful `Email/set` / `Mailbox/set` envelope appends
a transition and bumps `Fixture::state` to `<seed>.<n>` (counter
monotonic). Semantics:

- `sinceState == fixture.state` -> empty delta. `newState` echoes
  back, `hasMoreChanges = false`, `created/updated/destroyed = []`.
  `Email/changes` additionally returns `updatedProperties: null`
  per RFC 8621 §4.2.
- `sinceState` matches the seed or any retained transition's
  `from_state` -> walk forward, union the per-resource ids with
  RFC 8620 §5.2 dominance: created+destroyed cancels, created+
  updated collapses to created, destroyed+updated collapses to
  destroyed. Per-list dedup preserves first-seen order so the
  byte-stable invariant holds.
- `sinceState` is unknown (older than seed, or evicted from the
  bounded ring at `ChangeLog::MAX_TRANSITIONS = 256`) ->
  `cannotCalculateChanges`. The client falls back to a fresh
  `Email/query` + `Email/get` round.

A fixture with no recorded mutations (the post-load steady state)
hits the first branch: any `sinceState == seed` returns empty.
That preserves the v0 "no change history" behaviour for read-only
fixtures while letting tests that mutate prove the round-trip
through delta.

## `Email/set` and `Mailbox/set`

RFC 8621 §4.6 (`Email/set`) and §2.5 (`Mailbox/set`). v0 accepts
the canonical create / update / destroy maps; mutations apply
in-place to the fixture, bump `state`, and record a transition
visible to the next `Email/changes` / `Mailbox/changes`.

`ifInState`, when present, is checked against the current state
before any mutation; mismatch returns `stateMismatch`.

`Email/set` patch shapes ratatoskr drives:

- `keywords` (full replace, `String[Boolean]`) and
  `keywords/<flag>` (`true` to add, `null` to remove).
- `mailboxIds` (full replace, `String[true]`) and
  `mailboxIds/<id>` (`true` to add, `null` to remove).
- Other patch paths return `notUpdated[<id>] = invalidProperties`.

`Mailbox/set` honours `name`, `parentId`, `sortOrder`, `role`,
`isSubscribed` on both create and update. Destroy fails with
`mailboxHasEmail` while any email still references the mailbox
in its `mailboxIds`, mirroring real JMAP servers.

Server-assigned ids are deterministic for byte-stable transcripts:
created emails come back as `mock-email-<n>` (1-based, counted
against `fixture.emails.len()` at create time) and created
mailboxes as `mock-mailbox-<n>`. Counter values reset across
fixture loads but advance monotonically within one process.

## Constants worth knowing

- `BATCH_SIZE = 50` - `Email/query` limit, `Email/get` chunk size.
  `crates/jmap/src/sync/mod.rs:22`.
- `JMAP_MAX_CHANGES = 500` - `Email/changes` / `Mailbox/changes`
  batch cap. `crates/jmap/src/lib.rs:9`. v1+ concern.
- `MAILBOX_CACHE_TTL = 60s` - client-side TTL on the mailbox list.
  Doesn't affect the wire protocol.

## Things the mock can return as null/empty without breaking sync

- The three custom `header:*:asText` properties.
- `bcc`, `replyTo`, `inReplyTo`, `references`, `messageId`.
- `attachments` (empty array).
- `hasAttachment: false` paired with empty `attachments`.
- The non-mail capabilities (`submission`, `websocket`, `sieve`).
- `parentId`, `role`, `myRights`, `isSubscribed` on mailboxes.

## Things that WILL break sync if wrong

- Missing `state` on `Mailbox/get` or `Email/get` responses.
- Missing `id` or `threadId` on an email - parser errors on either.
- A non-personal account in `session.accounts()` - pulls the client
  into shared-account sync.
- Advertising `urn:ietf:params:jmap:principals` - pulls the client
  into `Principal/get` and `ShareNotification/changes`.
- An `Email/query` final page that returns exactly 50 ids - the loop
  re-queries with `position += 50` and the mock must terminate.
  Either return `< 50` on the last page, or return an empty page
  next.

## Calendar surface

ratatoskr's JMAP calendar arm
(`<ratatoskr>/crates/jmap/src/calendar_sync/`) drives a small subset
of the JMAP Calendars spec backed by JSCalendar (RFC 8984) event
objects. The wire shape needs:

- `Calendar/get` - no ids = list all. Reads `id`, `name`, `color`,
  `isDefault` per entry; ignores `myRights` / `isVisible` /
  `isSubscribed` / `sortOrder` but they must serialize cleanly.
  `state` is read for the calendar-list state token; we reuse the
  fixture-level `state` here.
- `Calendar/changes` - v0 only handles `sinceState ==
  fixture.state` (empty delta). Anything else returns
  `cannotCalculateChanges`; the client tolerates that and falls
  back to a fresh `Calendar/get`.
- `CalendarEvent/get` - no ids = list all. Returns JSCalendar
  objects with `id`, `uid`, `calendarIds`, `title`, `description`,
  `start` (LocalDateTime "%Y-%m-%dT%H:%M:%S" or "%Y-%m-%d" all-day),
  `duration` (ISO 8601), `timeZone` ("UTC"), `showWithoutTime` for
  all-day, `status` ("confirmed"), `locations` (`{"loc1": {"@type":
  "Location", "name": ...}}`), `participants` (`{"<id>": {"@type":
  "Participant", "email", "sendTo": {"imip": "mailto:<addr>"},
  "name", "roles": {"owner"|"attendee": true}}}`).
  ratatoskr reads back via
  `<ratatoskr>/crates/jmap/src/calendar_sync/payload.rs` -
  `extract_location`, `resolve_calendar_id`,
  `extract_organizer_email`, `extract_attendees_json`,
  `parse_jscalendar_times`.
- `CalendarEvent/changes` - walks the change_log via
  `Fixture::event_delta_since` unioned across every declared
  calendar, since JMAP carries no calendar-id filter on
  `/changes`. Returns `cannotCalculateChanges` for unknown / evicted
  states. Source: `<ratatoskr>/crates/jmap/src/calendar_sync/
  mod.rs::sync_events_delta`.
- `CalendarEvent/set` - create / update / destroy. Mutations bump
  `Fixture::state` and record `event_*` transitions (same shape
  Graph and CalDAV writes use), so a JMAP create surfaces in a
  follow-up Graph `calendarView/delta`. Source:
  `<ratatoskr>/crates/jmap/src/calendar_sync/protocol.rs`.

Things that WILL break calendar sync if wrong:

- `cannotCalculateChanges` must use that exact error type string;
  ratatoskr checks for it in
  `sync_events_delta`'s error mapper to decide on a full re-sync.
- `start` must be a JSCalendar LocalDateTime (no Z suffix). RFC 3339
  with offset is parsed as a fallback in ratatoskr but the
  canonical wire form is local.

## Headers, transport, status codes

- `POST /jmap/api` with `Content-Type: application/json` and a body
  shaped per RFC 8620 §3.3 (`{"using": [...], "methodCalls": [[name,
  args, callId], ...]}`).
- Response is `{"methodResponses": [[name, result, callId], ...],
  "sessionState": "<string>"}`.
- jmap-client's reqwest layer handles 401/redirects/retries - v0
  always returns 200 on the API endpoint.
- The session URL accepts `GET` and returns `application/json`.

## Cross-plan contract (resolved)

These were open questions until plans 1 and 3 were read; resolutions
recorded here so we don't have to fan out again. Full detail in
`orchestration.md`.

- **Readiness sentinel.** Brokkr's `wait_for_sentinel` is
  presence-only (returns `Appeared` / `BackstopExpired`). The
  sentinel's `JMAP <port>\n` line content is for plan-3-side port
  extraction, not for the watcher. Atomic write (temp + rename)
  required.
- **Endpoint env var name.** Default `RATATOSKR_TEST_JMAP_ENDPOINT`,
  overridable via `[ratatoskr] test_endpoint_env_jmap` in
  ratatoskr's brokkr.toml. We don't read it; the harness binary
  does. We just bind a port and report it via the sentinel.
- **Fixture directory contract.** No manifest file. Brokkr resolves
  `<fixtures_dir>/<name>.toml` and passes the path to us via
  `--fixture`. The fixture's internal `name = "..."` field is
  informational, not load-bearing.
