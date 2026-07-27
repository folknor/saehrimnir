# Ratatoskr's IMAP client surface

What the v0 IMAP mock has to satisfy. Distilled from
`<ratatoskr>/crates/imap/` on 2026-05-06. Source-of-truth lives there;
this file is a cheat sheet so we don't have to fan out every turn.

Each entry below names the file/line where the behavior is observable,
so the next person can re-verify after the client drifts.

## Connection lifecycle

- Client connects via TCP to `host:port` with security mode: `"tls"`
  (direct TLS), `"starttls"` (plain to upgrade), or `"none"`
  (plaintext). Source: `connection.rs:228-266`.
- Server greeting line required (`* OK ...`). For plain and starttls
  paths the client reads and validates the greeting; STARTTLS path
  explicitly checks for `OK` in the response. `connection.rs:285-296`
  (plain greeting), `:299-315` (STARTTLS response).
- v0 mock plan: plaintext only (`security = "none"`). No STARTTLS, no
  TLS, no compression. The mock will not advertise STARTTLS.
- After connection the client sends `CAPABILITY`, then authenticates,
  then `ENABLE QRESYNC`.

## Authentication

- Three auth methods supported: LOGIN (password), XOAUTH2,
  OAUTHBEARER. Determined by `config.auth_method`. Source:
  `connection.rs:344-390`.
