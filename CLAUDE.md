# Sæhrimnir

Deterministic mock email-protocol server. Test peer for ratatoskr's
sync code, spawned by brokkr's `[ratatoskr]` sync commands. Originally
scoped to JMAP only; now growing to cover every protocol ratatoskr's
sync code talks to (JMAP, IMAP, SMTP, Microsoft Graph, Gmail), all
backed by one shared fixture authored as either TOML or Lua.

All five protocols are implemented for v0. See `TODO.md` for what's
left per protocol (mostly future fixture-format growth and sibling
resource modules), and `notes/` for the per-protocol surface docs.

## Where to read

- `notes/orchestration.md` - how brokkr drives us: lifecycle,
  sentinel, env vars, brokkr.toml fields.
- `notes/fixture-format.md` - fixture shape and validation rules,
  shared by the TOML and Lua loaders.
- `notes/ratatoskr-jmap-surface.md` - what the JMAP client expects
  on the wire, with `crates/jmap/src/...:LL` citations.
- `notes/ratatoskr-imap-surface.md` - same shape, for IMAP.
- `notes/ratatoskr-smtp-surface.md` - same shape, for SMTP.
- `notes/ratatoskr-graph-surface.md` - same shape, for Microsoft
  Graph (mail-sync only in v0; the doc also lists the resource
  categories we'll need to scaffold for later).
- `notes/ratatoskr-gmail-surface.md` - same shape, for Gmail's REST
  API.
- `TODO.md` - what's left, per protocol.

The notes are the source of truth. Do not refer to siblings
(`../ratatoskr`, `../jmap-client`, `../brokkr`) without first
checking whether the fact is already in `notes/`.

## Project constraints

- Determinism: same fixture in, byte-stable bytes out. Responses
  derive entirely from the fixture; no clocks, no random IDs, no
  unsorted iteration. Output uses `serde_json::Map` (BTreeMap-backed)
  for stable key ordering.
- No auth in v0: every protocol accepts any credential. Bearer,
  basic, LOGIN, XOAUTH2, OAUTHBEARER all return success without
  validating.
- One shared fixture per process. Each protocol projects its own wire
  shape from the same canonical types in `src/fixture.rs`.
- Out-of-scope JMAP methods (`Email/changes`, `Mailbox/changes`,
  `Email/set`, `EmailSubmission/set`, push, etc.) return
  `unknownMethod`. Out-of-scope IMAP commands (write paths, IDLE,
  NOTIFY, etc.) return `BAD`.
- The session must NOT advertise `urn:ietf:params:jmap:principals`.
  It would pull the client into `Principal/get` and
  `ShareNotification` paths the mock cannot satisfy.

## Layout

- `src/main.rs` - runtime entry. Loads fixture, binds listener,
  writes sentinel, serves until SIGTERM with a 1-second graceful
  budget.
- `src/cli.rs` - clap CLI flags.
- `src/fixture.rs` - canonical types (`Fixture`, `Account`, `Mailbox`,
  `Email`, ...), shared `RawFixture`-shaped intermediate, the
  `normalize` cross-reference validator, and the TOML loader. The
  public `load(path)` dispatches to `lua::load` for `.lua` files,
  TOML otherwise.
- `src/lua.rs` - dellingr-backed Lua scenario loader and reactive-
  callback dispatcher. Exposes the `fixture` / `account` / `mailbox`
  / `email` builder `RustFunc`s plus `bulk_emails` and `bulk_threads`
  for synthetic-data scale testing. Hosts the `BOOTSTRAP` Lua
  snippet that defines `on(protocol, command, fn)` and
  `_sae_dispatch`. `Dispatcher` retains the dellingr `State` behind
  a `Mutex` so protocol handlers can dispatch callbacks. Accumulates
  into a `Builder` in user_data, and hands the `RawFixture` to
  `fixture::normalize` so validation is shared with the TOML path.
- `src/scenario.rs` - main loader entry point. `Scenario { fixture,
  dispatcher }` bundles the validated fixture with the optional
  callback dispatcher. `scenario::load(path)` dispatches by
  extension.
- `src/templates.rs` - synthetic data pools (names, domains,
  projects, teams, topics) and a `fill_template` primitive used by
  `bulk_emails`. Lifted from `<ratatoskr>/crates/dev-seed/src/
  templates.rs` and pruned.
- `src/sentinel.rs` - atomic readiness-file write (temp + rename).
- `src/shutdown.rs` - SIGTERM/SIGINT handler.
- `src/lib.rs` - library surface; `main.rs` keeps just the runtime.
- `src/routes.rs` - axum router, `AppState`, JMAP HTTP route handlers.
- `src/jmap.rs` - JMAP request envelope, dispatcher, per-method
  handlers.
- `src/imap.rs` - IMAP listener, connection state machine, command
  dispatcher, RFC 822 emit.
- `src/smtp.rs` - SMTP submission listener + in-memory submission
  capture log.
- `src/graph/` - Microsoft Graph mock. `mod.rs` (router, AppState,
  catchall 404), `odata.rs` (query parsing, pagination cursors,
  envelope), `mail.rs` (mail-sync handlers). Sibling files for
  calendar / contacts / drive / groups / EWS land here when those
  surfaces are scouted.
- `src/gmail/` - Gmail REST mock. `mod.rs` (router, AppState,
  catchall 404), `mail.rs` (profile, labels, threads, history,
  attachments stub, MIME payload builder, hand-rolled base64url).
  Sibling files for People-API contacts / Drive uploads land here
  later.
- `tests/api.rs` - JMAP integration tests via
  `tower::ServiceExt::oneshot`.
- `tests/imap.rs` - IMAP integration tests over a duplex stream.
- `tests/smtp.rs` - SMTP integration tests over a duplex stream.
- `tests/graph.rs` - Graph integration tests via
  `tower::ServiceExt::oneshot`.
- `tests/gmail.rs` - Gmail integration tests via
  `tower::ServiceExt::oneshot`.
- `tests/lua_fixture.rs` - asserts the Lua loader produces a
  `Fixture` byte-identical to the equivalent TOML fixture, plus
  error paths.
- `tests/scale.rs` - 10k-email scale-correctness tests against
  `fixtures/jmap-bulk.lua`. Verifies pagination through the four
  protocol layers (JMAP `Email/query`, Graph `messages` and
  `messages/delta`, Gmail `threads`) without drops or dupes.
- `tests/lifecycle.rs` - subprocess test that spawns the actual
  binary, polls for the readiness sentinel, hits a real network
  endpoint, sends SIGTERM, asserts a clean exit. Closes the
  coverage gap that `scripts/smoke.sh` covers manually.
- `fixtures/jmap-small.toml` and `fixtures/jmap-small.lua` - the
  canonical v0 scenario in both authoring formats. Asserted
  equivalent by `tests/lua_fixture.rs`.
- `fixtures/jmap-bulk.lua` - 10k-email scale fixture demonstrating
  `bulk_emails`.
- `scripts/smoke.sh` - boot, curl, SIGTERM verification script.

## Status

JMAP: complete for v0 (session resource, `Mailbox/get`, `Email/query`,
`Email/get`, full integration test coverage).

IMAP: complete for v0's read path (greeting, `CAPABILITY`, `LOGIN`/
`AUTHENTICATE`, `ENABLE QRESYNC`, `LIST`, `STATUS`, `SELECT`/`EXAMINE`/
`CLOSE`, `UID SEARCH`, `UID FETCH` with full RFC 822 body emission,
CONDSTORE `CHANGEDSINCE`). Plus `UID STORE` as a non-persistent
no-op: emits the post-op FETCH untagged update and a tagged OK so
ratatoskr's flag-writeback path completes cleanly without erroring;
the mutation does not persist (subsequent fetches see the fixture's
keywords unchanged). Integration test in `tests/imap.rs` drives the
full initial-sync transcript.

