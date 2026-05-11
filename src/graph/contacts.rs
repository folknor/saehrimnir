//! Microsoft Graph contact endpoints.
//!
//! Implements the subset of `/v1.0/me/contactFolders/...` and
//! `/v1.0/me/contacts/...` (plus the parallel
//! `/v1.0/users/{userId}/...` paths) ratatoskr's
//! `graph_contacts_initial_sync` and `graph_contacts_delta_sync`
//! exercise. v0 covers GET list / single / `contacts/delta`;
//! mutations land via change-script ops.
//!
//! Stage 3 of the multi-account refactor routes per-account on
//! this surface: `/v1.0/me/...` scopes to the primary;
//! `/v1.0/users/{userId}/...` scopes to the named account
//! (`me` aliases the primary; unknown user returns 404).
//! Folder-agnostic `/v1.0/me/contacts/{cid}` looks up the contact
//! within the resolved account.
//!
//! `$select` is parsed but ignored: we always emit the full
//! `id, displayName, emailAddresses, parentFolderId` projection.
//! Real Graph honours $select, but ratatoskr always asks for the
//! full set (`CONTACT_SELECT = "id,displayName,emailAddresses,
//! parentFolderId"`), so the v0 mock can omit the projection.

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
        // /me/...
        .route("/v1.0/me/contactFolders", get(list_folders_me))
        .route("/v1.0/me/contactFolders/{folder}", get(get_folder_me))
        .route(
            "/v1.0/me/contactFolders/{folder}/contacts",
            get(list_contacts_me),
        )
        .route(
            "/v1.0/me/contactFolders/{folder}/contacts/delta",
            get(delta_contacts_me),
        )
        .route(
            "/v1.0/me/contactFolders/{folder}/contacts/{contact}",
            get(get_contact_in_folder_me),
        )
        .route("/v1.0/me/contacts/{contact}", get(get_contact_me))
        // /users/{user}/...
        .route(
            "/v1.0/users/{user}/contactFolders",
            get(list_folders_user),
        )
        .route(
            "/v1.0/users/{user}/contactFolders/{folder}",
            get(get_folder_user),
        )
        .route(
            "/v1.0/users/{user}/contactFolders/{folder}/contacts",
            get(list_contacts_user),
        )
        .route(
            "/v1.0/users/{user}/contactFolders/{folder}/contacts/delta",
            get(delta_contacts_user),
        )
        .route(
            "/v1.0/users/{user}/contactFolders/{folder}/contacts/{contact}",
            get(get_contact_in_folder_user),
        )
        .route(
            "/v1.0/users/{user}/contacts/{contact}",
            get(get_contact_user),
        )
}

// ── /me/ wrappers ───────────────────────────────────────────────────

async fn list_folders_me(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    list_folders_impl(state, &account_id, headers, raw_query, true).await
}

async fn get_folder_me(
    State(state): State<AppState>,
    Path(folder): Path<String>,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    get_folder_impl(state, &account_id, &folder).await
}

async fn list_contacts_me(
    State(state): State<AppState>,
    Path(folder): Path<String>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    list_contacts_impl(state, &account_id, &folder, headers, raw_query, true).await
}

async fn delta_contacts_me(
    State(state): State<AppState>,
    Path(folder): Path<String>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    delta_contacts_impl(state, &account_id, &folder, headers, raw_query, true).await
}

async fn get_contact_in_folder_me(
    State(state): State<AppState>,
    Path((folder, contact)): Path<(String, String)>,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    get_contact_in_folder_impl(state, &account_id, &folder, &contact).await
}

async fn get_contact_me(
    State(state): State<AppState>,
    Path(contact): Path<String>,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    get_contact_impl(state, &account_id, &contact).await
}

// ── /users/{user}/ wrappers ─────────────────────────────────────────

async fn list_folders_user(
    State(state): State<AppState>,
    Path(user): Path<String>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    list_folders_impl(state, &account_id, headers, raw_query, false).await
}

