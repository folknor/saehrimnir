//! Lua-driven fixture loader and reactive-callback dispatcher,
//! backed by dellingr.
//!
//! Static surface: the script calls global builders `fixture {...}`,
//! `account {...}`, `mailbox {...}`, `email {...}` (plus
//! `bulk_emails` / `bulk_threads` for synthetic data) to populate
//! the same `RawFixture` shape the TOML loader produces.
//!
//! Dynamic surface: the script registers callbacks via
//! `on(protocol, command, function)`. Each protocol's handler
//! consults [`Dispatcher::dispatch`] before generating its default
//! response; the callback can return a table to override the wire
//! response, or `nil` to pass through.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use dellingr::{Anchor, ArgCount, LuaType, RetCount, State, error::ErrorKind};
use rand::SeedableRng;
use rand::rngs::SmallRng;

use crate::fixture::{
    self, Fixture, RawAccount, RawAddress, RawAttachment, RawEmail, RawFixture, RawMailbox,
};
use crate::templates;

/// `i64` range bounds expressed as `f64`. These constants encode the
/// fact that `i64::MIN` and `i64::MAX` are not exactly representable
/// as `f64`, but the conversion is well-defined for the
/// containment-check pattern below.
#[allow(clippy::cast_precision_loss)]
const I64_MIN_F64: f64 = i64::MIN as f64;
#[allow(clippy::cast_precision_loss)]
const I64_MAX_F64: f64 = i64::MAX as f64;

/// Hard cap on `count` for the bulk builders. Above this we'd be
/// allocating gigabytes of `RawEmail`s; failure mode is OOM. The cap
/// is high enough to permit "test sync at scale" scenarios but low
/// enough that a typo in the script (`count = 9_000_000_000`) gets a
/// clean error instead of a process death.
const MAX_BULK_COUNT: i64 = 10_000_000;

/// Read a `.lua` scenario file and produce a fully validated `Fixture`.
/// Discards the dispatcher; for callback-aware loading, use
/// [`crate::scenario::load`].
pub fn load(path: &Path) -> Result<Fixture, String> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    let chunk_name = format!("@{}", path.display());
    let dir = path.parent().unwrap_or(Path::new("."));
    load_source_with_dir(&source, &chunk_name, dir)
}

/// Test-friendly entry point: parse and execute a Lua source string,
/// returning only the validated `Fixture`. The dispatcher is built
/// internally and dropped before return; for callback-aware loading
/// use [`load_source_with_dispatcher`]. Resolves any attachment
/// `data_path` references against the current working directory; for
/// scripts that declare attachments, use [`load_source_with_dir`].
pub fn load_source(source: &str, chunk_name: &str) -> Result<Fixture, String> {
    load_source_with_dir(source, chunk_name, Path::new("."))
}

/// Like [`load_source`] but attachment `data_path` references resolve
/// relative to `fixture_dir`. Used by [`load`] and by tests that
/// exercise attachment loading from a temp dir.
pub fn load_source_with_dir(
    source: &str,
    chunk_name: &str,
    fixture_dir: &Path,
) -> Result<Fixture, String> {
    load_source_with_dispatcher_and_dir(source, chunk_name, fixture_dir).map(|(f, _)| f)
}

/// Run a Lua source string and produce both the validated `Fixture`
/// and a `Dispatcher` that retains the VM for reactive callbacks.
/// The dispatcher is always returned even if no callbacks were
/// registered - the cost is one idle `Mutex<State>` and the
/// per-protocol-handler dispatch becomes a fast no-op when no
/// handlers are anchored. Resolves attachment `data_path` references
/// against the current working directory; see
/// [`load_source_with_dispatcher_and_dir`] for the path-aware variant.
pub fn load_source_with_dispatcher(
    source: &str,
    chunk_name: &str,
) -> Result<(Fixture, Dispatcher), String> {
    load_source_with_dispatcher_and_dir(source, chunk_name, Path::new("."))
}

pub fn load_source_with_dispatcher_and_dir(
    source: &str,
    chunk_name: &str,
    fixture_dir: &Path,
) -> Result<(Fixture, Dispatcher), String> {
    let mut state = State::new();
    state.set_user_data(LuaExtras::default());
    install_builders(&mut state);

    state
        .load_string_named(source, Some(chunk_name.to_string()))
        .map_err(|e| format!("parse {chunk_name}: {e}"))?;
    state
        .call(ArgCount::Fixed(0), RetCount::Fixed(0))
        .map_err(|e| format!("run {chunk_name}: {e}"))?;

    // Pull the builder out for normalisation, but leave LuaExtras in
    // place on the State so dispatch-time `mock_done()` / `mock_fail()`
    // calls can still record their signal into `extras.exit`.
    let builder = {
        let extras = state
            .user_data_mut::<LuaExtras>()
            .ok_or_else(|| "internal: scenario extras lost".to_string())?;
        std::mem::take(&mut extras.builder)
    };

    let (raw, handlers) = builder.into_parts()?;
    let fixture = fixture::normalize_with_dir(raw, fixture_dir)?;
    let dispatcher = Dispatcher::new(state, handlers);
    Ok((fixture, dispatcher))
}

// ── Builder accumulator ─────────────────────────────────────────────

/// `(protocol, command) -> Anchor` map: the registered reactive
/// callbacks accumulated during script load. Lives in `Builder`
/// during load, moves into `Dispatcher` after.
type HandlerMap = HashMap<(String, String), Anchor>;

/// Self-termination signal raised by the script via `mock_done()` or
/// `mock_fail(reason)`. Stored on the State's user_data so it can be
/// observed both at load time and inside callback dispatch. The
/// runtime drains it and exits with the corresponding status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockExit {
    /// Clean termination: process exits 0.
    Done,
    /// Fault termination: process exits non-zero, message printed
    /// to stderr.
    Fail(String),
}

