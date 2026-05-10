# Fixture format

The TOML shape sæhrimnir loads at startup. v0 supports exactly one
fixture per process. Open questions are flagged inline; the format
will grow as new protocol surfaces (calendar, contacts, attachments,
drive files) need fixture-side data.

## Top level

```toml
name = "jmap-small"

# Optional. Used as the session's `state` token and the per-collection
# `state` strings unless overridden. Any non-empty string is fine; v0
# never advances state.
state = "fixture-state"

[account]
id = "account-1"
# Used as both the JMAP account name and the address ratatoskr stores
# in its accounts row. Should be an email-shaped string - the client
# falls back to it when principals lookup fails (out of scope for v0,
# but cheap to satisfy).
name = "test@example.com"
is_personal = true   # MUST be true for v0; false triggers shared-account paths
```

`name` is a fixture identifier brokkr resolves against (typically
`<fixtures_dir>/<name>.toml`); sæhrimnir just consumes whatever file
path it's handed via `--fixture`.

## Mailboxes

```toml
[[mailbox]]
id = "mbx-inbox"
name = "Inbox"
role = "inbox"           # optional; one of: inbox, archive, drafts, sent, trash, junk, important
parent_id = "mbx-parent" # optional; must reference another mailbox id in this fixture
sort_order = 0           # optional; client doesn't read it but RFC has the field
is_subscribed = true     # optional; defaults to true
```

Validation at load time:

- `id` is unique across mailboxes.
- `role`, if present, is one of the seven recognized values.
  Returning an unknown role is fine (client maps to "other") but
  rejecting at load time keeps fixtures honest.
- `parent_id`, if present, references an existing mailbox.
- No cycles in `parent_id`.

`my_rights` defaults to all-true on serialization (the mock has no
notion of permissions). If a fixture wants to test rights-aware code
paths, add a `[mailbox.rights]` sub-table; not needed for v0.

## Emails

```toml
[[email]]
id = "email-001"
thread_id = "thread-1"           # optional; defaults to id
mailbox_ids = ["mbx-inbox"]      # required; each must exist
keywords = ["$seen"]             # optional; "$seen", "$flagged", or user labels
size = 1024                      # optional; defaults to len(body) or 1024
received_at = "2026-01-15T10:00:00Z"
sent_at = "2026-01-15T09:59:50Z" # optional; defaults to received_at
from = "alice@example.com"       # or { name = "Alice", email = "alice@example.com" }
to = ["bob@example.com"]
cc = []                          # optional
bcc = []                         # optional
reply_to = []                    # optional
subject = "Hello"
preview = "First message body."  # optional; defaults to truncated body_text
message_id = ["<001@example.com>"]   # optional; array per RFC
in_reply_to = ["<000@example.com>"]  # optional
references = ["<000@example.com>"]   # optional
has_attachment = false           # optional; auto-derived if attachments present

# Body. Exactly one of:
body_text = "First message body."
# or
body_path = "messages/email-002.eml"

# Optional attachments. Each entry references a blob stored under the
# fixture file's parent directory; the loader reads the bytes at
# startup. `has_attachment` is auto-derived to true when any entry is
# present (declaring `has_attachment = false` while listing
# attachments is rejected).
[[email.attachment]]
blob_id = "blob-001"            # opaque, fixture-controlled. Echoed
                                # as the JMAP attachments[].blobId,
                                # the Gmail attachmentId, and the
                                # Graph attachment id.
name = "report.pdf"             # filename used by all four protocols.
content_type = "application/pdf"
size = 245000                   # optional; defaults to the byte
                                # length of the file at data_path.
disposition = "attachment"      # optional; "attachment" (default) or
                                # "inline".
cid = "report-cid"              # optional; used when disposition =
                                # "inline" (without angle brackets;
                                # protocols add them on emit).
data_path = "blobs/report.pdf"  # required; resolved relative to the
                                # fixture file's parent dir. The bytes
                                # are loaded eagerly at startup and
                                # served verbatim from each protocol's
                                # download endpoint.
```

### Body sources

Two mutually exclusive options:

- `body_text` - inline string. Becomes a single `text/plain` body
  part with a synthetic `partId`. Cheap, deterministic, hand-editable.
  **Default for v0.** Most fixtures will use this.

