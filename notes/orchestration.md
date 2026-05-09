# How brokkr drives sæhrimnir

Distilled from plan 1 (`ratatoskr-service-harness.md`) and plan 3
(`ratatoskr-sync-orchestration.md`) in the brokkr repo. Source-of-truth
lives there; this file is a cheat sheet so the next person doesn't
have to read both ~800-line plans cold.

## Top-level shape

For a sync test, brokkr spawns two children, in order:

1. **sæhrimnir** (us) - the mock JMAP server. Writes a readiness
   sentinel when bound. Serves JMAP from a fixture. Exits on SIGTERM.
2. **harness binary** - `app --test-harness <script.lua>`, the
   ratatoskr-side runtime that hosts the Lua VM + `ServiceClient`
   userdata. Reads the mock's endpoint via env var, drives sync over
   JSON-RPC against the Service it spawns internally.

Brokkr never speaks JMAP or JSON-RPC. It only spawns, sets env,
watches sentinels and exit codes, and aggregates artefacts.

## Spawn lifecycle (`brokkr sync-smoke`)

```
brokkr sync-smoke <script.lua>
    │
    ├─ allocate .brokkr/ratatoskr/sync/<test>/run-N/
    │
    ├─ spawn sæhrimnir
    │     args: --fixture <fixtures_dir>/<name>.{toml,lua}
    │           --readiness-file <run_dir>/mock-ready
    │           [--<proto>-port 0]   (default; ephemeral)
    │           [--log-file ...]     (omitted - stderr is captured)
    │
    ├─ wait_for_sentinel(<run_dir>/mock-ready, backstop)
    │     ↳ Appeared | BackstopExpired
    │     on BackstopExpired: SIGKILL mock, preserve dir, fail
    │
    ├─ read sentinel content → parse per-protocol ports
    │     (one line per protocol: JMAP/IMAP/SMTP/GRAPH/GMAIL <port>)
    │
    ├─ spawn harness binary
    │     env: RATATOSKR_TEST_JMAP_ENDPOINT=http://127.0.0.1:<jmap-port>
    │          RATATOSKR_TEST_IMAP_ENDPOINT=127.0.0.1:<imap-port>
    │          RATATOSKR_TEST_SMTP_ENDPOINT=127.0.0.1:<smtp-port>
    │          RATATOSKR_TEST_GRAPH_ENDPOINT=http://127.0.0.1:<graph-port>
    │          RATATOSKR_TEST_GMAIL_ENDPOINT=http://127.0.0.1:<gmail-port>
    │          BROKKR_HARNESS_ARTEFACT_DIR=<run_dir>/harness
    │          BROKKR_TEST_BIN_DIR=<bin dir>
    │          BROKKR_MARKER_FIFO=<fifo path>     (sync-bench only)
    │
    ├─ wait for harness exit (sync, std::process::Command-level)
    │
    ├─ SIGTERM sæhrimnir
    ├─ wait for mock exit (~1s budget)
    ├─ SIGKILL if not exited
    │
    ├─ collect: harness exit, mock exit, mock/stderr.log,
    │           summary.json (sync-bench), marker FIFO spans
    │
    └─ on failure: snapshot_proc both PIDs (writes
                   harness/proc-{status,wchan,syscall,stack}.txt and
                   mock/proc-{status,wchan,syscall,stack}.txt),
                   preserve run dir
```

The five env-var names are each configurable via
`[ratatoskr] test_endpoint_env_<proto>` in ratatoskr's brokkr.toml.
Plan 3 wires whatever names are configured; we don't hardcode them.

## Readiness sentinel contract

- **Path:** brokkr supplies via `--readiness-file <PATH>`. We don't
  pick the location.
- **Trigger:** atomic write the moment the TCP listener is bound.
  Write-temp-then-rename, so a reader can't catch us mid-write with
  an empty file.
- **Content:** one line per protocol, each `<NAME> <port>\n`, with
  `<NAME>` upper-case: `JMAP`, `IMAP`, `SMTP`, `GRAPH`, `GMAIL`.
  Every line is always present (we bind every listener, even when
  the test only cares about one). Brokkr's `wait_for_sentinel`
  doesn't parse the content (it's presence-only); plan-3-side code
  reads the file and picks the port for the protocol it cares
  about.
- **Brokkr's watcher:** polls the path until it appears or a backstop
  fires. Returns `Appeared` or `BackstopExpired` as first-class
  outcomes. No inotify; polling is fine.
  Source: brokkr's `src/ratatoskr/process.rs`, `wait_for_sentinel`.

