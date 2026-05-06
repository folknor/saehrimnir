# Mock JMAP server (preliminary) - planning note

Status: **preliminary planning.** The standalone project doesn't exist
yet. Norse-mythology name TBD. This note lives in brokkr's `notes/`
provisionally and moves to the new repo once it's bootstrapped.

Companion to `notes/ratatoskr-service-harness.md` (plan 1) and
`notes/ratatoskr-sync-orchestration.md` (plan 3). This is plan 2.

## Background for reviewers

Throughout this document, `<ratatoskr>` refers to the root of the
ratatoskr repository (typically `~/Programs/ratatoskr` on the
author's machine, sibling to brokkr's checkout). All other paths
are relative to brokkr's repo (or, post-bootstrap, the mock-server
repo).

**Ratatoskr** is a Rust desktop email client (an `iced` UI process
plus a child Service worker that owns all writes - sync, DB,
body/inline/blob stores, Tantivy indexing). Its sync code talks to
real JMAP and IMAP providers; testing that code in isolation needs a
deterministic peer it can talk to instead of the public internet.
This project is that peer.

**Brokkr** is a single-binary Rust dev tool that already orchestrates
several sibling projects (`pbfhogg`, `elivagar`, `nidhogg`, etc.) -
builds, runs, benchmarks, retains artefacts, stores results in
`.brokkr/results.db`. This project (the mock server) is one more
binary brokkr will spawn as a child process during ratatoskr's sync
tests; brokkr's `[ratatoskr]` commands (in plan 3) start it on a
configured port, point ratatoskr's Service at it, drive a workload,
and tear it down.

**JMAP** (RFC 8620 core + RFC 8621 mail) is a JSON-over-HTTP
replacement for IMAP. JSON request/response, well-defined type
hierarchy, far smaller protocol surface than IMAP. Ratatoskr's JMAP
client lives at `<ratatoskr>/crates/jmap/` and uses a local fork of
`jmap-client` at `/home/folk/Programs/jmap-client`.

**Three-plan architecture this note lives inside.** The work needed
to test ratatoskr's sync code splits into three independent pieces:

