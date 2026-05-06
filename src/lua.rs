//! Lua-driven fixture loader, backed by dellingr.
//!
//! v0 surface: a script can call the global builders `fixture {...}`,
//! `account {...}`, `mailbox {...}`, and `email {...}` to populate the
//! same `RawFixture` shape the TOML loader produces. The script runs
//! once at process start; after it returns, the accumulated state is
//! handed to [`fixture::normalize`] for cross-reference validation.
//!
//! Reactive callbacks (`on(protocol, command, function)` style) are
//! deliberately NOT yet wired - this commit just proves we can embed
//! dellingr and produce a `Fixture` byte-identical to what TOML would.

use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use dellingr::{ArgCount, LuaType, RetCount, State, error::ErrorKind};
use rand::SeedableRng;
use rand::rngs::SmallRng;

use crate::fixture::{self, Fixture, RawAccount, RawAddress, RawEmail, RawFixture, RawMailbox};
use crate::templates;

/// Read a `.lua` scenario file and produce a fully validated `Fixture`.
pub fn load(path: &Path) -> Result<Fixture, String> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    let chunk_name = format!("@{}", path.display());
    load_source(&source, &chunk_name)
}

/// Run a Lua source string through the same pipeline as
/// [`load`]. Useful for tests that want to skip the file-system dance.
pub fn load_source(source: &str, chunk_name: &str) -> Result<Fixture, String> {
    let mut state = State::new();
    state.set_user_data(Builder::default());
    install_builders(&mut state);

    state
        .load_string_named(source, Some(chunk_name.to_string()))
        .map_err(|e| format!("parse {chunk_name}: {e}"))?;
    state
        .call(ArgCount::Fixed(0), RetCount::Fixed(0))
        .map_err(|e| format!("run {chunk_name}: {e}"))?;

    let builder = state
        .user_data_mut::<Builder>()
        .map(std::mem::take)
        .ok_or_else(|| "internal: scenario builder lost".to_string())?;

    let raw = builder.into_raw_fixture()?;
    fixture::normalize(raw)
}

// ── Builder accumulator ─────────────────────────────────────────────

#[derive(Debug, Default)]
struct Builder {
    name: Option<String>,
    state_token: Option<String>,
    account: Option<RawAccount>,
    mailboxes: Vec<RawMailbox>,
    emails: Vec<RawEmail>,
}

impl Builder {
    fn into_raw_fixture(self) -> Result<RawFixture, String> {
        let Some(name) = self.name else {
            return Err("scenario must call fixture { name = ... }".to_string());
        };
        let Some(account) = self.account else {
            return Err("scenario must call account { ... }".to_string());
        };
        Ok(RawFixture {
            name,
            state: self.state_token,
            account,
            mailboxes: self.mailboxes,
            emails: self.emails,
        })
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
fn builder_bulk_emails(state: &mut State) -> dellingr::Result<u8> {
    require_one_table_arg(state, "bulk_emails")?;
    let count = read_int(state, 1, "count")?;
    if count < 0 {
        return fail(state, "bulk_emails count must be non-negative");
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

    let mut rng = SmallRng::seed_from_u64(seed);
    let builder = builder_mut(state)?;
    builder.emails.reserve(count as usize);

    // Emit ids zero-padded so lex order matches numeric order even at
    // millions-of-emails scale.
    let pad = pad_width(count);
    for i in 0..count {
        let id = format!("{id_prefix}-{i:0pad$}", pad = pad);
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
    let mut n = count.max(1) as u64;
    let mut w = 0;
    while n > 0 {
        n /= 10;
        w += 1;
    }
    w.max(1)
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
    };
    builder_mut(state)?.emails.push(em);
    Ok(0)
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
    let missing = state.error(ErrorKind::InternalError("builder user_data missing".into()));
    state.user_data_mut::<Builder>().ok_or(missing)
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
