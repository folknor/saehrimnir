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
    self, ChangeOp, ChangeStep, ContactEmail, Fixture, RawAccount, RawAddress, RawAttachment,
    RawEmail, RawEventAttendee, RawFixture, RawMailbox, RawOAuth,
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
    let source =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
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
    install_builders(&mut state).map_err(|e| format!("install builders for {chunk_name}: {e}"))?;

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

    let (raw, change_script, handlers) = builder.into_parts()?;
    let mut fixture = fixture::normalize_with_dir(raw, fixture_dir)?;
    fixture.change_script = change_script;
    // Announce triggers name change-script steps, and the script is
    // only attached here (normalize never sees it on the Lua path).
    fixture::validate_announce_triggers(&fixture)?;
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
    accounts: Vec<RawAccount>,
    mailboxes: Vec<RawMailbox>,
    emails: Vec<RawEmail>,
    oauth: Option<RawOAuth>,
    /// CalDAV server-capability block set via `caldav({...})`.
    caldav: Option<crate::fixture::RawCalDav>,
    /// Request-ordered change-script triggers via `announce({...})`.
    announce: Vec<crate::fixture::RawAnnounce>,
    /// Discovery entries accumulated via `discovery({...})`. Keyed
    /// by prefix; duplicate prefixes are rejected at builder time.
    discovery: std::collections::BTreeMap<String, crate::fixture::RawDiscoveryEntry>,
    /// Reactive callbacks registered via the global `on()` RustFunc.
    /// Each entry holds an `Anchor` keeping the Lua closure alive
    /// in the State's registry until the Dispatcher releases it (or
    /// the VM tears down).
    handlers: HandlerMap,
    /// Incremental-sync script accumulated via `change({...})`. Each
    /// entry is one named step the harness drives via
    /// `POST /test/fixture/step`. Threaded onto the resulting
    /// `Fixture` (post-normalize) so the step handler can read it
    /// without re-parsing the script.
    change_script: Vec<ChangeStep>,
    /// Contact folders accumulated via `contact_folder({...})`.
    contact_folders: Vec<crate::fixture::RawContactFolder>,
    /// Contacts accumulated via `contact({...})`.
    contacts: Vec<crate::fixture::RawContact>,
    /// Google contact groups accumulated via `contact_group({...})`.
    contact_groups: Vec<crate::fixture::RawContactGroup>,
    /// Google `otherContacts` accumulated via `other_contact({...})`.
    other_contacts: Vec<crate::fixture::RawOtherContact>,
    /// Google directory people accumulated via `directory_person({...})`.
    directory_people: Vec<crate::fixture::RawDirectoryPerson>,
    /// Outlook master categories accumulated via `category({...})`.
    categories: Vec<crate::fixture::RawCategory>,
    /// Microsoft Graph groups accumulated via `group({...})`.
    groups: Vec<crate::fixture::RawGroup>,
    /// Gmail SendAs identities accumulated via `send_as({...})`.
    send_as: Vec<crate::fixture::RawSendAs>,
    /// Shared-folder ACL grants accumulated via `acl({...})`.
    acls: Vec<crate::fixture::RawAcl>,
}

impl Builder {
    fn into_parts(self) -> Result<(RawFixture, Vec<ChangeStep>, HandlerMap), String> {
        let Some(name) = self.name else {
            return Err("scenario must call fixture { name = ... }".to_string());
        };
        if self.accounts.is_empty() {
            return Err("scenario must call account { ... } at least once".to_string());
        }
        let raw = RawFixture {
            name,
            state: self.state_token,
            accounts: self.accounts,
            mailboxes: self.mailboxes,
            emails: self.emails,
            oauth: self.oauth,
            caldav: self.caldav,
            announce: self.announce,
            discovery: self.discovery,
            calendars: vec![],
            events: vec![],
            contact_folders: self.contact_folders,
            contacts: self.contacts,
            contact_groups: self.contact_groups,
            other_contacts: self.other_contacts,
            directory_people: self.directory_people,
            categories: self.categories,
            groups: self.groups,
            acls: self.acls,
            // No Lua builders for the EWS public-folder tables yet;
            // TOML-only in v0.
            public_folders: vec![],
            public_items: vec![],
            send_as: self.send_as,
            // Lua change_script lives in `Builder::change_script`
            // (fully-typed `Vec<ChangeStep>`) and is grafted onto
            // the Fixture after `normalize_with_dir` returns; the
            // RawFixture pathway is TOML-only.
            change_script: vec![],
        };
        Ok((raw, self.change_script, self.handlers))
    }
}

// ── Builder registration ────────────────────────────────────────────

