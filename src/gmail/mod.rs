//! Gmail REST API mock.
//!
//! axum router serving `/gmail/v1/users/me/...` over plain HTTP.
//! Mail sync only in v0; everything else falls through to the Gmail
//! error envelope on 404. The module is laid out as a directory so
//! contacts (People API) and Drive can drop in as sibling files - see
//! `notes/ratatoskr-gmail-surface.md`.

pub mod mail;

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
}

/// Build the Gmail router. v0 mounts mail handlers under
/// `/gmail/v1/users/me/`; everything else is caught by
/// [`not_implemented`].
pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(mail::router())
        .fallback(any(not_implemented))
        .with_state(state)
}

/// Gmail error envelope as documented at
/// https://developers.google.com/gmail/api/guides/handle-errors.
pub fn error(status: StatusCode, message: &str, reason: &str) -> Response {
    let body = json!({
        "error": {
            "code": status.as_u16(),
            "message": message,
            "errors": [{
                "domain": "global",
                "reason": reason,
                "message": message,
            }],
        }
    });
    (status, Json(body)).into_response()
}

async fn not_implemented(req: Request) -> Response {
    let path = req.uri().path().to_string();
    error(
        StatusCode::NOT_FOUND,
        &format!("v0 mock does not implement {} {path}", req.method()),
        "notFound",
    )
}

pub fn ok_json(v: Value) -> Response {
    (StatusCode::OK, Json(v)).into_response()
}
