# TODO

Running task list, ordered by what ratatoskr is actively waiting on.
Per-protocol design notes live alongside in `notes/`; this file just
tracks what's next. Landed work is described in `CLAUDE.md` "Status".

## From the 2026-05-10 multi-agent review (today's slice)

Findings from a four-agent (bugs / security / perf / arch) sweep of
the four commits in today's slice (`2aff7e6..bb2ecef`: JMAP
calendar, cross-protocol raw_bytes + slow-paging, People API,
Google Calendar). Only items that need work end up here.

### Fix now

- **[bugs] `event_delta_since` doesn't filter `created` / `updated`
  by parent calendar; JMAP `CalendarEvent/changes` over-reports.**
  `src/fixture.rs:549-551` extends only `destroyed` through the
  parent filter; `created` / `updated` walks every transition
  unconditionally. Combined with the union-across-calendars walk
  in `src/jmap_calendar.rs:279-304`, an event created in cal A
  shows up in cal B's per-calendar walk too. The cross-cal
  `seen.insert` dedupe hides single-event cases but loses
  per-calendar dominance: an event created in A and destroyed in
  B in the same window survives as `created`. No JMAP test
  covers multi-calendar deltas. Fix is to thread the parent
  filter through `created` / `updated` walks too, then drop the
  cross-cal union and call once with no filter (or extend
  `delta_since_filtered_destroys` to a `delta_since_filtered_all`
  that filters every set).
- **[bugs] People `is_known_state` accepts mid-history tokens and
  silently drops tombstones.** `src/people/contacts.rs:186-196`
  treats any retained `from_state` / `to_state` / seed as known.
  The handler then has only two branches: `token == fixture.state`
  → empty; else → full *live* list with no `metadata.deleted:
  true` tombstones. Notes/ratatoskr-people-surface.md explicitly
  requires unknown-token → 410 to drive ratatoskr's recovery, but
  any retained intermediate state slips past as "known" and
  ratatoskr never sees the tombstone for a destroyed contact.
  Two-line fix: tighten `is_known_state` to current-only
  (everything else 410), or actually walk the change_log and
  emit deleted-Person entries per `contact_destroyed` /
  `contact_destroyed_parents`.
- **[bugs] gcal events list has the same stale-token tombstone
  gap.** `src/gcal/events.rs:138-194`: when `syncToken !=
  fixture.state` but is_known, falls through to a full live-events
  listing. `event_destroyed` change-log entries never surface as
  the `status: "cancelled"` tombstones notes/ratatoskr-gcal-
  surface.md documents and ratatoskr's `:189-200` cancelled-
  routing branch reads. Same fix shape as the People item above.
- **[bugs] JMAP `CalendarEvent/set` and gcal create synthesize
  ids from `events.len() + 1`, colliding after a destroy.**
  `src/jmap_calendar.rs:485` and `src/gcal/events.rs:451`. Fixture
  declares `mock-event-1`/`mock-event-2`, JMAP destroys 1, JMAP
  creates → `mock-event-2`, collides with the still-live event.
  Determinism + UID-stability are project invariants; an in-test
  Lua scenario doing destroy-then-create reaches this trivially.
  Replace with a monotonic counter on `Fixture` (parallel to the
  IMAP UID history) that only ever increments.

### Fix soon

- **[bugs] JMAP `apply_event_patch` flips `is_all_day` without
  recomputing start/end.** `src/jmap_calendar.rs:514-516, 553-581`.
  A patch that sets `showWithoutTime = true` alone (no start/
  duration) leaves the event's all-day flag flipped but its
  start/end timed; subsequent serialization emits an inconsistent
  shape. Recompute when `is_all_day` changes too, not just when
  start/duration are present in the patch.
- **[bugs] JMAP `Calendar/changes` returns `cannotCalculateChanges`
  on any non-current state, even seed.** `src/jmap_calendar.rs:117-126`.
  Calendars are static in v0; an event-only mutation bumps
  `fixture.state` but doesn't touch the calendar resource type.
  RFC 8620 wants empty deltas for an unchanged resource type.
  Fix: short-circuit on seed-or-known too, return empty.