/// User-data wrapper installed on the dellingr `State`. Carries the
/// in-progress `Builder` during script load and the optional
/// `MockExit` signal both during load and through the dispatcher's
/// lifetime.
#[derive(Debug, Default)]
struct LuaExtras {
    builder: Builder,
    exit: Option<MockExit>,
}

#[derive(Debug, Default)]
struct Builder {
    name: Option<String>,
    state_token: Option<String>,
    account: Option<RawAccount>,
    mailboxes: Vec<RawMailbox>,
    emails: Vec<RawEmail>,
    /// Reactive callbacks registered via the global `on()` RustFunc.
    /// Each entry holds an `Anchor` keeping the Lua closure alive
    /// in the State's registry until the Dispatcher releases it (or
    /// the VM tears down).
    handlers: HandlerMap,
}

impl Builder {
    fn into_parts(self) -> Result<(RawFixture, HandlerMap), String> {
        let Some(name) = self.name else {
            return Err("scenario must call fixture { name = ... }".to_string());
        };
        let Some(account) = self.account else {
            return Err("scenario must call account { ... }".to_string());
        };
        let raw = RawFixture {
            name,
            state: self.state_token,
            account,
            mailboxes: self.mailboxes,
            emails: self.emails,
        };
        Ok((raw, self.handlers))
    }
}

// ── Builder registration ────────────────────────────────────────────

fn install_builders(state: &mut State) {
    state.push_rust_fn(builder_fixture);
    state.set_global("fixture");
    state.push_rust_fn(builder_account);
    state.set_global("account");
    state.push_rust_fn(builder_mailbox);
    state.set_global("mailbox");
    state.push_rust_fn(builder_email);
    state.set_global("email");
    state.push_rust_fn(builder_bulk_emails);
    state.set_global("bulk_emails");
    state.push_rust_fn(builder_bulk_threads);
    state.set_global("bulk_threads");
    state.push_rust_fn(builder_bulk_mailboxes);
    state.set_global("bulk_mailboxes");
    state.push_rust_fn(builder_on);
    state.set_global("on");
    state.push_rust_fn(builder_wait);
    state.set_global("wait");
    state.push_rust_fn(builder_mock_done);
    state.set_global("mock_done");
    state.push_rust_fn(builder_mock_fail);
    state.set_global("mock_fail");
}

/// `wait(ms)` blocks the calling thread for `ms` milliseconds. Used
/// inside callbacks to inject latency. Safe under the dispatcher
/// `Mutex<State>`: each connection holds the lock only for its own
/// dispatch turn, so a long sleep on one connection just queues
/// other connections briefly on the dispatcher mutex rather than
/// stalling unrelated protocol handling.
fn builder_wait(state: &mut State) -> dellingr::Result<u8> {
    if state.get_top() != 1 {
        return fail(state, "wait expects (ms)");
    }
    if state.typ(1) != LuaType::Number {
        return fail(state, "wait: ms must be a number");
    }
    let ms = state.to_number(1)?;
    if !ms.is_finite() || ms < 0.0 || ms > u64::MAX as f64 {
        return fail(state, "wait: ms must be a non-negative finite number");
    }
    // Bounds-checked above: 0 <= ms <= u64::MAX, finite.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ms_u64 = ms as u64;
    std::thread::sleep(std::time::Duration::from_millis(ms_u64));
    Ok(0)
}

/// `mock_done()` records a clean-exit signal. The runtime observes
/// it and shuts the listeners down with exit code 0. First call
/// wins: subsequent calls are no-ops so a chatty script doesn't
/// override an earlier `mock_fail`.
fn builder_mock_done(state: &mut State) -> dellingr::Result<u8> {
    if state.get_top() != 0 {
        return fail(state, "mock_done expects no arguments");
    }
    let extras = extras_mut(state)?;
    if extras.exit.is_none() {
        extras.exit = Some(MockExit::Done);
    }
    Ok(0)
}

/// `mock_fail(reason)` records a fault-exit signal. Reason gets
/// printed to stderr; the runtime returns a non-zero exit code.
/// First call wins.
fn builder_mock_fail(state: &mut State) -> dellingr::Result<u8> {
    if state.get_top() != 1 {
        return fail(state, "mock_fail expects (reason)");
    }
    if state.typ(1) != LuaType::String {
        return fail(state, "mock_fail: reason must be a string");
    }
    let reason = state.to_string(1)?;
    let extras = extras_mut(state)?;
    if extras.exit.is_none() {
        extras.exit = Some(MockExit::Fail(reason));
    }
    Ok(0)
}

/// Reactive-callback registration: `on(protocol, command, fn)`. The
/// callback is anchored in the dellingr registry and stored in the
/// Builder's `handlers` map keyed by (protocol, command). Replaces
/// the previous Lua-side `_sae_handlers` table approach, which
/// polluted the script's globals and was visible through
/// `with_restricted_env`.
fn builder_on(state: &mut State) -> dellingr::Result<u8> {
    if state.get_top() != 3 {
        return fail(state, "on expects (protocol, command, fn)");
    }
    if state.typ(1) != LuaType::String {
        return fail(state, "on: protocol must be a string");
    }
    if state.typ(2) != LuaType::String {
        return fail(state, "on: command must be a string");
    }
    let protocol = state.to_string(1)?;
    let command = state.to_string(2)?;
    // anchor_function_at validates that arg 3 is a function and
    // returns InvalidAnchor at registration time on mismatch.
    let anchor = state.anchor_function_at(3)?;
    let builder = builder_mut(state)?;
    if let Some(prev) = builder.handlers.insert((protocol, command), anchor) {
        // Replacing an existing handler: release the old anchor so
        // the GC can collect it. Idempotent and infallible per the
        // dellingr API contract.
        let _ = prev;
        // We can't release here because we don't have &mut State
        // (we hold &mut Builder). The old anchor leaks one slotmap
        // entry until the State drops. Acceptable: scenarios
        // typically register each callback once.
    }
    Ok(0)
}

// ── Per-builder RustFuncs ───────────────────────────────────────────

