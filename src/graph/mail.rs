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
    Json, Router,
    extract::{Path, RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{get, post},
};
use chrono::SecondsFormat;
use serde_json::{Map, Value, json};

use axum::body::Body as AxumBody;
use axum::http::header;
use axum::response::IntoResponse;

use super::{AppState, error, odata, ok_json};
use crate::fixture::{Address, Attachment, Body, Disposition, Email, Fixture, Mailbox, MutationDiff, Role};

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
        .route(
            "/v1.0/me/messages/{message_id}/$value",
            get(get_message_value_me),
        )
        .route(
            "/v1.0/me/messages/{message_id}",
            get(get_message_me)
                .patch(patch_message_me)
                .delete(delete_message_me),
        )
        .route(
            "/v1.0/me/messages/{message_id}/move",
            post(move_message_me),
        )
        .route("/v1.0/me/messages", get(list_messages_collection_me))
        // JSON batching. bifrost hydrates message metadata by batching
        // per-id GET /me/messages/{id} sub-requests through here.
        .route("/v1.0/$batch", post(batch))
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
        .route(
            "/v1.0/users/{user}/messages/{message_id}/$value",
            get(get_message_value_user),
        )
        .route(
            "/v1.0/users/{user}/messages/{message_id}",
            get(get_message_user)
                .patch(patch_message_user)
                .delete(delete_message_user),
        )
        .route(
            "/v1.0/users/{user}/messages/{message_id}/move",
            post(move_message_user),
        )
        .route(
            "/v1.0/users/{user}/messages",
            get(list_messages_collection_user),
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

async fn get_message_me(
    State(state): State<AppState>,
    Path(message_id): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    get_message_impl(state, &account_id, &message_id, raw).await
}

async fn get_message_value_me(
    State(state): State<AppState>,
    Path(message_id): Path<String>,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    get_message_value_impl(state, &account_id, &message_id).await
}

async fn patch_message_me(
    State(state): State<AppState>,
    Path(message_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    patch_message_impl(state, &account_id, &message_id, body).await
}

async fn delete_message_me(
    State(state): State<AppState>,
    Path(message_id): Path<String>,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    delete_message_impl(state, &account_id, &message_id).await
}

async fn move_message_me(
    State(state): State<AppState>,
    Path(message_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    move_message_impl(state, &account_id, &message_id, body).await
}

async fn list_messages_collection_me(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    list_messages_collection_impl(state, &account_id, headers, raw, true).await
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

async fn get_message_user(
    State(state): State<AppState>,
    Path((user, message_id)): Path<(String, String)>,
    RawQuery(raw): RawQuery,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    get_message_impl(state, &account_id, &message_id, raw).await
}

async fn get_message_value_user(
    State(state): State<AppState>,
    Path((user, message_id)): Path<(String, String)>,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    get_message_value_impl(state, &account_id, &message_id).await
}

async fn patch_message_user(
    State(state): State<AppState>,
    Path((user, message_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    patch_message_impl(state, &account_id, &message_id, body).await
}

async fn delete_message_user(
    State(state): State<AppState>,
    Path((user, message_id)): Path<(String, String)>,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    delete_message_impl(state, &account_id, &message_id).await
}

async fn move_message_user(
    State(state): State<AppState>,
    Path((user, message_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    move_message_impl(state, &account_id, &message_id, body).await
}

async fn list_messages_collection_user(
    State(state): State<AppState>,
    Path(user): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    list_messages_collection_impl(state, &account_id, headers, raw, false).await
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

/// `GET /v1.0/me/messages` (account-wide, not folder-scoped). bifrost
/// fetches a whole conversation here via `$filter=conversationId eq
/// '<thread>'` (`pim.rs::message_values_for_thread`) and runs message
/// search via `$search`. v0 honours the `conversationId` filter
/// (mapped to the email's `thread_id`); other filters / `$search`
/// fall through to the full account list. Paginates with `$top` /
/// `$skiptoken` like the folder-scoped listing.
async fn list_messages_collection_impl(
    state: AppState,
    account_id: &str,
    headers: HeaderMap,
    raw: Option<String>,
    me_path: bool,
) -> Response {
    if let Some(r) = super::maybe_override(&state, "list_messages", |_| Ok(())) {
        return r;
    }
    let fixture = state.fixture();
    let q = odata::OdataQuery::parse(raw.as_deref());
    let host = host_or_default(&headers);
    let expand = expand_attachments(q.expand.as_deref());

    let mut messages: Vec<&Email> = fixture.emails_for(account_id).collect();
    if let Some(filter) = &q.filter
        && let Some(conv) = parse_conversation_id_filter(filter)
    {
        messages.retain(|e| e.thread_id == conv);
    }
    messages.sort_by(|a, b| {
        b.received_at
            .cmp(&a.received_at)
            .then_with(|| a.id.cmp(&b.id))
    });

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
        .map(|e| {
            let parent = e.mailbox_ids.first().map(String::as_str).unwrap_or("");
            message_value(e, parent, expand)
        })
        .collect();

    let path = if me_path {
        "/v1.0/me/messages".to_string()
    } else {
        format!("/v1.0/users/{account_id}/messages")
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

/// Extract the thread id from a `conversationId eq '<id>'` `$filter`.
/// Returns `None` for any other filter shape (the caller then lists
/// the full account set).
fn parse_conversation_id_filter(filter: &str) -> Option<String> {
    let rest = filter.trim().strip_prefix("conversationId eq ")?.trim();
    let inner = rest.strip_prefix('\'')?.strip_suffix('\'')?;
    Some(inner.to_string())
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

/// `GET /v1.0/me/messages/{id}` (and the `/users/{user}` twin).
/// bifrost reads single messages here both directly and - via the
/// `$batch` envelope - for metadata hydration of every id surfaced by
/// `messages/delta`. Without it the bare `/me/messages/{id}` path
/// fell to the catchall 404 and hydration could not complete.
async fn get_message_impl(
    state: AppState,
    account_id: &str,
    message_id: &str,
    raw: Option<String>,
) -> Response {
    let message_owned = message_id.to_string();
    if let Some(r) = super::maybe_override(&state, "get_message", move |s| {
        crate::lua::req_set_str(s, "message_id", &message_owned)
    }) {
        return r;
    }
    let fixture = state.fixture();
    let q = odata::OdataQuery::parse(raw.as_deref());
    let expand = expand_attachments(q.expand.as_deref());
    match message_get_value(&fixture, account_id, message_id, expand) {
        Some(mut v) => {
            if let Value::Object(ref mut m) = v {
                m.insert(
                    "@odata.context".to_string(),
                    Value::String(
                        "https://graph.microsoft.com/v1.0/$metadata#message/$entity".into(),
                    ),
                );
            }
            ok_json(v)
        }
        None => error(
            StatusCode::NOT_FOUND,
            "ErrorItemNotFound",
            &format!("message {message_id:?} not found"),
        ),
    }
}

/// `GET /v1.0/me/messages/{id}/$value` - the assembled RFC 822
/// message bytes. bifrost's `open_raw_rfc822` (blob.rs) defers real
/// body bytes to this endpoint after metadata hydration. Reuses the
/// IMAP module's `assembled_rfc822` so the Graph and IMAP body
/// surfaces agree byte-for-byte (multipart/mixed when the email
/// carries attachments).
async fn get_message_value_impl(state: AppState, account_id: &str, message_id: &str) -> Response {
    let message_owned = message_id.to_string();
    if let Some(r) = super::maybe_override(&state, "get_message_value", move |s| {
        crate::lua::req_set_str(s, "message_id", &message_owned)
    }) {
        return r;
    }
    let fixture = state.fixture();
    let Some(e) = fixture.emails_for(account_id).find(|e| e.id == message_id) else {
        return error(
            StatusCode::NOT_FOUND,
            "ErrorItemNotFound",
            &format!("message {message_id:?} not found"),
        );
    };
    let bytes = crate::imap::assembled_rfc822(e);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain".to_string())],
        AxumBody::from(bytes),
    )
        .into_response()
}

/// `PATCH /v1.0/me/messages/{id}` - the message-flag writeback
/// bifrost's mutation pipeline drives (mark read, flag/star,
/// categorize, set importance), both directly and as `$batch`
/// sub-requests. Maps the Graph fields onto fixture keywords:
/// `isRead` <-> `$seen`, `flag.flagStatus: flagged` <-> `$flagged`,
/// `categories[]` <-> the email's non-`$` (user) keywords.
/// `importance` is accepted but not durably stored (the fixture
/// `Email` has no importance field; reads always project "normal").
/// Records an `email_updated` transition so the change surfaces in
/// the next `messages/delta`. `If-Match` is not enforced in v0.
async fn patch_message_impl(
    state: AppState,
    account_id: &str,
    message_id: &str,
    body: Value,
) -> Response {
    let message_owned = message_id.to_string();
    if let Some(o) = super::maybe_override(&state, "patch_message", move |s| {
        crate::lua::req_set_str(s, "message_id", &message_owned)
    }) {
        return o;
    }
    let projected = {
        let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
        patch_message_core(&mut fix, account_id, message_id, &body)
    };
    match projected {
        Some(v) => ok_json(v),
        None => error(
            StatusCode::NOT_FOUND,
            "ErrorItemNotFound",
            &format!("message {message_id:?} not found"),
        ),
    }
}

// ── Message mutation cores ──────────────────────────────────────────
//
// Sync helpers operating on `&mut Fixture`, shared by the direct
// PATCH/DELETE/move handlers and the `$batch` write path (bifrost
// routes message writes through `$batch`, so the batch path is the
// one it actually exercises). Each runs the mutation under
// `Fixture::mutate` to record the change-log transition. Returning
// the projection (or a found flag) lets the caller shape the wire
// response (direct HTTP vs batch sub-response).

/// Apply a Graph message PATCH; returns the projected message, or
/// `None` when the id is not in the account.
fn patch_message_core(
    fix: &mut Fixture,
    account_id: &str,
    message_id: &str,
    body: &Value,
) -> Option<Value> {
    let acct = account_id.to_string();
    let mid = message_id.to_string();
    let mut projected = None;
    let _ = fix.mutate(|f| {
        let Some(idx) = f
            .emails
            .iter()
            .position(|e| e.account_id == acct && e.id == mid)
        else {
            return MutationDiff::default();
        };
        let mut email = f.emails[idx].clone();
        apply_graph_message_patch(&mut email, body);
        let parent = email.mailbox_ids.first().cloned().unwrap_or_default();
        projected = Some(message_value(&email, &parent, false));
        f.emails[idx] = email;
        MutationDiff {
            email_updated: vec![mid.clone()],
            ..Default::default()
        }
    });
    projected
}

/// Permanently delete a message; returns `false` when not found.
/// Retires the message's UID slots and records `email_destroyed`.
fn delete_message_core(fix: &mut Fixture, account_id: &str, message_id: &str) -> bool {
    let acct = account_id.to_string();
    let mid = message_id.to_string();
    let Some((mailboxes, owner)) = fix
        .emails
        .iter()
        .find(|e| e.account_id == acct && e.id == mid)
        .map(|e| (e.mailbox_ids.clone(), e.account_id.clone()))
    else {
        return false;
    };
    let _ = fix.mutate(move |f| {
        f.emails.retain(|e| !(e.account_id == acct && e.id == mid));
        for mb in &mailboxes {
            f.retire_uid(mb, &mid);
        }
        MutationDiff {
            email_destroyed: vec![mid.clone()],
            email_destroyed_accounts: vec![owner.clone()],
            ..Default::default()
        }
    });
    true
}

/// Move a message to `dest` (single-folder membership replace);
/// returns the projected moved message, or `None` when not found.
fn move_message_core(
    fix: &mut Fixture,
    account_id: &str,
    message_id: &str,
    dest: &str,
) -> Option<Value> {
    let acct = account_id.to_string();
    let mid = message_id.to_string();
    let dest = dest.to_string();
    let mut projected = None;
    let _ = fix.mutate(|f| {
        let Some(idx) = f
            .emails
            .iter()
            .position(|e| e.account_id == acct && e.id == mid)
        else {
            return MutationDiff::default();
        };
        let old = f.emails[idx].mailbox_ids.clone();
        let new = vec![dest.clone()];
        f.emails[idx].mailbox_ids = new.clone();
        f.sync_mailbox_uids(&mid, &old, &new);
        projected = Some(message_value(&f.emails[idx], &dest, false));
        MutationDiff {
            email_updated: vec![mid.clone()],
            ..Default::default()
        }
    });
    projected
}

/// Apply the Graph message-PATCH fields bifrost sends to a fixture
/// `Email`'s keyword set. See `patch_message_impl` for the mapping.
fn apply_graph_message_patch(email: &mut Email, body: &Value) {
    if let Some(read) = body.get("isRead").and_then(Value::as_bool) {
        set_keyword(&mut email.keywords, "$seen", read);
    }
    if let Some(status) = body
        .get("flag")
        .and_then(|f| f.get("flagStatus"))
        .and_then(Value::as_str)
    {
        set_keyword(
            &mut email.keywords,
            "$flagged",
            status.eq_ignore_ascii_case("flagged"),
        );
    }
    if let Some(cats) = body.get("categories").and_then(Value::as_array) {
        // categories replace the user (non-system) keyword set; the
        // `$`-prefixed system flags ($seen / $flagged / ...) survive.
        email.keywords.retain(|k| k.starts_with('$'));
        for c in cats {
            if let Some(s) = c.as_str()
                && !email.keywords.iter().any(|k| k == s)
            {
                email.keywords.push(s.to_string());
            }
        }
    }
    // `importance` has no fixture slot; accepted and ignored.
}

/// Add or remove a single keyword, idempotently.
fn set_keyword(keywords: &mut Vec<String>, kw: &str, present: bool) {
    let has = keywords.iter().any(|k| k == kw);
    if present && !has {
        keywords.push(kw.to_string());
    } else if !present && has {
        keywords.retain(|k| k != kw);
    }
}

/// `DELETE /v1.0/me/messages/{id}` - permanent delete. Mirrors the
/// JMAP `Email/set` destroy path: retires the message's UID slot in
/// every mailbox it belonged to (so IMAP UIDs stay stable) and
/// records an `email_destroyed` transition with the owning account,
/// so the next `messages/delta` emits the tombstone and a
/// multi-account fixture's per-account delta filters correctly. 204
/// on success, 404 `ErrorItemNotFound` on unknown id.
async fn delete_message_impl(state: AppState, account_id: &str, message_id: &str) -> Response {
    let message_owned = message_id.to_string();
    if let Some(o) = super::maybe_override(&state, "delete_message", move |s| {
        crate::lua::req_set_str(s, "message_id", &message_owned)
    }) {
        return o;
    }
    let found = {
        let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
        delete_message_core(&mut fix, account_id, message_id)
    };
    if found {
        StatusCode::NO_CONTENT.into_response()
    } else {
        error(
            StatusCode::NOT_FOUND,
            "ErrorItemNotFound",
            &format!("message {message_id:?} not found"),
        )
    }
}

/// `POST /v1.0/me/messages/{id}/move` - moves the message to the
/// folder named by the body's `destinationId`. The fixture models a
/// single-folder membership, so the move replaces `mailbox_ids` with
/// `[destinationId]`; `sync_mailbox_uids` assigns a UID in the
/// destination and retires the old slots (RFC 3501 UID stability).
/// Records an `email_updated` transition. Returns 201 with the moved
/// message (Graph mints a new id on move; v0 keeps the id stable,
/// which the fixture's deterministic-id contract relies on). 404 on
/// unknown message, 400 on a missing/blank `destinationId`.
async fn move_message_impl(
    state: AppState,
    account_id: &str,
    message_id: &str,
    body: Value,
) -> Response {
    let message_owned = message_id.to_string();
    if let Some(o) = super::maybe_override(&state, "move_message", move |s| {
        crate::lua::req_set_str(s, "message_id", &message_owned)
    }) {
        return o;
    }
    let Some(dest) = body
        .get("destinationId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return error(
            StatusCode::BAD_REQUEST,
            "ErrorInvalidRequest",
            "move requires a non-empty destinationId",
        );
    };
    let projected = {
        let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
        move_message_core(&mut fix, account_id, message_id, dest)
    };
    match projected {
        Some(v) => (StatusCode::CREATED, axum::Json(v)).into_response(),
        None => error(
            StatusCode::NOT_FOUND,
            "ErrorItemNotFound",
            &format!("message {message_id:?} not found"),
        ),
    }
}

/// Find a message by id within the account and project it via
/// `message_value`. Shared by the single-message GET route and the
/// `$batch` hydration path. `parentFolderId` is the message's first
/// mailbox membership. Returns `None` when the id is not in the
/// account.
fn message_get_value(
    fixture: &Fixture,
    account_id: &str,
    message_id: &str,
    expand: bool,
) -> Option<Value> {
    let e = fixture.emails_for(account_id).find(|e| e.id == message_id)?;
    let parent = e.mailbox_ids.first().map(String::as_str).unwrap_or("");
    Some(message_value(e, parent, expand))
}

/// `POST /v1.0/$batch` (Microsoft Graph JSON batching). bifrost
/// hydrates message metadata by batching per-id `GET /me/messages/{id}`
/// sub-requests through here (`client.rs::post_batch`,
/// `account/get.rs`); without it the hydration pass 404'd at the
/// catchall. The whole batch is read-only in v0: GET message
/// sub-requests are serviced; any other sub-request gets a per-item
/// error (bifrost surfaces that as one failed sub-request, not a
/// batch-level failure), so write batches (`pim.rs`) degrade
/// gracefully rather than corrupting state.
async fn batch(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let requests = body
        .get("requests")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // One write guard for the whole batch: sub-requests are a mix of
    // reads (hydration GETs) and writes (bifrost routes its message
    // mutations through `$batch`), all processed synchronously against
    // the same `&mut Fixture` - no await between them, so the guard is
    // never held across a suspension point.
    let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
    let responses: Vec<Value> = requests
        .iter()
        .map(|r| batch_sub_response(&mut fix, r))
        .collect();
    drop(fix);
    ok_json(json!({ "responses": responses }))
}

fn batch_sub_response(fix: &mut Fixture, req: &Value) -> Value {
    let id = req.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("GET")
        .to_ascii_uppercase();
    let url = req.get("url").and_then(Value::as_str).unwrap_or("");
    let (status, body) = dispatch_batch_request(fix, &method, url, req.get("body"));
    json!({
        "id": id,
        "status": status,
        "headers": { "Content-Type": "application/json" },
        "body": body,
    })
}

/// Route one `$batch` sub-request. v0 services message GET (the
/// hydration path) and the message writes bifrost batches - PATCH,
/// DELETE, and POST `.../move`. Sub-request URLs are relative
/// (`/me/...` or `/users/{u}/...`) and may or may not carry the
/// `/v1.0` prefix.
fn dispatch_batch_request(
    fix: &mut Fixture,
    method: &str,
    url: &str,
    body: Option<&Value>,
) -> (u16, Value) {
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (url, None),
    };
    let path = path.strip_prefix("/v1.0").unwrap_or(path);

    // Resolve the (account, "{id}" or "{id}/move") tail from the path.
    let (account_id, tail) = if let Some(tail) = path.strip_prefix("/me/messages/") {
        (fix.primary_account().id.clone(), tail)
    } else if let Some(rest) = path.strip_prefix("/users/")
        && let Some((user, tail)) = rest.split_once("/messages/")
    {
        let account_id = if user == "me" {
            fix.primary_account().id.clone()
        } else {
            match fix.account(user) {
                Some(a) => a.id.clone(),
                None => {
                    return (
                        404,
                        batch_error("ResourceNotFound", &format!("user {user:?} not found")),
                    );
                }
            }
        };
        (account_id, tail)
    } else {
        return (
            404,
            batch_error(
                "ResourceNotImplemented",
                &format!("v0 $batch does not implement {method} {url}"),
            ),
        );
    };

    // The tail is `{id}` or `{id}/move`.
    let (message_id, suffix) = match tail.split_once('/') {
        Some((id, s)) => (id, Some(s)),
        None => (tail, None),
    };
    if message_id.is_empty() {
        return (
            404,
            batch_error(
                "ResourceNotImplemented",
                &format!("v0 $batch does not implement {method} {url}"),
            ),
        );
    }

    let not_found = || {
        (
            404,
            batch_error("ErrorItemNotFound", &format!("message {message_id:?} not found")),
        )
    };

    match (method, suffix) {
        ("GET", None) => {
            let expand = expand_attachments(odata::OdataQuery::parse(query).expand.as_deref());
            match message_get_value(fix, &account_id, message_id, expand) {
                Some(v) => (200, v),
                None => not_found(),
            }
        }
        ("PATCH", None) => {
            let body = body.cloned().unwrap_or(Value::Null);
            match patch_message_core(fix, &account_id, message_id, &body) {
                Some(v) => (200, v),
                None => not_found(),
            }
        }
        ("DELETE", None) => {
            if delete_message_core(fix, &account_id, message_id) {
                (204, Value::Null)
            } else {
                not_found()
            }
        }
        ("POST", Some("move")) => {
            let dest = body
                .and_then(|b| b.get("destinationId"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            let Some(dest) = dest else {
                return (
                    400,
                    batch_error("ErrorInvalidRequest", "move requires a non-empty destinationId"),
                );
            };
            match move_message_core(fix, &account_id, message_id, dest) {
                Some(v) => (201, v),
                None => not_found(),
            }
        }
        _ => (
            501,
            batch_error(
                "ResourceNotImplemented",
                &format!("v0 $batch does not implement {method} {url}"),
            ),
        ),
    }
}

fn batch_error(code: &str, message: &str) -> Value {
    json!({ "error": { "code": code, "message": message } })
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
