//! Microsoft Graph calendar endpoints.
//!
//! Implements the subset of `/v1.0/me/calendars/...` and
//! `/v1.0/me/events/...` ratatoskr's Graph calendar client
//! exercises. v0 covers:
//!
//! - `GET /v1.0/me/calendars` (list).
//! - `GET /v1.0/me/calendars/{id}` (single).
//! - `GET /v1.0/me/calendars/{id}/events` (list events in a calendar).
//! - `GET /v1.0/me/calendars/{id}/calendarView/delta`.
//! - `GET /v1.0/me/events/{id}` (single).
//! - `POST /v1.0/me/calendars/{id}/events` (create).
//! - `PATCH /v1.0/me/events/{id}` (update).
//! - `DELETE /v1.0/me/events/{id}` (drop).
//!
//! GET endpoints project from the fixture. Mutating endpoints
//! (POST / PATCH / DELETE) do *not* mutate the fixture - the
//! fixture is read-only in v0 - they instead echo the request body
//! back into the response so tests can assert on what the client
//! tried to write. Real Graph echoes the created/updated event in
//! the 201/200 response body, so this also matches reality.
//! `DELETE` returns 204 with no body, again matching real Graph.
//!
//! The mutation echo is captured wherever it's useful via the
//! existing cross-protocol `RequestLog` middleware in
//! `super::log_request` - the path lands there already, and the
//! handler additionally appends the parsed body to the matching
//! entry's detail. (Graph itself doesn't expose a separate
//! mutation log because the fixture stays read-only; tests that
//! need the body inspect the response or the request log.)
//!
//! `calendarView/delta` returns the full event list on the first
//! call (no `$deltatoken`) and an empty result with a fresh
//! `@odata.deltaLink` on follow-ups, mirroring the messages/delta
//! shape in `mail.rs`.

use axum::{
    Router,
    body::Body as AxumBody,
    extract::{Path, RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::SecondsFormat;
use serde_json::{Map, Value, json};

use super::{AppState, error, odata, ok_json};
use crate::fixture::{Calendar, Event, Fixture};

const EVENTS_DEFAULT_TOP: u32 = 50;
const EVENTS_MAX_TOP: u32 = 256;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1.0/me/calendars", get(list_calendars))
        .route("/v1.0/me/calendars/{calendar}", get(get_calendar))
        .route(
            "/v1.0/me/calendars/{calendar}/events",
            get(list_events).post(create_event),
        )
        .route(
            "/v1.0/me/calendars/{calendar}/calendarView/delta",
            get(delta_events),
        )
        .route(
            "/v1.0/me/events/{event}",
            get(get_event).patch(patch_event).delete(delete_event),
        )
}

// ── Listing / single-resource projection ────────────────────────────

async fn list_calendars(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(_q): RawQuery,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "list_calendars", |_| Ok(())) {
        return o;
    }
    let _ = headers;
    let fixture = state.fixture();
    let value: Vec<Value> = fixture
        .calendars
        .iter()
        .map(|c| serialize_calendar(&fixture, c))
        .collect();
    ok_json(json!({
        "@odata.context": "https://graph.microsoft.com/v1.0/$metadata#me/calendars",
        "value": value,
    }))
}

async fn get_calendar(
    State(state): State<AppState>,
    Path(calendar): Path<String>,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "get_calendar", |s| {
        crate::lua::req_set_str(s, "calendar", &calendar)
    }) {
        return o;
    }
    let fixture = state.fixture();
    match resolve_calendar(&fixture, &calendar) {
        Some(c) => ok_json(serialize_calendar(&fixture, c)),
        None => error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("calendar {calendar:?} not declared in fixture"),
        ),
    }
}

