//! People API contacts handlers.
//!
//! Projects fixture `Contact` entries into Google People API
//! `Person` resources. The People API has no folder concept, so
//! the projection collapses every `Contact` across every
//! `ContactFolder` into one flat connections list, ordered by the
//! contact's `id` lexicographically. Folder-scoped sync remains
//! visible through the existing Graph contacts surface.
//!
//! Sync tokens piggy-back on `Fixture::state`. A follow-up call
//! with a token that matches the current state returns an empty
//! delta + the same token. A token the fixture doesn't recognize
//! (older than the seed or simply not one we ever issued) returns
//! HTTP 410 with the standard People error envelope; ratatoskr's
//! recovery path drops the saved token and re-bootstraps via a
//! token-less call.
//!
//! Contact-group, directory, and otherContacts coverage:
//! - `GET /v1/contactGroups` returns the fixture's `[[contact_group]]`
//!   rows (empty-but-200 when none are declared). bifrost-google
//!   fetches this unconditionally during `address_books_list`, so a
//!   404 would break the whole Google contact pull.
//! - `GET /v1/otherContacts` pages the fixture's `[[other_contact]]`
//!   auto-collected corpus (readMask / pageToken).
//! - `people:listDirectoryPeople` / `people:searchDirectoryPeople`
//!   serve the fixture's `[[directory_person]]` GAL rows; an account
//!   with no directory people answers 403 (`PermissionDenied`), which
//!   bifrost swallows to an empty page (personal-account behavior).
//! - `PATCH .../{id}:updateContact` durably stores the patched
//!   phone / organization / notes / name / email fields, so a
//!   write-back round-trips on the next read.

use axum::{
    Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::get,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{AppState, error, ok_json};
use crate::fixture::{Contact, ContactEmail, ContactPhone, Fixture, MutationDiff, OtherContact};

/// Resolve the request's bearer to the account it authorizes,
/// falling back to primary.
fn bearer_account(state: &AppState, headers: &HeaderMap) -> String {
    crate::oauth::account_from_bearer(&state.fixture(), &state.shared.token_store, headers)
}

/// People-API page size cap from
/// `<ratatoskr>/crates/gmail/src/contacts/mod.rs::PAGE_SIZE`.
/// We honour the request's `pageSize` but never exceed this.
const MAX_PAGE_SIZE: usize = 1000;
const DEFAULT_PAGE_SIZE: usize = 100;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/people/me/connections", get(list_connections))
        .route("/v1/otherContacts", get(list_other_contacts))
        // Google People contact groups. bifrost-google's
        // `address_books_list` fetches this UNCONDITIONALLY as step 1
        // of the whole Google contact pull, so a 404 here breaks every
        // downstream contact operation. An empty-but-200 groups list is
        // acceptable when the fixture declares none.
        .route("/v1/contactGroups", get(list_contact_groups))
        // Organization directory (GAL) search. The custom-verb forms
        // arrive as a single `people:{verb}` segment under `/v1/`.
        .route("/v1/people:listDirectoryPeople", get(list_directory_people))
        .route(
            "/v1/people:searchDirectoryPeople",
            get(search_directory_people),
        )
        // Google's `:updateContact` / `:deleteContact` custom verbs
        // arrive as a single segment (`{id}:updateContact`) on the
        // `/v1/people/...` path. ratatoskr addresses contacts via
        // their `people/{id}` resource name, so the wire URL is
        // `/v1/people/{id}:updateContact`. We capture the whole
        // segment as `spec` and pull off the `:verb` suffix in the
        // handler.
        .route(
            "/v1/people/{spec}",
            get(get_person).patch(update_contact).delete(delete_contact),
        )
}

