# IMAP mock - implementation plan

Companion to `notes/ratatoskr-imap-surface.md`. Lays out the v0
increment for IMAP support in saehrimnir.

## Goal

Ratatoskr's IMAP client, pointed at saehrimnir's IMAP listener,
performs a full initial sync against the same fixture used by the JMAP
side. The two protocols share fixture state - one fixture, two wire
representations.

## Out of scope (deferred to v1+)

- TLS / STARTTLS. Plaintext only.
- Write paths (APPEND, COPY, MOVE, DELETE, EXPUNGE, STORE that
  modifies flags). v0 STORE is allowed in the protocol but the mock
  ignores side effects.
- IDLE, NOTIFY, COMPRESS, NAMESPACE, ACL.
- Multi-account fixtures (one personal account, just like JMAP).
- Failure injection.
- Incremental change scripts. UIDVALIDITY and HIGHESTMODSEQ are
  pinned at 1 for the lifetime of a fixture.

## CLI

The single binary listens on both protocols. Add `--imap-port N`
alongside the existing `--port N` (which becomes `--jmap-port` for
clarity, with the old name kept as a hidden alias). `0` means
ephemeral. Sentinel writes `READY <jmap_port> <imap_port>` so the
orchestrator can extract both.

Open question: do we want one sentinel per protocol or one combined
line? Combined is simpler to write atomically; orchestration.md says
plan-3 only needs presence anyway, and the port is used by the
harness binary, not the watcher. Combined wins.

## Architecture

- One `IMAPListener` accepting TCP connections, one tokio task per
  connection.
- Per-connection state machine:
  `NotAuthenticated -> Authenticated -> Selected -> Logout`.
- Inside each state, a tag-line reader/writer drives a command
  dispatcher.
- Shared `Arc<Fixture>` provides the read-only data model. The IMAP
  layer projects fixture mailboxes/emails to IMAP wire shapes.

`src/imap/` (new module) will hold:

- `listener.rs` - accept loop, per-connection task spawn.
- `connection.rs` - greeting, line reader/writer, command parse loop,
  graceful shutdown integration.
- `commands.rs` - per-command handlers.
- `wire.rs` - response formatting helpers (quoted strings, literals,
  list syntax, BODY[] rendering).
- `fixture_view.rs` - projection from `crate::fixture` to IMAP shapes
  (UIDs, folder paths, RFC 822 emission).

Determinism contract (mirrors JMAP):

- UIDs are assigned in fixture declaration order within each folder,
  starting at 1.
- UIDVALIDITY is `1` for every folder, every run.
- HIGHESTMODSEQ is `1` for every folder, every run.
- Folder paths derive from fixture mailbox `name` + `parent_id` chain.
  Hierarchy delimiter is `/`.
- INTERNALDATE is the fixture's `received_at` formatted per RFC 3501.
- BODY[] renders a deterministic RFC 822 message: From/To/Cc/Bcc/
  Subject/Date/Message-ID/In-Reply-To/References headers from the
  fixture, plus the `body_text` as the single text/plain part.

## Suggested implementation order

1. **Listener bootstrap.** New `--imap-port` flag, sentinel format
   change, IMAP listener that accepts a connection, sends the
   greeting, reads tagged commands, responds to `CAPABILITY`,
   `NOOP`, and `LOGOUT`. No auth yet, no folders. Smoke this with
   `nc` or extend `scripts/smoke.sh`.

2. **Auth + ENABLE.** `LOGIN`, `AUTHENTICATE PLAIN`, `XOAUTH2`,
   `OAUTHBEARER` all return `OK` regardless of credential.
   `ENABLE QRESYNC` echoes back `* ENABLED QRESYNC`.

3. **Folder listing.** `LIST "" "*"` emits one untagged `* LIST` per
   fixture mailbox with the right special-use attribute and the
   computed path. `STATUS folder (MESSAGES UNSEEN)` per folder.

4. **SELECT + UID SEARCH.** Enter a folder, emit EXISTS, FLAGS,
   PERMANENTFLAGS, OK [UIDVALIDITY 1], OK [UIDNEXT n+1], OK
   [HIGHESTMODSEQ 1]. Then implement `UID SEARCH ALL`,
   `UID SEARCH <n>:*`, `UID SEARCH SINCE <date>`.

5. **UID FETCH with BODY.PEEK[].** Render fixture emails as RFC 822,
   emit `* n FETCH (UID x FLAGS (...) INTERNALDATE "..." BODY[]
   {N}\r\n<body>)`. Honour the `1:*` and `n:m` UID range syntaxes.
   Batch responses by 200 even though the mock could send them all
   at once - keeping the boundary visible helps diagnose client
   bugs later.

6. **CONDSTORE / CHANGEDSINCE.** With HIGHESTMODSEQ pinned at 1, any
   `CHANGEDSINCE 0` matches everything and `CHANGEDSINCE 1` matches
   nothing. That is enough for the client's flag-resync code path.

7. **Integration tests.** `tests/imap.rs`: open a TCP socket against
   a port-zero listener, drive a full initial-sync transcript, assert
   the wire output. Also: a `tower`-style helper feels wrong for a
   line protocol; we will instead expose a `serve_imap_connection`
   helper that takes a `tokio::io::DuplexStream` so tests can drive
   it without sockets.

8. **Stretch: STORE (no-op writeback).** Accept `STORE FLAGS`
   commands and respond with the would-be FETCH untagged update,
   without persisting. Lets ratatoskr's flag writeback path complete
   without erroring. No fixture mutation in v0.

## Open questions

- **CONDSTORE FETCH item.** When CONDSTORE is on, FETCH responses
  optionally include `MODSEQ (n)`. Ratatoskr does not require it
  (only CHANGEDSINCE matters), so v0 will skip emitting MODSEQ.
- **INTERNALDATE format.** RFC 3501 specifies
  `dd-Mmm-yyyy HH:MM:SS +ZZZZ` with a quoted wrapping. chrono can
  format this directly with `%d-%b-%Y %H:%M:%S %z`.
- **Mailbox path encoding.** All v0 fixture names are ASCII. UTF-7
  encoding is not implemented for the emit path; we will reject any
  fixture mailbox name with non-ASCII characters at load time so we
  do not silently produce invalid UTF-7.
- **Concurrent connections per fixture.** Cheap because the fixture
  is read-only; one tokio task per connection is fine. No locking
  needed.
- **Idle / hung connections.** v0 has no per-connection idle timeout.
  Plan-3 will tear the process down via SIGTERM when done.