- v0 mock: every credential and method succeeds (no validation).
  Stage 5 of the multi-account refactor grew **per-connection
  account binding** on top: the credential's identity is parsed
  and matched against the fixture's declared `[[account]]`s. A
  match rebinds the connection's `account_id`; subsequent LIST /
  STATUS / SELECT / FETCH return only that account's mailboxes
  and messages. An unrecognised user (or no AUTH at all) keeps
  the connection on the fixture's primary account, matching the
  v0 no-auth baseline.
  - **LOGIN**: the (quoted) username arg is matched
    case-insensitively against `account.name`.
  - **AUTHENTICATE PLAIN**: base64 of `\0user\0pass`. `user` is
    matched as above.
  - **AUTHENTICATE LOGIN**: the single continuation line is
    treated as the username (the second `Password:` round-trip
    isn't modelled today; extend when a fixture forces it).
  - **AUTHENTICATE XOAUTH2 / OAUTHBEARER**: the `\x01`-separated
    SASL blob is scanned for `auth=Bearer <token>` (looked up in
    the OAuth `TokenStore`, same store the Google-family
    listeners use); if no bearer match, the `user=` field is
    tried instead.
- No STARTTLS check is needed for plaintext paths; the gate is on the
  security mode, not a capability probe.

## Capabilities

The mock advertises:

- `IMAP4REV1` - baseline.
- `CONDSTORE` (RFC 4551) - enables HIGHESTMODSEQ in SELECT and the
  `CHANGEDSINCE` modifier in FETCH. Probed at `connection.rs:419-427`,
  used at `client/commands.rs:293-320`.
- `QRESYNC` (RFC 7162) - efficient resync. Client sends
  `ENABLE QRESYNC` after LOGIN and expects `* ENABLED QRESYNC` in the
  response. Without that line the client falls back to CONDSTORE-only.
  `connection.rs:438-502`.
- `NAMESPACE` (RFC 2342) - advertised and implemented. Returns the
  personal namespace (`("" "/")`), the other-users namespace
  (`("#user/" "/")`) that surfaces shared folders, and a NIL shared
  namespace. Drives ratatoskr's shared-folder (A5c) discovery.
- `ACL` (RFC 4314) - advertised; `MYRIGHTS` and `GETACL` implemented
  (read-only ACL surface, no SETACL/DELETEACL in v0).
- Optional but recognized: `MOVE`. Used if present, graceful
  fallback otherwise.

Advertised in v0:

- `IDLE` (RFC 2177) - advertised in CAPABILITY and implemented. The
  test-admin state-mutation trigger (`POST /test/fixture/step` ->
  `PushHub::emit_state_advance`) wakes an idling connection so it emits
  the unsolicited `* n EXISTS` / `* n EXPUNGE` / `* n RECENT` for the
  selected mailbox. See "IDLE" below and `src/imap.rs::cmd_idle`.

NOT advertised in v0:

- `NOTIFY`, `COMPRESS`, `STARTTLS`, `APPEND` (write paths),
  `XLIST` (Google extension; client falls back to LIST + attributes).
- `SPECIAL-USE` is fine to advertise or omit; only the per-folder
  attribute flags matter (`\Sent`, `\Trash`, etc.) and those are
  emitted on LIST regardless.

## Folder listing

- Command: `LIST "" "*"` (list all). `client/mod.rs:43`.
- Shared folders use `LIST "" "{prefix}*"` per namespace
  (`client/mod.rs:134`). We advertise NAMESPACE and serve the
  other-users prefix `#user/`: a `LIST "" "#user/*"` enumerates
  mailboxes other accounts have shared with the authenticated one
  (fixture `[[acl]]` grants), projected as `#user/<owner>/<path>`. A
  bare `LIST "" "*"` stays personal-only. See "Shared folders" below.
- Untagged response: `* LIST (attributes) "delimiter" "name"`. Parser:
  `parse.rs:14-40`.
  - Attributes: `\Noselect` skips the folder. RFC 6154 special-use
    flags (`\Sent`, `\Trash`, `\Drafts`, `\Junk`, `\Archive`, `\All`,
    `\Flagged`, `\Important`, plus `\Inbox` for the inbox) are parsed
    and used to tag folders.
  - Delimiter: string or NIL. Folder hierarchy separator (e.g., `/`).
  - Name: modified UTF-7 (RFC 3501 sec 5.1.3). Client decodes it to
    UTF-8 (`client/mod.rs:61`). All-ASCII names need no encoding.
  - Casing: case-insensitive flag matching via
    `eq_ignore_ascii_case` (`client/mod.rs:27`, `parse.rs:27`).
- After LIST, client sends `STATUS "folder" (MESSAGES UNSEEN)` per
  folder. `client/mod.rs:82-90`.

## SELECT / EXAMINE / folder state

- Command: `SELECT folder` (read-write) before any UID FETCH or UID
  SEARCH. `client/sync.rs:34`. bifrost opens with the RFC 7162
  CONDSTORE select-parameter: `SELECT INBOX (CONDSTORE)`. The mock
  parses the `(...)` select-parameter group (`parse_select_args`):
  `CONDSTORE` is accepted, and `QRESYNC (<uidvalidity> <modseq>
  [<known-uids> [<seq-match-data>]])` is accepted too (the
  `uidvalidity` / `modseq` / seq-match elements are parsed for syntax
  but not acted on - UIDVALIDITY is pinned and VANISHED is resolved
  from UID history; the optional known-UID set bounds VANISHED output).
  An unknown select-param or any trailing junk replies `BAD`. Before
  this, the name-only astring parser rejected the trailing parens and
  replied `BAD` to bifrost's real SELECT.
- Untagged responses parsed, in any order, until the tagged OK:
  - `* <n> EXISTS` - total messages.
  - `* OK [UIDVALIDITY <u32>]` - folder identity. Cached per-folder;
    mismatch on resync triggers full refetch
    (`sync_pipeline.rs:487-503`).
  - `* OK [UIDNEXT <u32>]` - predicted next UID.
  - `* <n> RECENT` - parsed by async_imap, not actively used.
  - `* OK [HIGHESTMODSEQ <u64>]` - required for CONDSTORE/QRESYNC
    paths. Cached per-folder. Real value: `Fixture::imap_highestmodseq`
    = the per-account change counter plus one, so it is `1` on a
    never-mutated fixture and advances on every mutation.
  - `* VANISHED (EARLIER) <uids>` - emitted only when the client
    opened with a `(QRESYNC (...))` parameter. Lists UIDs whose
    history slot has retired to `None` (expunged / moved out /
    destroyed), bounded to the client's known-UID set when supplied.
    Lets a QRESYNC client prune expunged messages without the
    `UID SEARCH ALL` diff. bifrost itself opens with CONDSTORE (not
    QRESYNC), so it uses the SEARCH-diff path; VANISHED is available
    for any QRESYNC-capable client.
  - `* FLAGS (\Seen \Flagged \Draft ...)` - supported flags.
  - `* OK [PERMANENTFLAGS (\Seen \Flagged \* ...)]` - flags clients
    can set/create. The `\*` token signals custom-keyword support
    (Flag::MayCreate, `client/mod.rs:24-31`).

## Message fetching - initial sync

- Per-folder: `UID FETCH 1:* (UID FLAGS INTERNALDATE BODY.PEEK[])` in
  batches of 200 messages. `client/sync.rs:279`, `client/mod.rs:230`,
  `sync_pipeline.rs:24` (CHUNK_SIZE).
- Attributes:
  - `UID` - numeric message id.
  - `FLAGS` - `\Seen`, `\Flagged`, `\Draft`, plus custom keywords.
  - `INTERNALDATE` - server timestamp (fallback when Date: header
    cannot be parsed).
  - `BODY.PEEK[]` - full RFC 822 message. PEEK means do not set
    `\Seen` as a side effect.
- `BODYSTRUCTURE` is not used; the client parses the raw RFC 822
  body itself via mail-parser.

### bifrost note (the client we are migrating to)

The sections above describe the OLD ratatoskr-direct IMAP client.
bifrost's inventory FETCH is different and the mock must serve it:
`UID FLAGS ENVELOPE RFC822.SIZE` plus `MODSEQ` whenever CONDSTORE is
enabled (`research/bifrost/crates/imap/src/account/inventory.rs:225-231`,
`get.rs:213-219`). Two consequences for the mock:

- `ENVELOPE` (RFC 3501 7.4.2) must parse and emit - bifrost reads
  sender/subject/date from it instead of the raw headers. `src/imap.rs`
  serves it via `render_envelope`.
- bifrost APPENDS `MODSEQ` to the attr list because we advertise
  `CONDSTORE QRESYNC`, and rejects a value of 0
  (`folder_registry.rs` "FETCH returned MODSEQ 0"). The mock emits a
  real per-message `MODSEQ (<n>)`: `Fixture::email_modseq` resolves to
  the change-log counter of the email's last create/update plus one
  (baseline 1 for an email untouched since load). On a never-mutated
  fixture every message reports `MODSEQ (1)`, matching the baseline
  `HIGHESTMODSEQ 1`; after a mutation the touched message's modseq
  advances. When `CHANGEDSINCE` is present the mock also auto-appends
  `MODSEQ` to the response even if the client did not list it
  (RFC 7162 3.1.4.1).

Before both were parsed, the unknown attr made `parse_fetch_attrs`
return `None` and the whole `UID FETCH` replied `BAD`, breaking
bifrost's initial mail sync right after SELECT.

## Message fetching - delta / CONDSTORE

- New messages: `UID SEARCH (last_uid+1):*` to find new UIDs, then the
  same FETCH as initial. `client/sync.rs:111`,
  `client/commands.rs:9-33`.
- Flag changes (CONDSTORE): `UID FETCH 1:* (FLAGS) (CHANGEDSINCE
  <modseq>)`. Returns only flags for messages changed since cached
  HIGHESTMODSEQ. `client/commands.rs:293-320`. The mock honours this
  per-message: it keeps only emails whose `email_modseq` exceeds the
  given value, so a message touched since the client's cached modseq
  surfaces while untouched ones are filtered out.
- Flag changes (no CONDSTORE): `UID FETCH 1:* (FLAGS)` - full sweep,
  diffed client-side. `client/commands.rs:366-393`.
- Deletion detection: `UID SEARCH ALL` to enumerate live UIDs; client
  diffs against cache. `imap_delta_janitor.rs:144-189`,
  `client/commands.rs:35-59`.

## Search / UID SEARCH

Client uses UID SEARCH only - no SORT, no THREAD, no plain SEARCH.

- `UID SEARCH ALL` - all UIDs.
- `UID SEARCH <last_uid+1>:*` - UIDs newer than the cached cursor.
- `UID SEARCH SINCE <date>` - UIDs after a date (initial sync uses a
  `days_back` cutoff). `client/sync.rs:173-176`.

Response: untagged `* SEARCH uid1 uid2 ...` (space-separated, may be
empty). Order does not matter to the client (it sorts anyway), but
the mock will emit them in ascending UID order for determinism.

## State persistence

Per-folder fields ratatoskr stores in its DB and compares on every
sync run:

- `uidvalidity: u32` - compared on every SELECT. Mismatch triggers
  full refetch. `imap_delta.rs:159`, `sync_pipeline.rs:487-503`.
- `last_uid: u32` - highest UID seen previously. Drives
  `(last_uid+1):*`.
- `modseq: u64` (optional) - cached HIGHESTMODSEQ. Used for
  CHANGEDSINCE.
- `last_sync_at` - timestamp; throttles deletion checks and
  non-CONDSTORE flag syncs (10-5 min intervals).
  `imap_delta_janitor.rs:16-21`.

These must remain stable across runs. The mock pins UIDVALIDITY to 1
and derives HIGHESTMODSEQ from the per-account change counter
(`Fixture::imap_highestmodseq` = counter + 1), so each mutation that
bumps the account's log (JMAP `Email/set`, IMAP `UID STORE`/`COPY`/
`EXPUNGE`, change-script `email_*` ops) advances HIGHESTMODSEQ, and
the per-message `MODSEQ` (`Fixture::email_modseq`) reflects each
email's own last-change counter. A never-mutated fixture still
reports `HIGHESTMODSEQ 1` / `MODSEQ (1)`, so byte-stable baseline
snapshots are unchanged.

