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
    Json, Router,
    extract::{Path, RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Map, Value, json};

use super::{AppState, error, odata, ok_json};
use crate::fixture::{Contact, ContactEmail, ContactFolder, ContactPhone, Fixture, MutationDiff};

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
            get(list_contacts_me).post(create_contact_in_folder_me),
        )
        .route(
            "/v1.0/me/contactFolders/{folder}/contacts/delta",
            get(delta_contacts_me),
        )
        .route(
            "/v1.0/me/contactFolders/{folder}/contacts/{contact}",
            get(get_contact_in_folder_me),
        )
        .route(
            "/v1.0/me/contacts/{contact}",
            get(get_contact_me)
                .patch(update_contact_me)
                .delete(delete_contact_me),
        )
        .route(
            "/v1.0/me/contacts",
            get(list_all_contacts_me).post(create_contact_default_me),
        )
        // /users/{user}/...
        .route("/v1.0/users/{user}/contactFolders", get(list_folders_user))
        .route(
            "/v1.0/users/{user}/contactFolders/{folder}",
            get(get_folder_user),
        )
        .route(
            "/v1.0/users/{user}/contactFolders/{folder}/contacts",
            get(list_contacts_user).post(create_contact_in_folder_user),
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
            get(get_contact_user)
                .patch(update_contact_user)
                .delete(delete_contact_user),
        )
        .route(
            "/v1.0/users/{user}/contacts",
            get(list_all_contacts_user).post(create_contact_default_user),
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

async fn get_folder_me(State(state): State<AppState>, Path(folder): Path<String>) -> Response {
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

async fn list_all_contacts_me(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    list_all_contacts_impl(state, &account_id, headers, raw_query, true).await
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

async fn get_contact_me(State(state): State<AppState>, Path(contact): Path<String>) -> Response {
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

async fn list_all_contacts_user(
    State(state): State<AppState>,
    Path(user): Path<String>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    list_all_contacts_impl(state, &account_id, headers, raw_query, false).await
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
        Value::String("https://graph.microsoft.com/v1.0/$metadata#me/contactFolders".to_string()),
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
    let email = q.filter.as_deref().and_then(parse_contact_email_filter);

    let value: Vec<Value> = fixture
        .contacts_for(account_id)
        .filter(|c| c.folder_id == folder_id)
        .filter(|c| contact_email_matches(c, email.as_deref()))
        .skip(offset as usize)
        .take(top as usize)
        .map(serialize_contact)
        .collect();
    let next_offset_val = (offset as usize) + value.len();
    let has_more = fixture
        .contacts_for(account_id)
        .filter(|c| c.folder_id == folder_id)
        .filter(|c| contact_email_matches(c, email.as_deref()))
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

/// `GET /v1.0/me/contacts` (account-wide, folder-agnostic). bifrost's
/// `contacts_list(None)` / `contact_search(None)` hit this when no
/// address book is named (the mock only had the folder-scoped
/// `/contactFolders/{id}/contacts` and the single `/contacts/{id}`).
/// Lists every contact in the account across folders, `$top` /
/// `$skiptoken` paginated; `$select` / `$filter` are parsed and
/// ignored (we always emit the full projection, matching the
/// folder-scoped handler).
async fn list_all_contacts_impl(
    state: AppState,
    account_id: &str,
    headers: HeaderMap,
    raw_query: Option<String>,
    me_path: bool,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "list_contacts", |_| Ok(())) {
        return o;
    }
    let fixture = state.fixture();
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
        "/v1.0/me/contacts".to_string()
    } else {
        format!("/v1.0/users/{account_id}/contacts")
    };
    let email = q.filter.as_deref().and_then(parse_contact_email_filter);

    let value: Vec<Value> = fixture
        .contacts_for(account_id)
        .filter(|c| contact_email_matches(c, email.as_deref()))
        .skip(offset as usize)
        .take(top as usize)
        .map(serialize_contact)
        .collect();
    let next_offset_val = (offset as usize) + value.len();
    let has_more = fixture
        .contacts_for(account_id)
        .filter(|c| contact_email_matches(c, email.as_deref()))
        .nth(next_offset_val)
        .is_some();

    let mut envelope = Map::new();
    envelope.insert(
        "@odata.context".to_string(),
        Value::String("https://graph.microsoft.com/v1.0/$metadata#contacts".to_string()),
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
        let delta_link = odata::build_delta_link(
            &host,
            &path,
            raw_query.as_deref(),
            fixture.state_for(account_id),
        );
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
            let delta_link = odata::build_delta_link(
                &host,
                &path,
                raw_query.as_deref(),
                fixture.state_for(account_id),
            );
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
                fixture.state_for(account_id),
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
        obj.insert("parentFolderId".to_string(), Value::String(parent.clone()));
    }
    Value::Object(obj)
}

// ── Contact mutation (create / update / delete) ─────────────────────
//
// bifrost's contact pipeline: create POST .../contacts (default
// folder) or .../contactFolders/{id}/contacts; update PATCH (sparse)
// .../contacts/{id}; delete DELETE .../contacts/{id}. Mutations land
// on the shared `Contact` set through `Fixture::mutate`, recording
// `contact_*` transitions so `contacts/delta` and JMAP
// `ContactCard/changes` observe them. The fixture `Contact` carries
// only displayName + emails + folder, so other Graph contact fields
// (phones / addresses / company / jobTitle / notes) are accepted but
// not durably stored.

async fn create_contact_default_me(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    create_contact_impl(state, &account_id, None, body).await
}

async fn create_contact_in_folder_me(
    State(state): State<AppState>,
    Path(folder): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    create_contact_impl(state, &account_id, Some(folder), body).await
}

async fn update_contact_me(
    State(state): State<AppState>,
    Path(contact): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    update_contact_impl(state, &account_id, &contact, body).await
}

async fn delete_contact_me(State(state): State<AppState>, Path(contact): Path<String>) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    delete_contact_impl(state, &account_id, &contact).await
}

async fn create_contact_default_user(
    State(state): State<AppState>,
    Path(user): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    create_contact_impl(state, &account_id, None, body).await
}

async fn create_contact_in_folder_user(
    State(state): State<AppState>,
    Path((user, folder)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    create_contact_impl(state, &account_id, Some(folder), body).await
}

async fn update_contact_user(
    State(state): State<AppState>,
    Path((user, contact)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    update_contact_impl(state, &account_id, &contact, body).await
}

async fn delete_contact_user(
    State(state): State<AppState>,
    Path((user, contact)): Path<(String, String)>,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    delete_contact_impl(state, &account_id, &contact).await
}

async fn create_contact_impl(
    state: AppState,
    account_id: &str,
    folder: Option<String>,
    body: Value,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "create_contact", |_| Ok(())) {
        return o;
    }
    let folder_id = {
        let fixture = state.fixture();
        match &folder {
            Some(f) => match resolve_folder(&fixture, f, account_id) {
                Some(cf) => cf.id.clone(),
                None => {
                    return error(
                        StatusCode::NOT_FOUND,
                        "ResourceNotFound",
                        &format!("contact folder {f:?} not found"),
                    );
                }
            },
            // No folder named: the default Contacts folder (or the
            // first declared one).
            None => match fixture
                .contact_folders_for(account_id)
                .find(|cf| cf.is_default)
                .or_else(|| fixture.contact_folders_for(account_id).next())
            {
                Some(cf) => cf.id.clone(),
                None => {
                    return error(
                        StatusCode::NOT_FOUND,
                        "ResourceNotFound",
                        "account has no contact folder",
                    );
                }
            },
        }
    };
    let projected = {
        let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
        create_contact_core(&mut fix, account_id, &folder_id, &body)
    };
    (StatusCode::CREATED, axum::Json(projected)).into_response()
}

async fn update_contact_impl(
    state: AppState,
    account_id: &str,
    contact: &str,
    body: Value,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "update_contact", |_| Ok(())) {
        return o;
    }
    let projected = {
        let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
        update_contact_core(&mut fix, account_id, contact, &body)
    };
    match projected {
        Some(v) => ok_json(v),
        None => error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("contact {contact:?} not found"),
        ),
    }
}

async fn delete_contact_impl(state: AppState, account_id: &str, contact: &str) -> Response {
    if let Some(o) = super::maybe_override(&state, "delete_contact", |_| Ok(())) {
        return o;
    }
    let found = {
        let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
        delete_contact_core(&mut fix, account_id, contact)
    };
    if found {
        StatusCode::NO_CONTENT.into_response()
    } else {
        error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("contact {contact:?} not found"),
        )
    }
}