fn builder_fixture(state: &mut State) -> dellingr::Result<u8> {
    require_one_table_arg(state, "fixture")?;
    let name = read_string(state, 1, "name")?;
    let state_token = read_string_opt(state, 1, "state")?;
    let builder = builder_mut(state)?;
    if builder.name.is_some() {
        return fail(state, "fixture { ... } may only be called once");
    }
    builder.name = Some(name);
    builder.state_token = state_token;
    Ok(0)
}

fn builder_account(state: &mut State) -> dellingr::Result<u8> {
    require_one_table_arg(state, "account")?;
    let id = read_string(state, 1, "id")?;
    let name = read_string(state, 1, "name")?;
    let is_personal = read_bool_opt(state, 1, "is_personal")?.unwrap_or(true);
    let builder = builder_mut(state)?;
    if builder.account.is_some() {
        return fail(state, "account { ... } may only be called once");
    }
    builder.account = Some(RawAccount {
        id,
        name,
        is_personal,
    });
    Ok(0)
}

fn builder_mailbox(state: &mut State) -> dellingr::Result<u8> {
    require_one_table_arg(state, "mailbox")?;
    let mb = RawMailbox {
        id: read_string(state, 1, "id")?,
        name: read_string(state, 1, "name")?,
        role: read_string_opt(state, 1, "role")?,
        parent_id: read_string_opt(state, 1, "parent_id")?,
        sort_order: read_int_opt(state, 1, "sort_order")?,
        is_subscribed: read_bool_opt(state, 1, "is_subscribed")?,
    };
    builder_mut(state)?.mailboxes.push(mb);
    Ok(0)
}

/// Bulk-generate synthetic emails directly into the builder, avoiding
/// per-email Lua allocation overhead. Useful for "test sync against
/// 100k emails" scenarios. Determinism: same `seed` + same opts ->
/// same emails out.
///
/// ```lua
/// bulk_emails({
///   count = 100000,
///   mailbox = "mb-inbox",
///   seed = 42,                            -- default 42
///   start_at = "2026-01-01T00:00:00Z",    -- default
///   interval_seconds = 60,                -- default (1 email/min)
///   id_prefix = "bulk",                   -- default
/// })
/// ```
//
// Casts inside the loop body are validated up-front (`count` capped
// at `MAX_BULK_COUNT`, sign-checked) and the seed is intentionally
// reinterpreted as `u64` for `SmallRng`. Suppressing the cast lints
// avoids per-iteration `try_from` checks that wouldn't catch
// anything the bounds-check above doesn't already reject.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
fn builder_bulk_emails(state: &mut State) -> dellingr::Result<u8> {
    require_one_table_arg(state, "bulk_emails")?;
    let count = read_int(state, 1, "count")?;
    if count < 0 {
        return fail(state, "bulk_emails count must be non-negative");
    }
    if count > MAX_BULK_COUNT {
        return fail(
            state,
            format!("bulk_emails count {count} exceeds MAX_BULK_COUNT={MAX_BULK_COUNT}"),
        );
    }
    let mailbox = read_string(state, 1, "mailbox")?;
    let seed = read_int_opt(state, 1, "seed")?.unwrap_or(42) as u64;
    let start_at_raw = read_string_opt(state, 1, "start_at")?
        .unwrap_or_else(|| "2026-01-01T00:00:00Z".to_string());
    let interval_seconds = read_int_opt(state, 1, "interval_seconds")?.unwrap_or(60);
    let id_prefix = read_string_opt(state, 1, "id_prefix")?
        .unwrap_or_else(|| "bulk".to_string());

    let start = match DateTime::parse_from_rfc3339(&start_at_raw) {
        Ok(dt) => dt.with_timezone(&Utc),
        Err(e) => return fail(state, format!("bulk_emails bad start_at: {e}")),
    };

    // Validate the worst-case timestamp arithmetic upfront. With
    // count capped at MAX_BULK_COUNT this can only blow up on
    // pathologically large interval_seconds, but the borrow checker
    // makes mid-loop `fail()` painful (state vs builder), so we do
    // the check here while we still hold &State.
    if count > 0 {
        let last_offset = (count - 1)
            .checked_mul(interval_seconds)
            .and_then(|s| start.checked_add_signed(Duration::seconds(s)));
        if last_offset.is_none() {
            return fail(
                state,
                "bulk_emails extends past the chrono datetime range; reduce count or interval_seconds",
            );
        }
    }

    let mut rng = SmallRng::seed_from_u64(seed);
    let builder = builder_mut(state)?;
    builder.emails.reserve(count as usize);

    // Emit ids zero-padded so lex order matches numeric order even at
    // millions-of-emails scale.
    let pad = pad_width(count);
    for i in 0..count {
        let id = format!("{id_prefix}-{i:0pad$}");
        let received_at = start + Duration::seconds(i * interval_seconds);
        let (from_name, from_email) = templates::pick_address(&mut rng);
        let (_, to_email) = templates::pick_address(&mut rng);
        let subject_tmpl = templates::SUBJECT_TEMPLATES
            [(seed.wrapping_add(i as u64) as usize) % templates::SUBJECT_TEMPLATES.len()];
        let body_tmpl = templates::BODY_TEMPLATES
            [(seed.wrapping_add(i as u64) as usize) % templates::BODY_TEMPLATES.len()];
        let subject = templates::fill_template(subject_tmpl, &mut rng);
        let body = templates::fill_template(body_tmpl, &mut rng);

        builder.emails.push(RawEmail {
            id: id.clone(),
            thread_id: Some(id.clone()),
            mailbox_ids: vec![mailbox.clone()],
            received_at: received_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            from: Some(RawAddress::Full {
                name: Some(from_name),
                email: from_email,
            }),
            to: vec![RawAddress::Bare(to_email)],
            subject: Some(subject),
            message_id: vec![format!("<{id}@{}>", templates::DOMAINS[0])],
            body_text: Some(body),
            ..Default::default()
        });
    }
    Ok(0)
}