fn install_builders(state: &mut State) -> dellingr::Result<()> {
    state.push_rust_fn(builder_fixture)?;
    state.set_global("fixture");
    state.push_rust_fn(builder_account)?;
    state.set_global("account");
    state.push_rust_fn(builder_oauth)?;
    state.set_global("oauth");
    state.push_rust_fn(builder_caldav)?;
    state.set_global("caldav");
    state.push_rust_fn(builder_announce)?;
    state.set_global("announce");
    state.push_rust_fn(builder_mailbox)?;
    state.set_global("mailbox");
    state.push_rust_fn(builder_email)?;
    state.set_global("email");
    state.push_rust_fn(builder_bulk_emails)?;
    state.set_global("bulk_emails");
    state.push_rust_fn(builder_bulk_threads)?;
    state.set_global("bulk_threads");
    state.push_rust_fn(builder_bulk_mailboxes)?;
    state.set_global("bulk_mailboxes");
    state.push_rust_fn(builder_on)?;
    state.set_global("on");
    state.push_rust_fn(builder_change)?;
    state.set_global("change");
    state.push_rust_fn(builder_contact_folder)?;
    state.set_global("contact_folder");
    state.push_rust_fn(builder_contact)?;
    state.set_global("contact");
    state.push_rust_fn(builder_contact_group)?;
    state.set_global("contact_group");
    state.push_rust_fn(builder_other_contact)?;
    state.set_global("other_contact");
    state.push_rust_fn(builder_directory_person)?;
    state.set_global("directory_person");
    state.push_rust_fn(builder_category)?;
    state.set_global("category");
    state.push_rust_fn(builder_group)?;
    state.set_global("group");
    state.push_rust_fn(builder_send_as)?;
    state.set_global("send_as");
    state.push_rust_fn(builder_acl)?;
    state.set_global("acl");
    state.push_rust_fn(builder_discovery)?;
    state.set_global("discovery");
    state.push_rust_fn(builder_wait)?;
    state.set_global("wait");
    state.push_rust_fn(builder_mock_done)?;
    state.set_global("mock_done");
    state.push_rust_fn(builder_mock_fail)?;
    state.set_global("mock_fail");
    Ok(())
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
    let prev = {
        let builder = builder_mut(state)?;
        builder.handlers.insert((protocol, command), anchor)
    };
    if let Some(prev) = prev {
        // Replace: release the old anchor so its slotmap slot is
        // free for reuse. Done via &mut State here because the
        // builder borrow is already dropped.
        state.release_anchor(prev);
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

/// `oauth { enforce = true|false, issuer = "..." }`. Optional; if a
/// scenario omits it the loader behaves as if no `[oauth]` block were
/// present (parity with the TOML loader: `enforce = false`, default
/// issuer). Calling `oauth { ... }` more than once is rejected so a
/// scenario can't shadow itself silently.
fn builder_oauth(state: &mut State) -> dellingr::Result<u8> {
    require_one_table_arg(state, "oauth")?;
    let enforce = read_bool_opt(state, 1, "enforce")?.unwrap_or(false);
    let issuer = read_string_opt(state, 1, "issuer")?;
    let builder = builder_mut(state)?;
    if builder.oauth.is_some() {
        return fail(state, "oauth { ... } may only be called once");
    }
    builder.oauth = Some(RawOAuth { enforce, issuer });
    Ok(0)
}

/// `caldav({ scheduling = true|false })`. Optional; omitting it (or
/// omitting the key) leaves the CalDAV surface advertising RFC 6638
/// scheduling, which is what every fixture did before the block
/// existed. Set `scheduling = false` to stage a plain RFC 4791 store,
/// the server shape a client's derived `event_rsvp = false` describes.
/// Rejected on a second call so a scenario cannot shadow itself.
fn builder_caldav(state: &mut State) -> dellingr::Result<u8> {
    require_one_table_arg(state, "caldav")?;
    let scheduling = read_bool_opt(state, 1, "scheduling")?;
    let builder = builder_mut(state)?;
    if builder.caldav.is_some() {
        return fail(state, "caldav { ... } may only be called once");
    }
    builder.caldav = Some(crate::fixture::RawCalDav { scheduling });
    Ok(0)
}

/// `announce({ step = "...", before = "GET /path", nth = 2 })`.
///
/// Declares that the named change-script step is applied - and pushed
/// on the change feed - immediately BEFORE the `nth` request whose
/// `"<METHOD> <path>"` line starts with `before`. This is the only way
/// to land a change while a backfill walk is IN FLIGHT;
/// `POST /test/fixture/step` can only interleave between whole
/// request/response pairs. Callable repeatedly; each call adds one
/// trigger. See `crate::fixture::AnnounceTrigger`.
fn builder_announce(state: &mut State) -> dellingr::Result<u8> {
    require_one_table_arg(state, "announce")?;
    let step = read_string(state, 1, "step")?;
    let before = read_string(state, 1, "before")?;
    let nth = read_int_opt(state, 1, "nth")?;
    let nth = match nth {
        None => None,
        Some(n) => match u32::try_from(n) {
            Ok(n) => Some(n),
            Err(_) => return fail(state, format!("announce {step:?}: nth must fit in u32")),
        },
    };
    let builder = builder_mut(state)?;
    builder
        .announce
        .push(crate::fixture::RawAnnounce { step, before, nth });
    Ok(0)
}

fn builder_account(state: &mut State) -> dellingr::Result<u8> {
    require_one_table_arg(state, "account")?;
    let id = read_string(state, 1, "id")?;
    let name = read_string(state, 1, "name")?;
    let is_personal = read_bool_opt(state, 1, "is_personal")?.unwrap_or(true);
    let primary = read_bool_opt(state, 1, "primary")?.unwrap_or(false);
    let builder = builder_mut(state)?;
    if builder.accounts.iter().any(|a| a.id == id) {
        return fail(state, "account { ... } called with a duplicate id");
    }
    builder.accounts.push(RawAccount {
        id,
        name,
        is_personal,
        primary,
    });
    Ok(0)
}

/// `acl { mailbox_id, identifier, rights? }` - one static shared-folder
/// grant, the Lua mirror of the TOML `[[acl]]` table. Cross-reference
/// validation (mailbox / account existence, owner-self-grant, duplicate
/// pairs) happens in `normalize`, so this builder only collects.
/// Needed for parity with the change-script `acl_revoke` op: a Lua
/// scenario that revokes mid-run has to be able to declare the grant it
/// revokes.
fn builder_acl(state: &mut State) -> dellingr::Result<u8> {
    require_one_table_arg(state, "acl")?;
    let raw = crate::fixture::RawAcl {
        mailbox_id: read_string(state, 1, "mailbox_id")?,
        identifier: read_string(state, 1, "identifier")?,
        rights: read_string_opt(state, 1, "rights")?,
    };
    builder_mut(state)?.acls.push(raw);
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
        account_id: read_string_opt(state, 1, "account_id")?,
    };
    builder_mut(state)?.mailboxes.push(mb);
    Ok(0)
}

/// `contact_folder { id, display_name, parent_folder_id?, is_default?, account_id? }`
fn builder_contact_folder(state: &mut State) -> dellingr::Result<u8> {
    require_one_table_arg(state, "contact_folder")?;
    let cf = crate::fixture::RawContactFolder {
        id: read_string(state, 1, "id")?,
        display_name: read_string(state, 1, "display_name")?,
        parent_folder_id: read_string_opt(state, 1, "parent_folder_id")?,
        is_default: read_bool_opt(state, 1, "is_default")?.unwrap_or(false),
        account_id: read_string_opt(state, 1, "account_id")?,
    };
    builder_mut(state)?.contact_folders.push(cf);
    Ok(0)
}

/// `contact { id, folder_id, display_name?, emails = { "a@b", { name = "X",
/// address = "x@y" }, ... } }`. Mirrors the address shape used elsewhere:
/// each entry is either a bare string or a `{name, address}` table.
fn builder_contact(state: &mut State) -> dellingr::Result<u8> {
    require_one_table_arg(state, "contact")?;
    let id = read_string(state, 1, "id")?;
    let folder_id = read_string(state, 1, "folder_id")?;
    let display_name = read_string_opt(state, 1, "display_name")?;
    let emails = read_contact_email_array_opt(state, 1, "emails")?;
    let phones = read_contact_phone_array_opt(state, 1, "phones")?;
    let company = read_string_opt(state, 1, "company")?;
    let job_title = read_string_opt(state, 1, "job_title")?;
    let department = read_string_opt(state, 1, "department")?;
    let notes = read_string_opt(state, 1, "notes")?;
    let groups = read_string_array_opt(state, 1, "groups")?;
    let malformed_vcard = read_bool_opt(state, 1, "malformed_vcard")?.unwrap_or(false);
    let account_id = read_string_opt(state, 1, "account_id")?;
    builder_mut(state)?
        .contacts
        .push(crate::fixture::RawContact {
            id,
            folder_id,
            display_name,
            emails,
            phones,
            company,
            job_title,
            department,
            notes,
            groups,
            malformed_vcard,
            account_id,
        });
    Ok(0)
}

/// `contact_group { id, name, account_id? }`. Google People contact
/// group; `id` is the `contactGroups/{id}` resource name.
fn builder_contact_group(state: &mut State) -> dellingr::Result<u8> {
    require_one_table_arg(state, "contact_group")?;
    let g = crate::fixture::RawContactGroup {
        id: read_string(state, 1, "id")?,
        name: read_string(state, 1, "name")?,
        account_id: read_string_opt(state, 1, "account_id")?,
    };
    builder_mut(state)?.contact_groups.push(g);
    Ok(0)
}

/// `other_contact { id, display_name?, emails?, phones?, account_id? }`.
/// Google People `otherContacts` (auto-collected) entry.
fn builder_other_contact(state: &mut State) -> dellingr::Result<u8> {
    require_one_table_arg(state, "other_contact")?;
    let c = crate::fixture::RawOtherContact {
        id: read_string(state, 1, "id")?,
        display_name: read_string_opt(state, 1, "display_name")?,
        emails: read_contact_email_array_opt(state, 1, "emails")?,
        phones: read_contact_phone_array_opt(state, 1, "phones")?,
        account_id: read_string_opt(state, 1, "account_id")?,
    };
    builder_mut(state)?.other_contacts.push(c);
    Ok(0)
}

/// `directory_person { id, display_name?, emails?, phones?, company?,
/// job_title?, department?, account_id? }`. Google directory (GAL)
/// person for `people:listDirectoryPeople` / `searchDirectoryPeople`.
fn builder_directory_person(state: &mut State) -> dellingr::Result<u8> {
    require_one_table_arg(state, "directory_person")?;
    let p = crate::fixture::RawDirectoryPerson {
        id: read_string(state, 1, "id")?,
        display_name: read_string_opt(state, 1, "display_name")?,
        emails: read_contact_email_array_opt(state, 1, "emails")?,
        phones: read_contact_phone_array_opt(state, 1, "phones")?,
        company: read_string_opt(state, 1, "company")?,
        job_title: read_string_opt(state, 1, "job_title")?,
        department: read_string_opt(state, 1, "department")?,
        account_id: read_string_opt(state, 1, "account_id")?,
    };
    builder_mut(state)?.directory_people.push(p);
    Ok(0)
}

/// `category { id, display_name, color?, account_id? }`. The
/// Graph master category list is flat per account, no folder scope.
fn builder_category(state: &mut State) -> dellingr::Result<u8> {
    require_one_table_arg(state, "category")?;
    let cat = crate::fixture::RawCategory {
        id: read_string(state, 1, "id")?,
        display_name: read_string(state, 1, "display_name")?,
        color: read_string_opt(state, 1, "color")?,
        account_id: read_string_opt(state, 1, "account_id")?,
    };
    builder_mut(state)?.categories.push(cat);
    Ok(0)
}

/// `send_as { send_as_email, display_name?, reply_to_address?,
///   signature?, is_primary?, is_default?, treat_as_alias?,
///   account_id? }`. Gmail SendAs identity. The `send_as_email` is
/// the primary key per account. Multi-account fixtures may declare
/// the same address under different accounts.
fn builder_send_as(state: &mut State) -> dellingr::Result<u8> {
    require_one_table_arg(state, "send_as")?;
    let sa = crate::fixture::RawSendAs {
        send_as_email: read_string(state, 1, "send_as_email")?,
        account_id: read_string_opt(state, 1, "account_id")?,
        display_name: read_string_opt(state, 1, "display_name")?,
        reply_to_address: read_string_opt(state, 1, "reply_to_address")?,
        signature: read_string_opt(state, 1, "signature")?,
        is_primary: read_bool_opt(state, 1, "is_primary")?,
        is_default: read_bool_opt(state, 1, "is_default")?,
        treat_as_alias: read_bool_opt(state, 1, "treat_as_alias")?,
    };
    builder_mut(state)?.send_as.push(sa);
    Ok(0)
}

/// `discovery { prefix = "...", webfinger = {...}, oidc = {...},
/// autoconfig = {...} }`. At least one of the three nested tables
/// must be present; absent fields stay None. Calling `discovery`
/// twice with the same prefix is rejected so a scenario can't
/// shadow itself silently. Multiple distinct prefixes accumulate.
#[allow(clippy::cast_possible_wrap)]
fn builder_discovery(state: &mut State) -> dellingr::Result<u8> {
    require_one_table_arg(state, "discovery")?;
    let prefix = read_string(state, 1, "prefix")?;

    let webfinger = read_table_opt(state, 1, "webfinger", |state| {
        let arr_idx = state.get_top() as isize;
        let links = read_webfinger_links(state, arr_idx)?;
        let raw_body = read_string_opt(state, arr_idx, "raw_body")?;
        let raw_content_type = read_string_opt(state, arr_idx, "raw_content_type")?;
        Ok(crate::fixture::RawWebFinger {
            links,
            raw_body,
            raw_content_type,
        })
    })?;

    let oidc = read_table_opt(state, 1, "oidc", |state| {
        let idx = state.get_top() as isize;
        Ok(crate::fixture::RawOidc {
            issuer: read_string(state, idx, "issuer")?,
            authorization_endpoint: read_string(state, idx, "authorization_endpoint")?,
            token_endpoint: read_string(state, idx, "token_endpoint")?,
            userinfo_endpoint: read_string_opt(state, idx, "userinfo_endpoint")?,
            scopes_supported: read_string_array_opt(state, idx, "scopes_supported")?,
            code_challenge_methods_supported: read_string_array_opt(
                state,
                idx,
                "code_challenge_methods_supported",
            )?,
            token_endpoint_auth_methods_supported: read_string_array_opt(
                state,
                idx,
                "token_endpoint_auth_methods_supported",
            )?,
            raw_body: read_string_opt(state, idx, "raw_body")?,
            raw_content_type: read_string_opt(state, idx, "raw_content_type")?,
        })
    })?;

    let autoconfig = read_table_opt(state, 1, "autoconfig", |state| {
        let idx = state.get_top() as isize;
        Ok(crate::fixture::RawAutoconfig {
            raw_body: read_string(state, idx, "raw_body")?,
            raw_content_type: read_string_opt(state, idx, "raw_content_type")?,
        })
    })?;

    if webfinger.is_none() && oidc.is_none() && autoconfig.is_none() {
        return fail(
            state,
            "discovery { ... } must declare at least one of webfinger / oidc / autoconfig",
        );
    }

    let builder = builder_mut(state)?;
    if builder.discovery.contains_key(&prefix) {
        return fail(
            state,
            format!("discovery {{ prefix = {prefix:?} }} declared twice"),
        );
    }
    builder.discovery.insert(
        prefix,
        crate::fixture::RawDiscoveryEntry {
            webfinger,
            oidc,
            autoconfig,
        },
    );
    Ok(0)
}

/// If table `t.key` is a Lua table, push it on the stack, call
/// `f` with the inner table at the top, pop, and return its
/// result wrapped in `Some`. Nil = None.
fn read_table_opt<T>(
    state: &mut State,
    t: isize,
    key: &str,
    f: impl FnOnce(&mut State) -> dellingr::Result<T>,
) -> dellingr::Result<Option<T>> {
    let typ = lookup(state, t, key)?;
    match typ {
        LuaType::Nil => {
            state.pop(1)?;
            Ok(None)
        }
        LuaType::Table => {
            let r = f(state)?;
            state.pop(1)?;
            Ok(Some(r))
        }
        _ => {
            state.pop(1)?;
            fail(state, format!("field {key:?} must be a table"))
        }
    }
}

#[allow(clippy::cast_possible_wrap)]
fn read_webfinger_links(
    state: &mut State,
    t: isize,
) -> dellingr::Result<Vec<crate::fixture::RawWebFingerLink>> {
    let typ = lookup(state, t, "links")?;
    let result = match typ {
        LuaType::Nil => Ok(Vec::new()),
        LuaType::Table => {
            let arr = state.get_top() as isize;
            let len = state.table_len(arr);
            let mut out = Vec::with_capacity(len);
            for i in 1..=len {
                state.push_number(i as f64)?;
                state.get_table_raw(arr)?;
                if state.typ(-1) != LuaType::Table {
                    state.pop(1)?;
                    return fail(
                        state,
                        format!("field \"links\" entry {i} must be a {{rel, href}} table"),
                    );
                }
                let entry_idx = state.get_top() as isize;
                let rel = read_string(state, entry_idx, "rel")?;
                let href = read_string(state, entry_idx, "href")?;
                state.pop(1)?;
                out.push(crate::fixture::RawWebFingerLink { rel, href });
            }
            Ok(out)
        }
        _ => fail(state, "field \"links\" must be an array".to_string()),
    };
    state.pop(1)?;
    result
}

/// `group { id, display_name, description?, mail?, mail_enabled?,
/// security_enabled?, members = { "account-1", "account-2", ... } }`
/// Microsoft Graph group with cross-account membership. `members`
/// is an array of account ids; the loader validates each against
/// the declared `[[account]]` set.
fn builder_group(state: &mut State) -> dellingr::Result<u8> {
    require_one_table_arg(state, "group")?;
    let grp = crate::fixture::RawGroup {
        id: read_string(state, 1, "id")?,
        display_name: read_string(state, 1, "display_name")?,
        description: read_string_opt(state, 1, "description")?,
        mail: read_string_opt(state, 1, "mail")?,
        mail_enabled: read_bool_opt(state, 1, "mail_enabled")?,
        security_enabled: read_bool_opt(state, 1, "security_enabled")?,
        group_types: read_string_array_opt(state, 1, "group_types")?,
        members: read_string_array_opt(state, 1, "members")?,
    };
    builder_mut(state)?.groups.push(grp);
    Ok(0)
}

/// Same as [`read_contact_email_array_opt`] but distinguishes
/// "absent" from "empty". Returns `None` for Nil, `Some(vec)` for
/// any Table (even empty). Used by the change-script
/// `contact_update` reader where the shared op-builder needs to
/// distinguish "don't touch the emails field" from "replace it
/// with the empty list".
#[allow(clippy::cast_possible_wrap)]
fn read_contact_email_array_opt_present(
    state: &mut State,
    t: isize,
    key: &str,
) -> dellingr::Result<Option<Vec<crate::fixture::RawContactEmail>>> {
    let typ = lookup(state, t, key)?;
    let result: dellingr::Result<Option<Vec<crate::fixture::RawContactEmail>>> = match typ {
        LuaType::Nil => {
            state.pop(1)?;
            return Ok(None);
        }
        LuaType::Table => {
            // Read inline (matching the body of read_contact_email_array_opt).
            let arr = state.get_top() as isize;
            let len = state.table_len(arr);
            let mut out = Vec::with_capacity(len);
            for i in 1..=len {
                state.push_number(i as f64)?;
                state.get_table_raw(arr)?;
                let entry_typ = state.typ(-1);
                match entry_typ {
                    LuaType::String => {
                        out.push(crate::fixture::RawContactEmail::Bare(state.to_string(-1)?));
                        state.pop(1)?;
                    }
                    LuaType::Table => {
                        let entry_idx = state.get_top() as isize;
                        let address = read_string(state, entry_idx, "address")?;
                        let name = read_string_opt(state, entry_idx, "name")?;
                        state.pop(1)?;
                        out.push(crate::fixture::RawContactEmail::Full { address, name });
                    }
                    _ => {
                        state.pop(1)?;
                        return fail(
                            state,
                            format!(
                                "field {key:?} entry {i} must be a string or {{address, name}} table"
                            ),
                        );
                    }
                }
            }
            Ok(Some(out))
        }
        _ => fail(state, format!("field {key:?} must be an array")),
    };
    state.pop(1)?;
    result
}

/// Read a phone array (`{ "+1", { number = "+2", kind = "mobile" }, ... }`).
/// Each entry is a bare number string or a `{ number, kind }` table.
#[allow(clippy::cast_possible_wrap)]
fn read_contact_phone_array_opt(
    state: &mut State,
    t: isize,
    key: &str,
) -> dellingr::Result<Vec<crate::fixture::RawContactPhone>> {
    let typ = lookup(state, t, key)?;
    let result: dellingr::Result<Vec<crate::fixture::RawContactPhone>> = match typ {
        LuaType::Nil => Ok(Vec::new()),
        LuaType::Table => {
            let arr = state.get_top() as isize;
            let len = state.table_len(arr);
            let mut out = Vec::with_capacity(len);
            for i in 1..=len {
                state.push_number(i as f64)?;
                state.get_table_raw(arr)?;
                let entry_typ = state.typ(-1);
                match entry_typ {
                    LuaType::String => {
                        out.push(crate::fixture::RawContactPhone::Bare(state.to_string(-1)?));
                        state.pop(1)?;
                    }
                    LuaType::Table => {
                        let entry_idx = state.get_top() as isize;
                        let number = read_string(state, entry_idx, "number")?;
                        let kind = read_string_opt(state, entry_idx, "kind")?;
                        state.pop(1)?;
                        out.push(crate::fixture::RawContactPhone::Full { number, kind });
                    }
                    _ => {
                        state.pop(1)?;
                        return fail(
                            state,
                            format!(
                                "field {key:?} entry {i} must be a string or {{number, kind}} table"
                            ),
                        );
                    }
                }
            }
            Ok(out)
        }
        _ => fail(state, format!("field {key:?} must be an array")),
    };
    state.pop(1)?;
    result
}

/// Present-distinguishing phone reader for `contact_update` (Nil ->
/// None, any Table -> Some, even empty). Mirrors
/// [`read_contact_email_array_opt_present`].
#[allow(clippy::cast_possible_wrap)]
fn read_contact_phone_array_opt_present(
    state: &mut State,
    t: isize,
    key: &str,
) -> dellingr::Result<Option<Vec<crate::fixture::RawContactPhone>>> {
    let typ = lookup(state, t, key)?;
    if typ == LuaType::Nil {
        state.pop(1)?;
        return Ok(None);
    }
    state.pop(1)?;
    read_contact_phone_array_opt(state, t, key).map(Some)
}

#[allow(clippy::cast_possible_wrap)]
fn read_contact_email_array_opt(
    state: &mut State,
    t: isize,
    key: &str,
) -> dellingr::Result<Vec<crate::fixture::RawContactEmail>> {
    let typ = lookup(state, t, key)?;
    let result: dellingr::Result<Vec<crate::fixture::RawContactEmail>> = match typ {
        LuaType::Nil => Ok(Vec::new()),
        LuaType::Table => {
            let arr = state.get_top() as isize;
            let len = state.table_len(arr);
            let mut out = Vec::with_capacity(len);
            for i in 1..=len {
                state.push_number(i as f64)?;
                state.get_table_raw(arr)?;
                let entry_typ = state.typ(-1);
                match entry_typ {
                    LuaType::String => {
                        out.push(crate::fixture::RawContactEmail::Bare(state.to_string(-1)?));
                        state.pop(1)?;
                    }
                    LuaType::Table => {
                        let entry_idx = state.get_top() as isize;
                        let address = read_string(state, entry_idx, "address")?;
                        let name = read_string_opt(state, entry_idx, "name")?;
                        state.pop(1)?;
                        out.push(crate::fixture::RawContactEmail::Full { address, name });
                    }
                    _ => {
                        state.pop(1)?;
                        return fail(
                            state,
                            format!(
                                "field {key:?} entry {i} must be a string or {{address, name}} table"
                            ),
                        );
                    }
                }
            }
            Ok(out)
        }
        _ => fail(state, format!("field {key:?} must be an array")),
    };
    state.pop(1)?;
    result
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
    let id_prefix = read_string_opt(state, 1, "id_prefix")?.unwrap_or_else(|| "bulk".to_string());

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
    let id_prefix = read_string_opt(state, 1, "id_prefix")?.unwrap_or_else(|| "thread".to_string());

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
            let body_tmpl = templates::BODY_TEMPLATES[(seed
                .wrapping_add(thread_i as u64)
                .wrapping_add(msg_i as u64)
                as usize)
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
                email.in_reply_to = vec![prior_message_ids.last().expect("non-empty").clone()];
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
    let id_prefix = read_string_opt(state, 1, "id_prefix")?.unwrap_or_else(|| "mb".to_string());

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
        let base =
            templates::PROJECTS[(seed.wrapping_add(i as u64) as usize) % templates::PROJECTS.len()];
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
            account_id: None,
        });
    }
    Ok(0)
}

