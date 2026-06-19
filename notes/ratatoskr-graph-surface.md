# Ratatoskr's Microsoft Graph client surface

What the v0 Graph mock has to satisfy. Distilled from
`<ratatoskr>/crates/graph/` on 2026-05-06. Source-of-truth lives
there; this file is a cheat sheet so we don't have to fan out every
turn.

Mail-sync, calendar, and contacts are wired in v0. Other resource
categories (OneDrive, groups, labels, public folders via EWS,
shared mailboxes, webhooks, autodiscover) are listed at the end so
the next reader knows what scaffolding the module structure has to
accommodate.

## Profile / account-open (`src/graph/profile.rs`)

`GraphAccountFactory::open` (bifrost `crates/graph/src/account/mod.rs:288`)
issues `GET /me?$select=displayName,mail,userPrincipalName` as its
FIRST request, then derives the account's own address from
`profile.mail.or(profile.user_principal_name)`. This must succeed or
the account never opens (the bare `/v1.0/me` path used to fall to the
catchall 404).

| Endpoint | Behaviour |
|---|---|
| `GET /v1.0/me` | Bearer-resolved account (`oauth::account_from_bearer`, fallback-to-primary), projected as a Graph user: `id`, `displayName`, `mail`, `userPrincipalName` (all derived from `account.name` since the fixture `Account` carries only id + email). `$select` is ignored (we always emit the full set). |
| `GET /v1.0/users/{id}` | The named declared account; `me` aliases the primary; unknown id 404s `ResourceNotFound`. Same projection. |
| `GET /v1.0/users` + `GET /v1.0/me/users` | GAL directory search (bifrost's `directory_search`, which addresses `/me/users`). Matches declared accounts by `startswith(displayName,'X') or startswith(mail,'X')` (case-insensitive); no filter lists all. Returns a `value` collection of bare user entities. |

## Calendar (`src/graph/calendar.rs`)

GET endpoints project from `[[calendar]]` and `[[event]]` fixture
entries; mutating endpoints echo their request body so tests can
assert on what the client tried to write without mutating the
fixture.

| Endpoint | Notes |
|----------|-------|
| `GET /v1.0/me/calendars` | OData-shaped envelope. Order = fixture declaration order. |
| `GET /v1.0/me/calendars/{id}` | `id` accepts the literal id or the alias `default` (resolves to the first calendar with `is_default = true`, then the first declared calendar). |
| `GET /v1.0/me/calendars/{id}/events` | `$top` / `$skiptoken` paginate; default top is 50, max 256. `@odata.nextLink` is emitted while a window remains. |
| `GET /v1.0/me/calendars/{id}/calendarView?startDateTime=&endDateTime=` | The non-delta time-range read bifrost's `events_in_range` drives. Filters the calendar's events to those overlapping `[startDateTime, endDateTime)` (coarse - bifrost re-checks client-side); absent bound is open on that side. `startDateTime` / `endDateTime` are non-`$` params; `$top` / `$skiptoken` paginate. |
| `GET /v1.0/me/calendars/{id}/calendarView/delta` | First call (no `$deltatoken`) returns the full event list; follow-up calls walk `Fixture::change_log` between the supplied state and the current state, returning created/updated events as full bodies and destroyed events as Graph tombstones (`{ id, "@removed": { reason: "deleted" } }`). `@odata.deltaLink` carries the post-mutation fixture state. Unknown / evicted token falls back to a fresh bootstrap. |
| `GET /v1.0/me/events/{id}` | Single event. 404 if not declared. |
| `POST /v1.0/me/calendars/{id}/events` | 201 with the freshly created event projected via `serialize_event`. Server id is `mock-event-N` (1-based, counted against current `fixture.events.len()`). Mutates the fixture under a write guard, bumps `Fixture::state`, records `event_created` in the change log. Logs `(graph, "POST /v1.0/me/calendars/{id}/events", { body })` to the request log. |
| `PATCH /v1.0/me/events/{id}` | 200 with the post-patch event. Honours `subject`, `start`, `end`, `body.content`, `location.displayName`, `isAllDay`, `attendees`. Records `event_updated`. |
| `DELETE /v1.0/me/events/{id}` | 204 with no body. Removes the event and records `event_destroyed`. Logs `(graph, "DELETE /v1.0/me/events/{id}", { id })`. |
| `POST /v1.0/me/events/{id}/{accept\|decline\|tentativelyAccept}` | RSVP (bifrost addresses it via `/me/events/{id}`, not the calendar-scoped path). 202 Accepted; accept-and-ignore - the fixture `Event` has no per-attendee response-status slot, so nothing durably changes and no transition is recorded. Unknown action -> 400, unknown event -> 404. |

Event projection populates `subject`, `bodyPreview`, `body`
(`contentType: "text"`), `start`/`end` (`{ dateTime, timeZone }`,
always `UTC`), `isAllDay`, `location.displayName`, `organizer.
emailAddress`, and `attendees[].emailAddress` with `type =
"required"`. Attendee tone (`required`/`optional`) and
`responseStatus` are not yet projected; add when a fixture forces
it.

## Contacts (`src/graph/contacts.rs`)

GET endpoints project from `[[contact_folder]]` and `[[contact]]`
fixture entries; mutations land via change-script ops only (the
fixture is otherwise read-only across this surface in v0). Wire
shape matches what ratatoskr's `GraphContact` /
`GraphContactFolder` deserialise: `id`, `displayName`,
`emailAddresses` (array of `{ name?, address }`), and
`parentFolderId`.

| Endpoint | Notes |
|----------|-------|
| `GET /v1.0/me/contactFolders` | Paged via `$top` / `$skiptoken`; default top 100, max 250. Order = fixture declaration order. |
| `GET /v1.0/me/contactFolders/{id}` | `id` accepts the literal id or the alias `default` (resolves to the folder with `is_default = true`). |
| `GET /v1.0/me/contactFolders/{id}/contacts` | Paged via `$top` / `$skiptoken`; default top 50, max 999 (matches ratatoskr's `?$top=999`). `$select` is parsed and ignored; we always emit the full `id, displayName, emailAddresses, parentFolderId` projection. |
| `GET /v1.0/me/contactFolders/{id}/contacts/{cid}` | Single contact scoped to a folder. 404 if id mismatches the folder. |
| `GET /v1.0/me/contacts/{cid}` | Folder-agnostic single-contact resolver. |
| `GET /v1.0/me/contacts` | Folder-agnostic list across the whole account (bifrost's `contacts_list(None)` / `contact_search(None)` when no address book is named). `$top` / `$skiptoken` paginate; `$select` ignored (full projection always). |
| `POST /v1.0/me/contacts` / `POST .../contactFolders/{id}/contacts` | Create (default folder vs named); `{ displayName, emailAddresses }` mapped to the fixture `Contact` (other Graph fields accepted, not stored). 201 with the contact; mints `mock-contact-N`. |
| `PATCH /v1.0/me/contacts/{id}` | Sparse update of `displayName` / `emailAddresses` (null clears, omitted untouched). 200 / 404. |
| `DELETE /v1.0/me/contacts/{id}` | 204 / 404. Records `contact_destroyed`. |
| `$filter=emailAddresses/any(a:a/address eq '...')` | Honoured on both contact-list endpoints (case-insensitive address match). Other filter shapes fall through to the full list. |
| `GET /v1.0/me/contactFolders/{id}/contacts/delta` | First call (no `$deltatoken`) paginates the full contact dump for the folder, emitting `@odata.deltaLink` only on the final page. Follow-ups walk `Fixture::change_log` between the supplied state and the current state. Created/updated contacts project as full bodies; destroyed contacts emit Graph tombstones (`{ id, "@removed": { reason: "deleted" } }`). `$deltatoken=latest` returns an empty page with a fresh deltaLink (no contact dump). Unknown / evicted token falls back to bootstrap (real Graph emits 410 Gone; ratatoskr handles that by retriggering full sync, so an immediate bootstrap is a coherent v0 stand-in). |

Change-script ops (Lua `change({...})`):

- `contact_folder_create`: array of `{ id, display_name,
  parent_folder_id?, is_default? }`. Apply rejects duplicate ids
  and forward references to undeclared parents.
- `contact_folder_update`: array of `{ id, display_name?,
  parent_folder_id? }`. At least one field must be set.
- `contact_folder_destroy`: array of folder-id strings. Apply
  rejects destroy if any contact still references the folder.
- `contact_create`: array of `{ id, folder_id, display_name?,
  emails }`. Same `emails` shape as the static builder: bare
  string sugar or `{ address, name }` table per entry. Folder is
  validated at apply time.
- `contact_update`: array of `{ id, display_name?, folder_id?,
  emails? }`. `emails`, when present, is a full-replace.
- `contact_destroy`: array of contact-id strings.

`fixtures/graph-contacts-incremental.lua` exercises the
new/change/delete trio end-to-end through `contacts/delta`; see
`tests/step.rs::fixture_step_mutations_visible_through_graph_contacts_delta`.

## Master categories (`src/graph/label_sync.rs`)

The Outlook master category list is the Graph analogue of Gmail
labels / JMAP keywords. Flat per-account in real Graph - no
folder scope - and exposed under
`/v1.0/me/outlook/masterCategories`. Wire shape matches Graph's
`OutlookCategory` resource: `id`, `displayName`, `color`
(optional Graph preset enum string).

| Endpoint | Notes |
|----------|-------|
| `GET /v1.0/me/outlook/masterCategories` | List. No paging - the master category list is small and real Graph returns it unpaginated. |
| `GET /v1.0/me/outlook/masterCategories/{id}` | Single. |
| `POST /v1.0/me/outlook/masterCategories` | Body `{ displayName, color?, id? }`. `id` is optional: if absent the mock mints `mock-category-N` via `Fixture::mint_category_id`; if present the mock honours it (real Graph rejects client-supplied ids with a 400, but v0 keeps this permissive so test fixtures can author predictable ids). Duplicate id returns 409 Conflict. Missing `displayName` returns 400. |
| `PATCH /v1.0/me/outlook/masterCategories/{id}` | Body may patch `displayName` and `color`. Unknown fields are ignored. 404 on unknown id. |
| `DELETE /v1.0/me/outlook/masterCategories/{id}` | 204 on success, 404 on unknown id. |

Mutations land via `Fixture::mutate` and append
`category_created` / `category_updated` / `category_destroyed`
to the change log. Real Graph has no `masterCategories/delta`
endpoint, so v0 doesn't expose one either - the change-log
entries are purely observability for tests asserting state
moved.

The fixture format adds a flat `[[category]]` block (see
`notes/fixture-format.md`); the Lua loader exposes the same
shape via `category({...})`.

## Groups (`src/graph/group_sync.rs`)

Cross-account groups: each `[[group]]` in the fixture names a
`members` list of declared `[[account]]` ids. The wire surface
ratatoskr's group-enumeration code path consumes:

| Endpoint | Notes |
|----------|-------|
| `GET /v1.0/groups` | List all groups. No paging in v0 - the group set is small. Members are NOT inlined; clients call `/groups/{id}/members` to expand (matches real Graph). |
| `GET /v1.0/groups/{id}` | Single group; 404 on unknown id. |
| `GET /v1.0/groups/{id}/members` | Project each member-account as a `#microsoft.graph.user` with `id`, `displayName`, `mail`, `userPrincipalName` populated from `account.name`. Real Graph emits each entry typed (user / group / device); v0 only models user-typed members. |
| `GET /v1.0/me/memberOf` | Groups containing the bearer-resolved account (`oauth::account_from_bearer`, same fallback-to-primary semantics as Gmail / gcal / People). |
| `GET /v1.0/users/{userId}/memberOf` | Path-resolved: `me` aliases the bearer-resolved primary, otherwise the `userId` must match a declared account. Unknown id returns 404 `ResourceNotFound`. |

Read-only in v0 - the Graph group surface has no mutating verbs.

## Multi-account routing

Graph mail surfaces both `/v1.0/me/...` and
`/v1.0/users/{userId}/...` for the same handler set. `me`
scopes to the primary account; a `userId` matching a declared
`[[account]]` scopes to that account; `me` is also accepted as
the literal value of `{userId}`. An unknown `userId` returns
HTTP 404 with the Graph `{"error": {"code":
"ResourceNotFound"}}` envelope. Folder lookups (including
well-known aliases like `inbox`, `drafts`) are scoped to the
named account, so a `/v1.0/users/{secondary}/mailFolders/inbox`
request resolves the secondary account's inbox - not the
primary's.

Coverage: every Graph resource family routes per-account -
mail (`mailFolders`, `messages`, `messages/delta`,
`messages/.../attachments`), calendar (`calendars`,
`calendars/{id}/events`, `calendars/{id}/calendarView/delta`,
`events/{id}`), contacts (`contactFolders`,
`contactFolders/{id}/contacts`, `contacts/delta`,
`contacts/{id}`), and master categories
(`outlook/masterCategories`).

## Connection / transport

- Base URL: `https://graph.microsoft.com/v1.0` (a few flows use
  `/beta`; ignored in v0). Mock will serve `/v1.0/...` over plain
  HTTP on `--graph-port`.
- HTTP/1.1, JSON, UTF-8.
- Bearer auth: `Authorization: Bearer <token>` on every request.
  Refresh-token cycle is invisible to us; mock accepts any token,
  never returns 401.
- Concurrency: ratatoskr caps itself at 3 concurrent in-flight
  requests per mailbox (`client.rs:21,92`). Graph itself enforces 4.
  v0 mock has no concurrency cap.
- Retry: `client.rs:23-26` retries 429s up to 3 times with 1s
  initial backoff. v0 mock never emits 429.

## OData envelope

```json
{
  "@odata.context": "<absolute or relative context URL>",
  "value": [ ... ],
  "@odata.nextLink": "https://.../...?$skiptoken=...",
  "@odata.deltaLink": "https://.../...?$deltatoken=..."
}
```

Read by ratatoskr (`types.rs:4-11`):

- `value` - required, an array.
- `@odata.nextLink` - absolute URL, optional. Client follows it
  verbatim (`sync/folders.rs:200`). Relative URLs are not supported.
- `@odata.deltaLink` - absolute URL, optional. Stored per folder for
  the next delta cycle.
- `@odata.context` - tolerated, not required.
- Per-item `@odata.id` and `@odata.type` - tolerated, not required.
- Deleted-item marker on a delta response: `{"@removed": ...}` -
  any truthy value satisfies the parser (`sync/mod.rs:390`).

## Folder model

Per `types.rs:75-80`, `parse.rs`, and `folder_mapper.rs`:

```json
{
  "id": "<opaque>",
  "displayName": "<name>",
  "parentFolderId": "<opaque or null>",
  "childFolderCount": <int>,
  "unreadItemCount": <int>,
  "totalItemCount": <int>,
  "wellKnownName": "<alias or null>"
}
```

Well-known aliases (case-insensitive) and how they map to ratatoskr's
internal label IDs (`crates/db/src/db/folder_roles.rs:129-138`):

| alias          | label   |
|----------------|---------|
| `inbox`        | INBOX   |
| `drafts`       | DRAFT   |
| `sentitems`    | SENT    |
| `deleteditems` | TRASH   |
| `junkemail`    | SPAM    |
| `archive`      | archive |

The aliases double as path tokens: `GET /me/mailFolders/inbox`
returns the same folder as `GET /me/mailFolders/{opaque-id}` for the
inbox. v0 mock honours both.

## Mail-sync endpoints

### Single-message read + `$batch` hydration (bifrost)

bifrost's NEW client (research/bifrost/crates/graph) hydrates message
metadata differently from the delta-only path the rest of this doc
describes: after `messages/delta` surfaces ids, it batches per-id
`GET /me/messages/{id}?$select=...` sub-requests through
`POST /v1.0/$batch` (`client.rs::post_batch`, `account/get.rs`).
v0 serves:

- `GET /v1.0/me/messages/{id}` + `/v1.0/users/{u}/messages/{id}` -
  single-message projection (reuses `message_value`), 404
  `ErrorItemNotFound` on unknown id. `$select` parsed + ignored.
- `PATCH /v1.0/me/messages/{id}` (+ `/users/{u}` twin) - flag
  writeback (`isRead` <-> `$seen`, `flag.flagStatus: flagged` <->
  `$flagged`, `categories[]` <-> user keywords) through
  `Fixture::mutate`, recording an `email_updated` transition.
  `importance` is accepted but not durably stored (no fixture slot).
  `If-Match` not enforced in v0. bifrost drives this both directly
  and as `$batch` sub-requests (both wired through the shared cores).
- `DELETE /v1.0/me/messages/{id}` (+ `/users/{u}` twin) - permanent
  delete. Retires the message's UID slots (IMAP stability) and
  records `email_destroyed` + the owning account, so the next
  `messages/delta` tombstones it. 204 / 404.
- `POST /v1.0/me/messages/{id}/move` (+ `/users/{u}` twin) - body
  `{ destinationId }`. Single-folder model: replaces `mailbox_ids`
  with `[destinationId]`, syncs UIDs, records `email_updated`.
  Returns 201 with the moved message; 400 on missing `destinationId`,
  404 on unknown message.
- `POST /v1.0/me/messages` (draft create) + `POST
  /v1.0/me/messages/{id}/send` (+ `/users/{u}` twins) - bifrost's
  send path is create-draft-then-send (`pim.rs::send_message`), not
  `/sendMail`. Create stores a `$draft`-keyworded Email (subject /
  from / to / cc / bcc / body parsed from the Graph shape) in the
  Drafts-role mailbox, or the first mailbox if none, recording
  `email_created`; returns 201 with the message so the id is real
  (GET/PATCH then find it). Send returns 202 and leaves the draft -
  v0 models neither the Sent-folder transition nor delivery.
- `GET /v1.0/me/messages/{id}/$value` (+ `/users/{u}` twin) -
  assembled RFC 822 bytes (`text/plain`), bifrost's `open_raw_rfc822`
  body-fetch path. Reuses `crate::imap::assembled_rfc822`, so the
  Graph `$value` and IMAP `BODY[]` surfaces are byte-identical
  (multipart/mixed when the email carries attachments).
- `GET /v1.0/me/messages` (+ `/users/{u}` twin) - account-wide
  message collection (not folder-scoped). bifrost fetches a whole
  conversation here via `$filter=conversationId eq '<thread>'`
  (`pim.rs::message_values_for_thread`); v0 maps `conversationId` to
  the email's `thread_id`. `$top`/`$skiptoken` paginate. Other
  filters and `$search` fall through to the full account list (P2).
- `POST /v1.0/$batch` - `{ requests: [{id, method, url, body?}] }` ->
  `{ responses: [{id, status, headers, body}] }`. Holds one write
  guard for the batch and routes each sub-request through the shared
  message cores: GET (hydration) plus the writes bifrost batches -
  PATCH (flags), DELETE, and POST `.../move`. URLs are relative
  (`/me/...` or `/users/{u}/...`, with or without the `/v1.0`
  prefix). A sub-request it doesn't model gets a per-item error (404
  unknown route, 501 unmodelled method), so a batch degrades
  per-item, not batch-wide. NOTE: bifrost routes its message writes
  through `$batch`, so this - not the direct PATCH/DELETE/move
  endpoints - is the path it actually exercises.

### Folder mutation (bifrost container pipeline)

`src/graph/mail.rs` serves the mailFolder write surface
(`pim.rs::container_*`): `POST .../mailFolders` (top-level) and
`POST .../mailFolders/{parent}/childFolders` create
(`{ displayName }` -> 201 folder); `PATCH .../mailFolders/{id}`
rename (`{ displayName }`); `POST .../mailFolders/{id}/move`
(`{ destinationId }`, the literal `msgfolderroot` re-parents to top);
`DELETE .../mailFolders/{id}`. All mutate the shared `Mailbox` set
through `Fixture::mutate` (`mailbox_*` transitions, observed by JMAP
`Mailbox/changes`). New folder ids are `mock-mailbox-N`. Delete
removes the folder only - it does not cascade to the messages that
referenced it (v0 simplification).

### Folder resolution

```
GET /v1.0/me/mailFolders/{alias}
```

Returns the single folder matching `alias` (well-known) or `id`
(opaque). 404 if neither matches. `sync/folders.rs:28-39`.

### Folder tree

```
GET /v1.0/me/mailFolders?$top=250
GET /v1.0/me/mailFolders/{folderId}/childFolders?$top=250
```

Returns `ODataCollection<Folder>`. Pagination via `@odata.nextLink`
when the page is full. `sync/folders.rs:174-219`.

### Initial message fetch

```
GET /v1.0/me/mailFolders/{folderId}/messages
    ?$filter=receivedDateTime ge 2025-02-01T00:00:00Z
    &$select=<MESSAGE_SELECT>
    &$expand=<EXPAND>
    &$top=50
    &$orderby=receivedDateTime desc
```

`MESSAGE_SELECT` fields (`types.rs:206-211`):

```
id, conversationId, subject, bodyPreview, body, uniqueBody,
from, toRecipients, ccRecipients, bccRecipients, replyTo,
receivedDateTime, sentDateTime, isRead, isDraft, hasAttachments,
importance, parentFolderId, categories, flag,
inferenceClassification, isReadReceiptRequested,
internetMessageHeaders, internetMessageId
```

`EXPAND` (`types.rs:220-224`):

```
attachments($select=id,name,contentType,size,isInline,contentId,contentBytes),
singleValueExtendedProperties($filter=...REACTIONS_GUID...)
```

Page size: 50 (`sync/mod.rs:30 BATCH_SIZE`). Pagination via
`@odata.nextLink`.

### Delta sync bootstrap

```
GET /v1.0/me/mailFolders/{folderId}/messages/delta
    ?$select=<...>&$expand=<...>
```

Walks pages until a response carries `@odata.deltaLink` (no
`nextLink`). The deltaLink is stored per folder
(`sync/delta_tokens.rs:18-52`).

Minimal-bootstrap variant for newly discovered folders:

```
GET /v1.0/me/mailFolders/{folderId}/messages/delta?$deltatoken=latest
```

Returns no messages, just a fresh `@odata.deltaLink`
(`sync/delta_tokens.rs:96-114`).

### Delta sync query

```
GET <deltaLink-from-previous-cycle>
```

Same response shape. Deleted messages appear as
`{"@removed": ..., "id": "<opaque>"}` items in `value[]`. The
`@removed` field's content is ignored (`sync/mod.rs:390`).

A "no changes since last cycle" response is empty `value[]` with an
immediate `@odata.deltaLink`.

## Message model

`types.rs:14-38`, `parse.rs:47-193`. The non-trivial fields:

- `id` (string, required).
- `conversationId` (string, optional). Falls back to `id` for
  threading.
- `subject`, `bodyPreview` (string, optional).
- `body`: `{"contentType": "html"|"text", "content": "..."}`.
- `from`: `{"emailAddress": {"address": "...", "name": "..."}}`.
- `toRecipients`, `ccRecipients`, `bccRecipients`, `replyTo`: arrays
  of the same recipient shape.
- `receivedDateTime`, `sentDateTime`: ISO 8601 strings (RFC 3339 or
  naive accepted).
- `isRead`, `isDraft`, `hasAttachments`: bool.
- `importance`: `"low"|"normal"|"high"`. Currently unused by
  ratatoskr but reserved.
- `parentFolderId`: opaque folder id.
- `categories`: array of strings (custom keyword labels).
- `flag`: `{"flagStatus": "flagged"|"notFlagged"|"complete"}`.
- `inferenceClassification`: `"focused"|"other"`. `"focused"` adds a
  FOCUSED label client-side.
- `isReadReceiptRequested`: bool.
- `internetMessageHeaders`: `[{"name": "...", "value": "..."}]`.
  Headers ratatoskr looks up case-insensitively (`parse.rs:109-120`):
  `Message-ID`, `References`, `In-Reply-To`,
  `Authentication-Results`, `List-Unsubscribe`,
  `List-Unsubscribe-Post`, `Disposition-Notification-To`.
- `internetMessageId`: string.
- `attachments`: array of attachment objects.
- `singleValueExtendedProperties`: array of extended-property
  objects. Used for Exchange reactions; v0 emits an empty list.

Mapping from saehrimnir's fixture:

- fixture `keywords` containing `$seen` -> `isRead: true`.
- fixture `keywords` containing `$flagged` -> `flag.flagStatus =
  "flagged"`.
- non-`$`-prefixed keywords -> `categories[]` entries.
- fixture `body_text` -> `body: {contentType: "text", content: "..."}`.
- fixture `from`, `to`, `cc`, `bcc`, `reply_to` -> the recipient
  arrays in their natural shape.
- fixture `received_at`, `sent_at` -> ISO 8601 strings.
- fixture `message_id`, `in_reply_to`, `references` ->
  `internetMessageHeaders` entries plus the standalone
  `internetMessageId`.

## Attachment model

`types.rs:96-105`:

```json
{
  "id": "<opaque>",
  "name": "report.pdf",
  "contentType": "application/pdf",
  "size": 1024000,
  "isInline": false,
  "contentId": null,
  "contentBytes": "<base64 or null>"
}
```

`contentBytes` is null for attachments larger than ~100 KiB; the
client downloads them via `/$value`. v0 mock has no fixture
attachments yet so the array is always empty. When fixtures grow
attachments, this is the moment to fill it in.

## Wire-format strictness

- camelCase everywhere (`types.rs:14` uses
  `#[serde(rename_all = "camelCase")]`).
- ISO 8601 dates accepted with or without timezone, with or without
  fractional seconds. Mock will always emit
  `<yyyy>-<mm>-<dd>T<HH>:<MM>:<SS>Z` (full UTC).
- `@odata.nextLink` MUST be an absolute URL or the client breaks
  (`sync/folders.rs:200`).
- 401 triggers token refresh; v0 never returns 401.
- 429 triggers exponential-backoff retry; v0 never emits.

## Account settings + opt-in (accept-and-ignore, `src/graph/settings.rs`)

These have no fixture slot, so v0 serves shaped-but-non-durable
responses (no change-log transitions):

- `GET /v1.0/me/mailboxSettings` (+ `/users/{u}`) -> a disabled
  `automaticRepliesSetting` (bifrost reads it as "vacation off").
  `PATCH` echoes the submitted setting (`vacation_set` ignores the
  response body).
- `GET /v1.0/me/mailFolders/{folder}/messageRules` -> empty `value`;
  POST create echoes the body with a minted id; PATCH echoes; DELETE
  204; GET-by-id 404 (no rules stored).
- `POST /v1.0/subscriptions` -> 201 with a minted id + echoed
  `expirationDateTime` / `resource`; PATCH renew echoes the new
  expiration; DELETE 204. Only driven in
  `PushMode::GraphSubscriptions`; the mock delivers no notifications.

## Constants worth knowing

- `BATCH_SIZE = 50` (messages per page on initial sync).
- `CONCURRENCY_LIMIT = 3` (per mailbox).
- Folder pagination `$top = 250`.
- Reactions GUID: `{41F28F13-83F4-4114-A584-EEDB5A6B0BFF}`.

## Out of scope for v0 - resource categories to scaffold for later

Calendar (`calendar.rs`), master categories (`label_sync.rs`),
groups (`group_sync.rs`), and shared-mailbox / per-user mail
routing (`mail.rs` + `/v1.0/users/{id}/...` paths) are landed.
Remaining:

| Module                       | What it syncs                              |
|------------------------------|--------------------------------------------|
| `onedrive.rs`                | Resumable upload sessions for attachments  |
| `public_folder_sync.rs`      | Pinned public folders via EWS              |
| `webhooks.rs`                | Change subscriptions                       |
| `autodiscover.rs`            | Shared-mailbox discovery via SOAP          |
| `ews/`                       | Exchange Web Services SOAP                 |

The v0 module structure (`src/graph/`) keeps mail handlers in
`mail.rs`, OData plumbing in `odata.rs`, and reserves room for
sibling files - `calendar.rs`, `contacts.rs`, `drive.rs`, etc. -
without router restructuring.
