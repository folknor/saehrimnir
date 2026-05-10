//! Microsoft Graph contact endpoints.
//!
//! Implements the subset of `/v1.0/me/contactFolders/...` and
//! `/v1.0/me/contacts/...` ratatoskr's `graph_contacts_initial_sync`
//! and `graph_contacts_delta_sync` exercise. v0 covers:
//!
//! - `GET /v1.0/me/contactFolders` (list, paged via `$top`).
//! - `GET /v1.0/me/contactFolders/{id}` (single).
//! - `GET /v1.0/me/contactFolders/{id}/contacts` (list with
//!   `$top` / `$skiptoken` / `$select`).
//! - `GET /v1.0/me/contactFolders/{id}/contacts/{cid}` (single).
//! - `GET /v1.0/me/contacts/{cid}` (single, folder-agnostic shortcut).
//! - `GET /v1.0/me/contactFolders/{id}/contacts/delta` (initial dump
//!   paginated to a `@odata.deltaLink`; follow-ups walk the change
//!   log from the client-supplied `$deltatoken`; `$deltatoken=latest`
//!   shortcut returns an empty page with a fresh deltaLink).
//!
//! All endpoints are read paths; mutations land via change-script
//! ops in the next slice (see `notes/fixture-format.md`).
//! Bearer-enforcement and request-logging are applied by the parent
//! router's middleware.
//!
//! `$select` is parsed but ignored: we always emit the full
//! `id, displayName, emailAddresses, parentFolderId` projection so
//! the wire shape is stable. Real Graph honours $select, but
//! ratatoskr always asks for the full set
//! (`CONTACT_SELECT = "id,displayName,emailAddresses,parentFolderId"`),
//! so the v0 mock can omit the projection without breaking the
//! client.

use axum::{
    Router,
    extract::{Path, RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::get,
};
use serde_json::{Map, Value, json};

use super::{AppState, error, odata, ok_json};
use crate::fixture::{Contact, ContactFolder, Fixture};

const CONTACTS_DEFAULT_TOP: u32 = 50;
const CONTACTS_MAX_TOP: u32 = 999;

const FOLDERS_DEFAULT_TOP: u32 = 100;
const FOLDERS_MAX_TOP: u32 = 250;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1.0/me/contactFolders", get(list_folders))
        .route("/v1.0/me/contactFolders/{folder}", get(get_folder))
        .route(
            "/v1.0/me/contactFolders/{folder}/contacts",
            get(list_contacts),
        )
        .route(
            "/v1.0/me/contactFolders/{folder}/contacts/delta",
            get(delta_contacts),
        )
        .route(
            "/v1.0/me/contactFolders/{folder}/contacts/{contact}",
            get(get_contact_in_folder),
        )
        .route("/v1.0/me/contacts/{contact}", get(get_contact))
}

// ── Listing / single-resource projection ────────────────────────────

async fn list_folders(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "list_contact_folders", |_| Ok(())) {
        return o;
    }
    let fixture = state.fixture();
    let q = odata::OdataQuery::parse(raw_query.as_deref());
    let host = host_or_default(&headers);
    let path = "/v1.0/me/contactFolders";
    let top = q.page_size(FOLDERS_DEFAULT_TOP, FOLDERS_MAX_TOP);
    let offset = match q.offset() {
        Some(o) => o,
        None => {
            return error(
                StatusCode::BAD_REQUEST,
                "InvalidQueryParameter",
                "$skiptoken did not decode - reset pagination by retrying without it",
            );
        }
    };
    let value: Vec<Value> = fixture
        .contact_folders
        .iter()
        .skip(offset as usize)
        .take(top as usize)
        .map(serialize_folder)
        .collect();
    let next_offset_val = (offset as usize) + value.len();
    let has_more = fixture.contact_folders.len() > next_offset_val;

    let mut envelope = Map::new();
    envelope.insert(
        "@odata.context".to_string(),
        Value::String(
            "https://graph.microsoft.com/v1.0/$metadata#me/contactFolders".to_string(),
        ),
    );
    envelope.insert("value".to_string(), Value::Array(value));
    if has_more {
        let next_off = u32::try_from(next_offset_val).unwrap_or(u32::MAX);
        envelope.insert(
            "@odata.nextLink".to_string(),
            Value::String(odata::build_next_link(
                &host,
                path,
                raw_query.as_deref(),
                next_off,
            )),
        );
    }
    ok_json(Value::Object(envelope))
}