fn builder_email(state: &mut State) -> dellingr::Result<u8> {
    require_one_table_arg(state, "email")?;
    // Same field list as the change-script `email_create` reader; both
    // go through `read_raw_email_at` so a new fixture field can never
    // land on one authoring path and not the other.
    let em = read_raw_email_at(state, 1)?;
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
    state.pop(1)?;
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
        state.push_number(i as f64)?;
        state.get_table_raw(arr)?;
        if state.typ(-1) != LuaType::Table {
            state.pop(1)?;
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
        state.pop(1)?;
        out.push(raw);
    }
    Ok(out)
}

/// `change { id = "...", email_create = {...}, email_update = {...},
/// email_move = {...}, email_destroy = {...}, mailbox_create = {...},
/// mailbox_update = {...}, mailbox_destroy = {...}, event_create = {...},
/// event_update = {...}, event_destroy = {...} }`
///
/// One named entry in the incremental-sync script. Steps land in the
/// fixture's `change_script` and are applied atomically, in order, by
/// `POST /test/fixture/step`. Every op is optional; a step that names
/// only a couple of buckets is fine.
///
/// Op shapes:
/// - `email_create`: array of email tables (same fields as the
///   top-level `email` builder, minus `attachments` which the change-
///   script v0 rejects). Validated against the current fixture's
///   mailbox set at apply time.
/// - `email_update`: array of `{ id, keywords?, mailbox_ids? }`
///   tables. Each emits a JMAP-shape patch (`{"keywords": {...},
///   "mailboxIds": {...}}`) routed through `apply_email_patch` at
///   apply time, so the wire semantics match `Email/set` exactly.
/// - `email_reaction`: array of `{ id, reaction_type?,
///   reaction_count?, clear? }`. Sets or clears the Graph
///   reaction extended properties on an existing message. `clear =
///   true` removes both (the "reaction removed" case, distinct from a
///   zero count); otherwise each named field is written and each
///   omitted field left alone.
/// - `email_move`: array of `{ id, mailbox_ids }`. Applies as a
///   regular `mailboxIds` full-replace update; the step handler
///   surfaces the id under `changes.emails.moved` so harness asserts
///   can distinguish a move from a flag flip.
/// - `email_destroy`: array of email-id strings.
/// - `mailbox_create`: array of mailbox tables (same fields as the
///   top-level `mailbox` builder).
/// - `mailbox_update`: array of `{ id, name?, parent_id?, sort_order?,
///   role?, is_subscribed? }`. Patch routes through
///   `apply_mailbox_patch`.
/// - `mailbox_destroy`: array of mailbox-id strings.
/// - `event_create`: array of event tables (id, calendar_id, subject,
///   start, end, plus optional body_preview, body_text, location,
///   organizer, attendees, is_all_day).
/// - `event_update`: array of `{ id, subject?, start?, end?, location?,
///   body_text? }`. Patch is a JSON object the Graph-side `event_set`
///   path applies field-by-field at apply time.
/// - `event_destroy`: array of event-id strings.
/// - `acl_grant`: array of `{ mailbox_id, identifier, rights? }` -
///   shares an owned mailbox with another declared account mid-run.
///   `rights` defaults to `DEFAULT_SHARED_RIGHTS`.
/// - `acl_revoke`: array of `{ mailbox_id, identifier }` - withdraws
///   the grant. Both run last within a step (so a step can create a
///   mailbox and share it), and both advance the owning *and* the
///   granted account's state token.
fn builder_change(state: &mut State) -> dellingr::Result<u8> {
    require_one_table_arg(state, "change")?;
    let id = read_string(state, 1, "id")?;

    let mut ops: Vec<ChangeOp> = Vec::new();

    // email_create: read each entry as a RawEmail and normalise it
    // against the builder's current mailbox set. We deliberately
    // resolve mailbox refs against the *baseline* mailboxes (the
    // ones declared in the script before `change(...)` ran);
    // mailboxes added by an earlier `mailbox_create` op in another
    // step are not visible at script-load time, which means
    // create-mailbox-then-create-email-into-it must live inside the
    // same `change({...})` step (handled below by mailbox_create
    // running first at apply time).
    read_email_create(state, 1, &mut ops)?;

    // email_update / email_move: shaped as { id, ... }; each op
    // emits a JMAP-style patch.
    read_email_update(state, 1, &mut ops)?;
    read_email_reaction(state, 1, &mut ops)?;
    read_email_move(state, 1, &mut ops)?;

    // email_destroy: array of id strings.
    read_id_destroy(state, 1, "email_destroy", &mut ops, |id| {
        ChangeOp::EmailDestroy { id }
    })?;

    // Mailbox ops.
    read_mailbox_create(state, 1, &mut ops)?;
    read_mailbox_update(state, 1, &mut ops)?;
    read_id_destroy(state, 1, "mailbox_destroy", &mut ops, |id| {
        ChangeOp::MailboxDestroy { id }
    })?;

    // Event ops.
    read_event_create(state, 1, &mut ops)?;
    read_event_update(state, 1, &mut ops)?;
    read_id_destroy(state, 1, "event_destroy", &mut ops, |id| {
        ChangeOp::EventDestroy { id }
    })?;

    // Contact-folder ops.
    read_contact_folder_create(state, 1, &mut ops)?;
    read_contact_folder_update(state, 1, &mut ops)?;
    read_id_destroy(state, 1, "contact_folder_destroy", &mut ops, |id| {
        ChangeOp::ContactFolderDestroy { id }
    })?;

    // Contact ops.
    read_contact_create(state, 1, &mut ops)?;
    read_contact_update(state, 1, &mut ops)?;
    read_id_destroy(state, 1, "contact_destroy", &mut ops, |id| {
        ChangeOp::ContactDestroy { id }
    })?;

    // Directory-group ops deliberately do not produce an account delta:
    // the Graph group surface has no delta endpoint. They remain fixture
    // mutations so reconciliation tests can stage a new snapshot.
    read_group_create(state, 1, &mut ops)?;
    read_group_update(state, 1, &mut ops)?;
    read_id_destroy(state, 1, "group_destroy", &mut ops, |id| {
        ChangeOp::GroupDestroy { id }
    })?;

    // ACL ops run last so a step can create a mailbox and share it in
    // the same transition. Matches the TOML projection's op order.
    read_acl_grant(state, 1, &mut ops)?;
    read_acl_revoke(state, 1, &mut ops)?;

    builder_mut(state)?
        .change_script
        .push(ChangeStep { id, ops });
    Ok(0)
}

