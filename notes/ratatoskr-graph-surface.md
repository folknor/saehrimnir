# Ratatoskr's Microsoft Graph client surface

What the v0 Graph mock has to satisfy. Distilled from
`<ratatoskr>/crates/graph/` on 2026-05-06. Source-of-truth lives
there; this file is a cheat sheet so we don't have to fan out every
turn.

Mail-sync and calendar are wired in v0. Other resource categories
(contacts, OneDrive, groups, labels, public folders via EWS, shared
mailboxes, webhooks, autodiscover) are listed at the end so the
next reader knows what scaffolding the module structure has to
accommodate.

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
| `GET /v1.0/me/calendars/{id}/calendarView/delta` | First call (no `$deltatoken`) returns the full event list; follow-up calls with any `$deltatoken` return an empty `value`. `@odata.deltaLink` is fixed to the fixture state. |
| `GET /v1.0/me/events/{id}` | Single event. 404 if not declared. |
| `POST /v1.0/me/calendars/{id}/events` | 201 with `id = "mock-event-create"`, `echoedRequest` carrying the parsed body. Logs `(graph, "POST /v1.0/me/calendars/{id}/events", { body })` to the request log. |
| `PATCH /v1.0/me/events/{id}` | 200 with `id = <path>`, `echoedRequest` carrying the body. Logs to the request log. |
| `DELETE /v1.0/me/events/{id}` | 204 with no body. Logs `(graph, "DELETE /v1.0/me/events/{id}", { id })`. |

Event projection populates `subject`, `bodyPreview`, `body`
(`contentType: "text"`), `start`/`end` (`{ dateTime, timeZone }`,
always `UTC`), `isAllDay`, `location.displayName`, `organizer.
emailAddress`, and `attendees[].emailAddress` with `type =
"required"`. Attendee tone (`required`/`optional`) and
`responseStatus` are not yet projected; add when a fixture forces
it.

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

## Constants worth knowing

- `BATCH_SIZE = 50` (messages per page on initial sync).
- `CONCURRENCY_LIMIT = 3` (per mailbox).
- Folder pagination `$top = 250`.
- Reactions GUID: `{41F28F13-83F4-4114-A584-EEDB5A6B0BFF}`.

## Out of scope for v0 - resource categories to scaffold for later

| Module                       | What it syncs                              |
|------------------------------|--------------------------------------------|
| `calendar_sync.rs`           | Calendars, events, recurrence, attendees   |
| `contact_sync.rs`            | Contacts, contactFolders                   |
| `label_sync.rs`              | Master category list                       |
| `group_sync.rs`              | M365 groups, mail-enabled distribution     |
| `onedrive.rs`                | Resumable upload sessions for attachments  |
| `public_folder_sync.rs`      | Pinned public folders via EWS              |
| `shared_mailbox_sync.rs`     | Per-mailbox sync via `/users/{id}/...`     |
| `webhooks.rs`                | Change subscriptions                       |
| `autodiscover.rs`            | Shared-mailbox discovery via SOAP          |
| `ews/`                       | Exchange Web Services SOAP                 |

The v0 module structure (`src/graph/`) keeps mail handlers in
`mail.rs`, OData plumbing in `odata.rs`, and reserves room for
sibling files - `calendar.rs`, `contacts.rs`, `drive.rs`, etc. -
without router restructuring.
