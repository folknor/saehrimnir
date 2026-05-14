//! Microsoft Graph mail-sync endpoints.
//!
//! Implements the subset of `/v1.0/me/mailFolders/...` and the
//! parallel `/v1.0/users/{userId}/mailFolders/...` paths
//! ratatoskr's Graph client exercises during initial and delta
//! sync. See `notes/ratatoskr-graph-surface.md`.
//!
//! Stage 3 of the multi-account refactor introduces per-account
//! routing on the Graph mail surface: `/v1.0/me/...` continues to
//! scope to the primary account (matching every v0 single-account
//! fixture); `/v1.0/users/{userId}/...` scopes to the
//! `userId`-named declared account (`me` is accepted as an alias
//! for the primary). Unknown `userId` returns 404
//! `ResourceNotFound`. The same set of inner `*_impl` handlers
//! powers both paths.

use axum::{
    Router,
    extract::{Path, RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::get,
};
use chrono::SecondsFormat;
use serde_json::{Map, Value, json};

use axum::body::Body as AxumBody;
use axum::http::header;
use axum::response::IntoResponse;

use super::{AppState, error, odata, ok_json};
use crate::fixture::{Address, Attachment, Body, Disposition, Email, Fixture, Mailbox, Role};

/// Default page size for messages, matching ratatoskr's BATCH_SIZE.
const MESSAGES_DEFAULT_TOP: u32 = 50;
const MESSAGES_MAX_TOP: u32 = 256;
/// Default page size for folder enumeration.
const FOLDERS_DEFAULT_TOP: u32 = 250;
const FOLDERS_MAX_TOP: u32 = 250;

pub fn router() -> Router<AppState> {
    Router::new()
        // /v1.0/me/...
        .route("/v1.0/me/mailFolders", get(list_folders_me))
        .route("/v1.0/me/mailFolders/{folder}", get(get_folder_me))
        .route(
            "/v1.0/me/mailFolders/{folder}/childFolders",
            get(list_child_folders_me),
        )
        .route(
            "/v1.0/me/mailFolders/{folder}/messages",
            get(list_messages_me),
        )
        .route(
            "/v1.0/me/mailFolders/{folder}/messages/delta",
            get(delta_messages_me),
        )
        .route(
            "/v1.0/me/messages/{message_id}/attachments",
            get(list_message_attachments_me),
        )
        .route(
            "/v1.0/me/messages/{message_id}/attachments/{attachment_id}",
            get(get_message_attachment_me),
        )
        .route(
            "/v1.0/me/messages/{message_id}/attachments/{attachment_id}/$value",
            get(get_message_attachment_value_me),
        )
        // /v1.0/users/{userId}/...
        .route("/v1.0/users/{user}/mailFolders", get(list_folders_user))
        .route("/v1.0/users/{user}/mailFolders/{folder}", get(get_folder_user))
        .route(
            "/v1.0/users/{user}/mailFolders/{folder}/childFolders",
            get(list_child_folders_user),
        )
        .route(
            "/v1.0/users/{user}/mailFolders/{folder}/messages",
            get(list_messages_user),
        )
        .route(
            "/v1.0/users/{user}/mailFolders/{folder}/messages/delta",
            get(delta_messages_user),
        )
        .route(
            "/v1.0/users/{user}/messages/{message_id}/attachments",
            get(list_message_attachments_user),
        )
        .route(
            "/v1.0/users/{user}/messages/{message_id}/attachments/{attachment_id}",
            get(get_message_attachment_user),
        )
        .route(
            "/v1.0/users/{user}/messages/{message_id}/attachments/{attachment_id}/$value",
            get(get_message_attachment_value_user),
        )
}

// ── /me/ route wrappers ─────────────────────────────────────────────

async fn list_folders_me(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    list_folders_impl(state, &account_id, headers, raw, /*me_path=*/ true).await
}

async fn get_folder_me(State(state): State<AppState>, Path(folder): Path<String>) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    get_folder_impl(state, &account_id, &folder, /*me_path=*/ true).await
}

async fn list_child_folders_me(
    State(state): State<AppState>,
    Path(folder): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    list_child_folders_impl(state, &account_id, &folder, headers, raw, true).await
}