fn pad_width(count: i64) -> usize {
    // count.max(1) >= 1, so the cast is safe.
    #[allow(clippy::cast_sign_loss)]
    let mut n = count.max(1) as u64;
    let mut w = 0;
    while n > 0 {
        n /= 10;
        w += 1;
    }
    w.max(1)
}

/// Bulk-generate threaded conversations. Each thread gets
/// `messages_per_thread` messages sharing a `thread_id`, with
/// `In-Reply-To` and `References` headers chaining replies back to
/// prior messages. Subjects on replies are prefixed `Re: `; the
/// first message gets a synthesised subject.
///
/// ```lua
/// bulk_threads({
///   count = 100,                          -- required: thread count
///   mailbox = "mb-inbox",                 -- required
///   messages_per_thread = 3,              -- default 3
///   seed = 42,                            -- default 42
///   start_at = "2026-01-01T00:00:00Z",    -- default
///   thread_interval_seconds = 3600,       -- default (1h between thread starts)
///   reply_interval_seconds = 300,         -- default (5min between replies)
///   id_prefix = "thread",                 -- default
/// })
/// ```
//
// Same cast story as `builder_bulk_emails`: `count` and
// `messages_per_thread` are bounded above; the seed is
// intentionally reinterpreted as `u64`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
fn builder_bulk_threads(state: &mut State) -> dellingr::Result<u8> {
    require_one_table_arg(state, "bulk_threads")?;
    let count = read_int(state, 1, "count")?;
    if count < 0 {
        return fail(state, "bulk_threads count must be non-negative");
    }
    if count > MAX_BULK_COUNT {
        return fail(
            state,
            format!("bulk_threads count {count} exceeds MAX_BULK_COUNT={MAX_BULK_COUNT}"),
        );
    }
    let mailbox = read_string(state, 1, "mailbox")?;
    let messages_per_thread = read_int_opt(state, 1, "messages_per_thread")?.unwrap_or(3);
    if messages_per_thread < 1 {
        return fail(state, "bulk_threads messages_per_thread must be >= 1");
    }
    let seed = read_int_opt(state, 1, "seed")?.unwrap_or(42) as u64;
    let start_at_raw = read_string_opt(state, 1, "start_at")?
        .unwrap_or_else(|| "2026-01-01T00:00:00Z".to_string());
    let thread_interval = read_int_opt(state, 1, "thread_interval_seconds")?.unwrap_or(3600);
    let reply_interval = read_int_opt(state, 1, "reply_interval_seconds")?.unwrap_or(300);
    let id_prefix = read_string_opt(state, 1, "id_prefix")?
        .unwrap_or_else(|| "thread".to_string());

    let start = match DateTime::parse_from_rfc3339(&start_at_raw) {
        Ok(dt) => dt.with_timezone(&Utc),
        Err(e) => return fail(state, format!("bulk_threads bad start_at: {e}")),
    };

    // Worst-case timestamp: last thread's last message =
    // start + (count-1)*thread_interval + (messages_per_thread-1)*reply_interval.
    if count > 0 {
        let thread_offset = (count - 1).checked_mul(thread_interval);
        let reply_offset = (messages_per_thread - 1).checked_mul(reply_interval);
        let last = thread_offset
            .zip(reply_offset)
            .and_then(|(t, r)| t.checked_add(r))
            .and_then(|s| start.checked_add_signed(Duration::seconds(s)));
        if last.is_none() {
            return fail(
                state,
                "bulk_threads extends past the chrono datetime range; reduce count, messages_per_thread, or interval seconds",
            );
        }
    }

    let mut rng = SmallRng::seed_from_u64(seed);
    let messages_per_thread = messages_per_thread as usize;
    let builder = builder_mut(state)?;
    builder.emails.reserve(count as usize * messages_per_thread);

    let thread_pad = pad_width(count);
    let msg_pad = pad_width(messages_per_thread as i64);

    for thread_i in 0..count {
        let thread_id = format!("{id_prefix}-{thread_i:0thread_pad$}");
        let thread_start = start + Duration::seconds(thread_i * thread_interval);
        let first_subject_tmpl = templates::SUBJECT_TEMPLATES
            [(seed.wrapping_add(thread_i as u64) as usize) % templates::SUBJECT_TEMPLATES.len()];
        let first_subject = templates::fill_template(first_subject_tmpl, &mut rng);

        let mut prior_message_ids: Vec<String> = Vec::with_capacity(messages_per_thread);
        for msg_i in 0..messages_per_thread {
            let n = msg_i + 1;
            let msg_id = format!("{thread_id}-{n:0msg_pad$}");
            let received_at = thread_start + Duration::seconds(msg_i as i64 * reply_interval);
            let subject = if msg_i == 0 {
                first_subject.clone()
            } else {
                format!("Re: {first_subject}")
            };
            let body_tmpl = templates::BODY_TEMPLATES
                [(seed.wrapping_add(thread_i as u64).wrapping_add(msg_i as u64) as usize)
                    % templates::BODY_TEMPLATES.len()];
            let body = templates::fill_template(body_tmpl, &mut rng);

            let (from_name, from_email) = templates::pick_address(&mut rng);
            let (_, to_email) = templates::pick_address(&mut rng);
            let message_id_header = format!("<{msg_id}@{}>", templates::DOMAINS[0]);

            let mut email = RawEmail {
                id: msg_id,
                thread_id: Some(thread_id.clone()),
                mailbox_ids: vec![mailbox.clone()],
                received_at: received_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                from: Some(RawAddress::Full {
                    name: Some(from_name),
                    email: from_email,
                }),
                to: vec![RawAddress::Bare(to_email)],
                subject: Some(subject),
                message_id: vec![message_id_header.clone()],
                body_text: Some(body),
                ..Default::default()
            };
            if msg_i > 0 {
                // Reply: In-Reply-To = the immediate parent;
                // References = the full chain back to the root.
                email.in_reply_to =
                    vec![prior_message_ids.last().expect("non-empty").clone()];
                email.references = prior_message_ids.clone();
            }
            prior_message_ids.push(message_id_header);
            builder.emails.push(email);
        }
    }
    Ok(0)
}

