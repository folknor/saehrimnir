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
9. Calendar sync flows through a separate `CalendarRuntime`, not via
   this code path.

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
- `sentAt`, `receivedAt` - Unix seconds. `date` defaults to `sentAt`,
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

## State tokens (out of scope for changes, IN scope for getters)

Even though `Email/changes` and `Mailbox/changes` are out of scope
for v0, the initial-sync code persists state tokens at the end:

- `get_mailbox_state` reads `state` off a bare `Mailbox/get` response.
- `get_email_state` reads `state` off an empty `Email/get` response
  (ids = `[]`).

Both must be present and non-empty strings. Any stable value is
fine - `"v0"`, `"fixture-state"`, the SHA of the fixture file -
the mock just has to return the same string consistently within a
process lifetime.

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
