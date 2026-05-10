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

// ── PROPFIND ────────────────────────────────────────────────────────

async fn handle_propfind(
    state: &AppState,
    path: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Response {
    let body_str = match body_to_str(body) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let depth = depth_header(headers);
    let fixture = state.shared.fixture.read().expect("fixture lock poisoned");

    match parse_path(&fixture, path) {
        ResourcePath::Root => propfind_root(&fixture, body_str),
        ResourcePath::WellKnown => propfind_root(&fixture, body_str),
        ResourcePath::Principal { user } => {
            propfind_principal(&fixture, &user, body_str)
        }
        ResourcePath::CalendarHome { user } => {
            propfind_calendar_home(&fixture, &user, body_str, depth)
        }
        ResourcePath::Calendar { user, calendar_id } => {
            propfind_calendar(&fixture, &user, &calendar_id, body_str, depth)
        }
        ResourcePath::Event { user, calendar_id, event_id } => {
            propfind_event(&fixture, &user, &calendar_id, &event_id, body_str)
        }
        ResourcePath::Unknown => not_found(path),
    }
}

/// What kind of CalDAV resource a request URL refers to. Parsing is
/// permissive about a trailing slash because real clients send both
/// `/calendars/{user}` and `/calendars/{user}/`.
#[derive(Debug)]
enum ResourcePath {
    Root,
    WellKnown,
    Principal { user: String },
    CalendarHome { user: String },
    Calendar { user: String, calendar_id: String },
    Event { user: String, calendar_id: String, event_id: String },
    Unknown,
}

fn parse_path(fixture: &crate::fixture::Fixture, path: &str) -> ResourcePath {
    if path == "/" {
        return ResourcePath::Root;
    }
    if path == "/.well-known/caldav" || path == "/.well-known/caldav/" {
        return ResourcePath::WellKnown;
    }
    let trimmed = path.trim_end_matches('/');
    let segments: Vec<&str> = trimmed
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let user = &fixture.account.id;
    match segments.as_slice() {
        ["principals", u] if u == user => ResourcePath::Principal {
            user: (*u).to_string(),
        },
        ["calendars", u] if u == user => ResourcePath::CalendarHome {
            user: (*u).to_string(),
        },
        ["calendars", u, cal] if u == user => ResourcePath::Calendar {
            user: (*u).to_string(),
            calendar_id: (*cal).to_string(),
        },
        ["calendars", u, cal, event] if u == user => {
            let event_id = event.strip_suffix(".ics").unwrap_or(event).to_string();
            ResourcePath::Event {
                user: (*u).to_string(),
                calendar_id: (*cal).to_string(),
                event_id,
            }
        }
        _ => ResourcePath::Unknown,
    }
}

/// PROPFIND on `/` (or `/.well-known/caldav`): return the
/// `current-user-principal` URL. Some clients ask for additional
/// properties on the root resource; we honour
/// `current-user-principal` and silently drop anything else (a
/// truly conformant server would emit them under a 404 propstat).
fn propfind_root(fixture: &crate::fixture::Fixture, body: &str) -> Response {
    let user = &fixture.account.id;
    let principal_url = format!("/principals/{user}/");
    let mut props = String::new();
    if xml::body_requests_prop(body, "current-user-principal") {
        props.push_str(&format!(
            "<D:current-user-principal><D:href>{}</D:href></D:current-user-principal>",
            xml::escape(&principal_url),
        ));
    }
    if xml::body_requests_prop(body, "principal-URL") {
        // Older clients ask for D:principal-URL alongside (or instead
        // of) current-user-principal. Same value.
        props.push_str(&format!(
            "<D:principal-URL><D:href>{}</D:href></D:principal-URL>",
            xml::escape(&principal_url),
        ));
    }
    multistatus(wrap_responses(&[Response207 {
        href: "/",
        ok_props: &props,
    }]))
}

/// PROPFIND on `/principals/{user}/`: return the
/// `calendar-home-set` URL.
fn propfind_principal(fixture: &crate::fixture::Fixture, user: &str, body: &str) -> Response {
    let home_url = format!("/calendars/{user}/");
    let mut props = String::new();
    if xml::body_requests_prop(body, "calendar-home-set") {
        props.push_str(&format!(
            "<C:calendar-home-set><D:href>{}</D:href></C:calendar-home-set>",
            xml::escape(&home_url),
        ));
    }
    if xml::body_requests_prop(body, "current-user-principal") {
        props.push_str(&format!(
            "<D:current-user-principal><D:href>/principals/{}/</D:href></D:current-user-principal>",
            xml::escape(user),
        ));
    }
    if xml::body_requests_prop(body, "displayname") {
        props.push_str(&format!(
            "<D:displayname>{}</D:displayname>",
            xml::escape(&fixture.account.name),
        ));
    }
    multistatus(wrap_responses(&[Response207 {
        href: &format!("/principals/{user}/"),
        ok_props: &props,
    }]))
}

/// PROPFIND on `/calendars/{user}/`. Depth=0 returns just the home
/// collection; Depth=1 lists every calendar plus the home.
fn propfind_calendar_home(
    fixture: &crate::fixture::Fixture,
    user: &str,
    body: &str,
    depth: u8,
) -> Response {
    let home_href = format!("/calendars/{user}/");
    let home_props = home_collection_props(body);

    let mut entries = vec![Response207 {
        href: &home_href,
        ok_props: &home_props,
    }];
    let mut per_calendar_hrefs = Vec::new();
    let mut per_calendar_props = Vec::new();
    if depth >= 1 {
        for cal in &fixture.calendars {
            per_calendar_hrefs.push(format!("/calendars/{user}/{}/", cal.id));
            per_calendar_props.push(calendar_props(fixture, cal, body));
        }
    }
    for (i, href) in per_calendar_hrefs.iter().enumerate() {
        entries.push(Response207 {
            href,
            ok_props: &per_calendar_props[i],
        });
    }
    multistatus(wrap_responses(&entries))
}

/// PROPFIND on `/calendars/{user}/{cal}/`. Depth=0 returns just
/// the calendar's own props; Depth=1 also lists each event resource
/// (handled in wedge C).
fn propfind_calendar(
    fixture: &crate::fixture::Fixture,
    user: &str,
    calendar_id: &str,
    body: &str,
    depth: u8,
) -> Response {
    let cal = match fixture.calendars.iter().find(|c| c.id == calendar_id) {
        Some(c) => c,
        None => return not_found(&format!("/calendars/{user}/{calendar_id}/")),
    };
    let cal_href = format!("/calendars/{user}/{calendar_id}/");
    let cal_props = calendar_props(fixture, cal, body);

    let mut entries = vec![Response207 {
        href: &cal_href,
        ok_props: &cal_props,
    }];
    // Depth=1 event listing handled in wedge C.
    let mut event_hrefs = Vec::new();
    let mut event_props = Vec::new();
    if depth >= 1 {
        for ev in fixture.events.iter().filter(|e| e.calendar_id == calendar_id) {
            event_hrefs.push(format!("/calendars/{user}/{calendar_id}/{}.ics", ev.id));
            event_props.push(event_resource_props(fixture, ev, body));
        }
    }
    for (i, href) in event_hrefs.iter().enumerate() {
        entries.push(Response207 {
            href,
            ok_props: &event_props[i],
        });
    }
    multistatus(wrap_responses(&entries))
}

/// PROPFIND on a single event resource. Used rarely; ratatoskr
/// prefers `REPORT calendar-multiget` for the bulk fetch path.
fn propfind_event(
    fixture: &crate::fixture::Fixture,
    user: &str,
    calendar_id: &str,
    event_id: &str,
    body: &str,
) -> Response {
    let ev = match fixture
        .events
        .iter()
        .find(|e| e.id == event_id && e.calendar_id == calendar_id)
    {
        Some(e) => e,
        None => {
            return not_found(&format!("/calendars/{user}/{calendar_id}/{event_id}.ics"));
        }
    };
    let href = format!("/calendars/{user}/{calendar_id}/{event_id}.ics");
    let props = event_resource_props(fixture, ev, body);
    multistatus(wrap_responses(&[Response207 {
        href: &href,
        ok_props: &props,
    }]))
}

// ── Property serialisation helpers ──────────────────────────────────

/// Properties for the calendar-home collection itself.
fn home_collection_props(body: &str) -> String {
    let mut props = String::new();
    if xml::body_requests_prop(body, "resourcetype") {
        props.push_str("<D:resourcetype><D:collection/></D:resourcetype>");
    }
    if xml::body_requests_prop(body, "displayname") {
        props.push_str("<D:displayname>Calendars</D:displayname>");
    }
    props
}

/// Properties for a calendar collection. Includes the Apple
/// `calendar-color`, the CalendarServer `getctag`, and a
/// permissive `current-user-privilege-set` (read + write +
/// write-properties + write-content), since v0 has no
/// permission model.
fn calendar_props(
    fixture: &crate::fixture::Fixture,
    cal: &crate::fixture::Calendar,
    body: &str,
) -> String {
    let mut props = String::new();
    if xml::body_requests_prop(body, "resourcetype") {
        props.push_str("<D:resourcetype><D:collection/><C:calendar/></D:resourcetype>");
    }
    if xml::body_requests_prop(body, "displayname") {
        props.push_str(&format!(
            "<D:displayname>{}</D:displayname>",
            xml::escape(&cal.name),
        ));
    }
    if xml::body_requests_prop(body, "getctag") {
        props.push_str(&format!(
            "<CS:getctag>{}</CS:getctag>",
            xml::escape(&calendar_ctag(fixture, &cal.id)),
        ));
    }
    if let Some(color) = &cal.color
        && xml::body_requests_prop(body, "calendar-color")
    {
        props.push_str(&format!(
            "<IC:calendar-color>{}</IC:calendar-color>",
            xml::escape(color),
        ));
    }
    if xml::body_requests_prop(body, "current-user-privilege-set") {
        props.push_str(
            "<D:current-user-privilege-set>\
                <D:privilege><D:read/></D:privilege>\
                <D:privilege><D:write/></D:privilege>\
                <D:privilege><D:write-properties/></D:privilege>\
                <D:privilege><D:write-content/></D:privilege>\
                <D:privilege><D:read-current-user-privilege-set/></D:privilege>\
            </D:current-user-privilege-set>",
        );
    }
    if xml::body_requests_prop(body, "supported-calendar-component-set") {
        props.push_str(
            "<C:supported-calendar-component-set>\
                <C:comp name=\"VEVENT\"/>\
            </C:supported-calendar-component-set>",
        );
    }
    props
}

/// Properties for a VEVENT resource. Used by PROPFIND Depth=1 on a
/// calendar collection (the inner event listing) and by direct
/// PROPFIND on an event URL.
fn event_resource_props(
    fixture: &crate::fixture::Fixture,
    ev: &crate::fixture::Event,
    body: &str,
) -> String {
    let mut props = String::new();
    if xml::body_requests_prop(body, "resourcetype") {
        // Resources (as opposed to collections) emit an empty
        // `<D:resourcetype/>`. Ratatoskr's parser distinguishes
        // collections from resources by the presence of children
        // here.
        props.push_str("<D:resourcetype/>");
    }
    if xml::body_requests_prop(body, "getetag") {
        props.push_str(&format!(
            "<D:getetag>{}</D:getetag>",
            xml::escape(&event_etag(fixture, &ev.id)),
        ));
    }
    if xml::body_requests_prop(body, "getcontenttype") {
        props.push_str("<D:getcontenttype>text/calendar; component=vevent</D:getcontenttype>");
    }
    props
}

/// One row in the multistatus envelope. `ok_props` goes under a
/// `<D:status>HTTP/1.1 200 OK</D:status>` propstat. v0 doesn't yet
/// emit 404 propstat for missing properties; ratatoskr's parser
/// tolerates their absence.
struct Response207<'a> {
    href: &'a str,
    ok_props: &'a str,
}

