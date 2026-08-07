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
//! See `reference/ratatoskr-caldav-surface.md` for the wire shape.

// Helpers below land in subsequent wedges; suppress the noisy
// "unused" warnings while the verb handlers are still stubs.
#![allow(dead_code)]

pub mod ical;
pub mod xml;

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::any,
};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::watch;

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
}

/// Build the CalDAV router. CalDAV uses HTTP verbs (`PROPFIND`,
/// `REPORT`) that axum's `MethodFilter` doesn't enumerate, so we
/// install a single `any` fallback that dispatches internally on
/// `(method, path)`. The verb name is matched textually so the
/// extension verbs flow through without a custom `MethodFilter`.
///
/// Bearer enforcement layers in front of dispatch when
/// `fixture.oauth.enforce` is true, mirroring the Graph / Gmail /
/// JMAP listeners. CalDAV has no shared error-envelope schema; the
/// rejection is a bare `401 Unauthorized` with a
/// `WWW-Authenticate: Bearer` header.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", any(dispatch))
        .route("/{*rest}", any(dispatch))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            enforce_bearer_middleware,
        ))
        .with_state(state)
}

/// When `fixture.oauth.enforce` is true, reject CalDAV requests
/// without a valid `Authorization: Bearer` header before dispatch.
/// Without this layer, a fixture that flips bearer enforcement on
/// would still let CalDAV `PUT` / `DELETE` mutate state with no
/// token - breaking parity with the JMAP / Graph / Gmail listeners.
async fn enforce_bearer_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    // Drop the read guard before `next.run` awaits so we don't pin
    // the lock across the inner handler chain (which itself takes
    // the read or write guard).
    let decision = {
        let fixture = state.shared.fixture.read().expect("fixture lock poisoned");
        crate::oauth::check_bearer(&fixture, &state.shared.token_store, req.headers())
    };
    match decision {
        BearerDecision::Allow => next.run(req).await,
        BearerDecision::Deny(_reason) => Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("WWW-Authenticate", "Bearer")
            .body(Body::empty())
            .expect("static 401 response builds"),
    }
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
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<crate::connection_id::ConnInfo>(),
    )
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
const MKCALENDAR: &str = "MKCALENDAR";

/// Resolve the requesting principal from an `Authorization: Basic`
/// header. Returns `Some(account_id)` when the decoded username matches
/// a declared fixture account by id, else `None`. Multi-account CalDAV
/// fixtures rely on this to route the bootstrap PROPFIND on `/` to the
/// authenticating principal rather than always to the fixture's
/// primary - real CalDAV servers identify the user from the credential.
fn account_from_basic_auth(
    fixture: &crate::fixture::Fixture,
    headers: &HeaderMap,
) -> Option<String> {
    let value = headers.get("authorization")?.to_str().ok()?;
    let b64 = value
        .strip_prefix("Basic ")
        .or_else(|| value.strip_prefix("basic "))?;
    let decoded = crate::imap::sasl_decode_b64(b64.trim())?;
    let creds = std::str::from_utf8(&decoded).ok()?;
    let (user, _pass) = creds.split_once(':')?;
    fixture.account(user).map(|a| a.id.clone())
}

