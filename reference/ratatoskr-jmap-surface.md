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
  Source: `crates/jmap/src/client.rs` (`Client::connect`).
- Auth is bearer (OAuth) or basic. v0 ignores credentials entirely;
  the listener accepts any header.

## Session resource

What ratatoskr reads off the session object:

- `session.accounts()` - iterated; one account is enough.
- `account.is_personal()` - must be `true` for the only account, or
  the shared-account branches fire.
  Source: `crates/jmap/src/sync/`.
- `account.name()` - used as a fallback owner email when the account
  advertises no owner principal. Should be the account's email address
  (every mock surface follows that convention).
- `session.has_capability("urn:ietf:params:jmap:principals")` - level
  ONE of the shared-mailbox owner-email gate. The client makes NO
  owner-email plan at all without it, not even the name fallback, so a
  shared mailbox's owner address comes back as nothing. **Advertised.**
  See "Principals" below.
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

- `urn:ietf:params:jmap:principals` - advertised unconditionally.
  Principals are a property of the server, not of what the fixture
  declares, and every fixture has at least one account with an owner.
- `urn:ietf:params:jmap:submission` - advertised (the send surface
  landed); `maxDelayedSend` is non-zero so scheduled send enables.
- `urn:ietf:params:jmap:websocket` - advertised (push landed).

The client tolerates missing capabilities; it just won't take those
code paths.

## Method calls invoked during initial sync

In order, every method `jmap_initial_sync` reaches:

1. `Mailbox/get` (no ids = list all). Reads list + `state` token.
   Source: `crates/jmap/src/sync/`.
2. `Mailbox/get` again (also no ids), this time for the state token
   only - called by `get_mailbox_state` after the list is persisted.
   Same call, just discards the list.
3. `Email/query` in a loop until a page returns fewer than 50 ids.
   Per call: filter `{ "after": <UTCDate> }` (RFC3339 string per RFC
   8621, integer unix seconds also accepted by the mock), sort
   `[{"property": "receivedAt"}]`, `position`, `limit: 50`,
   `calculateTotal: true` on first page only.
   Source: `crates/jmap/src/sync/`.
4. `Email/get` per batch of 50 ids, with the property list below,
   `fetchTextBodyValues: true`, `fetchHtmlBodyValues: true`.
   Source: `crates/jmap/src/sync/`.
5. `Email/get` once more with empty ids - called by `get_email_state`
   purely to read a state token. **Mock must return a `state` field
   even when no ids were requested.**

Then unconditionally (but they won't fire if the session is shaped
right):

6. `discover_shared_accounts` - iterates `session.accounts()`, skips
   `is_personal=true`. With one personal account, nothing happens.
7. Shared-account owner-email resolution - gated on the principals
   capability, then on each account's `principals:owner`. Fires once
   per foreign account; see "Principals" below.
8. `jmap_contacts_initial_sync` - fails soft, just logs a warning.
   Plan 2 can ignore.
9. Calendar sync flows through a separate `CalendarRuntime` (the
   JMAP arm calls `<ratatoskr>/crates/jmap/src/calendar_sync/mod.rs::
   sync_calendars`); see "Calendar surface" below.

## `Email/get` property list

Exactly what `email_get_properties()` returns
(`crates/jmap/src/parse.rs`):

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

Per email (`crates/jmap/src/parse.rs`):

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
  **skipped** by the client (`parse.rs`).
- `bodyValues` - map keyed by `partId`, value
  `{ "value": "<body string>" }`. Client looks up by `partId` from
  the `textBody`/`htmlBody` arrays.
- `attachments` - array of `EmailBodyPart`. Reads `blobId` (required),
  `name`, `type`, `size`, `cid`, `disposition`. `disposition: "inline"`
  marks inline attachments.

## What ratatoskr reads off `Mailbox/get` responses

Per mailbox (`crates/jmap/src/sync/mailbox.rs`):

- `id` - required.
- `name` - string, falls back to `"(unnamed)"`.
- `role` - enum. Recognized values:
  `inbox, archive, drafts, sent, trash, junk, important`.
  Anything else stringifies to `"other"` and is treated as a generic
  user folder.
- `parentId` - optional string; resolved against the mailbox list to
  build the parent label id.
- `myRights` - object with these booleans:
  `mayReadItems, mayAddItems, mayRemoveItems, maySetSeen,
   maySetKeywords, mayCreateChild, mayRename, mayDelete, maySubmit`.
- `isSubscribed` - boolean.
- `state` - top-level on the response, NOT per-mailbox. Returned by
  the wrapping `Mailbox/get` response.

Counts (`totalEmails`, `unreadEmails`, etc.) and `sortOrder` are not
read by the parser, but the mock still emits them. Either way works.

