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
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::any,
};
use serde_json::{Value, json};

use crate::fixture::Fixture;
use crate::oauth::{BearerDecision, TokenStore};
use crate::request_log::RequestLog;

#[derive(Clone)]
pub struct AppState {
    pub fixture: Arc<Fixture>,
    pub dispatcher: Option<Arc<crate::lua::Dispatcher>>,
    pub request_log: RequestLog,
    pub token_store: TokenStore,
}

/// Consult the Lua dispatcher for `("gmail", command)` and convert
/// any `Override::Tagged` into a Gmail 400 error response. Returns
/// `None` when no override fired.
pub fn maybe_override(
    state: &AppState,
    command: &str,
    build_req: impl FnOnce(&mut dellingr::State) -> dellingr::Result<()>,
) -> Option<Response> {
    let d = state.dispatcher.as_ref()?;
    match d.dispatch("gmail", command, build_req) {
        crate::lua::Override::Tagged { status, message } => {
            Some(error(StatusCode::BAD_REQUEST, &message, &status))
        }
        crate::lua::Override::None => None,
    }
}

/// Build the Gmail router. v0 mounts mail handlers under
/// `/gmail/v1/users/me/`; everything else is caught by
/// [`not_implemented`].
/// See `graph::router` for the full rationale on layer ordering;
/// same semantics apply here. `enforce_bearer_middleware` ends up
/// outermost; denied requests skip `log_request`.
pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(mail::router())
        .fallback(any(not_implemented))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            log_request,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            enforce_bearer_middleware,
        ))
        .with_state(state)
}

/// Mirrors `graph::enforce_bearer_middleware`. Returns the Gmail
/// error envelope on rejection.
async fn enforce_bearer_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    match crate::oauth::check_bearer(
        &state.fixture,
        &state.token_store,
        req.headers(),
    ) {
        BearerDecision::Allow => next.run(req).await,
        BearerDecision::Deny(reason) => {
            error(StatusCode::UNAUTHORIZED, &reason, "authError")
        }
    }
}

/// Per-request middleware mirroring `graph::log_request`. Records
/// `(method, path)` into the shared request log; the query string
/// (Gmail's `?q=after:...&pageToken=...`) lands in `detail.query`
/// so tests can assert on it without the path noise.
async fn log_request(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(str::to_string);
    state.request_log.record(
        "gmail",
        format!("{method} {path}"),
        json!({ "query": query }),
    );
    next.run(req).await
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
