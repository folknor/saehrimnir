//! JMAP contacts handlers (`urn:ietf:params:jmap:contacts`).
//!
//! RFC 9610 (JMAP for Contacts) over RFC 9553 (JSContact). The shared
//! `Fixture` already carries `ContactFolder` / `Contact` because the
//! Graph and People listeners project them; this module adds the JMAP
//! projection on top:
//!
//! - `AddressBook/get` projects each `ContactFolder` to an AddressBook.
//! - `ContactCard/get` / `ContactCard/query` project each `Contact` to
//!   a JSContact Card.
//! - `ContactCard/changes` walks the change_log via the per-folder
//!   `Fixture::contact_delta_since`, unioned over the account's
//!   folders (JMAP carries no address-book filter on `/changes`, but we
//!   still scope by account so a multi-account fixture's secondary
//!   cards don't leak into the primary's walk).
//! - `ContactCard/set` create / update / destroy mutate the shared
//!   fixture and record the same `contact_*` transitions Graph
//!   `contacts/delta` and the People listener observe, so a JMAP write
//!   surfaces cross-protocol.
//!
//! bifrost's contacts sync (`crates/jmap/src/sync/contacts.rs`) drives
//! `AddressBook/get` (all) -> `ContactCard/query` (paged) ->
//! `ContactCard/get` (by ids); delta via `ContactCard/changes`;
//! write-back via `ContactCard/set`. It reaches this surface only when
//! the session advertises `urn:ietf:params:jmap:contacts`, which
//! `routes::session` now does whenever the fixture carries contacts.

use serde_json::{Map, Value, json};

use crate::fixture::{Contact, ContactEmail, ContactFolder, DeltaSet, Fixture, MutationDiff};

/// Server-side cap on `ContactCard/query` `limit` (mirrors
/// `Email/query`).
const QUERY_LIMIT_CAP: u64 = 256;
/// Default page size when the client omits `limit`.
const QUERY_LIMIT_DEFAULT: u64 = 50;

// ── AddressBook/get ─────────────────────────────────────────────────

/// RFC 9610 §1.4 (a JMAP `/get`). Projects each fixture
/// `ContactFolder` to an AddressBook. bifrost reads `id`, `name`,
/// `isDefault`, and `myRights` (the write-capability gate).
pub(crate) fn address_book_get(fixture: &Fixture, args: &Value) -> Result<Value, Value> {
    let account_id = require_account(fixture, args)?;

    let (list, not_found) = match args.get("ids") {
        None | Some(Value::Null) => {
            let all = fixture
                .contact_folders_for(account_id)
                .map(serialize_address_book)
                .collect::<Vec<_>>();
            (Value::Array(all), Value::Array(vec![]))
        }
        Some(Value::Array(requested)) => {
            let mut list = Vec::with_capacity(requested.len());
            let mut not_found = Vec::new();
            for v in requested {
                let Some(id) = v.as_str() else {
                    return Err(invalid_args("ids must be an array of strings"));
                };
                match fixture.contact_folders_for(account_id).find(|f| f.id == id) {
                    Some(f) => list.push(serialize_address_book(f)),
                    None => not_found.push(Value::String(id.to_string())),
                }
            }
            (Value::Array(list), Value::Array(not_found))
        }
        Some(_) => return Err(invalid_args("ids must be an array or null")),
    };

    Ok(get_response(account_id, &fixture.state, list, not_found))
}

fn serialize_address_book(f: &ContactFolder) -> Value {
    let mut obj = Map::new();
    obj.insert("id".into(), Value::String(f.id.clone()));
    obj.insert("name".into(), Value::String(f.display_name.clone()));
    obj.insert("description".into(), Value::Null);
    obj.insert("sortOrder".into(), Value::Number(0.into()));
    obj.insert("isDefault".into(), Value::Bool(f.is_default));
    obj.insert("isSubscribed".into(), Value::Bool(true));
    let mut rights = Map::new();
    for k in ["mayRead", "mayWrite", "mayShare", "mayDelete"] {
        rights.insert(k.into(), Value::Bool(true));
    }
    obj.insert("myRights".into(), Value::Object(rights));
    Value::Object(obj)
}

// ── ContactCard/get ─────────────────────────────────────────────────

