# sæhrimnir

Deterministic mock email-protocol server. The boar that's slaughtered
every evening and resurrected every morning - fitting for a
fixture-driven test peer that comes up identical on every spawn.

Used by ratatoskr's sync tests, orchestrated by brokkr. Started life
as a JMAP-only mock (plan-2 of a three-plan effort, `notes/plan.md`)
and has grown to cover every protocol ratatoskr's sync code talks to.
One TOML fixture in, five wire shapes out, byte-stable across runs.

## Protocols

| Protocol | Surface                                          | Notes                                                                  |
|----------|--------------------------------------------------|------------------------------------------------------------------------|
| JMAP     | `/jmap/session`, `/.well-known/jmap`, `POST /jmap/api` | Session resource, `Mailbox/get`, `Email/query`, `Email/get`.       |
| IMAP     | TCP, plaintext                                   | Full initial-sync read path: greeting, CAPABILITY, LOGIN/AUTHENTICATE, ENABLE QRESYNC, LIST, STATUS, SELECT/EXAMINE/CLOSE, UID SEARCH, UID FETCH (with RFC 822 body emission), CONDSTORE CHANGEDSINCE. |
| SMTP     | TCP, plaintext                                   | Submission only. EHLO, AUTH (PLAIN/LOGIN/XOAUTH2/OAUTHBEARER), MAIL FROM, RCPT TO, DATA with dot-stuffing reversal. Submissions captured in an in-memory log tests can introspect. |
| Microsoft Graph | `/v1.0/me/mailFolders/...`                | Folder enumeration (list, by-id, by-well-known-alias, childFolders), message list with `$filter` / `$top` / `$skiptoken` / `$orderby` / `$count`, delta sync (initial dump, follow-up no-op, `$deltatoken=latest` shortcut). Catchall returns the Graph error envelope for unimplemented resources. |
| Gmail    | `/gmail/v1/users/me/...`                         | Profile, labels, threads (list with `q=after:YYYY/M/D` and `nextPageToken`, full thread fetch with MIME payload), history (read-only no-op), attachments (404 stub), sendAs (empty). |

Each protocol projects from the same canonical types in
`src/fixture.rs`; no fixture-format changes when a new protocol
landed. Determinism contract: same fixture in, byte-stable bytes out.
State tokens are pinned for the lifetime of a fixture (`fixture-state`
for JMAP, `1` for IMAP HIGHESTMODSEQ and Gmail historyId, `s.0` /
`d.1` for Graph cursors).

## Where to read

- `CLAUDE.md` - project rules, layout, and the `brokkr check` /
  `scripts/smoke.sh` workflow.
- `TODO.md` - per-protocol task list, what's done and what's left.
- `notes/plan.md` - the original JMAP-only v0 plan.
- `notes/orchestration.md` - how brokkr drives us: lifecycle,
  sentinel, env vars.
- `notes/fixture-format.md` - TOML fixture shape and validation
  rules.
- `notes/ratatoskr-{jmap,imap,smtp,graph,gmail}-surface.md` - per-
  protocol cheat sheets distilled from ratatoskr's client code, with
  `crates/<proto>/src/...:LL` citations.
- `notes/{imap,smtp,graph,gmail}-plan.md` - per-protocol
  implementation plans with the design decisions worked out.

## Running

```sh
cargo run -- \
    --readiness-file /tmp/sae.ready \
    --fixture fixtures/jmap-small.toml
```

Each protocol takes its own `--<proto>-port`; passing `0` (the
default) picks an ephemeral port. The chosen ports land in the
readiness sentinel, one line per protocol:

```
READY 38779
IMAP 37445
SMTP 44037
GRAPH 43603
GMAIL 38091
```

Brokkr's `wait_for_sentinel` watches the file for presence; the
calling code reads the file to extract the per-protocol port.

`scripts/smoke.sh` builds the binary, boots it, drives every
protocol against the live process, sends SIGTERM, and verifies a
clean exit within the 1-second graceful-shutdown budget.

## Status

JMAP, IMAP read path, SMTP submission, Graph mail-sync, and Gmail
mail-sync are complete for v0. Future increments grow the fixture
format (calendar events, contacts, attachments, drive files,
incremental-sync change scripts) and add sibling resource modules
inside `src/graph/` and `src/gmail/`. See `TODO.md`.
