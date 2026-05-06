# TODO

Plan for the next sessions. Tracks the suggested implementation order
from `notes/plan.md`, with the design decisions already worked out so
the next session can drop straight into code.

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

- A `brokkr.toml` with `project = "saehrimnir"` plus a `[[check]]`
  sweep needs `Project::Saehrimnir` to land in brokkr's enum first,
  or the file would fail brokkr's parse-time validation. Until then,
  rely on `brokkr check`'s no-toml fallback.
- A subprocess + reqwest test that exercises the sentinel + SIGTERM
  path end-to-end. The `tests/api.rs` suite covers the wire format
  via `tower::ServiceExt::oneshot`; a single subprocess-shaped test
  would close the gap currently filled only by `scripts/smoke.sh`.