/// RFC 9610 §2.3 (a JMAP `/get`). `ids = null` lists every contact in
/// the account; an explicit id array returns matched cards with
/// unknown ids in `notFound`. Reads scope by `accountId` via
/// `contacts_for` so a multi-account fixture's secondary cards don't
/// leak.
pub(crate) fn contact_card_get(fixture: &Fixture, args: &Value) -> Result<Value, Value> {
    let account_id = require_account(fixture, args)?;

    let (list, not_found) = match args.get("ids") {
        None | Some(Value::Null) => {
            let all = fixture
                .contacts_for(account_id)
                .map(serialize_contact_card)
                .collect::<Vec<_>>();
            (Value::Array(all), Value::Array(vec![]))
        }
        Some(Value::Array(requested)) => {
            let mut list = Vec::with_capacity(requested.len());
            let mut not_found = Vec::new();
            for v in requested {
                let Some(id) = v.as_str() else {
                    return Err(invalid_args("ids must be an array of strings"));
                };
                match fixture.contacts_for(account_id).find(|c| c.id == id) {
                    Some(c) => list.push(serialize_contact_card(c)),
                    None => not_found.push(Value::String(id.to_string())),
                }
            }
            (Value::Array(list), Value::Array(not_found))
        }
        Some(_) => return Err(invalid_args("ids must be an array or null")),
    };

    Ok(get_response(account_id, &fixture.state, list, not_found))
}

/// Project one fixture `Contact` to a JSContact Card (RFC 9553). The
/// JMAP `id` and JSContact `uid` both use the fixture contact id;
/// `addressBookIds` maps the contact's folder (its JMAP AddressBook)
/// to `true`. Emails key off a stable 1-based index so the wire bytes
/// are deterministic. The fixture `Contact` has no phones /
/// organizations / addresses / notes / media, so those JSContact maps
/// are omitted.
pub(crate) fn serialize_contact_card(c: &Contact) -> Value {
    let mut obj = Map::new();
    obj.insert("@type".into(), Value::String("Card".into()));
    obj.insert("version".into(), Value::String("1.0".into()));
    obj.insert("id".into(), Value::String(c.id.clone()));
    obj.insert("uid".into(), Value::String(c.id.clone()));

    let mut books = Map::new();
    books.insert(c.folder_id.clone(), Value::Bool(true));
    obj.insert("addressBookIds".into(), Value::Object(books));

    obj.insert("kind".into(), Value::String("individual".into()));

    if let Some(name) = &c.display_name {
        obj.insert("name".into(), json!({ "@type": "Name", "full": name }));
    }

    if !c.emails.is_empty() {
        let mut emails = Map::new();
        for (i, e) in c.emails.iter().enumerate() {
            emails.insert(
                format!("e{}", i + 1),
                json!({ "@type": "EmailAddress", "address": e.address }),
            );
        }
        obj.insert("emails".into(), Value::Object(emails));
    }

    Value::Object(obj)
}

// ── ContactCard/query ───────────────────────────────────────────────

/// RFC 9610 §2.4 (a JMAP `/query`). Same envelope as `Email/query`.
/// v0 supports the two filter conditions bifrost sends:
/// `inAddressBook` (folder membership) and `text` (case-insensitive
/// substring across display name + email addresses). Results sort by
/// contact id ascending for byte-stable paging.
// `expect()` calls below are bounds-checked one line above each use
// (position/limit non-negative, start/end <= matches.len()); they
// cannot panic and are not wire errors to surface.
#[allow(clippy::unwrap_in_result)]
pub(crate) fn contact_card_query(fixture: &Fixture, args: &Value) -> Result<Value, Value> {
    let account_id = require_account(fixture, args)?;
    let filter = parse_filter(args.get("filter"))?;

    let position = match args.get("position") {
        None | Some(Value::Null) => 0u64,
        Some(v) => {
            let n = v
                .as_i64()
                .ok_or_else(|| invalid_args("position must be an integer"))?;
            if n < 0 {
                return Err(invalid_args("negative position not supported in v0"));
            }
            u64::try_from(n).expect("position non-negative checked above")
        }
    };
    let limit = match args.get("limit") {
        None | Some(Value::Null) => QUERY_LIMIT_DEFAULT,
        Some(v) => {
            let n = v
                .as_i64()
                .ok_or_else(|| invalid_args("limit must be an integer"))?;
            if n < 0 {
                return Err(invalid_args("limit must be non-negative"));
            }
            u64::try_from(n)
                .expect("limit non-negative checked above")
                .min(QUERY_LIMIT_CAP)
        }
    };
    let calculate_total = args
        .get("calculateTotal")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut matches: Vec<&Contact> = fixture
        .contacts_for(account_id)
        .filter(|c| filter.matches(c))
        .collect();
    matches.sort_by(|a, b| a.id.cmp(&b.id));

    let total = matches.len() as u64;
    let start = usize::try_from(position.min(total)).expect("start <= matches.len()");
    let end = usize::try_from((position + limit).min(total)).expect("end <= matches.len()");
    let ids: Vec<Value> = matches[start..end]
        .iter()
        .map(|c| Value::String(c.id.clone()))
        .collect();

    let mut out = Map::new();
    out.insert("accountId".into(), Value::String(account_id.to_string()));
    out.insert("queryState".into(), Value::String(fixture.state.clone()));
    out.insert("canCalculateChanges".into(), Value::Bool(false));
    out.insert("position".into(), Value::Number(position.into()));
    out.insert("ids".into(), Value::Array(ids));
    if calculate_total {
        out.insert("total".into(), Value::Number(total.into()));
    }
    Ok(Value::Object(out))
}

