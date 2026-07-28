# Ratatoskr's Gmail REST API client surface

What the v0 Gmail mock has to satisfy. Distilled from
`<ratatoskr>/crates/gmail/` on 2026-05-06. Source-of-truth lives
there; this file is a cheat sheet so we don't have to fan out every
turn.

Mail-sync only for v0. Contacts (People API), Drive uploads, and the
Calendar runtime are listed at the end so the next reader knows what
sibling files the module needs to accommodate.

## Connection / transport

- Base URL: `https://www.googleapis.com/gmail/v1/users/me`. Mock
  serves `/gmail/v1/users/me/...` over plain HTTP.
- People API base: `https://people.googleapis.com/v1/`. Out of scope
  for v0 mail; will land alongside contacts.
- HTTP/1.1, JSON, UTF-8.
- Bearer auth: `Authorization: Bearer <token>` on every request
  (`client.rs:306-308`). Refresh on 401; v0 mock never returns 401.
- Concurrency: 10 worker tasks during initial thread fetch
  (`sync/mod.rs:90`), 5 during delta sync (`sync/delta.rs:82`). No
  per-account cap on the wire; v0 mock has no concurrency cap.
- Retry: 3 attempts on 429 with 1 s initial backoff
  (`client.rs:14-16`). v0 mock never emits 429.

## Mail-sync endpoints

### Profile

```
GET /gmail/v1/users/me/profile
```

Response (`api.rs:14-17`):

```json
{
  "emailAddress": "alice@example.com",
  "messagesTotal": <int>,
  "threadsTotal": <int>,
  "historyId": "<u64-as-string>"
}
```

The `historyId` is the cursor for the History API; the client stores
it after every cycle.

### Labels

```
GET /gmail/v1/users/me/labels
```

Response (`api.rs:23-26`):

```json
{
  "labels": [
    {
      "id": "INBOX",
      "name": "INBOX",
      "type": "system",
      "messageListVisibility": "show",
      "labelListVisibility": "labelShow",
      "messagesTotal": 42,
      "messagesUnread": 5,
      "threadsTotal": 40,
      "threadsUnread": 3
    },
    {
      "id": "Label_42",
      "name": "Newsletters",
      "type": "user",
      ...
    }
  ]
}
```

System label IDs ratatoskr recognises (`sync/labels.rs:36-41`):

`INBOX`, `SENT`, `DRAFT`, `TRASH`, `SPAM`, `IMPORTANT`, `STARRED`,
`UNREAD`. Anything else with `type: "user"` becomes a user-defined
tag.

### Thread list

```
GET /gmail/v1/users/me/threads
    ?q=after:YYYY/M/D
    &maxResults=100
    &pageToken=<token>
```

Query (`sync/mod.rs:169-176`):

