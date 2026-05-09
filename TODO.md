# TODO

Running task list, ordered by what ratatoskr is actively waiting on.
Per-protocol design notes live alongside in `notes/`; this file just
tracks what's next.

## Ratatoskr-driven gaps (2026-05-09 audit)

Ten items ratatoskr's sync code is actively waiting on, ordered
roughly by leverage. Items 1, 2, 3, and 9 are tightly coupled -
they all need the fixture to become writable / steppable, which
`POST /test/fixture/step` is the natural seam for. Lifting the
read-only fixture invariant is the prerequisite for the mutation
trio.

### Mutation surfaces (highest leverage)

- **[jmap] `Email/set` + `Mailbox/set`.** `src/jmap.rs:120` falls
  through to `unknownMethod` for any method outside the v0 reads.
  Needs to accept the changes and reflect them in the next
  `Email/changes` / `Mailbox/changes` so a delta-after-mutation
  script can prove the round-trip. Closes the IMAP/JMAP "mutation
  fixtures remain" gap on the JMAP side.
- **[imap] `UID STORE` (persistent), `UID COPY`, `UID EXPUNGE`.**
  `UID STORE` is wired today as a non-persistent no-op
  (`src/imap.rs:683` - tagged OK plus post-op FETCH untagged, but
  the fixture is unchanged). `UID COPY` and `UID EXPUNGE` are not
  matched at all. All three should mutate fixture state and
  surface in subsequent `UID FETCH` / CONDSTORE replies. Same
  gap as the JMAP item above, on the IMAP side.
- **[graph] Calendar mutations surface in delta.** `POST` /
  `PATCH` / `DELETE /v1.0/me/events` exist
  (`src/graph/calendar.rs:60,68,344,390,397`) and echo bodies into
  the request log, but the fixture stays read-only - the next
  `events/delta` doesn't reflect the mutation. Lift the read-only
  invariant for calendar so M6.10's create/update/delete coverage
  can land.

### Request-log granularity (landed)

Both granularity items shipped together:

- **[imap]** `UID FETCH` log rows now expose `detail.attrs` (parsed
  FETCH item list as stable string labels: `"UID"`, `"FLAGS"`,
  `"BODY[]"`, `"BODY[N]"`, `"BODY[N.MIME]"`, ...) and
  `detail.body` (true when any item asks for message bytes,
  false for metadata-only fetches). Lets a steady-state delta
  test soften to "no body refetch" while still permitting a
  flag-only reconciliation pass. Contract documented in
  `notes/request-log.md`.
- **[jmap]** Method-call log rows now surface `detail.account_id`
  (when present), `detail.ids` (when the call carries a string-
  typed `ids[]`), and `detail.properties` (when the call carries a
  string-typed `properties[]`). Distinguishes a metadata-only
  `Email/get` (e.g. `properties=["id","keywords"]`) from a body
  fetch (`properties=[..., "bodyValues"]`) without re-deriving it
  from response shape. Filter args / result references are
  deliberately left out: shape-sensitive, would bloat the log.

### OAuth-enforced fixture (M6.9 closeout, landed)

- **Revocation toggle + checked-in fixture variant.**
  `fixtures/jmap-oauth.toml` is the canonical bearer-enforced
  scenario (`[oauth] enforce = true`). The full revoked-token-
  recovery walk (mint -> sync -> revoke -> 401 -> re-mint -> sync)
  is asserted end-to-end in
  `tests/api.rs::jmap_oauth_fixture_drives_revoked_token_recovery_flow`.
  The Lua loader gained a parallel `oauth { enforce, issuer }`
  builder so dynamic scenarios can opt in too.

### Fixture breadth (M8 exit + M9 prep)

- **Larger named fixtures.** `fixtures/jmap-bulk.lua` is the 10k
  case; the `bulk_emails` / `bulk_threads` / `bulk_mailboxes`
  builders make medium (~1k), huge-thread, and many-folders
  fixtures one-liners. Author them as M9 sync benchmarks need them.
- **Edge-case fixtures.** Duplicate `Message-Id`, malformed MIME,
  configurable per-page latency. The first two need the validator
  carve-out + `body_raw_bytes` escape hatch tracked under
  "Authoring hooks for adversarial-shape fixtures" below; slow
  paging is already achievable via `wait(ms)` inside an `on()`
  callback (a documented recipe in `notes/fixture-format.md`
  closes the third).
- **Incremental sequence fixture.** A scripted timeline of
  new + change + delete + move events so a single steady-state
  script can assert the delta path handles all four. Drives
  `POST /test/fixture/step` (currently 501) and the change-script
  item under "Fixture format growth" below.

### M9 prerequisite (lower priority)

- **Deterministic timing knobs.** `POST /test/set-latency` per
  route and `GET /test/snapshot-state` to dump current server-side
  mailbox state. Neither route exists today
  (`src/routes.rs` only has `smtp/submissions`, `requests`,
  `fixture/reset`, `fixture/step`, `oauth/invalidate`). Per-route
  latency can be hacked via `on()` + `wait(ms)`; the global knob
  is what unblocks reproducible sync-bench numbers.

