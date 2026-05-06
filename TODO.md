# TODO

Plan for the next sessions. Tracks the suggested implementation order
from `notes/plan.md`, with the design decisions already worked out so
the next session can drop straight into code.

## Step 5: `/jmap/api` envelope + `Mailbox/get`

Highest-leverage next step. Once the envelope works, steps 6 and 7 are
just additional match arms in the dispatcher.

File layout: new `src/jmap.rs` for envelope types + dispatcher +
per-method handlers. Keep `routes.rs` thin (just routing).

Request envelope (RFC 8620 section 3.3):

```rust
#[derive(Deserialize)]
struct JmapRequest {
    using: Vec<String>,
    #[serde(rename = "methodCalls")]
    method_calls: Vec<MethodCall>,
    #[serde(rename = "createdIds", default)]
    created_ids: Option<serde_json::Value>,
}

type MethodCall = (String, serde_json::Value, String);
```

Response envelope (RFC 8620 section 3.4):

```rust
#[derive(Serialize)]
struct JmapResponse {
    #[serde(rename = "methodResponses")]
    method_responses: Vec<MethodResponse>,
    #[serde(rename = "sessionState")]
    session_state: String,
    #[serde(rename = "createdIds", skip_serializing_if = "Option::is_none")]
    created_ids: Option<serde_json::Value>,
}

type MethodResponse = (String, serde_json::Value, String);
```

`session_state` mirrors `fixture.state`. Tuples serialize as arrays.

Dispatcher:

```rust
fn dispatch(fixture: &Fixture, name: &str, args: &Value) -> (String, Value) {
    match name {
        "Mailbox/get" => match mailbox_get(fixture, args) {
            Ok(v) => (name.to_string(), v),
            Err(err) => ("error".to_string(), err),
        },
        // step 6: "Email/query" => ...
        // step 7: "Email/get"   => ...
        _ => ("error".to_string(), json!({"type": "unknownMethod"})),
    }
}
```

Errors per RFC section 3.5.2: `{"type": "<code>", "description": "..."}`.
Standard codes we will emit: `unknownMethod`, `accountNotFound`,
`invalidArguments`.

`Mailbox/get` (RFC 8621 section 2.1):

Request args:
- `accountId: String`. Must match `fixture.account.id` else
  `accountNotFound`.
- `ids: [String] | null`. Null/missing = all; otherwise filter,
  unknown ids land in `notFound`.
- `properties: [String] | null`. Ignore for v0; return all fields.
  Ratatoskr does not pass this for `Mailbox/get`.

Response args: `accountId`, `state` (= `fixture.state`), `list`,
`notFound`.

Per-mailbox JSON shape (`notes/ratatoskr-client-surface.md` is the
source of truth for which fields are read):

```json
{
  "id": "...", "name": "...",
  "parentId": null,
  "role": "inbox",
  "sortOrder": 0,
  "totalEmails": 0, "unreadEmails": 0,
  "totalThreads": 0, "unreadThreads": 0,
  "myRights": {
    "mayReadItems": true, "mayAddItems": true, "mayRemoveItems": true,
    "maySetSeen": true,   "maySetKeywords": true,
    "mayCreateChild": true, "mayRename": true,
    "mayDelete": true,    "maySubmit": true
  },
  "isSubscribed": true
}
```

Compute counts from the fixture: `totalEmails` = count of emails where
mailbox id is in `mailbox_ids`; `unreadEmails` = same minus those whose
`keywords` contains `"$seen"`. Threads = unique `thread_id` over the
email set. Returning zeros also works (the parser does not read these),
but computing is cheap.

Route: `POST /jmap/api`, content-type `application/json`. Return 200
always (errors land inside the response envelope, not as HTTP statuses).
On JSON parse failure axum will 400 automatically; that is fine.

## Step 6: `Email/query`

Request args (RFC 8621 section 4.4):
- `accountId`.
- `filter`. For v0 accept `{"after": <unix_seconds>}` (received_at >=
  ts) and `{"inMailbox": <id>}` (used by ratatoskr's helpers for
  thread-scoped lookups, `crates/jmap/src/helpers.rs:11-21`).
- `sort`. `[{"property": "receivedAt"}]`. Direction defaults
  ascending in the wire format; for determinism sort descending by
  receivedAt with ties broken by `id` lex (per
  `notes/fixture-format.md` determinism contract).
- `position`. Int (default 0). Only non-negative values from the
  client.