#[allow(clippy::cast_possible_wrap)]
fn read_group_create(state: &mut State, t: isize, ops: &mut Vec<ChangeOp>) -> dellingr::Result<()> {
    let typ = lookup(state, t, "group_create")?;
    match typ {
        LuaType::Nil => {
            state.pop(1)?;
            return Ok(());
        }
        LuaType::Table => {}
        _ => {
            state.pop(1)?;
            return fail(state, "field \"group_create\" must be an array");
        }
    }
    let arr = state.get_top() as isize;
    for i in 1..=state.table_len(arr) {
        state.push_number(i as f64)?;
        state.get_table_raw(arr)?;
        if state.typ(-1) != LuaType::Table {
            state.pop(2)?;
            return fail(state, format!("group_create entry {i} must be a table"));
        }
        let entry = state.get_top() as isize;
        let raw = crate::fixture::RawGroup {
            id: read_string(state, entry, "id")?,
            display_name: read_string(state, entry, "display_name")?,
            description: read_string_opt(state, entry, "description")?,
            mail: read_string_opt(state, entry, "mail")?,
            mail_enabled: read_bool_opt(state, entry, "mail_enabled")?,
            security_enabled: read_bool_opt(state, entry, "security_enabled")?,
            group_types: read_string_array_opt(state, entry, "group_types")?,
            members: read_string_array_opt(state, entry, "members")?,
        };
        state.pop(1)?;
        ops.push(ChangeOp::GroupCreate(Box::new(crate::fixture::Group {
            id: raw.id,
            display_name: raw.display_name,
            description: raw.description,
            mail: raw.mail,
            mail_enabled: raw.mail_enabled.unwrap_or(false),
            security_enabled: raw.security_enabled.unwrap_or(false),
            group_types: raw.group_types,
            members: raw.members,
        })));
    }
    state.pop(1)?;
    Ok(())
}

#[allow(clippy::cast_possible_wrap)]
fn read_group_update(state: &mut State, t: isize, ops: &mut Vec<ChangeOp>) -> dellingr::Result<()> {
    let typ = lookup(state, t, "group_update")?;
    match typ {
        LuaType::Nil => {
            state.pop(1)?;
            return Ok(());
        }
        LuaType::Table => {}
        _ => {
            state.pop(1)?;
            return fail(state, "field \"group_update\" must be an array");
        }
    }
    let arr = state.get_top() as isize;
    for i in 1..=state.table_len(arr) {
        state.push_number(i as f64)?;
        state.get_table_raw(arr)?;
        if state.typ(-1) != LuaType::Table {
            state.pop(2)?;
            return fail(state, format!("group_update entry {i} must be a table"));
        }
        let entry = state.get_top() as isize;
        let id = read_string(state, entry, "id")?;
        let display_name = read_string_opt(state, entry, "display_name")?;
        // `false` clears mail. Lua table keys cannot distinguish an absent
        // field from `nil`, so false is the explicit null spelling.
        let mail = read_group_mail_patch(state, entry)?;
        let group_types = read_string_array_opt_present(state, entry, "group_types")?;
        let members = read_string_array_opt_present(state, entry, "members")?;
        state.pop(1)?;
        ops.push(ChangeOp::GroupUpdate {
            id,
            display_name,
            mail,
            group_types,
            members,
        });
    }
    state.pop(1)?;
    Ok(())
}

