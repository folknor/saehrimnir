//! Microsoft Graph mock.
//!
//! axum router serving `/v1.0/me/...` over plain HTTP. Mail sync is
//! the only resource implemented in v0; everything else falls through
//! to a Graph-shaped 404.
//!
//! The module is laid out as a directory so calendar/contacts/drive/
//! groups/EWS can drop in as sibling files when their surfaces are
//! scouted - see the resource-category table in
//! `notes/ratatoskr-graph-surface.md`.

pub mod mail;
pub mod odata;

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::Request,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::any,
};
use serde_json::{Value, json};

use crate::fixture::Fixture;

#[derive(Clone)]
pub struct AppState {
    pub fixture: Arc<Fixture>,
    pub dispatcher: Option<Arc<crate::lua::Dispatcher>>,
}

/// Consult the Lua dispatcher for `("graph", command)` and convert
/// any `Override::Tagged` into a Graph 400 error response. Returns
/// `None` when no override fired (caller proceeds with default
/// behaviour).
///
/// `build_req` populates the `req` table with command-specific
/// fields. The dispatcher pre-populates `call_index`.
pub fn maybe_override(
    state: &AppState,
    command: &str,
    build_req: impl FnOnce(&mut dellingr::State) -> dellingr::Result<()>,
) -> Option<Response> {
    let d = state.dispatcher.as_ref()?;
    match d.dispatch("graph", command, build_req) {
        crate::lua::Override::Tagged { status, message } => {
            Some(error(StatusCode::BAD_REQUEST, &status, &message))
        }
        crate::lua::Override::None => None,
    }
}

/// Build the Graph router. v0 mounts mail handlers under
/// `/v1.0/me/`; everything else is caught by [`not_implemented`].
pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(mail::router())
        .fallback(any(not_implemented))
        .with_state(state)
}

/// Graph error envelope, RFC 7807 / Microsoft Graph error format.
pub fn error(status: StatusCode, code: &str, message: &str) -> Response {
    let body = json!({
        "error": {
            "code": code,
            "message": message,
        }
    });
    (status, Json(body)).into_response()
}

/// Catchall for any path we haven't implemented yet. Returns the
/// canonical Graph error shape so the client gets a uniform "not
/// implemented in v0" response.
async fn not_implemented(req: Request) -> Response {
    let path = req.uri().path().to_string();
    error(
        StatusCode::NOT_FOUND,
        "ResourceNotImplemented",
        &format!("v0 mock does not implement {} {path}", req.method()),
    )
}

/// Surface a value-wrapped OData collection. Exposed for tests and
/// for handlers in the resource modules.
pub fn ok_json(v: Value) -> Response {
    (StatusCode::OK, Json(v)).into_response()
}