- `body_path` - relative path to an `.eml` file under the fixture
  directory. The mock parses the MIME tree to extract `text/plain`,
  `text/html`, attachments, headers. Useful for round-tripping real
  messages. **Implementation deferred until a fixture needs it** - v0
  can ship with `body_text` only and add `body_path` when the first
  test actually requires real MIME.

Open question: HTML-only bodies. Add `body_html` as a third option,
or require `.eml`? Lean toward adding `body_html` as a parallel field -
many fixture cases will be "single text/html part" with no need for
full MIME.

### Threading

`thread_id` defaults to the email's own `id`, giving each message a
singleton thread. Multi-message threads share a `thread_id`. The mock
does not derive threads from `in_reply_to`; explicit `thread_id` keeps
fixtures debuggable.

### Timestamps

ISO-8601 in. Stored as Unix seconds internally, emitted as Unix seconds
on the wire (per RFC 8621). Sub-second precision is dropped. The mock
never reads system time - every timestamp on the wire comes from the
fixture.

### Address shape

Per `crates/jmap/src/parse.rs:200-207`, the client expects each
address as `{name, email}`. Fixtures accept two forms:

- A bare string `"alice@example.com"` - interpreted as
  `{name: null, email: "alice@example.com"}`.
- A table `{name = "Alice", email = "alice@example.com"}`.

Internally normalized to the table form before serialization.

## Calendars and events (optional)

```toml
[[calendar]]
id = "cal-work"
name = "Work"
color = "lightBlue"        # optional; passed through to Graph
is_default = true          # optional; defaults to false

[[event]]
id = "ev-001"
calendar_id = "cal-work"   # required; must reference a declared calendar
subject = "Standup"
body_preview = "Daily ..." # optional
body_text = "..."          # optional
start = "2026-01-15T09:00:00Z"   # required, RFC3339
end   = "2026-01-15T09:15:00Z"   # required, RFC3339
location = "Conf Room A"   # optional
organizer = { name = "Alice", email = "alice@example.com" }   # optional, address-shaped
attendees = [
    { name = "Bob", email = "bob@example.com" },
    "carol@example.com",
]                          # optional; addresses accept bare-string or table form
is_all_day = false         # optional
```

Validation rules:

- `calendar.id` is unique within the fixture.
- `event.id` is unique within the fixture.
- `event.calendar_id` must reference a declared calendar.
- `event.start` and `event.end` are RFC3339; sub-second precision is dropped.

Calendars and events project over the Microsoft Graph
`/v1.0/me/calendars/...` surface today; the same canonical types
will feed CalDAV when that listener lands. The Graph mock supports
the GET endpoints, plus echo-mode POST/PATCH/DELETE that record
the request body in the cross-protocol request log without
mutating the fixture (which is read-only in v0).

## Contact folders and contacts (optional)

```toml
[[contact_folder]]
id = "cf-default"
display_name = "Contacts"
is_default = true             # optional; at most one default per fixture
parent_folder_id = "cf-root"  # optional; must reference a declared folder

[[contact]]
id = "contact-001"
folder_id = "cf-default"      # required; must reference a declared folder
display_name = "Alice Anderson"  # optional
emails = [
    { name = "Alice", address = "alice@example.com" },
    "alice.anderson@example.org",  # bare-string sugar - same as
                                   # { address = "...", name = nil }
]
```

Validation:

- `contact_folder.id` is unique across folders.
- At most one folder may have `is_default = true`.
- `contact_folder.parent_folder_id`, if present, references a
  previously-declared folder (no forward references).
- `contact.id` is unique across contacts.
- `contact.folder_id` references a declared folder.

Contacts and folders project over the Graph
`/v1.0/me/contactFolders/...` and `/v1.0/me/contacts/...` surfaces;
the same canonical types feed any future People-API listener when
that scout doc lands. The Graph mock supports the GET endpoints
plus the `contacts/delta` walker driven by the change_log.

The Lua loader exposes the same blocks via `contact_folder({...})`
and `contact({...})` builders; the `emails` field accepts the same
bare-string-or-table sugar as the TOML form.

## OAuth (optional)

```toml
[oauth]
enforce = false                          # default; existing v0 "no auth" baseline
issuer = "https://saehrimnir.test/oauth" # default; echoed in /oauth/userinfo
```