UID stability (RFC 3501 §2.3.1.1): once assigned, a UID never
refers to a different message in the same `(UIDVALIDITY, mailbox)`
pair. The mock honours this by tracking
`Fixture::mailbox_uid_history`: an insertion-ordered list of email
ids per mailbox. Each new addition (load-time email declaration,
JMAP `Email/set` create, change-script `EmailCreate`, `UID COPY`,
`Email/set` mailboxIds add, change-script `EmailMove`) gets the
next UID; deletions / moves-out flip the slot to a tombstone but
the slot is NEVER reclaimed. UIDNEXT (= history.len() + 1) is
monotonically increasing across the fixture's lifetime. Pre-fix
the mock derived UIDs from filter-then-enumerate over the live
email list, so a delete / move would silently shift sibling UIDs
- a real client treating the new message at the freed UID as a
content update for the original would corrupt its cache. Reported
by ratatoskr 2026-05-10.

## Wire format strictness

- Parser uses `async_imap::imap_proto`. Keywords (commands,
  capabilities, flags) are case-insensitive.
  `connection.rs:457-474`, `connection.rs:461`.
- Untagged responses can arrive in any order between command tag and
  tagged OK.
- Both quoted-string and literal-string forms are accepted on the
  wire; the mock will use quoted strings for everything that fits and
  literals only when forced (CR/LF, NUL, > 1024 bytes).
