# Gmail mock - implementation plan

Companion to `notes/ratatoskr-gmail-surface.md`. Gmail is HTTP+JSON
like JMAP and Graph; we lean on the existing axum infrastructure.
Mail-sync surface only for v0; module layout leaves room for
contacts (People API), Drive uploads, and Calendar.

## Goal

Ratatoskr's Gmail mail-sync code path, pointed at the Gmail
listener's URL, completes an initial sync against the same fixture
used by JMAP / IMAP / SMTP / Graph. Reuses the existing `Fixture`
types - no fixture format changes for v0.

## Out of scope (v0)

- People API contacts. People API base URL is a different host;
  separate listener if/when fixtures grow contacts.
- Drive uploads. Same reasoning.
- Calendar - lives in a separate runtime in ratatoskr; not part of
  Gmail mail-sync.
- SendAs / signatures bidirectional sync. v0 emits an empty
  `sendAs[]`.
- Attachment download endpoints. v0 fixtures have no attachments.
- TLS, token validation, rate limiting.

## Architecture

```
src/gmail/
  mod.rs       - public router builder, AppState, error envelope,
                 catchall 404. Sibling files (contacts.rs,
                 drive.rs) plug in here without restructuring.
  mail.rs      - profile, labels, threads (list and detail),
                 messages.attachments, history. The MIME-payload
                 builder for projecting fixture emails into Gmail's
                 nested mimePart shape lives here.
```

Listener: separate `--gmail-port`. Sentinel grows a fifth line
`GMAIL <port>`.

## Fixture mapping

No fixture format changes. Projection from existing types:

- Fixture mailbox `id` -> if the mailbox has a recognised role, the
  emails it contains carry the system label id (`INBOX`, `SENT`,
  ...). Otherwise the mailbox becomes a user-defined Gmail label
  with id `Label_<fixture-id>` and name = fixture mailbox name.
- Fixture mailbox `archive` role: Gmail "archive" is the absence of
  `INBOX`, not a separate label. Archive-role emails get no
  container system label.
- Fixture email `id` -> Gmail `id`.
- Fixture `thread_id` -> Gmail `threadId`.
- Fixture `keywords`:
  - `$seen` absent -> add `UNREAD` to `labelIds`.
  - `$flagged` -> add `STARRED`.
  - `$draft` -> add `DRAFT`.
  - non-`$`-prefixed keywords -> add `Label_<keyword>`.
- Fixture `body_text` -> single `text/plain` leaf in the payload
  tree, base64url-encoded with no padding.
- Fixture headers (From / To / Cc / Bcc / Reply-To / Subject /
  Message-Id / In-Reply-To / References / Date) -> `headers[]` on
  the leaf part.
- Fixture `received_at` -> `internalDate` as Unix milliseconds quoted
  string.

`historyId`: pinned at `1` for the lifetime of a fixture (matches
JMAP state token and IMAP HIGHESTMODSEQ pin).

## Pagination

`nextPageToken` cursors as short ASCII (`t.<offset>`), opaque to the
client. With small fixtures we never paginate, but the path emits
proper cursors when `maxResults` < total.

## Determinism contract

- Threads enumerated by latest `received_at` desc, ties by
  `thread_id` lex.
- Messages within a thread enumerated by `received_at` asc (oldest
  first - Gmail convention).
- All timestamps as Unix milliseconds (string-quoted).
- `historyId` always `"1"`.

## Suggested implementation order

1. Bootstrap. `--gmail-port`, sentinel line, `src/gmail/mod.rs` with
   the catchall 404, profile, and labels endpoints returning
   fixture-projected data.
2. Threads (list + detail). MIME payload projection from the
   fixture's `body_text`.
3. History endpoint. Returns empty + the same `historyId` for any
   `startHistoryId`, since fixtures are read-only.
4. Integration tests in `tests/gmail.rs`.

## Open questions

- **Contacts sync.** ratatoskr's mail-sync code triggers a contacts
  sync on the 20th delta cycle. The first time a fixture wants to
  exercise that, we'll need either a `contacts.rs` sibling here or a
  separate `src/people.rs` for the People API host. Lean toward
  `src/people.rs` because People API has a distinct base URL.
- **`attachmentId` semantics.** v0 always returns 404 on attachment
  download. Once a fixture grows attachments, the attachmentId
  needs to round-trip and the body needs to carry base64url data.
- **Multi-account.** Same as Graph: shared mailbox is `users/{id}/`
  in path. v0 fixtures are single-account.