fn read_group_mail_patch(state: &mut State, t: isize) -> dellingr::Result<Option<Option<String>>> {
    let typ = lookup(state, t, "mail")?;
    let result = match typ {
        LuaType::Nil => Ok(None),
        LuaType::String => state.to_string(-1).map(Some).map(Some),
        LuaType::Boolean => {
            let value = state.to_boolean(-1);
            if value {
                fail(state, "field \"mail\" must be a string or false")
            } else {
                Ok(Some(None))
            }
        }
        _ => fail(state, "field \"mail\" must be a string or false"),
    };
    state.pop(1)?;
    result
}

#[allow(clippy::cast_possible_wrap)]
fn read_contact_folder_create(
    state: &mut State,
    t: isize,
    ops: &mut Vec<ChangeOp>,
) -> dellingr::Result<()> {
    let typ = lookup(state, t, "contact_folder_create")?;
    match typ {
        LuaType::Nil => {
            state.pop(1)?;
            return Ok(());
        }
        LuaType::Table => {}
        _ => {
            state.pop(1)?;
            return fail(state, "field \"contact_folder_create\" must be an array");
        }
    }
    let arr = state.get_top() as isize;
    let len = state.table_len(arr);
    for i in 1..=len {
        state.push_number(i as f64)?;
        state.get_table_raw(arr)?;
        if state.typ(-1) != LuaType::Table {
            state.pop(2)?;
            return fail(
                state,
                format!("contact_folder_create entry {i} must be a table"),
            );
        }
        let entry_idx = state.get_top() as isize;
        let raw = crate::fixture::RawContactFolder {
            id: read_string(state, entry_idx, "id")?,
            display_name: read_string(state, entry_idx, "display_name")?,
            parent_folder_id: read_string_opt(state, entry_idx, "parent_folder_id")?,
            is_default: read_bool_opt(state, entry_idx, "is_default")?.unwrap_or(false),
            account_id: read_string_opt(state, entry_idx, "account_id")?,
        };
        state.pop(1)?;
        ops.push(crate::fixture::contact_folder_create_op(raw));
    }
    state.pop(1)?;
    Ok(())
}

#[allow(clippy::cast_possible_wrap)]
fn read_contact_folder_update(
    state: &mut State,
    t: isize,
    ops: &mut Vec<ChangeOp>,
) -> dellingr::Result<()> {
    let typ = lookup(state, t, "contact_folder_update")?;
    match typ {
        LuaType::Nil => {
            state.pop(1)?;
            return Ok(());
        }
        LuaType::Table => {}
        _ => {
            state.pop(1)?;
            return fail(state, "field \"contact_folder_update\" must be an array");
        }
    }
    let arr = state.get_top() as isize;
    let len = state.table_len(arr);
    for i in 1..=len {
        state.push_number(i as f64)?;
        state.get_table_raw(arr)?;
        if state.typ(-1) != LuaType::Table {
            state.pop(2)?;
            return fail(
                state,
                format!("contact_folder_update entry {i} must be a table"),
            );
        }
        let entry_idx = state.get_top() as isize;
        let id = read_string(state, entry_idx, "id")?;
        let display_name = read_string_opt(state, entry_idx, "display_name")?;
        let parent_folder_id = read_string_opt(state, entry_idx, "parent_folder_id")?;
        state.pop(1)?;
        let op = crate::fixture::contact_folder_update_op(id, display_name, parent_folder_id)
            .map_err(|e| {
                state.error(ErrorKind::InternalError(format!(
                    "contact_folder_update entry {i}: {e}"
                )))
            })?;
        ops.push(op);
    }
    state.pop(1)?;
    Ok(())
}

#[allow(clippy::cast_possible_wrap)]
fn read_contact_create(
    state: &mut State,
    t: isize,
    ops: &mut Vec<ChangeOp>,
) -> dellingr::Result<()> {
    let typ = lookup(state, t, "contact_create")?;
    match typ {
        LuaType::Nil => {
            state.pop(1)?;
            return Ok(());
        }
        LuaType::Table => {}
        _ => {
            state.pop(1)?;
            return fail(state, "field \"contact_create\" must be an array");
        }
    }
    let arr = state.get_top() as isize;
    let len = state.table_len(arr);
    for i in 1..=len {
        state.push_number(i as f64)?;
        state.get_table_raw(arr)?;
        if state.typ(-1) != LuaType::Table {
            state.pop(2)?;
            return fail(state, format!("contact_create entry {i} must be a table"));
        }
        let entry_idx = state.get_top() as isize;
        let raw = crate::fixture::RawContact {
            id: read_string(state, entry_idx, "id")?,
            folder_id: read_string(state, entry_idx, "folder_id")?,
            display_name: read_string_opt(state, entry_idx, "display_name")?,
            emails: read_contact_email_array_opt(state, entry_idx, "emails")?,
            phones: read_contact_phone_array_opt(state, entry_idx, "phones")?,
            company: read_string_opt(state, entry_idx, "company")?,
            job_title: read_string_opt(state, entry_idx, "job_title")?,
            department: read_string_opt(state, entry_idx, "department")?,
            notes: read_string_opt(state, entry_idx, "notes")?,
            groups: read_string_array_opt(state, entry_idx, "groups")?,
            malformed_vcard: read_bool_opt(state, entry_idx, "malformed_vcard")?.unwrap_or(false),
            account_id: read_string_opt(state, entry_idx, "account_id")?,
        };
        state.pop(1)?;
        ops.push(crate::fixture::contact_create_op(raw));
    }
    state.pop(1)?;
    Ok(())
}

#[allow(clippy::cast_possible_wrap)]
fn read_contact_update(
    state: &mut State,
    t: isize,
    ops: &mut Vec<ChangeOp>,
) -> dellingr::Result<()> {
    let typ = lookup(state, t, "contact_update")?;
    match typ {
        LuaType::Nil => {
            state.pop(1)?;
            return Ok(());
        }
        LuaType::Table => {}
        _ => {
            state.pop(1)?;
            return fail(state, "field \"contact_update\" must be an array");
        }
    }
    let arr = state.get_top() as isize;
    let len = state.table_len(arr);
    for i in 1..=len {
        state.push_number(i as f64)?;
        state.get_table_raw(arr)?;
        if state.typ(-1) != LuaType::Table {
            state.pop(2)?;
            return fail(state, format!("contact_update entry {i} must be a table"));
        }
        let entry_idx = state.get_top() as isize;
        let id = read_string(state, entry_idx, "id")?;
        let display_name = read_string_opt(state, entry_idx, "display_name")?;
        let folder_id = read_string_opt(state, entry_idx, "folder_id")?;
        let emails = read_contact_email_array_opt_present(state, entry_idx, "emails")?
            .map(|raws| raws.into_iter().map(ContactEmail::from).collect());
        let phones =
            read_contact_phone_array_opt_present(state, entry_idx, "phones")?.map(|raws| {
                raws.into_iter()
                    .map(crate::fixture::ContactPhone::from)
                    .collect()
            });
        let company = read_string_opt(state, entry_idx, "company")?;
        let job_title = read_string_opt(state, entry_idx, "job_title")?;
        let department = read_string_opt(state, entry_idx, "department")?;
        let notes = read_string_opt(state, entry_idx, "notes")?;
        state.pop(1)?;
        let fields = crate::fixture::ContactUpdateFields {
            display_name,
            folder_id,
            emails,
            phones,
            company,
            job_title,
            department,
            notes,
        };
        let op = crate::fixture::contact_update_op(id, fields).map_err(|e| {
            state.error(ErrorKind::InternalError(format!(
                "contact_update entry {i}: {e}"
            )))
        })?;
        ops.push(op);
    }
    state.pop(1)?;
    Ok(())
}

// ── Per-op readers for the change builder ───────────────────────────

#[allow(clippy::cast_possible_wrap)]
fn read_email_create(state: &mut State, t: isize, ops: &mut Vec<ChangeOp>) -> dellingr::Result<()> {
    let typ = lookup(state, t, "email_create")?;
    match typ {
        LuaType::Nil => {
            state.pop(1)?;
            return Ok(());
        }
        LuaType::Table => {}
        _ => {
            state.pop(1)?;
            return fail(state, "field \"email_create\" must be an array");
        }
    }
    let arr = state.get_top() as isize;
    let len = state.table_len(arr);
    let mut entries: Vec<RawEmail> = Vec::with_capacity(len);
    for i in 1..=len {
        state.push_number(i as f64)?;
        state.get_table_raw(arr)?;
        if state.typ(-1) != LuaType::Table {
            state.pop(2)?;
            return fail(state, format!("email_create entry {i} must be a table"));
        }
        let entry_idx = state.get_top() as isize;
        let raw = read_raw_email_at(state, entry_idx)?;
        if !raw.attachments.is_empty() {
            state.pop(2)?;
            return fail(
                state,
                format!(
                    "email_create entry {i}: attachments are not supported in change scripts (v0)"
                ),
            );
        }
        state.pop(1)?;
        entries.push(raw);
    }
    state.pop(1)?;
    // Normalise each entry against the mailbox set declared *so far*
    // in script order. This is an early-error convenience for the
    // common case where every `mailbox(...)` declaration comes before
    // any `change(...)` call; later mailbox declarations or
    // mailbox_create ops in earlier change steps are not reflected
    // here. The authoritative check happens at apply time
    // (`src/routes.rs::apply_change_step::EmailCreate`), which sees
    // the live fixture including everything an earlier op in the
    // current step has added - so a script that creates a mailbox
    // in one op and an email pointing at it in the next op of the
    // same step works fine even though the load-time check would
    // not have known about the mailbox.
    // Build the same mb_ids + mb_accounts + account_ids context the
    // TOML loader passes to `normalize_email`, so the Lua- and
    // TOML-built `Email` values are byte-identical (asserted in
    // `tests/lua_fixture.rs`). Mailboxes declared via
    // `mailbox(...)` carry an optional `account_id`; default to
    // the builder's primary account (the first declared with
    // `primary = true`, or the lone account if exactly one).
    let builder = builder_mut(state)?;
    let primary_id: String = builder
        .accounts
        .iter()
        .find(|a| a.primary)
        .or_else(|| {
            if builder.accounts.len() == 1 {
                builder.accounts.first()
            } else {
                None
            }
        })
        .map(|a| a.id.clone())
        .unwrap_or_default();
    let mb_ids: HashMap<String, ()> = builder
        .mailboxes
        .iter()
        .map(|m| (m.id.clone(), ()))
        .collect();
    let mb_accounts: HashMap<String, String> = builder
        .mailboxes
        .iter()
        .map(|m| {
            (
                m.id.clone(),
                m.account_id.clone().unwrap_or_else(|| primary_id.clone()),
            )
        })
        .collect();
    let account_ids: HashMap<String, ()> = builder
        .accounts
        .iter()
        .map(|a| (a.id.clone(), ()))
        .collect();
    for (i, raw) in entries.into_iter().enumerate() {
        let email = match crate::fixture::normalize_email(
            raw,
            &mb_ids,
            Some(&mb_accounts),
            Some(&account_ids),
            std::path::Path::new("."),
        ) {
            Ok(e) => e,
            Err(e) => return fail(state, format!("email_create entry {}: {e}", i + 1)),
        };
        ops.push(ChangeOp::EmailCreate(Box::new(email)));
    }
    Ok(())
}