async fn list_events(
    State(state): State<AppState>,
    Path(calendar): Path<String>,
    RawQuery(raw_query): RawQuery,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "list_events", |s| {
        crate::lua::req_set_str(s, "calendar", &calendar)
    }) {
        return o;
    }
    let fixture = state.fixture();
    if resolve_calendar(&fixture, &calendar).is_none() {
        return error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("calendar {calendar:?} not declared in fixture"),
        );
    }
    let q = odata::OdataQuery::parse(raw_query.as_deref());
    let top = q
        .top
        .unwrap_or(EVENTS_DEFAULT_TOP)
        .clamp(1, EVENTS_MAX_TOP);
    let skip = parse_skiptoken(q.skiptoken.as_deref())
        .or(q.skip)
        .unwrap_or(0);

    // Pagination math (verified 2026-05-09):
    //   skip=0/top=2/total=5 -> page=2, next_skip=2, link emitted.
    //   skip=2/top=2         -> page=2, next_skip=4, link emitted.
    //   skip=4/top=1         -> page=1, next_skip=5, no link.
    //   skip=10/total=5      -> page=0, next_skip=10, no link.
    //
    // The page is built straight off a streaming iterator chain
    // so we never materialise the full filtered list. The
    // `nextLink` decision needs the total count, which is one
    // additional pass and still avoids the O(N) clone.
    let page: Vec<Value> = fixture
        .events
        .iter()
        .filter(|e| e.calendar_id == calendar)
        .skip(skip as usize)
        .take(top as usize)
        .map(|e| serialize_event(&fixture, e))
        .collect();
    let next_skip = (skip as usize) + page.len();
    let has_more = fixture
        .events
        .iter()
        .filter(|e| e.calendar_id == calendar)
        .nth(next_skip)
        .is_some();
    let next_link = if has_more {
        Some(format!(
            "https://graph.microsoft.com/v1.0/me/calendars/{calendar}/events?$skiptoken=s.{next_skip}"
        ))
    } else {
        None
    };

    let mut envelope = Map::new();
    envelope.insert(
        "@odata.context".to_string(),
        Value::String(format!(
            "https://graph.microsoft.com/v1.0/$metadata#me/calendars(\"{calendar}\")/events"
        )),
    );
    envelope.insert("value".to_string(), Value::Array(page));
    if let Some(link) = next_link {
        envelope.insert("@odata.nextLink".to_string(), Value::String(link));
    }
    ok_json(Value::Object(envelope))
}

async fn delta_events(
    State(state): State<AppState>,
    Path(calendar): Path<String>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "delta_events", |s| {
        crate::lua::req_set_str(s, "calendar", &calendar)
    }) {
        return o;
    }
    let fixture = state.fixture();
    if resolve_calendar(&fixture, &calendar).is_none() {
        return error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("calendar {calendar:?} not declared in fixture"),
        );
    }
    // The "no changes vs no token" branch hinges on
    // `q.deltatoken.is_some()`. An empty `?$deltatoken=` parses
    // as `Some("")` per `odata::OdataQuery::parse`, so a client
    // that sends an empty token still hits the follow-up branch
    // (correct: any acknowledged token means "I've seen the
    // initial dump").
    let q = odata::OdataQuery::parse(raw_query.as_deref());
    let host = host_or_default(&headers);
    let path = format!("/v1.0/me/calendars/{calendar}/calendarView/delta");
    let context = format!(
        "https://graph.microsoft.com/v1.0/$metadata#me/calendars(\"{calendar}\")/calendarView"
    );

    // The `$deltatoken=latest` shortcut: client wants a fresh
    // delta link with no event dump (mirrors the messages/delta
    // shape).
    if q.deltatoken.as_deref() == Some("latest") {
        let delta_link = odata::build_delta_link(&host, &path, raw_query.as_deref(), 1);
        return ok_json(json!({
            "@odata.context": context,
            "value": [],
            "@odata.deltaLink": delta_link,
        }));
    }

    // Subsequent delta cycles. v0 fixtures are read-only, so
    // nothing has changed - return empty + a re-issued delta
    // link.
    if q.deltatoken.is_some() {
        let delta_link = odata::build_delta_link(&host, &path, raw_query.as_deref(), 1);
        return ok_json(json!({
            "@odata.context": context,
            "value": [],
            "@odata.deltaLink": delta_link,
        }));
    }

    // Initial bootstrap: paginate the full event dump with
    // `@odata.nextLink`; only on the final page emit
    // `@odata.deltaLink`. Real Graph behaves the same way; v0
    // previously short-circuited and emitted deltaLink on the
    // first call regardless of size, which silently dropped
    // pages off the wire for any calendar bigger than the
    // default top.
    let top = q.page_size(EVENTS_DEFAULT_TOP, EVENTS_MAX_TOP);
    let offset = match q.offset() {
        Some(o) => o,
        None => {
            return error(
                StatusCode::BAD_REQUEST,
                "InvalidQueryParameter",
                "$skiptoken did not decode - reset pagination by retrying without it",
            );
        }
    };
    let page: Vec<Value> = fixture
        .events
        .iter()
        .filter(|e| e.calendar_id == calendar)
        .skip(offset as usize)
        .take(top as usize)
        .map(|e| serialize_event(&fixture, e))
        .collect();
    let next_offset_val = (offset as usize) + page.len();
    let has_more = fixture
        .events
        .iter()
        .filter(|e| e.calendar_id == calendar)
        .nth(next_offset_val)
        .is_some();

    let mut envelope = Map::new();
    envelope.insert("@odata.context".to_string(), Value::String(context));
    envelope.insert("value".to_string(), Value::Array(page));
    if has_more {
        let next_off = u32::try_from(next_offset_val).unwrap_or(u32::MAX);
        envelope.insert(
            "@odata.nextLink".to_string(),
            Value::String(odata::build_next_link(&host, &path, raw_query.as_deref(), next_off)),
        );
    } else {
        envelope.insert(
            "@odata.deltaLink".to_string(),
            Value::String(odata::build_delta_link(&host, &path, raw_query.as_deref(), 1)),
        );
    }
    ok_json(Value::Object(envelope))
}