async fn list_messages_me(
    State(state): State<AppState>,
    Path(folder): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    list_messages_impl(state, &account_id, &folder, headers, raw, true).await
}

async fn delta_messages_me(
    State(state): State<AppState>,
    Path(folder): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    delta_messages_impl(state, &account_id, &folder, headers, raw, true).await
}

async fn list_message_attachments_me(
    State(state): State<AppState>,
    Path(message_id): Path<String>,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    list_message_attachments_impl(state, &account_id, &message_id).await
}

async fn get_message_attachment_me(
    State(state): State<AppState>,
    Path((message_id, attachment_id)): Path<(String, String)>,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    get_message_attachment_impl(state, &account_id, &message_id, &attachment_id).await
}

async fn get_message_attachment_value_me(
    State(state): State<AppState>,
    Path((message_id, attachment_id)): Path<(String, String)>,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    get_message_attachment_value_impl(state, &account_id, &message_id, &attachment_id).await
}

// ── /users/{user}/ route wrappers ───────────────────────────────────

async fn list_folders_user(
    State(state): State<AppState>,
    Path(user): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    list_folders_impl(state, &account_id, headers, raw, false).await
}

async fn get_folder_user(
    State(state): State<AppState>,
    Path((user, folder)): Path<(String, String)>,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    get_folder_impl(state, &account_id, &folder, false).await
}

async fn list_child_folders_user(
    State(state): State<AppState>,
    Path((user, folder)): Path<(String, String)>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    list_child_folders_impl(state, &account_id, &folder, headers, raw, false).await
}

async fn list_messages_user(
    State(state): State<AppState>,
    Path((user, folder)): Path<(String, String)>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    list_messages_impl(state, &account_id, &folder, headers, raw, false).await
}

async fn delta_messages_user(
    State(state): State<AppState>,
    Path((user, folder)): Path<(String, String)>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    delta_messages_impl(state, &account_id, &folder, headers, raw, false).await
}

async fn list_message_attachments_user(
    State(state): State<AppState>,
    Path((user, message_id)): Path<(String, String)>,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    list_message_attachments_impl(state, &account_id, &message_id).await
}

async fn get_message_attachment_user(
    State(state): State<AppState>,
    Path((user, message_id, attachment_id)): Path<(String, String, String)>,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    get_message_attachment_impl(state, &account_id, &message_id, &attachment_id).await
}

async fn get_message_attachment_value_user(
    State(state): State<AppState>,
    Path((user, message_id, attachment_id)): Path<(String, String, String)>,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    get_message_attachment_value_impl(state, &account_id, &message_id, &attachment_id).await
}

// ── Inner handlers (account-scoped) ─────────────────────────────────

async fn list_folders_impl(
    state: AppState,
    account_id: &str,
    headers: HeaderMap,
    raw: Option<String>,
    me_path: bool,
) -> Response {
    if let Some(r) = super::maybe_override(&state, "list_folders", |_s| Ok(())) {
        return r;
    }
    let q = odata::OdataQuery::parse(raw.as_deref());
    let host = host_or_default(&headers);

    let fixture = state.fixture();
    let folders: Vec<&Mailbox> = fixture
        .mailboxes_for(account_id)
        .filter(|m| m.parent_id.is_none())
        .collect();

    let total = folders.len() as u64;
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
    let page: Vec<Value> = folders
        .iter()
        .skip(offset as usize)
        .take(top as usize)
        .map(|m| folder_value(&fixture, m))
        .collect();

    let base_path = if me_path {
        "/v1.0/me/mailFolders".to_string()
    } else {
        format!("/v1.0/users/{account_id}/mailFolders")
    };
    let next_link = next_offset(offset, top, total)
        .map(|next| odata::build_next_link(&host, &base_path, raw.as_deref(), next));
    let count = q.count.unwrap_or(false).then_some(total);

    ok_json(odata::collection_envelope(
        "https://graph.microsoft.com/v1.0/$metadata#me/mailFolders",
        page,
        next_link,
        None,
        count,
    ))
}

