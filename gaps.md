# sæhrimnir gap analysis: bifrost wire surface vs mock coverage

Proactive audit (2026-06-19). For each wire-bearing bifrost crate we
enumerated what bifrost's client actually emits and cross-referenced
sæhrimnir's implementation, to find calls that would hit
`unknownMethod` / `BAD` / `404` / a catchall before the bifrost devs
do.

- **Source of truth:** `research/bifrost/crates/*` (the client we are
  migrating TO). The old `research/jmap-client/` was ignored.
- **`research/` is gitignored** (vendored scratch); citations below
  point into it for verification but it is not part of the tree.

**Severity model** (the lens that matters here, learned from the
Thread/get cutover):

- **P0 open-blocker** - emitted during account *open / discovery*. A
  failure means the account never opens (how `Thread/get` hard-failed
  with `Wire(Jmap(UnknownMethod))`). Most important.
- **P1 sync-path** - emitted during normal initial/delta sync or core
  read/write. Failure breaks that sync flow.
- **P2 edge** - write-back, optional args/filters, opt-in features.

Effort: S = small (route/attr/field), M = medium (handler + projection),
L = large (subsystem).

---

## Fix-first (cross-protocol priority)

| # | Gap | Protocol | Sev | Effort |
|---|---|---|---|---|
| 1 | ~~`GET /v1.0/me` (profile root) MISSING -> no Graph account opens at all~~ **DONE** (`src/graph/profile.rs`) | Graph | P0 | S |
| 2 | ~~IMAP `FETCH` parser rejects `ENVELOPE` + `MODSEQ` -> initial inventory FETCH returns `BAD`~~ **DONE** (`src/imap.rs`) | IMAP | P0 (conditional) | M |
| 3 | ~~`Thread/changes` MISSING -> first delta cycle after open fails~~ **DONE** (`src/jmap.rs`) | JMAP | P1 | S |
| 4 | ~~Graph `POST /$batch` MISSING -> all message hydration + writes fail~~ **DONE** for the read/hydration path (`src/graph/mail.rs`); write sub-requests still per-item 501 | Graph | P0/P1 | L |
| 5 | CalDAV `sync-collection` REPORT + `sync-token` PROPFIND prop MISSING (ship together) | CalDAV | P1 | M |
| 6 | People `GET /v1/people/{id}` (single) **DONE** (`src/people/contacts.rs`); `contactGroups.list` STILL MISSING (drives `address_books_list`) | Google | P1 | M |
| 7 | ~~`CalendarEvent/query` MISSING -> JMAP calendar read path (query-based) cannot run~~ **DONE** (`src/jmap_calendar.rs`) | JMAP | P1 | M |
| 8 | No CardDAV surface at all (latent: only when an IMAP fixture configures `carddav`) | CardDAV | P1 (latent) | M |

Items 1 and 2 are true open/initial-sync blockers and should land
first. SMTP is essentially clean.

---

## JMAP (`src/jmap.rs`, `jmap_calendar.rs`, `jmap_contacts.rs`)

**P0: none.** Open path (`research/bifrost/.../jmap/src/sync/factory.rs::open`)
drives only `Email/get` (empty-ids state probe), `Mailbox/get`,
`Thread/get`, and reads session capabilities - all implemented.
`Principal/get` in `fetch_self_emails` is gated on the `principals`
capability we deliberately never advertise, so it is never sent.

### P1

| Method | bifrost evidence | sæhrimnir status | Effort |
|---|---|---|---|
| `Thread/changes` | `sync/changes.rs:239-253`, driven for the Thread cursor scope seeded at `factory.rs:214-221`; fires unconditionally on the first delta cycle after open | **DONE** - `src/jmap.rs::thread_changes` projects the per-account email delta onto threads (created/updated email threads; `destroyed` empty, bifrost reconciles via `Thread/get`) | S (shipped) |
| `CalendarEvent/query` | `sync/calendar_ops.rs:52-60` (`events_in_range`), the core calendar read; gated on `calendars` capability | **DONE** - `src/jmap_calendar.rs::calendar_event_query`: AND FilterOperator of `inCalendar`/`after`/`before`/`text`, `position`/`limit`/`calculateTotal`, start-sorted | M (shipped) |