fn host_or_default(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("graph.microsoft.com")
        .to_string()
}

async fn get_event(
    State(state): State<AppState>,
    Path(event): Path<String>,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "get_event", |s| {
        crate::lua::req_set_str(s, "event", &event)
    }) {
        return o;
    }
    let fixture = state.fixture();
    match fixture.events.iter().find(|e| e.id == event) {
        Some(e) => ok_json(serialize_event(&fixture, e)),
        None => error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("event {event:?} not declared in fixture"),
        ),
    }
}

// ── Mutation echoes ─────────────────────────────────────────────────
//
// The fixture is read-only in v0, so POST/PATCH/DELETE never
// mutate state. Each handler echoes the parsed body into the
// response (real Graph also echoes on POST/PATCH) and appends the
// body to the cross-protocol request log under `detail.body`, so
// tests can assert on what the client tried to write.
//
// PATCH and DELETE return `ResourceNotFound` when the event id
// is not declared in the fixture, mirroring real Graph and the
// GET-side behaviour. The check happens before the body is
// parsed / recorded so a 404 won't pollute the request log
// either.

async fn create_event(
    State(state): State<AppState>,
    Path(calendar): Path<String>,
    body: AxumBody,
) -> Response {
    let calendar_known = {
        let fixture = state.fixture();
        resolve_calendar(&fixture, &calendar).is_some()
    };
    if !calendar_known {
        return error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("calendar {calendar:?} not declared in fixture"),
        );
    }
    // calendar_known dropped before we await the body; the read
    // guard does not span the await.
    let parsed = match parse_json_body(body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    state.shared.request_log.record(
        "graph",
        format!("POST /v1.0/me/calendars/{calendar}/events"),
        json!({ "body": parsed }),
    );
    let echoed = mutation_echo(&calendar, "mock-event-create", &parsed);
    (StatusCode::CREATED, axum::Json(echoed)).into_response()
}

async fn patch_event(
    State(state): State<AppState>,
    Path(event): Path<String>,
    body: AxumBody,
) -> Response {
    let calendar_id = {
        let fixture = state.fixture();
        match fixture.events.iter().find(|e| e.id == event) {
            Some(e) => e.calendar_id.clone(),
            None => {
                return error(
                    StatusCode::NOT_FOUND,
                    "ResourceNotFound",
                    &format!("event {event:?} not declared in fixture"),
                );
            }
        }
    };
    // Read guard dropped before the await on the request body.
    let parsed = match parse_json_body(body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    state.shared.request_log.record(
        "graph",
        format!("PATCH /v1.0/me/events/{event}"),
        json!({ "body": parsed }),
    );
    let echoed = mutation_echo(&calendar_id, &event, &parsed);
    ok_json(echoed)
}

async fn delete_event(
    State(state): State<AppState>,
    Path(event): Path<String>,
) -> Response {
    let event_known = {
        let fixture = state.fixture();
        fixture.events.iter().any(|e| e.id == event)
    };
    if !event_known {
        return error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("event {event:?} not declared in fixture"),
        );
    }
    state.shared.request_log.record(
        "graph",
        format!("DELETE /v1.0/me/events/{event}"),
        json!({ "id": event }),
    );
    StatusCode::NO_CONTENT.into_response()
}

