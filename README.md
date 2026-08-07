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
| JMAP     | `/jmap/session`, `/.well-known/jmap`, `POST /jmap/api` | Session resource, `Mailbox/get`, `Email/query`, `Email/get`, `Mailbox/changes` and `Email/changes` with steady-state semantics (empty delta on matching `sinceState`, `cannotCalculateChanges` otherwise). |
| IMAP     | TCP, plaintext                                   | Full initial-sync read path: greeting, CAPABILITY, LOGIN/AUTHENTICATE, ENABLE QRESYNC, LIST, STATUS, SELECT/EXAMINE/CLOSE, UID SEARCH, UID FETCH (with RFC 822 body emission), CONDSTORE CHANGEDSINCE. |
| SMTP     | TCP, plaintext                                   | Submission only. EHLO, AUTH (PLAIN/LOGIN/XOAUTH2/OAUTHBEARER), MAIL FROM, RCPT TO, DATA with dot-stuffing reversal. Submissions captured in an in-memory log tests can introspect. |
| Microsoft Graph | `/v1.0/me/mailFolders/...`, `/v1.0/me/calendars/...`, `/v1.0/me/events/...` | Mail: folder enumeration (list, by-id, by-well-known-alias, childFolders), message list with `$filter` / `$top` / `$skiptoken` / `$orderby` / `$count`, delta sync. Calendar: list / by-id / `default` alias, events list with pagination, `calendarView/delta`, single-event GET, plus echo-mode POST/PATCH/DELETE that record bodies in the request log without mutating the fixture. Catchall returns the Graph error envelope for unimplemented resources. |
| Gmail    | `/gmail/v1/users/me/...`                         | Profile, labels, threads (list with `q=after:YYYY/M/D` and `nextPageToken`, full thread fetch with MIME payload), history (read-only no-op), attachments (404 stub), sendAs (empty). |
| OAuth 2.0 | `/oauth/token`, `/oauth/userinfo` (mounted on the JMAP listener) | Authorization-code and refresh-token grants, OIDC userinfo projecting the fixture account. Bearer enforcement on JMAP/Graph/Gmail is opt-in via `[oauth] enforce = true` in the fixture; default keeps the v0 "no auth" baseline. |

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

Lua scenarios can also register reactive callbacks via
`on(protocol, command, function)`. The protocol layer consults the
script before generating its default response; the callback receives
a `req` table with `call_index` plus protocol-specific fields, and
can return `{ status = "...", message = "..." }` to override the
wire response (or `nil` to pass through). Wired across all five
protocols - IMAP `UID FETCH`, JMAP method calls, Graph mail
endpoints, Gmail mail endpoints, SMTP `MAIL`/`RCPT`/`DATA` - with
per-protocol mapping documented in CLAUDE.md.

Inside callbacks (or at script load), three control helpers are
also available:

- `wait(ms)` - block the current dispatch for `ms` milliseconds.
  Useful for latency injection. Other connections queue briefly on
  the dispatcher mutex but unrelated protocol handling continues.
- `mock_done()` - signal the runtime to shut the listeners down
  cleanly (exit 0). First call wins, so a chatty script doesn't
  override an earlier `mock_fail`.
- `mock_fail("reason")` - signal a fault exit. The reason is
  printed to stderr; the process returns a non-zero exit code.
  Lets brokkr observe scenario success/failure via exit code
  instead of polling.

Note that dellingr deliberately omits Lua's unparenthesized
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
- `reference/orchestration.md` - how brokkr drives us: lifecycle,
  sentinel, env vars.
- `reference/fixture-format.md` - fixture shape and validation rules
  (shared by the TOML and Lua loaders).
- `reference/ratatoskr-{jmap,imap,smtp,graph,gmail,oauth}-surface.md` -
  per-protocol cheat sheets distilled from ratatoskr's client code,
  with `crates/<proto>/src/...:LL` citations.

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

## Test / admin control plane

A handful of tests-only routes mounted on the JMAP HTTP listener
let harness scripts inspect and reset the binary's in-memory state
across cohort cycles without restarting it:

- `GET /test/smtp/submissions` / `DELETE /test/smtp/submissions` -
  parsed projection of every captured SMTP submission.
- `GET /test/requests` / `DELETE /test/requests` - cross-protocol
  request log spanning every dispatch event across the five
  protocol layers (`(protocol, command, received_at, detail)`).
- `GET /test/fixture/identity` - `{ name, path, sha256 }` for the
  fixture source this process was launched with. The digest covers
  the file's bytes as read, so a consumer holding its own copy can
  `sha256sum` it and assert the mock it is driving is the copy it
  thinks it is driving. Publishing is ours; checking is theirs.
- `POST /test/fixture/reset` - clear the SMTP submission log, the
  request log, and the OAuth token store. The fixture itself is
  read-only in v0; this route grows when `[[change]]` scripts land.
- `POST /test/fixture/step` - reserved for `[[change]]` scripts;
  returns 501 today so harness scripts detect the gap rather than
  silently no-op.
- `POST /test/oauth/invalidate` - drop a token from the OAuth
  store so subsequent userinfo / bearer-enforced requests reject
  it.

See `reference/orchestration.md` for the full contract.

## Status

JMAP (including incremental `*/changes` methods), IMAP read path,
SMTP submission, Graph mail-sync + calendar, Gmail mail-sync, and
the OAuth 2.0 / OIDC provider are complete for v0. The Lua fixture
loader (via dellingr) covers both the static-fixture surface
(`fixture` / `account` / `mailbox` / `email` builders plus
`bulk_emails` / `bulk_threads`) and the dynamic surface (`on(...)`
callbacks, `wait`, `mock_done`, `mock_fail`). Future increments
grow the fixture shape (incremental change scripts, contacts,
adversarial-shape MIME) and add sibling resource modules inside
`src/graph/` and `src/gmail/`. See `TODO.md`.