/// v0 `ContactCard/query` filter. Condition keys are AND-ed; the
/// FilterOperator form (`operator` + `conditions`) is rejected because
/// bifrost never sends one.
#[derive(Default)]
struct Filter {
    in_address_book: Option<String>,
    text: Option<String>,
}

impl Filter {
    fn matches(&self, c: &Contact) -> bool {
        if let Some(book) = &self.in_address_book
            && &c.folder_id != book
        {
            return false;
        }
        if let Some(text) = &self.text {
            let needle = text.to_lowercase();
            let in_name = c
                .display_name
                .as_deref()
                .is_some_and(|n| n.to_lowercase().contains(&needle));
            let in_email = c
                .emails
                .iter()
                .any(|e| e.address.to_lowercase().contains(&needle));
            if !in_name && !in_email {
                return false;
            }
        }
        true
    }
}

fn parse_filter(raw: Option<&Value>) -> Result<Filter, Value> {
    let mut f = Filter::default();
    let Some(v) = raw else { return Ok(f) };
    if v.is_null() {
        return Ok(f);
    }
    let obj = v
        .as_object()
        .ok_or_else(|| invalid_args("filter must be an object"))?;
    for (key, val) in obj {
        match key.as_str() {
            "inAddressBook" => {
                let s = val
                    .as_str()
                    .ok_or_else(|| invalid_args("filter.inAddressBook must be a string"))?;
                f.in_address_book = Some(s.to_string());
            }
            "text" => {
                let s = val
                    .as_str()
                    .ok_or_else(|| invalid_args("filter.text must be a string"))?;
                f.text = Some(s.to_string());
            }
            other => {
                return Err(json!({
                    "type": "unsupportedFilter",
                    "description": format!("v0 does not support filter.{other:?}"),
                }));
            }
        }
    }
    Ok(f)
}

// ── ContactCard/changes ─────────────────────────────────────────────

/// RFC 9610 §2.5 (a JMAP `/changes`). JMAP carries no address-book
/// filter on `/changes`, so walk the union of `contact_delta_since`
/// across the account's folders. Scoping to the account's folders
/// keeps a multi-account fixture's secondary card changes out of the
/// primary's delta (the per-folder helper already scopes tombstones
/// via `contact_destroyed_parents`).
pub(crate) fn contact_card_changes(fixture: &Fixture, args: &Value) -> Result<Value, Value> {
    let account_id = require_account(fixture, args)?;
    let since_state = require_since_state(args)?;

    let delta = account_contact_delta(fixture, since_state, account_id).ok_or_else(|| {
        json!({
            "type": "cannotCalculateChanges",
            "description": format!(
                "sinceState {since_state:?} is not a known fixture state \
                 (older than the seed or evicted from the bounded change log)"
            ),
        })
    })?;

    Ok(json!({
        "accountId": account_id,
        "oldState": since_state,
        "newState": fixture.state,
        "hasMoreChanges": false,
        "created": delta.created,
        "updated": delta.updated,
        "destroyed": delta.destroyed,
    }))
}

/// Account-scoped contact delta: union the per-folder
/// `contact_delta_since` over every address book the account owns. An
/// unknown `sinceState` surfaces as `None` (the per-folder helper
/// returns `None`); an account with no folders defers to the
/// cross-folder walker purely to validate the state token (the result
/// is empty either way).
fn account_contact_delta(fixture: &Fixture, since: &str, account_id: &str) -> Option<DeltaSet> {
    let folders: Vec<String> = fixture
        .contact_folders_for(account_id)
        .map(|f| f.id.clone())
        .collect();
    if folders.is_empty() {
        return fixture.contact_delta_since_any(since);
    }
    let mut out = DeltaSet::default();
    for fid in &folders {
        let d = fixture.contact_delta_since(since, fid)?;
        out.created.extend(d.created);
        out.updated.extend(d.updated);
        out.destroyed.extend(d.destroyed);
    }
    // A contact that moved between two of the account's folders in the
    // window can surface in both folders' deltas; dedup keeps the wire
    // arrays clean while preserving first-seen order.
    dedup_preserve_order(&mut out.created);
    dedup_preserve_order(&mut out.updated);
    dedup_preserve_order(&mut out.destroyed);
    Some(out)
}

