//! Microsoft Graph master-category endpoints.
//!
//! Outlook's master category list is the Graph analogue of Gmail
//! labels / JMAP keywords. Flat per account (no folder scope), and
//! exposed under `/v1.0/me/outlook/masterCategories`. v0 covers:
//!
//! - `GET    /v1.0/me/outlook/masterCategories` (list).
//! - `GET    /v1.0/me/outlook/masterCategories/{id}` (single).
//! - `POST   /v1.0/me/outlook/masterCategories` (create).
//! - `PATCH  /v1.0/me/outlook/masterCategories/{id}` (update).
//! - `DELETE /v1.0/me/outlook/masterCategories/{id}` (delete).
//!
//! Mutations land via `Fixture::mutate` and record
//! `category_created` / `category_updated` / `category_destroyed`
//! transitions. Real Graph has no `masterCategories/delta` endpoint,
//! so v0 also doesn't expose one - the change_log entries are
//! purely observability for tests asserting state moved.
//!
//! Bearer-enforcement and request-logging are applied by the parent
//! router's middleware.

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
            get(list_categories).post(create_category),
        )
        .route(
            "/v1.0/me/outlook/masterCategories/{category}",
            get(get_category)
                .patch(patch_category)
                .delete(delete_category),
        )
}

// ── Read paths ──────────────────────────────────────────────────────

async fn list_categories(State(state): State<AppState>) -> Response {
    if let Some(o) = super::maybe_override(&state, "list_categories", |_| Ok(())) {
        return o;
    }
    let fixture = state.fixture();
    let value: Vec<Value> = fixture.categories.iter().map(serialize_category).collect();
    ok_json(json!({
        "@odata.context": "https://graph.microsoft.com/v1.0/$metadata#me/outlook/masterCategories",
        "value": value,
    }))
}

async fn get_category(
    State(state): State<AppState>,
    Path(category): Path<String>,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "get_category", |s| {
        crate::lua::req_set_str(s, "category", &category)
    }) {
        return o;
    }
    let fixture = state.fixture();
    match fixture.categories.iter().find(|c| c.id == category) {
        Some(c) => ok_json(serialize_category(c)),
        None => not_found(&category),
    }
}

// ── Mutations ───────────────────────────────────────────────────────

async fn create_category(State(state): State<AppState>, body: AxumBody) -> Response {
    let parsed = match parse_json_body(body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    state.shared.request_log.record(
        "graph",
        "POST /v1.0/me/outlook/masterCategories".to_string(),
        crate::request_log::body_detail(&parsed),
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
    let color = obj
        .get("color")
        .and_then(Value::as_str)
        .map(str::to_string);
    let client_id = obj.get("id").and_then(Value::as_str).map(str::to_string);

    let result: Result<Value, Response> = {
        let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
        let id = match client_id {
            Some(id) => {
                if fix.categories.iter().any(|c| c.id == id) {
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
            account_id: fix.primary_account().id.clone(),
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

async fn patch_category(
    State(state): State<AppState>,
    Path(category): Path<String>,
    body: AxumBody,
) -> Response {
    let known = {
        let fixture = state.fixture();
        fixture.categories.iter().any(|c| c.id == category)
    };
    if !known {
        return not_found(&category);
    }
    let parsed = match parse_json_body(body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    state.shared.request_log.record(
        "graph",
        format!("PATCH /v1.0/me/outlook/masterCategories/{category}"),
        crate::request_log::body_detail(&parsed),
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
        let idx = match fix.categories.iter().position(|c| c.id == category) {
            Some(i) => i,
            None => return not_found(&category),
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

async fn delete_category(
    State(state): State<AppState>,
    Path(category): Path<String>,
) -> Response {
    let known = {
        let fixture = state.fixture();
        fixture.categories.iter().any(|c| c.id == category)
    };
    if !known {
        return not_found(&category);
    }
    state.shared.request_log.record(
        "graph",
        format!("DELETE /v1.0/me/outlook/masterCategories/{category}"),
        json!({ "id": category }),
    );
    {
        let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
        let id = category.clone();
        let _ = fix.mutate(|f| {
            let before = f.categories.len();
            f.categories.retain(|c| c.id != id);
            if f.categories.len() < before {
                MutationDiff {
                    category_destroyed: vec![id.clone()],
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
