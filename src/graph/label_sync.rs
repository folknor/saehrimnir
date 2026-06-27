//! Microsoft Graph master-category endpoints.
//!
//! Outlook's master category list is the Graph analogue of Gmail
//! labels / JMAP keywords. Flat per account (no folder scope), and
//! exposed under `/v1.0/me/outlook/masterCategories` (primary
//! account) and `/v1.0/users/{userId}/outlook/masterCategories`
//! (any declared account). v0 covers GET list/single + POST /
//! PATCH / DELETE. `userId = me` aliases the primary; unknown
//! userIds return 404 `ResourceNotFound`.
//!
//! Mutations land via `Fixture::mutate` and record
//! `category_created` / `category_updated` / `category_destroyed`
//! transitions. Real Graph has no `masterCategories/delta`
//! endpoint, so v0 also doesn't expose one - the change_log
//! entries are observability for tests asserting state moved.

use axum::{
    Router,
    body::Body as AxumBody,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Map, Value, json};

use super::{AppState, error, ok_json};
use crate::fixture::{Category, MutationDiff};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/v1.0/me/outlook/masterCategories",
            get(list_categories_me).post(create_category_me),
        )
        .route(
            "/v1.0/me/outlook/masterCategories/{category}",
            get(get_category_me)
                .patch(patch_category_me)
                .delete(delete_category_me),
        )
        .route(
            "/v1.0/users/{user}/outlook/masterCategories",
            get(list_categories_user).post(create_category_user),
        )
        .route(
            "/v1.0/users/{user}/outlook/masterCategories/{category}",
            get(get_category_user)
                .patch(patch_category_user)
                .delete(delete_category_user),
        )
}

// ── /me/ wrappers ───────────────────────────────────────────────────

async fn list_categories_me(State(state): State<AppState>) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    list_categories_impl(state, &account_id).await
}

async fn get_category_me(State(state): State<AppState>, Path(category): Path<String>) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    get_category_impl(state, &account_id, &category).await
}

async fn create_category_me(
    State(state): State<AppState>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
    body: AxumBody,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    create_category_impl(state, &account_id, body, connection_id).await
}

async fn patch_category_me(
    State(state): State<AppState>,
    Path(category): Path<String>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
    body: AxumBody,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    patch_category_impl(state, &account_id, &category, body, connection_id).await
}

async fn delete_category_me(
    State(state): State<AppState>,
    Path(category): Path<String>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    delete_category_impl(state, &account_id, &category, connection_id).await
}

// ── /users/{user}/ wrappers ─────────────────────────────────────────

async fn list_categories_user(State(state): State<AppState>, Path(user): Path<String>) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    list_categories_impl(state, &account_id).await
}

async fn get_category_user(
    State(state): State<AppState>,
    Path((user, category)): Path<(String, String)>,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    get_category_impl(state, &account_id, &category).await
}

async fn create_category_user(
    State(state): State<AppState>,
    Path(user): Path<String>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
    body: AxumBody,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    create_category_impl(state, &account_id, body, connection_id).await
}

async fn patch_category_user(
    State(state): State<AppState>,
    Path((user, category)): Path<(String, String)>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
    body: AxumBody,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    patch_category_impl(state, &account_id, &category, body, connection_id).await
}

async fn delete_category_user(
    State(state): State<AppState>,
    Path((user, category)): Path<(String, String)>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    delete_category_impl(state, &account_id, &category, connection_id).await
}

// ── Inner handlers (account-scoped) ─────────────────────────────────

async fn list_categories_impl(state: AppState, account_id: &str) -> Response {
    if let Some(o) = super::maybe_override(&state, "list_categories", |_| Ok(())) {
        return o;
    }
    let fixture = state.fixture();
    let value: Vec<Value> = fixture
        .categories_for(account_id)
        .map(serialize_category)
        .collect();
    ok_json(json!({
        "@odata.context": "https://graph.microsoft.com/v1.0/$metadata#me/outlook/masterCategories",
        "value": value,
    }))
}

async fn get_category_impl(state: AppState, account_id: &str, category: &str) -> Response {
    let category_owned = category.to_string();
    if let Some(o) = super::maybe_override(&state, "get_category", move |s| {
        crate::lua::req_set_str(s, "category", &category_owned)
    }) {
        return o;
    }
    let fixture = state.fixture();
    match fixture
        .categories_for(account_id)
        .find(|c| c.id == category)
    {
        Some(c) => ok_json(serialize_category(c)),
        None => not_found(category),
    }
}