/// Bulk-generate a hierarchical mailbox tree directly into the
/// builder. Useful for exercising client-side folder-tree handling
/// at scale. Determinism: same `seed` + same opts -> byte-identical
/// mailboxes out.
///
/// Tree shape is breadth-first: mailbox `i` (0-indexed) has parent
/// `(i-1) / branching` for `i >= 1`; the root (`i = 0`) has no
/// parent. So `branching = 4, count = 21` gives one root, four
/// first-level children, then 16 grandchildren. `sort_order` is the
/// 0-indexed BFS position. Names are picked deterministically from
/// the `PROJECTS` template pool, suffixed with the index when the
/// pool wraps so collisions stay visible in test output.
///
/// ```lua
/// bulk_mailboxes({
///   count = 100,           -- required
///   branching = 4,         -- default 4 (children per parent)
///   seed = 42,             -- default 42
///   id_prefix = "mb",      -- default "mb"
/// })
/// ```
//
// Same cast story as `builder_bulk_emails`: `count` is bounded above
// and the seed is intentionally reinterpreted as `u64`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
fn builder_bulk_mailboxes(state: &mut State) -> dellingr::Result<u8> {
    require_one_table_arg(state, "bulk_mailboxes")?;
    let count = read_int(state, 1, "count")?;
    if count < 0 {
        return fail(state, "bulk_mailboxes count must be non-negative");
    }
    if count > MAX_BULK_COUNT {
        return fail(
            state,
            format!("bulk_mailboxes count {count} exceeds MAX_BULK_COUNT={MAX_BULK_COUNT}"),
        );
    }
    let branching = read_int_opt(state, 1, "branching")?.unwrap_or(4);
    if branching < 1 {
        return fail(state, "bulk_mailboxes branching must be >= 1");
    }
    let seed = read_int_opt(state, 1, "seed")?.unwrap_or(42) as u64;
    let id_prefix = read_string_opt(state, 1, "id_prefix")?
        .unwrap_or_else(|| "mb".to_string());

    let builder = builder_mut(state)?;
    builder.mailboxes.reserve(count as usize);

    let pad = pad_width(count);
    for i in 0..count {
        let id = format!("{id_prefix}-{i:0pad$}");
        let parent_id = if i == 0 {
            None
        } else {
            // Integer division: i - 1 >= 0 and branching >= 1.
            let p = (i - 1) / branching;
            Some(format!("{id_prefix}-{p:0pad$}"))
        };
        // Stable name pick. Suffix with the index once the pool
        // wraps so a 100-mailbox tree doesn't collapse into 20
        // duplicate "Atlas" / "Beacon" entries.
        let base = templates::PROJECTS
            [(seed.wrapping_add(i as u64) as usize) % templates::PROJECTS.len()];
        let name = if (count as usize) > templates::PROJECTS.len() {
            format!("{base} {i:0pad$}")
        } else {
            base.to_string()
        };
        builder.mailboxes.push(RawMailbox {
            id,
            name,
            role: None,
            parent_id,
            sort_order: Some(i),
            is_subscribed: None,
        });
    }
    Ok(0)
}

fn builder_email(state: &mut State) -> dellingr::Result<u8> {
    require_one_table_arg(state, "email")?;
    let em = RawEmail {
        id: read_string(state, 1, "id")?,
        thread_id: read_string_opt(state, 1, "thread_id")?,
        mailbox_ids: read_string_array(state, 1, "mailbox_ids")?,
        keywords: read_string_array_opt(state, 1, "keywords")?,
        size: read_int_opt(state, 1, "size")?,
        received_at: read_string(state, 1, "received_at")?,
        sent_at: read_string_opt(state, 1, "sent_at")?,
        from: read_address_opt(state, 1, "from")?,
        to: read_address_array_opt(state, 1, "to")?,
        cc: read_address_array_opt(state, 1, "cc")?,
        bcc: read_address_array_opt(state, 1, "bcc")?,
        reply_to: read_address_array_opt(state, 1, "reply_to")?,
        subject: read_string_opt(state, 1, "subject")?,
        preview: read_string_opt(state, 1, "preview")?,
        message_id: read_string_array_opt(state, 1, "message_id")?,
        in_reply_to: read_string_array_opt(state, 1, "in_reply_to")?,
        references: read_string_array_opt(state, 1, "references")?,
        has_attachment: read_bool_opt(state, 1, "has_attachment")?,
        body_text: read_string_opt(state, 1, "body_text")?,
        attachments: read_attachment_array_opt(state, 1, "attachments")?,
    };
    builder_mut(state)?.emails.push(em);
    Ok(0)
}

fn read_attachment_array_opt(
    state: &mut State,
    t: isize,
    key: &str,
) -> dellingr::Result<Vec<RawAttachment>> {
    let typ = lookup(state, t, key)?;
    let result = match typ {
        LuaType::Nil => Ok(Vec::new()),
        LuaType::Table => read_attachment_array_at_top(state, key),
        _ => fail(state, format!("field {key:?} must be an array")),
    };
    state.pop(1);
    result
}