async fn dispatch(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
    body: Bytes,
) -> Response {
    let path = uri.path().to_string();

    // Record into the cross-protocol request log so
    // `GET /test/requests` covers CalDAV the same way it covers
    // Graph + Gmail. Query strings are stripped from `command` and
    // surfaced in `detail.query` to match the existing convention.
    state.shared.request_log.record_with_conn(
        "caldav",
        format!("{method} {path}"),
        json!({ "query": uri.query() }),
        connection_id,
    );
    state.shared.latency.sleep_for("caldav").await;

    let m = method.as_str();
    if m == PROPFIND {
        return handle_propfind(&state, &path, &headers, &body).await;
    }
    if m == REPORT {
        return handle_report(&state, &path, &headers, &body).await;
    }
    if m == MKCALENDAR {
        return handle_mkcalendar(&state, &path, &body).await;
    }
    match method {
        Method::GET => handle_get(&state, &path, &headers).await,
        Method::PUT => handle_put(&state, &path, &headers, &body).await,
        Method::DELETE => handle_delete(&state, &path, &headers).await,
        Method::POST => handle_post(&state, &path, &headers, &body, connection_id).await,
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

    let auth_user = account_from_basic_auth(&fixture, headers);
    match parse_path(&fixture, path) {
        ResourcePath::Root => propfind_root(&fixture, auth_user.as_deref(), body_str),
        ResourcePath::WellKnown => propfind_root(&fixture, auth_user.as_deref(), body_str),
        ResourcePath::Principal { user } => propfind_principal(&fixture, &user, body_str),
        ResourcePath::CalendarHome { user } => {
            propfind_calendar_home(&fixture, &user, body_str, depth)
        }
        ResourcePath::Calendar { user, calendar_id } => {
            propfind_calendar(&fixture, &user, &calendar_id, body_str, depth)
        }
        ResourcePath::Event {
            user,
            calendar_id,
            event_id,
        } => propfind_event(&fixture, &user, &calendar_id, &event_id, body_str),
        // A PROPFIND on the outbox collection itself is not part of the
        // RSVP flow (bifrost only POSTs there); v0 treats it as absent.
        ResourcePath::ScheduleOutbox { .. } => not_found(path),
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
    Principal {
        user: String,
    },
    CalendarHome {
        user: String,
    },
    Calendar {
        user: String,
        calendar_id: String,
    },
    Event {
        user: String,
        calendar_id: String,
        event_id: String,
    },
    /// RFC 6638 scheduling outbox (`/calendars/{user}/outbox/`).
    /// bifrost POSTs the iTIP REPLY here during an RSVP.
    ScheduleOutbox {
        user: String,
    },
    Unknown,
}

fn parse_path(fixture: &crate::fixture::Fixture, path: &str) -> ResourcePath {
    if path == "/" {
        return ResourcePath::Root;
    }
    if path == "/.well-known/caldav" || path == "/.well-known/caldav/" {
        return ResourcePath::WellKnown;
    }
    // Reject paths with empty segments (`//`, `/calendars//cal/`).
    // Pre-fix the segment-filter swallowed them, so the request log
    // and per-resource caches couldn't tell `/calendars/u/cal/`
    // apart from `/calendars//u///cal//`. This is also defence-in-
    // depth against any future filesystem-touching code path.
    let trimmed = path.trim_end_matches('/');
    if trimmed.split('/').skip(1).any(str::is_empty) {
        return ResourcePath::Unknown;
    }
    // Percent-decode each segment so calendar / event ids that
    // round-trip through real clients carrying `%`-encoded
    // characters (Apple Calendar happily emits them) match the
    // fixture-declared id. Decode failures fall back to the raw
    // bytes so a deliberately-malformed segment still routes to
    // Unknown without panicking.
    let segments: Vec<String> = trimmed
        .split('/')
        .filter(|s| !s.is_empty())
        .map(percent_decode)
        .collect();
    let segs: Vec<&str> = segments.iter().map(String::as_str).collect();
    // The `{user}` segment in `/principals/{user}/` and
    // `/calendars/{user}/...` names a declared account by id. Multi-
    // account fixtures hand each principal its own URL space; pre-fix
    // we only accepted the primary id, which left secondary
    // principals 404 even when the fixture declared them.
    match segs.as_slice() {
        ["principals", u] if fixture.account(u).is_some() => ResourcePath::Principal {
            user: (*u).to_string(),
        },
        ["calendars", u] if fixture.account(u).is_some() => ResourcePath::CalendarHome {
            user: (*u).to_string(),
        },
        // The scheduling outbox lives at `/calendars/{user}/outbox/`.
        // Match it before the generic calendar arm so a POST there
        // routes to the schedule-reply handler rather than being read
        // as a calendar named "outbox" (no fixture declares one).
        // Gated on the same fixture switch that advertises it. A
        // scheduling-less server must not merely stop ADVERTISING the
        // outbox while still honouring a POST to it - a consumer that
        // guessed the conventional URL would then succeed against a
        // server that reported no scheduling, and the fixture would be
        // staging a shape no real server has.
        ["calendars", u, "outbox"] if fixture.caldav.scheduling && fixture.account(u).is_some() => {
            ResourcePath::ScheduleOutbox {
                user: (*u).to_string(),
            }
        }
        ["calendars", u, cal] if fixture.account(u).is_some() => ResourcePath::Calendar {
            user: (*u).to_string(),
            calendar_id: (*cal).to_string(),
        },
        ["calendars", u, cal, event] if fixture.account(u).is_some() => {
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

/// Decode percent-escapes (`%XX`) in a single URL path segment.
/// Slashes inside encoded form (`%2F`) decode to literal `/`,
/// which is the right answer for a single segment - the slash
/// split that produced this segment already happened. Malformed
/// escapes (`%X`, `%`, non-hex) pass through verbatim so a
/// deliberately-broken path still routes deterministically.
fn percent_decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            )
        {
            // hi/lo are <= 0xF; the OR fits in u8.
            out.push(u8::try_from((hi << 4) | lo).expect("hex pair fits in u8"));
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|e| {
        // Non-UTF-8 percent-decoded bytes: fall back to the raw
        // input. A real client wouldn't send these for an
        // ASCII-id fixture; defensive only.
        String::from_utf8_lossy(e.as_bytes()).into_owned()
    })
}

/// PROPFIND on `/` (or `/.well-known/caldav`): return the
/// `current-user-principal` URL. Some clients ask for additional
/// properties on the root resource; we honour
/// `current-user-principal` and silently drop anything else (a
/// truly conformant server would emit them under a 404 propstat).
fn propfind_root(
    fixture: &crate::fixture::Fixture,
    auth_user: Option<&str>,
    body: &str,
) -> Response {
    let requested = xml::requested_props(body);
    // Multi-account fixtures want the bootstrap PROPFIND to surface
    // the authenticating principal, not the fixture-level primary -
    // otherwise a secondary account would discover the primary's
    // principal URL and import the primary's calendars. Fall back to
    // the primary id when no usable Basic Auth header is present so
    // single-account fixtures (which never send credentials in tests)
    // keep working.
    let primary_id = fixture.primary_account().id.clone();
    let user = auth_user.unwrap_or(&primary_id);
    let principal_url = format!("/principals/{user}/");
    let mut props = String::new();
    if requested.contains("current-user-principal") {
        props.push_str(&format!(
            "<D:current-user-principal><D:href>{}</D:href></D:current-user-principal>",
            xml::escape(&principal_url),
        ));
    }
    if requested.contains("principal-URL") {
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
    let requested = xml::requested_props(body);
    let home_url = format!("/calendars/{user}/");
    let mut props = String::new();
    if requested.contains("calendar-home-set") {
        props.push_str(&format!(
            "<C:calendar-home-set><D:href>{}</D:href></C:calendar-home-set>",
            xml::escape(&home_url),
        ));
    }
    if requested.contains("current-user-principal") {
        props.push_str(&format!(
            "<D:current-user-principal><D:href>/principals/{}/</D:href></D:current-user-principal>",
            xml::escape(user),
        ));
    }
    if requested.contains("displayname") {
        props.push_str(&format!(
            "<D:displayname>{}</D:displayname>",
            xml::escape(&fixture.primary_account().name),
        ));
    }
    // RFC 6638 scheduling discovery. `calendar-user-address-set` is the
    // principal's own address (a `mailto:` of the account's email, which
    // is `account.name` by the fixture convention); `schedule-outbox-URL`
    // points at the collection bifrost POSTs the iTIP REPLY to during an
    // RSVP.
    //
    // A client may DERIVE its RSVP capability from these rather than
    // hardcoding it (bifrost's `scheduling_available` requires both
    // non-empty), so what this block emits decides whether the
    // consumer reports RSVP as supported at all. `[caldav] scheduling
    // = false` omits both, which is how a fixture stages a plain
    // RFC 4791 store - the FALSE branch of that derivation, and the
    // case the derivation exists for. Omission is deliberate over
    // emitting an empty element: a 207 that carries the property with
    // no href is a half-answer real servers do not give, and it would
    // conflate "no scheduling" with "scheduling, address unknown".
    if fixture.caldav.scheduling {
        if requested.contains("calendar-user-address-set") {
            props.push_str(&format!(
                "<C:calendar-user-address-set><D:href>mailto:{}</D:href></C:calendar-user-address-set>",
                xml::escape(&principal_email(fixture, user)),
            ));
        }
        if requested.contains("schedule-outbox-URL") {
            props.push_str(&format!(
                "<C:schedule-outbox-URL><D:href>/calendars/{user}/outbox/</D:href></C:schedule-outbox-URL>",
            ));
        }
    }
    multistatus(wrap_responses(&[Response207 {
        href: &format!("/principals/{user}/"),
        ok_props: &props,
    }]))
}

/// The principal's own email address for `calendar-user-address-set`.
/// The fixture convention is that `account.name` is the account's
/// email (the Graph `mail` projection derives from it too); fall back
/// to a deterministic `<id>@saehrimnir.test` when the account is
/// unknown or its name is not email-shaped.
fn principal_email(fixture: &crate::fixture::Fixture, user: &str) -> String {
    match fixture.account(user) {
        Some(a) if a.name.contains('@') => a.name.clone(),
        _ => format!("{user}@saehrimnir.test"),
    }
}

/// PROPFIND on `/calendars/{user}/`. Depth=0 returns just the home
/// collection; Depth=1 lists every calendar plus the home. The
/// listing is account-scoped to `user`'s principal so a multi-account
/// fixture's secondary principal does not surface the primary's
/// calendars (or vice versa).
fn propfind_calendar_home(
    fixture: &crate::fixture::Fixture,
    user: &str,
    body: &str,
    depth: u8,
) -> Response {
    let requested = xml::requested_props(body);
    let home_href = format!("/calendars/{user}/");
    let home_props = home_collection_props(&requested);

    let mut entries = vec![Response207 {
        href: &home_href,
        ok_props: &home_props,
    }];
    let mut per_calendar_hrefs = Vec::new();
    let mut per_calendar_props = Vec::new();
    if depth >= 1 {
        for cal in fixture.calendars_for(user) {
            per_calendar_hrefs.push(format!("/calendars/{user}/{}/", cal.id));
            per_calendar_props.push(calendar_props(fixture, cal, &requested));
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
/// the calendar's own props; Depth=1 also lists each event resource.
/// The calendar must belong to the `{user}` principal - a request
/// for another account's calendar through this principal's URL
/// returns 404 so principals stay isolated.
fn propfind_calendar(
    fixture: &crate::fixture::Fixture,
    user: &str,
    calendar_id: &str,
    body: &str,
    depth: u8,
) -> Response {
    let requested = xml::requested_props(body);
    let cal = match fixture.calendars_for(user).find(|c| c.id == calendar_id) {
        Some(c) => c,
        None => return not_found(&format!("/calendars/{user}/{calendar_id}/")),
    };
    let cal_href = format!("/calendars/{user}/{calendar_id}/");
    let cal_props = calendar_props(fixture, cal, &requested);

    let mut entries = vec![Response207 {
        href: &cal_href,
        ok_props: &cal_props,
    }];
    let mut event_hrefs = Vec::new();
    let mut event_props = Vec::new();
    if depth >= 1 {
        for ev in fixture
            .events
            .iter()
            .filter(|e| e.calendar_id == calendar_id)
        {
            event_hrefs.push(format!("/calendars/{user}/{calendar_id}/{}.ics", ev.id));
            event_props.push(event_resource_props(fixture, ev, &requested));
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
/// prefers `REPORT calendar-multiget` for the bulk fetch path. The
/// event's calendar must belong to `{user}` - a cross-account lookup
/// through this principal's URL 404s.
fn propfind_event(
    fixture: &crate::fixture::Fixture,
    user: &str,
    calendar_id: &str,
    event_id: &str,
    body: &str,
) -> Response {
    let requested = xml::requested_props(body);
    let calendar_in_account = fixture.calendars_for(user).any(|c| c.id == calendar_id);
    if !calendar_in_account {
        return not_found(&format!("/calendars/{user}/{calendar_id}/{event_id}.ics"));
    }
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
    let props = event_resource_props(fixture, ev, &requested);
    multistatus(wrap_responses(&[Response207 {
        href: &href,
        ok_props: &props,
    }]))
}

// ── Property serialisation helpers ──────────────────────────────────

/// Properties for the calendar-home collection itself.
fn home_collection_props(requested: &std::collections::HashSet<String>) -> String {
    let mut props = String::new();
    if requested.contains("resourcetype") {
        props.push_str("<D:resourcetype><D:collection/></D:resourcetype>");
    }
    if requested.contains("displayname") {
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
    requested: &std::collections::HashSet<String>,
) -> String {
    let mut props = String::new();
    if requested.contains("resourcetype") {
        props.push_str("<D:resourcetype><D:collection/><C:calendar/></D:resourcetype>");
    }
    if requested.contains("displayname") {
        props.push_str(&format!(
            "<D:displayname>{}</D:displayname>",
            xml::escape(&cal.name),
        ));
    }
    if requested.contains("getctag") {
        props.push_str(&format!(
            "<CS:getctag>{}</CS:getctag>",
            xml::escape(&calendar_ctag(fixture, &cal.id)),
        ));
    }
    if let Some(color) = &cal.color
        && requested.contains("calendar-color")
    {
        props.push_str(&format!(
            "<IC:calendar-color>{}</IC:calendar-color>",
            xml::escape(color),
        ));
    }
    if requested.contains("current-user-privilege-set") {
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
    if requested.contains("supported-calendar-component-set") {
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
    requested: &std::collections::HashSet<String>,
) -> String {
    let mut props = String::new();
    if requested.contains("resourcetype") {
        // Resources (as opposed to collections) emit an empty
        // `<D:resourcetype/>`. Ratatoskr's parser distinguishes
        // collections from resources by the presence of children
        // here.
        props.push_str("<D:resourcetype/>");
    }
    if requested.contains("getetag") {
        props.push_str(&format!(
            "<D:getetag>{}</D:getetag>",
            xml::escape(&event_etag(fixture, &ev.id)),
        ));
    }
    if requested.contains("getcontenttype") {
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

/// Deterministic CTag for a calendar. Walks the change_log to find
/// the most recent transition that touched any event in this
/// calendar; the CTag advances only when something inside the
/// calendar actually changed. Pre-fix the CTag tracked
/// `fixture.state` directly, which advanced on every JMAP /
/// IMAP / Graph mutation regardless - so a real CalDAV client
/// would re-walk every calendar after every unrelated mailbox
/// change. Falls back to the change-log seed when no transition
/// has touched the calendar (the post-load baseline).
fn calendar_ctag(fixture: &crate::fixture::Fixture, calendar_id: &str) -> String {
    let last = last_state_touching_calendar(fixture, calendar_id);
    format!("{last}/{calendar_id}")
}

/// Deterministic ETag for an event. Walks the change_log to find
/// the most recent transition that touched this specific event
/// (created / updated / destroyed). Same scoping rationale as
/// `calendar_ctag`. Ratatoskr only checks for byte equality so the
/// format is free to evolve.
fn event_etag(fixture: &crate::fixture::Fixture, event_id: &str) -> String {
    let last = last_state_touching_event(fixture, event_id);
    format!("\"{last}/{event_id}\"")
}

/// Outcome of an If-Match precondition check.
#[derive(Debug, PartialEq, Eq)]
enum IfMatchOutcome {
    /// At least one tag in the header matched the current resource
    /// (or a wildcard against an existing resource).
    Match,
    /// The header was a wildcard but the resource does not exist.
    /// PUT distinguishes this case (it 412s); DELETE always has
    /// the resource (it 404s before getting here) so the
    /// distinction is mostly for the PUT path.
    WildcardNoResource,
    /// No tag matched.
    NoMatch,
}

/// Evaluate an `If-Match` header value against the current ETag.
/// RFC 7232 §3.1: the value is a comma-separated list of opaque
/// tags, optionally prefixed with `W/` (weak validators), each
/// surrounded by double quotes; or the wildcard `*`. Real clients
/// vary on the exact framing - some quote the wildcard, some emit
/// `W/"..."`, some emit a single bare token. This helper tolerates
/// the lot. The bare-quote-strip on both sides keeps the
/// comparison body-equality even when one side is quoted and the
/// other isn't.
fn if_match_matches(header_value: &str, current_etag: Option<&str>) -> IfMatchOutcome {
    let cur_unquoted = current_etag.map(unquote_etag);
    for tag in header_value.split(',') {
        let trimmed = tag.trim();
        let unweak = trimmed.strip_prefix("W/").unwrap_or(trimmed).trim();
        let unquoted = unquote_etag(unweak);
        if unquoted == "*" {
            return if current_etag.is_some() {
                IfMatchOutcome::Match
            } else {
                IfMatchOutcome::WildcardNoResource
            };
        }
        if let Some(cur) = cur_unquoted
            && unquoted == cur
        {
            return IfMatchOutcome::Match;
        }
    }
    IfMatchOutcome::NoMatch
}

/// Strip surrounding double quotes from an ETag, leaving the
/// opaque body. Idempotent: an unquoted input passes through.
fn unquote_etag(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
}

/// Most recent `to_state` from any transition that listed
/// `event_id` in its created / updated / destroyed sets. Returns
/// the change-log seed when no transition has touched the event,
/// which is the right answer for a freshly-loaded fixture.
fn last_state_touching_event(fixture: &crate::fixture::Fixture, event_id: &str) -> String {
    // Resolve the event's owning account (event -> calendar -> account)
    // so we walk the right per-account log. A live event is required to
    // derive its ETag, so the lookup succeeds on every real call site
    // (GET / PUT / DELETE precondition); fall back to the seed if not.
    let Some(account_id) = fixture
        .events
        .iter()
        .find(|e| e.id == event_id)
        .and_then(|e| fixture.calendars.iter().find(|c| c.id == e.calendar_id))
        .map(|c| c.account_id.as_str())
    else {
        return fixture.change_log_seed().to_string();
    };
    for t in fixture.change_log_transitions_for(account_id).rev() {
        if t.event_created.iter().any(|id| id == event_id)
            || t.event_updated.iter().any(|id| id == event_id)
            || t.event_destroyed.iter().any(|id| id == event_id)
        {
            return t.to_state.clone();
        }
    }
    fixture.change_log_seed().to_string()
}

/// Most recent `to_state` from any transition that touched any
/// event in `calendar_id`. For created / updated, the event is
/// still in the fixture so we look up the parent there. For
/// destroyed, the parent comes from `event_destroyed_parents`
/// (parallel to `event_destroyed`).
fn last_state_touching_calendar(fixture: &crate::fixture::Fixture, calendar_id: &str) -> String {
    // The calendar's own account owns the transitions that touch it.
    let Some(account_id) = fixture
        .calendars
        .iter()
        .find(|c| c.id == calendar_id)
        .map(|c| c.account_id.as_str())
    else {
        return fixture.change_log_seed().to_string();
    };
    for t in fixture.change_log_transitions_for(account_id).rev() {
        let touches_live = t
            .event_created
            .iter()
            .chain(t.event_updated.iter())
            .any(|id| {
                fixture
                    .events
                    .iter()
                    .any(|e| &e.id == id && e.calendar_id == calendar_id)
            });
        if touches_live {
            return t.to_state.clone();
        }
        if t.event_destroyed_parents.iter().any(|p| p == calendar_id) {
            return t.to_state.clone();
        }
    }
    fixture.change_log_seed().to_string()
}

async fn handle_report(
    state: &AppState,
    path: &str,
    _headers: &HeaderMap,
    body: &[u8],
) -> Response {
    let body_str = match body_to_str(body) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let fixture = state.shared.fixture.read().expect("fixture lock poisoned");
    let (user, calendar_id) = match parse_path(&fixture, path) {
        ResourcePath::Calendar { user, calendar_id } => (user, calendar_id),
        // REPORT can also target the home collection (rare, but
        // some clients use this for cross-calendar queries). v0
        // treats it as a 404 since we don't support cross-calendar
        // filters.
        _ => return not_found(path),
    };
    if !fixture.calendars_for(&user).any(|c| c.id == calendar_id) {
        return not_found(path);
    }

    // Two report kinds: calendar-multiget (explicit href list) and
    // calendar-query (server-side filter, optionally narrowing by
    // time range). We tell them apart by which root element is
    // present in the body.
    if xml::body_requests_prop(body_str, "calendar-multiget") {
        return report_multiget(&fixture, &user, &calendar_id, body_str);
    }
    if xml::body_requests_prop(body_str, "calendar-query") {
        return report_calendar_query(&fixture, &user, &calendar_id, body_str);
    }
    bad_request("REPORT body must be calendar-multiget or calendar-query")
}

/// `calendar-multiget`: client lists `<href>` elements naming the
/// resources it wants. We return one `<response>` per href, each
/// carrying `getetag` + `<C:calendar-data>` (the iCalendar body).
/// Hrefs not present in the fixture get a 404 propstat so the
/// client can prune stale entries.
fn report_multiget(
    fixture: &crate::fixture::Fixture,
    user: &str,
    calendar_id: &str,
    body: &str,
) -> Response {
    let hrefs = xml::collect_hrefs(body);
    let mut out = String::new();
    out.push_str(r#"<?xml version="1.0" encoding="utf-8"?>"#);
    out.push('\n');
    out.push_str(r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">"#);

    for href in hrefs {
        let resolved = match parse_path(fixture, &href) {
            ResourcePath::Event {
                user: u,
                calendar_id: c,
                event_id,
            } if u == user && c == calendar_id => Some(event_id),
            _ => None,
        };
        let event = resolved.as_deref().and_then(|id| {
            fixture
                .events
                .iter()
                .find(|e| e.id == id && e.calendar_id == calendar_id)
        });
        match event {
            Some(ev) => {
                let etag = event_etag(fixture, &ev.id);
                let ical = event_body(ev);
                out.push_str("<D:response>");
                out.push_str(&format!("<D:href>{}</D:href>", xml::escape(&href)));
                out.push_str("<D:propstat><D:prop>");
                out.push_str(&format!("<D:getetag>{}</D:getetag>", xml::escape(&etag)));
                out.push_str(&format!(
                    "<C:calendar-data>{}</C:calendar-data>",
                    xml::escape(&ical),
                ));
                out.push_str("</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>");
                out.push_str("</D:response>");
            }
            None => {
                // Not in this calendar: emit 404 so the client knows
                // to drop the local copy.
                out.push_str("<D:response>");
                out.push_str(&format!("<D:href>{}</D:href>", xml::escape(&href)));
                out.push_str("<D:status>HTTP/1.1 404 Not Found</D:status>");
                out.push_str("</D:response>");
            }
        }
    }
    out.push_str("</D:multistatus>");
    multistatus(out)
}

/// `calendar-query`: server-side filter. v0 honours
/// `<C:time-range start="..." end="..."/>` inside the nested
/// `<C:comp-filter name="VEVENT">`. Events whose [start, end)
/// overlaps the query range are returned with their ical body.
/// Other filter shapes (text-match, prop-filter on a property) get
/// the unfiltered list - ratatoskr only sends the time-range form.
fn report_calendar_query(
    fixture: &crate::fixture::Fixture,
    user: &str,
    calendar_id: &str,
    body: &str,
) -> Response {
    let range = parse_time_range(body);
    let mut out = String::new();
    out.push_str(r#"<?xml version="1.0" encoding="utf-8"?>"#);
    out.push('\n');
    out.push_str(r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">"#);

    // Empty-207 mode: a calendar flagged `caldav_empty_report` returns
    // an empty-but-successful multistatus (zero `<D:response>` rows)
    // regardless of the events it holds, exercising the consumer's
    // empty-pull deletion guard (a populated previous snapshot diffed
    // against an empty current one must not mass-delete).
    let empty_mode = fixture
        .calendars_for(user)
        .find(|c| c.id == calendar_id)
        .is_some_and(|c| c.caldav_empty_report);

    for ev in fixture
        .events
        .iter()
        .filter(|e| !empty_mode && e.calendar_id == calendar_id)
    {
        if let Some((start, end)) = range
            && !overlaps(ev, start, end)
        {
            continue;
        }
        let href = format!("/calendars/{user}/{calendar_id}/{}.ics", ev.id);
        let etag = event_etag(fixture, &ev.id);
        let ical = event_body(ev);
        out.push_str("<D:response>");
        out.push_str(&format!("<D:href>{}</D:href>", xml::escape(&href)));
        out.push_str("<D:propstat><D:prop>");
        out.push_str(&format!("<D:getetag>{}</D:getetag>", xml::escape(&etag)));
        out.push_str(&format!(
            "<C:calendar-data>{}</C:calendar-data>",
            xml::escape(&ical),
        ));
        out.push_str("</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>");
        out.push_str("</D:response>");
    }
    out.push_str("</D:multistatus>");
    multistatus(out)
}

/// Parse a `<C:time-range start="..." end="..."/>` element from a
/// REPORT body. Returns `(start, end)` as parsed UTC datetimes,
/// or None if the element isn't present (or one of the attributes
/// fails to parse).
fn parse_time_range(
    body: &str,
) -> Option<(jiff::Timestamp, jiff::Timestamp)> {
    let idx = body.find("time-range")?;
    let tail = &body[idx..];
    let start = extract_attr(tail, "start").and_then(|s| ical::parse_dt(&s))?;
    let end = extract_attr(tail, "end").and_then(|s| ical::parse_dt(&s))?;
    Some((start, end))
}

/// Pull a quoted attribute value out of a tag. Tolerant of either
/// single or double quotes; CalDAV bodies use double quotes.
fn extract_attr(s: &str, name: &str) -> Option<String> {
    let key_eq = format!("{name}=\"");
    if let Some(start) = s.find(&key_eq) {
        let after = &s[start + key_eq.len()..];
        if let Some(close) = after.find('"') {
            return Some(after[..close].to_string());
        }
    }
    let key_eq = format!("{name}='");
    if let Some(start) = s.find(&key_eq) {
        let after = &s[start + key_eq.len()..];
        if let Some(close) = after.find('\'') {
            return Some(after[..close].to_string());
        }
    }
    None
}

/// The iCalendar body a CalDAV read serves for `ev`: the fixture's
/// verbatim `raw_ical` override when present (a multi-VEVENT resource,
/// or a deliberately malformed body staging a per-resource parse
/// failure), otherwise the structured projection through
/// `ical::event_to_ical`.
fn event_body(ev: &crate::fixture::Event) -> String {
    ev.raw_ical
        .clone()
        .unwrap_or_else(|| ical::event_to_ical(ev))
}

/// Recurrence-aware time-range overlap test. A non-recurring event
/// overlaps `[range_start, range_end)` iff `ev.start < range_end &&
/// ev.end > range_start`; a recurring event (one carrying a structured
/// `recurrence_rule`) matches iff its DTSTART falls before the window
/// closes and its RRULE `UNTIL` (when present) does not end the series
/// before the window opens. See [`crate::recurrence::event_matches_range`].
fn overlaps(
    ev: &crate::fixture::Event,
    range_start: jiff::Timestamp,
    range_end: jiff::Timestamp,
) -> bool {
    crate::recurrence::event_matches_range(
        ev.start,
        ev.end,
        ev.recurrence_rule.as_deref(),
        Some(range_start),
        Some(range_end),
    )
}

async fn handle_get(state: &AppState, path: &str, _headers: &HeaderMap) -> Response {
    let fixture = state.shared.fixture.read().expect("fixture lock poisoned");
    match parse_path(&fixture, path) {
        ResourcePath::Event {
            user,
            calendar_id,
            event_id,
        } => match fixture
            .events
            .iter()
            .find(|e| e.id == event_id && e.calendar_id == calendar_id && e.account_id == user)
        {
            Some(ev) => {
                let body = event_body(ev);
                let etag = event_etag(&fixture, &ev.id);
                Response::builder()
                    .status(StatusCode::OK)
                    .header(
                        axum::http::header::CONTENT_TYPE,
                        HeaderValue::from_static("text/calendar; charset=utf-8"),
                    )
                    .header("ETag", etag)
                    .body(Body::from(body))
                    .expect("static GET response builds")
            }
            None => not_found(path),
        },
        _ => not_found(path),
    }
}

async fn handle_put(state: &AppState, path: &str, headers: &HeaderMap, body: &[u8]) -> Response {
    let body_str = match body_to_str(body) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let if_match = headers
        .get("if-match")
        .and_then(|v| v.to_str().ok())
        .map(str::trim);

    // Read-only resolution first so we can early-return without
    // taking the write guard. The fixture image stays read-locked
    // for the duration of the parse + validate; we drop it before
    // re-acquiring as a write.
    let (user, calendar_id, event_id, parsed) = {
        let fixture = state.shared.fixture.read().expect("fixture lock poisoned");
        let (user, cal, event_id) = match parse_path(&fixture, path) {
            ResourcePath::Event {
                user,
                calendar_id,
                event_id,
            } => (user, calendar_id, event_id),
            _ => return not_found(path),
        };
        if !fixture.calendars_for(&user).any(|c| c.id == cal) {
            return not_found(path);
        }
        let parsed = match ical::parse_vevent(body_str) {
            Ok(p) => p,
            Err(msg) => return bad_request(&format!("PUT body: {msg}")),
        };
        // The URL identifies the resource; the body's UID must
        // either be absent (caller expects us to use the URL id) or
        // match the URL's id exactly. A mismatch would orphan the
        // URL ↔ id mapping (we'd create / update an event whose id
        // is the body's UID, leaving the URL pointing at nothing or
        // at a different event entirely).
        if let Some(body_uid) = parsed.uid.as_deref()
            && body_uid != event_id
        {
            return bad_request(&format!(
                "PUT body UID {body_uid:?} does not match URL event id {event_id:?}"
            ));
        }
        (user, cal, event_id, parsed)
    };

    // Now acquire write. Re-validate the calendar (it might have
    // been destroyed between the read and write guard) and
    // honour If-Match against the current event ETag (or the
    // wildcard `*` against existence). Calendar must still belong
    // to `user`'s principal after any concurrent mutation.
    let mut fixture = state.shared.fixture.write().expect("fixture lock poisoned");
    if !fixture.calendars_for(&user).any(|c| c.id == calendar_id) {
        return not_found(path);
    }
    let existing_idx = fixture
        .events
        .iter()
        .position(|e| e.id == event_id && e.calendar_id == calendar_id);
    if let Some(im) = if_match {
        let current = existing_idx.map(|i| event_etag(&fixture, &fixture.events[i].id));
        match if_match_matches(im, current.as_deref()) {
            IfMatchOutcome::Match => {}
            IfMatchOutcome::WildcardNoResource => {
                return precondition_failed("If-Match: * but event does not exist");
            }
            IfMatchOutcome::NoMatch => {
                return precondition_failed("If-Match did not match the current ETag");
            }
        }
    }

    // Build the new Event from the parsed VEVENT. Preserve any
    // calendar-id-shaped value the client sent under UID, but fall
    // back to the path's event_id so a malformed body still routes
    // to the URL.
    let id = parsed.uid.unwrap_or_else(|| event_id.clone());
    let start = match parsed.start {
        Some(s) => s,
        None => return bad_request("VEVENT missing DTSTART"),
    };
    let end = match parsed.end {
        Some(s) => s,
        // Real CalDAV allows DTEND to be omitted (then DURATION
        // applies, or it's an instantaneous event). v0 fixtures
        // always carry both; default DTEND to `start + 1h` to
        // match `synthesize_event_dto`'s behaviour in ratatoskr.
        None => start + jiff::SignedDuration::from_hours(1),
    };
    // Derive the event's account from the principal that owns the
    // URL. The calendar exists in this principal's space (we already
    // checked `calendars_for(&user)` above), so `user` is the
    // authoritative source.
    let new_event = crate::fixture::Event {
        id: id.clone(),
        account_id: user.clone(),
        calendar_id: calendar_id.clone(),
        subject: parsed.summary.unwrap_or_default(),
        body_preview: None,
        body_text: parsed.description,
        start,
        end,
        location: parsed.location,
        organizer: parsed.organizer,
        attendees: parsed.attendees,
        is_all_day: parsed.is_all_day,
        recurrence_rule: parsed.recurrence_rule,
        recurrence_exdates: parsed.recurrence_exdates,
        // The v0 PUT parser (`ical::parse_vevent`) does not extract
        // per-event timezone, VALARM reminders, or preserve a raw
        // multi-VEVENT body, so a PUT-written event drops those.
        // Reads of the structured fields still round-trip; the
        // static `[[event]]` shape is the authoring path for the
        // TZID / reminder / multi-VEVENT read affordances.
        time_zone: None,
        reminders: Vec::new(),
        raw_ical: None,
    };
    let was_create = existing_idx.is_none();
    fixture.mutate(|f| match existing_idx {
        Some(idx) => {
            f.events[idx] = new_event.clone();
            crate::fixture::MutationDiff {
                event_updated: vec![id.clone()],
                ..Default::default()
            }
        }
        None => {
            f.events.push(new_event.clone());
            crate::fixture::MutationDiff {
                event_created: vec![id.clone()],
                ..Default::default()
            }
        }
    });
    let etag = event_etag(&fixture, &id);
    let status = if was_create {
        StatusCode::CREATED
    } else {
        StatusCode::NO_CONTENT
    };
    Response::builder()
        .status(status)
        .header("ETag", etag)
        .body(Body::empty())
        .expect("static PUT response builds")
}

async fn handle_delete(state: &AppState, path: &str, headers: &HeaderMap) -> Response {
    let if_match = headers
        .get("if-match")
        .and_then(|v| v.to_str().ok())
        .map(str::trim);
    let mut fixture = state.shared.fixture.write().expect("fixture lock poisoned");
    let (user, calendar_id, event_id) = match parse_path(&fixture, path) {
        ResourcePath::Event {
            user,
            calendar_id,
            event_id,
        } => (user, calendar_id, event_id),
        // DELETE on a calendar-collection path (RFC 4791 permits it):
        // unlist the calendar so a later discovery PROPFIND on the
        // calendar-home-set no longer enumerates it - the "hide the
        // stale calendar" instrument. Symmetric with MKCALENDAR, which
        // restores it. Events under the calendar are left intact so a
        // later MKCALENDAR restore surfaces them again.
        ResourcePath::Calendar { user, calendar_id } => {
            return delete_calendar(&mut fixture, path, &user, &calendar_id);
        }
        _ => return not_found(path),
    };
    // Cross-account lookups under this principal's URL 404.
    if !fixture.calendars_for(&user).any(|c| c.id == calendar_id) {
        return not_found(path);
    }
    let idx = fixture
        .events
        .iter()
        .position(|e| e.id == event_id && e.calendar_id == calendar_id);
    let Some(idx) = idx else {
        return not_found(path);
    };
    if let Some(im) = if_match {
        let current = event_etag(&fixture, &fixture.events[idx].id);
        match if_match_matches(im, Some(&current)) {
            IfMatchOutcome::Match => {}
            // DELETE only fires after `idx` resolved, so the
            // wildcard branch can't take WildcardNoResource here -
            // but we still funnel through the same helper to keep
            // the parsing centralised.
            IfMatchOutcome::WildcardNoResource | IfMatchOutcome::NoMatch => {
                return precondition_failed("If-Match did not match the current ETag");
            }
        }
    }
    let id = fixture.events[idx].id.clone();
    let parent = fixture.events[idx].calendar_id.clone();
    fixture.mutate(|f| {
        f.events.remove(idx);
        crate::fixture::MutationDiff {
            event_destroyed: vec![id.clone()],
            event_destroyed_parents: vec![parent.clone()],
            ..Default::default()
        }
    });
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .expect("static DELETE response builds")
}

/// Remove a calendar collection from the fixture for the resolved
/// principal. Mirrors the account-scoping of the event delete: a
/// cross-principal reference (the calendar exists but under a
/// different account) 404s, and an unknown calendar 404s. On success
/// it records a `calendar_destroyed` transition so the same
/// `Fixture::mutate` change-log that MKCALENDAR / event mutations feed
/// lights up - Graph `/v1.0/me/calendars` (a live read) stops listing
/// the calendar and JMAP `Calendar/changes` walks the destroy.
///
/// If-Match is not honoured on the collection: calendar collections
/// carry a CTag, not an ETag, and the only existing collection-aware
/// verb (MKCALENDAR) takes no precondition either, so a plain DELETE
/// keeps parity. Returns 204 on success.
fn delete_calendar(
    fixture: &mut crate::fixture::Fixture,
    path: &str,
    user: &str,
    calendar_id: &str,
) -> Response {
    // Cross-account lookups under this principal's URL 404, matching
    // the event delete's isolation guarantee.
    if !fixture.calendars_for(user).any(|c| c.id == calendar_id) {
        return not_found(path);
    }
    let Some(idx) = fixture.calendars.iter().position(|c| c.id == calendar_id) else {
        return not_found(path);
    };
    // Capture the owning account before removal. The change-log split
    // resolves a destroyed calendar's account from the parallel
    // `calendar_destroyed_accounts` vector (the calendar is gone from
    // the fixture by split time, so a live lookup would miss it).
    let account_id = fixture.calendars[idx].account_id.clone();
    let cal_id = calendar_id.to_string();
    fixture.mutate(|f| {
        f.calendars.remove(idx);
        crate::fixture::MutationDiff {
            calendar_destroyed: vec![cal_id.clone()],
            calendar_destroyed_accounts: vec![account_id.clone()],
            ..Default::default()
        }
    });
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .expect("static calendar DELETE response builds")
}

/// POST dispatch. The only POST target v0 models is the RFC 6638
/// scheduling outbox (`/calendars/{user}/outbox/`), where bifrost
/// submits an iTIP REPLY during an RSVP.
async fn handle_post(
    state: &AppState,
    path: &str,
    headers: &HeaderMap,
    body: &[u8],
    connection_id: Option<u64>,
) -> Response {
    let is_outbox = {
        let fixture = state.shared.fixture.read().expect("fixture lock poisoned");
        matches!(
            parse_path(&fixture, path),
            ResourcePath::ScheduleOutbox { .. }
        )
    };
    if is_outbox {
        return handle_schedule_outbox(state, path, headers, body, connection_id);
    }
    not_found(path)
}

/// Accept an iTIP REPLY POSTed to the scheduling outbox. bifrost's
/// CalDAV RSVP posts a `text/calendar` REPLY here carrying `Originator`
/// (the replying user) and `Recipient` (the organizer) headers, and
/// checks the HTTP status only - no schedule-response XML, no inbox, no
/// free-busy. v0 records the reply (headers + iTIP body) in the request
/// log for observability and returns a bare 200. The durable
/// participation-status change lands via the follow-up PUT, not here.
fn handle_schedule_outbox(
    state: &AppState,
    path: &str,
    headers: &HeaderMap,
    body: &[u8],
    connection_id: Option<u64>,
) -> Response {
    let header_str = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let itip = std::str::from_utf8(body).unwrap_or("");
    state.shared.request_log.record_with_conn(
        "caldav",
        format!("POST {path}"),
        json!({
            "schedule_reply": true,
            "originator": header_str("originator"),
            "recipient": header_str("recipient"),
            "itip": itip,
        }),
        connection_id,
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/xml; charset=utf-8"),
        )
        .body(Body::empty())
        .expect("static schedule-outbox response builds")
}

fn precondition_failed(msg: &str) -> Response {
    (StatusCode::PRECONDITION_FAILED, msg.to_string()).into_response()
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
        .header(
            "Allow",
            "OPTIONS, PROPFIND, REPORT, GET, PUT, POST, DELETE, MKCALENDAR",
        )
        .body(Body::empty())
        .expect("static OPTIONS response builds")
}

/// `MKCALENDAR` on `/calendars/{user}/{cal}/`. Parses the optional
/// `<D:set><D:prop>...</D:prop></D:set>` block for `displayname` and
/// Apple `calendar-color`, then creates a new fixture `Calendar`
/// bound to the principal's account. Records a `calendar_created`
/// transition so Graph `/v1.0/me/calendars` (which reads live
/// state) and JMAP `Calendar/changes` (which walks the delta) both
/// observe the new calendar.
///
/// RFC 4791 §5.3.1 wire shape:
/// - 201 Created on success
/// - 405 Method Not Allowed if the URL already exists
/// - 409 Conflict for unknown intermediate collections (we 404
///   instead since the URL space is fixed by the principal)
async fn handle_mkcalendar(state: &AppState, path: &str, body: &[u8]) -> Response {
    let body_str = match body_to_str(body) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let (user, calendar_id) = {
        let fixture = state.shared.fixture.read().expect("fixture lock poisoned");
        match parse_path(&fixture, path) {
            ResourcePath::Calendar { user, calendar_id } => (user, calendar_id),
            _ => return not_found(path),
        }
    };

    let display_name =
        xml::text_value(body_str, "displayname").unwrap_or_else(|| calendar_id.clone());
    let color = xml::text_value(body_str, "calendar-color");

    let mut fixture = state.shared.fixture.write().expect("fixture lock poisoned");
    if fixture.calendars.iter().any(|c| c.id == calendar_id) {
        // Existing calendar at this URL: RFC 4791 §5.3.1.2 (HTTP
        // 405). Real clients fall back to PROPPATCH; we simply
        // refuse the create.
        return Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(Body::from(format!(
                "calendar {calendar_id:?} already exists",
            )))
            .expect("static 405 response builds");
    }
    let new_cal = crate::fixture::Calendar {
        id: calendar_id.clone(),
        name: display_name,
        color,
        is_default: false,
        account_id: user.clone(),
        caldav_empty_report: false,
    };
    let cal_id_for_diff = calendar_id.clone();
    fixture.mutate(|f| {
        f.calendars.push(new_cal.clone());
        crate::fixture::MutationDiff {
            calendar_created: vec![cal_id_for_diff.clone()],
            ..Default::default()
        }
    });
    Response::builder()
        .status(StatusCode::CREATED)
        .body(Body::empty())
        .expect("static MKCALENDAR response builds")
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