- `q`: free-form Gmail search query. The mail-sync code only ever
  emits `after:YYYY/M/D` (no quoting; the date comes from the
  account's `days_back` config).
- `maxResults`: page size, default 100.
- `pageToken`: opaque continuation cursor.

Response:

```json
{
  "threads": [
    {"id": "<thread-id>", "snippet": "...", "historyId": "<u64>"},
    ...
  ],
  "nextPageToken": "<opaque>",
  "resultSizeEstimate": 42
}
```

`nextPageToken` absent on the last page.

### Thread fetch (full detail)

```
GET /gmail/v1/users/me/threads/{threadId}?format=full
```

Returns a `GmailThread` with nested `messages[]`. Each message has
the wire shape described under "Message model" below
(`api.rs:103-111`, `storage.rs:25`).

### Message list (bifrost's message-centric backfill)

```
GET /gmail/v1/users/me/messages
    ?q=after:YYYY/M/D
    &maxResults=100
    &pageToken=<token>
```

bifrost's `GoogleAccount` is message-centric, not thread-centric: it
backfills via `messages.list` (page 1 is the first backfill call) and
hydrates each result through `messages.get`. The legacy `threads`
surface does not satisfy it.

Response:

```json
{
  "messages": [
    {"id": "<message-id>", "threadId": "<thread-id>"},
    ...
  ],
  "nextPageToken": "<opaque>",
  "resultSizeEstimate": 42
}
```

Same `q=after:YYYY/M/D`-only / `t.<offset>` paging contract as the
thread list. Most-recent message first, id-lex tiebreak. An
unsupported `q=` shape is a hard 400 (`reason: invalidQuery`), never
a silent full-list dump.

### Message fetch (per-message hydration)

```
GET /gmail/v1/users/me/messages/{messageId}?format=metadata|full|minimal|raw
```

`metadata` / `full` / `minimal` share the structured `message_value`
projection (the "Message model" shape below); bifrost parses each
leniently. `raw` is the exception: it drops the structured `payload`
and emits a top-level base64url `raw` field carrying the assembled
RFC 822 bytes (the same `assembled_rfc822` the IMAP `BODY[]` and
Graph `$value` paths use). bifrost's `raw_bytes()` reads that field
and errors `ParseFailed` without it. Default format is `full`.
Unknown id -> 404 (`reason: notFound`).

### Message attachment fetch

```
GET /gmail/v1/users/me/messages/{messageId}/attachments/{attachmentId}
```

Returns `{ "data": "<base64url>", "size": <int> }` (`api.rs:184-195`).
v0 fixtures carry no attachments, so any call gets a 404 with the
canonical Gmail error envelope.

### History (incremental sync)

```
GET /gmail/v1/users/me/history
    ?startHistoryId=<u64>
    &maxResults=500
    &historyTypes=messageAdded
    &historyTypes=messageDeleted
    &historyTypes=labelAdded
    &historyTypes=labelRemoved
    &pageToken=<opaque>
```

Response (`types.rs:101-124`):

```json
{
  "history": [
    {
      "id": "<u64>",
      "messagesAdded":   [{"message": <GmailMessage>}, ...],
      "messagesDeleted": [{"message": <GmailMessage>}, ...],
      "labelsAdded":     [{"message": <GmailMessage>, "labelIds": [...]}, ...],
      "labelsRemoved":   [{"message": <GmailMessage>, "labelIds": [...]}, ...]
    }
  ],
  "historyId": "<u64>",
  "nextPageToken": "<opaque>"
}
```

The client persists `historyId` between cycles. A 404 on this
endpoint (or an error mentioning `historyId`) triggers a full
re-sync (`sync/delta.rs:180-182`).

`historyId` is the per-account change-log counter mapped to the
reported space as `counter + 1`, so a freshly-loaded fixture reports
`1` and each applied change-script step advances it by one. The mock
walks the requesting account's change log and projects one history
record per transition newer than `startHistoryId`:

- `email_created` -> `messagesAdded` (full `message` body, so
  bifrost's `changes_from_history` reads `labelIds` and emits a
  `ScopeChange::Added` per label).
- `email_destroyed` -> `messagesDeleted` (keyed on `message.id`;
  the email is gone so `threadId` echoes the id as a placeholder -
  bifrost ignores it on a tombstone).
- a label-set delta on an updated email -> `labelsAdded` /
  `labelsRemoved` with the precise `labelIds` that moved (bifrost
  applies each as a `ScopeChange::Added` / `Removed`).

The label delta is captured at mutation time into the change log's
`email_label_changes` sidecar, by BOTH mutation paths: the
change-script step applier (`src/test_admin.rs`, for a change that
arrives from the server) and the served mutation verbs
(`modify_one` in `src/gmail/mail.rs`, for a change the consumer
made itself). Either way the record is MESSAGE-scoped, never
thread-scoped - which is what lets a consumer tell "one message of
this thread changed" apart from "every message in this thread
changed". A `startHistoryId` older than the bounded ring can
reconstruct returns a 404 mentioning `historyId`, driving the
full-resync fallback. `startHistoryId` at or past the current id
returns an empty `history[]` paired with the current id.

### Mutations

```
POST   /gmail/v1/users/me/messages/{id}/modify
POST   /gmail/v1/users/me/threads/{id}/modify
DELETE /gmail/v1/users/me/threads/{id}
POST   /gmail/v1/users/me/messages/batchModify
POST   /gmail/v1/users/me/messages/batchDelete
```

bifrost's mutation pipeline (`google pim.rs::modify_target`) splits
on the target: a `MutationTarget::Message` becomes the singular
`messages/{id}/modify`, a `MutationTarget::Thread` becomes
`threads/{id}/modify`, and `delete_thread` issues
`DELETE /threads/{id}` only when the thread already sits in TRASH
(every other destroy is a move-to-trash through `modify`).

The two `batch*` forms are driven too, by a different pipeline:
bifrost's BULK driver (`google account/mutation.rs`) posts
`messages/batchModify` for `bulk_set_flags` / `bulk_move` and
`messages/batchDelete` for `bulk_destroy`, batching ids up to
`GMAIL_BATCH_MODIFY_LIMIT`. (An earlier revision of this doc said
bifrost did not drive them; that has not been true since the bulk
driver landed.)

#### Combined add-and-remove

Gmail has no move verb, so a bulk move is ONE `batchModify` carrying
both the destination in `addLabelIds` and the container being left in
`removeLabelIds`. The mock honours both halves of a single patch on
every mutating path (`messages/{id}/modify`, `threads/{id}/modify`,
`messages/batchModify`), which matters as the bulk surface grows a
SOURCE: the request shape stays one round trip rather than a move plus
a per-message detach.

Ordering is adds first, then removes. That is what makes
`add INBOX` + `remove [SPAM, TRASH]` - Gmail's un-spam / un-trash,
where the removed containers outrank the added one - resolve to plain
inbox membership, and it keeps a move from passing through a
transiently container-less state. A label named in BOTH lists nets out
to removed; real Gmail's behaviour there is unspecified and no client
drives it, so the mock simply pins a deterministic answer.

`SPAM` and `TRASH` removals resolve through the account's `junk` /
`trash` role mailboxes, so a fixture that stages those containers gets
the real exclusive-container semantics rather than a no-op.

#### Archive, and the two fixture-authoring gaps the mock refuses

`removeLabelIds: ["INBOX"]` with nothing added is an ARCHIVE. Real
Gmail answers 200 and the message keeps existing with no container
label at all - it lives in All Mail, which is not a label. The fixture
format spells that state as membership in a `role = "archive"`
mailbox, which `Fixture::gmail_label_ids` deliberately projects to no
Gmail label, so an archived message round-trips to exactly the shape
real Gmail serves.

The fixture LOADER rejects an email with empty `mailbox_ids`, and that
rule stays: a mailbox-less email is not representable on the other
protocols the same fixture feeds (JMAP requires a non-empty
`mailboxIds`, IMAP and Graph both need a container, and the loader
derives an email's account from its mailboxes). The MUTATION is the
side that adapts - a patch that would empty the set lands the message
in the archive mailbox instead.

That fallback is Gmail-only. The same shape on JMAP (`Email/set`
emptying `mailboxIds`) is REFUSED with `invalidProperties`, because
RFC 8621 has no All Mail for the message to fall into; see
`notes/ratatoskr-jmap-surface.md` § "`mailboxIds` can never end up
empty". The divergence is the protocols disagreeing, not an
inconsistency in the mock.

Two cases the mock cannot represent are refused with a Gmail-shaped
`400 invalidArgument` naming the missing fixture declaration, after
rolling the patch back whole (no partial application, no state
advance, no history record):

- the patch would empty the mailbox set and the account declares no
  `role = "archive"` mailbox;
- `addLabelIds` names a system container (`INBOX` / `TRASH` / `SPAM`)
  the account declares no role mailbox for.

Both are loud on purpose. Swallowing the second silently turns a
consumer's move-to-trash into an archive: a 2xx, a passing gate, and
the wrong state on the server. `SENT` and `IMPORTANT` are NOT in that
set - neither is a destination a move can name, and `IMPORTANT` behaves
like a flag rather than a container - so they stay silent no-ops.

`fixtures/gmail-bulk-move.lua` stages every container the bulk paths
can name, which is what a source-carrying bulk-move gate should point
at.

The thread form is deliberately the message form looped over the
thread's messages, one `modify_one` per message, so `history.list`
emits one record per message exactly as real Gmail does. A
thread-level history record would make a whole-thread change
indistinguishable from a single-message one and would defeat the
consumer gate that asserts a message's thread-mates kept their
membership.

Bodies are `{ addLabelIds: [...], removeLabelIds: [...] }` (plus
`ids` for the batch forms). `messages/{id}/modify` answers 200 with
the updated Message; `threads/{id}/modify` answers 200 with
`{ id, historyId, messages: [...] }` (bifrost's `GmailThread`); the
`batch*` forms and `DELETE /threads/{id}` answer 204. A thread the
account cannot see is a `notFound`, not a silent success.

### SendAs / signatures (out of scope for read-only mail sync)

```
GET /gmail/v1/users/me/settings/sendAs
PUT /gmail/v1/users/me/settings/sendAs/{email}
```

Used for signature sync. v0 mock emits an empty `sendAs[]` so the
write path never fires; bidirectional signature sync is a v1
concern.

## Message model

`types.rs:10-24`:

```json
{
  "id": "<opaque>",
  "threadId": "<opaque>",
  "labelIds": ["INBOX", "UNREAD", "Label_42"],
  "snippet": "first 100 chars of body",
  "historyId": "<u64>",
  "internalDate": "<unix-ms-as-string>",
  "sizeEstimate": <bytes>,
  "payload": <MimePart>,
  "raw": null
}
```

`internalDate` is Unix milliseconds as a quoted string, not ISO 8601
(`parse.rs:79-83`).

### MIME payload tree

`types.rs:27-38`:

```json
{
  "partId": "0",
  "mimeType": "multipart/alternative",
  "filename": "",
  "headers": [
    {"name": "From", "value": "..."},
    {"name": "Subject", "value": "..."},
    ...
  ],
  "body": {"size": 0, "data": null, "attachmentId": null},
  "parts": [
    {
      "partId": "0.0",
      "mimeType": "text/plain",
      "headers": [...],
      "body": {"size": 13, "data": "<base64url of body>", "attachmentId": null}
    }
  ]
}
```

Headers ratatoskr looks up case-insensitively (`parse.rs:121-123`):

`From`, `To`, `Cc`, `Bcc`, `Reply-To`, `Subject`, `Message-ID`,
`References`, `In-Reply-To`, `List-Unsubscribe`,
`List-Unsubscribe-Post`, `Disposition-Notification-To`,
`Authentication-Results`.

Body data: base64url without padding (`=` chars stripped). Decoded
via `decode_base64url_nopad()` (`parse.rs:269-275`).

Special MIME types:

- `text/x-amp-html`: skipped by parser (`parse.rs:231` JMAP analogue;
  same skip logic in Gmail parse).
- `text/vnd.google.email-reaction+json`: emoji reactions, payload is
  small JSON (`parse.rs:247-266`). v0 mock does not emit these.

### Attachment parts

A part is treated as an attachment when:

- `body.attachmentId` is non-null, AND
- the part has either a `filename` attribute or a `Content-ID`
  header.

The client deduplicates attachments by `attachmentId` when the same
blob appears in multiple parts (`parse.rs:152-184`). v0 fixtures
have no attachments; the payload is always a single `text/plain`
leaf.

## Label semantics

`labelIds` on a message drives every keyword/folder relationship the
client cares about (`parse.rs:98-99`):

- `UNREAD` -> not read; absence means read.
- `STARRED` -> starred / flagged.
- `IMPORTANT` -> important flag (v1 concern; v0 doesn't emit).
- System container labels (`INBOX`, `SENT`, `DRAFT`, `TRASH`, `SPAM`)
  -> the message lives in that container.
- Anything else -> user tag.

Mapping from saehrimnir's fixture:

- Fixture mailbox role -> Gmail system label id, applied to every
  email in that mailbox:
  | role       | label   |
  |------------|---------|
  | inbox      | INBOX   |
  | sent       | SENT    |
  | drafts     | DRAFT   |
  | trash      | TRASH   |
  | junk       | SPAM    |
  | important  | IMPORTANT |
  | archive    | (no label - Gmail's "archive" means "no INBOX") |
- No-role fixture mailboxes -> user labels with id `Label_<mb-id>`.
- Fixture `keywords`:
  - `$seen` absent -> add `UNREAD` to labelIds.
  - `$flagged` -> add `STARRED`.
  - `$draft` -> add `DRAFT`.
  - non-`$`-prefixed keywords -> become user labels with id
    `Label_<keyword>`.

## Pagination

All list endpoints use `nextPageToken` cursors. The client treats
them as opaque strings (`api.rs:90`). v0 mock uses `t.<offset>` and
follows the same shape for thread list and history.

## Wire-format strictness

- camelCase everywhere (`types.rs:11` uses
  `#[serde(rename_all = "camelCase")]`).
- `internalDate` and `historyId`: numeric values quoted as strings.
- `labelIds`: array of strings, never null.
- `body.data`: base64url, no padding.
- 401 triggers token refresh; v0 never emits.
- 404 with `historyId` mentioned triggers full re-sync; v0 never
  emits 404 on the history endpoint with a valid token (we just
  return empty).

## Constants worth knowing

- `INITIAL_THREAD_FETCH_WORKERS = 10` (`sync/mod.rs:90`).
- `DELTA_THREAD_FETCH_WORKERS = 5` (`sync/delta.rs:82`).
- `HISTORY_MAX_RESULTS = 500` (`api.rs:209`).
- Thread list page size default: 100 (`sync/mod.rs:172`).
- Contact full-sync runs on the 20th delta cycle
  (`sync/delta.rs:48-60`).

## Out of scope for v0 - resource categories to scaffold for later

| Module                              | What it syncs                                    |
|-------------------------------------|--------------------------------------------------|
| `contacts/google_contacts.rs`       | People API connections                           |
| `contacts/other_contacts.rs`        | People API otherContacts                         |
| `gdrive.rs`                         | Resumable file uploads + sharing                 |
| (separate Calendar runtime)         | calendar.googleapis.com Calendars/events         |

The v0 module structure (`src/gmail/`) keeps mail handlers in
`mail.rs` and reserves room for sibling files - `contacts.rs`,
`drive.rs`, etc. - without router restructuring.
