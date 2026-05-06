# TODO

Running task list. Per-protocol design notes live alongside in
`notes/`; this file just tracks what's next.

## Lua dynamic surface

Phase 2 callbacks (`on(protocol, command, fn)`) are wired across all
five protocols, mapped via `Override::Tagged { status, message }`.
What's left on the Lua side:

- `wait(ms)` Lua helper for latency injection inside callbacks.
  Implementation is `std::thread::sleep` from a RustFunc - fine
  inside a callback because the dispatch already holds the
  `Mutex<State>` and runs synchronously on whichever tokio worker
  the protocol handler landed on. Multiple connections each get
  their own dispatch lock turn, so a long sleep on one connection
  doesn't stall the others' protocol handling (they just queue
  briefly on the dispatcher mutex).
- `mock_done()` / `mock_fail("reason")` for self-terminating
  scripts. Calling either signals the runtime to exit cleanly
  (code 0) or with a reported failure (non-zero, message to
  stderr). Lets brokkr observe scenario success/failure via exit
  code instead of polling.
- Pushing structured request data to the `req` table. Currently we
  push only flat strings/ints; a fixture wanting to react to
  `Email/get`'s `ids` array needs us to push a Lua table from a
  `Vec<String>`. Small helper, blocked on no test forcing it yet.
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

## IMAP

- Streaming `UID FETCH` to avoid materialising the full response
  set before writing. Today's loop builds a `Vec<String>` first;
  at huge N (think `bulk_emails(count=1_000_000)`) that's enough
  memory to matter. Refactor when a fixture forces it.
- 200-message FETCH batching boundary. The current handler emits
  all matched FETCH responses in one go. Ratatoskr's client
  batches client-side (`CHUNK_SIZE = 200`), so the wire boundary
  is invisible to it; flagged as a v1 thing if we ever want to
  test the batch boundary explicitly.

## Microsoft Graph

v0 mail-sync surface is complete. Future Graph work, in roughly the
order the next fixture is likely to need it:

- Calendar sync (`<ratatoskr>/crates/graph/src/calendar_sync.rs`).
  Will need `[[calendar]]` and `[[event]]` fixture entries.
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

## Gmail

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

## Fixture format growth

Each item below requires both fixture-side schema work and at
least one protocol layer's projection layer to consume it. Mostly
unblocked - we just haven't needed them yet.

- `body_html` parallel to `body_text`. Reserved in
  `notes/fixture-format.md`; not implemented. Pressing once IMAP
  needs to render an HTML wire body or Graph wants HTML rendering.
- Multipart MIME via `body_path`. Same constraint - IMAP forces
  this when a fixture grows attachments because `BODY[]` must emit
  a real multipart/mixed.
- Attachments. Need fixture-side `[[email.attachment]]` (or Lua
  builder), then projection to JMAP `attachments[]`, IMAP
  `BODYSTRUCTURE` + `BODY[]` parts, Graph `attachments`, Gmail
  parts under the payload tree.
- Multi-account. v0 enforces `is_personal = true` and exactly one
  account. Lifting requires per-protocol tweaks to surface multiple
  accounts (JMAP session resource, Graph `/users/{id}/...` paths,
  IMAP per-connection account context).
- Incremental sync change scripts. `[[change]]` entries (or a Lua
  equivalent) that advance state tokens between phases - JMAP
  state, IMAP UIDVALIDITY/HIGHESTMODSEQ bumps, Graph deltatokens,
  Gmail historyId. Out of scope until every happy path lands.
- `bulk_mailboxes` builder for hierarchical folder generation.
  Complements `bulk_emails` / `bulk_threads`. Useful for
  exercising client-side folder-tree logic at scale.

## Cosmetic and housekeeping

- A `brokkr.toml` with `project = "saehrimnir"` plus a `[[check]]`
  sweep needs `Project::Saehrimnir` to land in brokkr's enum first,
  or the file would fail brokkr's parse-time validation. Until then,
  rely on `brokkr check`'s no-toml fallback.
- `dellingr = { path = "../dellingr" }` in Cargo.toml. Flip back to
  a versioned dep once dellingr 0.3 (Anchor) ships to crates.io.
- Plan-3 / ratatoskr wiring. From saehrimnir's side this just needs
  jmap-client + ratatoskr's IMAP/Graph/Gmail/SMTP clients to talk
  to us cleanly. Behaviours worth re-verifying when plan-3 lights
  up: whether jmap-client follows a relative `apiUrl`, whether
  ratatoskr's IMAP client tolerates our exact greeting/CAPABILITY
  ordering, whether Gmail's q-parser mismatch (we only honour
  `after:YYYY/M/D`) trips any internal-sync code path.