SMTP: complete for v0's submission path (greeting, EHLO,
AUTH PLAIN/LOGIN/XOAUTH2/OAUTHBEARER, MAIL FROM, RCPT TO, DATA with
dot-stuffing reversal, RSET, NOOP, QUIT). Submissions captured in an
in-memory `SubmissionLog` that tests read directly. Integration tests
in `tests/smtp.rs`.

Graph: complete for v0's mail-sync path. `/v1.0/me/mailFolders`
(list, by-id, by-well-known-alias, childFolders), `/v1.0/me/
mailFolders/{id}/messages` (with `$top`/`$skip`/`$skiptoken`/
`$filter`), `/v1.0/me/mailFolders/{id}/messages/delta` (initial
dump, follow-up no-op, `$deltatoken=latest` shortcut). Catchall
returns the Graph error envelope so unimplemented resources are
visibly out-of-scope. Module is laid out as a directory so calendar/
contacts/drive/groups/EWS drop in as siblings later.

Gmail: complete for v0's mail-sync path. `/gmail/v1/users/me/profile`
+ `/labels` + `/threads` (list paginated by `nextPageToken`, with
`q=after:YYYY/M/D` filtering) + `/threads/{id}` (full MIME payload
projection of fixture emails into Gmail's nested mimePart shape) +
`/history` (read-only no-op since fixtures don't change) +
`/messages/{id}/attachments/{aid}` (404 stub) + `/settings/sendAs`
(empty list). Catchall returns Gmail error envelope. Module
structure leaves room for People-API contacts and Drive sibling
files.

Lua fixture loader: wired via [dellingr](https://crates.io/crates/dellingr),
a pure-Rust deterministic sandboxed Lua VM with cost-bounded
execution. The main entry point is `scenario::load(path)` which
dispatches by extension: `.lua` goes through `src/lua.rs` and
retains the dellingr `State` as a `Dispatcher` for reactive
callbacks; anything else parses as TOML and the dispatcher is
`None`. The legacy `fixture::load` keeps returning a bare `Fixture`
for tests that don't care about callbacks.

Static surface: `fixture`, `account`, `mailbox`, `email` builders
for hand-authored scenarios; `bulk_emails({ count, mailbox, seed,
start_at, interval_seconds, id_prefix })` for synthetic scale-test
fixtures; `bulk_threads({ count, mailbox, messages_per_thread, seed,
... })` for multi-message conversations with proper `In-Reply-To` /
`References` chaining (deterministic, byte-stable across runs at
the same seed; templates in `src/templates.rs`).

Dynamic surface: scripts register callbacks via
`on(protocol, command, function)`; the protocol layer consults the
`Dispatcher` before generating its default response. The callback
receives a `req` table with `call_index` (1-based per (protocol,
command)) plus protocol-specific fields. Returning a table with
`{ status = "...", message = "..." }` overrides the response; `nil`
or no return = pass through. Currently wired for IMAP `UID FETCH`
and JMAP method calls (any method - `Mailbox/get`, `Email/query`,
`Email/get`, etc.). For JMAP, the override maps to a method-level
JMAP error: the `methodResponses` entry becomes
`("error", {"type": status, "description": message}, callId)`.
Other protocols' AppState plumbing is in place; individual commands
fan out as fixtures need them.

`Dispatcher` is `Arc<Mutex<State>>`-shaped (dellingr 0.2 made
`State: Send`); the mutex covers brief synchronous Lua calls and is
never held across `.await`. Per-(protocol, command) call counts
are tracked alongside so `req.call_index` is strictly increasing.

Note: dellingr deliberately omits Lua's unparenthesized function-call
sugar, so builder calls in `.lua` fixtures are written
`mailbox({...})` not `mailbox{...}`. Also: `dellingr::set_table_raw`
takes `(key=top, value=below_top)` order - push value FIRST, then
key on top, then call. Different from standard Lua C API
(`lua_settable` pops key from -2, value from -1). And: `get_table_raw`
called via the public `State` API consumes the top key BEFORE
checking the table index, so passing a relative `i = -1` (which
points at the key, not the table) panics with an out-of-bounds
on the now-shifted stack. Capture the table's absolute index via
`state.get_top()` before pushing the key.

## Rules

### General rules

- Don't use gremlins! Em-dash, en-dash, strange quotes, whatever - they're all verboten.
- Don't remind the user of CLAUDE.md rules. They wrote them, so they know them.

### Memory rules

Do not use your Memory functionality. Do not read, write, or update memories. Do not suggest saving things to memory. Durable context belongs in CLAUDE.md or the relevant docs, not in per-session memory files - this project is developed across several hosts and users, and memory does not transfer between them; CLAUDE.md does.

### Bash rules

- Never use `sed`, `find`, `awk`, `head`, `tail`, or complex bash commands.
- Never chain commands with `&&`.
- Never chain commands with `;`.
- Never chain/pipe commands with `|`. Exception: piping into `review` is allowed (writing scratch prompt files is wasteful).
- Never capture stdout into env vars (`UUID=$(...)`).
- Never read or write from `/tmp`. All data lives in the project.
- Never run raw `cargo`, `curl`, `pkill`. Use `brokkr`.
- Never run `git` with `-C <path>`. Run `git` from the current working directory.

### git commit rules

- Never commit markdown changes alone. Bundle them with upcoming code commits.
- When committing other changes: always tag along markdown files if dirty.
- Write substantive engineering-focused commit messages.
- Has `Cargo.lock` changed? Commit it.
- Never `git push` unless the user explicitly asks. Stop after the commit.

## Commands

Use `brokkr` (not `cargo`) for check/test. It runs a gremlins scan (banned Unicode), then clippy, then tests - clippy denies warnings project-wide, so a clippy failure short-circuits before tests run. By default output is filtered to changed files and capped at 20 diagnostics per phase.

- `brokkr check` - gremlins + clippy + all tests (changed-files scope)
- `brokkr check --all` - show every diagnostic, no cap, no scope filter
- `brokkr check --fix-gremlins` - rewrite banned Unicode in tracked files (em/en dash -> `-`, smart quotes -> straight, NBSP -> space, zero-width/bidi deleted) before checking
- `brokkr check -p <crate>` - scope to one package (e.g. `-p rtsk`, `-p app`, `-p squeeze`)
- `brokkr check -- --test <file>` - forward args to `cargo test` (args after the second `--` go to the test binary)
- `brokkr test -p <crate> <NAME>` - release-mode focused single-test runner. Always passes `--release --include-ignored --nocapture --test-threads=1`. `<NAME>` is a case-sensitive substring filter (matches both unit and integration tests). Streams the test's own stdout/stderr live and prints a `[test] PASS/FAIL` footer with wall time. Defaults to `--all-features`; runs a second sweep if `[check].consumer_features` is set in `brokkr.toml`. Gated off for litehtml/sluggrs (use `brokkr visual` there).
  - `-p, --package <PKG>` - cargo package. Required in this workspace - no default package, and overrides `[test] default_package` in `brokkr.toml` if set.
  - `-N, --repeat <N>` - run the test N times per sweep (flaky-test hunting).
  - `-j, --jobs <N>` - parallel cargo compile jobs.
  - `--raw` - bypass output filtering, print everything cargo emits.
  - `--debug` - build and run the test in dev profile instead of release. Use this for subprocess-lifecycle / IPC / boot-path tests where release-LTO compile time (3-4 min for the full workspace) dominates wall time and the optimization level doesn't change the behavior under test. `BROKKR_TEST_BIN_DIR` points at `<target>/debug` accordingly.
  - Example: `brokkr test -p common truncates_without_splitting` or `brokkr test -p calendar extract_tag_value_flattens_nested_text -N 5` or `brokkr test -p app terminal_failure_at_initial_boot_does_not_respawn --debug`.
- `cargo run -p app` - run the iced app (requires a seeded DB, see `crates/app/seed-db.py`)

**Always run `brokkr check` in the foreground with a 4-minute (240000ms) timeout.** A healthy `brokkr check` finishes well under that. If it does not, something is wrong - kill it and investigate (most often: a test hangs because a background task wasn't drained on shutdown). Do not raise the timeout to "wait it out", and do not run `brokkr check` in the background.

## Multi-Agent Orchestration

**Do NOT use worktree isolation for parallel agents.** Worktrees create merge conflicts that silently drop agent work. Instead, launch agents in the same tree with strict file ownership - zero overlap.

**Why no worktrees:** Worktrees let agents work on diverged snapshots. When merging back, `git checkout --ours/--theirs` drops code, conflict markers get missed, and features end up "existing but not wired" - types/functions created but never connected to message dispatch, views, or call sites. This happened repeatedly in a 114-commit session and was only caught by a rigorous 3-pass audit.

**Agent coordination rules:**
- Each agent gets exclusive ownership of specific files. No two agents touch the same file.
- `main.rs` is shared - agents may ONLY add Message enum variants and one-line dispatch arms. All handler logic goes in `handlers/*.rs`.
- Agents must read their handler file FIRST (it already has extracted methods). Do not replace existing code with placeholders.
- Agents must NOT run `cargo check/build/test`. The orchestrator validates between agents.

**Verification standard - "implemented" means wired:**
- A feature is NOT implemented unless the user can reach it through current Message dispatch -> handler -> view wiring.
- Types that exist but are never constructed, methods that exist but are never called, message variants with no dispatch arm - these are dead code, not implementations.
- After agents complete, verify wiring by checking: (1) Message variant exists, (2) dispatch arm in update() calls handler, (3) handler performs the work, (4) view renders the result or side effect is observable.

**Audit protocol:**
- Do not trust agent claims of completion. Verify existence + wiring + behavior.
- Use the 3-pass audit structure: domain-specific verification -> cross-cutting reconciliation -> editorial normalization.
- Discrepancies docs should contain only current gaps, not historical records. Remove resolved items entirely.

## Code Review (`review`)

`review` is a CLI tool that fans out code review requests to anchored AI sessions. Each archetype has a stored prime prompt (in `.review.toml` under `[_prime].<archetype>`) that defines the review lens. Configuration lives in `.review.toml` at the repo root.

Four archetypes: `security`, `bugs`, `perf`, `arch`. The `sweep` group fans out to all four in parallel.

**Multiple archetypes go in one invocation as a comma list** (e.g. `review bugs,arch,perf --oneshot`), not as separate parallel `review` calls. The CLI staggers requests internally to stay under upstream HTTP rate limits; firing several `review` processes at once defeats that and trips the limiter.

**Default to `--oneshot`.** Anthropic's prompt cache is ~1 hour, so starting a fresh session for each unrelated review is cheaper than resuming a long-lived one. `--oneshot` starts a fresh session, prepends the stored prime prompt, runs the query, and prints the session ID to stdout.

**Follow-ups:** within the cache window, resume the same session with `--session <id>` (using the ID printed by the previous `--oneshot`). The cache stays warm; only the new query and reply are billed. `--session` requires `--provider` and is mutually exclusive with `--oneshot`.

```bash
echo "review the new sync code" | review bugs --oneshot
# session: 019de...
# <findings>

echo "follow up on the second finding" | review bugs --provider claude --session 019de...
# <answer>
```

Don't reach for a second `--oneshot` to follow up - that creates a different fresh session with no memory of the first. Use `--session` for continuity within a thread, `--oneshot` for new threads.

To update the prime prompt for an archetype, pipe new content to `review prime <archetype> --provider <p>`. The prompt is stored once per archetype and shared across providers; once stored, prime any other provider with no stdin to reuse it.