## `Email/query` shape

What the client sends (`crates/jmap/src/sync/`):

- `accountId` - defaulted to the session's primary mail account.
- `filter` - for initial sync: `{ "after": <UTCDate> }`. Per RFC 8621
  §4.4.1, `after`/`before` are `UTCDate` strings (RFC3339, "Z"-suffixed,
  e.g. `"2026-01-15T11:00:00Z"`). The mock parser also accepts a
  unix-seconds integer for legacy callers. `after` is inclusive
  (`receivedAt >= after`); `before` is exclusive (`receivedAt < before`).
  For thread-scoped lookups (used outside initial sync):
  `{ "inThread": "<thread_id>" }`.
  v0 needs `after`/`before` only; `inThread` can be a v1 concern.
- `sort` - `[{ "property": "receivedAt" }]`. ratatoskr does not
  pass `isAscending`,
  but reads results in the order returned. **For determinism, sort
  by `receivedAt` descending and break ties by `id` lexicographic.**
  (Plan 2 explicitly decides this; the client doesn't care about
  direction at the query level since it persists everything.)
- `position` - int offset (0 on first page, then accumulates).
- `limit` - `50` (`BATCH_SIZE` in `sync/mod.rs`).
- `calculateTotal` - `true` on first page, `false` after.

What the client reads off the response:

- `ids` - array of email ids.
- `total` - only consumed on first page (`query_result.total()`).
- Loop terminates when `ids.len() < BATCH_SIZE` (i.e., the last
  partial page).