- All lines end `\r\n`.
- NAMESPACE is not parsed by imap_proto; the client extracts the raw
  line and walks it manually (`connection.rs:514-643`). Not relevant
  in v0 because we do not advertise NAMESPACE.

## Mutation surface (v0)

`UID STORE`, `UID COPY`, `UID EXPUNGE` are persistent and bump
`Fixture::state` so the change shows up in the JMAP `Email/changes`
delta on the same fixture.

- `UID STORE <set> <flag-op> <flags>` mutates `Email::keywords`
  for every matched message. IMAP wire flags map to fixture
  keywords (`\Seen` -> `$seen`, `\Flagged` -> `$flagged`,
  `\Draft` -> `$draft`, `\Answered` -> `$answered`, `\Deleted`
  -> `$deleted`; custom tokens pass through). `+FLAGS` /
  `-FLAGS` / `FLAGS` (replace) all supported, plus `.SILENT`.
  Each touched email is recorded as `email_updated` in the
  resulting transition.
- `UID COPY <set> <mailbox>` adds the target mailbox id to each
  matched email's `mailbox_ids[]`. The source mailbox keeps the
  email and its UID; the copy appears in the target with that
  mailbox's local sequence numbering. Unknown target -> `NO
  [TRYCREATE]`. COPYUID is omitted in v0.
- `UID EXPUNGE <set>` removes every matched email that carries
  `\Deleted` *from the current mailbox*. If the email no longer
  belongs to any mailbox after the operation, the email is
  destroyed entirely (`email_destroyed`); otherwise it survives
  and contributes `email_updated`. EXPUNGE responses fire in
  descending sequence-number order so no per-line renumbering is
  required.

## Out of scope for v0

- APPEND, MOVE, DELETE - destructive write paths not driven by
  ratatoskr's writeback flow.
- NOTIFY, COMPRESS - bandwidth / push-refinement optimisations
  (`IDLE` itself is implemented; see above).
- SETACL / DELETEACL / LISTRIGHTS - the ACL surface is read-only in
  v0 (`GETACL` / `MYRIGHTS` only; the *grants themselves* are
  fixture-authored and cannot be changed over the wire). Whether a
  shared folder accepts writes is a different question, and is driven
  by the granted rights - see "Shared folders" below.
- XLIST - client falls back to LIST + attributes.

## Shared folders (NAMESPACE / ACL, RFC 2342 + 4314)

Drives ratatoskr's shared-folder sync (A5c). A fixture `[[acl]]`
grant shares an owned mailbox with another declared account:

```toml
[[acl]]
mailbox_id = "mbx-bob-inbox"   # owned by some account
identifier = "account-alice"   # the account it is shared with
rights = "lr"                  # RFC 4314 rights; default "lr"
```

- `NAMESPACE` -> `* NAMESPACE (("" "/")) (("#user/" "/")) NIL`.
- `LIST "" "#user/*"` -> the grantee sees each shared mailbox as
  `#user/<owner-name>/<owner-path>` (e.g. `#user/bob@example.com/
  INBOX`) with the owner's role attributes. A bare `LIST "" "*"`
  stays personal-only.
- `MYRIGHTS <mailbox>` -> the authenticated account's rights: full
  `lrswipkxtea` on a personal mailbox it owns, the granted rights on
  a shared one. `NO` when the name doesn't resolve or the account
  holds no grant.
- `GETACL <mailbox>` -> `* ACL <mailbox> <owner> lrswipkxtea
  <grantee> <rights> ...` (owner first, always full rights, then each
  grant in declared order).
- `SELECT #user/<owner>/<path>` + `UID FETCH` / `UID SEARCH` /
  `IDLE` read the owner's messages while the connection stays
  authenticated as the borrowing account (per-selection account
  override). A shared folder the account holds no grant on returns
  `NO SELECT unknown mailbox`.
