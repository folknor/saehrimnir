# TODO

Running task list. Per-protocol design notes live alongside in
`notes/`; this file just tracks what's next.

## Now: IMAP - what's left

Most of the v0 IMAP surface has landed. Still pending:

- Stretch: `STORE FLAGS` no-op acknowledgement so ratatoskr's flag
  writeback path completes (not load-bearing for read-only sync).
- 200-message FETCH batching boundary. The current handler emits all
  matched FETCH responses in one go. Ratatoskr's client batches
  client-side (CHUNK_SIZE=200), so the wire boundary is invisible to
  it; flagged as a v1 thing if we ever want to test the batch
  boundary explicitly.

Done in this session:
1. Listener bootstrap, greeting, CAPABILITY, NOOP, LOGOUT.
2. LOGIN, AUTHENTICATE PLAIN/XOAUTH2/OAUTHBEARER, ENABLE QRESYNC.
3. LIST + STATUS with role-derived special-use attributes.
4. SELECT/EXAMINE/CLOSE, UID SEARCH ALL/range/SINCE.
5. UID FETCH with the four ratatoskr attributes (UID FLAGS
   INTERNALDATE BODY.PEEK[]) plus BODY[HEADER]/BODY[TEXT]/RFC822.SIZE
   for free.
6. CONDSTORE / CHANGEDSINCE - works because HIGHESTMODSEQ is pinned.
7. Integration test in `tests/imap.rs` exercising the full
   initial-sync transcript plus a literal-block byte-accuracy check
   and a determinism check across two runs.

## Next: SMTP

Smaller scope - submission only, no delivery. Surface scout of
`<ratatoskr>/crates/smtp/src/` first, then a plan doc, then code.
SMTP shares no abstractions with the read protocols; it will need a
"submitted message buffer" so tests can assert what got sent.

## Next: Microsoft Graph

JSON-over-HTTPS like JMAP, so the axum infrastructure carries over.
New surface scout (`<ratatoskr>/crates/graph/src/`), new plan doc,
new dispatcher. EWS (Exchange Web Services) is in
`graph/src/ews/` - flag whether it is also exercised by ratatoskr's
sync path; if so it lands here too.

## Next: Gmail

Google's REST API. Same shape as Graph - HTTP + JSON. Surface scout
of `<ratatoskr>/crates/gmail/src/`, plan doc, then code.

## Open questions still pending

- HTML-only bodies. Add `body_html` to the fixture format as a
  parallel option to `body_text`? Reserved in fixture-format.md but
  not implemented. Will become more pressing once IMAP needs to
  render the wire body and Graph wants HTML rendering.
- Multipart MIME via `body_path`. Deferred until a fixture needs it.
  IMAP forces this question because BODY[] has to emit a real
  multipart/mixed when there are attachments.
- Multi-account fixtures. v0 enforces `is_personal = true` and
  exactly one account. Lifting requires per-protocol tweaks to
  surface multiple accounts.
- Failure injection. Plan 2 reserves `[fault]` blocks for v1. Slow
  responses, retryable errors, network-level errors. Useful for all
  protocols once the happy paths land.
- Incremental sync. JMAP `Email/changes` / `Mailbox/changes`,
  IMAP UIDVALIDITY bumps and HIGHESTMODSEQ advancement, Graph delta
  tokens. All require fixture-side `[[change]]` entries that advance
  state tokens. Out of scope until every happy path lands.

## Cosmetic and housekeeping

- A `brokkr.toml` with `project = "saehrimnir"` plus a `[[check]]`
  sweep needs `Project::Saehrimnir` to land in brokkr's enum first,
  or the file would fail brokkr's parse-time validation. Until then,
  rely on `brokkr check`'s no-toml fallback.
- A subprocess + reqwest test that exercises the sentinel + SIGTERM
  path end-to-end. The `tests/api.rs` suite covers the wire format
  via `tower::ServiceExt::oneshot`; a single subprocess-shaped test
  would close the gap currently filled only by `scripts/smoke.sh`.
- Plan-3 / ratatoskr wiring. From saehrimnir's side this just needs
  jmap-client + ratatoskr's IMAP client to talk to us cleanly. Two
  observable behaviours we should re-verify once plan-3 lights up:
  whether jmap-client follows a relative `apiUrl` and whether
  ratatoskr's IMAP client tolerates our exact greeting/CAPABILITY
  ordering.
