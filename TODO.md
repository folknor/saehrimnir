# TODO

Running task list, ordered by what ratatoskr is actively waiting on.
Per-protocol design notes live alongside in `notes/`; this file just
tracks what's next. Landed work is described in `CLAUDE.md` "Status".

## From the 2026-05-10 multi-agent review

Findings from a four-agent (security / bugs / perf / arch) sweep of
the work landed in commits `8f7798c..7602fdb` (RwLock + change_log,
JMAP `Email/set` + `Mailbox/set`, IMAP `UID STORE`/`COPY`/`EXPUNGE`,
Graph calendar mutations + delta, Graph contacts + delta, change-
script pipeline + `/test/fixture/step` + `/test/fixture/reset`,
latency knob, stable request log, OAuth-enforced fixture, CalDAV
listener + module, TOML `[[change]]` projection, `body_raw_bytes`
escape hatch). Only items that need work end up here; verified-
correct invariants and accepted trade-offs are omitted.

### Fix now

- ~~**[bugs] `apply_change_step` rewind drops `contacts` /
  `contact_folders`.**~~ Landed. `src/routes.rs::step_fixture`
  now snapshots and restores both, alongside emails / mailboxes /
  events. Regression test
  `tests/step.rs::fixture_step_rewind_covers_contacts_and_contact_folders`.
  Future-proofing (clone-then-swap restructure) tracked under
  the closure-only-mutator architectural item.
- ~~**[bugs] Graph `calendarView/delta` and `contacts/delta` emit
  cross-collection tombstones.**~~ Landed. `Transition` /
  `MutationDiff` gained `event_destroyed_parents` and
  `contact_destroyed_parents` parallel vectors capturing the
  calendar / folder each destroyed resource lived in. Producers
  (CalDAV DELETE, Graph calendar DELETE, change-script
  EventDestroy / ContactDestroy) snapshot the parent before the
  retain. `event_delta_since` / `contact_delta_since` now take a
  `parent_id` arg and pre-filter tombstones. Regression test
  `tests/graph.rs::graph_calendar_view_delta_does_not_leak_tombstones_across_calendars`.
- ~~**[bugs] IMAP `BODY[HEADER]` for raw-bytes emails includes
  one extra `\r\n`.**~~ Landed. `src/imap.rs::split_raw` now
  returns the header slice ending at `i + 2` (matching what the
  structured `render_rfc822_headers` emits). Test in
  `tests/imap.rs::body_raw_bytes_emits_verbatim_through_imap_fetch`
  updated.
- ~~**[security] CalDAV listener never enforces
  `fixture.oauth.enforce`.**~~ Landed.
  `src/caldav/mod.rs::enforce_bearer_middleware` mirrors the Graph
  pattern, returning a bare `401 + WWW-Authenticate: Bearer` on
  rejection (CalDAV has no shared body schema). Regression test
  `tests/caldav.rs::caldav_enforces_bearer_when_oauth_enforce_is_true`
  walks every CalDAV verb. Docs in
  `notes/ratatoskr-oauth-surface.md` and
  `notes/fixture-format.md` extended to mention CalDAV.
- ~~**[security] CalDAV iCal `ORGANIZER` / `ATTENDEE` email
  addresses are emitted verbatim.**~~ Landed.
  `src/caldav/ical.rs::sanitize_address` strips control bytes
  and CR/LF before emit; `write_address_line` routes through
  it. Real RFC-5321 addresses are unaffected. Test in
  `src/caldav/ical.rs::tests`.
- ~~**[security] `/test/latency` accepts unbounded `u64` ms.**~~
  Landed. `src/routes.rs::set_latency` clamps both `global_ms`
  and per-protocol values at 60_000ms (`LATENCY_MAX_MS`),
  returning 400 above that. Test
  `tests/api.rs::test_latency_rejects_values_above_cap`.
