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
//!
//! `mailboxSettings` and `messageRules` stay accept-and-ignore (no
//! fixture slot, nothing durable changes, no change-log transition).
//!
//! `/subscriptions` (webhook push) is NOT accept-and-ignore anymore:
//! the create handler stores the subscription (resource, changeType,
//! clientState, notificationUrl, expiration) in the shared
//! `crate::push::PushHub` and echoes Graph's `validationToken` for the
//! subscription-validation handshake; renew updates the stored
//! expiration; delete removes it. The stored `clientState` /
//! `notificationUrl` drive the change-notification the test-admin
//! state-mutation trigger POSTs to the registered loopback endpoint.
//! Subscriptions live in the push hub (process-volatile, cleared by
//! `POST /test/fixture/reset`), not the fixture, so they record no
//! change-log transition.

use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};

use super::{AppState, ok_json};
use crate::push::GraphSubscription;

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

/// Default subscription expiration echoed when the create body omits
/// one. Deterministic far-future value (real Graph caps mail
/// subscriptions near ~3 days; the mock pins it for byte-stable
/// responses - a harness renews via PATCH exactly as a real client
/// would).
const SUBSCRIPTION_EXPIRATION: &str = "2100-01-01T00:00:00Z";

/// `POST /v1.0/subscriptions`. Two behaviours:
///
/// 1. **Validation handshake.** When the request carries a
///    `?validationToken=...` query param, echo it verbatim as
///    `text/plain` 200 and store nothing. This is Graph's subscription-
///    validation contract (the side that owns the endpoint must echo
///    the token); exposing it on this route keeps the whole handshake
///    loopback-drivable without an out-of-band callback.
/// 2. **Create.** Otherwise parse the body, resolve the bearer's
///    account, store the subscription (id, resource, changeType,
///    clientState, notificationUrl, expiration) in the push hub, and
///    return 201 with the created resource. The stored `clientState`
///    and `notificationUrl` drive the change-notification the state-
///    mutation trigger later POSTs.
async fn create_subscription(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(token) = params.get("validationToken") {
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/plain")],
            token.clone(),
        )
            .into_response();
    }
    let body = body.map_or(Value::Null, |Json(v)| v);
    let account_id =
        crate::oauth::account_from_bearer(&state.fixture(), &state.shared.token_store, &headers);
    let resource = body
        .get("resource")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let change_type = body
        .get("changeType")
        .and_then(|v| v.as_str())
        .unwrap_or("created,updated,deleted")
        .to_string();
    let notification_url = body
        .get("notificationUrl")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let client_state = body
        .get("clientState")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let expiration = body
        .get("expirationDateTime")
        .and_then(|v| v.as_str())
        .unwrap_or(SUBSCRIPTION_EXPIRATION)
        .to_string();
    let id = format!("mock-subscription-{}", state.shared.push.next_seq());
    state.shared.push.graph_create_subscription(GraphSubscription {
        id: id.clone(),
        account_id,
        resource: resource.clone(),
        change_type: change_type.clone(),
        client_state: client_state.clone(),
        notification_url: notification_url.clone(),
        expiration: expiration.clone(),
    });
    (
        StatusCode::CREATED,
        axum::Json(json!({
            "@odata.context": "https://graph.microsoft.com/v1.0/$metadata#subscriptions/$entity",
            "id": id,
            "resource": resource,
            "changeType": change_type,
            "notificationUrl": notification_url,
            "clientState": client_state,
            "expirationDateTime": expiration,
        })),
    )
        .into_response()
}

/// Renew (PATCH): update the stored expiration and echo it back. An
/// unknown id still echoes (real Graph would 404, but a renew against a
/// never-created mock subscription is a harness mistake, not a wire
/// contract worth modelling).
async fn renew_subscription(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let expiration = body
        .get("expirationDateTime")
        .and_then(|v| v.as_str())
        .unwrap_or(SUBSCRIPTION_EXPIRATION)
        .to_string();
    state
        .shared
        .push
        .graph_renew_subscription(&id, expiration.clone());
    ok_json(json!({
        "id": id,
        "expirationDateTime": expiration,
    }))
}

async fn delete_subscription(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    state.shared.push.graph_delete_subscription(&id);
    StatusCode::NO_CONTENT.into_response()
}