// Same stack-top cast story as `read_string_array_at_top`.
#[allow(clippy::cast_possible_wrap)]
fn read_attachment_array_at_top(
    state: &mut State,
    key: &str,
) -> dellingr::Result<Vec<RawAttachment>> {
    let arr = state.get_top() as isize;
    let len = state.table_len(arr);
    let mut out = Vec::with_capacity(len);
    for i in 1..=len {
        state.push_number(i as f64);
        state.get_table_raw(arr)?;
        if state.typ(-1) != LuaType::Table {
            state.pop(1);
            return fail(state, format!("field {key:?} entry {i} must be a table"));
        }
        let entry_idx = state.get_top() as isize;
        let raw = RawAttachment {
            blob_id: read_string(state, entry_idx, "blob_id")?,
            name: read_string(state, entry_idx, "name")?,
            content_type: read_string(state, entry_idx, "content_type")?,
            size: read_int_opt(state, entry_idx, "size")?,
            disposition: read_string_opt(state, entry_idx, "disposition")?,
            cid: read_string_opt(state, entry_idx, "cid")?,
            data_path: read_string(state, entry_idx, "data_path")?,
        };
        state.pop(1);
        out.push(raw);
    }
    Ok(out)
}

// ── Stack helpers ───────────────────────────────────────────────────

fn require_one_table_arg(state: &State, name: &str) -> dellingr::Result<()> {
    if state.get_top() != 1 {
        return fail(state, format!("{name} expects exactly one table argument"));
    }
    if state.typ(1) != LuaType::Table {
        return fail(state, format!("{name} argument must be a table"));
    }
    Ok(())
}

fn builder_mut(state: &mut State) -> dellingr::Result<&mut Builder> {
    Ok(&mut extras_mut(state)?.builder)
}

fn extras_mut(state: &mut State) -> dellingr::Result<&mut LuaExtras> {
    let missing = state.error(ErrorKind::InternalError("extras user_data missing".into()));
    state.user_data_mut::<LuaExtras>().ok_or(missing)
}

fn fail<T>(state: &State, msg: impl Into<String>) -> dellingr::Result<T> {
    Err(state.error(ErrorKind::InternalError(msg.into())))
}

/// Push the value at `t[key]` to the stack top; returns its `LuaType`.
/// On entry the input table is at index `t`. On exit the value is at
/// `-1` and the caller is responsible for popping it.
fn lookup(state: &mut State, t: isize, key: &str) -> dellingr::Result<LuaType> {
    state.push_string(key);
    state.get_table_raw(t)?;
    Ok(state.typ(-1))
}

fn read_string(state: &mut State, t: isize, key: &str) -> dellingr::Result<String> {
    let typ = lookup(state, t, key)?;
    if typ == LuaType::Nil {
        state.pop(1);
        return fail(state, format!("missing required field {key:?}"));
    }
    if typ != LuaType::String {
        state.pop(1);
        return fail(state, format!("field {key:?} must be a string"));
    }
    let s = state.to_string(-1)?;
    state.pop(1);
    Ok(s)
}

fn read_string_opt(state: &mut State, t: isize, key: &str) -> dellingr::Result<Option<String>> {
    let typ = lookup(state, t, key)?;
    let result = match typ {
        LuaType::Nil => Ok(None),
        LuaType::String => Ok(Some(state.to_string(-1)?)),
        _ => fail(state, format!("field {key:?} must be a string")),
    };
    state.pop(1);
    result
}

fn read_bool_opt(state: &mut State, t: isize, key: &str) -> dellingr::Result<Option<bool>> {
    let typ = lookup(state, t, key)?;
    let result = match typ {
        LuaType::Nil => Ok(None),
        LuaType::Boolean => Ok(Some(state.to_boolean(-1))),
        _ => fail(state, format!("field {key:?} must be a boolean")),
    };
    state.pop(1);
    result
}

fn read_int(state: &mut State, t: isize, key: &str) -> dellingr::Result<i64> {
    match read_int_opt(state, t, key)? {
        Some(n) => Ok(n),
        None => fail(state, format!("missing required field {key:?}")),
    }
}

fn read_int_opt(state: &mut State, t: isize, key: &str) -> dellingr::Result<Option<i64>> {
    let typ = lookup(state, t, key)?;
    let result = match typ {
        LuaType::Nil => Ok(None),
        LuaType::Number => {
            let n = state.to_number(-1)?;
            if n.fract() != 0.0 {
                let msg = format!("field {key:?} must be an integer");
                state.pop(1);
                return fail(state, msg);
            }
            // f64 -> i64 cast bounded by the i64 range check above.
            #[allow(clippy::cast_possible_truncation)]
            if !(I64_MIN_F64..=I64_MAX_F64).contains(&n) {
                let msg = format!("field {key:?} integer is out of i64 range");
                state.pop(1);
                return fail(state, msg);
            }
            #[allow(clippy::cast_possible_truncation)]
            Ok(Some(n as i64))
        }
        _ => fail(state, format!("field {key:?} must be a number")),
    };
    state.pop(1);
    result
}

fn read_string_array(state: &mut State, t: isize, key: &str) -> dellingr::Result<Vec<String>> {
    let typ = lookup(state, t, key)?;
    if typ == LuaType::Nil {
        state.pop(1);
        return fail(state, format!("missing required field {key:?}"));
    }
    if typ != LuaType::Table {
        state.pop(1);
        return fail(state, format!("field {key:?} must be an array"));
    }
    let result = read_string_array_at_top(state, key);
    state.pop(1);
    result
}

fn read_string_array_opt(
    state: &mut State,
    t: isize,
    key: &str,
) -> dellingr::Result<Vec<String>> {
    let typ = lookup(state, t, key)?;
    let result = match typ {
        LuaType::Nil => Ok(Vec::new()),
        LuaType::Table => read_string_array_at_top(state, key),
        _ => fail(state, format!("field {key:?} must be an array")),
    };
    state.pop(1);
    result
}

/// Read an array of strings whose table is at the top of the stack.
/// Does not pop the table; caller pops.
//
// `state.get_top()` returns `usize` but the dellingr table-index API
// takes `isize`. The Lua stack top is bounded by the runtime's stack
// limit (a few hundred at most), so the cast is safe.
#[allow(clippy::cast_possible_wrap)]
fn read_string_array_at_top(state: &mut State, key: &str) -> dellingr::Result<Vec<String>> {
    let arr = state.get_top() as isize;
    let len = state.table_len(arr);
    let mut out = Vec::with_capacity(len);
    for i in 1..=len {
        state.push_number(i as f64);
        state.get_table_raw(arr)?;
        if state.typ(-1) != LuaType::String {
            state.pop(1);
            return fail(state, format!("field {key:?} entry {i} must be a string"));
        }
        out.push(state.to_string(-1)?);
        state.pop(1);
    }
    Ok(out)
}

