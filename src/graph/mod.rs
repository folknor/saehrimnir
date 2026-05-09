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

pub mod calendar;
pub mod mail;
pub mod odata;

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
use crate::oauth::BearerDecision;
use crate::shared::SharedHandles;

#[derive(Clone)]
pub struct AppState {
    /// Shared handle bag: fixture, dispatcher, request log,
    /// token store. See `crate::shared::SharedHandles`.
    pub shared: SharedHandles,
}

impl AppState {
    /// Build an `AppState` around `fixture` with fresh, default
    /// shared handles.
    pub fn for_test(fixture: Arc<Fixture>) -> Self {
        Self {
            shared: SharedHandles::for_test(fixture),
        }
    }

    /// Replace the request log on the shared handle bag.
    pub fn with_request_log(mut self, log: crate::request_log::RequestLog) -> Self {
        self.shared.request_log = log;
        self
    }

    /// Attach a Lua dispatcher.
    pub fn with_dispatcher(mut self, dispatcher: Arc<crate::lua::Dispatcher>) -> Self {
        self.shared.dispatcher = Some(dispatcher);
        self
    }
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
    let d = state.shared.dispatcher.as_ref()?;
    match d.dispatch("graph", command, build_req) {
        crate::lua::Override::Tagged { status, message } => {
            Some(error(StatusCode::BAD_REQUEST, &status, &message))
        }
        crate::lua::Override::None => None,
    }
}

/// Build the Graph router. v0 mounts mail handlers under
/// `/v1.0/me/`; everything else is caught by [`not_implemented`].
///
/// Layer order matters: axum applies layers in reverse declaration
/// order ("onion" semantics), so the bearer-enforcement layer ends
/// up *outermost* and `log_request` runs only for allowed requests.
/// That is intentional - denied requests skip the request log
/// entirely, no auth-bypassed-but-logged paradox - but tests
/// asserting "no extra calls happened" cannot see denied probes.
/// (Observed during 2026-05-09 review.) The `not_implemented`
/// fallback is *inside* both layers, so 404s still get logged when
/// auth allows them through.
pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(mail::router())
        .merge(calendar::router())
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

/// When `fixture.oauth.enforce` is true, reject requests without a
/// valid `Authorization: Bearer` header before the route handler
/// runs. Returns the standard Graph error envelope on rejection so
/// the wire shape matches the rest of the surface.
async fn enforce_bearer_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    match crate::oauth::check_bearer(
        &state.shared.fixture,
        &state.shared.token_store,
        req.headers(),
    ) {
        BearerDecision::Allow => next.run(req).await,
        BearerDecision::Deny(reason) => {
            error(StatusCode::UNAUTHORIZED, "InvalidAuthenticationToken", &reason)
        }
    }
}

/// Per-request middleware that records `(method, path)` into the
/// shared request log before the handler runs. Query strings are
/// dropped so `?$top=10` and `?$top=20` collapse to the same entry
/// for assertion purposes; tests that need the query string can
/// look in `detail.query`.
async fn log_request(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(str::to_string);
    state.shared.request_log.record(
        "graph",
        format!("{method} {path}"),
        json!({ "query": query }),
    );
    next.run(req).await
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
