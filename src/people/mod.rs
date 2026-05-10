//! Google People API mock.
//!
//! axum router serving `/v1/people/me/connections` and
//! `/v1/otherContacts` over plain HTTP. Real Google People API
//! lives on a separate host (`https://people.googleapis.com/v1`)
//! from Gmail, so this is a sibling listener, not a path-prefix
//! mount on the Gmail one - keeps the protocol separation clean
//! and lets ratatoskr's eventual `RATATOSKR_TEST_PEOPLE_ENDPOINT`
//! override point at exactly this listener.
//!
//! v0 surface scope:
//!
//! - `GET /v1/people/me/connections` (paged list with sync-token
//!   recovery).
//! - `GET /v1/otherContacts` (paged list; v0 always projects an
//!   empty list since the fixture has no `other_contacts` field
//!   yet - the route is here so ratatoskr's bootstrap path
//!   doesn't 404).
//!
//! Mutations live elsewhere (POST / PATCH / DELETE on People API
//! aren't wired by ratatoskr in v0). When they land, follow the
//! Graph `contacts` pattern: write through `Fixture::mutate`,
//! record `contact_*` transitions, and surface deltas via the
//! `nextSyncToken` walk.

pub mod contacts;

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

use crate::oauth::BearerDecision;
use crate::shared::SharedHandles;

#[derive(Clone)]
pub struct AppState {
    pub shared: SharedHandles,
}

impl AppState {
    pub fn for_test(fixture: crate::shared::FixtureHandle) -> Self {
        Self {
            shared: SharedHandles::for_test(fixture),
        }
    }

    pub(crate) fn fixture(&self) -> std::sync::RwLockReadGuard<'_, crate::fixture::Fixture> {
        self.shared.fixture.read().expect("fixture lock poisoned")
    }
}

pub fn maybe_override(
    state: &AppState,
    command: &str,
    build_req: impl FnOnce(&mut dellingr::State) -> dellingr::Result<()>,
) -> Option<Response> {
    let d = state.shared.dispatcher.as_ref()?;
    match d.dispatch("people", command, build_req) {
        crate::lua::Override::Tagged { status, message } => {
            Some(error(StatusCode::BAD_REQUEST, &message, &status))
        }
        crate::lua::Override::None => None,
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(contacts::router())
        .fallback(any(not_implemented))
        .layer(middleware::from_fn_with_state(state.clone(), log_request))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            enforce_bearer_middleware,
        ))
        .with_state(state)
}

async fn enforce_bearer_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let decision = {
        let fixture = state.shared.fixture.read().expect("fixture lock poisoned");
        crate::oauth::check_bearer(&fixture, &state.shared.token_store, req.headers())
    };
    match decision {
        BearerDecision::Allow => next.run(req).await,
        BearerDecision::Deny(reason) => {
            error(StatusCode::UNAUTHORIZED, &reason, "authError")
        }
    }
}

async fn log_request(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(str::to_string);
    state.shared.request_log.record(
        "people",
        format!("{method} {path}"),
        json!({ "query": query }),
    );
    state.shared.latency.sleep_for("people").await;
    next.run(req).await
}

/// People API error envelope. Mirrors the Gmail shape; the People
/// API uses the same Google Cloud error contract.
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

/// Async listener entry. Mirrors the shape of `gmail::serve` so
/// `main.rs` can spawn it identically. Kept as `serve` for
/// consistency even though the body is just `axum::serve`.
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: AppState,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> std::io::Result<()> {
    let app = router(state);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            while shutdown_rx.changed().await.is_ok() {
                if *shutdown_rx.borrow() {
                    return;
                }
            }
        })
        .await
}

// Suppress unused-import warning when test helpers aren't compiled.
#[allow(dead_code)]
fn _arc_keepalive(_: Arc<()>) {}