async fn get_folder_impl(
    state: AppState,
    account_id: &str,
    folder: &str,
    _me_path: bool,
) -> Response {
    let folder_owned = folder.to_string();
    if let Some(r) = super::maybe_override(&state, "get_folder", move |s| {
        crate::lua::req_set_str(s, "folder", &folder_owned)
    }) {
        return r;
    }
    let fixture = state.fixture();
    let Some(m) = resolve_folder(&fixture, folder, account_id) else {
        return error(
            StatusCode::NOT_FOUND,
            "ErrorItemNotFound",
            &format!("mailFolder {folder:?} not found"),
        );
    };
    let mut v = folder_value(&fixture, m);
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "@odata.context".to_string(),
            Value::String(
                "https://graph.microsoft.com/v1.0/$metadata#me/mailFolders/$entity".to_string(),
            ),
        );
    }
    ok_json(v)
}

async fn list_child_folders_impl(
    state: AppState,
    account_id: &str,
    folder: &str,
    headers: HeaderMap,
    raw: Option<String>,
    me_path: bool,
) -> Response {
    let folder_owned = folder.to_string();
    if let Some(r) = super::maybe_override(&state, "list_child_folders", move |s| {
        crate::lua::req_set_str(s, "folder", &folder_owned)
    }) {
        return r;
    }
    let fixture = state.fixture();
    let Some(parent) = resolve_folder(&fixture, folder, account_id) else {
        return error(
            StatusCode::NOT_FOUND,
            "ErrorItemNotFound",
            &format!("mailFolder {folder:?} not found"),
        );
    };
    let q = odata::OdataQuery::parse(raw.as_deref());
    let host = host_or_default(&headers);

    let children: Vec<&Mailbox> = fixture
        .mailboxes_for(account_id)
        .filter(|m| m.parent_id.as_deref() == Some(parent.id.as_str()))
        .collect();
    let total = children.len() as u64;
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
    let page: Vec<Value> = children
        .iter()
        .skip(offset as usize)
        .take(top as usize)
        .map(|m| folder_value(&fixture, m))
        .collect();

    let path = if me_path {
        format!("/v1.0/me/mailFolders/{}/childFolders", parent.id)
    } else {
        format!(
            "/v1.0/users/{account_id}/mailFolders/{}/childFolders",
            parent.id
        )
    };
    let next_link = next_offset(offset, top, total)
        .map(|next| odata::build_next_link(&host, &path, raw.as_deref(), next));

    ok_json(odata::collection_envelope(
        "https://graph.microsoft.com/v1.0/$metadata#me/mailFolders('...')/childFolders",
        page,
        next_link,
        None,
        q.count.unwrap_or(false).then_some(total),
    ))
}

async fn list_messages_impl(
    state: AppState,
    account_id: &str,
    folder: &str,
    headers: HeaderMap,
    raw: Option<String>,
    me_path: bool,
) -> Response {
    let folder_owned = folder.to_string();
    if let Some(r) = super::maybe_override(&state, "list_messages", move |s| {
        crate::lua::req_set_str(s, "folder", &folder_owned)
    }) {
        return r;
    }
    let fixture = state.fixture();
    let Some(m) = resolve_folder(&fixture, folder, account_id) else {
        return error(
            StatusCode::NOT_FOUND,
            "ErrorItemNotFound",
            &format!("mailFolder {folder:?} not found"),
        );
    };
    let q = odata::OdataQuery::parse(raw.as_deref());
    let host = host_or_default(&headers);
    let expand = expand_attachments(q.expand.as_deref());

    let mut messages = sorted_messages_in(&fixture, account_id, &m.id);
    if let Some(filter) = &q.filter
        && let Some(after) = parse_received_ge_filter(filter)
    {
        messages.retain(|e| e.received_at >= after);
    }
    let total = messages.len() as u64;
    let top = q.page_size(MESSAGES_DEFAULT_TOP, MESSAGES_MAX_TOP);
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
    let page: Vec<Value> = messages
        .iter()
        .skip(offset as usize)
        .take(top as usize)
        .map(|e| message_value(e, &m.id, expand))
        .collect();

    let path = if me_path {
        format!("/v1.0/me/mailFolders/{}/messages", m.id)
    } else {
        format!("/v1.0/users/{account_id}/mailFolders/{}/messages", m.id)
    };
    let next_link = next_offset(offset, top, total)
        .map(|next| odata::build_next_link(&host, &path, raw.as_deref(), next));

    ok_json(odata::collection_envelope(
        "https://graph.microsoft.com/v1.0/$metadata#message",
        page,
        next_link,
        None,
        q.count.unwrap_or(false).then_some(total),
    ))
}