async fn get_folder_user(
    State(state): State<AppState>,
    Path((user, folder)): Path<(String, String)>,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    get_folder_impl(state, &account_id, &folder).await
}

async fn list_contacts_user(
    State(state): State<AppState>,
    Path((user, folder)): Path<(String, String)>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    list_contacts_impl(state, &account_id, &folder, headers, raw_query, false).await
}

async fn delta_contacts_user(
    State(state): State<AppState>,
    Path((user, folder)): Path<(String, String)>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    delta_contacts_impl(state, &account_id, &folder, headers, raw_query, false).await
}

async fn get_contact_in_folder_user(
    State(state): State<AppState>,
    Path((user, folder, contact)): Path<(String, String, String)>,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    get_contact_in_folder_impl(state, &account_id, &folder, &contact).await
}

async fn get_contact_user(
    State(state): State<AppState>,
    Path((user, contact)): Path<(String, String)>,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    get_contact_impl(state, &account_id, &contact).await
}

// ── Inner handlers ──────────────────────────────────────────────────

async fn list_folders_impl(
    state: AppState,
    account_id: &str,
    headers: HeaderMap,
    raw_query: Option<String>,
    me_path: bool,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "list_contact_folders", |_| Ok(())) {
        return o;
    }
    let fixture = state.fixture();
    let q = odata::OdataQuery::parse(raw_query.as_deref());
    let host = host_or_default(&headers);
    let path = if me_path {
        "/v1.0/me/contactFolders".to_string()
    } else {
        format!("/v1.0/users/{account_id}/contactFolders")
    };
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
    let folders: Vec<&ContactFolder> = fixture.contact_folders_for(account_id).collect();
    let total = folders.len();
    let value: Vec<Value> = folders
        .iter()
        .skip(offset as usize)
        .take(top as usize)
        .map(|f| serialize_folder(f))
        .collect();
    let next_offset_val = (offset as usize) + value.len();
    let has_more = total > next_offset_val;

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
                &path,
                raw_query.as_deref(),
                next_off,
            )),
        );
    }
    ok_json(Value::Object(envelope))
}

async fn get_folder_impl(state: AppState, account_id: &str, folder: &str) -> Response {
    let folder_owned = folder.to_string();
    if let Some(o) = super::maybe_override(&state, "get_contact_folder", move |s| {
        crate::lua::req_set_str(s, "folder", &folder_owned)
    }) {
        return o;
    }
    let fixture = state.fixture();
    match resolve_folder(&fixture, folder, account_id) {
        Some(f) => ok_json(serialize_folder(f)),
        None => error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("contact folder {folder:?} not declared in fixture"),
        ),
    }
}