- `limit`. Int (default 50, ratatoskr's `BATCH_SIZE`). Cap server-side
  at 256.
- `calculateTotal`. Bool (default false). True only on first page.

Response args: `accountId`, `queryState` (any stable string;
`fixture.state` reused is fine), `canCalculateChanges: false` (v0),
`position` (echo request), `ids: [String]`, `total: u64` (only when
`calculateTotal: true`).

Loop-termination contract (`ratatoskr-client-surface.md`): client loops
with `position += 50` until a page returns fewer than 50 ids. Do not
return exactly 50 on the last page or the loop re-queries forever.
Easy: `let last = (start + limit).min(total);` and slice.

## Step 7: `Email/get`

Request args (RFC 8621 section 4.2): `accountId`, `ids: [String]`,
`properties: [String] | null`, `bodyProperties: [String] | null`,
`fetchTextBodyValues: bool`, `fetchHtmlBodyValues: bool`,
`fetchAllBodyValues: bool`.

Response args: `accountId`, `state`, `list`, `notFound`.

Per-email JSON: full RFC 8621 section 4.1 shape. Source-of-truth for
what the client actually reads is the property list at
`crates/jmap/src/parse.rs:35-63` and the parser at `:72-197`. Mock
should always return:

- `id`, `blobId` (derive `"blob-<email-id>"`; never read by the
  parser for our path but RFC requires it), `threadId`, `size`,
  `receivedAt`, `sentAt`.
- `mailboxIds`. Object `{<id>: true}`. Map, not array.
- `keywords`. Object `{<kw>: true}`. Map.
- `messageId`, `inReplyTo`, `references`. Arrays of strings or null
  if empty.
- `from`, `to`, `cc`, `bcc`, `replyTo`. Array of `{name, email}`
  (name may be null). `from` is an array on the wire even though the
  parser reads `.first()`.
- `subject`, `preview`, `hasAttachment`.
- `textBody`, `htmlBody`. Arrays of `EmailBodyPart`. For `body_text`
  fixtures emit one part `{partId, blobId, type: "text/plain", size,
  charset: "utf-8", disposition: null, language: null, location: null,
  subParts: null, headers: [], name: null, cid: null}`. `partId`
  derivation: `"<email-id>:text"`. `htmlBody` empty array.
- `bodyValues`. Map keyed by `partId`, value `{value,
  isEncodingProblem: false, isTruncated: false}`. Only emit when
  `fetchTextBodyValues` / `fetchHtmlBodyValues` was true.
- `attachments`. Array of `EmailBodyPart`. Empty for v0 (no
  attachment body sources).
- Custom-header property keys requested by ratatoskr
  (`header:List-Unsubscribe:asText`,
  `header:List-Unsubscribe-Post:asText`,
  `header:Disposition-Notification-To:asText`). Always emit them;
  `null` is fine. Source: `parse.rs:59-63`.

When the client sends `Email/get` with `ids: []`, return `state` and
empty list. Mandatory: `get_email_state` calls this purely for the
state token (`sync/mod.rs:236-258`).

## Step 8: Integration test

Two viable shapes:

1. Pure axum: use `tower::ServiceExt::oneshot` to fire requests
   directly at the Router without binding a port. Fastest, no
   subprocess. Lives in `tests/api.rs`. Need a `lib.rs` to expose
   `routes::router` and `fixture::load`. Minor refactor: split
   `main.rs` into `main.rs` (just the runtime) plus `lib.rs`
   (everything else as `pub mod`s).
2. Subprocess + reqwest: spawn the binary, wait for sentinel, hit it
   with reqwest. Closer to production but slower and trickier to
   clean up.

Lean toward option 1 for unit-test-speed coverage of the wire format,
plus a single option-2 shaped test that exercises sentinel + SIGTERM.

## Step 9: Wire to ratatoskr

This is plan-3 work in brokkr. From sæhrimnir's side, we need to
verify nothing about our wire protocol surprises jmap-client. Concrete
checks before plan-3 lights up:

- Does jmap-client follow `apiUrl: "/jmap/api"` (relative) when the
  session URL was absolute? If not, switch to absolute URLs computed
  from the request's Host header.
- Does jmap-client tolerate `/.well-known/jmap` returning the session
  body directly, or does it require a 301 to `/jmap/session`? If the
  latter, swap the well-known route to a redirect.

These are small observable behaviours; once the integration test
binary in plan 3 runs, divergences will be visible.

## Open questions still pending

- HTML-only bodies. Add `body_html` to the fixture format as a
  parallel option to `body_text`? Reserved in fixture-format.md but
  not implemented.
- Multipart MIME via `body_path`. Deferred until a fixture needs it.
- Multi-account fixtures. v0 enforces `is_personal = true` and
  exactly one account. Lifting requires the session resource to emit
  multiple accounts and `Mailbox/get` to honour `accountId` against
  any of them.
- Failure injection. Plan 2 reserves `[fault]` blocks for v1. Slow
  responses, retryable errors, `cannotCalculateChanges` would go
  here.
- Incremental sync. `Email/changes`, `Mailbox/changes`, `[[change]]`
  fixture entries advancing state tokens. Out of scope until the
  happy path proves the orchestration story.

## Cosmetic and housekeeping

- The `#![allow(dead_code)]` in `src/fixture.rs` should come off when
  step 7 lands. Every field will have a consumer by then.
- A `brokkr.toml` with `project = "saehrimnir"` plus a `[[check]]`
  sweep needs `Project::Saehrimnir` to land in brokkr's enum first,
  or the file would fail brokkr's parse-time validation. Until then,
  rely on `brokkr check`'s no-toml fallback.
- `scripts/smoke.sh` writes its readiness file under `mktemp -d`,
  which lands in `/tmp`. CLAUDE.md bash rules say data lives in the
  project; move the tmpdir to a `.smoke/` directory under the repo
  root next time the script gets touched.