fn wrap_responses(entries: &[Response207<'_>]) -> String {
    let mut out = String::new();
    out.push_str(r#"<?xml version="1.0" encoding="utf-8"?>"#);
    out.push('\n');
    out.push_str(
        r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:CS="http://calendarserver.org/ns/" xmlns:IC="http://apple.com/ns/ical/">"#,
    );
    for e in entries {
        out.push_str("<D:response>");
        out.push_str(&format!("<D:href>{}</D:href>", xml::escape(e.href)));
        out.push_str("<D:propstat>");
        out.push_str("<D:prop>");
        out.push_str(e.ok_props);
        out.push_str("</D:prop>");
        out.push_str("<D:status>HTTP/1.1 200 OK</D:status>");
        out.push_str("</D:propstat>");
        out.push_str("</D:response>");
    }
    out.push_str("</D:multistatus>");
    out
}

/// Deterministic CTag for a calendar. Cycles when `fixture.state`
/// advances (i.e. on any mutation through `Fixture::mutate`).
/// Calendar id is folded in so two calendars in the same fixture
/// don't appear synchronised when one of them mutates - real
/// servers emit different CTags for sibling calendars and
/// ratatoskr's per-calendar polling expects that. (Pre-state-advance
/// the values stay byte-stable across runs since `fixture.state`
/// is fixture-controlled.)
fn calendar_ctag(fixture: &crate::fixture::Fixture, calendar_id: &str) -> String {
    format!("{}/{calendar_id}", fixture.state)
}

/// Deterministic ETag for an event. Same shape as the CTag (state +
/// event id), and changes whenever the fixture state advances.
/// Ratatoskr only checks for byte equality so the format is free to
/// evolve.
fn event_etag(fixture: &crate::fixture::Fixture, event_id: &str) -> String {
    format!("\"{}/{event_id}\"", fixture.state)
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