## Env vars we are invoked with

**None mandated by brokkr.** All inputs come via CLI flags:
`--fixture`, `--port`, `--readiness-file`, `--log-file`. We should not
read environment for fixture data - keeps the CLI the single source
of truth and makes manual `brokkr mock-serve` invocations debuggable.

(The harness binary, separately, gets `RATATOSKR_TEST_*_ENDPOINT`
env vars. That's plan 1's surface, not ours.)

## Lifecycle / signals

- **SIGTERM** on success → graceful shutdown within 1s.
  Plan 2 acceptance #6 mandates this. In-flight requests should drain
  if they fit in the budget; otherwise drop the listener and exit.
- **SIGKILL** on backstop expiry → no cleanup possible; brokkr
  preserves whatever artefacts already exist.
- **No restart** within a run. One spawn, one teardown.

## brokkr.toml fields plan 3 introduces

Lives in **ratatoskr's** brokkr.toml, not ours:

```toml
[ratatoskr]
mock_server_binary = "../sæhrimnir/target/release/sæhrimnir"
fixtures_dir = "../sæhrimnir/fixtures"
test_endpoint_env_jmap = "RATATOSKR_TEST_JMAP_ENDPOINT"
test_endpoint_env_imap = "RATATOSKR_TEST_IMAP_ENDPOINT"   # v1+
sync_script_dir = "crates/app/tests/sync-harness"
```

Implications for us:

- Brokkr builds us on demand via `cargo_build` from our project root -
  same model as `brokkr serve` for nidhogg.
- Fixture file paths resolve at `<fixtures_dir>/<name>.toml`. The
  fixture's `name = "..."` field is informational; the filename is
  what brokkr passes to `--fixture`.
- Endpoint env-var name is configurable; we don't bake one in.

Our own `brokkr.toml` is a separate file: `project = "sæhrimnir"`
plus a `[[check]]` sweep so `brokkr check` works inside this repo.

## Artefacts brokkr collects from us

Per-run, under `.brokkr/ratatoskr/sync/<test>/run-N/mock/`. Brokkr
writes everything in this subdir; we don't see it. Plan 3 mirrors
plan 1's `harness/` subdir for symmetry, so the four-file `/proc`
snapshot from brokkr's `snapshot_proc` doesn't collide between the
two children.

- `mock/stderr.log` - our stderr, captured verbatim. Default log
  channel.
- `mock/proc-{status,wchan,syscall,stack}.txt` - `/proc/<our-pid>/`
  snapshot taken at failure-declaration time by brokkr's
  `snapshot_proc`. Same four-file shape the harness side uses.
- `mock/outcome.toml` - our exit code, signal, wait time, fixture
  name. Brokkr writes.

Our own outputs go to wherever the CLI flags point:

- `--readiness-file` → `<run_dir>/mock-ready`
- `--log-file` → optional; if omitted, our stderr is what brokkr
  captures into `mock/stderr.log`. If set explicitly, we write to
  the configured path AND brokkr still captures stderr (which will
  be empty or near-empty in that case).

We do not write into the harness binary's artefact dir
(`BROKKR_HARNESS_ARTEFACT_DIR`, which lives at
`<run_dir>/harness/`); that's plan 1's surface.

## What brokkr does NOT do

- Speak JMAP. Doesn't know which method calls happen.
- Speak JSON-RPC. Doesn't see the harness ↔ Service wire.
- Validate fixture content. We do that at startup; brokkr just runs
  us and reports our exit code.
- Inject account credentials. The Lua script seeds the test account
  via a `TestSeedAccount` RequestParams variant on the Service.
- Restart us if we crash. One spawn per run.

## Implications for sæhrimnir's design

- **Atomic sentinel write.** A reader can `read()` the file the
  moment we create it. Write to a temp file first, then rename.
  `tokio::fs::rename` (or `std::fs::rename`) on the same filesystem
  is atomic.
- **Stderr is the log channel by default.** Brokkr captures it
  verbatim. `--log-file` redirects only when the caller wants file
  form.
- **Fast SIGTERM handling.** `tokio::signal::ctrl_c` (which also
  catches SIGTERM via `tokio::signal::unix`) plus a graceful drain
  with a hard 1s budget. After the budget, drop the listener and
  return.
- **No state to preserve.** Stateless service; determinism comes
  from the fixture, not from anything we write.
- **Byte-stable responses.** Same fixture in → same bytes out. A
  failure-triage tool should be able to diff two runs byte-for-byte.

## Test / admin control plane

