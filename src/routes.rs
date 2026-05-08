//! HTTP route handlers.
//!
//! All responses derive from the loaded [`Fixture`]. Determinism
//! contract: same fixture in → byte-identical responses out (modulo
//! json key-ordering, which is alphabetical by virtue of `serde_json`'s
//! default `BTreeMap`-backed `Map`).

use std::sync::Arc;

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};

use crate::fixture::Fixture;
use crate::jmap::{self, JmapRequest, JmapResponse};

#[derive(Clone)]
pub struct AppState {
    pub fixture: Arc<Fixture>,
    pub dispatcher: Option<Arc<crate::lua::Dispatcher>>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/.well-known/jmap", get(session))
        .route("/jmap/session", get(session))
        .route("/jmap/api", post(api))
        .route(
            "/jmap/download/{account_id}/{blob_id}/{name}",
            get(download),
        )
        .with_state(state)
}

async fn root() -> &'static str {
    "saehrimnir\n"
}

/// Session resource per RFC 8620 §2.
///
/// Capabilities are deliberately limited to `core` + `mail` (see
/// `notes/ratatoskr-jmap-surface.md` - advertising `principals`
/// pulls the client into `Principal/get` and `ShareNotification`
/// paths the mock can't satisfy in v0).
async fn session(State(state): State<AppState>) -> Json<Value> {
    let fixture = &state.fixture;
    let acct_id = &fixture.account.id;
    let acct_name = &fixture.account.name;

    let mut accounts = serde_json::Map::new();
    accounts.insert(
        acct_id.clone(),
        json!({
            "name": acct_name,
            "isPersonal": true,
            "isReadOnly": false,
            "accountCapabilities": {
                "urn:ietf:params:jmap:mail": {}
            }
        }),
    );

    let mut primary = serde_json::Map::new();
    primary.insert(
        "urn:ietf:params:jmap:core".to_string(),
        Value::String(acct_id.clone()),
    );
    primary.insert(
        "urn:ietf:params:jmap:mail".to_string(),
        Value::String(acct_id.clone()),
    );

    Json(json!({
        "capabilities": {
            "urn:ietf:params:jmap:core": {
                "maxSizeUpload": 50_000_000_u64,
                "maxConcurrentUpload": 4,
                "maxSizeRequest": 10_000_000_u64,
                "maxConcurrentRequests": 4,
                "maxCallsInRequest": 16,
                "maxObjectsInGet": 500,
                "maxObjectsInSet": 500,
                "collationAlgorithms": []
            },
            "urn:ietf:params:jmap:mail": {}
        },
        "accounts": accounts,
        "primaryAccounts": primary,
        "username": acct_name,
        "apiUrl": "/jmap/api",
        "downloadUrl": "/jmap/download/{accountId}/{blobId}/{name}?accept={type}",
        "uploadUrl": "/jmap/upload/{accountId}",
        "eventSourceUrl": "/jmap/eventsource/?types={types}&closeafter={closeafter}&ping={ping}",
        "state": fixture.state
    }))
}

/// JMAP method-call endpoint. Always 200; per-call errors land in the
/// envelope's `methodResponses`. JSON parse failures bubble up as 400
/// via axum's `Json` extractor, which is the right behaviour per RFC
/// 8620 §3.6.1.
async fn api(State(state): State<AppState>, Json(req): Json<JmapRequest>) -> Json<JmapResponse> {
    Json(jmap::handle(&state.fixture, state.dispatcher.as_ref(), req))
}

/// Blob-download endpoint per RFC 8620 §6.2. The session resource
/// advertises the URL template `/jmap/download/{accountId}/{blobId}/
/// {name}` plus an `accept` query string; we accept any path-shape
/// the client renders and resolve `blob_id` against every email's
/// attachments. Filenames are echoed in `Content-Disposition` but
/// otherwise unused for resolution. The mock doesn't validate
/// `account_id` (single-account in v0).
async fn download(
    State(state): State<AppState>,
    Path((_account_id, blob_id, _name)): Path<(String, String, String)>,
) -> Response {
    for email in &state.fixture.emails {
        for att in &email.attachments {
            if att.blob_id == blob_id {
                return (
                    StatusCode::OK,
                    [
                        (header::CONTENT_TYPE, att.content_type.clone()),
                        (
                            header::CONTENT_DISPOSITION,
                            format!(
                                "{}; filename=\"{}\"",
                                att.disposition.as_str(),
                                att.name
                            ),
                        ),
                    ],
                    Body::from(att.data.clone()),
                )
                    .into_response();
            }
        }
    }
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "type": "urn:ietf:params:jmap:error:notFound",
            "status": 404,
            "detail": format!("blob {blob_id} not found"),
        })),
    )
        .into_response()
}