async fn get_folder(
    State(state): State<AppState>,
    Path(folder): Path<String>,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "get_contact_folder", |s| {
        crate::lua::req_set_str(s, "folder", &folder)
    }) {
        return o;
    }
    let fixture = state.fixture();
    match resolve_folder(&fixture, &folder) {
        Some(f) => ok_json(serialize_folder(f)),
        None => error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("contact folder {folder:?} not declared in fixture"),
        ),
    }
}

async fn list_contacts(
    State(state): State<AppState>,
    Path(folder): Path<String>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "list_contacts", |s| {
        crate::lua::req_set_str(s, "folder", &folder)
    }) {
        return o;
    }
    let fixture = state.fixture();
    let folder_id = match resolve_folder(&fixture, &folder) {
        Some(f) => f.id.clone(),
        None => {
            return error(
                StatusCode::NOT_FOUND,
                "ResourceNotFound",
                &format!("contact folder {folder:?} not declared in fixture"),
            );
        }
    };
    let q = odata::OdataQuery::parse(raw_query.as_deref());
    let top = q.page_size(CONTACTS_DEFAULT_TOP, CONTACTS_MAX_TOP);
    let offset = match q.offset() {
        Some(o) => o,
        None => {
            return error(
                StatusCode::BAD_REQUEST,
                "InvalidQueryParameter",
                "$skiptoken did not decode - reset pagination by retrying without it",
            );
        }
    };
    let host = host_or_default(&headers);
    let path = format!("/v1.0/me/contactFolders/{folder}/contacts");

    let value: Vec<Value> = fixture
        .contacts
        .iter()
        .filter(|c| c.folder_id == folder_id)
        .skip(offset as usize)
        .take(top as usize)
        .map(serialize_contact)
        .collect();
    let next_offset_val = (offset as usize) + value.len();
    let has_more = fixture
        .contacts
        .iter()
        .filter(|c| c.folder_id == folder_id)
        .nth(next_offset_val)
        .is_some();

    let mut envelope = Map::new();
    envelope.insert(
        "@odata.context".to_string(),
        Value::String(format!(
            "https://graph.microsoft.com/v1.0/$metadata#me/contactFolders(\"{folder_id}\")/contacts"
        )),
    );
    envelope.insert("value".to_string(), Value::Array(value));
    if has_more {
        let next_off = u32::try_from(next_offset_val).unwrap_or(u32::MAX);
        envelope.insert(
            "@odata.nextLink".to_string(),
            Value::String(odata::build_next_link(
                &host,
                &path,
                raw_query.as_deref(),
                next_off,
            )),
        );
    }
    ok_json(Value::Object(envelope))
}

async fn get_contact_in_folder(
    State(state): State<AppState>,
    Path((folder, contact)): Path<(String, String)>,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "get_contact", |s| {
        crate::lua::req_set_str(s, "folder", &folder)?;
        crate::lua::req_set_str(s, "contact", &contact)
    }) {
        return o;
    }
    let fixture = state.fixture();
    let folder_id = match resolve_folder(&fixture, &folder) {
        Some(f) => f.id.clone(),
        None => {
            return error(
                StatusCode::NOT_FOUND,
                "ResourceNotFound",
                &format!("contact folder {folder:?} not declared in fixture"),
            );
        }
    };
    match fixture
        .contacts
        .iter()
        .find(|c| c.id == contact && c.folder_id == folder_id)
    {
        Some(c) => ok_json(serialize_contact(c)),
        None => error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("contact {contact:?} not in folder {folder:?}"),
        ),
    }
}

