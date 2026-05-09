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
    let value: Vec<Value> = state
        .fixture
        .calendars
        .iter()
        .map(|c| serialize_calendar(&state.fixture, c))
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
    match resolve_calendar(&state.fixture, &calendar) {
        Some(c) => ok_json(serialize_calendar(&state.fixture, c)),
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
    if resolve_calendar(&state.fixture, &calendar).is_none() {
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
    // PERF TODO (2026-05-09 review): the `Vec<&Event>` allocation
    // walks every event in the fixture per request, which is
    // O(N) work amortising to O(N²) over many pages. Replace
    // with a chained iterator (filter().skip().take().map()) and
    // a separate filter().count() if `total` is needed. Tracked
    // in TODO.md "Fix now".
    let all: Vec<&Event> = state
        .fixture
        .events
        .iter()
        .filter(|e| e.calendar_id == calendar)
        .collect();
    let total = all.len();
    let page: Vec<Value> = all
        .iter()
        .skip(skip as usize)
        .take(top as usize)
        .map(|e| serialize_event(&state.fixture, e))
        .collect();
    let next_skip = (skip as usize) + page.len();
    let next_link = if next_skip < total {
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
    RawQuery(raw_query): RawQuery,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "delta_events", |s| {
        crate::lua::req_set_str(s, "calendar", &calendar)
    }) {
        return o;
    }
    if resolve_calendar(&state.fixture, &calendar).is_none() {
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
    //
    // Spec deviation (acknowledged 2026-05-09): real Graph
    // paginates the initial dump with `@odata.nextLink` and only
    // emits `@odata.deltaLink` on the final page. v0 doesn't
    // paginate the initial dump at all and emits `deltaLink`
    // immediately. Acceptable for current ratatoskr smokes; if a
    // calendar grows past EVENTS_DEFAULT_TOP, add nextLink-style
    // pagination to the initial branch (TODO.md "Fix soon").
    let q = odata::OdataQuery::parse(raw_query.as_deref());
    let value: Vec<Value> = if q.deltatoken.is_some() {
        // Follow-up call with a delta cursor; nothing has changed.
        vec![]
    } else {
        state
            .fixture
            .events
            .iter()
            .filter(|e| e.calendar_id == calendar)
            .map(|e| serialize_event(&state.fixture, e))
            .collect()
    };
    // Byte-stable delta link: derives from `fixture.state` which
    // is constant within a process lifetime.
    let delta_link = format!(
        "https://graph.microsoft.com/v1.0/me/calendars/{calendar}/calendarView/delta?$deltatoken=d.{}",
        state.fixture.state
    );
    ok_json(json!({
        "@odata.context": format!(
            "https://graph.microsoft.com/v1.0/$metadata#me/calendars(\"{calendar}\")/calendarView"
        ),
        "value": value,
        "@odata.deltaLink": delta_link,
    }))
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
    match state.fixture.events.iter().find(|e| e.id == event) {
        Some(e) => ok_json(serialize_event(&state.fixture, e)),
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
// Two known deviations from real Graph, accepted for v0:
//   - PATCH on an unknown event id silently 200s with
//     `calendarId: "unknown"`. Real Graph 404s. TODO.md "Fix now".
//   - DELETE on an unknown event id always 204s. Real Graph 404s.
//     TODO.md "Fix now".
//
// PERF TODO (2026-05-09 review): the parsed body (capped at 1MB
// by `parse_json_body`) is cloned into the request log entry.
// Long-running scenarios accumulate it. Capping the request log
// itself addresses the symptom; storing only body size/hash here
// is a smaller alternative.

async fn create_event(
    State(state): State<AppState>,
    Path(calendar): Path<String>,
    body: AxumBody,
) -> Response {
    if resolve_calendar(&state.fixture, &calendar).is_none() {
        return error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("calendar {calendar:?} not declared in fixture"),
        );
    }
    let parsed = match parse_json_body(body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    state.request_log.record(
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
    let calendar_id = state
        .fixture
        .events
        .iter()
        .find(|e| e.id == event)
        .map(|e| e.calendar_id.clone());
    let parsed = match parse_json_body(body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    state.request_log.record(
        "graph",
        format!("PATCH /v1.0/me/events/{event}"),
        json!({ "body": parsed }),
    );
    let cal = calendar_id.unwrap_or_else(|| "unknown".to_string());
    let echoed = mutation_echo(&cal, &event, &parsed);
    ok_json(echoed)
}

async fn delete_event(
    State(state): State<AppState>,
    Path(event): Path<String>,
) -> Response {
    state.request_log.record(
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