/// `GET /v1/people/{resourceName}` - a single Person. bifrost's
/// `get_person` drives this for `contact_get` AND for the etag
/// prefetch before `updateContact`, so without it both the contact
/// read and the contact write-back 404. The resource name arrives
/// as the bare id segment (`people/{id}` -> `{id}` after the route
/// prefix); the `{id}:verb` custom-verb forms keep their PATCH /
/// DELETE handlers, so a `spec` carrying a `:` is not a plain GET.
async fn get_person(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(spec): Path<String>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
) -> Response {
    if spec.contains(':') {
        return error(
            StatusCode::NOT_FOUND,
            &format!("v0 mock does not implement GET /v1/people/{spec}"),
            "notFound",
        );
    }
    let account_id = bearer_account(&state, &headers);
    state.shared.request_log.record_with_conn(
        "people",
        format!("GET /v1/people/{spec}"),
        json!({}),
        connection_id,
    );
    if let Some(o) = super::maybe_override(&state, "get_contact", {
        let spec = spec.clone();
        move |s| crate::lua::req_set_str(s, "contact_id", &spec)
    }) {
        return o;
    }
    let fixture = state.fixture();
    match fixture.contacts_for(&account_id).find(|c| c.id == spec) {
        Some(c) => ok_json(serialize_person(c)),
        None => error(
            StatusCode::NOT_FOUND,
            &format!("person people/{spec} not found"),
            "notFound",
        ),
    }
}

#[derive(Debug, Deserialize, Default)]
struct ListParams {
    #[serde(default, rename = "pageSize")]
    page_size: Option<usize>,
    #[serde(default, rename = "pageToken")]
    page_token: Option<String>,
    #[serde(default, rename = "syncToken")]
    sync_token: Option<String>,
    /// Echoed silently. The mock always emits the full Person
    /// shape regardless of what fields were requested; ratatoskr's
    /// parser tolerates extra fields. Captured so the request log
    /// records what the client asked for.
    #[serde(default, rename = "personFields")]
    _person_fields: Option<String>,
    #[serde(default, rename = "readMask")]
    _read_mask: Option<String>,
    #[serde(default, rename = "requestSyncToken")]
    _request_sync_token: Option<String>,
}

async fn list_connections(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ListParams>,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "list_connections", |s| {
        if let Some(t) = &params.sync_token {
            crate::lua::req_set_str(s, "sync_token", t)?;
        }
        if let Some(t) = &params.page_token {
            crate::lua::req_set_str(s, "page_token", t)?;
        }
        Ok(())
    }) {
        return o;
    }

    let account_id = bearer_account(&state, &headers);
    let fixture = state.fixture();

    // Sync-token paths:
    // - current state: empty delta + same token echoed back.
    // - known historical state: walk the change_log, emit
    //   created/updated contacts as full Persons and destroyed
    //   contacts as `metadata.deleted: true` tombstones, then
    //   emit the current state as `nextSyncToken`. ratatoskr's
    //   recovery branch only fires on 410, so a known token
    //   must produce an honest delta (or recovery breaks
    //   silently: stale token returns full live list, missing
    //   tombstones, contacts persist forever).
    // - unknown / evicted token: HTTP 410 with the
    //   `expired`-reason envelope. Real People API uses
    //   `EXPIRED_TOKEN`; ratatoskr matches on the `syncToken`
    //   substring + the 410 status, so this lands.
    if params.sync_token.as_deref() == Some(fixture.state_for(&account_id)) {
        return ok_json(json!({
            "connections": [],
            "totalPeople": 0,
            "totalItems": 0,
            "nextSyncToken": fixture.state_for(&account_id),
        }));
    }
    if let Some(token) = params.sync_token.as_deref() {
        let Some(delta) = fixture.contact_delta_since_any(token, &account_id) else {
            return error(
                StatusCode::GONE,
                "syncToken expired or not recognised",
                "expired",
            );
        };
        let live_by_id: std::collections::HashMap<&str, &Contact> = fixture
            .contacts_for(&account_id)
            .map(|c| (c.id.as_str(), c))
            .collect();
        let mut connections: Vec<Value> = Vec::new();
        for id in delta.created.iter().chain(delta.updated.iter()) {
            if let Some(c) = live_by_id.get(id.as_str()) {
                connections.push(serialize_person(c));
            }
        }
        for id in &delta.destroyed {
            connections.push(tombstone_person(id));
        }
        return ok_json(json!({
            "connections": connections,
            "totalPeople": connections.len(),
            "totalItems": connections.len(),
            "nextSyncToken": fixture.state_for(&account_id),
        }));
    }

    // Sort once (refs only), then serialize the requested page
    // slice. Pre-fix this materialised every contact's full JSON
    // before paging, which made each page request O(N) regardless
    // of page_size.
    let mut sorted: Vec<&Contact> = fixture.contacts_for(&account_id).collect();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));
    let total = sorted.len();
    let page_size = clamp_page_size(params.page_size);
    let offset = parse_page_token(params.page_token.as_deref()).unwrap_or(0);
    if offset > total {
        return error(
            StatusCode::BAD_REQUEST,
            "pageToken offset exceeds total",
            "badRequest",
        );
    }
    let end = (offset + page_size).min(total);
    let slice: Vec<Value> = sorted[offset..end]
        .iter()
        .map(|c| serialize_person(c))
        .collect();
    let mut body = Map::new();
    body.insert("connections".into(), Value::Array(slice));
    body.insert("totalPeople".into(), Value::Number((total as u64).into()));
    body.insert("totalItems".into(), Value::Number((total as u64).into()));
    if end < total {
        body.insert(
            "nextPageToken".into(),
            Value::String(encode_page_token(end)),
        );
    } else {
        // Final page emits the syncToken; prior pages don't
        // (matches real People API).
        body.insert(
            "nextSyncToken".into(),
            Value::String(fixture.state_for(&account_id).to_string()),
        );
    }
    ok_json(Value::Object(body))
}