async fn get_contact(
    State(state): State<AppState>,
    Path(contact): Path<String>,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "get_contact", |s| {
        crate::lua::req_set_str(s, "contact", &contact)
    }) {
        return o;
    }
    let fixture = state.fixture();
    match fixture.contacts.iter().find(|c| c.id == contact) {
        Some(c) => ok_json(serialize_contact(c)),
        None => error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("contact {contact:?} not declared in fixture"),
        ),
    }
}

async fn delta_contacts(
    State(state): State<AppState>,
    Path(folder): Path<String>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "delta_contacts", |s| {
        crate::lua::req_set_str(s, "folder", &folder)
    }) {
        return o;
    }
    let fixture = state.fixture();
    let folder_id = match resolve_folder(&fixture, &folder) {
        Some(f) => f.id.clone(),
        None => {
            return error(
                StatusCode::NOT_FOUND,
                "ResourceNotFound",
                &format!("contact folder {folder:?} not declared in fixture"),
            );
        }
    };
    let q = odata::OdataQuery::parse(raw_query.as_deref());
    let host = host_or_default(&headers);
    let path = format!("/v1.0/me/contactFolders/{folder}/contacts/delta");
    let context = format!(
        "https://graph.microsoft.com/v1.0/$metadata#me/contactFolders(\"{folder_id}\")/contacts"
    );

    // `$deltatoken=latest` shortcut: fresh deltaLink, no contact dump.
    if q.deltatoken.as_deref() == Some("latest") {
        let delta_link =
            odata::build_delta_link(&host, &path, raw_query.as_deref(), &fixture.state);
        return ok_json(json!({
            "@odata.context": context,
            "value": [],
            "@odata.deltaLink": delta_link,
        }));
    }

    // Subsequent delta cycles: walk the change log between the
    // client-supplied token and the current state. Created /
    // updated contacts project as full bodies; destroyed contacts
    // emit Graph-shaped tombstones. Unknown / evicted token falls
    // through to the bootstrap path (real Graph signals 410 Gone;
    // ratatoskr's contact_sync handles that by retriggering a full
    // sync and re-bootstrapping the deltaLink, so an immediate
    // bootstrap is a coherent v0 stand-in).
    if let Some(token) = q.deltatoken.as_deref() {
        let raw = odata::decode_deltatoken(token).unwrap_or("");
        if let Some(delta) = fixture.contact_delta_since(raw, &folder_id) {
            // Build an id -> &Contact map once; per-id `find` over
            // `fixture.contacts` would otherwise be O(K · N).
            let by_id: std::collections::HashMap<&str, &crate::fixture::Contact> = fixture
                .contacts
                .iter()
                .filter(|c| c.folder_id == folder_id)
                .map(|c| (c.id.as_str(), c))
                .collect();
            let mut value: Vec<Value> = Vec::new();
            for id in delta.created.iter().chain(delta.updated.iter()) {
                if let Some(c) = by_id.get(id.as_str()) {
                    value.push(serialize_contact(c));
                }
            }
            // Tombstones are pre-filtered to this folder by
            // `contact_delta_since`; sibling-folder destroys never
            // surface here.
            for id in &delta.destroyed {
                value.push(graph_contact_tombstone(id));
            }
            let delta_link =
                odata::build_delta_link(&host, &path, raw_query.as_deref(), &fixture.state);
            return ok_json(json!({
                "@odata.context": context,
                "value": value,
                "@odata.deltaLink": delta_link,
            }));
        }
        // Token unknown / evicted: fall through to full bootstrap.
    }

    // Bootstrap: paginate the full contact dump for the folder with
    // `@odata.nextLink`; only the final page emits
    // `@odata.deltaLink` pinned to the current fixture state.
    let top = q.page_size(CONTACTS_DEFAULT_TOP, CONTACTS_MAX_TOP);
    let offset = match q.offset() {
        Some(o) => o,
        None => {
            return error(
                StatusCode::BAD_REQUEST,
                "InvalidQueryParameter",
                "$skiptoken did not decode - reset pagination by retrying without it",
            );
        }
    };
    let page: Vec<Value> = fixture
        .contacts
        .iter()
        .filter(|c| c.folder_id == folder_id)
        .skip(offset as usize)
        .take(top as usize)
        .map(serialize_contact)
        .collect();
    let next_offset_val = (offset as usize) + page.len();
    let has_more = fixture
        .contacts
        .iter()
        .filter(|c| c.folder_id == folder_id)
        .nth(next_offset_val)
        .is_some();

    let mut envelope = Map::new();
    envelope.insert("@odata.context".to_string(), Value::String(context));
    envelope.insert("value".to_string(), Value::Array(page));
    if has_more {
        let next_off = u32::try_from(next_offset_val).unwrap_or(u32::MAX);
        envelope.insert(
            "@odata.nextLink".to_string(),
            Value::String(odata::build_next_link(
                &host,
                &path,
                raw_query.as_deref(),
                next_off,
            )),
        );
    } else {
        envelope.insert(
            "@odata.deltaLink".to_string(),
            Value::String(odata::build_delta_link(
                &host,
                &path,
                raw_query.as_deref(),
                &fixture.state,
            )),
        );
    }
    ok_json(Value::Object(envelope))
}