- ~~**[bugs] CalDAV ETag / CTag derive from global `fixture.state`,
  not per-resource.**~~ Landed. `event_etag` walks the change_log
  to find the last transition that listed `event_id` in its
  created / updated / destroyed sets; `calendar_ctag` walks for
  the last transition that touched any event in the named
  calendar (using `event_destroyed_parents` for tombstones, the
  live event's `calendar_id` for created / updated). Both fall
  back to the change-log seed for resources no transition has
  touched. New `Fixture::change_log_transitions` /
  `Fixture::change_log_seed` accessors. Regression test
  `tests/caldav.rs::caldav_etag_and_ctag_are_per_resource_not_fixture_wide`
  asserts that an unrelated PUT in cal-work doesn't bump
  cal-personal's CTag or ev-001's ETag.
- ~~**[arch] Two parallel `ChangeOp` producers with drift-prone
  patch construction.**~~ Landed. Per-op builder helpers
  (`email_update_op`, `email_move_op`, `mailbox_create_op`,
  `mailbox_update_op`, `event_create_op`, `event_update_op`,
  `contact_folder_create_op`, `contact_folder_update_op`,
  `contact_create_op`, `contact_update_op`) live in
  `src/fixture.rs` next to `normalize_change_step`. Both the
  TOML loader and the Lua per-op readers call them, so the
  patch JSON shape (camelCase JMAP keys, snake_case for
  change-script-only resources, `bool_map` for keywords /
  mailboxIds, etc.) lives in exactly one place. Lua got a new
  `read_string_array_opt_present` helper that distinguishes
  Nil from empty Table (needed by the shared helper's
  Option<T> contract). Future field additions touch one site
  per resource family.
- ~~**[arch] `step_fixture` calls `mutate(|_f| diff)` after
  already mutating the fixture in `apply_change_step`,
  violating the closure-only-mutator contract.**~~ Landed.
  `Fixture` gained `pub fn record_transition(&mut self, diff)`
  exposing the bump-state + append-transition path explicitly;
  `mutate` is now a thin closure-wrapper that delegates to it.
  `step_fixture` calls `record_transition` directly so the
  read-the-doc-literal is preserved (mutate's closure is the
  only mutation site for closure-style callers; the change-
  script path's prior in-place mutation is now the documented
  reason `record_transition` exists).
- ~~**[bugs] Lua `email_create` baseline-mailbox snapshot is taken
  at the wrong moment.**~~ Documented. The early sanity check at
  `src/lua.rs::read_email_create` is duplicated by the
  authoritative apply-time check in `apply_change_step`, so the
  fix is to clarify the contract rather than restructure: place
  every `mailbox(...)` declaration before any `change(...)` call
  in Lua scripts, and apply-time validation catches anything the
  early check would have missed. Inline comment + the
  `notes/fixture-format.md` `email_create` op contract updated.

### Fix soon

- ~~**[security] CalDAV PUT can change a stored event's id behind
  the URL.**~~ Landed. `handle_put` now rejects with 400 when
  the body's `UID` is present and disagrees with the URL's
  event id; absent UID continues to fall back to the URL id.
- ~~**[security] CalDAV `If-Match` quote / weak-validator
  handling.**~~ Landed. New `if_match_matches` helper +
  `IfMatchOutcome` enum tolerates comma-separated lists,
  `W/`-prefixed weak validators, and quoted wildcards (`"*"`).
  Both PUT and DELETE route through it. Tests exercise quoted
  wildcard and `W/<etag>` against an existing resource.
- ~~**[security] CalDAV `mailto:` strip is case-sensitive.**~~
  Landed. `parse_address` calls a new `strip_mailto_prefix`
  helper that does a case-insensitive 7-byte prefix check.
  Test covers `mailto:` / `MAILTO:` / `MailTo:`.
- ~~**[security] CalDAV multi-VEVENT body silently drops the
  second event.**~~ Landed. `parse_vevent` returns
  `Result<ParsedEvent, &'static str>`; a body with more than
  one VEVENT is rejected ("multiple VEVENTs"), an empty body
  is rejected ("must contain a VEVENT"). PUT path 400s on
  either error.
- ~~**[bugs] Cross-folder contact moves disappear from source-
  folder delta.**~~ Landed.
  `src/routes.rs::apply_contact_patch` rejects any `folder_id`
  update where the new value differs from the current
  `folder_id`. Real Microsoft Graph doesn't expose `folder_id`
  as a writable property on contacts either; clients move via
  destroy + create, which surface in both source and
  destination folders' deltas through the existing
  `contact_destroyed` / `contact_created` machinery. Same
  shape will apply to event `calendar_id` moves once event
  patches grow that field.
- ~~**[bugs] `step_fixture` and `reset_fixture` acquire locks in
  opposite orders.**~~ Landed. Both now follow cursor → fixture;
  the convention is documented inline in `reset_fixture`.
  Reset's prior pattern used scoped blocks so the inversion was
  actually unreachable, but unifying the order future-proofs
  against any change that holds both simultaneously.
- ~~**[security] `RequestLog::snapshot` race window.**~~ Landed.
  Replaced the steal-clone-restore dance with a single
  hold-the-lock-and-clone. Avoids the eviction race entirely;
  realistic stall is sub-millisecond against the 100k cap.
  Folds the related `[perf] RequestLog::snapshot deep-clones
  twice` follow-up - now it's exactly one clone of the live
  deque per call. The handler-side `?stable=true` rebuild of
  `Value` per row remains as a separate item below.
- ~~**[bugs] CalDAV `xml::body_requests_prop` is a substring
  match.**~~ Landed. New `xml::requested_props(body)` parses
  the propfind body once into a `HashSet<String>`, skipping
  comments, CDATA sections, processing instructions, and
  `<!DOCTYPE>` declarations. Every PROPFIND handler computes
  the set at entry and consults it via `contains`; folds in
  the matching `[perf] CalDAV PROPFIND `body_requests_prop`
  runs per event per property` item below. `body_requests_prop`
  remains as a thin wrapper for one-shot tests.
- ~~**[bugs] CalDAV path parsing accepts duplicated slashes /
  doesn't percent-decode.**~~ Landed. `parse_path` rejects any
  trimmed-path segment that's empty (`/calendars//u/cal/` →
  `Unknown`) and percent-decodes each segment via the new
  `percent_decode` helper, so calendar / event ids that
  round-trip through clients carrying `%XX`-encoded chars
  match. Tests in `tests/caldav.rs`.