So the mock must:
- Honor `position` and `limit`.
- Return `total` on every response (the client always includes it
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
  `keywords/<flag>` (`true` to add, `null` or `false` to remove).
- `mailboxIds` (full replace, `String[true]`) and
  `mailboxIds/<id>` (`true` to add, `null` or `false` to remove).
- Other patch paths return `notUpdated[<id>] = invalidProperties`.

Both properties are RFC 8620 `Set`s, so a member's value MUST be
`true`: on the full-replace form a key mapped to `false` is NOT a
member and is dropped, exactly as if it had been omitted. That keeps
the two forms agreeing on what `false` means.

### `mailboxIds` can never end up empty

RFC 8621 §4.1.1: an Email "MUST belong to one or more Mailboxes at all
times". JMAP has no All Mail, so a mutation whose result is an empty
`mailboxIds` has no representable outcome, and the mock refuses it
with a per-item `SetError`:

```json
"notUpdated": { "email-001": {
  "type": "invalidProperties",
  "description": "mailboxIds must name at least one mailbox: ..."
}}
```

This covers every shape that reaches the empty set:

- full replace with `{}` or with an all-`false` map;
- `mailboxIds/<id>` set to `null` or `false` where `<id>` is the last
  remaining membership;
- `Email/set` create and `Email/import` with an empty / all-`false`
  `mailboxIds` (RFC 8621 §4.6 requires the new Email to name a
  Mailbox); those land in `notCreated`.

The check runs once after the whole patch is folded in, so it is
order-insensitive: a patch that drops one mailbox and adds another
nets out non-empty and applies. A refusal is per item and total - no
partial mutation, no state bump, no transition appended, so the next
`Email/changes` shows nothing for that id and any sibling update in
the same envelope still applies.

Contrast with the other protocols, which genuinely disagree here:

- **Gmail** treats "no container" as a real state (All Mail), so
  `batchModify`/`modify` removing the last container is an ARCHIVE and
  the message lands in the `role = "archive"` mailbox. See
  `reference/ratatoskr-gmail-surface.md`.
- **IMAP** destroys it: `EXPUNGE` on the last mailbox membership
  removes the message outright.
- **Graph** never reaches the case; a move always writes exactly one
  non-empty parent folder.

A well-behaved client never produces the empty shape anyway (bifrost's
bulk move REPLACES membership with the destination). The rejection
exists so a buggy or hostile mutation gets the answer a conforming
server gives, instead of the mock manufacturing an unmailboxed email
that downstream sync would then ingest.

`Mailbox/set` honours `name`, `parentId`, `sortOrder`, `role`,
`isSubscribed` on both create and update. Destroy fails with
`mailboxHasEmail` while any email still references the mailbox
in its `mailboxIds`, mirroring real JMAP servers.

Server-assigned ids are deterministic for byte-stable transcripts:
created emails come back as `mock-email-<n>` (1-based, counted
against `fixture.emails.len()` at create time) and created
mailboxes as `mock-mailbox-<n>`. Counter values reset across
fixture loads but advance monotonically within one process.

## `Thread/get`

RFC 8621 §3. Not part of `jmap_initial_sync`, but bifrost's JMAP
`Account::open` probes it during account discovery (`discover`); if
the method returns `unknownMethod` the open fails with
`Wire(Jmap(UnknownMethod))` and no sync runs at all. The legacy
client's open path never needed it, so this only surfaced once
account-open was routed through bifrost's JMAP account.

What the mock serves:

- Request: `{ accountId, ids }`. `ids = null` (or omitted) lists
  every thread in the account; an explicit id array returns matched
  threads, unknown ids land in `notFound`.
- Per Thread object: `{ id, emailIds }`. The mock has no separate
  thread resource - threads are derived from each email's
  `thread_id` (which defaults to the email's own id when the fixture
  doesn't set one, so an un-threaded fixture is all single-message
  threads). `emailIds` is sorted by `receivedAt` ascending (RFC 8621
  §3) with `id` lexicographic as a deterministic tiebreak.
- Reads scope to the request's `accountId` via `emails_for`, so a
  multi-account fixture's secondary threads don't leak.
- `state` reuses the fixture-level state token, like the other
  `/get` responses.

`Thread/changes` (RFC 8621 §3.2) is implemented: bifrost seeds a
Thread cursor at open and drives it on the first delta cycle. The
mock has no thread-specific change log, so it projects the
per-account email delta (`email_delta_since_account`) onto threads -
threads of created emails go to `created`, of updated emails to
`updated` (deduped). `destroyed` is always empty: a destroyed
email's `thread_id` is unrecoverable from the log, so v0 emits no
thread tombstones; bifrost re-fetches the reported threads via
`Thread/get` and reconciles (an emptied thread reads back with empty
`emailIds`). Unknown / evicted `sinceState` -> `cannotCalculateChanges`.

## Contacts (`AddressBook/*` + `ContactCard/*`)

RFC 9610 (JMAP for Contacts) over RFC 9553 (JSContact). A
`ContactCard` is a JSContact Card object plus a server-set
`addressBookIds` membership map; an `AddressBook` is the JMAP
projection of a contact folder. Handlers live in
`src/jmap_contacts.rs`.

Where it sits in the client flow (bifrost `crates/jmap/src/sync/
contacts.rs`):

- Account open does NOT touch contacts. `factory.rs::open` resolves
  the contacts account with `client.primary_account::<Contacts>()
  .ok()` and probes only email / mailbox / thread state. So unlike
  `Thread/get`, a missing contacts surface does not block open.
- Initial sync: `AddressBook/get` (all) -> `ContactCard/query`
  (paged, `calculateTotal`, optional `inAddressBook` / `text`
  filter) -> `ContactCard/get` (by ids) -> upsert. Delta sync uses
  `ContactCard/changes`; write-back uses `ContactCard/set`.
- bifrost reaches the contacts account only when the session
  advertises `urn:ietf:params:jmap:contacts` in both
  `accounts[].accountCapabilities` and `primaryAccounts`. The mock
  advertises it whenever the fixture carries any
  `[[contact_folder]]` (gated like `:calendars`).

What bifrost reads off an AddressBook (`address_book_from_jmap`):
`id`, `name` (defaults to "Address Book"), `isDefault`, and
`myRights` (`mayWrite` / `mayDelete` gate create/update/delete). The
mock emits all `myRights` true.

What bifrost reads off a card (`contact_from_jmap`):

- `id` - the JMAP record id (string).
- `addressBookIds` - object; bifrost takes the FIRST key as the
  card's address book.
- `name.full` - display name (falls back to `name.given`).
- `emails` - object of `{ address, contexts, pref }`; bifrost reads
  `address` (and `pref == 1` for primary, first `contexts` key for
  kind). Other JSContact maps it can read when present: `phones`
  (`number`), `organizations` (`name` / `title`), `addresses`,
  `notes` (`note`), `media` (`kind == "photo"` -> `uri`).

What the v0 mock serves:

- `AddressBook/get` - one AddressBook per fixture `ContactFolder`
  (`id`, `name`, `isDefault`, `sortOrder: 0`, `isSubscribed: true`,
  `myRights` all-true). `ids = null` lists; an id array partitions.
- `ContactCard/get` - a Card per fixture `Contact` with `@type`
  `"Card"`, `version` `"1.0"`, `id` + `uid` (both the contact id),
  `addressBookIds: { <folder_id>: true }`, `kind: "individual"`,
  `name.full` when the contact has a display name, and `emails`
  keyed `e1`, `e2`, ... The fixture `Contact` has no phones / orgs /
  addresses / notes / media, so those are omitted.
- `ContactCard/query` - `Email/query`-shaped envelope. Supports
  `inAddressBook` (folder membership) and `text` (case-insensitive
  substring over display name + email address) filters, plus
  `position` / `limit` / `calculateTotal`. Ids sort ascending for
  byte-stable paging.
- `ContactCard/changes` - account-scoped union of
  `contact_delta_since` over the account's folders (JMAP has no
  per-address-book filter on `/changes`). Unknown / evicted
  `sinceState` returns `cannotCalculateChanges`.