async fn list_other_contacts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ListParams>,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "list_other_contacts", |s| {
        if let Some(t) = &params.sync_token {
            crate::lua::req_set_str(s, "sync_token", t)?;
        }
        Ok(())
    }) {
        return o;
    }
    let account_id = bearer_account(&state, &headers);
    let fixture = state.fixture();
    if let Some(token) = params.sync_token.as_deref()
        && !is_known_state(&fixture, &account_id, token)
    {
        return error(
            StatusCode::GONE,
            "syncToken expired or not recognised",
            "expired",
        );
    }

    // Page the fixture-declared otherContacts corpus (auto-collected
    // addresses). Same pageSize / pageToken contract as connections;
    // the final page carries `nextSyncToken`, earlier pages a
    // `nextPageToken`.
    let mut sorted: Vec<&OtherContact> = fixture.other_contacts_for(&account_id).collect();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));
    let total = sorted.len();
    let page_size = clamp_page_size(params.page_size);
    let offset = parse_page_token(params.page_token.as_deref()).unwrap_or(0);
    if offset > total {
        return error(
            StatusCode::BAD_REQUEST,
            "pageToken offset exceeds total",
            "badRequest",
        );
    }
    let end = (offset + page_size).min(total);
    let slice: Vec<Value> = sorted[offset..end]
        .iter()
        .map(|c| serialize_other_contact(c))
        .collect();
    let mut body = Map::new();
    body.insert("otherContacts".into(), Value::Array(slice));
    body.insert("totalSize".into(), Value::Number((total as u64).into()));
    if end < total {
        body.insert(
            "nextPageToken".into(),
            Value::String(encode_page_token(end)),
        );
    } else {
        body.insert(
            "nextSyncToken".into(),
            Value::String(fixture.state_for(&account_id).to_string()),
        );
    }
    ok_json(Value::Object(body))
}