fn create_contact_core(
    fix: &mut Fixture,
    account_id: &str,
    folder_id: &str,
    body: &Value,
) -> Value {
    let display_name = body
        .get("displayName")
        .and_then(Value::as_str)
        .map(str::to_string);
    let emails = parse_graph_emails(body.get("emailAddresses"));
    let phones = parse_graph_phones(body);
    let company = body
        .get("companyName")
        .and_then(Value::as_str)
        .map(str::to_string);
    let job_title = body
        .get("jobTitle")
        .and_then(Value::as_str)
        .map(str::to_string);
    let department = body
        .get("department")
        .and_then(Value::as_str)
        .map(str::to_string);
    let notes = body
        .get("personalNotes")
        .and_then(Value::as_str)
        .map(str::to_string);
    let acct = account_id.to_string();
    let fid = folder_id.to_string();
    let mut projected = Value::Null;
    let _ = fix.mutate(|f| {
        let new_id = f.mint_contact_id();
        let contact = Contact {
            id: new_id.clone(),
            account_id: acct.clone(),
            folder_id: fid.clone(),
            display_name,
            emails,
            phones,
            company,
            job_title,
            department,
            notes,
            groups: Vec::new(),
            malformed_vcard: false,
        };
        f.contacts.push(contact.clone());
        projected = serialize_contact(&contact);
        MutationDiff {
            contact_created: vec![new_id],
            ..Default::default()
        }
    });
    projected
}