- **[perf] `Fixture::delta_since` cancel set is O(c·d).**
  `src/fixture.rs:355-360` does `destroyed.contains(id)`
  (linear `Vec` scan) per created id. With a 256-transition
  log over a 10k-event fixture, a stale-client follow-up
  delta can pay ~600k comparisons. Build `destroyed` as
  `HashSet<&str>` once. Same code path: `dedup_preserving_order`
  clones every id into the HashSet - hash on `&str`.
- **[perf] Graph delta walkers do nested `find` per delta id.**
  `src/graph/calendar.rs:248-256`,
  `src/graph/contacts.rs:329-336`. O(K · N) per delta call
  where K = delta size, N = fixture-wide resources. Build an
  `id → &Resource` HashMap once at the top of the handler.
- **[perf] `step_fixture` clones full `emails` / `mailboxes` /
  `events` vectors on every step under the write guard.**
  `src/routes.rs:803-805`. Pure-defensive snapshot for
  rewind-on-error; a 100-step script against a 10k-email
  fixture re-pays the deep clone 100 times. Either pre-validate
  cross-refs so the apply path is infallible (the validation
  loops at lines 897-906, 944-953, 992-994 already exist), or
  snapshot only the touched indices.
- **[perf] `?stable=true` rebuilds a fresh `Value` per row.**
  `src/routes.rs:484-493` walks the snapshot and clones every
  `detail: Value` into a fresh `json!({...})` to strip
  `received_at`. With cap = 100k and rich JSON details that's
  the dominant per-call working set. Serialize directly from
  `RequestEntry` borrows via a view struct rather than rebuilding
  the JSON tree. The internal-clone half of this item landed via
  the `RequestLog::snapshot` simplification above.
- **[perf] IMAP `RFC822.SIZE` re-renders the entire body just
  to take `.len()`.** `src/imap.rs:1699-1701`. Combined with
  every other `BODY[*]` attribute on the same fetch line,
  `render_rfc822` runs once per attribute - 5x re-encoding for
  a typical Apple-Mail-shaped fetch list. Render once into a
  small struct (`headers`, `text`, optional `multipart_full`)
  and reuse slices.
- **[perf] `split_raw` rerun per raw-bytes attribute.**
  `src/imap.rs:1781-1786, 1790, 1848, 2057, 2082`. A FETCH
  asking for `BODY[HEADER]` + `BODY[TEXT]` + `BODY[1.MIME]` +
  `BODY[1]` runs the search 4× per message. Folds into the
  render-once fix above.
- **[perf] `cmd_uid_fetch` materializes all FETCH lines before
  writing.** `src/imap.rs:715-728` builds `Vec<String>` of
  every rendered message under the read guard. For a
  10k-message FETCH peaks RAM at the entire mailbox; the lock-
  drop motivation is correct but the obvious shape is to
  clone the inputs and render+write per item. Lift the
  existing `Streaming UID FETCH` future-work item from this
  file's "IMAP follow-ups" section into here.