/// `GET /v1/contactGroups` - the Google People contact groups list.
/// bifrost fetches this unconditionally before any contact read, so it
/// must answer 200 (an empty list is fine). Groups are minimal
/// `{ resourceName, name }` projections; the fixture drives them.
async fn list_contact_groups(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ListParams>,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "list_contact_groups", |_| Ok(())) {
        return o;
    }
    let account_id = bearer_account(&state, &headers);
    let fixture = state.fixture();
    let mut sorted: Vec<_> = fixture.contact_groups_for(&account_id).collect();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));
    let total = sorted.len();
    let page_size = clamp_page_size(params.page_size);
    let offset = parse_page_token(params.page_token.as_deref()).unwrap_or(0);
    if offset > total {
        return error(
            StatusCode::BAD_REQUEST,
            "pageToken offset exceeds total",
            "badRequest",
        );
    }
    let end = (offset + page_size).min(total);
    let groups: Vec<Value> = sorted[offset..end]
        .iter()
        .map(|g| {
            json!({
                "resourceName": g.id,
                "name": g.name,
                "formattedName": g.name,
                "groupType": "USER_CONTACT_GROUP",
            })
        })
        .collect();
    let mut body = Map::new();
    body.insert("contactGroups".into(), Value::Array(groups));
    body.insert("totalItems".into(), Value::Number((total as u64).into()));
    if end < total {
        body.insert(
            "nextPageToken".into(),
            Value::String(encode_page_token(end)),
        );
    }
    ok_json(Value::Object(body))
}

/// `GET /v1/people:listDirectoryPeople` - organization directory (GAL)
/// enumeration (bifrost's empty-query directory path). An account that
/// declares zero directory people models a personal Gmail account with
/// no directory: it answers 403 (`PermissionDenied`), which bifrost
/// swallows to an empty page. A corporate account returns its directory
/// people, `pageSize` / `pageToken` paged.
async fn list_directory_people(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ListParams>,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "list_directory_people", |_| Ok(())) {
        return o;
    }
    let account_id = bearer_account(&state, &headers);
    let fixture = state.fixture();
    directory_page(&fixture, &account_id, &params, None)
}

#[derive(Debug, Deserialize, Default)]
struct DirectoryParams {
    #[serde(default, rename = "pageSize")]
    page_size: Option<usize>,
    #[serde(default, rename = "pageToken")]
    page_token: Option<String>,
    #[serde(default)]
    query: Option<String>,
}

/// `GET /v1/people:searchDirectoryPeople` - directory search. Filters
/// the account's directory people by a case-insensitive substring over
/// display name / email / organization. An empty `query` (bifrost's
/// cache-priming warmup) matches everything. No-directory accounts 403
/// exactly as `listDirectoryPeople` does, so the warmup's own 403 short-
/// circuits bifrost to an empty page.
async fn search_directory_people(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<DirectoryParams>,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "search_directory_people", |s| {
        if let Some(q) = &params.query {
            crate::lua::req_set_str(s, "query", q)?;
        }
        Ok(())
    }) {
        return o;
    }
    let account_id = bearer_account(&state, &headers);
    let fixture = state.fixture();
    let list_params = ListParams {
        page_size: params.page_size,
        page_token: params.page_token.clone(),
        ..Default::default()
    };
    let needle = params
        .query
        .as_deref()
        .map(str::to_lowercase)
        .filter(|q| !q.is_empty());
    directory_page(&fixture, &account_id, &list_params, needle.as_deref())
}

/// Shared directory pagination + 403-absence rule for
/// `listDirectoryPeople` / `searchDirectoryPeople`. `needle`, when set,
/// filters by a case-insensitive substring across the projected fields.
fn directory_page(
    fixture: &Fixture,
    account_id: &str,
    params: &ListParams,
    needle: Option<&str>,
) -> Response {
    // No directory declared for this account -> the account has no
    // directory at all (personal Gmail). 403 PermissionDenied; bifrost
    // swallows it to an empty page.
    if fixture.directory_people_for(account_id).next().is_none() {
        return error(
            StatusCode::FORBIDDEN,
            "account has no organization directory",
            "forbidden",
        );
    }
    let mut sorted: Vec<_> = fixture
        .directory_people_for(account_id)
        .filter(|p| match needle {
            None => true,
            Some(n) => directory_matches(p, n),
        })
        .collect();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));
    let total = sorted.len();
    let page_size = clamp_page_size(params.page_size);
    let offset = parse_page_token(params.page_token.as_deref()).unwrap_or(0);
    if offset > total {
        return error(
            StatusCode::BAD_REQUEST,
            "pageToken offset exceeds total",
            "badRequest",
        );
    }
    let end = (offset + page_size).min(total);
    let people: Vec<Value> = sorted[offset..end]
        .iter()
        .map(|p| serialize_directory_person(p))
        .collect();
    let mut body = Map::new();
    body.insert("people".into(), Value::Array(people));
    if end < total {
        body.insert(
            "nextPageToken".into(),
            Value::String(encode_page_token(end)),
        );
    }
    ok_json(Value::Object(body))
}