fn update_contact_core(
    fix: &mut Fixture,
    account_id: &str,
    contact_id: &str,
    body: &Value,
) -> Option<Value> {
    let acct = account_id.to_string();
    let cid = contact_id.to_string();
    let mut projected = None;
    let _ = fix.mutate(|f| {
        let Some(idx) = f
            .contacts
            .iter()
            .position(|c| c.account_id == acct && c.id == cid)
        else {
            return MutationDiff::default();
        };
        let mut c = f.contacts[idx].clone();
        // Sparse PATCH: omitted -> untouched; null -> clear.
        if let Some(dn) = body.get("displayName") {
            c.display_name = if dn.is_null() {
                None
            } else {
                dn.as_str().map(str::to_string)
            };
        }
        if let Some(ea) = body.get("emailAddresses") {
            c.emails = parse_graph_emails(Some(ea));
        }
        // Any of the three phone keys present -> recompute the whole
        // phone set from the body (Graph PATCH replaces the collection).
        if body.get("homePhones").is_some()
            || body.get("businessPhones").is_some()
            || body.get("mobilePhone").is_some()
        {
            c.phones = parse_graph_phones(body);
        }
        if let Some(v) = body.get("companyName") {
            c.company = if v.is_null() {
                None
            } else {
                v.as_str().map(str::to_string)
            };
        }
        if let Some(v) = body.get("jobTitle") {
            c.job_title = if v.is_null() {
                None
            } else {
                v.as_str().map(str::to_string)
            };
        }
        if let Some(v) = body.get("department") {
            c.department = if v.is_null() {
                None
            } else {
                v.as_str().map(str::to_string)
            };
        }
        if let Some(v) = body.get("personalNotes") {
            c.notes = if v.is_null() {
                None
            } else {
                v.as_str().map(str::to_string)
            };
        }
        f.contacts[idx] = c.clone();
        projected = Some(serialize_contact(&c));
        MutationDiff {
            contact_updated: vec![cid.clone()],
            ..Default::default()
        }
    });
    projected
}

fn delete_contact_core(fix: &mut Fixture, account_id: &str, contact_id: &str) -> bool {
    let acct = account_id.to_string();
    let cid = contact_id.to_string();
    let Some(folder) = fix
        .contacts
        .iter()
        .find(|c| c.account_id == acct && c.id == cid)
        .map(|c| c.folder_id.clone())
    else {
        return false;
    };
    let _ = fix.mutate(move |f| {
        f.contacts
            .retain(|c| !(c.account_id == acct && c.id == cid));
        MutationDiff {
            contact_destroyed: vec![cid.clone()],
            contact_destroyed_parents: vec![folder.clone()],
            ..Default::default()
        }
    });
    true
}

