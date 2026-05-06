# sæhrimnir

Deterministic mock email-protocol server. The boar that's slaughtered
every evening and resurrected every morning - fitting for a
fixture-driven test peer that comes up identical on every spawn.

Used by ratatoskr's sync tests, orchestrated by brokkr. Started life
as a JMAP-only mock and has grown to cover every protocol
ratatoskr's sync code talks to. One fixture in (TOML or Lua), five
wire shapes out, byte-stable across runs.

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

## Fixtures

Two equivalent authoring formats, dispatched by file extension:

- **TOML** (`fixtures/<name>.toml`): plain declarative config. Easy
  to read and diff. Static only.
- **Lua** (`fixtures/<name>.lua`): Lua script run through
  [dellingr](https://crates.io/crates/dellingr), a pure-Rust
  deterministic sandboxed Lua VM with cost-bounded execution. The
  script populates the same in-memory `Fixture` shape via four
  builders - `fixture({...})`, `account({...})`, `mailbox({...})`,
  `email({...})` - and runs through the same cross-reference
  validation pass as the TOML loader. Both produce a byte-identical
  `Fixture`; `fixtures/jmap-small.{toml,lua}` are the canonical
  example pair.

Lua exists as the on-ramp for upcoming dynamic features (reactive
callbacks per protocol command, scenario-driven state mutations,
self-terminating scripts). v0 only exercises the static
fixture-builder surface; the dynamic surface is the next chunk of
work. Note that dellingr deliberately omits Lua's unparenthesized
function-call sugar, so builder calls are written `mailbox({...})`
not `mailbox{...}`.

For scale-testing scenarios (test sync against tens of thousands of
emails), `bulk_emails({ count = N, mailbox = "...", seed = ... })`
generates synthetic emails directly into the fixture without going
through per-email Lua allocation, and `bulk_threads({ count,
messages_per_thread, ... })` builds threaded conversations with
proper `In-Reply-To` / `References` chaining. Templates and pools
are lifted from ratatoskr's `dev-seed` crate; deterministic via a
seeded `SmallRng`. `fixtures/jmap-bulk.lua` shows the shape; loads
10k emails in well under a second on a modern host.

## Where to read

- `CLAUDE.md` - project rules, layout, and the `brokkr check` /
  `scripts/smoke.sh` workflow.
- `TODO.md` - per-protocol task list, what's done and what's left.
- `notes/orchestration.md` - how brokkr drives us: lifecycle,
  sentinel, env vars.
- `notes/fixture-format.md` - fixture shape and validation rules
  (shared by the TOML and Lua loaders).
- `notes/ratatoskr-{jmap,imap,smtp,graph,gmail}-surface.md` - per-
  protocol cheat sheets distilled from ratatoskr's client code, with
  `crates/<proto>/src/...:LL` citations.

## Running

The binary is `saehrimnir` (ASCII transliteration so `cargo`,
filesystems, and shells stay sane). After `cargo build` it lives at
`target/<profile>/saehrimnir`:

```sh
saehrimnir \
    --readiness-file .smoke/ready \
    --fixture fixtures/jmap-small.toml
```

Each protocol takes its own `--<proto>-port` (`--jmap-port`,
`--imap-port`, `--smtp-port`, `--graph-port`, `--gmail-port`);
passing `0` (the default) picks an ephemeral port. The chosen ports
land in the readiness sentinel, one line per protocol:

```
JMAP 38779
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
mail-sync are complete for v0. The Lua fixture loader (via dellingr)
is wired but currently only exposes the static-fixture surface;
reactive callbacks for dynamic scenarios are the next chunk. Future
increments grow the fixture shape (calendar events, contacts,
attachments, drive files), add sibling resource modules inside
`src/graph/` and `src/gmail/`, and expose the dynamic Lua surface.
See `TODO.md`.