async fn delta_messages_impl(
    state: AppState,
    account_id: &str,
    folder: &str,
    headers: HeaderMap,
    raw: Option<String>,
    me_path: bool,
) -> Response {
    let folder_owned = folder.to_string();
    if let Some(r) = super::maybe_override(&state, "delta_messages", move |s| {
        crate::lua::req_set_str(s, "folder", &folder_owned)
    }) {
        return r;
    }
    let fixture = state.fixture();
    let Some(m) = resolve_folder(&fixture, folder, account_id) else {
        return error(
            StatusCode::NOT_FOUND,
            "ErrorItemNotFound",
            &format!("mailFolder {folder:?} not found"),
        );
    };
    let q = odata::OdataQuery::parse(raw.as_deref());
    let host = host_or_default(&headers);
    let expand = expand_attachments(q.expand.as_deref());
    let path = if me_path {
        format!("/v1.0/me/mailFolders/{}/messages/delta", m.id)
    } else {
        format!(
            "/v1.0/users/{account_id}/mailFolders/{}/messages/delta",
            m.id
        )
    };

    if q.deltatoken.as_deref() == Some("latest") {
        return ok_json(odata::collection_envelope(
            "https://graph.microsoft.com/v1.0/$metadata#message",
            vec![],
            None,
            Some(odata::build_delta_link(&host, &path, raw.as_deref(), &fixture.state)),
            None,
        ));
    }

    if q.deltatoken.is_some() {
        return ok_json(odata::collection_envelope(
            "https://graph.microsoft.com/v1.0/$metadata#message",
            vec![],
            None,
            Some(odata::build_delta_link(&host, &path, raw.as_deref(), &fixture.state)),
            None,
        ));
    }

    let messages = sorted_messages_in(&fixture, account_id, &m.id);
    let total = messages.len() as u64;
    let top = q.page_size(MESSAGES_DEFAULT_TOP, MESSAGES_MAX_TOP);
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
    let page: Vec<Value> = messages
        .iter()
        .skip(offset as usize)
        .take(top as usize)
        .map(|e| message_value(e, &m.id, expand))
        .collect();

    let (next_link, delta_link) = match next_offset(offset, top, total) {
        Some(next) => (
            Some(odata::build_next_link(&host, &path, raw.as_deref(), next)),
            None,
        ),
        None => (
            None,
            Some(odata::build_delta_link(&host, &path, raw.as_deref(), &fixture.state)),
        ),
    };

    ok_json(odata::collection_envelope(
        "https://graph.microsoft.com/v1.0/$metadata#message",
        page,
        next_link,
        delta_link,
        None,
    ))
}

async fn list_message_attachments_impl(
    state: AppState,
    account_id: &str,
    message_id: &str,
) -> Response {
    let fixture = state.fixture();
    let Some(email) = fixture.emails_for(account_id).find(|e| e.id == message_id) else {
        return error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("message {message_id:?} not found"),
        );
    };
    let value: Vec<Value> = email
        .attachments
        .iter()
        .map(|a| graph_attachment_value(&email.id, a, true))
        .collect();
    ok_json(odata::collection_envelope(
        &format!("$metadata#users('me')/messages('{message_id}')/attachments"),
        value,
        None,
        None,
        None,
    ))
}

async fn get_message_attachment_impl(
    state: AppState,
    account_id: &str,
    message_id: &str,
    attachment_id: &str,
) -> Response {
    state
        .shared
        .latency
        .sleep_for_attachment(attachment_id)
        .await;
    let fixture = state.fixture();
    let Some((email, att)) = find_email_with_attachment(&fixture, account_id, message_id, attachment_id)
    else {
        return error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("attachment {attachment_id:?} on message {message_id:?} not found"),
        );
    };
    ok_json(graph_attachment_value(&email.id, att, true))
}