fn directory_matches(p: &crate::fixture::DirectoryPerson, needle: &str) -> bool {
    p.display_name
        .as_deref()
        .is_some_and(|n| n.to_lowercase().contains(needle))
        || p.emails
            .iter()
            .any(|e| e.address.to_lowercase().contains(needle))
        || p.company
            .as_deref()
            .is_some_and(|c| c.to_lowercase().contains(needle))
}

/// Project an `OtherContact` into a People `Person`. otherContacts
/// carry only names / emails / phones / metadata (the read-mask subset
/// bifrost requests), and their resource name lives under
/// `otherContacts/*`.
fn serialize_other_contact(c: &OtherContact) -> Value {
    let mut obj = Map::new();
    obj.insert("resourceName".into(), Value::String(c.id.clone()));
    obj.insert("etag".into(), Value::String(format!("etag-{}", c.id)));
    let mut metadata = Map::new();
    metadata.insert("deleted".into(), Value::Bool(false));
    metadata.insert(
        "sources".into(),
        Value::Array(vec![json!({ "type": "OTHER_CONTACT", "id": c.id })]),
    );
    obj.insert("metadata".into(), Value::Object(metadata));
    if let Some(name) = &c.display_name {
        obj.insert(
            "names".into(),
            Value::Array(vec![json!({ "displayName": name })]),
        );
    }
    let emails: Vec<Value> = c
        .emails
        .iter()
        .map(|e| json!({ "value": e.address, "type": "other" }))
        .collect();
    obj.insert("emailAddresses".into(), Value::Array(emails));
    if !c.phones.is_empty() {
        let phones: Vec<Value> = c
            .phones
            .iter()
            .map(|p| match &p.kind {
                Some(k) => json!({ "value": p.number, "type": k }),
                None => json!({ "value": p.number }),
            })
            .collect();
        obj.insert("phoneNumbers".into(), Value::Array(phones));
    }
    Value::Object(obj)
}

/// Project a `DirectoryPerson` into a People `Person` for the directory
/// endpoints (names / emails / phones / organizations read-mask).
fn serialize_directory_person(p: &crate::fixture::DirectoryPerson) -> Value {
    let mut obj = Map::new();
    obj.insert("resourceName".into(), Value::String(p.id.clone()));
    obj.insert("etag".into(), Value::String(format!("etag-{}", p.id)));
    let mut metadata = Map::new();
    metadata.insert("deleted".into(), Value::Bool(false));
    metadata.insert(
        "sources".into(),
        Value::Array(vec![json!({ "type": "DIRECTORY", "id": p.id })]),
    );
    obj.insert("metadata".into(), Value::Object(metadata));
    if let Some(name) = &p.display_name {
        obj.insert(
            "names".into(),
            Value::Array(vec![json!({ "displayName": name })]),
        );
    }
    let emails: Vec<Value> = p
        .emails
        .iter()
        .map(|e| json!({ "value": e.address, "type": "work" }))
        .collect();
    obj.insert("emailAddresses".into(), Value::Array(emails));
    if !p.phones.is_empty() {
        let phones: Vec<Value> = p
            .phones
            .iter()
            .map(|ph| match &ph.kind {
                Some(k) => json!({ "value": ph.number, "type": k }),
                None => json!({ "value": ph.number }),
            })
            .collect();
        obj.insert("phoneNumbers".into(), Value::Array(phones));
    }
    if p.company.is_some() || p.job_title.is_some() || p.department.is_some() {
        let mut org = Map::new();
        if let Some(company) = &p.company {
            org.insert("name".into(), Value::String(company.clone()));
        }
        if let Some(title) = &p.job_title {
            org.insert("title".into(), Value::String(title.clone()));
        }
        if let Some(dept) = &p.department {
            org.insert("department".into(), Value::String(dept.clone()));
        }
        obj.insert(
            "organizations".into(),
            Value::Array(vec![Value::Object(org)]),
        );
    }
    Value::Object(obj)
}