fn dedup_preserve_order(ids: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    ids.retain(|id| seen.insert(id.clone()));
}

// ── ContactCard/set ─────────────────────────────────────────────────

/// RFC 9610 §2.6. v0 accepts the create / update / destroy maps
/// bifrost's write-back drives. Mutations apply in-place, bump
/// `state`, and record `contact_*` transitions visible to the next
/// `ContactCard/changes` (and Graph `contacts/delta`). The fixture
/// `Contact` only stores a display name + emails + folder, so JSContact
/// `phones` / `organizations` / `notes` / `media` patches are accepted
/// but not durably stored (mirroring the People-API write-back).
pub(crate) fn contact_card_set(fixture: &mut Fixture, args: &Value) -> Result<Value, Value> {
    let account_id = require_account(fixture, args)?.to_string();
    if let Some(if_in_state) = args.get("ifInState").and_then(Value::as_str)
        && if_in_state != fixture.state
    {
        return Err(json!({
            "type": "stateMismatch",
            "description": format!(
                "ifInState {if_in_state:?} does not match current state {:?}",
                fixture.state,
            ),
        }));
    }

    let creates = args
        .get("create")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let updates = args
        .get("update")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let destroys: Vec<String> = args
        .get("destroy")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let mut created_out = Map::new();
    let mut not_created = Map::new();
    let mut updated_out = Map::new();
    let mut not_updated = Map::new();
    let mut destroyed_out: Vec<String> = Vec::new();
    let mut not_destroyed = Map::new();

    let trans = fixture.mutate(|fix| {
        let mut diff = MutationDiff::default();

        for (client_id, body) in &creates {
            match build_contact_from_create(fix, &account_id, body) {
                Ok((server_id, contact)) => {
                    fix.contacts.push(contact);
                    diff.contact_created.push(server_id.clone());
                    created_out.insert(
                        client_id.clone(),
                        json!({ "id": server_id, "uid": server_id }),
                    );
                }
                Err(err) => {
                    not_created.insert(client_id.clone(), err);
                }
            }
        }

        for (id, patch) in &updates {
            let Some(idx) = fix.contacts.iter().position(|c| &c.id == id) else {
                not_updated.insert(id.clone(), set_error("notFound", "no such contact"));
                continue;
            };
            let mut clone = fix.contacts[idx].clone();
            match apply_contact_patch(fix, &mut clone, patch) {
                Ok(()) => {
                    fix.contacts[idx] = clone;
                    diff.contact_updated.push(id.clone());
                    updated_out.insert(id.clone(), Value::Null);
                }
                Err(err) => {
                    not_updated.insert(id.clone(), err);
                }
            }
        }

        for id in &destroys {
            let Some(idx) = fix.contacts.iter().position(|c| &c.id == id) else {
                not_destroyed.insert(id.clone(), set_error("notFound", "no such contact"));
                continue;
            };
            let parent = fix.contacts[idx].folder_id.clone();
            fix.contacts.remove(idx);
            diff.contact_destroyed.push(id.clone());
            diff.contact_destroyed_parents.push(parent);
            destroyed_out.push(id.clone());
        }

        diff
    });

    Ok(json!({
        "accountId": account_id,
        "oldState": trans.from_state,
        "newState": trans.to_state,
        "created": if created_out.is_empty() { Value::Null } else { Value::Object(created_out) },
        "notCreated": if not_created.is_empty() { Value::Null } else { Value::Object(not_created) },
        "updated": if updated_out.is_empty() { Value::Null } else { Value::Object(updated_out) },
        "notUpdated": if not_updated.is_empty() { Value::Null } else { Value::Object(not_updated) },
        "destroyed": if destroyed_out.is_empty() { Value::Null } else { Value::Array(destroyed_out.into_iter().map(Value::String).collect()) },
        "notDestroyed": if not_destroyed.is_empty() { Value::Null } else { Value::Object(not_destroyed) },
    }))
}