- `ContactCard/set` - create / update / destroy through
  `Fixture::mutate`, recording `contact_*` transitions (so a JMAP
  create surfaces in a follow-up Graph `contacts/delta`). Create
  mints `mock-contact-<n>` and pins the card's account from its
  `addressBookIds` folder. `name` / `emails` / `addressBookIds`
  (full-replace or bifrost's `{old: null, new: true}` move shape)
  apply; `phones` / `organizations` / `notes` / `media` are accepted
  but not durably stored (the fixture `Contact` has no slots),
  mirroring the People-API write-back.

All reads scope by `accountId`. Still out of scope
(`unknownMethod`): `AddressBook/set`, `AddressBook/changes`,
`ContactCard/copy`, `ContactCard/queryChanges`, and the
`ContactCard/query` sort comparators (the mock ignores `sort` and
always orders by id).

## Constants worth knowing

- `BATCH_SIZE = 50` - `Email/query` limit, `Email/get` chunk size.
  `crates/jmap/src/sync/mod.rs`.
- `JMAP_MAX_CHANGES = 500` - `Email/changes` / `Mailbox/changes`
  batch cap. `crates/jmap/src/lib.rs`. v1+ concern.
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
  into shared-account sync (which is the point of
  `is_personal = false`, but a single-account fixture must not stage
  one by accident).
- A `principals:owner` capability naming a `principalId` that
  `Principal/get` cannot resolve. The owner-email path deliberately
  does NOT fall back to the account name once a principal id is
  present, so an unresolvable id is worse than no id at all: it
  silently yields no owner email.
- An `Email/query` final page that returns exactly 50 ids - the loop
  re-queries with `position += 50` and the mock must terminate.
  Either return `< 50` on the last page, or return an empty page
  next.

## Principals (RFC 9670)

Scope: resolving the OWNER EMAIL of a foreign / shared account, and
nothing else. `src/jmap_principals.rs`.

The consumer's gate is two levels deep and both must pass:

1. the SESSION advertises `urn:ietf:params:jmap:principals`. Without
   it no owner-email plan is made for any account - not even the
   account-name fallback - and every shared mailbox's owner resolves
   to nothing.
2. the individual ACCOUNT advertises
   `urn:ietf:params:jmap:principals:owner` carrying a `principalId`.
   That is what routes the account through `Principal/get`; an account
   with the capability but no `principalId` falls back to its session
   `name` when the name looks like an address.

Shapes served:

```jsonc
// session capabilities
"urn:ietf:params:jmap:principals": {
  "currentUserPrincipalId": "principal-<caller account id>",
  "accountIdForPrincipal": "<caller account id>"
}
// per-account accountCapabilities, on EVERY account
"urn:ietf:params:jmap:principals:owner": {
  "accountIdForPrincipal": "<caller account id>",
  "principalId": "principal-<that account's id>"
}
```

`Principal/get` returns the RFC 8620 get envelope - `accountId`,
`state`, `list`, `notFound`, all four mandatory - with each principal
projected as `{ id, type: "individual", name, email }`. `name` and
`email` are both the fixture `[[account]] name`, which is the account's
email address by mock-wide convention.

Principal ids derive from account ids (`principal-<account_id>`), so
the mapping is total and reversible with no fixture authoring.

Two traps, both of which make the read silently return nothing rather
than fail:

- The lookup must NOT be scoped by the request's `accountId`. The
  consumer asks its OWN account (`accountIdForPrincipal`) for the
  principal of a DIFFERENT, foreign account; scoping would put every
  shared owner in `notFound`.
- The whole path fails soft on the consumer side. A missing `state`, a
  missing `notFound`, or an `email` under a different key does not
  error - it yields no owner email at all, which reads exactly like
  "this share has no owner".

Out of scope, and honestly `unknownMethod`: `Principal/set`,
`Principal/changes`, `Principal/query`, and the `ShareNotification`
family.

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
- `CalendarEvent/query` - the read path bifrost's calendar sync
  actually drives (`sync/calendar_ops.rs::events_in_range`): an
  `Email/query`-shaped envelope. Supports an AND FilterOperator
  (`{ operator, conditions }`) of `inCalendar` + `after` + `before`
  (and `text` for search), plus `position` / `limit` /
  `calculateTotal`. Events sort by `start` ascending (id tiebreak).
  bifrost re-filters the time range client-side, so the mock's
  overlap test is coarse (`end > after`, `start < before`).
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
- bifrost's reqwest layer handles 401/redirects/retries - v0
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