// ── Mutations ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
struct UpdateParams {
    /// Comma-separated list of writable fields the client is touching
    /// (`phoneNumbers`, `organizations`, `biographies`, ...). Echoed
    /// into the request log; the mock doesn't durably store these
    /// fields (the fixture `Contact` only carries display name + email
    /// list) but treats any non-empty mask as a contact_updated
    /// transition so the next delta surfaces the contact.
    #[serde(default, rename = "updatePersonFields")]
    update_person_fields: Option<String>,
}

async fn update_contact(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(spec): Path<String>,
    Query(params): Query<UpdateParams>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
    body: axum::body::Body,
) -> Response {
    let account_id = bearer_account(&state, &headers);
    let Some(id) = spec.strip_suffix(":updateContact") else {
        return error(
            StatusCode::NOT_FOUND,
            &format!("v0 mock does not implement PATCH /v1/people/{spec}"),
            "notFound",
        );
    };
    let id = id.to_string();
    let parsed = match parse_json_body(body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    state.shared.request_log.record_with_conn(
        "people",
        format!("PATCH /v1/people/{id}:updateContact"),
        json!({
            "updatePersonFields": params.update_person_fields,
            "body": crate::request_log::body_detail(&parsed),
        }),
        connection_id,
    );

    if let Some(o) = super::maybe_override(&state, "update_contact", |s| {
        crate::lua::req_set_str(s, "contact_id", &id)?;
        if let Some(m) = &params.update_person_fields {
            crate::lua::req_set_str(s, "update_person_fields", m)?;
        }
        Ok(())
    }) {
        return o;
    }

    let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
    if !fix
        .contacts
        .iter()
        .any(|c| c.account_id == account_id && c.id == id)
    {
        return error(
            StatusCode::NOT_FOUND,
            &format!("contact {id:?} not declared in fixture"),
            "notFound",
        );
    }
    let id_for_diff = id.clone();
    let acct = account_id.clone();
    let body = parsed.clone();
    let _ = fix.mutate(|f| {
        if let Some(idx) = f
            .contacts
            .iter()
            .position(|c| c.account_id == acct && c.id == id_for_diff)
        {
            apply_person_patch(&mut f.contacts[idx], &body);
        }
        MutationDiff {
            contact_updated: vec![id_for_diff.clone()],
            ..Default::default()
        }
    });
    let view = fix
        .contacts
        .iter()
        .find(|c| c.account_id == account_id && c.id == id)
        .cloned();
    drop(fix);
    match view {
        Some(c) => ok_json(serialize_person(&c)),
        // Should never happen: we just checked above and hold the
        // write guard. Defensive only.
        None => error(StatusCode::NOT_FOUND, "contact vanished", "notFound"),
    }
}

async fn delete_contact(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(spec): Path<String>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
) -> Response {
    let account_id = bearer_account(&state, &headers);
    let Some(id) = spec.strip_suffix(":deleteContact") else {
        return error(
            StatusCode::NOT_FOUND,
            &format!("v0 mock does not implement DELETE /v1/people/{spec}"),
            "notFound",
        );
    };
    let id = id.to_string();
    state.shared.request_log.record_with_conn(
        "people",
        format!("DELETE /v1/people/{id}:deleteContact"),
        json!({ "id": id }),
        connection_id,
    );

    if let Some(o) = super::maybe_override(&state, "delete_contact", |s| {
        crate::lua::req_set_str(s, "contact_id", &id)
    }) {
        return o;
    }

    let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
    let Some(parent) = fix
        .contacts
        .iter()
        .find(|c| c.account_id == account_id && c.id == id)
        .map(|c| c.folder_id.clone())
    else {
        return error(
            StatusCode::NOT_FOUND,
            &format!("contact {id:?} not declared in fixture"),
            "notFound",
        );
    };
    let id_for_diff = id.clone();
    let acct = account_id.clone();
    let _ = fix.mutate(|f| {
        let len_before = f.contacts.len();
        f.contacts
            .retain(|c| !(c.account_id == acct && c.id == id_for_diff));
        if f.contacts.len() < len_before {
            MutationDiff {
                contact_destroyed: vec![id_for_diff.clone()],
                contact_destroyed_parents: vec![parent.clone()],
                ..Default::default()
            }
        } else {
            MutationDiff::default()
        }
    });
    // Real People API returns an empty `{}` body on delete success.
    ok_json(json!({}))
}

// Err is an axum `Response` (large); allow rather than box (every `?`
// caller would otherwise have to unbox).
#[allow(clippy::result_large_err)]
async fn parse_json_body(body: axum::body::Body) -> Result<Value, Response> {
    let bytes = match axum::body::to_bytes(body, 1_048_576).await {
        Ok(b) => b,
        Err(e) => {
            return Err(error(
                StatusCode::BAD_REQUEST,
                &format!("failed to read body: {e}"),
                "badRequest",
            ));
        }
    };
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&bytes).map_err(|e| {
        error(
            StatusCode::BAD_REQUEST,
            &format!("body is not JSON: {e}"),
            "invalidRequest",
        )
    })
}