When `enforce = false` (default), the JMAP / Graph / Gmail HTTP
listeners accept any (or no) `Authorization: Bearer` header, matching
the v0 "no auth" rule. When `enforce = true`, those listeners reject
requests whose bearer is not in the active token set (managed by
`crate::oauth::TokenStore`); IMAP and SMTP have their own auth
surfaces and are unaffected. See `notes/ratatoskr-oauth-surface.md`
for the full token-issuance / userinfo / invalidation contract.

The Lua loader exposes the same block via the `oauth` builder:

```lua
oauth({ enforce = true, issuer = "https://example.test/oauth" })
```

Both fields are optional and the call may appear at most once per
scenario (a second call returns a load-time error).
`fixtures/jmap-oauth.toml` is the canonical bearer-enforced fixture
and demonstrates the revoked-token-recovery flow harness scripts can
drive against `/oauth/token` + `/test/oauth/invalidate`.

## Validation rules

The mock refuses to start (non-zero exit, stderr message) if:

- TOML parse fails.
- Any `email.mailbox_ids` references a mailbox not declared in the
  fixture.
- Any `mailbox.parent_id` references a mailbox not declared in the
  fixture.
- A mailbox's role is set but not one of the seven recognized values.
- Two mailboxes share an `id`.
- Two emails share an `id`.
- `account.is_personal` is `false`.
- An email has neither `body_text` nor `body_path` (nor `body_html`
  once added).
- A `body_path` does not exist or is not readable.
- `received_at` cannot be parsed.

Notable non-rules - things the validator deliberately accepts so
that adversarial-shape fixtures can be authored:

- Two or more emails sharing the same `Message-Id`. None of the
  per-protocol projections key on `Message-Id`; it is emitted as a
  header (IMAP, Gmail) or as the `messageId` array on each email
  (JMAP), so duplicates are wire-safe and only differ from
  unique-id fixtures in the header value clients see. Useful for
  testing how ratatoskr's incremental sync handles the case where
  two distinct messages happen to share a Message-Id (a real-world
  edge case from broken senders).

## Determinism contract

For any fixture, every response from the mock is byte-identical across
runs:

- Mailbox/email iteration order is fixture declaration order.
- `Email/query` sorts by `receivedAt` desc, ties broken by `id`
  lexicographic.
- `partId`s are derived from email id + body type
  (e.g. `"email-001:text"`, `"email-001:html"`), never random.
- `state` tokens are the fixture-level `state` field, or a stable
  default if absent. They never change during a run.

## Incremental change scripts

Both authoring formats project to the same `Vec<ChangeStep>` on the
loaded fixture. A Lua `change(...)` call (or one TOML `[[change]]`
table) adds one named entry to the fixture's incremental-sync
script. Each entry is a `ChangeStep` with an `id` plus zero-or-more
op buckets. The harness drives steps via `POST /test/fixture/step`
(see `notes/orchestration.md`); each step's ops accumulate into a
single `Fixture::mutate` call so the change_log gains exactly one
transition per step. RFC 8620 §5.2 dominance applies naturally on
subsequent `Email/changes` walks.

`fixtures/jmap-incremental.lua` and `fixtures/jmap-incremental.toml`
are equivalent fixtures asserted byte-identical by
`tests/lua_fixture.rs::lua_incremental_fixture_matches_equivalent_toml`.

```lua
change({
    id = "new",                                -- required, unique within script
    email_create = {
        {
            id = "email-003",
            mailbox_ids = { "mb-inbox" },
            received_at = "2026-01-15T12:00:00Z",
            from = "carol@example.com",
            to = { "test@example.com" },
            subject = "Lunch?",
            body_text = "Free at 12:30?",
            message_id = { "<003@example.com>" },
        },
    },
    email_update = {
        { id = "email-002", keywords = { "$seen", "$flagged" } },
    },
    email_move = {
        { id = "email-002", mailbox_ids = { "mb-archive" } },
    },
    email_destroy = { "email-001" },
    mailbox_create = {
        { id = "mb-projects", name = "Projects" },
    },
    mailbox_update = {
        { id = "mb-inbox", sort_order = 5 },
    },
    mailbox_destroy = { "mb-old" },
    event_create = { ... },
    event_update = { { id = "ev-1", subject = "Renamed" } },
    event_destroy = { "ev-0" },
})
```

Op contracts:

- **`email_create`**: array of email tables. Same field set as the
  top-level `email` builder, minus `attachments` (rejected at
  script load - revisit when a fixture wants delta-time
  attachment scenarios). Mailbox ids are validated against the
  fixture state at apply time, so a step that creates a mailbox
  earlier in its op list can then create an email into it.
