//! Graph account-settings + opt-in surfaces, all accept-and-ignore.
//!
//! These three families have no fixture slot, so v0 services them
//! with shaped-but-non-durable responses (the same philosophy as
//! Graph `importance` and the People-API write-back):
//!
//! - `mailboxSettings` (vacation / automaticReplies) - bifrost's
//!   `vacation_get` / `vacation_set` (`pim.rs`). GET reports a
//!   disabled auto-reply; PATCH echoes the submitted setting.
//! - `messageRules` (inbox server-side filters) - bifrost's
//!   `filters_*`. GET lists none; create/patch/delete are stubs.
//! - `/subscriptions` (webhook push) - bifrost's `webhooks.rs`, only
//!   driven in `PushMode::GraphSubscriptions`. Create/renew echo an
//!   expiration; delete is a no-op 204.
//!
//! None of these record change-log transitions (nothing durable
//! changes). Reads of state the mock does store stay in the mail /
//! calendar / contacts modules.

use axum::{
    Json, Router,
    extract::Path,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};

use super::{AppState, ok_json};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/v1.0/me/mailboxSettings",
            get(mailbox_settings).patch(patch_mailbox_settings),
        )
        .route(
            "/v1.0/users/{user}/mailboxSettings",
            get(mailbox_settings).patch(patch_mailbox_settings),
        )
        .route(
            "/v1.0/me/mailFolders/{folder}/messageRules",
            get(list_rules).post(create_rule),
        )
        .route(
            "/v1.0/me/mailFolders/{folder}/messageRules/{rule}",
            get(get_rule).patch(patch_rule).delete(delete_rule),
        )
        .route("/v1.0/subscriptions", post(create_subscription))
        .route(
            "/v1.0/subscriptions/{id}",
            axum::routing::patch(renew_subscription).delete(delete_subscription),
        )
}

// ── mailboxSettings (vacation / automaticReplies) ───────────────────

/// A disabled auto-reply - the fixture stores no vacation state, so
/// the mock always reports "off". bifrost maps `status: disabled` to
/// "vacation not enabled".
async fn mailbox_settings() -> Response {
    ok_json(json!({
        "@odata.context": "https://graph.microsoft.com/v1.0/$metadata#users('me')/mailboxSettings",
        "automaticRepliesSetting": {
            "status": "disabled",
            "externalAudience": "all",
            "internalReplyMessage": "",
            "externalReplyMessage": "",
        },
    }))
}

/// Accept the auto-reply patch and echo it back (not durably stored).
async fn patch_mailbox_settings(Json(body): Json<Value>) -> Response {
    let setting = body.get("automaticRepliesSetting").cloned().unwrap_or(Value::Null);
    ok_json(json!({
        "@odata.context": "https://graph.microsoft.com/v1.0/$metadata#users('me')/mailboxSettings",
        "automaticRepliesSetting": setting,
    }))
}

// ── messageRules (inbox server-side filters) ────────────────────────

async fn list_rules() -> Response {
    ok_json(json!({
        "@odata.context":
            "https://graph.microsoft.com/v1.0/$metadata#me/mailFolders('inbox')/messageRules",
        "value": [],
    }))
}

/// Create a rule (not durably stored): echo the body with a minted
/// id. The id is fixed since the mock keeps no rule state.
async fn create_rule(Json(mut body): Json<Value>) -> Response {
    if let Value::Object(ref mut m) = body {
        m.entry("id".to_string())
            .or_insert_with(|| Value::String("mock-rule-1".to_string()));
    }
    (StatusCode::CREATED, axum::Json(body)).into_response()
}

async fn get_rule(Path((_folder, _rule)): Path<(String, String)>) -> Response {
    super::error(
        StatusCode::NOT_FOUND,
        "ErrorItemNotFound",
        "v0 stores no message rules",
    )
}

async fn patch_rule(Path((_folder, _rule)): Path<(String, String)>, Json(body): Json<Value>) -> Response {
    ok_json(body)
}

async fn delete_rule(Path((_folder, _rule)): Path<(String, String)>) -> Response {
    StatusCode::NO_CONTENT.into_response()
}

// ── /subscriptions (webhook push) ───────────────────────────────────

/// Create a webhook subscription: mint an id and echo the requested
/// `expirationDateTime` + `resource`. No notifications are ever
/// delivered (opt-in push, default off; the mock has no push side).
async fn create_subscription(Json(body): Json<Value>) -> Response {
    let expiration = body
        .get("expirationDateTime")
        .cloned()
        .unwrap_or(Value::Null);
    let resource = body.get("resource").cloned().unwrap_or(Value::Null);
    (
        StatusCode::CREATED,
        axum::Json(json!({
            "@odata.context": "https://graph.microsoft.com/v1.0/$metadata#subscriptions/$entity",
            "id": "mock-subscription-1",
            "resource": resource,
            "expirationDateTime": expiration,
        })),
    )
        .into_response()
}

/// Renew (PATCH): echo the new expiration under the same id.
async fn renew_subscription(Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let expiration = body
        .get("expirationDateTime")
        .cloned()
        .unwrap_or(Value::Null);
    ok_json(json!({
        "id": id,
        "expirationDateTime": expiration,
    }))
}

async fn delete_subscription(Path(_id): Path<String>) -> Response {
    StatusCode::NO_CONTENT.into_response()
}
