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

## Reserved for v1+

- Multiple accounts per fixture (the `[account]` shape is already
  positioned to accept `[[account]]`).
- Per-mailbox `rights` overrides.
- Multipart MIME via `body_path` (multipart/alternative HTML+text).
- Failure injection: `[fault]` blocks scoped to method calls (slow
  responses, retryable errors, `cannotCalculateChanges`).
- Incremental change scripts: `[[change]]` entries that advance state
  tokens between sync passes.