- **[bugs] gcal `apply_event_patch` ignores `organizer`.**
  `src/gcal/events.rs:467-495`. JMAP's apply_event_patch parses
  organizer; Graph's deliberately doesn't (Graph clients can't
  repoint). Real Google clients can. Add the parse, document
  Graph's omission inline.
- **[arch] Google-family `error()` argument order is `(message,
  reason)` while Graph's is `(code, message)`.** Drift is real
  but underlying envelope shapes genuinely differ - the names
  should track. Rename the param bindings in
  `src/{gmail,gcal,people}/mod.rs::error` to `(message, reason)`
  explicitly; add a one-line doc in each calling out which is
  which.
- **[arch] `gcal::AppState` and `people::AppState` lost the
  `with_request_log` / `with_dispatcher` builders that
  `gmail::AppState` and `routes::AppState` expose.** Tests
  reach into `shared.dispatcher` directly. Either add the
  builders to gcal/people for parity, or remove from
  gmail/graph if nothing in the tree uses them. Pick one shape.
- **[perf] People `projected_connections` materialises every
  contact's JSON before paging.** `src/people/contacts.rs:114,
  213-217`: collects refs, sorts, maps to `Vec<Value>`, then
  `drain(offset..end)`. Pages are O(N) per request even when the
  caller wants page_size = 100. Trivial fix: serialize after
  slicing (`.iter().skip(offset).take(page_size).map(serialize_
  person)`).
- **[security] Request log records full parsed mutation bodies
  unconditionally.** `src/gcal/events.rs:288-292, 334-338,
  378-382` insert `{"body": parsed}` (up to 1 MiB JSON) on every
  POST/PATCH/DELETE. With many calls + the 100k cap, request_log
  memory grows large between `DELETE /test/requests` calls.
  Loopback only, but other listeners deliberately keep the
  body slice small. Either gate behind a knob or truncate.

### Eventually (only when something forces it)

- **[perf] `CalendarEvent/changes` re-walks the change_log per
  calendar.** `src/jmap_calendar.rs:279-304`. O(C*T) where
  C = calendar count and T ≤ 256. Fixtures have ≤ ~10 calendars
  today; ~2,560 string compares per request is fine. Worth a
  `delta_since_any` helper if a fixture grows >50 calendars.
- **[perf] `Cow<str>` raw_bytes path always clones.**
  `src/jmap.rs:917-920`. The Owned arm calls `.to_string()` on
  an already-borrowed `&str`. `Cow::Borrowed(raw)` would work.
  Cleanup, not a hot path.
- **[arch] Three near-identical `mod.rs` files for the
  Google-family listeners.** `src/{gmail,gcal,people}/mod.rs`
  duplicate ~70 lines each (bearer middleware, `log_request`,
  `serve`, Google-shape `error`, `not_implemented`, `ok_json`).
  Threshold of three is met. Extract a
  `crate::http_listener::serve_with_shutdown` +
  `bearer_middleware<F>` + `google_error` once a fourth
  Google-shape listener appears (Drive resumable uploads is the
  next candidate).
- **[arch] `main.rs` listener wiring is 8x repetitive.**
  `src/main.rs:34-289`. Each new listener edits bind + sentinel
  entry + spawn + drain. Past the tipping point. Build a
  `Listener { name, port, factory }` table once Drive / EWS land.
- **[arch] `src/jmap_calendar.rs` is a flat 800-line file.**
  Defensible at v0 (single resource family); split into
  `src/jmap/calendar/{mod,jscalendar,set,changes}.rs` if it grows
  another resource (e.g. `Calendar/set`, `ParticipantIdentity`).
- **[arch] `_arc_keepalive` dead helpers.** `src/gcal/mod.rs:167`,
  `src/people/mod.rs:168`. Copied from a gmail import-warning
  workaround that no longer applies. Delete.
- **[arch] JSCalendar participant id scheme is positional
  (`org`, `att1`, `att2`, ...).** `src/jmap_calendar.rs:225`.
  Removing one attendee from a fixture renumbers everyone after
  it. JSCalendar permits arbitrary participant ids; key on a
  stable hash of email instead. Same shape as the `loc1`
  hardcode for locations (latent because we only ever emit one).
- **[bugs] `parse_iso8601_duration` silently drops year and
  month-`M` components.** `src/jmap_calendar.rs:691-734`. `'Y'`
  and pre-`T` `'M'` fall into the catchall arm; `buf` keeps
  growing across the next valid suffix. `"P1Y2M3D"` parses to
  123 days. v0 fixtures don't author year/month durations, so
  the failure is silent miscalculation rather than rejection.
  Reject unknown suffixes once a fixture wants them.
- **[bugs] gcal POST/PATCH/DELETE accept mutations regardless of
  `accessRole`.** `src/gcal/events.rs:269-405`. Hidden today
  because `serialize_calendar` hardcodes `accessRole: "owner"`
  on every calendar. Once the fixture format grows a per-calendar
  role, mutations on `reader` / `freeBusyReader` calendars will
  silently succeed. Pair the unhide-fix with the schema change.
- **[bugs] People `serialize_person` hardcodes
  `metadata.deleted: false`.** `src/people/contacts.rs:232`.
  Becomes wrong the moment any tombstone path lands; harmless
  until then.
- **[security] People `_person_fields` / `_read_mask` silently
  ignored.** `src/people/contacts.rs:55-60`. Real Google enforces
  field-mask; the mock always emits the full Person shape. Mock
  is more permissive than reality, which can hide a
  client-side bug where an over-broad request passes mock CI but
  fails in prod. Document in the People surface notes; reject
  unknown fields if a fixture cares.

### Test coverage gaps to close alongside the fixes above

- No JMAP test for cross-calendar `CalendarEvent/changes` (hides
  the `event_delta_since` parent-filter bug above).
- No People test for stale-but-retained `syncToken` (hides the
  tombstone gap).
- No gcal test for `syncToken`-driven cancellation tombstones
  after a delete (same shape as the People gap).
- No JMAP / gcal test asserting id uniqueness across destroy →
  create (hides the `mock-event-{len+1}` collision).

## From the 2026-05-10 multi-agent review (earlier slice)

Findings from a four-agent sweep of `8f7798c..7602fdb` (RwLock +
change_log, JMAP `Email/set` + `Mailbox/set`, IMAP `UID STORE` /
`COPY` / `EXPUNGE`, Graph calendar mutations + delta, Graph
contacts + delta, change-script pipeline + `/test/fixture/{step,
reset}`, latency knob, stable request log, OAuth-enforced fixture,
CalDAV listener, TOML `[[change]]` projection, `body_raw_bytes`
escape hatch). Fix-now and Fix-soon items have all landed; only
Eventually items remain.

### Eventually (only when something forces it)

- **[security] CalDAV iCal / XML parsers are O(n·m) on
  adversarial input.** `caldav/xml.rs::find_tag_open` /
  `find_tag_close` advance one byte per `<` and re-scan the
  suffix. Not exploitable for ReDoS (no backtracking) but a
  single PROPFIND can pin one core for tens of ms on a 2MB
  body. Acceptable for a loopback test mock.
- **[perf] `RequestLog` retains rich `serde_json::Value` per
  entry with cap = 100_000.** Multi-hundred-MB steady-state
  heap even when no test reads it. Drop cap to 10_000 or let
  fixtures override.
- **[perf] `LatencyKnob::sleep_for` is `async` even on the
  empty-knob path.** Saves the `tokio::time::sleep` call but
  not the await-state-machine churn. Marginal.
- **[perf] CalDAV `xml::escape` and `ical::escape_text` build
  a fresh `String` char-by-char.** Fine for fixture-sized
  bodies; swap to a byte-find fast path that pushes
  unescaped runs in slices once 10MB CalDAV bodies show up.
- **[perf] `caldav::dispatch` clones the path string at
  entry.** `src/caldav/mod.rs:106`. One allocation per
  request; trivial.
- **[perf] `caldav::wrap_responses` builds parallel
  `event_hrefs` / `event_props` / `entries` Vecs**
  (`src/caldav/mod.rs:331-344`) where one streaming push into
  the output buffer would suffice.
- **[perf] `imap::mailbox_counts` builds a `Vec<&Email>`** to
  count exists / unseen; could be two iterator counts.
- **[arch] `LatencyKnob::lookup` is `pub` and used only by
  internal callers.** Could be `pub(crate)`.
- **[arch] `shared::SharedHandles::for_test` builds its own
  `baseline` from a read of the live fixture.** Tests that
  hit `/test/fixture/reset` would rewind to whatever the
  fixture happened to look like at handle construction, not
  the post-load image. No test trips this today; flag for
  when one might.
- **[arch] `body_raw_bytes` doc lives in three places**
  (`notes/fixture-format.md`, the `Email::raw_bytes` field
  doc, the IMAP render doc). Consolidate onto the field
  doc-comment when next touched.

## From the 2026-05-09 multi-agent review

Findings from `de89827..3b87085`. Walk-backs, verified-correct
invariants, and accepted trade-offs are recorded as inline
comments at the relevant code sites; only items that need work
end up here.

### Eventually (only when something forces it)

- **[security] WWW-Authenticate header is bare `"Bearer"`.**
  Documented in `src/oauth.rs::unauthorized`. Lift to the full
  RFC 6750 form when an interop test or pedantic OAuth client
  points at the mock.
- **[security] FNV-1a body fingerprint in OAuth tokens is
  reversible.** Documented in `src/oauth.rs::fnv1a64`.
  Acceptable for a loopback-bound mock; revisit if the listener
  ever binds non-loopback.
- **[security] All `/test/*` routes are unauthenticated.**
  Documented in `src/routes.rs::router`. Acceptable while
  `main.rs` only binds 127.0.0.1; gate behind `--enable-admin`
  or a Unix socket if a non-loopback bind ever lands.
- **[security] All `Mutex::lock().expect("...poisoned")`
  panics.** Documented in `src/request_log.rs`. Acceptable
  today (no panic-prone code under any of the locks); revisit
  if a panic-under-the-lock path appears.
- **[arch] Bespoke `urldecode` in `src/oauth.rs` could be
  `form_urlencoded::parse`.** Documented in
  `src/oauth.rs::parse_token_body`. Replace if the OAuth module
  grows beyond its current scope.
- **[bugs] Refresh tokens reuse the `mock-access-` prefix.**
  Documented in `src/oauth.rs::TokenStore::mint`. Take a
  `prefix: &str` argument if any code ever filters by prefix.
- **[perf] IMAP per-line `json!` allocation + mutex acquire on
  every dispatched line.** Documented in
  `src/imap.rs::dispatch`. Folds into the RequestLog cap fix;
  not a standalone item.
- **[perf] `log_request` middleware records 404s through
  `not_implemented`.** `src/graph/mod.rs::log_request`,
  `src/gmail/mod.rs::log_request`. Folds into the RequestLog
  cap fix.

## Fixture format growth

Remaining items are unblocked-but-unneeded.

- Future growth on the change-script surface: attachments inside
  `email_create` ops (currently rejected at load), and bumping
  IMAP UIDVALIDITY / HIGHESTMODSEQ from a step (today's IMAP
  state derives mechanically from `Fixture::state`, so a step's
  state advance already moves HIGHESTMODSEQ; bumping UIDVALIDITY
  would need a fixture-side knob).
- `body_html` parallel to `body_text`. Reserved in
  `notes/fixture-format.md`; not implemented. Pressing once IMAP
  needs to render an HTML wire body or Graph wants HTML rendering.
- Multipart MIME via `body_path` for fixtures that want to round-trip
  a real `.eml` rather than authoring per-protocol projections from
  the canonical fields. Attachments are already wired via
  `[[email.attachment]]`; this item is specifically about replacing
  the canonical body+attachments shape with a parsed `.eml`.
- Multi-account. v0 enforces `is_personal = true` and exactly one
  account. Lifting requires per-protocol tweaks to surface multiple
  accounts (JMAP session resource, Graph `/users/{id}/...` paths,
  IMAP per-connection account context).
- Larger named fixtures beyond `fixtures/jmap-bulk.lua`. The
  `bulk_emails` / `bulk_threads` / `bulk_mailboxes` builders make
  medium (~1k), huge-thread, and many-folders fixtures one-liners.
  Author them as M9 sync benchmarks need them.
- People API `[other_contact]` table for adversarial coverage of
  `/v1/otherContacts`. Currently the route always returns an
  empty list. Wire a parallel table when a fixture needs it.
- Per-calendar `accessRole` so gcal can refuse mutations on
  read-only calendars (today every calendar serializes as
  `owner`).

## IMAP (lower-priority follow-ups)

- 200-message FETCH batching boundary. The current handler emits
  all matched FETCH responses in one go. Ratatoskr's client
  batches client-side (`CHUNK_SIZE = 200`), so the wire boundary
  is invisible to it; flagged as a v1 thing if we ever want to
  test the batch boundary explicitly.

## Microsoft Graph (other future work)

v0 mail-sync, calendar, and contacts are complete. Remaining future
Graph work, in roughly the order the next fixture is likely to
need it:

- Master category list (`label_sync.rs`).
- Group enumeration (`group_sync.rs`).
- OneDrive resumable upload sessions (`onedrive.rs`) - needed once
  the SMTP / Graph submit paths grow attachments.
- Public-folder sync via EWS (`ews/`, `public_folder_sync.rs`).
  Different protocol (SOAP), separate `src/ews.rs` module.
- Shared mailbox sync via `/users/{id}/...` paths
  (`shared_mailbox_sync.rs`). Needs multi-account fixtures.
- Webhooks / change notifications (`webhooks.rs`).
- Autodiscover (`autodiscover.rs`).

## Gmail (future work)

v0 mail-sync surface is complete. Future Gmail work:

- Google Drive resumable uploads
  (`<ratatoskr>/crates/gmail/src/gdrive.rs`). Needed once the
  submission paths grow attachments large enough to spill out of
  inline.
- SendAs / signatures bidirectional sync. v0 emits an empty
  `sendAs[]`; once a fixture grows `[account.signature]` we honour
  it both ways.

## CalDAV (future work)

v0 surface is complete (see `CLAUDE.md` "Status"). Out of scope
until a fixture forces it: MKCALENDAR, PROPPATCH, ACLs, delegation,
free-busy, scheduling (iTIP / iMIP), VEVENT recurrence (RRULE /
EXDATE), VALARM, attachments, per-event VTIMEZONE.

## Lua dynamic surface

Phase 2 callbacks (`on(protocol, command, fn)`) are wired across all
five protocols, mapped via `Override::Tagged { status, message }`.
What's left on the Lua side:

- SMTP `cmd_auth` callback hook. Skipped from the initial fanout
  because AUTH-time fault injection isn't a common scenario; the
  helper exists, adding the hook is one line if a fixture wants
  it.
- Gmail `get_attachment` and `send_as` callback hooks. Skipped
  because both are stubs. Wire when a fixture needs them.
- Per-protocol callbacks for the new gcal and People listeners
  (`list_events`, `calendar_list`, `list_connections`,
  `list_other_contacts` already accept overrides; mutating verbs
  on gcal don't yet).

## Cross-project follow-ups (ratatoskr-side)

- `RATATOSKR_TEST_PEOPLE_ENDPOINT` and `RATATOSKR_TEST_GCAL_ENDPOINT`
  overrides parallel to `RATATOSKR_TEST_GMAIL_ENDPOINT`. Today
  ratatoskr's `PEOPLE_API_BASE` and `GOOGLE_CALENDAR_API_BASE` are
  hardcoded consts (`crates/gmail/src/contacts/mod.rs:112`,
  `crates/calendar/src/lib.rs:12`). Sæhrimnir's listeners are in
  place; ratatoskr just needs the env-driven base URL plumbing.

## Cosmetic and housekeeping

- A `brokkr.toml` with `project = "saehrimnir"` plus a `[[check]]`
  sweep needs `Project::Saehrimnir` to land in brokkr's enum first,
  or the file would fail brokkr's parse-time validation. Until then,
  rely on `brokkr check`'s no-toml fallback.
- Plan-3 / ratatoskr wiring. From saehrimnir's side this just needs
  jmap-client + ratatoskr's IMAP/Graph/Gmail/SMTP clients to talk
  to us cleanly. Behaviours worth re-verifying when plan-3 lights
  up: whether jmap-client follows a relative `apiUrl`, whether
  ratatoskr's IMAP client tolerates our exact greeting/CAPABILITY
  ordering, whether Gmail's q-parser mismatch (we only honour
  `after:YYYY/M/D`) trips any internal-sync code path.
