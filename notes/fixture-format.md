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

[[account]]
id = "account-1"
# Used as both the JMAP account name and the address ratatoskr stores
# in its accounts row. Should be an email-shaped string - the client
# falls back to it when principals lookup fails (out of scope for v0,
# but cheap to satisfy).
name = "test@example.com"
# true = the authenticating user's own account (the common case).
# false = a foreign / shared mailbox the user merely has access to;
# the JMAP session reports it as `isPersonal: false`. Nothing else in
# the mock branches on the flag - a non-personal account is served
# exactly like a personal one.
is_personal = true
# primary = true     # optional for single-account fixtures (the lone
                     # entry is auto-promoted); required when more than
                     # one account is declared
```

Multi-account fixtures repeat the `[[account]]` block. Exactly one
entry must be flagged `primary = true`; that account is the one
non-JMAP protocols (Graph `/me/...`, IMAP, SMTP, Gmail, gcal,
People, CalDAV) scope to. JMAP advertises every declared account
in the session resource and method handlers honour the request's
`accountId` argument.

Resources (mailboxes, emails, calendars, contacts, categories)
accept an optional `account_id` field; the loader defaults to
the primary account when absent. Validation rules:

- A declared `account_id` must match one of the declared
  `[[account]]` ids.
- Children inherit from parents: events take their calendar's
  account, contacts take their folder's account, emails take
  their first mailbox's account (and reject straddling
  mailboxes across accounts).
- Mailbox / contact-folder hierarchies (parent / child) must
  share an account.

Stage 3 of the refactor adds the Graph `/v1.0/users/{id}/...`
parallel routes plus the universal primary-filter rewrite for
non-JMAP protocols; in the interim, fixtures should declare
resources on non-primary accounts only when they're exercised
through JMAP.

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

### Cross-protocol raw-bytes escape hatch

A third optional field, `body_raw_bytes`, lets a fixture override
what the wire layers emit for the body section. When set, the
mock hands those bytes back verbatim instead of composing from
the canonical `body_text` + `attachments`:

- IMAP: `BODY[]` / `RFC822.SIZE` / `BODY[HEADER]` / `BODY[TEXT]` /
  `BODY[1]` / `BODY[1.MIME]` all read from the raw block. The
  bytes are split at the first `\r\n\r\n` to derive the header /
  text sub-fetches. Sub-parts (`BODY[N]` for N > 1) return NIL -
  the mock does not parse the raw block. `BODYSTRUCTURE` projects
  a single `text/plain` leaf reporting the raw octet count, which
  is a deliberate lie when the bytes claim multipart but is the
  best syntactically-valid answer the mock can give without
  parsing.
- JMAP: `Email/get`'s `bodyValues[<text part>].value` carries the
  raw bytes lossily decoded as UTF-8; ill-formed sequences
  collapse to U+FFFD since JSON strings cannot carry arbitrary
  bytes. `textBody[0].size` reflects raw byte length. Useful for
  injecting anomalous body content (CRLF-only, bare-LF, weird
  encoded-words) through ratatoskr's JMAP parser.
- Gmail: `threads.get`'s `payload.body.data` carries the raw
  bytes base64url-encoded, with no `parts[]` tree (the raw block
  is the entire body, so a multipart wrapper is suppressed).
  `payload.headers[]` keeps the structured headers so ratatoskr's
  Gmail metadata path still finds From / To / Subject. `snippet`
  and `sizeEstimate` read off the raw bytes too.
- Graph: not yet wired (Graph projects a parsed metadata view,
  which sidesteps the malformed-MIME testing surface that
  ratatoskr's Graph mail parser exercises). Track in TODO.md if
  a fixture needs Graph adversarial coverage.

```toml
[[email]]
id = "broken-mp"
mailbox_ids = ["mb-inbox"]
received_at = "2026-01-15T10:00:00Z"
from = "alice@example.com"
to = ["test@example.com"]
subject = "malformed"
body_text = "ignored on the IMAP wire"
body_raw_bytes = """\
From: alice@example.com\r
Subject: malformed\r
Content-Type: multipart/mixed; boundary="X"\r
\r
--X-but-no-real-boundary\r
broken body\r
"""
```

Coexists with `body_text`: when `body_raw_bytes` is unset, the
canonical `body_text` is the source of truth on every wire and
nothing changes. When it IS set, IMAP / JMAP / Gmail all switch to
the raw block; structured headers (subject, from, to, ...) keep
flowing from the canonical fields so the wire is still
addressable. Mutually exclusive with `attachments` (the raw block
IS the entire body, so authoring per-attachment metadata alongside
is rejected at load time). For pure-IMAP adversarial fixtures,
`body_text` can be a minimal placeholder.

Worked tests in `tests/malformed_mime.rs` exercise the JMAP and
Gmail cross-protocol surfaces; `tests/imap.rs::body_raw_bytes_
emits_verbatim_through_imap_fetch` exercises IMAP.

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
    { name = "Bob", email = "bob@example.com", status = "accepted" },
    "carol@example.com",
]                          # optional; each attendee is a bare email string
                           # or a { name?, email, status? } table. `status`
                           # is the RSVP participation status - one of
                           # `needs-action` (default) / `accepted` /
                           # `declined` / `tentative` (case-insensitive;
                           # the provider spellings `needsAction`,
                           # `notResponded`, `tentativelyAccepted` are also
                           # accepted). This single slot is what every
                           # provider's RSVP write mutates and reads back:
                           # Graph `attendees[].status.response`, Google
                           # `attendees[].responseStatus`, JMAP
                           # `participants[].participationStatus`, CalDAV
                           # `ATTENDEE;PARTSTAT=`. The organizer carries no
                           # status slot (implicitly accepted).
is_all_day = false         # optional
recurrence_rule = "FREQ=WEEKLY;BYDAY=MO,WE,FR;COUNT=10"   # optional;
                                                          # raw RFC 5545
                                                          # RRULE value
                                                          # without the
                                                          # "RRULE:" prefix
recurrence_exdates = [
    "2026-03-15T17:00:00Z",
    "2026-07-15T17:00:00Z",
]                                                         # optional;
                                                          # excluded
                                                          # recurrence
                                                          # instances
```

