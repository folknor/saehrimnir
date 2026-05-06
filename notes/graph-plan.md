# Graph mock - implementation plan

Companion to `notes/ratatoskr-graph-surface.md`. Microsoft Graph is
HTTP+JSON like JMAP, so we lean on the existing axum infrastructure.
This plan covers v0 (mail-sync only) but the module layout is chosen
so the larger Graph surface (calendars, contacts, OneDrive, groups,
EWS, ...) drops in without restructuring.

## Goal

Ratatoskr's Graph mail-sync code, pointed at the Graph listener's
URL, completes an initial sync against the same fixture used by JMAP
and IMAP. Everything reuses the existing `Fixture` types; no fixture
format changes for v0.

## Out of scope (v0)

- Anything outside mail. Calendar/contacts/drive/groups/EWS/etc.
  return a Graph-shaped 404. The module structure leaves room for
  them as sibling files.
- Token validation. Any bearer is accepted; never returns 401.
- Rate-limit simulation (no 429s).
- TLS. Plaintext only; ratatoskr's account config will point at
  `http://127.0.0.1:<port>/v1.0`.
- Attachment bodies. Fixtures don't carry attachments yet; the
  `attachments` array is always empty.
- Webhook subscriptions, OneDrive upload sessions, autodiscover.

## Architecture

```
src/graph/
  mod.rs       - public router builder, AppState, error envelope,
                 the catchall 404 handler. Re-exports per-resource
                 routers so future `calendar.rs` / `contacts.rs` /
                 `drive.rs` can register theirs without changing
                 mod.rs's surface.
  odata.rs     - OData query-parameter parsing ($top, $skip,
                 $filter, $select, $expand, $orderby, $count),
                 ODataCollection envelope, pagination cursor
                 (opaque base64 of `{kind, offset}`),
                 absolute-URL builder using the request's Host
                 header.
  mail.rs      - mail-sync handlers: mailFolders list / by-id /
                 well-known-alias resolve, childFolders list,
                 messages list with $filter+pagination+$expand,
                 messages/delta bootstrap and follow.
```

Listener: separate `--graph-port`, separate axum::serve task in
`main.rs`, mirroring the JMAP listener wiring. Sentinel grows a
fourth line `GRAPH <port>`.

## Fixture mapping

No fixture format changes for v0. The projection layer in
`graph::mail`:

- Fixture mailbox `id` IS the Graph folder `id`. Opaque to the
  client, so any string works.
- Fixture mailbox `name` -> Graph `displayName`. Inbox case is
  preserved verbatim (in contrast to IMAP's "INBOX" canonicalisation).
- Fixture `parent_id` -> `parentFolderId`.
- Fixture `role` -> Graph `wellKnownName` via the table in the
  surface doc.
- Fixture mailbox membership counts -> `totalItemCount`,
  `unreadItemCount`, `childFolderCount`.

For messages:

- Fixture email `id` -> Graph message `id`.
- Fixture `thread_id` -> `conversationId` (defaults to `id` if
  missing).
- Fixture `keywords` containing `$seen` -> `isRead: true`.
- Fixture `keywords` containing `$flagged` ->
  `flag: {flagStatus: "flagged"}`. No `$flagged` ->
  `flag: {flagStatus: "notFlagged"}`.
- Custom (non-`$`-prefixed) keywords -> `categories[]`.
- Fixture `body_text` -> `body: {contentType: "text", content: ...}`.
- Fixture `received_at`, `sent_at` -> ISO 8601 in UTC with `Z`.
- Fixture message-id / in-reply-to / references ->
  `internetMessageHeaders` entries; the first `message_id` also
  populates the standalone `internetMessageId`.

## Pagination model

`$top` defaults to 50 for messages, 250 for folders (matching the
client's defaults). `$skip` shifts the offset.

When the result set is larger than `$top`, the response includes
`@odata.nextLink` pointing at the same path with
`$skiptoken=<opaque>`. The opaque token is just `offset=N`
base64-encoded; the client never inspects it.

Delta endpoints emit `@odata.deltaLink` with `$deltatoken=<opaque>`.
Following the deltaLink in v0 always returns an empty `value[]` and
the same deltaLink token, since the fixture is read-only.

## Determinism contract

- Folders enumerated in fixture declaration order.
- Messages enumerated by `receivedDateTime` desc with id-lex
  tiebreak (matching JMAP's `Email/query` sort).
- All timestamps formatted as `%Y-%m-%dT%H:%M:%SZ` (UTC).
- Pagination tokens deterministic for a given (path, query, offset).

## Suggested implementation order

1. Bootstrap. `--graph-port`, sentinel line, axum::serve task,
   skeleton router with the catchall 404 returning a Graph-shaped
   error. Folder list endpoint returning the canonical fixture.
2. Folder by-id and well-known-alias. Child-folder list.
3. Messages list with `$filter`, `$top`, `$skip`, `$orderby`,
   pagination via `@odata.nextLink`.
4. Messages delta endpoint. Initial dump with `@odata.deltaLink` at
   the end; subsequent deltaLink follow returns empty + same link.
5. Integration tests in `tests/graph.rs` driving the router via
   `tower::ServiceExt::oneshot` (same shape as `tests/api.rs`).

## Open questions

- **Multi-account / `/users/{id}` paths.** Shared mailbox sync uses
  `/v1.0/users/{id}/mailFolders/...` (`shared_mailbox_sync.rs`).
  For v0, mount the same handlers at both `/v1.0/me/...` and
  `/v1.0/users/{id}/...` once we grow multi-account fixtures.
- **`/beta` vs `/v1.0`.** A few flows hit `/beta`. v0 implements
  `/v1.0` only; if a fixture forces a `/beta` call we add the
  prefix later.
- **EWS.** Public-folder sync goes through SOAP-over-HTTP at
  `outlook.office365.com/EWS/Exchange.asmx`. Different protocol,
  different surface; will land as `src/ews.rs` when needed.
- **Reactions.** The `singleValueExtendedProperties` path is
  Exchange-specific. v0 emits an empty array; honouring fixture
  reactions is a v1 concern.