/// Graph `emailAddresses` (`[{ address, name? }]`) -> fixture
/// `ContactEmail`s.
fn parse_graph_emails(v: Option<&Value>) -> Vec<ContactEmail> {
    let Some(arr) = v.and_then(Value::as_array) else {
        return vec![];
    };
    arr.iter()
        .filter_map(|e| {
            let address = e.get("address").and_then(Value::as_str)?.to_string();
            let name = e.get("name").and_then(Value::as_str).map(str::to_string);
            Some(ContactEmail { address, name })
        })
        .collect()
}

/// Extract the address from a Graph contact `$filter` of the form
/// `emailAddresses/any(a:a/address eq 'X')`. Returns `None` for any
/// other shape (caller then lists unfiltered).
fn parse_contact_email_filter(filter: &str) -> Option<String> {
    let start = filter.find('\'')?;
    let rest = &filter[start + 1..];
    let end = rest.find('\'')?;
    Some(rest[..end].to_string())
}

/// `true` when the contact matches the parsed email `$filter` (`None`
/// = no filter, matches all). Address comparison is case-insensitive.
fn contact_email_matches(c: &Contact, want: Option<&str>) -> bool {
    match want {
        None => true,
        Some(addr) => c
            .emails
            .iter()
            .any(|e| e.address.eq_ignore_ascii_case(addr)),
    }
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

    // Phones fan out by kind into the Outlook contact shape: `mobile`
    // -> the single `mobilePhone`, `work`/`business` -> `businessPhones`,
    // everything else -> `homePhones`. This is the inverse of
    // `parse_graph_phones` so a written phone round-trips.
    let mut home_phones: Vec<Value> = Vec::new();
    let mut business_phones: Vec<Value> = Vec::new();
    let mut mobile_phone: Option<String> = None;
    for p in &contact.phones {
        match p.kind.as_deref() {
            Some("mobile" | "cell") => {
                if mobile_phone.is_none() {
                    mobile_phone = Some(p.number.clone());
                }
            }
            Some("work" | "business") => {
                business_phones.push(Value::String(p.number.clone()));
            }
            _ => home_phones.push(Value::String(p.number.clone())),
        }
    }
    obj.insert("homePhones".to_string(), Value::Array(home_phones));
    obj.insert("businessPhones".to_string(), Value::Array(business_phones));
    if let Some(mobile) = mobile_phone {
        obj.insert("mobilePhone".to_string(), Value::String(mobile));
    }

    if let Some(company) = &contact.company {
        obj.insert("companyName".to_string(), Value::String(company.clone()));
    }
    if let Some(title) = &contact.job_title {
        obj.insert("jobTitle".to_string(), Value::String(title.clone()));
    }
    if let Some(dept) = &contact.department {
        obj.insert("department".to_string(), Value::String(dept.clone()));
    }
    if let Some(notes) = &contact.notes {
        obj.insert("personalNotes".to_string(), Value::String(notes.clone()));
    }
    Value::Object(obj)
}

/// Collect the Outlook contact phone shape (`homePhones` /
/// `businessPhones` / `mobilePhone`) back into fixture `ContactPhone`s,
/// stamping the kind so a Graph write round-trips through the fanning
/// serializer above.
fn parse_graph_phones(body: &Value) -> Vec<ContactPhone> {
    let mut out = Vec::new();
    if let Some(arr) = body.get("homePhones").and_then(Value::as_array) {
        for v in arr {
            if let Some(n) = v.as_str() {
                out.push(ContactPhone {
                    number: n.to_string(),
                    kind: Some("home".to_string()),
                });
            }
        }
    }
    if let Some(arr) = body.get("businessPhones").and_then(Value::as_array) {
        for v in arr {
            if let Some(n) = v.as_str() {
                out.push(ContactPhone {
                    number: n.to_string(),
                    kind: Some("work".to_string()),
                });
            }
        }
    }
    if let Some(n) = body.get("mobilePhone").and_then(Value::as_str) {
        out.push(ContactPhone {
            number: n.to_string(),
            kind: Some("mobile".to_string()),
        });
    }
    out
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
    fixture
        .contact_folders_for(account_id)
        .find(|f| f.id == key)
}

fn host_or_default(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("graph.microsoft.com")
        .to_string()
}