| | Owns |
| --- | --- |
| Plan 1 (split: ratatoskr `app` crate + brokkr) | Deterministic Service-test harness. Ratatoskr's `app` crate hosts the Lua VM (`dellingr`), `ServiceClient` userdata bindings, the wait combinator, the frame-log tap, and the artefact-dir writers - exposed via `app --test-harness <script.lua>`. Brokkr provides build orchestration, the artefact-dir lifecycle, and low-level primitives (signal, pid_is_alive, sentinel watch, /proc snapshot) for orchestrator-side hang cleanup. Brokkr does not depend on ratatoskr or embed dellingr. See `notes/ratatoskr-service-harness.md` for the full design. |
| Plan 2 (this note; eventually a standalone repo) | The mock JMAP/IMAP server: protocol, fixture model, deterministic responses. Independent of brokkr and ratatoskr at the source level. |
| Plan 3 (in brokkr) | Commands that spawn plan 2's binary and ratatoskr together, drive a workload, collect metrics, store results. Ratatoskr's headless-sync surface (a Service IPC method, a small benchmark binary, or a Lua script via plan 1's `--test-harness`) is a plan-3 implementation detail; from plan 2's perspective it just sees JMAP traffic on the configured port. |

This project (plan 2) talks JMAP/HTTP outward and reads a fixture file
inward. It does not depend on brokkr or ratatoskr at the source level;
brokkr spawns it as a generic subprocess and points ratatoskr's
Service at the resulting endpoint via env var.

## Goal for v0

A small, deterministic JMAP server that ratatoskr can sync against
end-to-end. Just enough functionality for `brokkr sync-smoke
--fixture jmap-small` to drive an initial sync and have it succeed.

Not a complete JMAP implementation. Not a fuzz target. Not IMAP. Not a
benchmark fixture. Those are later increments.

## Why JMAP first

- JMAP is JSON over HTTP. IMAP is a stateful binary-ish protocol with
  decades of RFC accretion. The cost asymmetry is enormous.
- Ratatoskr already speaks JMAP via a local fork of `jmap-client` at
  `/home/folk/Programs/jmap-client`. The mock's wire format is fully
  spec'd (RFC 8620 core + RFC 8621 mail).
- A working JMAP mock is roughly the floor of useful test coverage for
  ratatoskr's sync code. IMAP can follow once the JMAP mock has proven
  the orchestration story.

## Scope of v0

A binary that:

- Listens on a TCP port (zero / ephemeral by default; explicit via
  CLI).
- Writes a readiness sentinel file when the listener is bound.
- Serves JMAP over HTTP on `/jmap/session` and `/jmap/api`.
- Loads exactly one fixture from a TOML file declaring mailboxes and
  messages.
- Implements the JMAP method subset ratatoskr's initial-sync code
  path actually calls (see below).
- Returns deterministic responses: stable IDs, stable sort order,
  stable timestamps from the fixture.
- Logs every received request and emitted response to stderr.
- Exits cleanly on SIGTERM.

That's it. No write methods, no push, no auth challenge, no incremental
changes-since-state, no submission, no WebSocket, no calendars,
contacts, or sieve.

## JMAP method subset for v0

From grepping `<ratatoskr>/crates/jmap/`, the load-bearing methods
during a fresh sync are:

- `GET /.well-known/jmap` -> redirect or session URL.
- `GET /jmap/session` -> session resource. Must advertise
  `urn:ietf:params:jmap:core` and `urn:ietf:params:jmap:mail` at
  minimum. Session lists accounts, primary account, API URL,
  download/upload/eventSource URLs (download/upload/eventSource can
  point to stub paths that 404 - v0 doesn't serve them).
- `POST /jmap/api` (JMAP method calls):
  - `Mailbox/get` - list mailboxes (id, name, parentId, role, total/
    unread counts, sort order).
  - `Email/query` - list email IDs in a mailbox, sorted, with paging.
  - `Email/get` - fetch full email properties + body parts by id.

Out of scope for v0 but listed here so we know what comes next:
`Email/changes`, `Mailbox/changes` (incremental sync), submission,
push, threading, search beyond basic `inMailbox` filter.

The session capabilities ratatoskr probes for (`submission`,
`websocket`, `sieve`) are deliberately absent in v0 - the JMAP code
already handles missing-capability paths gracefully.

## Authentication

v0: open. The HTTP listener accepts any request. No basic-auth
challenge, no bearer token validation. Ratatoskr's account config
will need a way to point at a no-auth endpoint - either a test-only
flag or accepting any credential against the mock. Coordinate with
the ratatoskr-side test-helper RPC surface.

This is a deliberate v0 simplification. Real auth (basic, OAuth) is
a v1+ concern and orthogonal to "does sync correctness work."

## Fixture format

TOML, loaded from a file path passed on the CLI. Sketch:

```toml
name = "jmap-small"

[account]
id = "account-1"
name = "test@example.com"
is_personal = true

[[mailbox]]
id = "mbx-inbox"
name = "Inbox"
role = "inbox"
sort_order = 0

[[mailbox]]
id = "mbx-archive"
name = "Archive"
role = "archive"
sort_order = 1

[[email]]
id = "email-001"
mailbox_ids = ["mbx-inbox"]
from = "alice@example.com"
to = ["bob@example.com"]
subject = "Hello"
received_at = "2025-01-15T10:00:00Z"
body_text = "First message body."

[[email]]
id = "email-002"
mailbox_ids = ["mbx-inbox"]
from = "carol@example.com"
to = ["bob@example.com"]
subject = "Re: Hello"
in_reply_to = "email-001"
received_at = "2025-01-15T11:00:00Z"
body_path = "messages/email-002.eml"
```

Two body sources: `body_text` for inline plain-text bodies (cheap,
generated), `body_path` for full RFC822 `.eml` files relative to the
fixture file (real headers, attachments, MIME parts when needed).

Format details (encoding, multipart, attachments, threading) refine
during implementation. The shape above is for plan-level shape only.

## CLI

```
mock-jmap [--port N] [--readiness-file PATH] [--fixture PATH] [--log-file PATH]
```

- `--port` - port to listen on. `0` (default) picks an ephemeral port;
  the chosen port is written to the readiness file and printed to
  stdout.
- `--readiness-file` - path to write `READY <port>` once the listener
  is bound. Brokkr's orchestrator watches this (via plan 1's
  `wait_for_sentinel` primitive) to know when to launch the
  ratatoskr-side process that will drive the workload.
- `--fixture` - path to the TOML fixture file. Required.
- `--log-file` - optional file path; if absent, logs go to stderr.

## Architecture

- Tokio runtime.
- HTTP framework: `axum` is the obvious pick - small surface, ergonomic
  routing, well-maintained. Alternatives (`hyper` direct, `warp`) on
  the table at implementation time.
- JMAP types: define our own minimal types via `serde`. v0 doesn't
  need the full RFC 8620 type hierarchy; just enough to round-trip the
  three method calls. Avoid pulling in `jmap-proto`-style crates until
  the type surface gets painful.
- Fixture loader: `serde` + `toml`, single pass at startup. Validate
  references (every email's `mailbox_ids` resolves; every `in_reply_to`
  resolves) and refuse to start on inconsistency.
- State: read-only after fixture load. No write paths means no locking.

Single binary, single crate. Workspace if/when fixtures grow into a
separate crate or a fuzzer is added.

## Determinism

All responses derive from the fixture. No clocks, no random IDs, no
unsorted iteration. Specifically:

- Email IDs / mailbox IDs / account IDs come straight from the
  fixture.
- `Email/query` sort orders are stable (sort by `receivedAt` desc,
  ties broken by `id` lexicographic).
- Timestamps are fixture-supplied; the server never reads system time
  for anything user-visible.
- Logs include monotonic request indices for replay.

## Repository / project layout

Standalone repo. Norse-mythology name (TBD; user's call). Single
crate, single binary. brokkr.toml at the root with
`project = "<name>"` so brokkr commands can be run from inside it
(e.g. `brokkr check`).

## Acceptance for v0

1. `cargo run -- --fixture fixtures/jmap-small.toml --port 0
   --readiness-file /tmp/ready` starts the server, writes
   `READY <port>` to the readiness file, listens.
2. A request to `GET /jmap/session` returns a valid session resource
   with at least core + mail capabilities and one account.
3. `Mailbox/get`, `Email/query`, `Email/get` respond per RFC 8621
   for the fixture content.
4. Ratatoskr's existing JMAP client code, pointed at the mock's
   endpoint, performs an initial sync and finishes without errors.
5. `brokkr sync-smoke --fixture jmap-small` (plan 3) passes
   end-to-end.
6. SIGTERM -> clean shutdown within 1s.

## Out of scope for v0

- IMAP entirely. Separate increment.
- JMAP write methods (`Email/set`, `Mailbox/set`, etc.).
- Incremental sync (`Email/changes`, `Mailbox/changes`, state tokens).
- Push (EventSource, WebSocket).
- Submission (`EmailSubmission/set`).
- Authentication (open server).
- Calendars, contacts, sieve, vacation responder.
- Threading metadata beyond raw `inReplyTo` / `references` headers.
- Search beyond `inMailbox` filter on `Email/query`.
- Attachments large enough to matter for performance work.
- Failure injection (slow responses, disconnects, retryable errors).
  Lands in v1 once the happy path works.
- Fuzz testing of the JMAP wire format.

## Suggested implementation order

1. Bootstrap the repo. `cargo init`, `Cargo.toml`, `brokkr.toml`,
   `.gitignore`.
2. HTTP listener + readiness-file + SIGTERM handling. No JMAP yet -
   just `GET /` returning 200.
3. Fixture loader (TOML). Validate references; reject malformed.
4. Session resource (`/jmap/session`).
5. `Mailbox/get`.
6. `Email/query` (sort + filter on `inMailbox`).
7. `Email/get` (full props, body parts).
8. Hand-written integration test: spin up the server, fire each
   method call, assert response shape.
9. Wire up to ratatoskr via plan 3's `sync-smoke`.

## Open questions

- **Norse-mythology name.** User's call. Suggestions if helpful:
  Heimdall (the watcher), Mimir (the head full of knowledge), Hugin
  (one of Odin's ravens, "thought"). Avoid names already in the
  ecosystem (pbfhogg, elivagar, nidhogg, ratatoskr, brokkr, sluggrs).
- **HTTP framework.** axum is the obvious default. Confirm at
  implementation time.
- **JMAP type surface.** Hand-rolled or pull in an existing crate.
  Lean hand-rolled for v0 to keep dep surface small; revisit if it
  hurts.
- **Fixture body sources.** Inline plain-text vs `.eml` file paths.
  Both are useful; pick which is the default.
- **Account config injection into ratatoskr.** Plan 3 says this lands
  via env var (`RATATOSKR_TEST_JMAP_ENDPOINT` or similar) read by a
  test-helpers feature in ratatoskr's account-config code. The
  endpoint plus a no-auth bypass need to be set up there. Coordinate
  at integration time.
- **Tracing / log format.** Structured (JSON) or human? Lean human
  for v0; structured later when test-failure dumps want it.
- **Where do fixtures live?** Inside the mock-server repo
  (`fixtures/jmap-small.toml`) is the obvious answer. Plan 3's
  `[ratatoskr] fixtures_dir` points at it.
- **Multiple accounts in one fixture.** v0 picks one account for
  simplicity. Multi-account is a later concern; the fixture format
  already accommodates it (`[[account]]` repeated) but the server
  binding logic doesn't.

## Non-goals

- Reusable as a public Rust crate. v0 is a private test peer for
  ratatoskr's sync code. If anyone else wants a JMAP mock server
  later, that's a different project.
- RFC compliance beyond what ratatoskr's JMAP client exercises. The
  spec is huge; we implement the subset our own client uses.
- Drop-in replacement for Fastmail / Stalwart / any real JMAP server.
  We are deterministic and small; they are general.