fn read_address_opt(
    state: &mut State,
    t: isize,
    key: &str,
) -> dellingr::Result<Option<RawAddress>> {
    let typ = lookup(state, t, key)?;
    let result = match typ {
        LuaType::Nil => Ok(None),
        LuaType::String => Ok(Some(RawAddress::Bare(state.to_string(-1)?))),
        LuaType::Table => Ok(Some(read_address_table_at_top(state, key)?)),
        _ => fail(state, format!("field {key:?} must be a string or table")),
    };
    state.pop(1);
    result
}

fn read_address_array_opt(
    state: &mut State,
    t: isize,
    key: &str,
) -> dellingr::Result<Vec<RawAddress>> {
    let typ = lookup(state, t, key)?;
    let result = match typ {
        LuaType::Nil => Ok(Vec::new()),
        LuaType::Table => read_address_array_at_top(state, key),
        _ => fail(state, format!("field {key:?} must be an array")),
    };
    state.pop(1);
    result
}

// Same stack-top cast story as `read_string_array_at_top`.
#[allow(clippy::cast_possible_wrap)]
fn read_address_array_at_top(
    state: &mut State,
    key: &str,
) -> dellingr::Result<Vec<RawAddress>> {
    let arr = state.get_top() as isize;
    let len = state.table_len(arr);
    let mut out = Vec::with_capacity(len);
    for i in 1..=len {
        state.push_number(i as f64);
        state.get_table_raw(arr)?;
        let typ = state.typ(-1);
        let entry = match typ {
            LuaType::String => RawAddress::Bare(state.to_string(-1)?),
            LuaType::Table => read_address_table_at_top(state, key)?,
            _ => {
                state.pop(1);
                return fail(state, format!("field {key:?} entry {i} bad shape"));
            }
        };
        state.pop(1);
        out.push(entry);
    }
    Ok(out)
}

/// Read a `{name = ?, email = ...}` table at the stack top into a
/// `RawAddress::Full`. Does not pop the table; caller pops.
//
// Same stack-top cast story as `read_string_array_at_top`.
#[allow(clippy::cast_possible_wrap)]
fn read_address_table_at_top(state: &mut State, key: &str) -> dellingr::Result<RawAddress> {
    let t = state.get_top() as isize;
    let name = read_string_opt(state, t, "name")?;
    let typ = lookup(state, t, "email")?;
    if typ != LuaType::String {
        state.pop(1);
        return fail(
            state,
            format!("field {key:?} address table needs a string `email`"),
        );
    }
    let email = state.to_string(-1)?;
    state.pop(1);
    Ok(RawAddress::Full { name, email })
}

// ── Reactive callback dispatcher ────────────────────────────────────

/// Override the protocol layer's default response for a request.
/// Returned from [`Dispatcher::dispatch`].
#[derive(Debug, Clone)]
pub enum Override {
    /// No handler matched (or the handler returned `nil`). Caller
    /// uses the default behaviour.
    None,
    /// Replace the response with a tagged status + message. For IMAP
    /// this becomes `<tag> <status> <message>`. For HTTP protocols
    /// it could map to a status code in v1.
    Tagged {
        /// Status token, e.g. `"NO"`, `"BAD"`, or `"OK"` for IMAP.
        status: String,
        /// Human-readable message tail.
        message: String,
    },
}

/// Holds the dellingr `State` after fixture loading so reactive
/// callbacks can be invoked. `Send` (dellingr 0.2 onward), wrapped
/// in `Mutex` because `State` is `!Sync` and callback dispatch
/// requires exclusive access.
///
/// Handlers are stored Rust-side as a map of `(protocol, command)
/// -> Anchor` (dellingr 0.3 onward). The Anchor keeps the Lua
/// closure alive in the State's registry without polluting the
/// script's globals. Per-(protocol, command) call counts are
/// tracked alongside so `req.call_index` is always a strictly
/// increasing 1-based count for that exact wire event.
pub struct Dispatcher {
    inner: Mutex<DispatcherInner>,
}

struct DispatcherInner {
    state: State,
    handlers: HandlerMap,
    call_counts: HashMap<(String, String), u64>,
    /// Latched mock-exit signal. Drained out of `LuaExtras` after
    /// load and after each dispatch; sticky once set.
    exit: Option<MockExit>,
}

impl Dispatcher {
    fn new(state: State, handlers: HandlerMap) -> Self {
        let me = Self {
            inner: Mutex::new(DispatcherInner {
                state,
                handlers,
                call_counts: HashMap::new(),
                exit: None,
            }),
        };
        // If the script signalled mock_done() / mock_fail() at load
        // time, drain it into the dispatcher so observers see it
        // even before any dispatch happens.
        {
            let mut inner = me.inner.lock().expect("dispatcher mutex poisoned");
            DispatcherInner::drain_exit(&mut inner);
        }
        me
    }

    /// Run any registered callback for `(protocol, command)`. The
    /// `build_req` closure receives the dellingr `State` after a
    /// fresh `req` table has been put on top of the stack with
    /// `call_index` already populated; the closure adds any
    /// protocol-specific fields to that table and must leave the
    /// stack balanced (the table at the top, no extra items).
    pub fn dispatch<F>(&self, protocol: &str, command: &str, build_req: F) -> Override
    where
        F: FnOnce(&mut State) -> dellingr::Result<()>,
    {
        let mut inner = self
            .inner
            .lock()
            .expect("dispatcher mutex poisoned");
        let key = (protocol.to_string(), command.to_string());
        let anchor = match inner.handlers.get(&key).copied() {
            Some(a) => a,
            None => return Override::None,
        };
        let call_index = {
            let entry = inner.call_counts.entry(key).or_insert(0);
            *entry += 1;
            *entry
        };
        let result = run_dispatch(&mut inner.state, anchor, call_index, build_req);
        DispatcherInner::drain_exit(&mut inner);
        result
    }