Recurrence per protocol:

- CalDAV: emits one `RRULE:` line plus one `EXDATE:` per excluded
  date in the VEVENT body; parses the same on PUT for full
  round-trip. Unknown RRULE keys travel through verbatim.
- Google Calendar v3: emits a `recurrence: ["RRULE:...",
  "EXDATE:..."]` array per
  developers.google.com/calendar/api/v3/reference/events.
- Microsoft Graph: parses the RRULE and emits a structured
  `recurrence: { pattern, range }` object covering daily /
  weekly / absoluteMonthly / relativeMonthly / absoluteYearly
  patterns with `noEnd` / `endDate` / `numbered` range types.
- JMAP JSCalendar: emits a `recurrenceRules: [{ ... }]` array
  with `frequency` / `interval` / `byDay` / `byMonthDay` /
  `byMonth` / `count` / `until`.

The four projections derive deterministically from the raw RRULE
+ EXDATE inputs in `src/recurrence.rs`; mutation paths on Graph /
gcal / JMAP currently ignore inbound recurrence (writes leave
`recurrence_rule` / `recurrence_exdates` empty). CalDAV PUT
round-trips automatically.

### Calendar PULL read affordances

Additional optional keys stage the shapes the windowed calendar
PULL surface exercises (Google `events.list` timeMin/timeMax,
Graph `calendarView`, JMAP `CalendarEvent/query`, CalDAV
`calendar-query` REPORT):

```toml
[[calendar]]
caldav_empty_report = true   # optional; the CalDAV calendar-query
                             # REPORT returns an empty-but-successful
                             # 207 multistatus regardless of the
                             # events the calendar holds (exercises a
                             # consumer's empty-pull deletion guard).

[[event]]
time_zone = "Europe/Oslo"    # optional; timed values project as a
                             # TZID-bearing wall-clock: CalDAV emits
                             # DTSTART;TZID=<zone>:<local> and JMAP a
                             # bare `start` + `timeZone`. The stored
                             # UTC clock-face digits become the local
                             # wall-clock under the zone (no tz-db
                             # conversion). Ignored for all-day and by
                             # Graph / Google (which keep UTC).
reminders = [                # optional; VALARM (CalDAV) + JSCalendar
                             # `alerts` (JMAP). Graph / Google omit.
    { trigger = "-PT15M" },                       # relative-to-start
    { trigger = "-PT5M", related_end = true,      # relative-to-end
      action = "EMAIL" },
    { trigger = "20260602T133000Z",               # absolute (iCal UTC)
      absolute = true },
]
raw_ical = """..."""         # optional; verbatim iCalendar body served
                             # by CalDAV GET / REPORT instead of the
                             # structured projection. Stages a
                             # multi-VEVENT resource (master +
                             # RECURRENCE-ID overrides + STATUS:CANCELLED
                             # in one .ics) or a deliberately malformed
                             # body (a per-resource parse failure the
                             # consumer surfaces in Page.failed_ids
                             # rather than deleting). `start` / `end`
                             # still drive the calendar-query time-range
                             # overlap. Graph / JMAP / Google ignore it.
```