// ── Shape helpers ───────────────────────────────────────────────────

fn is_known_state(fixture: &Fixture, account_id: &str, state: &str) -> bool {
    if state == fixture.state_for(account_id) {
        return true;
    }
    if state == fixture.change_log_seed() {
        return true;
    }
    fixture
        .change_log_transitions_for(account_id)
        .any(|t| t.from_state == state || t.to_state == state)
}

fn clamp_page_size(requested: Option<usize>) -> usize {
    let n = requested.unwrap_or(DEFAULT_PAGE_SIZE);
    n.clamp(1, MAX_PAGE_SIZE)
}

/// Encoded as `p.<offset>` so a malformed token (e.g. someone
/// hand-rolling one) is obvious in the request log.
fn encode_page_token(offset: usize) -> String {
    format!("p.{offset}")
}

fn parse_page_token(t: Option<&str>) -> Option<usize> {
    t?.strip_prefix("p.")?.parse().ok()
}

/// People API tombstone shape: a Person whose `metadata.deleted`
/// is true. ratatoskr's `PersonMetadata.deleted` (`crates/gmail/
/// src/contacts/mod.rs:58-61`) routes these to delete-the-row.
/// Other fields are minimised because the Person no longer
/// exists - the resource name is the only addressable identity.
fn tombstone_person(id: &str) -> Value {
    json!({
        "resourceName": format!("people/{id}"),
        "etag": format!("etag-{id}"),
        "metadata": {
            "deleted": true,
            "sources": [{ "type": "CONTACT", "id": id }],
        }
    })
}

/// Apply a People `updateContact` Person body to a stored contact.
/// bifrost sends the full modeled `Person` (it fetches, patches, and
/// re-sends), so a field's presence in the body means "write it".
/// Absent fields are left untouched.
fn apply_person_patch(contact: &mut Contact, body: &Value) {
    if let Some(names) = body.get("names").and_then(Value::as_array)
        && let Some(name) = names.first()
    {
        let display = name
            .get("displayName")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                let given = name.get("givenName").and_then(Value::as_str).unwrap_or("");
                let family = name.get("familyName").and_then(Value::as_str).unwrap_or("");
                let joined = format!("{given} {family}");
                let joined = joined.trim().to_string();
                (!joined.is_empty()).then_some(joined)
            });
        contact.display_name = display;
    }
    if let Some(emails) = body.get("emailAddresses").and_then(Value::as_array) {
        contact.emails = emails
            .iter()
            .filter_map(|e| {
                let address = e.get("value").and_then(Value::as_str)?.to_string();
                Some(ContactEmail {
                    address,
                    name: None,
                })
            })
            .collect();
    }
    if let Some(phones) = body.get("phoneNumbers").and_then(Value::as_array) {
        contact.phones = phones
            .iter()
            .filter_map(|p| {
                let number = p.get("value").and_then(Value::as_str)?.to_string();
                let kind = people_phone_kind(p);
                Some(ContactPhone { number, kind })
            })
            .collect();
    }
    if let Some(orgs) = body.get("organizations").and_then(Value::as_array) {
        let org = orgs.first();
        contact.company = org
            .and_then(|o| o.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string);
        contact.job_title = org
            .and_then(|o| o.get("title"))
            .and_then(Value::as_str)
            .map(str::to_string);
        contact.department = org
            .and_then(|o| o.get("department"))
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    if let Some(bios) = body.get("biographies").and_then(Value::as_array) {
        contact.notes = bios
            .first()
            .and_then(|b| b.get("value"))
            .and_then(Value::as_str)
            .map(str::to_string);
    }
}