    /// Snapshot of any pending mock-exit signal. Non-destructive.
    /// Returns the same value across calls until the runtime acts
    /// on it.
    pub fn current_exit(&self) -> Option<MockExit> {
        self.inner
            .lock()
            .expect("dispatcher mutex poisoned")
            .exit
            .clone()
    }

    /// Async future that resolves the first time `mock_done()` or
    /// `mock_fail()` fires (either at load time or inside a
    /// callback). Polls the dispatcher mutex every 100ms; a real
    /// notification channel would be tidier but the runtime only
    /// awaits this once across the process lifetime.
    pub async fn wait_for_exit(&self) -> MockExit {
        loop {
            if let Some(exit) = self.current_exit() {
                return exit;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
}

impl DispatcherInner {
    fn drain_exit(inner: &mut Self) {
        if inner.exit.is_some() {
            return;
        }
        if let Some(extras) = inner.state.user_data_mut::<LuaExtras>() {
            inner.exit = extras.exit.take();
        }
    }
}

fn run_dispatch<F>(
    state: &mut State,
    anchor: Anchor,
    call_index: u64,
    build_req: F,
) -> Override
where
    F: FnOnce(&mut State) -> dellingr::Result<()>,
{
    state.new_table();
    // Built-in field call_index. dellingr's set_table_raw uses the
    // standard Lua C API order: push KEY first, then VALUE on top,
    // then call - matching `lua_settable`.
    state.push_string("call_index");
    state.push_number(call_index as f64);
    if state.set_table_raw(-3).is_err() {
        state.set_top(0);
        return Override::None;
    }
    if build_req(state).is_err() {
        state.set_top(0);
        return Override::None;
    }
    // Stack: [req_table]. call_anchor inserts the function below the
    // 1 arg, then runs the call - so net effect is f(req).
    if state
        .call_anchor(anchor, ArgCount::Fixed(1), RetCount::Fixed(1))
        .is_err()
    {
        state.set_top(0);
        return Override::None;
    }
    // Stack: [result]
    let out = decode_override(state);
    state.pop(1);
    out
}

fn decode_override(state: &mut State) -> Override {
    match state.typ(-1) {
        LuaType::Nil => Override::None,
        LuaType::Table => {
            // Capture the table's absolute index BEFORE the field
            // lookups push their keys onto the stack - using -1 here
            // would shift to point at the just-pushed key after
            // push_string runs. Stack top is bounded so usize -> isize
            // is safe.
            #[allow(clippy::cast_possible_wrap)]
            let table_idx = state.get_top() as isize;
            let status = field_string(state, table_idx, "status")
                .unwrap_or_else(|| "BAD".to_string());
            let message = field_string(state, table_idx, "message")
                .unwrap_or_else(|| "scripted".to_string());
            Override::Tagged { status, message }
        }
        // Anything else (number, string, bool, function) is treated
        // as "weird, don't override." The script is misshapen but
        // the test peer should not crash.
        _ => Override::None,
    }
}

/// Read `t[key]` as a string. `t` MUST be either a positive absolute
/// stack index or already adjusted to account for pushes; passing a
/// negative relative index that points at the table is unsafe
/// because pushing the key shifts the relative position.
fn field_string(state: &mut State, t: isize, key: &str) -> Option<String> {
    state.push_string(key);
    if state.get_table_raw(t).is_err() {
        return None;
    }
    let result = if state.typ(-1) == LuaType::String {
        state.to_string(-1).ok()
    } else {
        None
    };
    state.pop(1);
    result
}

// ── Stack-builder helpers for per-protocol req tables ───────────────
//
// These are exposed so each protocol's dispatch site can populate
// a `req` table on top of the dellingr stack with one-liners
// instead of inlining the push/set_table_raw dance.

/// Set `req[key] = value` where the table is at the top of the
/// stack. Pops nothing (the table stays on top). Uses the standard
/// Lua C API order: push KEY first, then VALUE on top, then call
/// `set_table_raw` - matching `lua_settable`.
pub fn req_set_str(state: &mut State, key: &str, value: &str) -> dellingr::Result<()> {
    state.push_string(key);
    state.push_string(value);
    state.set_table_raw(-3)
}

/// Set `req[key] = value` (number) where the table is at the top of
/// the stack.
pub fn req_set_int(state: &mut State, key: &str, value: i64) -> dellingr::Result<()> {
    state.push_string(key);
    state.push_number(value as f64);
    state.set_table_raw(-3)
}

/// Set `req[key] = { values[0], values[1], ... }` (a 1-based Lua
/// array of strings) where the parent `req` table is at the top of
/// the stack on entry. Used to surface request shapes like
/// `Email/get`'s `ids[]` or IMAP UID lists to callbacks. On exit
/// the parent table is back on top, balanced.
//
// Stack-top capture mirrors the read-side helpers: `state.get_top()`
// returns `usize` but the table-index API takes `isize`. The Lua
// stack is bounded by the runtime's stack limit so the cast is safe.
#[allow(clippy::cast_possible_wrap)]
pub fn req_set_str_array<I, S>(state: &mut State, key: &str, values: I) -> dellingr::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    state.push_string(key);
    state.new_table();
    let arr = state.get_top() as isize;
    for (i, v) in values.into_iter().enumerate() {
        state.push_number((i + 1) as f64);
        state.push_string(v.as_ref());
        state.set_table_raw(arr)?;
    }
    state.set_table_raw(-3)
}

/// Wrap a [`Dispatcher`] in `Arc` for sharing across protocol
/// listeners.
pub fn into_arc(d: Dispatcher) -> Arc<Dispatcher> {
    Arc::new(d)
}
