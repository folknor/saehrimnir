# Sæhrimnir

Deterministic mock email-protocol server. Test peer for ratatoskr's
sync code, spawned by brokkr's `[ratatoskr]` sync commands. Originally
scoped to JMAP only; now growing to cover every protocol ratatoskr's
sync code talks to (JMAP, IMAP, SMTP, Microsoft Graph, Gmail), all
backed by one shared TOML fixture.

JMAP is implemented; IMAP is in progress; SMTP / Graph / Gmail are
queued. See `TODO.md` for the running task list and `notes/` for
per-protocol surface docs.

## Where to read

- `notes/plan.md` - the original JMAP-only v0 plan. Background
  reading; superseded for scope but still accurate for the JMAP
  layer's design decisions.
- `notes/orchestration.md` - how brokkr drives us: lifecycle,
  sentinel, env vars, brokkr.toml fields.
- `notes/ratatoskr-client-surface.md` - what the JMAP client expects
  on the wire, with `crates/jmap/src/...:LL` citations.
- `notes/ratatoskr-imap-surface.md` - same shape, for IMAP.
- `notes/imap-plan.md` - implementation plan for the IMAP layer.
- `notes/jmap-client-fork.md` - pointer to the local jmap-client fork.
- `notes/fixture-format.md` - TOML fixture shape and validation rules.
- `TODO.md` - implementation steps still pending, with the design
  decisions worked out per step.

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
- `src/fixture.rs` - TOML loader, validator, canonical types shared
  by every protocol layer.
- `src/sentinel.rs` - atomic readiness-file write (temp + rename).
- `src/shutdown.rs` - SIGTERM/SIGINT handler.
- `src/lib.rs` - library surface; `main.rs` keeps just the runtime.
- `src/routes.rs` - axum router, `AppState`, JMAP HTTP route handlers.
- `src/jmap.rs` - JMAP request envelope, dispatcher, per-method
  handlers.
- `src/imap/` - IMAP listener, connection state machine, command
  dispatcher, RFC 822 emit. (In progress.)
- `tests/api.rs` - JMAP integration tests via
  `tower::ServiceExt::oneshot`.
- `fixtures/jmap-small.toml` - canonical v0 fixture (despite the
  name, both protocols read from it).
- `scripts/smoke.sh` - boot, curl, SIGTERM verification script.

## Status

JMAP: complete for v0 (session resource, `Mailbox/get`, `Email/query`,
`Email/get`, full integration test coverage).

IMAP: surface scouted (`notes/ratatoskr-imap-surface.md`), plan
written (`notes/imap-plan.md`), implementation underway.

SMTP / Graph / Gmail: queued. Each will follow the JMAP/IMAP pattern
(scout the client surface, plan, implement).

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
