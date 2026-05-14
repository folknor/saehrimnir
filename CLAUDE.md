# Sæhrimnir

Deterministic mock email-protocol server. Test peer for ratatoskr's
sync code, spawned by brokkr's `[ratatoskr]` sync commands. Originally
scoped to JMAP only; now growing to cover every protocol ratatoskr's
sync code talks to (JMAP, IMAP, SMTP, Microsoft Graph, Gmail), all
backed by one shared fixture authored as either TOML or Lua.

All five protocols are implemented for v0. See `TODO.md` for what's
left per protocol (mostly future fixture-format growth and sibling
resource modules), and `notes/` for the per-protocol surface docs.

## Where to read

- `notes/orchestration.md` - how brokkr drives us: lifecycle,
  sentinel, env vars, brokkr.toml fields.
- `notes/fixture-format.md` - fixture shape and validation rules,
  shared by the TOML and Lua loaders.
- `notes/ratatoskr-jmap-surface.md` - what the JMAP client expects
  on the wire, with `crates/jmap/src/...:LL` citations.
- `notes/ratatoskr-imap-surface.md` - same shape, for IMAP.
- `notes/ratatoskr-smtp-surface.md` - same shape, for SMTP.
- `notes/ratatoskr-graph-surface.md` - same shape, for Microsoft
  Graph (mail-sync only in v0; the doc also lists the resource
  categories we'll need to scaffold for later).
- `notes/ratatoskr-gmail-surface.md` - same shape, for Gmail's REST
  API.
- `notes/ratatoskr-people-surface.md` - same shape, for Google
  People API contacts (`/v1/people/me/connections` +
  `/v1/otherContacts`). Hosted on a separate listener since real
  People API uses a different host from Gmail.
- `notes/ratatoskr-gcal-surface.md` - same shape, for Google
  Calendar v3 (`/calendar/v3/users/me/calendarList` +
  `/calendar/v3/calendars/{id}/events[/...]`). Sibling listener
  to Gmail.
- `notes/ratatoskr-oauth-surface.md` - mock OAuth 2.0 provider
  mounted on the JMAP listener (`/oauth/token`,
  `/oauth/userinfo`, `/test/oauth/invalidate`) plus the
  fixture-side `[oauth]` block that gates bearer enforcement on
  the mail listeners.
- `notes/request-log.md` - cross-protocol request log
  (`/test/requests`): per-protocol command / detail-key
  contract harness scripts can rely on.
- `TODO.md` - what's left, per protocol.

The notes are the source of truth. Do not refer to siblings
(`../ratatoskr`, `../jmap-client`, `../brokkr`) without first
checking whether the fact is already in `notes/`.

## Project constraints

- Determinism: same fixture in, byte-stable bytes out. Responses
  derive entirely from the fixture; no clocks, no random IDs, no
  unsorted iteration. Output uses `serde_json::Map` (BTreeMap-backed)
  for stable key ordering.
- Auth is opt-in: every protocol accepts any credential by default
  (basic, LOGIN, XOAUTH2, OAUTHBEARER all return success without
  validating). The HTTP-based listeners (JMAP, Graph, Gmail) can
  switch to bearer enforcement by setting `[oauth] enforce = true`
  in the fixture; tokens come from the mock OAuth provider on the
  JMAP listener (`/oauth/token`). IMAP and SMTP keep their own
  always-accept auth surfaces. See
  `notes/ratatoskr-oauth-surface.md`.
- Multi-account fixtures: every resource type carries an
  `account_id`, derived from its parent and validated against
  the declared `[[account]]` set at load time.
  `Fixture::*_for(account_id)` helpers expose filtered
  iterators. JMAP advertises every declared account in its
  session resource and method handlers (`Mailbox/get`,
  `Email/get`, `Email/query`, `Calendar/get`,
  `CalendarEvent/get`) honour the request's `accountId` arg.
  Every Graph resource family (mail, calendar, contacts,
  categories) exposes parallel `/v1.0/users/{userId}/...`
  routes alongside `/v1.0/me/...` (`me` aliases the primary;
  unknown userIds return 404 `ResourceNotFound`). `/v1.0/me/...`
  filters reads by the primary account so multi-account
  fixtures' secondary resources don't leak; folder + calendar +
  contact-folder aliases (`inbox`, `default`) resolve within
  the named account. OAuth tokens carry an `account_id` (via an
  optional form field on `/oauth/token`); the Google-family
  listeners (Gmail, gcal, People) scope reads to the bearer
  token's account via `oauth::account_from_bearer`, falling
  back to primary for missing / unknown tokens. IMAP `LOGIN`
  and `AUTHENTICATE PLAIN` / `LOGIN` / `XOAUTH2` / `OAUTHBEARER`
  parse the SASL response and rebind the connection's
  `account_id` (case-insensitive match against `account.name`,
  or token lookup via `TokenStore`); LIST / STATUS / SELECT /
  FETCH then scope by that account. SMTP `AUTH` shares the same
  SASL helpers; `Submission` records the resolved account
  (exposed on `GET /test/smtp/submissions`). CalDAV resolves
  the `{user}` segment in `/principals/{user}/` and
  `/calendars/{user}/...` against every declared account id;
  PROPFIND / GET / PUT / DELETE scope listings + lookups
  through `Fixture::calendars_for`, so a cross-principal
  calendar reference 404s. Unknown principals 404 too.
- One shared fixture per process, behind a single
  `Arc<RwLock<Fixture>>` (`shared::FixtureHandle`). Read paths take
  brief read guards; the JMAP `Email/set` / `Mailbox/set` mutators
  take a write guard for the duration of the envelope. Guards are
  never held across `.await` or dispatcher callbacks.
- Each protocol projects its own wire shape from the same canonical
  types in `src/fixture.rs`.
- `Fixture::state` advances on every successful mutation; the
  `change_log` (bounded at 256 transitions) records per-resource
  ids per transition. `Email/changes` and `Mailbox/changes` walk
  it via `email_delta_since_account` / `mailbox_delta_since_account`
  to partition the wire delta by the request's `accountId`:
  created / updated ids filter against the live resource's
  `account_id`, and destroyed ids filter against the parallel
  `email_destroyed_accounts` / `mailbox_destroyed_accounts`
  recorded at retire time. Mutations on a multi-account fixture's
  secondary do not leak into the primary's /changes walk. RFC 8620
  §5.2 dominance applies after the filter (created+destroyed
  cancels, created+updated collapses, etc.). Unknown / evicted
  `sinceState` returns `cannotCalculateChanges`.
  Out-of-scope JMAP methods (`EmailSubmission/set`, push,
  `Thread/get`, etc.) still return `unknownMethod`. Out-of-scope
  IMAP commands (write paths, IDLE, NOTIFY, etc.) return `BAD`.
- The session must NOT advertise `urn:ietf:params:jmap:principals`.
  It would pull the client into `Principal/get` and
  `ShareNotification` paths the mock cannot satisfy.
- The session advertises `urn:ietf:params:jmap:calendars` iff the
  fixture carries any `[[calendar]]` entries; this gates the JMAP
  `Calendar/*` and `CalendarEvent/*` surface.

## Layout

- `src/main.rs` - runtime entry. Loads fixture, binds listener,
  writes sentinel, serves until SIGTERM with a 1-second graceful
  budget.
- `src/cli.rs` - clap CLI flags.
- `src/fixture.rs` - canonical types (`Fixture`, `Account`, `Mailbox`,
  `Email`, ...), shared `RawFixture`-shaped intermediate, the
  `normalize` cross-reference validator, and the TOML loader. The
  public `load(path)` dispatches to `lua::load` for `.lua` files,
  TOML otherwise.
- `src/lua.rs` - dellingr-backed Lua scenario loader and reactive-
  callback dispatcher. Exposes the `fixture` / `account` / `mailbox`
  / `email` builder `RustFunc`s plus `bulk_emails`, `bulk_threads`,
  and `bulk_mailboxes` for synthetic-data scale testing, and the
  control helpers `wait`,
  `mock_done`, `mock_fail`. `on(protocol, command, fn)` is also a
  RustFunc that anchors the callback (via dellingr's `Anchor`, the
  `luaL_ref`-style registry) into a Rust-side `HandlerMap`; the
  script's globals stay clean. `Dispatcher` retains the dellingr
  `State` behind a `Mutex` so protocol handlers can fire callbacks
  via `call_anchor`. Accumulates into a `Builder` (wrapped in
  `LuaExtras` alongside the `MockExit` signal slot) in user_data;
  the LuaExtras stays installed past load so dispatch-time
  `mock_done()` / `mock_fail()` can record their signal. Hands the
  `RawFixture` to `fixture::normalize` so validation is shared with
  the TOML path.
- `src/scenario.rs` - main loader entry point. `Scenario { fixture,
  dispatcher }` bundles the validated fixture with the optional
  callback dispatcher. `scenario::load(path)` dispatches by
  extension.
- `src/templates.rs` - synthetic data pools (names, domains,
  projects, teams, topics) and a `fill_template` primitive used by
  `bulk_emails`. Lifted from `<ratatoskr>/crates/dev-seed/src/
  templates.rs` and pruned.
- `src/sentinel.rs` - atomic readiness-file write (temp + rename).
- `src/tls.rs` - self-signed cert + `tokio_rustls::TlsAcceptor`
  generated at startup. Used by SMTP for STARTTLS upgrades; clients
  must accept invalid certs.
- `src/shutdown.rs` - SIGTERM/SIGINT handler.
- `src/lib.rs` - library surface; `main.rs` keeps just the runtime.
- `src/request_log.rs` - cross-protocol request log. `RequestLog`
  is a cheap-to-clone `Arc<Mutex<Vec<RequestEntry>>>` threaded into
  every protocol layer; each command/dispatch event appends one
  entry. Each entry carries an optional `connection_id: u64`
  (allocated by `src/connection_id.rs::next()` once per accepted
  TCP socket) so harness scripts can group entries by session
  without sæhrimnir locking in an aggregation schema - the
  Phase 7 "one LOGIN + one SELECT per (account, folder) batch"
  assertion is a `GROUP BY connection_id`. `?stable=true` rewrites
  raw ids to dense first-seen indices for byte-deterministic
  snapshots. Read out via `GET /test/requests`, cleared via
  `DELETE /test/requests` or `POST /test/fixture/reset`. See
  `notes/orchestration.md` "Test / admin control plane".
- `src/connection_id.rs` - process-wide monotonic `u64` generator
  plus the `ConnInfo` carrier and `OptConnId` extractor. IMAP /
  SMTP `serve_connection` call `next()` once per accepted
  socket; HTTP listeners go through axum's
  `into_make_service_with_connect_info::<ConnInfo>`. Handlers
  pull the id with the infallible `OptConnId` extractor
  (`Option<u64>` newtype) so test paths that bypass
  connect-info plumbing (`tower::ServiceExt::oneshot`)
  serialize as `connection_id: null` rather than 500-ing.
- `src/oauth.rs` - mock OAuth 2.0 / OIDC provider. `TokenStore` is
  the analogous `Arc<Mutex<...>>` handle for active tokens, also
  cleared by `POST /test/fixture/reset`. Endpoints
  (`/oauth/token`, `/oauth/userinfo`, `/test/oauth/invalidate`)
  mount on the JMAP HTTP listener; bearer enforcement on the
  mail listeners is gated by `fixture.oauth.enforce`.
- `src/routes.rs` - axum router, `AppState`, JMAP HTTP route handlers.
  Also serves the test/admin control plane: `/test/smtp/submissions`,
  `/test/requests` (with `?stable=true` to strip wall-clock
  timestamps for byte-deterministic snapshots),
  `/test/fixture/reset` (rewinds the fixture image to the post-load
  baseline + clears volatile state + clears latency knob),
  `/test/fixture/step` (cursor-driven application of the Lua-authored
  `change(...)` script; one Transition per step, atomic apply),
  `/test/snapshot-state` (thin JSON projection of fixture state),
  `/test/latency` (GET / POST per-protocol latency knob).
- `src/latency.rs` - `LatencyKnob: Arc<Mutex<HashMap<String, u64>>>`.
  Each protocol's dispatch entry calls `latency.sleep_for("<tag>")`
  before doing real work; the sum of `"global"` plus the per-tag
  value is the effective delay. Cleared by
  `POST /test/fixture/reset`. The `"attachment"` tag is special:
  it fires once before serving attachment bytes on JMAP
  `/jmap/download/...`, Gmail `/messages/{id}/attachments/{aid}`,
  Graph `/messages/{id}/attachments/{aid}` (JSON + `/$value`), and
  IMAP `FETCH BODY[N]` for `N >= 2` - the per-protocol fetch path
  ratatoskr's prefetch hits across all four backends. Per-attachment
  overrides keyed by blob_id stack on top: `attachment:<blob_id>`
  adds to the base `attachment` value when that specific blob is
  served, so a script can stage one slow attachment among several
  fast ones (leave `attachment` at 0 to keep the others instant).
- `src/jmap.rs` - JMAP request envelope, dispatcher, per-method
  handlers.
- `src/jmap_calendar.rs` - JMAP calendar surface (`Calendar/get`
  + `Calendar/changes` + `CalendarEvent/get` + `CalendarEvent/
  changes` + `CalendarEvent/set`). Projects fixture `Calendar` /
  `Event` to JSCalendar (RFC 8984) objects; mutations land via
  `Fixture::mutate` and append the same `event_*` transitions
  Graph and CalDAV write. `CalendarEvent/changes` unions
  `event_delta_since` across every fixture calendar since JMAP
  carries no per-calendar filter on `/changes`.
- `src/imap.rs` - IMAP listener, connection state machine, command
  dispatcher, RFC 822 emit.
- `src/smtp.rs` - SMTP submission listener + in-memory submission
  capture log.
- `src/graph/` - Microsoft Graph mock. `mod.rs` (router, AppState,
  catchall 404, bearer middleware), `odata.rs` (query parsing,
  pagination cursors, envelope), `mail.rs` (mail-sync handlers),
  `calendar.rs` (calendar + event handlers, including echo-mode
  POST/PATCH/DELETE that record bodies in the request log),
  `contacts.rs` (contact + contactFolder GETs + contacts/delta
  walker, mutations via change-script), `label_sync.rs` (Outlook
  master category list - GET/POST/PATCH/DELETE on
  `/v1.0/me/outlook/masterCategories`, mutations via
  `Fixture::mutate`), `group_sync.rs` (cross-account groups -
  `/v1.0/groups`, `/v1.0/groups/{id}`,
  `/v1.0/groups/{id}/members`, `/v1.0/me/memberOf`,
  `/v1.0/users/{id}/memberOf`). Sibling files for drive / EWS
  land here when those surfaces are scouted.
- `src/caldav/` - CalDAV mock. `mod.rs` (single-handler dispatch
  on PROPFIND / REPORT / GET / PUT / DELETE / OPTIONS for the
  WebDAV verb surface ratatoskr's CalDavClient exercises), `xml.rs`
  (light XML emission + body matching - escape, body_requests_prop,
  collect_hrefs - hand-rolled rather than pulling quick-xml since
  bodies are small and well-known), `ical.rs` (VCALENDAR/VEVENT
  round-trip with the small subset of RFC 5545 v0 needs: UID /
  SUMMARY / DESCRIPTION / LOCATION / DTSTART / DTEND / ORGANIZER /
  ATTENDEE; line unfolding + TEXT escape). Reuses the existing
  `Calendar` / `Event` fixture types; PUT/DELETE mutate through
  `Fixture::mutate` so Graph `calendarView/delta` observes
  CalDAV writes.
- `src/gcal/` - Google Calendar v3 mock (sibling listener to
  Gmail). `mod.rs` (router, AppState, bearer middleware,
  `serve` entry, catchall 404), `events.rs`
  (`/calendar/v3/users/me/calendarList`,
  `/calendar/v3/calendars/{id}/events` GET / POST,
  `.../events/{id}` PATCH / DELETE; sync-token + pageToken
  paging; Google-shaped `dateTime`+`timeZone` for timed events
  and `date` for all-day; mutations write through
  `Fixture::mutate` and record `event_*` transitions parallel
  to Graph / CalDAV / JMAP).
- `src/recurrence.rs` - RFC 5545 RRULE parser shared by the
  Graph and JMAP calendar serializers. Covers FREQ, INTERVAL,
  COUNT, UNTIL, BYDAY (with optional ordinal), BYMONTHDAY,
  BYMONTH; unknown keys are dropped quietly so unsupported
  rules round-trip raw through CalDAV / gcal without breaking
  the structured projections. Exposes `Frequency` and
  `Weekday` helpers that pre-compute the Graph
  (`"monday"`, ...) and JSCalendar (`"mo"`, ...) strings.
- `src/people/` - Google People API mock (sibling listener to
  Gmail; real People API lives on a separate host). `mod.rs`
  (router, AppState, bearer middleware, `serve` entry,
  catchall 404), `contacts.rs`
  (`/v1/people/me/connections` + `/v1/otherContacts` with
  pageSize / pageToken / syncToken parsing, sync-token
  recovery via 410). Projects fixture `Contact` entries flat
  across all folders (People API has no folder concept).
- `src/gmail/` - Gmail REST mock. `mod.rs` (router, AppState,
  catchall 404), `mail.rs` (profile, labels, threads, history,
  attachments stub, MIME payload builder, hand-rolled base64url).
  Sibling files for People-API contacts / Drive uploads land here
  later.
- `tests/api.rs` - JMAP integration tests via
  `tower::ServiceExt::oneshot`.
- `tests/imap.rs` - IMAP integration tests over a duplex stream.
- `tests/smtp.rs` - SMTP integration tests over a duplex stream.
- `tests/graph.rs` - Graph integration tests via
  `tower::ServiceExt::oneshot`.
- `tests/gmail.rs` - Gmail integration tests via
  `tower::ServiceExt::oneshot`.
- `tests/malformed_mime.rs` - asserts `Email::raw_bytes` flows
  through the JMAP `Email/get` `bodyValues` and Gmail `threads.get`
  `payload.body.data` paths in addition to the IMAP wire. Lets a
  fixture inject anomalous body content for client-tolerance
  testing on every parsed surface.
- `tests/lua_fixture.rs` - asserts the Lua loader produces a
  `Fixture` byte-identical to the equivalent TOML fixture, plus
  error paths.
- `tests/scale.rs` - 10k-email scale-correctness tests against
  `fixtures/jmap-bulk.lua`. Verifies pagination through the four
  protocol layers (JMAP `Email/query`, Graph `messages` and
  `messages/delta`, Gmail `threads`) without drops or dupes.
- `tests/lifecycle.rs` - subprocess test that spawns the actual
  binary, polls for the readiness sentinel, hits a real network
  endpoint, sends SIGTERM, asserts a clean exit. Closes the
  coverage gap that `scripts/smoke.sh` covers manually.
- `tests/gcal.rs` - Google Calendar integration tests via
  `tower::ServiceExt::oneshot`. Covers calendarList, events
  listing with paging + syncToken + 410 recovery, create /
  patch / delete round-trip, and a cross-protocol assertion
  that a Google-Calendar create surfaces in a Graph
  `calendarView/delta`.
- `tests/people.rs` - People API integration tests via
  `tower::ServiceExt::oneshot`. Covers the no-token bootstrap
  + paging via `nextPageToken`, the same-token empty-delta
  contract, the unknown-token 410 recovery, and `/v1/otherContacts`'s
  empty-list shape.
- `tests/caldav.rs` - CalDAV integration tests via
  `tower::ServiceExt::oneshot`. Covers OPTIONS, the discovery
  walk (root + well-known + principal + home + Depth 0/1),
  PROPFIND on a calendar, GET event ical, REPORT
  calendar-multiget + calendar-query (time-range filter),
  PUT/DELETE with If-Match, and a cross-protocol assertion that
  a CalDAV PUT surfaces in Graph `calendarView/delta`.
- `fixtures/jmap-small.toml` and `fixtures/jmap-small.lua` - the
  canonical v0 scenario in both authoring formats. Asserted
  equivalent by `tests/lua_fixture.rs`.
- `fixtures/jmap-bulk.lua` - 10k-email scale fixture demonstrating
  `bulk_emails`.
- `fixtures/jmap-oauth.toml` - bearer-enforced
  (`[oauth] enforce = true`) variant of jmap-small. Drives the
  revoked-token-recovery flow asserted in `tests/api.rs`.
- `fixtures/jmap-incremental.lua` and
  `fixtures/jmap-incremental.toml` - equivalent 2-mailbox /
  2-email baseline plus a 4-step change script (new + change +
  delete + move). Lua form drives the integration tests in
  `tests/step.rs`; TOML form is asserted byte-equivalent in
  `tests/lua_fixture.rs`.
- `fixtures/graph-contacts-small.toml` - 2 contact folders + 4
  contacts exercising the full Graph contact wire shape (bare-
  string sugar, `{name, address}` tables, multi-address contacts,
  empty-emails contact). Drives the read-path tests in
  `tests/graph.rs`.
- `fixtures/graph-contacts-incremental.lua` - 3-step contact
  change script (new + change + delete) driving the
  `contacts/delta` round-trip in `tests/step.rs`.
- `scripts/smoke.sh` - boot, curl, SIGTERM verification script.

## Status

JMAP: complete for v0 (session resource, `Mailbox/get`, `Email/query`,
`Email/get`, `Mailbox/changes` + `Email/changes` walking the real
per-state change log, plus `Email/set` and `Mailbox/set` mutators
honouring the create / update / destroy maps and the patch shapes
ratatoskr drives - `keywords` / `keywords/<flag>`, `mailboxIds` /
`mailboxIds/<id>`, plus `name` / `parentId` / `sortOrder` / `role` /
`isSubscribed` on mailboxes. Full integration test coverage).

JMAP calendars: complete for v0. Session advertises
`urn:ietf:params:jmap:calendars` whenever the fixture has any
`[[calendar]]` entry. Surface: `Calendar/get` (no-ids list +
filtered + notFound), `Calendar/changes` (seed-or-current),
`CalendarEvent/get` projecting fixture `Event` to RFC 8984
JSCalendar (`@type: Event`, `id` / `uid`, `calendarIds`,
`title`, `description`, `start` LocalDateTime, `duration` ISO
8601, `timeZone: UTC`, `showWithoutTime` for all-day,
`locations` with `loc1` named entry, `participants` with owner
+ attendee roles + `sendTo: {imip: mailto:...}`),
`CalendarEvent/changes` unioning `event_delta_since` across
every declared calendar (JMAP carries no per-calendar filter
on `/changes`), and `CalendarEvent/set` create / update /
destroy through `Fixture::mutate`. Mutations record the same
`event_*` transitions Graph and CalDAV write, so a JMAP
create surfaces in a follow-up Graph `calendarView/delta`.
Integration tests in `tests/jmap_calendar.rs` cover the
read-path projection, the changes envelope, the create /
update / destroy round trip, and the cross-protocol
visibility assertion.

IMAP: complete for v0's read path (greeting, `CAPABILITY`, `LOGIN`/
`AUTHENTICATE`, `ENABLE QRESYNC`, `LIST`, `STATUS`, `SELECT`/`EXAMINE`/
`CLOSE`, `UID SEARCH`, `UID FETCH` with full RFC 822 body emission
including `multipart/mixed` for fixtures with attachments,
`BODYSTRUCTURE`, `BODY[N]` and `BODY[N.MIME]` sub-part fetch,
CONDSTORE `CHANGEDSINCE`). Single-part text emails stay byte-
identical to the pre-attachment wire format; multipart kicks in only
when `email.attachments` is non-empty (boundary is
`=_saehrimnir_<email-id>_=`). Plus a persistent mutation surface:
`UID STORE`, `UID COPY`, and `UID EXPUNGE`. Each takes a brief
write guard, mutates the shared `Fixture`, bumps `state`, and
records a transition so the change surfaces in the next JMAP
`Email/changes`. `UID STORE` translates IMAP wire flags
(`\Seen`, `\Flagged`, `\Draft`, `\Answered`, `\Deleted`) into
fixture keywords (`$seen`, ...) and back, supporting `+FLAGS` /
`-FLAGS` / `FLAGS` plus `.SILENT`. `UID COPY` adds the target
mailbox id to the matched email's `mailbox_ids[]` and allocates
a fresh UID in the target via `Fixture::assign_uid`; unknown
target returns `NO [TRYCREATE]`. `UID EXPUNGE` drops every
matched message that carries `\Deleted` from the current
mailbox, retiring its slot in `mailbox_uid_history` (so the UID
is never reused) and destroying the email entirely when its
last mailbox membership goes away. Integration tests in
`tests/imap.rs` drive both the full initial-sync transcript and
the persistent writeback / copy / expunge round-trips, plus
RFC 3501 §2.3.1.1 UID-stability regressions.

UIDs are assigned by `Fixture::mailbox_uid_history`: an
insertion-ordered list of email ids per mailbox. Each addition
(load-time email declaration, JMAP `Email/set` create / mailboxIds
add, change-script `EmailCreate` / `EmailMove`, IMAP `UID COPY`)
calls `Fixture::assign_uid` to push the next slot; deletes /
moves-out call `retire_uid` to flip the slot to `None`. Slots
are NEVER reclaimed, so existing UIDs stay stable and UIDNEXT
(`uid_history.len() + 1`) only ever grows. Sequence numbers are
the position in the live (non-retired) view, computed by the
FETCH path. UIDVALIDITY stays pinned at 1 across the fixture
lifetime; tests that need a UIDVALIDITY bump simulation use
`POST /test/fixture/reset`.

SMTP: complete for v0's submission path (greeting, EHLO,
AUTH PLAIN/LOGIN/XOAUTH2/OAUTHBEARER, MAIL FROM, RCPT TO, DATA with
dot-stuffing reversal, RSET, NOOP, QUIT). MAIL FROM / RCPT TO
extension parameters (SIZE, BODY, ENVID, NOTIFY, ORCPT, ...) are
parsed and surfaced as `Submission::from_params` /
`Submission::rcpt_params` (BTreeMaps keyed upper-case). STARTTLS is
advertised when the listener is constructed with a `TlsAcceptor`
(set up by `src/tls.rs`); on upgrade the per-connection state
machine swaps its boxed stream to a `TlsStream<...>` and discards
prior session knowledge per RFC 3207. `Submission::parse_mime()`
exposes a server-side MIME projection of the captured bytes
(subject, text/html bodies, attachments) so tests can assert on the
sent message without each pulling in `mail-parser`. Submissions
captured in an in-memory `SubmissionLog` that tests read directly.
The same `SubmissionLog` handle is also exposed to harness scripts
over the JMAP HTTP listener: `GET /test/smtp/submissions` returns a
parsed-projection JSON array (connection envelope plus subject /
body counts / attachment metadata; raw bytes deliberately not
serialized), and `DELETE /test/smtp/submissions` returns 204 and
clears the log so tests can assert on a clean window without
restarting the binary. Integration tests in `tests/smtp.rs`,
including a TCP-level STARTTLS round-trip; the route shape is
covered in `tests/api.rs`.

