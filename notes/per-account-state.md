# Per-account JMAP state (design + assessment)

Status: implemented. `Fixture` now carries `state_seed: String` plus
`account_logs: BTreeMap<String, ChangeLog>` (each log owns its counter
/ current-state token / bounded transition ring, all sharing the
seed). `record_transition` splits each `MutationDiff` per owning
account via `split_diff_by_account` and advances only the touched
accounts' logs; `state_for(account_id)` is the single read helper every
per-protocol reporting site routes through (`primary_state()` for the
genuinely session-/process-wide sites: JMAP `sessionState`,
`/test/snapshot-state`, `/test/fixture/step`). The walkers
(`email_delta_since_account`, `mailbox_delta_since_account`,
`calendar_delta_since_account`, `event_delta_since`,
`event_delta_since_any`, `contact_delta_since`,
`contact_delta_since_any`) resolve to one account's log and drop the
old post-walk live-account retain. Two new parallel destroyed-account
vectors (`contact_folder_destroyed_accounts`,
`category_destroyed_accounts`) carry the account for the two resource
families with no surviving parent at retire time. The wire token format
is unchanged (`{seed}.{N}`, per-account `N`).

The text below is the original design + assessment, retained for
rationale.

Replace sæhrimnir's single global state counter with a per-account
state model so the mock is RFC 8620 correct: an account's mutation
must move only that account's state, never any sibling's.

## The proposal (as received)

> saehrimnir keeps one global `Email/state` counter
> (`fixture-state.N`) shared across every account it serves (same
> pattern for `Mailbox/state`, and `Thread`/`ContactCard` state). In
> real JMAP (RFC 8620 1.5.2), state is per-account - mutating account
> B's mail must not move account A's state string. The mock collapses
> all accounts onto one counter, so any account's mutation bumps
> everyone's state.
>
> Replace the single global state with a per-`accountId` state model:
> a map `accountId -> { state_string, changelog }`, where each account
> tracks its own monotonic state and its own list of
> created/updated/destroyed object ids per state transition.
> `Email/set` (and every mutation) advances only that account's state
> + changelog. `Email/changes{accountId, sinceState}` diffs only that
> account's changelog. Needs doing for each object type that carries a
> state (Email, Mailbox, Thread, Contact/ContactCard).

## Assessment

### Agree on direction

Per-account state is the right model and worth doing. The current
single global counter (`Fixture::state` + one `Fixture::change_log`)
violates RFC 8620 1.5.2. Two defects the per-account changelog fixes
that nothing else can:

1. **Cross-account token bleed.** Any account's mutation advances
   every account's `newState`. This is the core wrongness.
2. **Shared eviction.** `change_log` is one bounded ring
   (`MAX_TRANSITIONS = 256`, `src/fixture.rs:279`). Heavy churn on
   account B can evict account A's `sinceState` boundary and
   spuriously hand account A a `cannotCalculateChanges`. A per-account
   log makes eviction per-account too. Id-filtering cannot touch this.

### Disagree on the diagnosis

The writeup says the mock "returns the primary's own email as
changed" through `Email/changes`. It does not. `email_delta_since_account`
(`src/fixture.rs:695`) already filters: it retains created/updated by
the live email's `account_id` and destroyed by the parallel
`email_destroyed_accounts`. After a secondary `Email/set`, the
primary's `Email/changes` walk collects the secondary's email id, then
retains it away - the `created`/`updated`/`destroyed` arrays come back
empty. **Content isolation already works on the wire**, not just at
the DB layer.

So bifrost is not issuing the failing `Email/get` off a populated
`changed[]` array. The real trigger is the **state token** itself: the
primary's `Email/changes` returns `oldState != newState` with empty
arrays, or the session/account state bump drives bifrost to re-query.
Both are fixed by per-account tokens - but the value here is the
token + eviction isolation, not content isolation.

- If the failing assertion is literally `newState == oldState`, the
  token fix nails it.
- If a non-empty `changed[]` has actually been observed reaching
  bifrost, there is a second bug; capture that transcript before
  claiming this change closes it.

### Two implementation caveats

1. **Keep the token format; do not take "map accountId ->
   {state_string, changelog}" literally on the wire string.** ~40
   tests pin the literal `"fixture-state"` / `"fixture-state.N"`
   (`tests/api.rs:118`, `tests/step.rs:135`, Graph
   `$deltatoken=d.fixture-state`, unit `state == "s1"`). Keep a
   **shared seed** plus the `{seed}.{counter}` format with a
   **per-account counter** - byte-identical for the primary,
   independent for the secondary. A freshly formatted per-account
   state string would break all of those for no benefit: tokens are
   namespaced by the requesting account (the request carries
   `accountId`; the bearer scopes the listener), so two accounts both
   reading `"fixture-state.1"` never collide - each resolves only in
   its own log.

2. **Implement at the `Fixture` level, uniformly - do not carve it out
   to JMAP.** `Fixture::state` is the cursor for Graph delta links,
   gcal / People sync tokens, and CalDAV ctag / etag too (~25 read
   sites, all already account-scoped via bearer / principal).
   Splitting the changelog per-account at the source makes all of them
   correct in one move and is less code than special-casing JMAP.

   Note: **Thread carries no independent state.** `Thread/get` /
   `Thread/changes` (`src/jmap.rs:1105`, `:1197`) report
   `Fixture::state` and derive the delta from the email log. "Thread
   state" rides email state; it is not a separate changelog. Same for
   the JMAP session resource's `state`.

## Plan

1. `Fixture`: replace `state: String` + `change_log: ChangeLog` with
   `state_seed: String` + `account_logs: BTreeMap<String, ChangeLog>`
   (each log gains its own `state` / `counter`, shares the seed). Add
   `Fixture::state_for(account_id) -> &str` returning the seed when an
   account has no log yet.
2. `record_transition`: split the `MutationDiff` per account, append
   to each touched account's log, and bump only those counters. Still
   return an aggregate `Transition` so set-handlers' id-list usage is
   unchanged. Destroyed-id account resolution needs two new parallel
   vectors - `contact_folder_destroyed_accounts` and
   `category_destroyed_accounts` - the only resource types that
   currently carry no account info at destroy time (email / mailbox /
   calendar already carry `*_destroyed_accounts`; events resolve via
   calendar -> account, contacts via folder -> account).
3. Walkers take or derive an account: the `*_delta_since_account`
   variants already do; `event_delta_since` / `contact_delta_since`
   resolve via calendar -> account / folder -> account; the
   `*_delta_since_any` variants gain an account parameter. Per-account
   logs hold only that account's ids, so the post-walk live-account
   `retain` filter in the `_account` walkers can drop away.
4. Repoint the ~30 reporting sites (`Fixture::state` reads) to
   `state_for(account)`. The account is in scope at each:
   - JMAP handlers carry the request `accountId`.
   - Graph / gcal / People resolve the bearer's account.
   - CalDAV resolves the principal's account.
   - Admin `/test/snapshot-state` and `/test/fixture/step` report the
     primary account's state (or a per-account map).
5. Reset is free: `POST /test/fixture/reset` clones the baseline
   `Fixture` (`src/shared.rs:78`), so per-account state living inside
   `Fixture` rewinds with it.
6. `brokkr check`.

## Open question

Share the failing transcript (the `jmap-...secondary-create` test) so
caveat #1 can be confirmed before code lands: is the assertion on
`newState` stability, or has a non-empty `changed[]` actually been
observed?