- Writes on a shared selection are gated on the granted rights, not
  blanket-refused. Each mutating command names the RFC 4314 right it
  needs - `UID STORE` -> `w`, `UID COPY` -> `i`, `UID MOVE` -> `i`+`t`,
  `UID EXPUNGE` -> `e` - and a selection whose grant lacks it gets
  `NO [NOPERM] <cmd> not permitted on this shared folder (requires
  "w", holds "lr")`. A grant of `lrswipkxte` therefore accepts the
  same writes a personal mailbox does. Personal selections are never
  gated (the owner holds `lrswipkxtea` implicitly).
- `SELECT` on a shared folder with no write-shaped right (none of
  `i` / `w` / `s` / `e` / `t`) completes `OK [READ-ONLY]`, so a client
  learns the folder is read-only without a separate `MYRIGHTS` round
  trip. The tagged verb still names the command that was issued
  (`SELECT`, not `EXAMINE`).
- `fixtures/shared-rights.toml` stages a read-only (`lr`) and a
  writable (`lrswipkxte`) shared folder in the same fixture, which is
  what a consumer needs in order to prove it distinguishes them
  rather than assuming one.

### Mid-session grant / revoke

Grants are not load-time-only. The change script's `acl_grant` /
`acl_revoke` ops (see `notes/fixture-format.md` "Incremental change
scripts") mutate the grant set through the same
`POST /test/fixture/step` path every other op uses, and every
shared-folder read path re-resolves grants from the live fixture on
each command. So on one connection, with no reconnect:

- an `acl_grant` makes a previously invisible mailbox appear in
  `LIST "" "#user/*"`, become selectable, and start reporting rights
  through `MYRIGHTS` / `GETACL`;
- an `acl_revoke` makes a previously visible one disappear from the
  listing and go back to `NO SELECT unknown mailbox`.

Both advance the grantee's state token as well as the owner's, so a
consumer polling as the grantee sees the change on its next sync the
same way an email or mailbox change would surface.
`fixtures/imap-acl-lifecycle.toml` stages both cases (grant first,
then revoke) against a live connection; `tests/acl_lifecycle.rs`
drives it.

## Things that WILL break sync if wrong

- Missing server greeting on initial connect.
- Missing `UID` field in any FETCH response item.
- Missing `UIDVALIDITY` in SELECT (defaults to 0; any later non-zero
  triggers mismatch).
- `CHANGEDSINCE` accepted when CONDSTORE was not advertised - the
  server error short-circuits the sync.
- `ENABLE QRESYNC` advertised but never echoed back as
  `* ENABLED QRESYNC` - client falls back, not fatal but slower and
  noisy in logs.
- A FETCH batch returning no messages when `EXISTS > 0` - client
  falls back to raw TCP recovery.

## Constants worth knowing

- TCP_CONNECT_TIMEOUT: 30s. `connection.rs:11`.
- IMAP_CMD_TIMEOUT: 30s (SELECT, STATUS, LIST, CAPABILITY).
- IMAP_FETCH_TIMEOUT: 120s (UID FETCH with bodies).
- IMAP_SEARCH_TIMEOUT: 60s.
- CHUNK_SIZE: 200 messages per FETCH batch. `sync_pipeline.rs:24`.
