# TODO

Running task list. Per-protocol design notes live alongside in
`notes/`; this file just tracks what's next.

## Now: IMAP

See `notes/imap-plan.md` for the design and implementation order. The
short version, in commit-sized chunks:

1. Listener bootstrap. `--imap-port` flag, sentinel format change,
   IMAP listener accepting one connection at a time, server greeting,
   `CAPABILITY`, `NOOP`, `LOGOUT`. No auth, no folders.
2. Auth + `ENABLE`. `LOGIN`, `AUTHENTICATE PLAIN`, `XOAUTH2`,
   `OAUTHBEARER` all accept anything. `ENABLE QRESYNC` echoes back.
3. Folder listing. `LIST "" "*"` from fixture mailboxes with
   special-use attributes. `STATUS folder (MESSAGES UNSEEN)` per
   folder.
4. `SELECT` + `UID SEARCH`. EXISTS / UIDVALIDITY=1 / UIDNEXT /
   FLAGS / PERMANENTFLAGS / HIGHESTMODSEQ=1. Implement the three
   SEARCH shapes (`ALL`, `<n>:*`, `SINCE <date>`).
5. `UID FETCH (UID FLAGS INTERNALDATE BODY.PEEK[])` - render the
   fixture emails as RFC 822, batch responses by 200.
6. CONDSTORE / CHANGEDSINCE. With HIGHESTMODSEQ pinned at 1, the two
   relevant CHANGEDSINCE values (0 and 1) collapse to "all" and
   "none" respectively.
7. Integration tests. `tests/imap.rs` driving a duplex stream.
8. Stretch: `STORE FLAGS` no-op acknowledgement so ratatoskr's flag
   writeback path completes.

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