## From the 2026-05-09 multi-agent review

Findings from a four-agent (security / bugs / perf / arch) review
of the work landed in commits `de89827..3b87085`. Walk-backs,
verified-correct invariants, and accepted trade-offs are recorded
as inline comments at the relevant code sites; only items that
need work end up here.

### Fix now

All four "Fix now" items from the 2026-05-09 review are landed
(SMTP / IMAP auth-payload redaction, RequestLog ring cap +
take-then-clone snapshot, list_events streaming pagination,
PATCH/DELETE 404 on unknown event ids). Remaining work is
the "Fix soon" backlog below.

### Fix soon (cleanup, ergonomics, smaller bugs)

- **[bugs] `received_at` makes `RequestEntry` JSON output
  non-byte-stable.** Documented in `src/request_log.rs`. Fix
  (when a test forces it): `#[serde(skip_serializing)]` behind
  an opt-in flag, or expose a `snapshot_stable()` that strips
  timestamps.

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
  `src/imap.rs::dispatch`. Folds into the RequestLog cap fix
  above; not a standalone item.
- **[perf] `log_request` middleware records 404s through
  `not_implemented`.** `src/graph/mod.rs::log_request`,
  `src/gmail/mod.rs::log_request`. Folds into the RequestLog
  cap fix.

## Fixture format growth

The incremental-sync item is now actively wanted for M8; the
adversarial-shape items unblock JMAP-depth scenarios; the rest
remain unblocked-but-unneeded.

- Incremental sync change scripts. `[[change]]` entries (or a Lua
  equivalent) that advance state tokens between phases - JMAP
  state, IMAP UIDVALIDITY/HIGHESTMODSEQ bumps, Graph deltatokens,
  Gmail historyId. **No longer parked** - ratatoskr needs
  new/change/delete/move scenarios to exercise incremental sync
  paths. JMAP `Email/changes` / `Mailbox/changes` are already wired
  with steady-state semantics (empty delta on matching state,
  `cannotCalculateChanges` otherwise); change scripts grow this
  into a real state machine. Also drives the test control plane's
  `POST /test/fixture/step`, which currently returns 501.
- Authoring hooks for adversarial-shape fixtures: duplicate
  `Message-Id` across emails (today's `normalize` rejects it as a
  cross-reference error, so this is a validator carve-out plus
  per-protocol projection that doesn't crash on it), and
  malformed MIME (probably a `body_raw_bytes = "..."` escape hatch
  that bypasses the canonical projection and emits the bytes
  verbatim). Slow paged responses are already achievable today
  via `wait(ms)` inside an `on()` callback - just needs a
  documented recipe in `notes/fixture-format.md` rather than its
  own bullet.
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

## CalDAV (matrix item 10, alternative path)

Net-new protocol surface for ratatoskr's calendar smoke; the Graph
calendar surface (above the line) is already wired, so this is
only needed if ratatoskr's calendar client ends up speaking CalDAV
to a non-Graph backend. New `src/caldav.rs` module + new listener
binding (separate `--caldav-port`) + `notes/ratatoskr-caldav-
surface.md` scout doc. Endpoints:

- Principal discovery (`PROPFIND /.well-known/caldav`).
- Calendar home set (`PROPFIND` on principal).
- `REPORT` calendar-query and calendar-multiget.
- `PUT` event with ETag generation.
- `DELETE` event with If-Match handling.

## IMAP (lower-priority follow-ups)

- Streaming `UID FETCH` to avoid materialising the full response
  set before writing. Today's loop builds a `Vec<String>` first;
  at huge N (think `bulk_emails(count=1_000_000)`) that's enough
  memory to matter. Refactor when a fixture forces it.
- 200-message FETCH batching boundary. The current handler emits
  all matched FETCH responses in one go. Ratatoskr's client
  batches client-side (`CHUNK_SIZE = 200`), so the wire boundary
  is invisible to it; flagged as a v1 thing if we ever want to
  test the batch boundary explicitly.

## Microsoft Graph (other future work)

v0 mail-sync surface is complete; calendar is promoted above.
Remaining future Graph work, in roughly the order the next fixture
is likely to need it:

- Contact sync (`contact_sync.rs`). Will need `[[contact]]` /
  `[[contact_folder]]` fixture entries.
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

## Lua dynamic surface

Phase 2 callbacks (`on(protocol, command, fn)`) are wired across all
five protocols, mapped via `Override::Tagged { status, message }`.
What's left on the Lua side:

- Anchor release on handler overwrite. `builder_on` only holds
  `&mut Builder`, not `&mut State`, so re-registering the same
  `(protocol, command)` orphans the previous Anchor. Fixable by
  pulling state access into builder_on (refactor user_data shape)
  or by tracking pending releases and applying them at load
  finalization. Acceptable today since real scenarios register
  once.
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
