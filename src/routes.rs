//! HTTP route handlers.
//!
//! All responses derive from the loaded [`Fixture`]. Determinism
//! contract: same fixture in → byte-identical responses out (modulo
//! json key-ordering, which is alphabetical by virtue of `serde_json`'s
//! default `BTreeMap`-backed `Map`).

use std::sync::Arc;

use axum::{Json, Router, extract::State, routing::get};
use serde_json::{Value, json};

use crate::fixture::Fixture;

#[derive(Clone)]
pub struct AppState {
    pub fixture: Arc<Fixture>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/.well-known/jmap", get(session))
        .route("/jmap/session", get(session))
        .with_state(state)
}

async fn root() -> &'static str {
    "saehrimnir\n"
}

/// Session resource per RFC 8620 §2.
///
/// Capabilities are deliberately limited to `core` + `mail` (see
/// `notes/ratatoskr-client-surface.md` — advertising `principals`
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