async fn create_category_impl(
    state: AppState,
    account_id: &str,
    body: AxumBody,
    connection_id: Option<u64>,
) -> Response {
    let parsed = match parse_json_body(body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    state.shared.request_log.record_with_conn(
        "graph",
        "POST /v1.0/me/outlook/masterCategories".to_string(),
        crate::request_log::body_detail(&parsed),
        connection_id,
    );
    let obj = match parsed.as_object() {
        Some(o) => o,
        None => {
            return error(
                StatusCode::BAD_REQUEST,
                "BadRequest",
                "create body must be a JSON object",
            );
        }
    };
    let display_name = obj
        .get("displayName")
        .and_then(Value::as_str)
        .map(str::to_string);
    let Some(display_name) = display_name else {
        return error(
            StatusCode::BAD_REQUEST,
            "BadRequest",
            "displayName is required",
        );
    };
    let color = obj.get("color").and_then(Value::as_str).map(str::to_string);
    let client_id = obj.get("id").and_then(Value::as_str).map(str::to_string);

    let result: Result<Value, Response> = {
        let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
        let id = match client_id {
            Some(id) => {
                if fix.categories_for(account_id).any(|c| c.id == id) {
                    return error(
                        StatusCode::CONFLICT,
                        "Conflict",
                        &format!("category {id:?} already exists"),
                    );
                }
                id
            }
            None => fix.mint_category_id(),
        };
        let cat = Category {
            id: id.clone(),
            account_id: account_id.to_string(),
            display_name,
            color,
        };
        let view = cat.clone();
        let _ = fix.mutate(|f| {
            f.categories.push(cat);
            MutationDiff {
                category_created: vec![id.clone()],
                ..Default::default()
            }
        });
        Ok(serialize_category(&view))
    };
    match result {
        Ok(v) => (StatusCode::CREATED, axum::Json(v)).into_response(),
        Err(resp) => resp,
    }
}

async fn patch_category_impl(
    state: AppState,
    account_id: &str,
    category: &str,
    body: AxumBody,
    connection_id: Option<u64>,
) -> Response {
    let known = {
        let fixture = state.fixture();
        fixture.categories_for(account_id).any(|c| c.id == category)
    };
    if !known {
        return not_found(category);
    }
    let parsed = match parse_json_body(body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    state.shared.request_log.record_with_conn(
        "graph",
        format!("PATCH /v1.0/me/outlook/masterCategories/{category}"),
        crate::request_log::body_detail(&parsed),
        connection_id,
    );
    let obj = match parsed.as_object() {
        Some(o) => o,
        None => {
            return error(
                StatusCode::BAD_REQUEST,
                "BadRequest",
                "patch body must be a JSON object",
            );
        }
    };

    let result: Result<Value, Response> = {
        let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
        let idx = match fix
            .categories
            .iter()
            .position(|c| c.account_id == account_id && c.id == category)
        {
            Some(i) => i,
            None => return not_found(category),
        };
        let mut clone = fix.categories[idx].clone();
        for (k, v) in obj {
            match k.as_str() {
                "displayName" => {
                    if let Some(s) = v.as_str() {
                        clone.display_name = s.to_string();
                    }
                }
                "color" => {
                    clone.color = v.as_str().map(str::to_string);
                }
                _ => {
                    // Quietly ignore unknown fields - real Graph
                    // accepts (and discards) `id` echoes in PATCH
                    // bodies; mirroring that keeps clients that
                    // do round-trip the full object working.
                }
            }
        }
        let id = clone.id.clone();
        let view = clone.clone();
        let _ = fix.mutate(|f| {
            f.categories[idx] = clone;
            MutationDiff {
                category_updated: vec![id.clone()],
                ..Default::default()
            }
        });
        Ok(serialize_category(&view))
    };
    match result {
        Ok(v) => ok_json(v),
        Err(resp) => resp,
    }
}

async fn delete_category_impl(
    state: AppState,
    account_id: &str,
    category: &str,
    connection_id: Option<u64>,
) -> Response {
    let known = {
        let fixture = state.fixture();
        fixture.categories_for(account_id).any(|c| c.id == category)
    };
    if !known {
        return not_found(category);
    }
    state.shared.request_log.record_with_conn(
        "graph",
        format!("DELETE /v1.0/me/outlook/masterCategories/{category}"),
        json!({ "id": category }),
        connection_id,
    );
    {
        let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
        let id = category.to_string();
        let acct = account_id.to_string();
        let _ = fix.mutate(|f| {
            let before = f.categories.len();
            f.categories
                .retain(|c| !(c.account_id == acct && c.id == id));
            if f.categories.len() < before {
                MutationDiff {
                    category_destroyed: vec![id.clone()],
                    category_destroyed_accounts: vec![acct.clone()],
                    ..Default::default()
                }
            } else {
                MutationDiff::default()
            }
        });
    }
    StatusCode::NO_CONTENT.into_response()
}

// ── Helpers ─────────────────────────────────────────────────────────

fn serialize_category(c: &Category) -> Value {
    let mut obj = Map::new();
    obj.insert("id".to_string(), Value::String(c.id.clone()));
    obj.insert(
        "displayName".to_string(),
        Value::String(c.display_name.clone()),
    );
    if let Some(color) = &c.color {
        obj.insert("color".to_string(), Value::String(color.clone()));
    }
    Value::Object(obj)
}

fn not_found(id: &str) -> Response {
    error(
        StatusCode::NOT_FOUND,
        "ResourceNotFound",
        &format!("category {id:?} not declared in fixture"),
    )
}

// Err is an axum `Response` (large); allow rather than box (every `?`
// caller would otherwise have to unbox).
#[allow(clippy::result_large_err)]
async fn parse_json_body(body: AxumBody) -> Result<Value, Response> {
    let bytes = match axum::body::to_bytes(body, 1_048_576).await {
        Ok(b) => b,
        Err(e) => {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "BadRequest",
                &format!("failed to read body: {e}"),
            ));
        }
    };
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&bytes).map_err(|e| {
        error(
            StatusCode::BAD_REQUEST,
            "InvalidRequest",
            &format!("body is not JSON: {e}"),
        )
    })
}