fn build_contact_from_create(
    fix: &mut Fixture,
    request_account_id: &str,
    body: &Value,
) -> Result<(String, Contact), Value> {
    let obj = body
        .as_object()
        .ok_or_else(|| set_error("invalidProperties", "create body must be an object"))?;

    let folder_id = first_address_book(obj.get("addressBookIds"))
        .ok_or_else(|| set_error("invalidProperties", "missing addressBookIds"))?;
    let folder = fix
        .contact_folders
        .iter()
        .find(|f| f.id == folder_id)
        .ok_or_else(|| {
            set_error(
                "invalidProperties",
                &format!("unknown addressBook {folder_id:?}"),
            )
        })?;
    // The card's address book pins its account; the request accountId
    // must agree (no cross-account creates in v0).
    if folder.account_id != request_account_id {
        return Err(set_error(
            "invalidProperties",
            &format!(
                "addressBook {folder_id:?} belongs to account {:?} but request accountId is {request_account_id:?}",
                folder.account_id,
            ),
        ));
    }
    let account_id = folder.account_id.clone();
    let display_name = name_full(obj.get("name"));
    let emails = parse_emails(obj.get("emails"));
    let server_id = fix.mint_contact_id();
    Ok((
        server_id.clone(),
        Contact {
            id: server_id,
            account_id,
            folder_id,
            display_name,
            emails,
        },
    ))
}

fn apply_contact_patch(fix: &Fixture, contact: &mut Contact, patch: &Value) -> Result<(), Value> {
    let obj = patch
        .as_object()
        .ok_or_else(|| set_error("invalidProperties", "patch must be an object"))?;
    for (k, v) in obj {
        match k.as_str() {
            "name" => {
                contact.display_name = if v.is_null() { None } else { name_full(Some(v)) };
            }
            "emails" => {
                contact.emails = parse_emails(Some(v));
            }
            "addressBookIds" => {
                // Accept both the full-replace form (`{ id: true }`)
                // and bifrost's move form (`{ old: null, new: true }`).
                // The fixture contact lives in exactly one folder, so
                // take the first key flagged `true`.
                if let Some(new_folder) = first_address_book(Some(v)) {
                    if !fix.contact_folders.iter().any(|f| f.id == new_folder) {
                        return Err(set_error(
                            "invalidProperties",
                            &format!("unknown addressBook {new_folder:?}"),
                        ));
                    }
                    contact.folder_id = new_folder;
                }
            }
            _ => {
                // JSContact phones / organizations / notes / media (and
                // any extension key) have no fixture storage; accept and
                // ignore, like the People-API write-back. Real JMAP
                // servers tolerate extension keys too.
            }
        }
    }
    Ok(())
}

/// First key flagged `true` in an `addressBookIds` / membership map.
fn first_address_book(v: Option<&Value>) -> Option<String> {
    v?.as_object()?
        .iter()
        .find(|(_, val)| val.as_bool() == Some(true))
        .map(|(k, _)| k.clone())
}

/// JSContact `name.full`, trimmed to `Some` only when non-empty.
fn name_full(v: Option<&Value>) -> Option<String> {
    v?.as_object()?
        .get("full")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// JSContact `emails` map -> fixture `ContactEmail`s, reading each
/// entry's `address`. Order follows the map's iteration order; keys
/// are not preserved (the fixture has no per-email id).
fn parse_emails(v: Option<&Value>) -> Vec<ContactEmail> {
    let Some(obj) = v.and_then(Value::as_object) else {
        return vec![];
    };
    obj.values()
        .filter_map(|e| {
            let address = e.get("address").and_then(Value::as_str)?.to_string();
            Some(ContactEmail {
                address,
                name: None,
            })
        })
        .collect()
}

// ── Shared helpers ──────────────────────────────────────────────────

fn get_response(account_id: &str, state: &str, list: Value, not_found: Value) -> Value {
    let mut out = Map::new();
    out.insert("accountId".into(), Value::String(account_id.to_string()));
    out.insert("state".into(), Value::String(state.to_string()));
    out.insert("list".into(), list);
    out.insert("notFound".into(), not_found);
    Value::Object(out)
}

fn require_account<'a>(fixture: &'a Fixture, args: &'a Value) -> Result<&'a str, Value> {
    let account_id = args
        .get("accountId")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_args("missing accountId"))?;
    if fixture.account(account_id).is_none() {
        return Err(json!({
            "type": "accountNotFound",
            "description": format!("account {account_id:?} not found"),
        }));
    }
    Ok(account_id)
}

fn require_since_state(args: &Value) -> Result<&str, Value> {
    args.get("sinceState")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_args("missing sinceState"))
}

fn invalid_args(description: &str) -> Value {
    json!({"type": "invalidArguments", "description": description})
}

fn set_error(kind: &str, description: &str) -> Value {
    json!({"type": kind, "description": description})
}
