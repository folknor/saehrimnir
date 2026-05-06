# jmap-client (the local fork)

A pointer doc. The mock doesn't depend on this crate; we hand-roll the
JSON wire format. But the fork is the source of truth for what
ratatoskr's client expects on the wire, so it's the second-best
reference after RFC 8620/8621.

## Where it lives

`/home/folk/Programs/jmap-client` — sibling to `ratatoskr/` and this
repo. Used by `<ratatoskr>/crates/jmap/` as a path dependency.

## Provenance

- Forked from Stalwart Labs' `jmap-client` crate.
- Authors line: `Stalwart Labs LLC <hello@stalw.art>` and
  `folk@folk.wtf`.
- License: Apache-2.0 OR MIT.
- Repository: `https://github.com/folknor/jmap-client` (per Cargo.toml;
  not necessarily the active development location).

## Version pinned

- `version = "0.5.0"`
- `edition = "2024"`
- `rust-version = "1.92"`

## Default features

```
default = ["tls-rustls", "websockets", "mail", "calendars", "contacts",
           "blob", "quota"]
```

Notable things ratatoskr inherits by default:

- `mail` — `Email`, `Mailbox`, `Thread`, `EmailSubmission`.
- `calendars`, `contacts`, `blob`, `quota` — the wider mail-adjacent
  surface. None are exercised by `jmap_initial_sync`.
- `websockets` — present but ratatoskr's sync code is HTTP-only.

The mock can ignore everything outside `mail` for v0.

## Module layout (src/)

The directories under `jmap-client/src/` map roughly 1:1 to JMAP type
families. Reading order if we ever need to verify wire shapes by hand:

- `core/` — capabilities, errors, query (`Filter`, `Comparator`),
  request/response framing, session.
- `email/` — `Email`, `EmailGet`, `EmailQuery`, `EmailChanges`,
  `EmailSet`, `EmailBodyPart`, `HeaderValue`, `Property` enum.
- `mailbox/` — `Mailbox`, `MailboxGet`, `MailboxRights`, `Role`.
- `thread/`, `blob/`, `quota/`, `identity/`, `address_book/`,
  `calendar/`, `calendar_event/`, `contact_card/`, `principal/`,
  `share_notification/`, `email_submission/`, `participant_identity/`,
  `push_subscription/`, `sieve/`, `vacation_response/`,
  `event_source/` — out of scope for v0.
- `client.rs`, `client_ws.rs`, `transport_reqwest.rs` — HTTP/WS
  transport. Reqwest under the hood; not relevant to the mock.
- `tests.rs` — integration-style tests. Useful as a wire-format
  reference if a hand-rolled JSON shape ever drifts from what the
  client deserializes.

## API surface ratatoskr actually uses

A non-exhaustive list, drawn from `crates/jmap/src/`:

- `Client::new() -> ClientBuilder`
- `ClientBuilder::credentials(Credentials)`,
  `Credentials::basic(user, pass)`, `Credentials::bearer(token)`
- `ClientBuilder::connect(&jmap_url) -> Client` — sends the session
  request.
- `Client::session()` -> session resource accessors:
  `accounts()`, `account(id)`, `has_capability(uri)`,
  `principals_capabilities()`.
- `Client::build()` -> `Request` builder, then
  `request.call(MethodCall)` returns a handle, `request.send().await`
  returns `Response`, `response.get(&handle)` extracts the typed
  result.
- Mail-side method calls used:
  - `MailboxGet::new(&account_id)` (no ids = list all)
  - `EmailQuery::new(&account_id)` with `.filter(...)`, `.sort(...)`,
    `.position(...)`, `.limit(...)`, `.calculate_total(true)`
  - `EmailGet::new(&account_id)` with `.ids(...)`, `.properties(...)`,
    `.arguments().fetch_text_body_values(true)`,
    `.arguments().fetch_html_body_values(true)`
- Convenience entry points called directly off `Client`:
  - `email_query(filter, sort).await` (used by `query_thread_email_ids`)
  - `email_changes(state, max).await` (out of v0 scope)
  - `mailbox_changes(state, max).await` (out of v0 scope)
  - `share_notification_changes(state, max)`,
    `share_notification_get(id, props)`,
    `share_notification_destroy(id)` (gated on principals capability;
    v0 mock won't trigger by not advertising it)

## Why the mock doesn't depend on this crate

- Closing the loop with the client lib would have the test peer using
  the same code under test as the client. The whole point is to be an
  independent reference.
- `jmap-client` brings in reqwest, tokio TLS, websockets, and the
  full type hierarchy for capabilities the mock doesn't implement.
  Hand-rolled `serde` types are smaller, faster to compile, and
  document our assumed wire shape inline.
- Stalwart's mainline crate may diverge from the fork; pinning to the
  fork would couple the mock to ratatoskr's checkout layout.

## When to read this crate's source vs the RFCs

- **Read jmap-client for:** field naming when the RFC is ambiguous
  (e.g. `myRights` vs `my_rights` over the wire — JMAP spec says
  camelCase, but verifying the serde `rename_all` is fastest in the
  source); whether optional fields are serialized as `null` vs
  omitted; the exact strings used in custom-header property keys
  (`header:Foo:asText`).
- **Read the RFCs for:** semantics, error response shapes, edge
  cases (`cannotCalculateChanges`, `requestTooLarge`, etc.), and
  anything the client doesn't directly observe (server-side
  invariants).