- **[perf] `Email/set` updates and destroys each scan all
  emails.** `src/jmap.rs:1060, 1080`,
  `src/routes.rs::apply_change_step:920, 954, 971`.
  `(U + D) · N` per envelope. Build an `id → idx` map at the
  top of the handler; rewrite `retain` as a single pass
  consulting a `HashSet<&str>` of destroy ids.
- ~~**[perf] CalDAV PROPFIND `body_requests_prop` runs per event
  per property.**~~ Landed via the `xml::requested_props`
  refactor above; the prop set is parsed once at PROPFIND
  entry and threaded through `home_collection_props`,
  `calendar_props`, `event_resource_props`.
- **[arch] `apply_contact_patch` / `apply_contact_folder_patch`
  / `apply_change_event_patch` live in `src/routes.rs`.**
  Routes is the HTTP transport seam; canonical-type apply
  logic belongs alongside the types or in a dedicated patch
  module. Plus there are now two distinct event-patch shapes
  (`graph::calendar::apply_event_patch` for Graph wire,
  `routes::apply_change_event_patch` for the change-script
  flat-RFC3339 form). Move to `src/fixture/patches.rs` (or
  onto canonical-type `impl` blocks); keep the JMAP / Graph
  wire-shape patches in their respective protocol modules.
- **[arch] Patch field-name conventions diverge.**
  `apply_mailbox_patch` uses camelCase JMAP names (`parentId`,
  `sortOrder`, `isSubscribed`); the contact / contact_folder /
  change-script-event patches use snake_case. Pick a
  convention per resource family and document it; lean
  snake_case for change-script-only resources since there's no
  protocol-wire forcing a name and the canonical types are
  already snake_case.
- **[arch] `src/routes.rs` has grown to ~1440 lines.** Hosts
  JMAP HTTP, OAuth wiring, SMTP-submission test routes,
  request-log routes, fixture reset, fixture step (with three
  patch helpers and a 380-line `apply_change_step`), latency
  GET/SET, snapshot-state, and an RFC 5987 utility. Split the
  `/test/*` family into `src/routes/test_admin.rs` so the JMAP
  HTTP / OAuth router glue stays scrollable.
- **[docs] CalDAV is wired but undocumented in
  `notes/orchestration.md`, `notes/request-log.md`, and
  `notes/fixture-format.md`.** The sentinel content section
  (`orchestration.md` line 79) lists the five original
  protocols; `src/main.rs:94` writes `CALDAV` too. The
  lifecycle diagram does not list `RATATOSKR_TEST_CALDAV_ENDPOINT`.
  `request-log.md` enumerates five protocol tags; CalDAV
  records under `"caldav"`. `fixture-format.md:233` reads
  "the same canonical types will feed CalDAV when that
  listener lands" - now stale. Update all three.
- ~~**[docs] `/test/snapshot-state` and `/test/fixture/step`
  responses miss `contacts` / `contact_folders` in the
  documented JSON shapes.**~~ Landed. `snapshot-state` now
  emits both fields too (was missing them; the `step` response
  already did). `notes/orchestration.md` updated to document
  both shapes including the new fields.

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

Findings from a four-agent (security / bugs / perf / arch) review of
the work landed in commits `de89827..3b87085`. Walk-backs, verified-
correct invariants, and accepted trade-offs are recorded as inline
comments at the relevant code sites; only items that need work end
up here.

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
- Documented recipe in `notes/fixture-format.md` for slow paged
  responses via `wait(ms)` inside an `on()` callback - already
  achievable today, just needs the writeup.
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

## IMAP (lower-priority follow-ups)

- Streaming `UID FETCH` is tracked under the 2026-05-10 perf
  bucket above (`cmd_uid_fetch` materializes all FETCH lines).
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

- People API contacts (`<ratatoskr>/crates/gmail/src/contacts/`).
  Different base URL (`https://people.googleapis.com/v1/`); will
  need either a `people.googleapis.com`-shaped listener or a
  separate `--people-port`. Lean toward separate listener.
- Google Drive resumable uploads
  (`<ratatoskr>/crates/gmail/src/gdrive.rs`). Needed once the
  submission paths grow attachments large enough to spill out of
  inline.
- Calendar lives in a separate `CalendarRuntime` in ratatoskr; not
  part of Gmail mail sync. Will land as its own surface scout when
  needed.
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