Tests-only routes mounted on the JMAP HTTP listener (the JMAP port
from the sentinel). Sæhrimnir is a test-only binary, so no auth or
feature gate guards these. All routes are scoped under `/test/`.

- `GET /test/smtp/submissions` - JSON array of captured SMTP
  submissions (parsed projection, see
  `notes/ratatoskr-smtp-surface.md`).
- `DELETE /test/smtp/submissions` -> 204; clears the SMTP log.
- `GET /test/requests` - JSON array of every protocol-level
  dispatch event the binary has handled across all five protocols,
  in arrival order. Each entry is `{ protocol, command,
  received_at, detail }`:
  - `protocol`: lowercase tag - `"jmap"` / `"imap"` / `"smtp"` /
    `"graph"` / `"gmail"`.
  - `command`: the protocol-native verb. JMAP method name (e.g.
    `"Mailbox/get"`), IMAP keyword (`"CAPABILITY"`, `"UID FETCH"`
    - `UID` sub-commands are recorded as `"UID <SUB>"`), SMTP verb
    (`"EHLO"`, `"MAIL"`, ...), or for HTTP-based protocols the
    request `METHOD path` with the query string stripped.
  - `received_at`: wall-clock RFC3339 timestamp. The only
    non-deterministic field saehrimnir emits anywhere; tests
    asserting on byte-stable JSON should ignore it.
  - `detail`: free-form JSON object with protocol-specific extras
    (JMAP `call_id`, IMAP `tag` + `args`, SMTP `args`, HTTP
    `query`).
- `DELETE /test/requests` -> 204; clears the request log.
- `POST /test/fixture/reset` -> 204; clears the SMTP submission
  log, the request log, AND the OAuth token store. The fixture
  itself is read-only in v0 (IMAP `UID STORE` is a non-persistent
  no-op), so reset is currently equivalent to "clear all three
  in-memory logs". When mutation lands (`[[change]]` scripts,
  persistent UID STORE), the implementation grows here without
  changing the route shape - harness scripts can rely on the
  contract.
- `POST /test/fixture/step` -> 501 with body
  `{"error": "fixture step not implemented", "detail": "..."}`.
  Reserved for `[[change]]` script entries (fixture-format growth,
  see `TODO.md`); returns 501 today so harness scripts can detect
  the gap rather than silently no-op.

The request log is process-scoped: a fresh saehrimnir start is
always an empty log.

OAuth provider routes (also mounted on the JMAP listener):

- `POST /oauth/token` - issue an access + refresh token from an
  authorization-code or refresh-token grant. v0 doesn't validate
  client credentials.
- `GET /oauth/userinfo` - read `Authorization: Bearer <token>`,
  return the fixture account's identity claims, or 401 if the
  token is unknown.
- `POST /test/oauth/invalidate` - admin route, body
  `{"token": "..."}`; drops the token so subsequent requests 401.

Full surface details in `notes/ratatoskr-oauth-surface.md`. Bearer
enforcement on the mail HTTP listeners (JMAP, Graph, Gmail) is
opt-in via `[oauth] enforce = true` in the fixture; default keeps
the v0 "no auth" baseline.

## Things plan 3 hasn't decided yet

- **Mock build orchestration in brokkr.** Plan 3 says "same model as
  `brokkr serve` for nidhogg" but defers the implementation. For us,
  the practical effect: brokkr will run `cargo build --release` from
  our project root before invoking us. No feature sweeps required.
- **Per-protocol port flags in `brokkr mock-serve`.** We bind one
  listener per protocol on its own port - `--jmap-port`,
  `--imap-port`, `--smtp-port`, `--graph-port`, `--gmail-port` - and
  the sentinel reports each one on its own line. `brokkr mock-serve`
  just spawns us once with a fixture flag and reads whichever ports
  it cares about out of the sentinel.
- **Per-fixture run-dir keying.** Plan 3 currently keys on script
  name; fixture-as-path-component is deferred. Not our concern.

## What we should ignore

- Marker FIFOs, `BROKKR_MARKER_FIFO`, brokkr's sidecar - all
  unrelated to us. Those are concerns of the harness binary, not
  ours.
- Lua / dellingr / `ServiceClient`. Live entirely in ratatoskr's
  `app` crate. We don't depend on or know about them.

(Brokkr does take a one-shot `/proc` snapshot of us at failure time
via its standalone `snapshot_proc` primitive - written into the
`mock/proc-{status,wchan,syscall,stack}.txt` set - but it reads
from outside; we don't participate.)