async fn get_message_attachment_value_impl(
    state: AppState,
    account_id: &str,
    message_id: &str,
    attachment_id: &str,
) -> Response {
    state
        .shared
        .latency
        .sleep_for_attachment(attachment_id)
        .await;
    let fixture = state.fixture();
    let Some((_, att)) = find_email_with_attachment(&fixture, account_id, message_id, attachment_id)
    else {
        return error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("attachment {attachment_id:?} on message {message_id:?} not found"),
        );
    };
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, att.content_type.clone())],
        AxumBody::from(att.data.clone()),
    )
        .into_response()
}

// ── Folder projection ───────────────────────────────────────────────

/// Resolve `folder` against the named account's mailbox set. Accepts
/// either the literal mailbox id or a well-known role alias
/// (`inbox`, `drafts`, ...). Scoped to one account so that a
/// shared-mailbox request that names "inbox" doesn't accidentally
/// resolve to the primary account's inbox.
fn resolve_folder<'a>(
    fixture: &'a Fixture,
    folder: &str,
    account_id: &'a str,
) -> Option<&'a Mailbox> {
    if let Some(m) = fixture.mailboxes_for(account_id).find(|m| m.id == folder) {
        return Some(m);
    }
    let role = role_from_alias(folder)?;
    fixture.mailboxes_for(account_id).find(|m| m.role == Some(role))
}

fn role_from_alias(alias: &str) -> Option<Role> {
    Some(match alias.to_ascii_lowercase().as_str() {
        "inbox" => Role::Inbox,
        "drafts" => Role::Drafts,
        "sentitems" => Role::Sent,
        "deleteditems" => Role::Trash,
        "junkemail" => Role::Junk,
        "archive" => Role::Archive,
        _ => return None,
    })
}

fn well_known_name_for(role: Option<Role>) -> Option<&'static str> {
    Some(match role? {
        Role::Inbox => "inbox",
        Role::Drafts => "drafts",
        Role::Sent => "sentItems",
        Role::Trash => "deletedItems",
        Role::Junk => "junkEmail",
        Role::Archive => "archive",
        Role::Important => return None,
    })
}

fn folder_value(fixture: &Fixture, m: &Mailbox) -> Value {
    let messages_in = fixture
        .emails_for(&m.account_id)
        .filter(|e| e.mailbox_ids.iter().any(|id| id == &m.id))
        .collect::<Vec<_>>();
    let total = messages_in.len() as u64;
    let unread = messages_in
        .iter()
        .filter(|e| !e.keywords.iter().any(|k| k == "$seen"))
        .count() as u64;
    let child_count = fixture
        .mailboxes_for(&m.account_id)
        .filter(|c| c.parent_id.as_deref() == Some(m.id.as_str()))
        .count() as u64;

    let mut obj = Map::new();
    obj.insert("id".to_string(), Value::String(m.id.clone()));
    obj.insert("displayName".to_string(), Value::String(m.name.clone()));
    obj.insert(
        "parentFolderId".to_string(),
        match &m.parent_id {
            Some(p) => Value::String(p.clone()),
            None => Value::Null,
        },
    );
    obj.insert("childFolderCount".to_string(), json!(child_count));
    obj.insert("unreadItemCount".to_string(), json!(unread));
    obj.insert("totalItemCount".to_string(), json!(total));
    obj.insert(
        "wellKnownName".to_string(),
        match well_known_name_for(m.role) {
            Some(name) => Value::String(name.to_string()),
            None => Value::Null,
        },
    );
    Value::Object(obj)
}

// ── Message projection ──────────────────────────────────────────────