async fn list_contacts_impl(
    state: AppState,
    account_id: &str,
    folder: &str,
    headers: HeaderMap,
    raw_query: Option<String>,
    me_path: bool,
) -> Response {
    let folder_owned = folder.to_string();
    if let Some(o) = super::maybe_override(&state, "list_contacts", move |s| {
        crate::lua::req_set_str(s, "folder", &folder_owned)
    }) {
        return o;
    }
    let fixture = state.fixture();
    let folder_id = match resolve_folder(&fixture, folder, account_id) {
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
    let path = if me_path {
        format!("/v1.0/me/contactFolders/{folder}/contacts")
    } else {
        format!("/v1.0/users/{account_id}/contactFolders/{folder}/contacts")
    };

    let value: Vec<Value> = fixture
        .contacts_for(account_id)
        .filter(|c| c.folder_id == folder_id)
        .skip(offset as usize)
        .take(top as usize)
        .map(serialize_contact)
        .collect();
    let next_offset_val = (offset as usize) + value.len();
    let has_more = fixture
        .contacts_for(account_id)
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

async fn get_contact_in_folder_impl(
    state: AppState,
    account_id: &str,
    folder: &str,
    contact: &str,
) -> Response {
    let folder_owned = folder.to_string();
    let contact_owned = contact.to_string();
    if let Some(o) = super::maybe_override(&state, "get_contact", move |s| {
        crate::lua::req_set_str(s, "folder", &folder_owned)?;
        crate::lua::req_set_str(s, "contact", &contact_owned)
    }) {
        return o;
    }
    let fixture = state.fixture();
    let folder_id = match resolve_folder(&fixture, folder, account_id) {
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
        .contacts_for(account_id)
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

async fn get_contact_impl(state: AppState, account_id: &str, contact: &str) -> Response {
    let contact_owned = contact.to_string();
    if let Some(o) = super::maybe_override(&state, "get_contact", move |s| {
        crate::lua::req_set_str(s, "contact", &contact_owned)
    }) {
        return o;
    }
    let fixture = state.fixture();
    match fixture.contacts_for(account_id).find(|c| c.id == contact) {
        Some(c) => ok_json(serialize_contact(c)),
        None => error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("contact {contact:?} not declared in fixture"),
        ),
    }
}

async fn delta_contacts_impl(
    state: AppState,
    account_id: &str,
    folder: &str,
    headers: HeaderMap,
    raw_query: Option<String>,
    me_path: bool,
) -> Response {
    let folder_owned = folder.to_string();
    if let Some(o) = super::maybe_override(&state, "delta_contacts", move |s| {
        crate::lua::req_set_str(s, "folder", &folder_owned)
    }) {
        return o;
    }
    let fixture = state.fixture();
    let folder_id = match resolve_folder(&fixture, folder, account_id) {
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
    let path = if me_path {
        format!("/v1.0/me/contactFolders/{folder}/contacts/delta")
    } else {
        format!("/v1.0/users/{account_id}/contactFolders/{folder}/contacts/delta")
    };
    let context = format!(
        "https://graph.microsoft.com/v1.0/$metadata#me/contactFolders(\"{folder_id}\")/contacts"
    );

    if q.deltatoken.as_deref() == Some("latest") {
        let delta_link =
            odata::build_delta_link(&host, &path, raw_query.as_deref(), &fixture.state);
        return ok_json(json!({
            "@odata.context": context,
            "value": [],
            "@odata.deltaLink": delta_link,
        }));
    }

    if let Some(token) = q.deltatoken.as_deref() {
        let raw = odata::decode_deltatoken(token).unwrap_or("");
        if let Some(delta) = fixture.contact_delta_since(raw, &folder_id) {
            let by_id: std::collections::HashMap<&str, &crate::fixture::Contact> = fixture
                .contacts_for(account_id)
                .filter(|c| c.folder_id == folder_id)
                .map(|c| (c.id.as_str(), c))
                .collect();
            let mut value: Vec<Value> = Vec::new();
            for id in delta.created.iter().chain(delta.updated.iter()) {
                if let Some(c) = by_id.get(id.as_str()) {
                    value.push(serialize_contact(c));
                }
            }
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
    }

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
        .contacts_for(account_id)
        .filter(|c| c.folder_id == folder_id)
        .skip(offset as usize)
        .take(top as usize)
        .map(serialize_contact)
        .collect();
    let next_offset_val = (offset as usize) + page.len();
    let has_more = fixture
        .contacts_for(account_id)
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

fn graph_contact_tombstone(id: &str) -> Value {
    json!({
        "id": id,
        "@removed": { "reason": "deleted" },
    })
}

/// Look up a folder by id (within the named account), by
/// `is_default = true` ("default" alias), or by lower-case display
/// name. Scoping to one account prevents the `default` alias from
/// resolving across accounts.
fn resolve_folder<'a>(
    fixture: &'a Fixture,
    key: &str,
    account_id: &'a str,
) -> Option<&'a ContactFolder> {
    if key == "default" {
        return fixture
            .contact_folders_for(account_id)
            .find(|f| f.is_default);
    }
    fixture.contact_folders_for(account_id).find(|f| f.id == key)
}

fn host_or_default(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("graph.microsoft.com")
        .to_string()
}
