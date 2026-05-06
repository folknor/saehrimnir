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
- v0 mock: accept any credentials, any method. Respond with `OK` and
  forget the username/password. No validation required.
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
- Optional but recognized: `NAMESPACE`, `MYRIGHTS`, `MOVE`. Used if
  present, graceful fallback otherwise.

NOT advertised in v0:

- `IDLE`, `NOTIFY`, `COMPRESS`, `STARTTLS`, `APPEND` (write paths),
  `XLIST` (Google extension; client falls back to LIST + attributes).
- `SPECIAL-USE` is fine to advertise or omit; only the per-folder
  attribute flags matter (`\Sent`, `\Trash`, etc.) and those are
  emitted on LIST regardless.

## Folder listing

- Command: `LIST "" "*"` (list all). `client/mod.rs:43`.
- Shared folders use `LIST "" "{prefix}*"` per namespace
  (`client/mod.rs:134`); v0 does not need to satisfy this since we do
  not advertise NAMESPACE.
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
  SEARCH. `client/sync.rs:34`.
- Untagged responses parsed, in any order, until the tagged OK:
  - `* <n> EXISTS` - total messages.
  - `* OK [UIDVALIDITY <u32>]` - folder identity. Cached per-folder;
    mismatch on resync triggers full refetch
    (`sync_pipeline.rs:487-503`).
  - `* OK [UIDNEXT <u32>]` - predicted next UID.
  - `* <n> RECENT` - parsed by async_imap, not actively used.
  - `* OK [HIGHESTMODSEQ <u64>]` - required for CONDSTORE/QRESYNC
    paths. Cached per-folder.
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

## Message fetching - delta / CONDSTORE

- New messages: `UID SEARCH (last_uid+1):*` to find new UIDs, then the
  same FETCH as initial. `client/sync.rs:111`,
  `client/commands.rs:9-33`.
- Flag changes (CONDSTORE): `UID FETCH 1:* (FLAGS) (CHANGEDSINCE
  <modseq>)`. Returns only flags for messages changed since cached
  HIGHESTMODSEQ. `client/commands.rs:293-320`.
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

These must remain stable across runs. v0 mock pins UIDVALIDITY to 1
and HIGHESTMODSEQ to 1 (it never changes), so resync just sees the
same values.

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

## Out of scope for v0

- APPEND, COPY, MOVE, DELETE, EXPUNGE - write paths not exercised by
  initial sync.
- IDLE, NOTIFY, COMPRESS - push and bandwidth optimisations.
- ACL / MYRIGHTS - attempted but failures are soft
  (`connection.rs:712-755`).
- NAMESPACE / shared folders - personal-mailbox-only is enough.
- XLIST - client falls back to LIST + attributes.

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