All-day events emit a `VALUE=DATE` DTSTART/DTEND on CalDAV (not a
bare `X-MICROSOFT-CDO-ALLDAYEVENT` hint) with an exclusive end
date (author `end` as the day *after* the last covered day, e.g. a
one-day event on 2026-06-02 uses `end = "2026-06-03T00:00:00Z"`),
matching the uniform exclusive all-day end contract.

The Google `events.list` no-token path honours `timeMin` /
`timeMax` (half-open overlap) and `orderBy=startTime`, and a
per-event GET (`.../events/{id}`) serves the event-detail read.
Graph `calendarView` accepts spans up to its ~1095-day maximum
with no clamp. `fixtures/calendar-pull-small.toml` is a worked
example covering every affordance above.

Validation rules:

- `calendar.id` is unique within the fixture.
- `event.id` is unique within the fixture.
- `event.calendar_id` must reference a declared calendar.
- `event.start` and `event.end` are RFC3339; sub-second precision is dropped.
- `event.recurrence_exdates` entries are RFC3339 timestamps and parsed via
  the same path as `start` / `end`.

Calendars and events project over the Microsoft Graph
`/v1.0/me/calendars/...` surface AND the CalDAV listener (same
canonical `Calendar` / `Event` types feed both backends). Graph
covers GET / POST / PATCH / DELETE on `/v1.0/me/events/...` and
the `calendarView/delta` walker; CalDAV covers PROPFIND
discovery, REPORT calendar-multiget / calendar-query, GET
event.ics, and PUT / DELETE with `If-Match`. Mutations in either
protocol surface in the other's delta path through the shared
change_log (see `notes/ratatoskr-caldav-surface.md` for the wire
shape).

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
# Enriched columns (all optional). These thread through EVERY provider
# projection (JMAP JSContact, Graph Outlook contact, People Person,
# CardDAV vCard) and each provider's write-back, so a written value
# round-trips on the next read.
phones = [
    { number = "+15551234", kind = "mobile" },  # kind: mobile/home/work/...
    "+15559999",                                # bare-string sugar (no kind)
]
company = "Analytical Engines"
job_title = "Programmer"
department = "R&D"            # note: no vCard slot, so it does NOT
                             # round-trip through CardDAV write-back
notes = "auto-collected note"
groups = ["contactGroups/friends"]  # Google contactGroup membership
malformed_vcard = false      # when true, the CardDAV mock serves an
                             # unparseable vCard body for this contact
                             # (drives bifrost's Page::failed_ids path)