async fn parse_json_body(body: AxumBody) -> Result<Value, Response> {
    let bytes = match axum::body::to_bytes(body, 1_048_576).await {
        Ok(b) => b,
        Err(e) => {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "BadRequest",
                &format!("failed to read body: {e}"),
            ));
        }
    };
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&bytes).map_err(|e| {
        error(
            StatusCode::BAD_REQUEST,
            "InvalidRequest",
            &format!("body is not JSON: {e}"),
        )
    })
}

/// Build a Graph-shaped event body around the request payload. The
/// id is fixed (`mock-event-create` for POST, the path id for
/// PATCH) so tests can match without parsing nondeterministic
/// values.
fn mutation_echo(calendar_id: &str, id: &str, body: &Value) -> Value {
    json!({
        "@odata.context": format!(
            "https://graph.microsoft.com/v1.0/$metadata#me/calendars(\"{calendar_id}\")/events/$entity"
        ),
        "id": id,
        "calendarId": calendar_id,
        "echoedRequest": body,
    })
}

// ── Serialisation ───────────────────────────────────────────────────

fn resolve_calendar<'a>(fixture: &'a Fixture, id_or_alias: &str) -> Option<&'a Calendar> {
    if id_or_alias == "default" {
        fixture
            .calendars
            .iter()
            .find(|c| c.is_default)
            .or_else(|| fixture.calendars.first())
    } else {
        fixture.calendars.iter().find(|c| c.id == id_or_alias)
    }
}

fn serialize_calendar(_fixture: &Fixture, c: &Calendar) -> Value {
    json!({
        "id": c.id,
        "name": c.name,
        "color": c.color,
        "isDefaultCalendar": c.is_default,
        "canEdit": true,
        "canShare": false,
        "canViewPrivateItems": true,
    })
}

fn serialize_event(_fixture: &Fixture, e: &Event) -> Value {
    let mut out = Map::new();
    out.insert("id".to_string(), Value::String(e.id.clone()));
    out.insert("calendarId".to_string(), Value::String(e.calendar_id.clone()));
    out.insert("subject".to_string(), Value::String(e.subject.clone()));
    if let Some(p) = &e.body_preview {
        out.insert("bodyPreview".to_string(), Value::String(p.clone()));
    }
    out.insert(
        "body".to_string(),
        json!({
            "contentType": "text",
            "content": e.body_text.clone().unwrap_or_default(),
        }),
    );
    out.insert(
        "start".to_string(),
        json!({
            "dateTime": e.start.to_rfc3339_opts(SecondsFormat::Secs, true),
            "timeZone": "UTC",
        }),
    );
    out.insert(
        "end".to_string(),
        json!({
            "dateTime": e.end.to_rfc3339_opts(SecondsFormat::Secs, true),
            "timeZone": "UTC",
        }),
    );
    out.insert("isAllDay".to_string(), Value::Bool(e.is_all_day));
    if let Some(loc) = &e.location {
        out.insert(
            "location".to_string(),
            json!({ "displayName": loc }),
        );
    }
    if let Some(org) = &e.organizer {
        out.insert(
            "organizer".to_string(),
            json!({
                "emailAddress": {
                    "name": org.name.clone().unwrap_or_default(),
                    "address": org.email,
                }
            }),
        );
    }
    let attendees: Vec<Value> = e
        .attendees
        .iter()
        .map(|a| {
            json!({
                "emailAddress": {
                    "name": a.name.clone().unwrap_or_default(),
                    "address": a.email,
                },
                "type": "required",
            })
        })
        .collect();
    out.insert("attendees".to_string(), Value::Array(attendees));
    Value::Object(out)
}

fn parse_skiptoken(t: Option<&str>) -> Option<u32> {
    let s = t?;
    s.strip_prefix("s.")?.parse().ok()
}