### P2

| Method | bifrost evidence | sæhrimnir status | Effort |
|---|---|---|---|
| Whole-message blob download (`blob-{id}` / `blob-{id}-text`) | `sync/blob.rs:55-113` (`Email/get` BlobId -> `download(blob-{id})`); cap `open_raw_rfc822` unconditional | PARTIAL - `download` (`src/routes.rs`) only matches `attachments[].blob_id`; whole-message + text-part blobs 404 | S - also match `blob-{id}` / `blob-{id}-text` |
| `Blob/upload` (`uploadUrl`) | `sync/pim.rs:344`; reached only via send-with-attachment (submission-gated) | MISSING - session advertises `uploadUrl` but no `/jmap/upload/{accountId}` route | M - inert until submission lands |
| `Email/queryChanges` | `sync/changes.rs:298-308`, only for `CursorScope::Query` which JMAP inventory never registers | MISSING (catchall) - not driven in v0 | - |

### Driven-but-unused note (not gaps)
bifrost's NEW client does **not** drive `Calendar/changes`,
`CalendarEvent/changes`, or `ContactCard/changes` - its calendar read
is `CalendarEvent/query` and its contacts delta is query-based. Those
three `*/changes` handlers we built serve the **old jmap-client**
ratatoskr still runs today, so keep them until that cutover completes;
they are dead weight only from bifrost's POV.

---

## IMAP (`src/imap.rs`) - hand-verified against both trees

**Conditional P0 (initial-sync blocker).** `parse_fetch_attrs`
(`src/imap.rs:1833`) handles `UID FLAGS INTERNALDATE RFC822.SIZE
RFC822/BODY[]/BODY[HEADER]/BODY[TEXT] BODYSTRUCTURE BODY[N]` - and
nothing else; the fallthrough `other => parse_part_section(other)?`
returns `None` for any non-`BODY[...]` token, which makes the whole
`UID FETCH` reply `BAD`. bifrost's initial inventory FETCH is
`(UID FLAGS ENVELOPE RFC822.SIZE [MODSEQ])`
(`research/bifrost/.../imap/src/account/inventory.rs:225-231`,
`get.rs:213-219`), and it appends `MODSEQ` precisely because we
advertise `CONDSTORE QRESYNC` (`src/imap.rs:39`). So right after
SELECT the inventory FETCH `BAD`s on `ENVELOPE` (always) and `MODSEQ`
(because we advertise CONDSTORE). "Conditional" only in that MODSEQ is
gated on our own advertisement - ENVELOPE breaks it regardless.

### P0 (conditional)

| Item | bifrost evidence | sæhrimnir status | Effort |
|---|---|---|---|
| `FETCH ENVELOPE` | `inventory.rs:227`, `get.rs:215` (every full projection) | **DONE** - `parse_fetch_attrs` parses `ENVELOPE`; `render_envelope` emits the RFC 3501 7.4.2 structure (date/subject/from/sender/reply-to/to/cc/bcc/in-reply-to/message-id) from fixture `Email` | M (shipped) |
| `FETCH MODSEQ` (CONDSTORE) | `inventory.rs:231`, `get.rs:207/219`; `folder_registry.rs` requires a real per-message MODSEQ (errors on "FETCH returned MODSEQ 0") | **DONE** - emits `MODSEQ (1)` per message (non-zero, consistent with pinned `HIGHESTMODSEQ 1`) | M (shipped) |

### P1

| Item | bifrost evidence | sæhrimnir status | Effort |
|---|---|---|---|
| `UIDPLUS` / `COPYUID` on `UID COPY` (and `UID MOVE`) | bifrost parses `COPYUID`/`APPENDUID` response codes (`account/error.rs:922-923`); mutate path needs the new UID after copy/move | OMITTED in v0 (explicit comment, `src/imap.rs:944`); `UIDPLUS` not in `CAPABILITIES` | M - advertise `UIDPLUS`, emit `[COPYUID <uidvalidity> <src> <dst>]` on COPY/MOVE |
| `SELECT (QRESYNC ...)` + `VANISHED` responses | `factory.rs:349` ENABLEs QRESYNC; `changes.rs` does QRESYNC SELECT and reads `VANISHED` for delta | MISSING (no `VANISHED` emit). bifrost **degrades to CONDSTORE** on QRESYNC SELECT failure (`changes.rs:268`), so soft - BUT the CONDSTORE fallback still needs MODSEQ FETCH (above) to work | M - either implement VANISHED, or rely on CONDSTORE fallback once MODSEQ lands |