#[allow(clippy::cast_possible_wrap)]
fn read_email_update(state: &mut State, t: isize, ops: &mut Vec<ChangeOp>) -> dellingr::Result<()> {
    let typ = lookup(state, t, "email_update")?;
    match typ {
        LuaType::Nil => {
            state.pop(1)?;
            return Ok(());
        }
        LuaType::Table => {}
        _ => {
            state.pop(1)?;
            return fail(state, "field \"email_update\" must be an array");
        }
    }
    let arr = state.get_top() as isize;
    let len = state.table_len(arr);
    for i in 1..=len {
        state.push_number(i as f64)?;
        state.get_table_raw(arr)?;
        if state.typ(-1) != LuaType::Table {
            state.pop(2)?;
            return fail(state, format!("email_update entry {i} must be a table"));
        }
        let entry_idx = state.get_top() as isize;
        let id = read_string(state, entry_idx, "id")?;
        let keywords = read_string_array_opt_present(state, entry_idx, "keywords")?;
        let mailbox_ids = read_string_array_opt_present(state, entry_idx, "mailbox_ids")?;
        state.pop(1)?;
        let op = crate::fixture::email_update_op(id, keywords, mailbox_ids).map_err(|e| {
            state.error(ErrorKind::InternalError(format!(
                "email_update entry {i}: {e}"
            )))
        })?;
        ops.push(op);
    }
    state.pop(1)?;
    Ok(())
}

/// `email_reaction = { { id = ..., reaction_type? = ...,
/// reaction_count? = ..., clear? = true }, ... }`
#[allow(clippy::cast_possible_wrap)]
fn read_email_reaction(
    state: &mut State,
    t: isize,
    ops: &mut Vec<ChangeOp>,
) -> dellingr::Result<()> {
    let typ = lookup(state, t, "email_reaction")?;
    match typ {
        LuaType::Nil => {
            state.pop(1)?;
            return Ok(());
        }
        LuaType::Table => {}
        _ => {
            state.pop(1)?;
            return fail(state, "field \"email_reaction\" must be an array");
        }
    }
    let arr = state.get_top() as isize;
    let len = state.table_len(arr);
    for i in 1..=len {
        state.push_number(i as f64)?;
        state.get_table_raw(arr)?;
        if state.typ(-1) != LuaType::Table {
            state.pop(2)?;
            return fail(state, format!("email_reaction entry {i} must be a table"));
        }
        let entry_idx = state.get_top() as isize;
        let id = read_string(state, entry_idx, "id")?;
        let reaction_type = read_string_opt(state, entry_idx, "reaction_type")?;
        let reaction_count = read_int_opt(state, entry_idx, "reaction_count")?;
        let clear = read_bool_opt(state, entry_idx, "clear")?.unwrap_or(false);
        state.pop(1)?;
        let op = crate::fixture::email_reaction_op(id, reaction_type, reaction_count, clear)
            .map_err(|e| {
                state.error(ErrorKind::InternalError(format!(
                    "email_reaction entry {i}: {e}"
                )))
            })?;
        ops.push(op);
    }
    state.pop(1)?;
    Ok(())
}

#[allow(clippy::cast_possible_wrap)]
fn read_email_move(state: &mut State, t: isize, ops: &mut Vec<ChangeOp>) -> dellingr::Result<()> {
    let typ = lookup(state, t, "email_move")?;
    match typ {
        LuaType::Nil => {
            state.pop(1)?;
            return Ok(());
        }
        LuaType::Table => {}
        _ => {
            state.pop(1)?;
            return fail(state, "field \"email_move\" must be an array");
        }
    }
    let arr = state.get_top() as isize;
    let len = state.table_len(arr);
    for i in 1..=len {
        state.push_number(i as f64)?;
        state.get_table_raw(arr)?;
        if state.typ(-1) != LuaType::Table {
            state.pop(2)?;
            return fail(state, format!("email_move entry {i} must be a table"));
        }
        let entry_idx = state.get_top() as isize;
        let id = read_string(state, entry_idx, "id")?;
        let mailbox_ids = read_string_array(state, entry_idx, "mailbox_ids")?;
        state.pop(1)?;
        let op = crate::fixture::email_move_op(id, mailbox_ids).map_err(|e| {
            state.error(ErrorKind::InternalError(format!(
                "email_move entry {i}: {e}"
            )))
        })?;
        ops.push(op);
    }
    state.pop(1)?;
    Ok(())
}

/// `acl_grant = { { mailbox_id = ..., identifier = ..., rights? }, ... }`
#[allow(clippy::cast_possible_wrap)]
fn read_acl_grant(state: &mut State, t: isize, ops: &mut Vec<ChangeOp>) -> dellingr::Result<()> {
    let typ = lookup(state, t, "acl_grant")?;
    match typ {
        LuaType::Nil => {
            state.pop(1)?;
            return Ok(());
        }
        LuaType::Table => {}
        _ => {
            state.pop(1)?;
            return fail(state, "field \"acl_grant\" must be an array");
        }
    }
    let arr = state.get_top() as isize;
    let len = state.table_len(arr);
    for i in 1..=len {
        state.push_number(i as f64)?;
        state.get_table_raw(arr)?;
        if state.typ(-1) != LuaType::Table {
            state.pop(2)?;
            return fail(state, format!("acl_grant entry {i} must be a table"));
        }
        let entry_idx = state.get_top() as isize;
        let raw = crate::fixture::RawAcl {
            mailbox_id: read_string(state, entry_idx, "mailbox_id")?,
            identifier: read_string(state, entry_idx, "identifier")?,
            rights: read_string_opt(state, entry_idx, "rights")?,
        };
        state.pop(1)?;
        let op = crate::fixture::acl_grant_op(raw).map_err(|e| {
            state.error(ErrorKind::InternalError(format!(
                "acl_grant entry {i}: {e}"
            )))
        })?;
        ops.push(op);
    }
    state.pop(1)?;
    Ok(())
}

/// `acl_revoke = { { mailbox_id = ..., identifier = ... }, ... }`
#[allow(clippy::cast_possible_wrap)]
fn read_acl_revoke(state: &mut State, t: isize, ops: &mut Vec<ChangeOp>) -> dellingr::Result<()> {
    let typ = lookup(state, t, "acl_revoke")?;
    match typ {
        LuaType::Nil => {
            state.pop(1)?;
            return Ok(());
        }
        LuaType::Table => {}
        _ => {
            state.pop(1)?;
            return fail(state, "field \"acl_revoke\" must be an array");
        }
    }
    let arr = state.get_top() as isize;
    let len = state.table_len(arr);
    for i in 1..=len {
        state.push_number(i as f64)?;
        state.get_table_raw(arr)?;
        if state.typ(-1) != LuaType::Table {
            state.pop(2)?;
            return fail(state, format!("acl_revoke entry {i} must be a table"));
        }
        let entry_idx = state.get_top() as isize;
        let raw = crate::fixture::RawAclRevoke {
            mailbox_id: read_string(state, entry_idx, "mailbox_id")?,
            identifier: read_string(state, entry_idx, "identifier")?,
        };
        state.pop(1)?;
        let op = crate::fixture::acl_revoke_op(raw).map_err(|e| {
            state.error(ErrorKind::InternalError(format!(
                "acl_revoke entry {i}: {e}"
            )))
        })?;
        ops.push(op);
    }
    state.pop(1)?;
    Ok(())
}

#[allow(clippy::cast_possible_wrap)]
fn read_id_destroy<F>(
    state: &mut State,
    t: isize,
    key: &str,
    ops: &mut Vec<ChangeOp>,
    mk: F,
) -> dellingr::Result<()>
where
    F: Fn(String) -> ChangeOp,
{
    let typ = lookup(state, t, key)?;
    match typ {
        LuaType::Nil => {
            state.pop(1)?;
            return Ok(());
        }
        LuaType::Table => {}
        _ => {
            state.pop(1)?;
            return fail(state, format!("field {key:?} must be an array"));
        }
    }
    let ids = read_string_array_at_top(state, key)?;
    state.pop(1)?;
    for id in ids {
        ops.push(mk(id));
    }
    Ok(())
}

