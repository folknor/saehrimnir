//! CalDAV mock listener.
//!
//! Serves the WebDAV / CalDAV verb surface ratatoskr's CalDAV
//! client (`<ratatoskr>/crates/core/src/caldav/client/`) exercises:
//! `PROPFIND`, `REPORT`, `GET`, `PUT`, `DELETE` over plain HTTP.
//!
//! The mock binds its own port (separate from the JMAP / Graph /
//! Gmail HTTP listeners) because CalDAV uses non-standard HTTP
//! verbs (`PROPFIND`, `REPORT`) that some intermediaries refuse to
//! pipe through, and clients identify the listener by its DAV-
//! namespaced response shape rather than a path prefix on a shared
//! host.
//!
//! Calendar collections and events project from the existing
//! `[[calendar]]` / `[[event]]` fixture entries the Graph calendar
//! surface already uses, so a single fixture exercises both
//! backends. Mutations (`PUT` / `DELETE`) route through
//! `Fixture::mutate` so the change_log lights up in the same way
//! Graph `POST /events` / `PATCH` / `DELETE` does. A subsequent
//! Graph `calendarView/delta` walk observes the CalDAV write
//! through the same `event_*` id sets, and vice versa.
//!
//! See `notes/ratatoskr-caldav-surface.md` for the wire shape.

// Helpers below land in subsequent wedges; suppress the noisy
// "unused" warnings while the verb handlers are still stubs.
#![allow(dead_code)]

pub mod ical;
pub mod xml;

use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::any,
};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::watch;

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
}

/// Build the CalDAV router. CalDAV uses HTTP verbs (`PROPFIND`,
/// `REPORT`) that axum's `MethodFilter` doesn't enumerate, so we
/// install a single `any` fallback that dispatches internally on
/// `(method, path)`. The verb name is matched textually so the
/// extension verbs flow through without a custom `MethodFilter`.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", any(dispatch))
        .route("/{*rest}", any(dispatch))
        .with_state(state)
}

/// Spawn the CalDAV listener bound to `listener`. Uses
/// `axum::serve` with the same graceful-shutdown pattern as the
/// other HTTP listeners.
pub async fn serve(
    listener: TcpListener,
    state: AppState,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let app = router(state);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            while shutdown.changed().await.is_ok() {
                if *shutdown.borrow() {
                    return;
                }
            }
        })
        .await
}

/// Method-name constants. Axum's `Method::PROPFIND` / `Method::REPORT`
/// aren't compile-time constants; we match on `as_str()` against
/// these uppercase strings. The HTTP spec requires methods are
/// case-sensitive uppercase tokens, and reqwest (ratatoskr's HTTP
/// stack) emits them that way.
const PROPFIND: &str = "PROPFIND";
const REPORT: &str = "REPORT";

async fn dispatch(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path().to_string();

    // Record into the cross-protocol request log so
    // `GET /test/requests` covers CalDAV the same way it covers
    // Graph + Gmail. Query strings are stripped from `command` and
    // surfaced in `detail.query` to match the existing convention.
    state.shared.request_log.record(
        "caldav",
        format!("{method} {path}"),
        json!({ "query": uri.query() }),
    );
    state.shared.latency.sleep_for("caldav").await;

    let m = method.as_str();
    if m == PROPFIND {
        return handle_propfind(&state, &path, &headers, &body).await;
    }
    if m == REPORT {
        return handle_report(&state, &path, &headers, &body).await;
    }
    match method {
        Method::GET => handle_get(&state, &path, &headers).await,
        Method::PUT => handle_put(&state, &path, &headers, &body).await,
        Method::DELETE => handle_delete(&state, &path, &headers).await,
        Method::OPTIONS => handle_options(),
        _ => not_found(&format!("{method} {path}")),
    }
}

// ── Stubs (filled in by subsequent wedges) ──────────────────────────

async fn handle_propfind(
    _state: &AppState,
    _path: &str,
    _headers: &HeaderMap,
    _body: &[u8],
) -> Response {
    not_implemented("PROPFIND")
}

async fn handle_report(
    _state: &AppState,
    _path: &str,
    _headers: &HeaderMap,
    _body: &[u8],
) -> Response {
    not_implemented("REPORT")
}

async fn handle_get(_state: &AppState, _path: &str, _headers: &HeaderMap) -> Response {
    not_implemented("GET")
}

async fn handle_put(
    _state: &AppState,
    _path: &str,
    _headers: &HeaderMap,
    _body: &[u8],
) -> Response {
    not_implemented("PUT")
}

async fn handle_delete(_state: &AppState, _path: &str, _headers: &HeaderMap) -> Response {
    not_implemented("DELETE")
}

/// `OPTIONS *` and `OPTIONS /` advertise the verbs we support plus
/// the DAV class headers ratatoskr's client checks during
/// discovery. v0 advertises class 1 (WebDAV) plus
/// `calendar-access` (RFC 4791); ACLs and free-busy are out of
/// scope.
fn handle_options() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("DAV", "1, calendar-access")
        .header("Allow", "OPTIONS, PROPFIND, REPORT, GET, PUT, DELETE")
        .body(Body::empty())
        .expect("static OPTIONS response builds")
}

fn not_implemented(verb: &str) -> Response {
    let body = format!("CalDAV {verb} not implemented in v0");
    (StatusCode::NOT_IMPLEMENTED, body).into_response()
}

fn not_found(what: &str) -> Response {
    let body = format!("not found: {what}");
    (StatusCode::NOT_FOUND, body).into_response()
}

/// Decode the request body as UTF-8. CalDAV XML and iCalendar are
/// both UTF-8 (with explicit `charset=utf-8` on the request); a
/// non-UTF-8 body is a client bug.
#[allow(clippy::result_large_err)]
pub(crate) fn body_to_str(body: &[u8]) -> Result<&str, Response> {
    match std::str::from_utf8(body) {
        Ok(s) => Ok(s),
        Err(_) => Err(bad_request("body must be UTF-8")),
    }
}

pub(crate) fn bad_request(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, msg.to_string()).into_response()
}

/// Read the `Depth:` header. Defaults to `0` per RFC 4918 §10.2.
pub(crate) fn depth_header(headers: &HeaderMap) -> u8 {
    match headers.get("depth").and_then(|v| v.to_str().ok()) {
        Some("0") | None => 0,
        Some("1") => 1,
        // "infinity" requests legitimate for some operations; v0
        // treats them as Depth=1 (the maximum we support) so a
        // recursive PROPFIND still returns one level.
        _ => 1,
    }
}

/// Build a 207 Multi-Status response with the given XML body.
pub(crate) fn multistatus(body: String) -> Response {
    Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/xml; charset=utf-8"),
        )
        .body(Body::from(body))
        .expect("static multistatus response builds")
}