### P2
`UID MOVE` (RFC 6851), `APPEND`, `IDLE`, `NOTIFY` - not on the default
open/sync path; only flag if a fixture/flow drives them.

### Intentional omissions confirmed safe
Write paths beyond STORE/COPY/EXPUNGE, IDLE, NOTIFY return `BAD` by
design and bifrost does not drive them on the open/sync path. SASL
mechanism ladder order is irrelevant: the mock accepts any mechanism.

---

## SMTP (`src/smtp.rs`) - essentially clean

The default-driven submission path (greeting -> EHLO -> AUTH
PLAIN/XOAUTH2 -> MAIL/RCPT/DATA, optional STARTTLS, connection-drop in
place of QUIT) is fully serviced.

| Item | bifrost evidence | sæhrimnir status | Sev | Effort |
|---|---|---|---|---|
| `FUTURERELEASE` EHLO advertisement | scheduled-send param; payload already captured by `split_envelope_payload` | not advertised in EHLO | P2 | S |

**Config-pairing note (not a gap):** bifrost refuses AUTH on an
unencrypted socket unless the harness selects `SubmissionTls::Plaintext`
(`research/bifrost/.../imap/src/account/submission.rs:198-200`,
`mod.rs:345-358`). A plaintext sæhrimnir is only auth-compatible under
`SubmissionTls::Plaintext`; under StartTls/Implicit the AUTH is refused
client-side pre-wire.

---

## Microsoft Graph (`src/graph/`)

**P0 - this is the worst single finding in the whole audit.**

| METHOD path | bifrost evidence | sæhrimnir status | Effort |
|---|---|---|---|
| `GET /v1.0/me?$select=displayName,mail,userPrincipalName` (+ `/users/{id}?$select=...` for shared) | the FIRST call `GraphAccountFactory::open` makes (`research/bifrost/.../graph/src/account/mod.rs:290`, `api.rs:6-12`) | **DONE** - `src/graph/profile.rs` serves `/v1.0/me` (bearer-resolved) + `/v1.0/users/{id}` (named, `me` alias, unknown 404), projecting `id`/`displayName`/`mail`/`userPrincipalName` | S (shipped) |
| `POST /v1.0/$batch` | `client.rs:323-328`; message hydration (`get.rs:95-123`) + every PIM write | **DONE for hydration** - `src/graph/mail.rs::batch` services `GET .../messages/{id}` sub-requests (the metadata-hydration path) and returns a per-item error for others, so write batches degrade per-item rather than batch-wide. Write sub-requests (PATCH/move/destroy) still per-item 501 -> P2 follow-up | L (read path shipped) |

### P1

| METHOD path | bifrost evidence | sæhrimnir status | Effort |
|---|---|---|---|
| `GET /v1.0/me/messages/{id}?$select=...` (single message) | `get.rs:317-321` (inside $batch), `pim.rs:903-924` (read-modify-write) | **DONE** - `src/graph/mail.rs::get_message_impl` (+ `/users/{user}` twin), reuses `message_value`; shared `message_get_value` also feeds `$batch` | M (shipped) |
| `GET /v1.0/me/messages?$select=...&$filter=conversationId eq '...'&$top=50` (collection + thread) | `pim.rs:932-944`, `search_url` `pim.rs:1487+` | **DONE** - `src/graph/mail.rs::list_messages_collection_impl` (account-wide, honours `conversationId` filter -> `thread_id`, `$top`/`$skiptoken` paging). `$search` falls through to full list -> P2 | M (shipped) |
| `GET /v1.0/me/messages/{id}/$value` (assembled RFC822) | `blob.rs:198,259-266` (the only honest body path; metadata hydration defers real bytes here) | **DONE** - `src/graph/mail.rs::get_message_value_impl` reuses `crate::imap::assembled_rfc822` (multipart when attachments present) | M (shipped) |
| `GET /v1.0/me/calendars/{id}/calendarView?startDateTime=&endDateTime=...` (non-delta range) | `calendar.rs:75-80` (`events_in_range`) | **DONE** - `src/graph/calendar.rs::calendar_view_impl` (+ `/users/{u}` twin); coarse `[start, end)` overlap filter, `$top`/`$skiptoken` paging | S (shipped) |
| `GET /v1.0/me/contacts?$select=...&$top=N` (folder-agnostic list) | `contacts.rs:351,396` (`contacts_path` with no/`default` book) | **DONE** - `src/graph/contacts.rs::list_all_contacts_impl` (+ `/users/{u}` twin); account-wide across folders, `$top`/`$skiptoken` paging | S (shipped) |