/// People phone `type` -> fixture kind. `custom` phones carry the label
/// in `formattedType`; use that when present.
fn people_phone_kind(p: &Value) -> Option<String> {
    match (
        p.get("type").and_then(Value::as_str),
        p.get("formattedType").and_then(Value::as_str),
    ) {
        (Some("custom"), Some(label)) if !label.is_empty() => Some(label.to_string()),
        (Some(t), _) => Some(t.to_string()),
        _ => None,
    }
}

fn serialize_person(c: &Contact) -> Value {
    let mut obj = Map::new();
    obj.insert(
        "resourceName".into(),
        Value::String(format!("people/{}", c.id)),
    );
    // ETag derives from the contact id so it stays byte-stable
    // for a given fixture. Real People API emits an opaque base64
    // token; ratatoskr's parser passes it through without
    // interpretation.
    obj.insert("etag".into(), Value::String(format!("etag-{}", c.id)));

    let mut metadata = Map::new();
    metadata.insert("deleted".into(), Value::Bool(false));
    metadata.insert(
        "sources".into(),
        Value::Array(vec![json!({
            "type": "CONTACT",
            "id": c.id,
        })]),
    );
    obj.insert("metadata".into(), Value::Object(metadata));

    if let Some(name) = &c.display_name {
        obj.insert(
            "names".into(),
            Value::Array(vec![json!({
                "displayName": name,
                "givenName": name,
                "familyName": "",
            })]),
        );
    }

    let emails: Vec<Value> = c
        .emails
        .iter()
        .map(|e| {
            json!({
                "value": e.address,
                "type": "other",
            })
        })
        .collect();
    obj.insert("emailAddresses".into(), Value::Array(emails));

    if !c.phones.is_empty() {
        let phones: Vec<Value> = c
            .phones
            .iter()
            .map(|p| match &p.kind {
                Some(k) => json!({ "value": p.number, "type": k }),
                None => json!({ "value": p.number }),
            })
            .collect();
        obj.insert("phoneNumbers".into(), Value::Array(phones));
    }

    if c.company.is_some() || c.job_title.is_some() || c.department.is_some() {
        let mut org = Map::new();
        if let Some(company) = &c.company {
            org.insert("name".into(), Value::String(company.clone()));
        }
        if let Some(title) = &c.job_title {
            org.insert("title".into(), Value::String(title.clone()));
        }
        if let Some(dept) = &c.department {
            org.insert("department".into(), Value::String(dept.clone()));
        }
        obj.insert(
            "organizations".into(),
            Value::Array(vec![Value::Object(org)]),
        );
    }

    if let Some(notes) = &c.notes {
        obj.insert(
            "biographies".into(),
            Value::Array(vec![json!({ "value": notes })]),
        );
    }

    if !c.groups.is_empty() {
        let memberships: Vec<Value> = c
            .groups
            .iter()
            .map(|g| json!({ "contactGroupMembership": { "contactGroupResourceName": g } }))
            .collect();
        obj.insert("memberships".into(), Value::Array(memberships));
    }

    Value::Object(obj)
}