Graph: mail-sync, calendar, contacts, master categories, and
groups are complete for v0. Mail:
`/v1.0/me/mailFolders` (list, by-id, by-well-known-alias,
childFolders), `/v1.0/me/mailFolders/{id}/messages` (with `$top` /
`$skip` / `$skiptoken` / `$filter`), `/v1.0/me/mailFolders/{id}/
messages/delta` (initial dump, follow-up no-op, `$deltatoken=latest`
shortcut). Calendar: `/v1.0/me/calendars` (list + by-id + `default`
alias + events list with `$top`/`$skiptoken` pagination + delta
view), `/v1.0/me/events/{id}` GET / PATCH / DELETE, plus
`POST /v1.0/me/calendars/{id}/events`. Calendar mutations are
persistent: POST/PATCH/DELETE on `/v1.0/me/events` mutate the
shared fixture, bump `Fixture::state`, and record `event_created`
/ `event_updated` / `event_destroyed` in the change log. The next
`calendarView/delta` (or `events/delta`) walks the log between
the client-supplied `$deltatoken` and the current state, returning
created/updated events as full bodies and destroyed events as
Graph tombstones (`{ id, "@removed": { reason: "deleted" } }`).
Contacts: `/v1.0/me/contactFolders` (paged list + by-id + `default`
alias), `/v1.0/me/contactFolders/{id}/contacts` (paged list with
`$top` / `$skiptoken`; `$select` parsed and ignored - we always
emit the full `id, displayName, emailAddresses, parentFolderId`
projection ratatoskr's `CONTACT_SELECT` requests),
`/v1.0/me/contactFolders/{id}/contacts/{cid}` (folder-scoped
single), `/v1.0/me/contacts/{cid}` (folder-agnostic single),
`/v1.0/me/contactFolders/{id}/contacts/delta` (initial dump
paginated to a final-page deltaLink, follow-ups walk the change
log, `$deltatoken=latest` shortcut, unknown / evicted token falls
back to bootstrap). Mutations land via change-script ops
(`contact_create` / `contact_update` / `contact_destroy` plus
folder counterparts) routed through `Fixture::mutate`; tombstones
use the Graph `{ id, "@removed": { reason: "deleted" } }` shape.
Master categories: `/v1.0/me/outlook/masterCategories` GET (list +
single) + POST / PATCH / DELETE. Flat per-account (no folder
scope, mirroring real Graph). POST mints `mock-category-N` when
the body omits `id`, or honours a client-supplied id when present
(409 on collision). Mutations land via `Fixture::mutate` and
record `category_*` transitions; the change_log entries are
observability for tests asserting state moved (real Graph has no
`masterCategories/delta` endpoint, so v0 doesn't expose one
either). Tests in `tests/graph.rs`.

Groups: `/v1.0/groups` (list + single), `/v1.0/groups/{id}/members`
projecting each member-account as a `#microsoft.graph.user` (id,
displayName, mail, userPrincipalName populated from
`account.name`), plus `/v1.0/me/memberOf` (bearer-resolved) and
`/v1.0/users/{id}/memberOf` (path-resolved; `me` aliases the
bearer's primary). Groups are cross-account by nature - each
fixture `[[group]]` carries a `members = [...]` list of declared
account ids. Read-only in v0 (no mutating verbs on the Graph
group surface).

Catchall returns the Graph error envelope so unimplemented
resources are visibly out-of-scope. Sibling files for drive /
EWS drop in later.

Google Calendar v3: complete for v0. Sibling listener to Gmail
on `--gcal-port`. `GET /calendar/v3/users/me/calendarList` (id
+ summary + backgroundColor + primary + accessRole=owner). `GET
/calendar/v3/calendars/{id}/events` with `syncToken` /
`pageToken` / `maxResults` paging, `nextSyncToken` emitted on
the final page only, unknown token returns HTTP 410
(`fullSyncRequired` reason - matches the substrings ratatoskr's
recovery checks for). `POST` / `PATCH` / `DELETE` write through
`Fixture::mutate` and record `event_*` transitions parallel to
Graph / CalDAV / JMAP, so a Google-Calendar create surfaces in
a Graph `calendarView/delta` follow-up. Sentinel grows
`GCAL <port>`; brokkr orchestration plumbs
`RATATOSKR_TEST_GCAL_ENDPOINT`. ratatoskr-side override
parallel to `RATATOSKR_TEST_GMAIL_ENDPOINT` hasn't landed
yet - listener is ready when it does.

Google People API: complete for v0's contacts read + write-back path. Sibling
listener to Gmail (real People API lives on a different host).
`/v1/people/me/connections` (paged with `pageSize` / `pageToken` /
`syncToken` / `requestSyncToken`; tombstones via
`metadata.deleted`; `nextSyncToken` on final page only) +
`/v1/otherContacts` (empty list in v0; fixture format has no
`[other_contact]` table yet). Sync-token recovery: an unknown
token returns 410 with the People error envelope, matching the
substrings ratatoskr's recovery path checks for ("syncToken").
A token matching the current fixture state returns an empty
delta + the same token. Plus contact write-back:
`PATCH /v1/people/{id}:updateContact?updatePersonFields=...` (validates
the contact, records a `contact_updated` transition, echoes the
Person back; the patched phone / company / notes fields land in
the request log but aren't durably stored since the fixture
`Contact` schema doesn't carry them) and
`DELETE /v1/people/{id}:deleteContact` (removes the contact and
records `contact_destroyed` so the next delta emits a
`metadata.deleted` tombstone). Catchall returns the People error
envelope. Mounted on `--people-port`; sentinel grows a
`PEOPLE <port>\n` line; brokkr orchestration plumbs
`RATATOSKR_TEST_PEOPLE_ENDPOINT`. ratatoskr-side override
(parallel to `RATATOSKR_TEST_GMAIL_ENDPOINT`) hasn't landed yet
- when it does, sæhrimnir is already in place to receive it.

Gmail: complete for v0's mail-sync path. `/gmail/v1/users/me/profile`
+ `/labels` + `/threads` (list paginated by `nextPageToken`, with
`q=after:YYYY/M/D` filtering) + `/threads/{id}` (full MIME payload
projection of fixture emails into Gmail's nested mimePart shape) +
`/history` (read-only no-op since fixtures don't change) +
`/messages/{id}/attachments/{aid}` (404 stub) + `/settings/sendAs`
(list + per-address GET + PATCH). SendAs identities project from
fixture `[[send_as]]` rows (TOML) or Lua `send_as({...})` builder,
keyed `(account_id, send_as_email)`. List + GET scope to the
bearer-resolved account; PATCH does a sparse merge on
`displayName` / `replyToAddress` / `signature` / `isDefault` /
`treatAsAlias` (real Gmail's read-only `isPrimary` is silently
ignored on write). PATCH writes through a brief write guard
without recording a change-log transition, since Gmail has no
`sendAs`-side `/changes` endpoint and harness scripts re-GET to
observe the update. Catchall returns Gmail error envelope. Module
structure leaves room for People-API contacts and Drive sibling
files.

Calendar recurrence (all four protocols, read + write): events
grow `recurrence_rule: Option<String>` (raw RFC 5545 RRULE value,
no `RRULE:` prefix) and `recurrence_exdates: Vec<DateTime<Utc>>`
on the canonical `Event`. Authoring uses `recurrence_rule` /
`recurrence_exdates` keys on TOML `[[event]]` and the Lua
change-script `event_create` op. Per-protocol projections (read +
write symmetric):
- CalDAV: emit `RRULE:` line plus one `EXDATE:` per excluded
  date; PUT parses the same back.
- Google Calendar v3: emit / parse a `recurrence: ["RRULE:...",
  "EXDATE:..."]` array on GET / POST / PATCH. `EXDATE;TZID=...`
  parameters are stripped.
- Microsoft Graph: translate between the parsed RRULE and a
  structured `recurrence: { pattern, range }` shape covering
  daily / weekly / absoluteMonthly / relativeMonthly /
  absoluteYearly patterns with noEnd / endDate / numbered range
  types. POST / PATCH parses the structured form back to an
  RRULE via the inverse translator; `recurrence: null` on PATCH
  clears the rule.
- JMAP JSCalendar: emit / parse a `recurrenceRules` array of
  `RecurrenceRule` objects (frequency, interval, byDay,
  byMonthDay, byMonth, count, until). `CalendarEvent/set` create
  + update accept the structured form; `recurrenceRules: null`
  on update clears.
`src/recurrence.rs` houses the shared `ParsedRule` plus
`format_rrule` (the write-side inverse). Graph models exceptions
as separate exception-event resources (not EXDATE arrays) and
JSCalendar via `recurrenceOverrides`; v0 ignores both on write
- the static `[[event]]` shape remains the way to author
fixture-side EXDATEs.

CalDAV: complete for v0's calendar-sync path. Discovery
(`PROPFIND /` -> principal -> calendar-home-set -> calendar
listing) follows ratatoskr's RFC 6764 walk; calendar listing
emits displayname / ctag / Apple calendar-color / privilege-set /
supported-calendar-component-set. Event surface:
`PROPFIND` Depth=1 lists VEVENT resources; `GET <event>.ics`
returns the iCalendar body + ETag; `REPORT calendar-multiget`
batch-fetches by href list (404 propstat for unknown hrefs);
`REPORT calendar-query` honours `<C:time-range>` on VEVENT.
Mutating verbs: `PUT` parses the VEVENT body and either creates
or updates the matching fixture event under `Fixture::mutate`,
honouring `If-Match` (`*` requires existence; specific ETag
requires byte-equality; mismatches return 412). `DELETE` removes
the event with the same If-Match semantics. Mutations land on
the change_log so a subsequent Graph `calendarView/delta`
observes the CalDAV write through the same `event_*` id sets.
ETags and CTags derive deterministically from `Fixture::state`
plus the resource id. `MKCALENDAR` on
`/calendars/{user}/{cal}/` parses optional `displayname` /
`calendar-color` props from the request body, creates the
fixture `Calendar` bound to the principal's account, and records
a `calendar_created` transition; Graph `/v1.0/me/calendars`
(live read) and JMAP `Calendar/changes` (delta walk via
`calendar_delta_since_account`) both observe the write. Returns
405 on an existing calendar id and 404 under an unknown
principal. v0 explicitly does not implement PROPPATCH / ACLs /
scheduling.

Lua fixture loader: wired via [dellingr](https://crates.io/crates/dellingr),
a pure-Rust deterministic sandboxed Lua VM with cost-bounded
execution. The main entry point is `scenario::load(path)` which
dispatches by extension: `.lua` goes through `src/lua.rs` and
retains the dellingr `State` as a `Dispatcher` for reactive
callbacks; anything else parses as TOML and the dispatcher is
`None`. The legacy `fixture::load` keeps returning a bare `Fixture`
for tests that don't care about callbacks.

Static surface: `fixture`, `account`, `mailbox`, `email` builders
for hand-authored scenarios; `bulk_emails({ count, mailbox, seed,
start_at, interval_seconds, id_prefix })` for synthetic scale-test
fixtures; `bulk_threads({ count, mailbox, messages_per_thread, seed,
... })` for multi-message conversations with proper `In-Reply-To` /
`References` chaining; `bulk_mailboxes({ count, branching, seed,
id_prefix })` for breadth-first folder trees where mailbox `i` has
parent `(i-1)/branching` (deterministic, byte-stable across runs
at the same seed; templates in `src/templates.rs`).

Dynamic surface: scripts register callbacks via
`on(protocol, command, function)`; the protocol layer consults the
`Dispatcher` before generating its default response. The callback
receives a `req` table with `call_index` (1-based per (protocol,
command)) plus protocol-specific fields. Returning a table with
`{ status = "...", message = "..." }` overrides the response; `nil`
or no return = pass through.

Control helpers callable both at script load and inside callbacks:
`wait(ms)` blocks the current dispatch turn (`std::thread::sleep`,
safe under the dispatcher mutex - other connections queue briefly
on the lock but unrelated protocol handling continues on other
tokio workers); `mock_done()` and `mock_fail("reason")` record a
`MockExit` signal that `Dispatcher::wait_for_exit` resolves on,
which `main.rs` races against `wait_for_signal()` to drive a
clean (`Done` -> exit 0) or fault (`Fail` -> reason on stderr,
non-zero exit) shutdown. First call wins, so a chatty script
can't override an earlier `mock_fail` with a later `mock_done`.

Wired across all five protocols. Per-protocol override semantics
for `Override::Tagged { status, message }`:

- **IMAP** (`UID FETCH`): tagged response `<tag> <status> <message>`,
  no FETCH untagged emitted. Status is typically `NO`/`BAD`/`OK`.
- **JMAP** (any method): method-level error envelope -
  `("error", {"type": status, "description": message}, callId)`
  inside `methodResponses`.
- **Microsoft Graph** (`list_folders`, `get_folder`,
  `list_child_folders`, `list_messages`, `delta_messages`):
  HTTP 400 with `{"error": {"code": status, "message": message}}`.
- **Gmail** (`profile`, `list_labels`, `list_threads`, `get_thread`,
  `history`): HTTP 400 with the Gmail error envelope (status maps to
  `errors[0].reason`, message to `error.message`).
- **SMTP** (`MAIL`, `RCPT`, `DATA`, `AUTH`): wire response
  `<code> <message>\r\n` where `code` is parsed from the `status`
  field as a `u16` (e.g. `"452"` for rate-limited rejection,
  `"552"` for body-too-large, `"535"` for invalid credentials,
  `"454"` for temporary AUTH failure); non-numeric status falls
  back to `550`. The DATA body is not consumed when the override
  fires before `354`. The `AUTH` hook fires after the SASL
  response is read but before the connection binds an account;
  `req.payload` carries the mechanism name (`PLAIN`, `LOGIN`,
  `XOAUTH2`, `OAUTHBEARER`) so the script can fail selectively.
  The decoded SASL response is deliberately NOT passed to the
  callback - it carries credentials and the request log already
  redacts them.

Per-(protocol, command) `call_index` is a built-in `req` field;
protocol-specific extras (`uid_set`, `attrs`, `mailbox`, `folder`,
`thread_id`, `account_id`, `payload`) are populated where natural.
JMAP additionally surfaces `ids` as a 1-based Lua array of strings
when the call carries a string-typed `ids[]` (Mailbox/get,
Email/get); absent from `req` when the request omits the field.

`Dispatcher` is `Arc<Mutex<State>>`-shaped (dellingr 0.2 made
`State: Send`); the mutex covers brief synchronous Lua calls and is
never held across `.await`. Per-(protocol, command) call counts
are tracked alongside so `req.call_index` is strictly increasing.

Note: dellingr deliberately omits Lua's unparenthesized function-call
sugar, so builder calls in `.lua` fixtures are written
`mailbox({...})` not `mailbox{...}`. Also: `get_table_raw` called
via the public `State` API consumes the top key BEFORE checking
the table index, so passing a relative `i = -1` (which points at
the key, not the table) panics with an out-of-bounds on the
now-shifted stack. Capture the table's absolute index via
`state.get_top()` before pushing the key.

## Rules

### General rules

- Don't use gremlins! Em-dash, en-dash, strange quotes, whatever - they're all verboten.
- Don't remind the user of CLAUDE.md rules. They wrote them, so they know them.

### Memory rules

Do not use your Memory functionality. Do not read, write, or update memories. Do not suggest saving things to memory. Durable context belongs in CLAUDE.md or the relevant docs, not in per-session memory files - this project is developed across several hosts and users, and memory does not transfer between them; CLAUDE.md does.

### Bash rules

- Never use `sed`, `find`, `awk`, `head`, `tail`, or complex bash commands.
- Never chain commands with `&&`.
- Never chain commands with `;`.
- Never chain/pipe commands with `|`. Exception: piping into `review` is allowed (writing scratch prompt files is wasteful).
- Never capture stdout into env vars (`UUID=$(...)`).
- Never read or write from `/tmp`. All data lives in the project.
- Never run raw `cargo`, `curl`, `pkill`. Use `brokkr`.
- Never run `git` with `-C <path>`. Run `git` from the current working directory.

### git commit rules

- Never commit markdown changes alone. Bundle them with upcoming code commits.
- When committing other changes: always tag along markdown files if dirty.
- Write substantive engineering-focused commit messages.
- Has `Cargo.lock` changed? Commit it.
- Never `git push` unless the user explicitly asks. Stop after the commit.

## Commands

Use `brokkr` (not `cargo`) for check/test. It runs a gremlins scan (banned Unicode), then clippy, then tests - clippy denies warnings project-wide, so a clippy failure short-circuits before tests run. By default output is filtered to changed files and capped at 20 diagnostics per phase.

- `brokkr check` - gremlins + clippy + all tests (changed-files scope)
- `brokkr check --all` - show every diagnostic, no cap, no scope filter
- `brokkr check --fix-gremlins` - rewrite banned Unicode in tracked files (em/en dash -> `-`, smart quotes -> straight, NBSP -> space, zero-width/bidi deleted) before checking
- `brokkr check -p <crate>` - scope to one package (e.g. `-p rtsk`, `-p app`, `-p squeeze`)
- `brokkr check -- --test <file>` - forward args to `cargo test` (args after the second `--` go to the test binary)
- `brokkr test -p <crate> <NAME>` - release-mode focused single-test runner. Always passes `--release --include-ignored --nocapture --test-threads=1`. `<NAME>` is a case-sensitive substring filter (matches both unit and integration tests). Streams the test's own stdout/stderr live and prints a `[test] PASS/FAIL` footer with wall time. Defaults to `--all-features`; runs a second sweep if `[check].consumer_features` is set in `brokkr.toml`. Gated off for litehtml/sluggrs (use `brokkr visual` there).
  - `-p, --package <PKG>` - cargo package. Required in this workspace - no default package, and overrides `[test] default_package` in `brokkr.toml` if set.
  - `-N, --repeat <N>` - run the test N times per sweep (flaky-test hunting).
  - `-j, --jobs <N>` - parallel cargo compile jobs.
  - `--raw` - bypass output filtering, print everything cargo emits.
  - `--debug` - build and run the test in dev profile instead of release. Use this for subprocess-lifecycle / IPC / boot-path tests where release-LTO compile time (3-4 min for the full workspace) dominates wall time and the optimization level doesn't change the behavior under test. `BROKKR_TEST_BIN_DIR` points at `<target>/debug` accordingly.
  - Example: `brokkr test -p common truncates_without_splitting` or `brokkr test -p calendar extract_tag_value_flattens_nested_text -N 5` or `brokkr test -p app terminal_failure_at_initial_boot_does_not_respawn --debug`.
- `cargo run -p app` - run the iced app (requires a seeded DB, see `crates/app/seed-db.py`)

**Always run `brokkr check` in the foreground with a 4-minute (240000ms) timeout.** A healthy `brokkr check` finishes well under that. If it does not, something is wrong - kill it and investigate (most often: a test hangs because a background task wasn't drained on shutdown). Do not raise the timeout to "wait it out", and do not run `brokkr check` in the background.

## Multi-Agent Orchestration

**Do NOT use worktree isolation for parallel agents.** Worktrees create merge conflicts that silently drop agent work. Instead, launch agents in the same tree with strict file ownership - zero overlap.

**Why no worktrees:** Worktrees let agents work on diverged snapshots. When merging back, `git checkout --ours/--theirs` drops code, conflict markers get missed, and features end up "existing but not wired" - types/functions created but never connected to message dispatch, views, or call sites. This happened repeatedly in a 114-commit session and was only caught by a rigorous 3-pass audit.

**Agent coordination rules:**
- Each agent gets exclusive ownership of specific files. No two agents touch the same file.
- `main.rs` is shared - agents may ONLY add Message enum variants and one-line dispatch arms. All handler logic goes in `handlers/*.rs`.
- Agents must read their handler file FIRST (it already has extracted methods). Do not replace existing code with placeholders.
- Agents must NOT run `cargo check/build/test`. The orchestrator validates between agents.

**Verification standard - "implemented" means wired:**
- A feature is NOT implemented unless the user can reach it through current Message dispatch -> handler -> view wiring.
- Types that exist but are never constructed, methods that exist but are never called, message variants with no dispatch arm - these are dead code, not implementations.
- After agents complete, verify wiring by checking: (1) Message variant exists, (2) dispatch arm in update() calls handler, (3) handler performs the work, (4) view renders the result or side effect is observable.

**Audit protocol:**
- Do not trust agent claims of completion. Verify existence + wiring + behavior.
- Use the 3-pass audit structure: domain-specific verification -> cross-cutting reconciliation -> editorial normalization.
- Discrepancies docs should contain only current gaps, not historical records. Remove resolved items entirely.

## Code Review (`review`)

`review` is a CLI tool that fans out code review requests to anchored AI sessions. Each archetype has a stored prime prompt (in `.review.toml` under `[_prime].<archetype>`) that defines the review lens. Configuration lives in `.review.toml` at the repo root.

Four archetypes: `security`, `bugs`, `perf`, `arch`. The `sweep` group fans out to all four in parallel.

**Multiple archetypes go in one invocation as a comma list** (e.g. `review bugs,arch,perf --oneshot`), not as separate parallel `review` calls. The CLI staggers requests internally to stay under upstream HTTP rate limits; firing several `review` processes at once defeats that and trips the limiter.

**Default to `--oneshot`.** Anthropic's prompt cache is ~1 hour, so starting a fresh session for each unrelated review is cheaper than resuming a long-lived one. `--oneshot` starts a fresh session, prepends the stored prime prompt, runs the query, and prints the session ID to stdout.

**Follow-ups:** within the cache window, resume the same session with `--session <id>` (using the ID printed by the previous `--oneshot`). The cache stays warm; only the new query and reply are billed. `--session` requires `--provider` and is mutually exclusive with `--oneshot`.

```bash
echo "review the new sync code" | review bugs --oneshot
# session: 019de...
# <findings>

echo "follow up on the second finding" | review bugs --provider claude --session 019de...
# <answer>
```

Don't reach for a second `--oneshot` to follow up - that creates a different fresh session with no memory of the first. Use `--session` for continuity within a thread, `--oneshot` for new threads.

To update the prime prompt for an archetype, pipe new content to `review prime <archetype> --provider <p>`. The prompt is stored once per archetype and shared across providers; once stored, prime any other provider with no stdin to reuse it.