```

Validation:

- `contact_folder.id` is unique across folders.
- At most one folder may have `is_default = true`.
- `contact_folder.parent_folder_id`, if present, references a
  previously-declared folder (no forward references).
- `contact.id` is unique across contacts.
- `contact.folder_id` references a declared folder.

Contacts and folders project over the Graph
`/v1.0/me/contactFolders/...` + `/v1.0/me/contacts/...`, the JMAP
`AddressBook/*` + `ContactCard/*`, the Google People
`/v1/people/me/connections`, and the CardDAV `/addressbooks/...`
surfaces from one canonical type. Each contact-folder is also a
CardDAV address book. Reads and change-log deltas cross all four
providers; a write on any one surfaces on the others.

The Lua loader exposes the same blocks via `contact_folder({...})`
and `contact({...})` builders; `emails` and `phones` accept the same
bare-string-or-table sugar as the TOML form.

### Google-only contact surfaces (optional)

```toml
# Contact groups - GET /v1/contactGroups (bifrost-google fetches this
# unconditionally during address_books_list, so it must exist).
[[contact_group]]
id = "contactGroups/friends"   # People resource name
name = "Friends"

# otherContacts - auto-collected addresses; GET /v1/otherContacts.
[[other_contact]]
id = "otherContacts/oc1"       # People resource name
display_name = "Auto Collected"  # optional
emails = ["auto@example.test"]
phones = [{ number = "+15550000", kind = "work" }]  # optional

# Directory (GAL) people - people:listDirectoryPeople /
# searchDirectoryPeople. An account with ZERO directory_person rows
# answers 403 (models a personal account with no directory; bifrost
# swallows it to an empty page). Declare rows to make a GAL fetch
# return results.
[[directory_person]]
id = "people/dir1"             # People resource name
display_name = "Directory Person"
emails = ["dir@corp.test"]
phones = [{ number = "+15551111", kind = "work" }]
company = "Corp"
job_title = "Engineer"
department = "Platform"
```

All three carry an optional `account_id` (defaults to primary).
Lua builders: `contact_group({...})`, `other_contact({...})`,
`directory_person({...})`.

## Master categories (optional)

```toml
[[category]]
id = "cat-work"
display_name = "Work"
color = "preset0"           # optional; Graph preset enum string

[[category]]
id = "cat-personal"
display_name = "Personal"   # color absent -> field omitted on wire
```

Validation:

- `category.id` is unique across categories.
- `color` is not validated against the Graph preset enum
  (`preset0`..`preset24` or `none`); real Graph accepts unknown
  values too and renders them as the default colour.

Categories project flat over the Graph
`/v1.0/me/outlook/masterCategories` surface (no folder scope -
they're per-account in real Graph). The mock supports the full
CRUD set; mutations land via `Fixture::mutate` and bump
`fixture.state`. The Lua loader exposes the same block via
`category({...})`.

## Groups (optional)

Cross-account groups for the Microsoft Graph `/v1.0/groups/...`
surface. Each group names a `members` list of declared
`[[account]]` ids; the same account can belong to several groups.

```toml
[[group]]
id = "grp-eng"
display_name = "Engineering"
description = "Engineering team"   # optional
mail = "engineering@example.com"   # optional
mail_enabled = true                # optional; defaults to false
security_enabled = true            # optional; defaults to false
members = ["account-primary", "account-secondary"]
```

Validation:

- `group.id` is unique across groups.
- Every entry in `members` must match a declared
  `[[account]]` id.
- Member ids must be unique within one group.

Groups project over the Graph
`/v1.0/groups`, `/v1.0/groups/{id}`,
`/v1.0/groups/{id}/members`, `/v1.0/me/memberOf`, and
`/v1.0/users/{id}/memberOf` surfaces. The Lua loader exposes the
same shape via `group({...})`.

## Shared-folder ACLs (optional)

RFC 4314 access-control grants for the IMAP other-users namespace.
Each `[[acl]]` shares an owned mailbox with another declared
account, exposing it under that account's `#user/<owner>/...`
namespace. Decoupled from `[[mailbox]]` (a top-level table) so
adding shared-folder coverage doesn't touch mailbox rows.

```toml
[[acl]]
mailbox_id = "mbx-bob-inbox"   # an owned mailbox
identifier = "account-alice"   # the account it is shared with
rights = "lr"                  # optional; RFC 4314 rights, default "lr"
```

Validation:

- `mailbox_id` must match a declared `[[mailbox]]`.
- `identifier` must match a declared `[[account]]`.
- `identifier` must not be the mailbox's own account (the owner
  holds full `lrswipkxtea` rights implicitly).
- `(mailbox_id, identifier)` pairs are unique.

Drives IMAP `NAMESPACE` / `LIST "" "#user/*"` / `MYRIGHTS` /
`GETACL` and shared-folder `SELECT` + read. The granted `rights` also
gate writes on the shared selection: `UID STORE` needs `w`, `UID
COPY` needs `i`, `UID MOVE` needs `i` + `t`, `UID EXPUNGE` needs `e`,
and a `SELECT` with no write-shaped right completes `OK [READ-ONLY]`.
So `rights = "lr"` is a read-only shared folder and
`rights = "lrswipkxte"` is a writable one; `fixtures/shared-rights.toml`
stages both in one fixture. The Lua loader exposes the same shape via
`acl({...})`. Grants are also mutable mid-run through the change
script's `acl_grant` / `acl_revoke` ops (see "Incremental change
scripts"); `fixtures/imap-acl-lifecycle.toml` stages a mid-session
grant and revoke. See `notes/ratatoskr-imap-surface.md` "Shared
folders".

## EWS public folders (optional)

Org-wide public-folder tree for the EWS `FindFolder` / `FindItem` /
`GetItem` surface. Not account-scoped: folders hang off a synthetic
`publicfoldersroot`. `[[public_folder]]` declares the tree,
`[[public_item]]` the messages inside it.

```toml
[[public_folder]]
id = "pf-root-eng"
display_name = "Engineering"
# parent_id omitted -> top-level (child of publicfoldersroot)
# folder_class omitted -> "IPF.Note" (a mail folder)
# effective_rights omitted -> read-only (Read alone)

[[public_folder]]
id = "pf-eng-releases"
display_name = "Releases"
parent_id = "pf-root-eng"

# A non-mail public folder plus a writable one.
[[public_folder]]
id = "pf-team-calendar"
display_name = "Team Calendar"
folder_class = "IPF.Appointment"

[[public_folder]]
id = "pf-drafts"
display_name = "Team Drafts"
effective_rights = { read = true, create_contents = true, modify = true, delete = true }

[[public_item]]
id = "pi-eng-001"
folder_id = "pf-root-eng"
subject = "Team sync notes"
from = "lead@example.com"       # bare string or { name, email }
to = ["team@example.com"]       # optional
body_text = "Notes from the weekly sync."   # optional
body_html = "<p>Notes.</p>"     # optional; wins over body_text on GetItem
received_at = "2026-02-01T09:00:00Z"

# Optional; same shape as `[[email.attachment]]`.
[[public_item.attachment]]
blob_id = "pf-blob-001"
name = "sync-notes.txt"
content_type = "text/plain"
disposition = "attachment"      # optional; "attachment" | "inline"
data_path = "blobs/sample.txt"  # relative to the fixture file
```

`effective_rights` accepts any subset of `create_associated`,
`create_contents`, `create_hierarchy`, `delete`, `modify`, `read`,
`view_private_items`; omitted bits default to the read-only shape
(`read = true`, everything else false).

Validation:

- `public_folder.id` / `public_item.id` are each unique within their
  table.
- `public_folder.parent_id` (when present) must reference another
  declared public folder, and a folder cannot parent itself.
- `public_folder.folder_class`, when given, must be non-empty.
- `public_item.folder_id` must reference a declared public folder.
- `public_item.received_at` is RFC 3339.
- Public attachment `blob_id`s are unique across the *whole* public
  tree, not just within their item: EWS `GetAttachment` resolves an
  attachment by id alone, with no item context.
- A public attachment's `data_path` must exist and be readable.

Read-only in v0 (the rights are reported, not enforced - there is no
EWS write surface). No Lua builders yet - TOML-only. See
`notes/ratatoskr-ews-surface.md`.

## OAuth (optional)

```toml
[oauth]
enforce = false                          # default; existing v0 "no auth" baseline
issuer = "https://saehrimnir.test/oauth" # default; echoed in /oauth/userinfo
```

When `enforce = false` (default), the JMAP / Graph / Gmail / CalDAV
HTTP listeners accept any (or no) `Authorization: Bearer` header,
matching the v0 "no auth" rule. When `enforce = true`, those
listeners reject requests whose bearer is not in the active token
set (managed by `crate::oauth::TokenStore`); IMAP and SMTP have
their own auth surfaces and are unaffected. CalDAV's rejection is
a bare `401` with `WWW-Authenticate: Bearer` (no shared error
envelope; the JMAP / Graph / Gmail listeners each return their own
protocol-shaped rejection body). See
`notes/ratatoskr-oauth-surface.md` for the full token-issuance /
userinfo / invalidation contract.

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
- No `[[account]]` block is declared.
- Two declared accounts share an `id`.
- More than one account is declared and either none or more than
  one is flagged `primary = true`.
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
- `account.is_personal = false`. It used to be a hard rejection (a
  Stage 1 invariant of the multi-account refactor). A shared-mailbox
  scenario cannot be staged without it, and no protocol surface
  branches on the flag beyond reporting it, so the rejection bought
  nothing. A fixture may also declare the *primary* account
  non-personal - unusual, but the mock has no reason to care.

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
  Lua-only ergonomics note: when authoring in Lua, place every
  `mailbox(...)` declaration before any `change(...)` call. The
  Lua loader runs an early sanity check on `email_create` mailbox
  references using whichever mailboxes were declared *so far in
  script order*; if a `change(...)` precedes the `mailbox(...)`
  declaration, that early check sees an empty set and rejects
  even though apply-time would have allowed it. The TOML loader
  has no equivalent ordering pitfall (the whole document is
  parsed before validation runs).
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
  emails, phones?, company?, job_title?, department?, notes?,
  groups?, malformed_vcard? }`. Same field shapes as the static
  `contact` builder. Folder reference is validated at apply time so
  a step can create the folder earlier in its op list and a contact
  later.
- **`contact_update`**: array of `{ id, display_name?,
  folder_id?, emails?, phones?, company?, job_title?, department?,
  notes? }`. `emails` / `phones`, when present, are full-replaces;
  the scalar enriched fields set the column. `folder_id` validates
  against the current fixture's folders at apply time (lets a step
  move a contact to a folder created by an earlier op in the same
  step).
- **`contact_destroy`**: array of contact-id strings.
- **`acl_grant`**: array of `{ mailbox_id, identifier, rights? }` -
  the same fields a static `[[acl]]` row carries, `rights`
  defaulting to `"lr"`. Shares an owned mailbox with another
  declared account mid-run (the post-attach ACL addition: a shared
  mailbox granted after the account is already connected). Apply
  time validates the mailbox exists, the identifier is a declared
  account, and the identifier does not own the mailbox; re-granting
  the same rights to the same pair is rejected as a duplicate, while
  re-granting *different* rights re-rights the existing grant (a
  read-only share becoming writable is a real transition).
- **`acl_revoke`**: array of `{ mailbox_id, identifier }`. Withdraws
  the grant (the ACL revocation case). Apply rejects a pair with no
  grant.

  ACL ops run last within a step, so a step may create a mailbox and
  share it in one transition. Both ops advance *two* accounts' state
  tokens - the owner's and the grantee's - because a grant changes
  what the grantee's other-users namespace contains, and a consumer
  polling as the grantee has to see its own state move. The step
  response reports them under `changes.acls.granted` /
  `changes.acls.revoked`, and the live grant set is readable from
  `GET /test/snapshot-state`'s `acls` key. On the IMAP surface the
  effect is immediate and needs no reconnect: `LIST "" "#user/*"`,
  `SELECT`, `MYRIGHTS` and `GETACL` all re-resolve grants from the
  live fixture on every command.

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

[[change]]
id = "share"

[[change.acl_grant]]
mailbox_id = "mbx-bob-projects"
identifier = "account-alice"
rights = "lr"

[[change]]
id = "unshare"

[[change.acl_revoke]]
mailbox_id = "mbx-bob-inbox"
identifier = "account-alice"
```

Same op contracts apply (attachments rejected in `email_create`,
`email_move.mailbox_ids` non-empty, every `*_update` requires at
least one field set, etc.). TOML patches never use the JMAP
camelCase wire form directly; the fields are the friendly Lua names
(`mailbox_ids`, `parent_id`, `sort_order`, `is_subscribed`) and the
loader rewrites them into the JMAP-shape patch the apply layer
expects.

## Slow-paging recipe (Lua-only)

Tests that need to assert client behaviour under a slow server -
client-side timeouts, mid-page progress UI, retry policies - drive
delay through a Lua `on()` callback that calls `wait(ms)` before
returning `nil` (pass-through). The dispatcher holds a mutex while
the callback runs, so `wait(ms)` is `std::thread::sleep` under the
hood: other connections briefly queue on the lock, but unrelated
protocol traffic continues on other tokio workers.

Two worked examples ratatoskr-side scripts can lift verbatim:

### Slow N-th JMAP `Email/query` page

```lua
fixture({ name = "slow-jmap" })
account({ id = "account-1", name = "alice@example.com" })
mailbox({ id = "mb-inbox", name = "Inbox", role = "inbox" })
bulk_emails({ count = 250, mailbox = "mb-inbox", seed = 1 })

-- Pages 1 and 2 stream at full speed; page 3 (the call_index=3
-- Email/query) holds the connection for 4s before passing
-- through. ratatoskr's per-call timeout fires here.
on("jmap", "Email/query", function(req)
    if req.call_index == 3 then
        wait(4000)
    end
    return nil
end)
```

### Slow Graph `messages` `$skiptoken` page

```lua
on("graph", "list_messages", function(req)
    -- req.skiptoken (when populated) is the s.<offset> token
    -- ratatoskr uses to drive subsequent pages. Slow only the
    -- second skiptoken page to mid-stream test partial-progress.
    if req.skiptoken == "s.50" then
        wait(2500)
    end
    return nil
end)
```

For batched per-protocol slowdowns that don't depend on call shape,
prefer `POST /test/latency` (per-protocol or `global` knob) over an
`on()` hook - it skips the dispatcher mutex entirely. See
`notes/orchestration.md` "Test / admin control plane".

## Reserved for v1+

- Per-mailbox `rights` overrides.
- Multipart MIME via `body_path` (multipart/alternative HTML+text).
- Failure injection: `[fault]` blocks scoped to method calls (slow
  responses, retryable errors, `cannotCalculateChanges`).
- Attachments inside change-script `email_create` ops.