// ── Serialisation + helpers ──────────────────────────────────────────

fn serialize_folder(folder: &ContactFolder) -> Value {
    let mut obj = Map::new();
    obj.insert("id".to_string(), Value::String(folder.id.clone()));
    obj.insert(
        "displayName".to_string(),
        Value::String(folder.display_name.clone()),
    );
    if let Some(parent) = &folder.parent_folder_id {
        obj.insert(
            "parentFolderId".to_string(),
            Value::String(parent.clone()),
        );
    }
    Value::Object(obj)
}

fn serialize_contact(contact: &Contact) -> Value {
    let mut obj = Map::new();
    obj.insert("id".to_string(), Value::String(contact.id.clone()));
    obj.insert(
        "parentFolderId".to_string(),
        Value::String(contact.folder_id.clone()),
    );
    if let Some(name) = &contact.display_name {
        obj.insert("displayName".to_string(), Value::String(name.clone()));
    }
    let emails: Vec<Value> = contact
        .emails
        .iter()
        .map(|e| {
            let mut em = Map::new();
            em.insert("address".to_string(), Value::String(e.address.clone()));
            if let Some(n) = &e.name {
                em.insert("name".to_string(), Value::String(n.clone()));
            }
            Value::Object(em)
        })
        .collect();
    obj.insert("emailAddresses".to_string(), Value::Array(emails));
    Value::Object(obj)
}

/// Graph-style deletion stub for `contacts/delta`. Mirrors the
/// `events/delta` tombstone shape: `{id, "@removed": {reason: "deleted"}}`.
fn graph_contact_tombstone(id: &str) -> Value {
    json!({
        "id": id,
        "@removed": { "reason": "deleted" },
    })
}

/// Look up a folder by id, by `is_default = true` ("default" alias),
/// or by lower-case display name. Real Graph supports the literal id
/// plus a couple of well-known aliases (`contacts` doesn't have one
/// in the `mailFolders` sense, but tests sometimes rely on `default`
/// pointing at the canonical Contacts folder, so v0 supports it).
fn resolve_folder<'a>(fixture: &'a Fixture, key: &str) -> Option<&'a ContactFolder> {
    if key == "default" {
        return fixture.contact_folders.iter().find(|f| f.is_default);
    }
    fixture.contact_folders.iter().find(|f| f.id == key)
}

fn host_or_default(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("graph.microsoft.com")
        .to_string()
}