fn sorted_messages_in<'a>(
    fixture: &'a Fixture,
    account_id: &'a str,
    mailbox_id: &str,
) -> Vec<&'a Email> {
    let mut v: Vec<&Email> = fixture
        .emails_for(account_id)
        .filter(|e| e.mailbox_ids.iter().any(|id| id == mailbox_id))
        .collect();
    // Same determinism contract as JMAP's Email/query: receivedAt
    // descending with id-lexicographic tiebreak.
    v.sort_by(|a, b| {
        b.received_at
            .cmp(&a.received_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    v
}

fn message_value(e: &Email, parent_folder_id: &str, expand_attachments: bool) -> Value {
    let mut obj = Map::new();
    obj.insert("id".to_string(), Value::String(e.id.clone()));
    obj.insert(
        "conversationId".to_string(),
        Value::String(e.thread_id.clone()),
    );
    obj.insert(
        "subject".to_string(),
        match &e.subject {
            Some(s) => Value::String(s.clone()),
            None => Value::Null,
        },
    );
    obj.insert(
        "bodyPreview".to_string(),
        match &e.preview {
            Some(s) => Value::String(s.clone()),
            None => Value::Null,
        },
    );
    obj.insert("body".to_string(), body_value(&e.body));
    obj.insert(
        "from".to_string(),
        match &e.from {
            Some(a) => recipient_value(a),
            None => Value::Null,
        },
    );
    obj.insert("toRecipients".to_string(), recipient_array(&e.to));
    obj.insert("ccRecipients".to_string(), recipient_array(&e.cc));
    obj.insert("bccRecipients".to_string(), recipient_array(&e.bcc));
    obj.insert("replyTo".to_string(), recipient_array(&e.reply_to));
    obj.insert(
        "receivedDateTime".to_string(),
        Value::String(e.received_at.to_rfc3339_opts(SecondsFormat::Secs, true)),
    );
    obj.insert(
        "sentDateTime".to_string(),
        Value::String(e.sent_at.to_rfc3339_opts(SecondsFormat::Secs, true)),
    );
    obj.insert(
        "isRead".to_string(),
        Value::Bool(e.keywords.iter().any(|k| k == "$seen")),
    );
    obj.insert(
        "isDraft".to_string(),
        Value::Bool(e.keywords.iter().any(|k| k == "$draft")),
    );
    obj.insert(
        "hasAttachments".to_string(),
        Value::Bool(e.has_attachment),
    );
    obj.insert("importance".to_string(), Value::String("normal".into()));
    obj.insert(
        "parentFolderId".to_string(),
        Value::String(parent_folder_id.to_string()),
    );
    obj.insert("categories".to_string(), categories_value(&e.keywords));
    obj.insert("flag".to_string(), flag_value(&e.keywords));
    obj.insert(
        "inferenceClassification".to_string(),
        Value::String("focused".into()),
    );
    obj.insert("isReadReceiptRequested".to_string(), Value::Bool(false));
    obj.insert(
        "internetMessageHeaders".to_string(),
        internet_message_headers(e),
    );
    obj.insert(
        "internetMessageId".to_string(),
        match e.message_id.first() {
            Some(id) => Value::String(id.clone()),
            None => Value::Null,
        },
    );
    let attachments_array: Vec<Value> = if expand_attachments {
        e.attachments
            .iter()
            .map(|a| graph_attachment_value(&e.id, a, true))
            .collect()
    } else {
        Vec::new()
    };
    obj.insert("attachments".to_string(), Value::Array(attachments_array));
    obj.insert(
        "singleValueExtendedProperties".to_string(),
        Value::Array(vec![]),
    );
    Value::Object(obj)
}

fn body_value(body: &Body) -> Value {
    match body {
        Body::Text(t) => json!({
            "contentType": "text",
            "content": t,
        }),
    }
}

fn recipient_value(a: &Address) -> Value {
    json!({
        "emailAddress": {
            "address": a.email,
            "name": a.name.clone().map(Value::String).unwrap_or(Value::Null),
        }
    })
}

fn recipient_array(xs: &[Address]) -> Value {
    Value::Array(xs.iter().map(recipient_value).collect())
}

fn categories_value(keywords: &[String]) -> Value {
    Value::Array(
        keywords
            .iter()
            .filter(|k| !k.starts_with('$'))
            .map(|k| Value::String(k.clone()))
            .collect(),
    )
}

fn flag_value(keywords: &[String]) -> Value {
    let status = if keywords.iter().any(|k| k == "$flagged") {
        "flagged"
    } else {
        "notFlagged"
    };
    json!({"flagStatus": status})
}

fn internet_message_headers(e: &Email) -> Value {
    let mut headers = Vec::new();
    for id in &e.message_id {
        headers.push(json!({"name": "Message-ID", "value": id}));
    }
    if !e.in_reply_to.is_empty() {
        headers.push(json!({
            "name": "In-Reply-To",
            "value": e.in_reply_to.join(" "),
        }));
    }
    if !e.references.is_empty() {
        headers.push(json!({
            "name": "References",
            "value": e.references.join(" "),
        }));
    }
    Value::Array(headers)
}

// ── Helpers ─────────────────────────────────────────────────────────

fn host_or_default(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("127.0.0.1")
        .to_string()
}

fn next_offset(offset: u32, top: u32, total: u64) -> Option<u32> {
    let next = u64::from(offset) + u64::from(top);
    if next < total {
        u32::try_from(next).ok()
    } else {
        None
    }
}

/// True when the OData `$expand` clause asks for the `attachments`
/// navigation property. Real Graph supports `$expand=attachments` and
/// `$expand=attachments($select=...)`; we just look for the prefix.
fn expand_attachments(expand: Option<&str>) -> bool {
    expand
        .map(|s| s.split(',').any(|p| p.trim().starts_with("attachments")))
        .unwrap_or(false)
}

fn graph_attachment_value(message_id: &str, a: &Attachment, include_bytes: bool) -> Value {
    let mut obj = Map::new();
    obj.insert(
        "@odata.type".to_string(),
        Value::String("#microsoft.graph.fileAttachment".to_string()),
    );
    obj.insert("@odata.mediaContentType".to_string(), Value::String(a.content_type.clone()));
    obj.insert("id".to_string(), Value::String(a.blob_id.clone()));
    obj.insert("name".to_string(), Value::String(a.name.clone()));
    obj.insert("contentType".to_string(), Value::String(a.content_type.clone()));
    obj.insert("size".to_string(), Value::Number(a.size.into()));
    obj.insert(
        "isInline".to_string(),
        Value::Bool(matches!(a.disposition, Disposition::Inline)),
    );
    if let Some(cid) = &a.cid {
        obj.insert("contentId".to_string(), Value::String(cid.clone()));
    }
    obj.insert(
        "lastModifiedDateTime".to_string(),
        Value::String("1970-01-01T00:00:00Z".to_string()),
    );
    obj.insert(
        "@odata.id".to_string(),
        Value::String(format!(
            "/v1.0/me/messages/{message_id}/attachments/{}",
            a.blob_id
        )),
    );
    if include_bytes {
        obj.insert(
            "contentBytes".to_string(),
            Value::String(base64_standard(&a.data)),
        );
    }
    Value::Object(obj)
}

fn find_email_with_attachment<'a>(
    fixture: &'a Fixture,
    account_id: &'a str,
    message_id: &str,
    attachment_id: &str,
) -> Option<(&'a Email, &'a Attachment)> {
    let email = fixture.emails_for(account_id).find(|e| e.id == message_id)?;
    let att = email.attachments.iter().find(|a| a.blob_id == attachment_id)?;
    Some((email, att))
}

/// Standard (`+`/`/`) base64 with padding. Avoids pulling in the
/// `base64` crate for one call site.
fn base64_standard(input: &[u8]) -> String {
    const ALPHA: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut chunks = input.chunks_exact(3);
    for c in chunks.by_ref() {
        let n = (u32::from(c[0]) << 16) | (u32::from(c[1]) << 8) | u32::from(c[2]);
        out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
        out.push(ALPHA[(n & 0x3f) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        1 => {
            let n = u32::from(rem[0]) << 16;
            out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = (u32::from(rem[0]) << 16) | (u32::from(rem[1]) << 8);
            out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
            out.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

/// Parse the only `$filter` shape ratatoskr emits during initial mail
/// sync: `receivedDateTime ge <iso8601>`.
fn parse_received_ge_filter(filter: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let s = filter.trim();
    let rest = s.strip_prefix("receivedDateTime")?.trim_start();
    let rest = rest.strip_prefix("ge")?.trim_start();
    chrono::DateTime::parse_from_rfc3339(rest.trim())
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}