- **`email_update`**: array of `{ id, keywords?, mailbox_ids? }`.
  Each emits a JMAP-shape patch (`keywords` and / or `mailboxIds`
  full-replace) routed through the same `apply_email_patch` the
  JMAP `Email/set` mutator uses. At least one of the two fields
  must be present.
- **`email_move`**: array of `{ id, mailbox_ids }`. Wire-equivalent
  to `email_update` with a `mailbox_ids` full-replace; the step
  handler additionally reports the id under
  `changes.emails.moved` in the response so harness asserts can
  distinguish a move from a flag flip.
- **`email_destroy`**: array of email-id strings.
- **`mailbox_create`**: array of mailbox tables (same fields as
  the top-level `mailbox` builder). Apply time validates the
  declared `parent_id` and rejects duplicates.
- **`mailbox_update`**: array of `{ id, name?, parent_id?,
  sort_order?, role?, is_subscribed? }`. Routes through
  `apply_mailbox_patch`.
- **`mailbox_destroy`**: array of mailbox-id strings. Apply
  rejects destroy if any email still references the mailbox.
- **`event_create` / `event_update` / `event_destroy`**: same
  shapes for calendar events, projecting through the Graph
  `calendarView/delta` path. Patches use plain RFC3339 strings
  for `start` / `end` (the change-script projection), not the
  Graph nested `start.dateTime` form.
- **`contact_folder_create`**: array of `{ id, display_name,
  parent_folder_id?, is_default? }`. Same shape as the top-level
  `contact_folder` builder. Apply rejects duplicate ids and
  forward references to undeclared parents.
- **`contact_folder_update`**: array of `{ id, display_name?,
  parent_folder_id? }`. At least one field must be set.
- **`contact_folder_destroy`**: array of folder-id strings. Apply
  rejects destroy if any contact still references the folder
  (forces the script author to destroy the contained contacts
  first).
- **`contact_create`**: array of `{ id, folder_id, display_name?,
  emails }`. Same `emails` shape as the static builder. Folder
  reference is validated at apply time so a step can create the
  folder earlier in its op list and a contact later.
- **`contact_update`**: array of `{ id, display_name?,
  folder_id?, emails? }`. `emails`, when present, is a
  full-replace. `folder_id` validates against the current
  fixture's folders at apply time (lets a step move a contact to
  a folder created by an earlier op in the same step).
- **`contact_destroy`**: array of contact-id strings.

### TOML projection

The TOML form mirrors the Lua surface field-for-field. Each step is
a `[[change]]` table; per-op buckets are `[[change.email_create]]`
arrays of inline tables (or `email_destroy = [...]` / similar for
the id-only buckets). Patch shape and op order match the Lua loader
exactly, so the produced `ChangeStep`s are byte-identical:

```toml
[[change]]
id = "new"

[[change.email_create]]
id = "email-003"
mailbox_ids = ["mb-inbox"]
received_at = "2026-01-15T12:00:00Z"
from = "carol@example.com"
to = ["test@example.com"]
subject = "Lunch?"
body_text = "Free at 12:30?"
message_id = ["<003@example.com>"]

[[change]]
id = "change"

[[change.email_update]]
id = "email-002"
keywords = ["$seen", "$flagged"]

[[change]]
id = "delete"
email_destroy = ["email-001"]

[[change]]
id = "move"

[[change.email_move]]
id = "email-002"
mailbox_ids = ["mb-archive"]
```

Same op contracts apply (attachments rejected in `email_create`,
`email_move.mailbox_ids` non-empty, every `*_update` requires at
least one field set, etc.). TOML patches never use the JMAP
camelCase wire form directly; the fields are the friendly Lua names
(`mailbox_ids`, `parent_id`, `sort_order`, `is_subscribed`) and the
loader rewrites them into the JMAP-shape patch the apply layer
expects.

## Reserved for v1+

- Multiple accounts per fixture (the `[account]` shape is already
  positioned to accept `[[account]]`).
- Per-mailbox `rights` overrides.
- Multipart MIME via `body_path` (multipart/alternative HTML+text).
- Failure injection: `[fault]` blocks scoped to method calls (slow
  responses, retryable errors, `cannotCalculateChanges`).
- Attachments inside change-script `email_create` ops.
