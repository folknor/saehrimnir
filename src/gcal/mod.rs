//! Google Calendar v3 API mock.
//!
//! axum router serving `/calendar/v3/users/me/calendarList` and
//! `/calendar/v3/calendars/{id}/events[/...]` over plain HTTP.
//! Real Google Calendar lives at
//! `https://www.googleapis.com/calendar/v3` - shares the
//! `googleapis.com` host with several other Google services but is
//! prefixed by `/calendar/v3`. We host it on a dedicated listener
//! keyed off `--gcal-port` so ratatoskr's eventual
//! `RATATOSKR_TEST_GCAL_ENDPOINT` override can point at exactly this
//! socket.
//!
//! v0 surface scope (drives
//! `<ratatoskr>/crates/calendar/src/google.rs`):
//!
//! - `GET /calendar/v3/users/me/calendarList`.
//! - `GET /calendar/v3/calendars/{id}/events` (`syncToken` /
//!   `pageToken` / `timeMin` / `timeMax` / `singleEvents` /
//!   `maxResults`).
//! - `POST /calendar/v3/calendars/{id}/events`.
//! - `PATCH /calendar/v3/calendars/{id}/events/{eid}`.
//! - `DELETE /calendar/v3/calendars/{id}/events/{eid}`.
//!
//! Mutations land through `Fixture::mutate` and record the same
//! `event_*` transitions Graph / CalDAV / JMAP write, so a Google
//! Calendar create surfaces in a Graph `calendarView/delta` follow-
//! up. Sync-token recovery: an unknown / evicted token returns 410
//! Gone, mirroring the real Google Calendar contract that
//! ratatoskr's recovery branch (`google.rs:175-186`) checks for.

pub mod events;

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

    /// Replace the request log on the shared handle bag. Mirrors
    /// `gmail::AppState::with_request_log` for parity.
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

pub fn maybe_override(
    state: &AppState,
    command: &str,
    build_req: impl FnOnce(&mut dellingr::State) -> dellingr::Result<()>,
) -> Option<Response> {
    let d = state.shared.dispatcher.as_ref()?;
    match d.dispatch("gcal", command, build_req) {
        crate::lua::Override::Tagged { status, message } => {
            Some(error(StatusCode::BAD_REQUEST, &message, &status))
        }
        crate::lua::Override::None => None,
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(events::router())
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
        BearerDecision::Deny(reason) => error(StatusCode::UNAUTHORIZED, &reason, "authError"),
    }
}

async fn log_request(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(str::to_string);
    let conn_id = req
        .extensions()
        .get::<axum::extract::ConnectInfo<crate::connection_id::ConnInfo>>()
        .map(|c| c.id);
    state.shared.request_log.record_with_conn(
        "gcal",
        format!("{method} {path}"),
        json!({ "query": query }),
        conn_id,
    );
    state.shared.latency.sleep_for("gcal").await;
    next.run(req).await
}

/// Google Calendar error envelope. Matches the shape ratatoskr's
/// `google_calendar_parse_json_response` reads back. Args are
/// `(http_status, human_message, errors[0].reason)` - Google
/// family convention. See `graph::error` for the OData reverse.
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

pub async fn serve(
    listener: tokio::net::TcpListener,
    state: AppState,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> std::io::Result<()> {
    let app = router(state);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<crate::connection_id::ConnInfo>(),
    )
    .with_graceful_shutdown(async move {
        while shutdown_rx.changed().await.is_ok() {
            if *shutdown_rx.borrow() {
                return;
            }
        }
    })
    .await
}

#[allow(dead_code)]
fn _arc_keepalive(_: Arc<()>) {}