The shared-mailbox `/v1.0/users/{userId}/...` twins are covered for
mail/calendar/contacts/categories delta, but the same single-message /
`$value` / `$batch` / `calendarView` gaps apply on that prefix too.

### P2
**DONE:** message writeback - `PATCH /me/messages/{id}` (`isRead` /
`flag.flagStatus` / `categories` mapped to fixture keywords;
`importance` accepted but not stored), `DELETE /me/messages/{id}`
(UID-retiring permanent delete + `email_destroyed`), and
`POST /me/messages/{id}/move` (`destinationId` re-parent + UID sync).
`src/graph/mail.rs`.

**DONE:** PATCH/DELETE/move as `$batch` sub-requests - the batch
handler holds one write guard and routes each sub-request through the
shared message cores, so bifrost's batched writes (it routes message
mutations through `$batch`, not the direct endpoints) work
end-to-end.

**DONE:** mailFolder CRUD - `src/graph/mail.rs`: POST create
(top-level + childFolders), PATCH rename, POST `/move`, DELETE,
mutating the shared `Mailbox` set with `mailbox_*` transitions.

**DONE:** contact write verbs (`POST`/`PATCH`/`DELETE`, default +
folder-scoped) + the contact list `$filter=emailAddresses/any(...)`
- `src/graph/contacts.rs`, mutating the shared `Contact` set with
`contact_*` transitions.

**DONE:** event RSVP actions (`POST /me/events/{id}/{accept|decline|
tentativelyAccept}` -> 202, accept-and-ignore) and GAL directory
search (`GET /users` + `/me/users` with `startswith` filter over
accounts) - `src/graph/calendar.rs` + `src/graph/profile.rs`.

**DONE (accept-and-ignore stubs, `src/graph/settings.rs`):**
`mailboxSettings` (vacation: GET disabled, PATCH echoes),
`messageRules` (inbox filters: empty list + create/patch/delete
stubs), `POST /subscriptions` + renew/delete (webhook push, opt-in).
None durably stored - no fixture slot.

**DONE:** mail draft create + send - `POST /me/messages` stores a
`$draft` Email in the Drafts-role mailbox (or first mailbox) and
`POST /me/messages/{id}/send` returns 202 (the draft stays; v0 does
not model the Sent transition). bifrost's send path is
create-draft-then-send, not `/sendMail`. `src/graph/mail.rs`.

The Graph write tier is now complete - every gap the audit
identified (P0 / P1 read+sync AND the P2 write surface) is closed.

### Confirmed safe (do not flag)
EWS, public folders, OneDrive/`host_attachment`, `attachment_upload`
(bifrost returns Unsupported before the wire), `/groups` +
`/me/memberOf` (implemented and not even required), `masterCategories`
(implemented; bifrost sets categories via `$batch` PATCH, not this
endpoint).

**Smell:** the catchall returns 404 `ResourceNotImplemented`; bifrost
classifies Graph 404 as a hard not-found, so the profile-root 404
surfaces as a misleading open failure. Fixing `GET /v1.0/me` is the
single highest-leverage Graph change.

---

## Google APIs (`src/gmail/`, `src/gcal/`, `src/people/`)

**P0: none.** Open is `users.getProfile`; discovery/inventory/changes
use `/labels`, `/messages` list + metadata, `getProfile`, `/history` -
all routed. Gaps are on the PIM / contacts / write paths.

### P1

