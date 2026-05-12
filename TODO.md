# TODO

Running task list, ordered by what ratatoskr is actively waiting on.
Per-protocol design notes live alongside in `notes/`; this file just
tracks what's next. Landed work is described in `CLAUDE.md` "Status".

## Priority (no particular order)

Concrete next-up items lifted above the per-protocol backlogs.

- **Lua Gmail attachment + sendAs hooks.** Wire `on("gmail",
  "get_attachment", fn)` through the dispatcher so fault-injection
  works against the attachment route the same way it does for
  `list_threads` etc. The sendAs handlers (`list_send_as` /
  `get_send_as` / `patch_send_as`) already consult `maybe_override`
  on `"send_as"`; remaining work is the attachment route in
  `src/gmail/mail.rs`.

## From the 2026-05-10 multi-agent review (today's slice)

Findings from a four-agent (bugs / security / perf / arch) sweep of
the four commits in today's slice (`2aff7e6..bb2ecef`: JMAP
calendar, cross-protocol raw_bytes + slow-paging, People API,
Google Calendar). Only items that need work end up here.

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

## From the 2026-05-10 multi-agent review (earlier slice)

Findings from a four-agent sweep of `8f7798c..7602fdb` (RwLock +
change_log, JMAP `Email/set` + `Mailbox/set`, IMAP `UID STORE` /
`COPY` / `EXPUNGE`, Graph calendar mutations + delta, Graph
contacts + delta, change-script pipeline + `/test/fixture/{step,
reset}`, latency knob, stable request log, OAuth-enforced fixture,
CalDAV listener, TOML `[[change]]` projection, `body_raw_bytes`
escape hatch).

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

v0 mail, calendar, contacts, categories, and `/v1.0/users/{id}/...`
per-account routing across all four resource families are complete.
Group enumeration is tracked under "Priority" above. Remaining
future Graph work:

- OneDrive resumable upload sessions (`onedrive.rs`) - needed once
  the SMTP / Graph submit paths grow attachments.
- Public-folder sync via EWS (`ews/`, `public_folder_sync.rs`).
  Different protocol (SOAP), separate `src/ews.rs` module.
- Webhooks / change notifications (`webhooks.rs`).
- Autodiscover (`autodiscover.rs`).

## Gmail (future work)

v0 mail-sync surface is complete. SendAs / signatures
bidirectional sync (list + per-address GET + PATCH on
`/gmail/v1/users/me/settings/sendAs`) is landed with the
`[[send_as]]` fixture table. Remaining future Gmail work:

- Google Drive resumable uploads
  (`<ratatoskr>/crates/gmail/src/gdrive.rs`). Needed once the
  submission paths grow attachments large enough to spill out of
  inline.

## CalDAV (future work)

v0 surface is complete (see `CLAUDE.md` "Status"). Recurrence
read + write paths are landed across all four calendar protocols,
and `MKCALENDAR` creates new calendar collections (records a
`calendar_created` transition observable by Graph `/me/calendars`
and JMAP `Calendar/changes`). Out of scope until a fixture forces
it: PROPPATCH, ACLs, delegation, free-busy, scheduling (iTIP /
iMIP), VALARM, attachments, per-event VTIMEZONE.

## Lua dynamic surface

Phase 2 callbacks (`on(protocol, command, fn)`) are wired across all
five protocols, mapped via `Override::Tagged { status, message }`.
The Gmail `send_as` hook is wired alongside the SendAs handlers
(`list_send_as` / `get_send_as` / `patch_send_as` consult
`maybe_override` with command `"send_as"`). The Gmail
`get_attachment` hook is tracked under "Priority" above. What's
left:

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