#[allow(clippy::cast_possible_wrap)]
fn read_mailbox_create(
    state: &mut State,
    t: isize,
    ops: &mut Vec<ChangeOp>,
) -> dellingr::Result<()> {
    let typ = lookup(state, t, "mailbox_create")?;
    match typ {
        LuaType::Nil => {
            state.pop(1)?;
            return Ok(());
        }
        LuaType::Table => {}
        _ => {
            state.pop(1)?;
            return fail(state, "field \"mailbox_create\" must be an array");
        }
    }
    let arr = state.get_top() as isize;
    let len = state.table_len(arr);
    let mut raws: Vec<RawMailbox> = Vec::with_capacity(len);
    for i in 1..=len {
        state.push_number(i as f64)?;
        state.get_table_raw(arr)?;
        if state.typ(-1) != LuaType::Table {
            state.pop(2)?;
            return fail(state, format!("mailbox_create entry {i} must be a table"));
        }
        let entry_idx = state.get_top() as isize;
        let mb = RawMailbox {
            id: read_string(state, entry_idx, "id")?,
            name: read_string(state, entry_idx, "name")?,
            role: read_string_opt(state, entry_idx, "role")?,
            parent_id: read_string_opt(state, entry_idx, "parent_id")?,
            sort_order: read_int_opt(state, entry_idx, "sort_order")?,
            is_subscribed: read_bool_opt(state, entry_idx, "is_subscribed")?,
            account_id: read_string_opt(state, entry_idx, "account_id")?,
        };
        state.pop(1)?;
        raws.push(mb);
    }
    state.pop(1)?;
    for raw in raws {
        let mb_id = raw.id.clone();
        let op = crate::fixture::mailbox_create_op(raw).map_err(|e| {
            state.error(ErrorKind::InternalError(format!(
                "mailbox_create {mb_id:?}: {e}"
            )))
        })?;
        ops.push(op);
    }
    Ok(())
}

#[allow(clippy::cast_possible_wrap)]
fn read_mailbox_update(
    state: &mut State,
    t: isize,
    ops: &mut Vec<ChangeOp>,
) -> dellingr::Result<()> {
    let typ = lookup(state, t, "mailbox_update")?;
    match typ {
        LuaType::Nil => {
            state.pop(1)?;
            return Ok(());
        }
        LuaType::Table => {}
        _ => {
            state.pop(1)?;
            return fail(state, "field \"mailbox_update\" must be an array");
        }
    }
    let arr = state.get_top() as isize;
    let len = state.table_len(arr);
    for i in 1..=len {
        state.push_number(i as f64)?;
        state.get_table_raw(arr)?;
        if state.typ(-1) != LuaType::Table {
            state.pop(2)?;
            return fail(state, format!("mailbox_update entry {i} must be a table"));
        }
        let entry_idx = state.get_top() as isize;
        let id = read_string(state, entry_idx, "id")?;
        let name = read_string_opt(state, entry_idx, "name")?;
        let parent_id = read_string_opt(state, entry_idx, "parent_id")?;
        let sort_order = read_int_opt(state, entry_idx, "sort_order")?;
        let role = read_string_opt(state, entry_idx, "role")?;
        let is_subscribed = read_bool_opt(state, entry_idx, "is_subscribed")?;
        state.pop(1)?;
        let op =
            crate::fixture::mailbox_update_op(id, name, parent_id, sort_order, role, is_subscribed)
                .map_err(|e| {
                    state.error(ErrorKind::InternalError(format!(
                        "mailbox_update entry {i}: {e}"
                    )))
                })?;
        ops.push(op);
    }
    state.pop(1)?;
    Ok(())
}

#[allow(clippy::cast_possible_wrap)]
fn read_event_create(state: &mut State, t: isize, ops: &mut Vec<ChangeOp>) -> dellingr::Result<()> {
    let typ = lookup(state, t, "event_create")?;
    match typ {
        LuaType::Nil => {
            state.pop(1)?;
            return Ok(());
        }
        LuaType::Table => {}
        _ => {
            state.pop(1)?;
            return fail(state, "field \"event_create\" must be an array");
        }
    }
    let arr = state.get_top() as isize;
    let len = state.table_len(arr);
    for i in 1..=len {
        state.push_number(i as f64)?;
        state.get_table_raw(arr)?;
        if state.typ(-1) != LuaType::Table {
            state.pop(2)?;
            return fail(state, format!("event_create entry {i} must be a table"));
        }
        let entry_idx = state.get_top() as isize;
        let raw = crate::fixture::RawEvent {
            id: read_string(state, entry_idx, "id")?,
            calendar_id: read_string(state, entry_idx, "calendar_id")?,
            subject: read_string(state, entry_idx, "subject")?,
            body_preview: read_string_opt(state, entry_idx, "body_preview")?,
            body_text: read_string_opt(state, entry_idx, "body_text")?,
            start: read_string(state, entry_idx, "start")?,
            end: read_string(state, entry_idx, "end")?,
            location: read_string_opt(state, entry_idx, "location")?,
            organizer: read_address_opt(state, entry_idx, "organizer")?,
            attendees: read_event_attendee_array_opt(state, entry_idx, "attendees")?,
            is_all_day: read_bool_opt(state, entry_idx, "is_all_day")?.unwrap_or(false),
            recurrence_rule: read_string_opt(state, entry_idx, "recurrence_rule")?,
            recurrence_exdates: read_string_array_opt(state, entry_idx, "recurrence_exdates")?,
            time_zone: read_string_opt(state, entry_idx, "time_zone")?,
            // Reminders are a static-fixture ([[event]]) authoring
            // affordance in v0; the Lua change-script create path
            // leaves them empty rather than parsing a table-of-tables.
            reminders: Vec::new(),
            raw_ical: read_string_opt(state, entry_idx, "raw_ical")?,
            account_id: read_string_opt(state, entry_idx, "account_id")?,
        };
        state.pop(1)?;
        let op = crate::fixture::event_create_op(raw).map_err(|e| {
            state.error(ErrorKind::InternalError(format!(
                "event_create entry {i} {e}"
            )))
        })?;
        ops.push(op);
    }
    state.pop(1)?;
    Ok(())
}

#[allow(clippy::cast_possible_wrap)]
fn read_event_update(state: &mut State, t: isize, ops: &mut Vec<ChangeOp>) -> dellingr::Result<()> {
    let typ = lookup(state, t, "event_update")?;
    match typ {
        LuaType::Nil => {
            state.pop(1)?;
            return Ok(());
        }
        LuaType::Table => {}
        _ => {
            state.pop(1)?;
            return fail(state, "field \"event_update\" must be an array");
        }
    }
    let arr = state.get_top() as isize;
    let len = state.table_len(arr);
    for i in 1..=len {
        state.push_number(i as f64)?;
        state.get_table_raw(arr)?;
        if state.typ(-1) != LuaType::Table {
            state.pop(2)?;
            return fail(state, format!("event_update entry {i} must be a table"));
        }
        let entry_idx = state.get_top() as isize;
        let id = read_string(state, entry_idx, "id")?;
        let subject = read_string_opt(state, entry_idx, "subject")?;
        let start = read_string_opt(state, entry_idx, "start")?;
        let end = read_string_opt(state, entry_idx, "end")?;
        let location = read_string_opt(state, entry_idx, "location")?;
        let body_text = read_string_opt(state, entry_idx, "body_text")?;
        state.pop(1)?;
        let op = crate::fixture::event_update_op(id, subject, start, end, location, body_text)
            .map_err(|e| {
                state.error(ErrorKind::InternalError(format!(
                    "event_update entry {i}: {e}"
                )))
            })?;
        ops.push(op);
    }
    state.pop(1)?;
    Ok(())
}

/// Read a `RawEmail` from the table at index `t`. Mirrors the field
/// list in `builder_email` but parameterised on the table index so the
/// change-script `email_create` reader can reuse it for sub-tables.
fn read_raw_email_at(state: &mut State, t: isize) -> dellingr::Result<RawEmail> {
    Ok(RawEmail {
        id: read_string(state, t, "id")?,
        thread_id: read_string_opt(state, t, "thread_id")?,
        mailbox_ids: read_string_array(state, t, "mailbox_ids")?,
        keywords: read_string_array_opt(state, t, "keywords")?,
        size: read_int_opt(state, t, "size")?,
        received_at: read_string(state, t, "received_at")?,
        sent_at: read_string_opt(state, t, "sent_at")?,
        from: read_address_opt(state, t, "from")?,
        to: read_address_array_opt(state, t, "to")?,
        cc: read_address_array_opt(state, t, "cc")?,
        bcc: read_address_array_opt(state, t, "bcc")?,
        reply_to: read_address_array_opt(state, t, "reply_to")?,
        subject: read_string_opt(state, t, "subject")?,
        preview: read_string_opt(state, t, "preview")?,
        message_id: read_string_array_opt(state, t, "message_id")?,
        in_reply_to: read_string_array_opt(state, t, "in_reply_to")?,
        references: read_string_array_opt(state, t, "references")?,
        has_attachment: read_bool_opt(state, t, "has_attachment")?,
        body_text: read_string_opt(state, t, "body_text")?,
        body_raw_bytes: read_string_opt(state, t, "body_raw_bytes")?,
        attachments: read_attachment_array_opt(state, t, "attachments")?,
        account_id: read_string_opt(state, t, "account_id")?,
        reaction_type: read_string_opt(state, t, "reaction_type")?,
        reaction_count: read_int_opt(state, t, "reaction_count")?,
    })
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
    state.push_string(key)?;
    state.get_table_raw(t)?;
    Ok(state.typ(-1))
}

fn read_string(state: &mut State, t: isize, key: &str) -> dellingr::Result<String> {
    let typ = lookup(state, t, key)?;
    if typ == LuaType::Nil {
        state.pop(1)?;
        return fail(state, format!("missing required field {key:?}"));
    }
    if typ != LuaType::String {
        state.pop(1)?;
        return fail(state, format!("field {key:?} must be a string"));
    }
    let s = state.to_string(-1)?;
    state.pop(1)?;
    Ok(s)
}

fn read_string_opt(state: &mut State, t: isize, key: &str) -> dellingr::Result<Option<String>> {
    let typ = lookup(state, t, key)?;
    let result = match typ {
        LuaType::Nil => Ok(None),
        LuaType::String => Ok(Some(state.to_string(-1)?)),
        _ => fail(state, format!("field {key:?} must be a string")),
    };
    state.pop(1)?;
    result
}