| METHOD path | sub-API | bifrost evidence | sæhrimnir status | Effort |
|---|---|---|---|---|
| `GET /v1/people/{resourceName}?personFields=...` (single) | People | `contacts.rs:213-227` (`get_person`); also the etag prefetch before `updateContact` | **DONE** - `src/people/contacts.rs::get_person` (bare-id GET on `/v1/people/{spec}`; `{id}:verb` forms keep PATCH/DELETE). Unblocks `contact_get` + the update etag prefetch | M (shipped) |
| `GET /v1/contactGroups?groupFields=...` | People | `contacts.rs:42-43,490` (drives `address_books_list`) | MISSING -> catchall 404 | M |
| `GET /v1/people/me/connections` without syncToken (full-page) | People | `contacts.rs:74-81` | PARTIAL - route exists but is built around `syncToken`/410 delta recovery that bifrost never uses; verify the plain full-list (no-token) path returns all connections | S (verify) |

### P2
gcal single-event `GET /calendar/v3/calendars/{id}/events/{eventId}`
(`calendar.rs:80-92`; we register only PATCH/DELETE on
`events/{id}`) - breaks `event_get`/`event_rsvp`, **S**; gcal event
`move`; Gmail `threads/{id}/modify` + `messages/{id}/modify` +
`batchModify`/`batchDelete` + `messages/send` + drafts + settings
filters/vacation + label CRUD + `watch`; People
`createContact`/`searchContacts`/`listDirectoryPeople`/photo verbs.

### Confirmed safe / dead mock surface
Gmail attachments 404 stub (only on blob hydration); `/history`
read-only no-op (valid "no changes"); People `syncToken`/410 machinery
and `/otherContacts` (bifrost drives neither - it full-pages
connections and never calls otherContacts); Calendar `syncToken`
(bifrost pages via `pageToken`, recovers via `timeMin`/`timeMax`).
Note: gcal list should tolerate (ignore) bifrost's `singleEvents` /
`orderBy` params.

---

## CalDAV (`src/caldav/`)

Discovery walk (OPTIONS, PROPFIND principal -> home -> calendar
listing) and write-back (PUT/DELETE with If-Match/412) are correct and
fully match bifrost. The break is the **incremental-sync cursor**, and
the two pieces are coupled - ship them together or not at all.

### P0 / P1 (coupled)

| Item | bifrost evidence | sæhrimnir status | Effort |
|---|---|---|---|
| `<D:sync-token/>` property on the calendar PROPFIND | requested in `PROPFIND_CALENDARS` (`research/bifrost/.../caldav/src/client.rs:809`), parsed (`parse.rs:119-121`), seeds the cursor at open (`account.rs:127-133`) | MISSING - `calendar_props` emits resourcetype/displayname/getctag/color/privilege/comp-set but never `sync-token` (`src/caldav/mod.rs:544-592`). Result: cursor always `None`, bifrost silently degrades to full PROPFIND+diff every poll and **never exercises the real cursor path** | S |
| `sync-collection` REPORT (RFC 6578) | `sync_events` posts `<D:sync-collection>` with `sync-token` + `sync-level` + `getetag` (`client.rs:273-284,611-623`); parses per-href status (404/410 = destroyed) + new token | MISSING - `handle_report` matches only `calendar-multiget` / `calendar-query`; anything else -> `bad_request` (`src/caldav/mod.rs:807-813`). Once we emit a sync-token, bifrost POSTs this and currently gets `400` = hard sync failure | M |

### P2
`calendar-query` text-match / prop-filter (we honour only `time-range`,
return unfiltered for text queries -> `event_search` false positives);
principal `calendar-user-address-set` + `schedule-outbox-URL`
(best-effort, swallowed); RFC 6638 scheduling POST (intentional, never
reached).

### Confirmed safe
PROPPATCH (never sent), ACLs, MKCALENDAR (we implement it; bifrost
never sends it), If-Match/If-None-Match on PUT/DELETE (fully covered),
OPTIONS (bifrost never sends), calendar-color namespace (bifrost parses
namespace-agnostically).

---

## CardDAV - whole protocol MISSING (latent)

bifrost has a full `carddav` client crate; sæhrimnir has **no carddav
module** (only `caldav/`), and zero mention in `notes/` or `TODO.md`.