fn read_bool_opt(state: &mut State, t: isize, key: &str) -> dellingr::Result<Option<bool>> {
    let typ = lookup(state, t, key)?;
    let result = match typ {
        LuaType::Nil => Ok(None),
        LuaType::Boolean => Ok(Some(state.to_boolean(-1))),
        _ => fail(state, format!("field {key:?} must be a boolean")),
    };
    state.pop(1)?;
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
                state.pop(1)?;
                return fail(state, msg);
            }
            // f64 -> i64 cast bounded by the i64 range check above.
            #[allow(clippy::cast_possible_truncation)]
            if !(I64_MIN_F64..=I64_MAX_F64).contains(&n) {
                let msg = format!("field {key:?} integer is out of i64 range");
                state.pop(1)?;
                return fail(state, msg);
            }
            #[allow(clippy::cast_possible_truncation)]
            Ok(Some(n as i64))
        }
        _ => fail(state, format!("field {key:?} must be a number")),
    };
    state.pop(1)?;
    result
}

fn read_string_array(state: &mut State, t: isize, key: &str) -> dellingr::Result<Vec<String>> {
    let typ = lookup(state, t, key)?;
    if typ == LuaType::Nil {
        state.pop(1)?;
        return fail(state, format!("missing required field {key:?}"));
    }
    if typ != LuaType::Table {
        state.pop(1)?;
        return fail(state, format!("field {key:?} must be an array"));
    }
    let result = read_string_array_at_top(state, key);
    state.pop(1)?;
    result
}

fn read_string_array_opt(state: &mut State, t: isize, key: &str) -> dellingr::Result<Vec<String>> {
    let typ = lookup(state, t, key)?;
    let result = match typ {
        LuaType::Nil => Ok(Vec::new()),
        LuaType::Table => read_string_array_at_top(state, key),
        _ => fail(state, format!("field {key:?} must be an array")),
    };
    state.pop(1)?;
    result
}

/// Like [`read_string_array_opt`] but distinguishes "field absent"
/// from "field present and empty". Returns `None` for Nil; returns
/// `Some(vec)` for a Table (even if empty). Used by the change-
/// script update readers, where the shared op-builder helpers in
/// `fixture.rs` need the absent-vs-empty distinction (an `email_update`
/// patch with `keywords = {}` clears all flags; an absent `keywords`
/// field is a no-op for that key).
fn read_string_array_opt_present(
    state: &mut State,
    t: isize,
    key: &str,
) -> dellingr::Result<Option<Vec<String>>> {
    let typ = lookup(state, t, key)?;
    let result = match typ {
        LuaType::Nil => Ok(None),
        LuaType::Table => read_string_array_at_top(state, key).map(Some),
        _ => fail(state, format!("field {key:?} must be an array")),
    };
    state.pop(1)?;
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
        state.push_number(i as f64)?;
        state.get_table_raw(arr)?;
        if state.typ(-1) != LuaType::String {
            state.pop(1)?;
            return fail(state, format!("field {key:?} entry {i} must be a string"));
        }
        out.push(state.to_string(-1)?);
        state.pop(1)?;
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
    state.pop(1)?;
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
    state.pop(1)?;
    result
}

// Same stack-top cast story as `read_string_array_at_top`.
#[allow(clippy::cast_possible_wrap)]
fn read_address_array_at_top(state: &mut State, key: &str) -> dellingr::Result<Vec<RawAddress>> {
    let arr = state.get_top() as isize;
    let len = state.table_len(arr);
    let mut out = Vec::with_capacity(len);
    for i in 1..=len {
        state.push_number(i as f64)?;
        state.get_table_raw(arr)?;
        let typ = state.typ(-1);
        let entry = match typ {
            LuaType::String => RawAddress::Bare(state.to_string(-1)?),
            LuaType::Table => read_address_table_at_top(state, key)?,
            _ => {
                state.pop(1)?;
                return fail(state, format!("field {key:?} entry {i} bad shape"));
            }
        };
        state.pop(1)?;
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
        state.pop(1)?;
        return fail(
            state,
            format!("field {key:?} address table needs a string `email`"),
        );
    }
    let email = state.to_string(-1)?;
    state.pop(1)?;
    Ok(RawAddress::Full { name, email })
}

/// Read an `attendees = { ... }` array into `Vec<RawEventAttendee>`.
/// Each entry is either a bare email string or a `{name = ?, email =
/// ..., status = ?}` table; `status` carries the RSVP participation
/// status (`needs-action` / `accepted` / `declined` / `tentative`).
fn read_event_attendee_array_opt(
    state: &mut State,
    t: isize,
    key: &str,
) -> dellingr::Result<Vec<RawEventAttendee>> {
    let typ = lookup(state, t, key)?;
    let result = match typ {
        LuaType::Nil => Ok(Vec::new()),
        LuaType::Table => read_event_attendee_array_at_top(state, key),
        _ => fail(state, format!("field {key:?} must be an array")),
    };
    state.pop(1)?;
    result
}

// Same stack-top cast story as `read_address_array_at_top`.
#[allow(clippy::cast_possible_wrap)]
fn read_event_attendee_array_at_top(
    state: &mut State,
    key: &str,
) -> dellingr::Result<Vec<RawEventAttendee>> {
    let arr = state.get_top() as isize;
    let len = state.table_len(arr);
    let mut out = Vec::with_capacity(len);
    for i in 1..=len {
        state.push_number(i as f64)?;
        state.get_table_raw(arr)?;
        let typ = state.typ(-1);
        let entry = match typ {
            LuaType::String => RawEventAttendee::Bare(state.to_string(-1)?),
            LuaType::Table => read_event_attendee_table_at_top(state, key)?,
            _ => {
                state.pop(1)?;
                return fail(state, format!("field {key:?} entry {i} bad shape"));
            }
        };
        state.pop(1)?;
        out.push(entry);
    }
    Ok(out)
}

/// Read a `{name = ?, email = ..., status = ?}` table at the stack top
/// into a `RawEventAttendee::Full`. Does not pop the table; caller pops.
//
// Same stack-top cast story as `read_string_array_at_top`.
#[allow(clippy::cast_possible_wrap)]
fn read_event_attendee_table_at_top(
    state: &mut State,
    key: &str,
) -> dellingr::Result<RawEventAttendee> {
    let t = state.get_top() as isize;
    let name = read_string_opt(state, t, "name")?;
    let status = read_string_opt(state, t, "status")?;
    let typ = lookup(state, t, "email")?;
    if typ != LuaType::String {
        state.pop(1)?;
        return fail(
            state,
            format!("field {key:?} attendee table needs a string `email`"),
        );
    }
    let email = state.to_string(-1)?;
    state.pop(1)?;
    Ok(RawEventAttendee::Full {
        name,
        email,
        status,
    })
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
        let mut inner = self.inner.lock().expect("dispatcher mutex poisoned");
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

    /// Reset per-(protocol, command) call counts so the next
    /// dispatch sees `call_index == 1`. Used by
    /// `POST /test/fixture/reset` to give scripted scenarios a
    /// clean window without restarting the binary. Anchors and
    /// the dellingr `State` are preserved - only the counter map
    /// is cleared.
    pub fn reset_counts(&self) {
        self.inner
            .lock()
            .expect("dispatcher mutex poisoned")
            .call_counts
            .clear();
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

fn run_dispatch<F>(state: &mut State, anchor: Anchor, call_index: u64, build_req: F) -> Override
where
    F: FnOnce(&mut State) -> dellingr::Result<()>,
{
    // Build the `req` table, hand it to the script's callback, and
    // run the call. Every step is fallible at the VM level (stack
    // overflow, bad index, a Lua error inside the callback); any of
    // them failing means "no override" - the mock falls back to its
    // default response rather than propagating the fault onto the
    // wire.
    let called = (|| -> dellingr::Result<()> {
        state.new_table()?;
        // Built-in field call_index. dellingr's set_table_raw uses the
        // standard Lua C API order: push KEY first, then VALUE on top,
        // then call - matching `lua_settable`.
        state.push_string("call_index")?;
        state.push_number(call_index as f64)?;
        state.set_table_raw(-3)?;
        build_req(state)?;
        // Stack: [req_table]. call_anchor inserts the function below
        // the 1 arg, then runs the call - so net effect is f(req).
        state.call_anchor(anchor, ArgCount::Fixed(1), RetCount::Fixed(1))
    })();
    if called.is_err() {
        // Best-effort unwind; a failing set_top leaves the next
        // dispatch to clean up, and there is nothing better to do here.
        let _ = state.set_top(0);
        return Override::None;
    }
    // Stack: [result]
    let out = decode_override(state);
    let _ = state.pop(1);
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
            let status =
                field_string(state, table_idx, "status").unwrap_or_else(|| "BAD".to_string());
            let message =
                field_string(state, table_idx, "message").unwrap_or_else(|| "scripted".to_string());
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
    state.push_string(key).ok()?;
    if state.get_table_raw(t).is_err() {
        return None;
    }
    let result = if state.typ(-1) == LuaType::String {
        state.to_string(-1).ok()
    } else {
        None
    };
    state.pop(1).ok()?;
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
    state.push_string(key)?;
    state.push_string(value)?;
    state.set_table_raw(-3)
}

/// Set `req[key] = value` (number) where the table is at the top of
/// the stack.
pub fn req_set_int(state: &mut State, key: &str, value: i64) -> dellingr::Result<()> {
    state.push_string(key)?;
    state.push_number(value as f64)?;
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
    state.push_string(key)?;
    state.new_table()?;
    let arr = state.get_top() as isize;
    for (i, v) in values.into_iter().enumerate() {
        state.push_number((i + 1) as f64)?;
        state.push_string(v.as_ref())?;
        state.set_table_raw(arr)?;
    }
    state.set_table_raw(-3)
}

/// Wrap a [`Dispatcher`] in `Arc` for sharing across protocol
/// listeners.
pub fn into_arc(d: Dispatcher) -> Arc<Dispatcher> {
    Arc::new(d)
}