**Urgency: latent, not urgent.** There is no standalone CardDAV sync
entry. `CardDavAccountFactory` is invoked only from the IMAP factory's
`open_carddav` when an `ImapAccountConfig` carries `carddav:
Some(CardDavConfig)` (`research/bifrost/.../imap/src/account/factory.rs:163,278-283`),
and DAV open is fail-soft (missing/failed CardDAV degrades to
IMAP-only). Nothing breaks until a fixture configures an IMAP account
with a CardDAV base URL - at which point `discover_addressbook_home` +
`list_addressbooks` run at open and hit a listener that does not exist.

**Surface bifrost drives** (mirrors CalDAV structurally; uses a
snapshot+`getctag` diff model, NOT RFC 6578 sync-collection):

- P0 discovery: PROPFIND `current-user-principal` -> `addressbook-home-set`
  -> Depth:1 home listing (resourcetype `<C:addressbook/>`, displayname,
  `CS:getctag`). `client.rs:52-130`.
- P1 list+fetch: PROPFIND Depth:1 addressbook (`getetag` +
  `getcontenttype`), `REPORT addressbook-multiget` (`getetag` +
  `address-data` = raw vCard), Depth:0 `getctag` short-circuit.
  `client.rs:143-212`.
- P2 write-back: PUT (If-None-Match `*` create / If-Match update),
  DELETE, `REPORT addressbook-query` (text-match over FN/N/EMAIL/TEL/
  ADR/ORG/TITLE/NOTE). No MKCOL, no PROPPATCH, no sync-collection.

**Sketch of `src/carddav/`** (effort: M for v0 read-path, +S for
write-back): mirror `src/caldav/` 1:1 - `mod.rs` dispatch on
OPTIONS/PROPFIND/REPORT/GET/PUT/DELETE (drop MKCALENDAR); reuse
`caldav/xml.rs` verbatim (protocol-agnostic); new `vcard.rs` analogous
to `caldav/ical.rs` emitting `BEGIN:VCARD`/`VERSION:3.0`/`UID`/`FN`/`N`/
`EMAIL` from the fixture `Contact`. Project `ContactFolder` ->
addressbook collection and `Contact`/`ContactEmail` -> vCard resource
(the same types JMAP/Graph/People already project). Route writes
through `Fixture::mutate` so a CardDAV PUT surfaces in Graph
`contacts/delta` / People delta / JMAP contacts (exactly as CalDAV PUT
already feeds Graph `calendarView/delta`). New `--carddav-port` +
`CARDDAV <port>` sentinel line.

**Fidelity caveat:** the fixture `Contact` carries only `display_name`
+ `emails`; bifrost's vCard parser also reads phones/orgs/addresses/
notes/photos. A read-path v0 round-trips fine (all optional), but a
fixture cannot stage a CardDAV contact with a phone/org until `Contact`
grows those fields - same limitation the People-API write-back already
has.

---

## Summary

Progress as of this pass (see the per-protocol tables above for the
shipped commits):

- **Both true blockers fixed:** Graph `GET /v1.0/me` and IMAP `FETCH
  ENVELOPE`/`MODSEQ`. Graph accounts now open and run initial mail
  sync against the mock.
- **JMAP:** `Thread/changes` + `CalendarEvent/query` shipped - the
  basic-mail-after-open and calendar-read gaps are closed.
- **Graph is COMPLETE - read/sync AND write.** Reads: profile,
  single-message GET, `$value`, `POST /$batch` (hydration + write
  sub-requests), `/me/messages` collection, `calendarView` range,
  `/me/contacts` list, GAL `/users` search. Writes: message
  PATCH/DELETE/move (direct + `$batch`), mailFolder CRUD, contact
  CRUD + email `$filter`, event RSVP, draft create + send, and the
  accept-and-ignore stubs (mailboxSettings, messageRules,
  `/subscriptions`). Only deliberately-out-of-scope surfaces remain
  (EWS, public folders, OneDrive, Drive hosting).
- **Google:** People single-GET shipped; `contactGroups.list`
  (address-book enumeration) still open.
- **SMTP** is clean (one P2: `FUTURERELEASE`).
- **CalDAV** discovery/write are correct; the sync-token +
  sync-collection pair is the one real gap (ship together).
- **CardDAV** is an entire missing protocol, but latent until a fixture
  wires it to an IMAP account.
