//! Google Calendar v3 calendar list + events handlers.
//!
//! Projection follows
//! `<ratatoskr>/crates/calendar/src/google.rs`'s deserialiser
//! shape (camelCase `dateTime` / `timeZone`, etc.). Mutations
//! (POST / PATCH / DELETE) write through `Fixture::mutate` and
//! record `event_*` transitions parallel to Graph / CalDAV /
//! JMAP, so a Google Calendar create surfaces in a Graph
//! `calendarView/delta` follow-up.
//!
//! Sync-token semantics:
//!
//! - No token: full event list. Final page emits
//!   `nextSyncToken`; mid-pages emit `nextPageToken` only.
//! - Token equals current `state`: empty `items[]`, same token
//!   echoed as `nextSyncToken`.
//! - Token unknown / evicted: HTTP 410 (`reason:
//!   "fullSyncRequired"`). ratatoskr's `google_calendar_sync_
//!   events_impl` matches on `"410"` or `"sync token"` to fall
//!   back to a full re-sync.

use axum::{
    Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete as delete_route, get},
};
use chrono::SecondsFormat;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{AppState, error, ok_json};
use crate::fixture::{Address, Calendar, Event, Fixture, MutationDiff};

/// Resolve the request's bearer to the account it authorizes,
/// falling back to primary. See `gmail::mail::bearer_account` for
/// the shape rationale.
fn bearer_account(state: &AppState, headers: &HeaderMap) -> String {
    crate::oauth::account_from_bearer(&state.fixture(), &state.shared.token_store, headers)
}

const DEFAULT_PAGE_SIZE: usize = 250;
const MAX_PAGE_SIZE: usize = 2500;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/calendar/v3/users/me/calendarList", get(calendar_list))
        .route(
            "/calendar/v3/calendars/{calendar}/events",
            get(list_events).post(create_event),
        )
        .route(
            "/calendar/v3/calendars/{calendar}/events/{event}",
            get(get_event).patch(patch_event).delete(delete_event_route()),
        )
}

// Workaround: axum's `MethodRouter::delete` re-exports clash with
// our `delete_route` rename above. Wrap the handler so the axum
// re-export resolves correctly via the `routing` import.
fn delete_event_route() -> axum::routing::MethodRouter<AppState> {
    delete_route(delete_event)
}

#[derive(Debug, Deserialize, Default)]
struct ListParams {
    #[serde(default, rename = "syncToken")]
    sync_token: Option<String>,
    #[serde(default, rename = "pageToken")]
    page_token: Option<String>,
    #[serde(default, rename = "timeMin")]
    time_min: Option<String>,
    #[serde(default, rename = "timeMax")]
    time_max: Option<String>,
    #[serde(default, rename = "singleEvents")]
    _single_events: Option<bool>,
    #[serde(default, rename = "maxResults")]
    max_results: Option<usize>,
    #[serde(default, rename = "orderBy")]
    order_by: Option<String>,
}

// ── calendarList ────────────────────────────────────────────────────

async fn calendar_list(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(o) = super::maybe_override(&state, "calendar_list", |_| Ok(())) {
        return o;
    }
    let account_id = bearer_account(&state, &headers);
    let fixture = state.fixture();
    let items: Vec<Value> = fixture
        .calendars_for(&account_id)
        .map(serialize_calendar)
        .collect();
    ok_json(json!({
        "kind": "calendar#calendarList",
        "etag": format!("etag-{}", fixture.state_for(&account_id)),
        "items": items,
    }))
}

fn serialize_calendar(c: &Calendar) -> Value {
    let mut obj = Map::new();
    obj.insert(
        "kind".into(),
        Value::String("calendar#calendarListEntry".into()),
    );
    obj.insert("id".into(), Value::String(c.id.clone()));
    obj.insert("summary".into(), Value::String(c.name.clone()));
    if let Some(color) = &c.color {
        obj.insert("backgroundColor".into(), Value::String(color.clone()));
    }
    if c.is_default {
        obj.insert("primary".into(), Value::Bool(true));
    }
    obj.insert("accessRole".into(), Value::String("owner".into()));
    Value::Object(obj)
}

// ── events list ─────────────────────────────────────────────────────

async fn list_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(calendar): Path<String>,
    Query(params): Query<ListParams>,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "list_events", |s| {
        crate::lua::req_set_str(s, "calendar", &calendar)?;
        if let Some(t) = &params.sync_token {
            crate::lua::req_set_str(s, "sync_token", t)?;
        }
        Ok(())
    }) {
        return o;
    }

    let account_id = bearer_account(&state, &headers);
    let fixture = state.fixture();
    if !fixture.calendars_for(&account_id).any(|c| c.id == calendar) {
        return error(
            StatusCode::NOT_FOUND,
            &format!("calendar {calendar:?} not declared in fixture"),
            "notFound",
        );
    }

    // Sync-token paths (mirrors People's connections handler):
    // - current state: empty delta + same token echoed back.
    // - known historical state: walk the change_log via
    //   `event_delta_since` (already parent-filtered for this
    //   calendar), emit live events for created/updated and
    //   `{id, status: "cancelled"}` tombstones for destroyed.
    //   ratatoskr's `:189-200` cancelled-routing branch reads
    //   these to drop deleted events from local DB.
    // - unknown / evicted token: HTTP 410 with reason
    //   `fullSyncRequired`. ratatoskr's recovery path matches on
    //   `"410"` or `"sync token"` substrings to drop the saved
    //   token and retry without it.
    if params.sync_token.as_deref() == Some(fixture.state_for(&account_id)) {
        return ok_json(json!({
            "kind": "calendar#events",
            "summary": calendar,
            "items": [],
            "nextSyncToken": fixture.state_for(&account_id),
        }));
    }
    if let Some(token) = params.sync_token.as_deref() {
        let Some(delta) = fixture.event_delta_since(token, &calendar) else {
            return error(
                StatusCode::GONE,
                "sync token expired or not recognised; full sync required",
                "fullSyncRequired",
            );
        };
        let live_by_id: std::collections::HashMap<&str, &Event> = fixture
            .events_for(&account_id)
            .filter(|e| e.calendar_id == calendar)
            .map(|e| (e.id.as_str(), e))
            .collect();
        let mut items: Vec<Value> = Vec::new();
        for id in delta.created.iter().chain(delta.updated.iter()) {
            if let Some(e) = live_by_id.get(id.as_str()) {
                items.push(serialize_event(e));
            }
        }
        for id in &delta.destroyed {
            items.push(cancelled_tombstone(id));
        }
        return ok_json(json!({
            "kind": "calendar#events",
            "summary": calendar,
            "items": items,
            "nextSyncToken": fixture.state_for(&account_id),
        }));
    }

    // Non-delta windowed pull (bifrost's `events_in_range`): no
    // syncToken, `timeMin` / `timeMax` bound the window and
    // `orderBy=startTime` requests start-ordered results. Keep events
    // overlapping `[timeMin, timeMax)`; an absent bound is open on that
    // side. The overlap stays coarse (bifrost re-filters client-side),
    // matching the Graph / JMAP calendar mocks.
    let time_min = params.time_min.as_deref().and_then(parse_range_bound);
    let time_max = params.time_max.as_deref().and_then(parse_range_bound);
    let mut all: Vec<&Event> = fixture
        .events_for(&account_id)
        .filter(|e| e.calendar_id == calendar)
        .filter(|e| event_in_window(e, time_min, time_max))
        .collect();
    if params.order_by.as_deref() == Some("startTime") {
        all.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.id.cmp(&b.id)));
    } else {
        all.sort_by(|a, b| a.id.cmp(&b.id));
    }
    let total = all.len();
    let page_size = params
        .max_results
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .clamp(1, MAX_PAGE_SIZE);
    let offset = parse_page_token(params.page_token.as_deref()).unwrap_or(0);
    if offset > total {
        return error(
            StatusCode::BAD_REQUEST,
            "pageToken offset exceeds total",
            "badRequest",
        );
    }
    let end = (offset + page_size).min(total);
    let items: Vec<Value> = all[offset..end]
        .iter()
        .map(|e| serialize_event(e))
        .collect();

    let mut body = Map::new();
    body.insert("kind".into(), Value::String("calendar#events".into()));
    body.insert("summary".into(), Value::String(calendar));
    body.insert("items".into(), Value::Array(items));
    if end < total {
        body.insert(
            "nextPageToken".into(),
            Value::String(encode_page_token(end)),
        );
    } else {
        body.insert(
            "nextSyncToken".into(),
            Value::String(fixture.state_for(&account_id).to_string()),
        );
    }
    ok_json(Value::Object(body))
}

/// Google Calendar tombstone shape: a calendar#event with
/// `status: "cancelled"` and only the id present. Per
/// `<ratatoskr>/crates/calendar/src/google.rs:189-200`, the
/// cancelled-status branch routes the id to deleted_remote_ids
/// and drops the row.
fn cancelled_tombstone(id: &str) -> Value {
    json!({
        "kind": "calendar#event",
        "id": id,
        "status": "cancelled",
    })
}

fn serialize_event(e: &Event) -> Value {
    let mut obj = Map::new();
    obj.insert("kind".into(), Value::String("calendar#event".into()));
    obj.insert("id".into(), Value::String(e.id.clone()));
    obj.insert(
        "iCalUID".into(),
        Value::String(format!("{}@saehrimnir.test", e.id)),
    );
    obj.insert("etag".into(), Value::String(format!("etag-{}", e.id)));
    obj.insert("status".into(), Value::String("confirmed".into()));
    obj.insert("summary".into(), Value::String(e.subject.clone()));
    if let Some(d) = &e.body_text {
        obj.insert("description".into(), Value::String(d.clone()));
    }
    if let Some(loc) = &e.location {
        obj.insert("location".into(), Value::String(loc.clone()));
    }

    let (start_obj, end_obj) = serialize_event_times(e);
    obj.insert("start".into(), start_obj);
    obj.insert("end".into(), end_obj);

    if let Some(org) = &e.organizer {
        let mut o = Map::new();
        o.insert("email".into(), Value::String(org.email.clone()));
        if let Some(n) = &org.name {
            o.insert("displayName".into(), Value::String(n.clone()));
        }
        obj.insert("organizer".into(), Value::Object(o));
    }
    if !e.attendees.is_empty() {
        let attendees: Vec<Value> = e
            .attendees
            .iter()
            .map(|a| {
                let mut m = Map::new();
                m.insert("email".into(), Value::String(a.email.clone()));
                if let Some(n) = &a.name {
                    m.insert("displayName".into(), Value::String(n.clone()));
                }
                m.insert("responseStatus".into(), Value::String("needsAction".into()));
                Value::Object(m)
            })
            .collect();
        obj.insert("attendees".into(), Value::Array(attendees));
    }
    // Google Calendar v3 emits a `recurrence` array carrying the
    // full iCal RRULE / EXDATE / RDATE lines verbatim (per
    // developers.google.com/calendar/api/v3/reference/events). v0
    // emits RRULE first, then one EXDATE per excluded date so the
    // wire ordering is stable for byte-deterministic snapshots.
    if e.recurrence_rule.is_some() || !e.recurrence_exdates.is_empty() {
        let mut rec: Vec<Value> = Vec::new();
        if let Some(r) = &e.recurrence_rule {
            rec.push(Value::String(format!("RRULE:{r}")));
        }
        for ex in &e.recurrence_exdates {
            rec.push(Value::String(format!(
                "EXDATE:{}",
                ex.format("%Y%m%dT%H%M%SZ")
            )));
        }
        obj.insert("recurrence".into(), Value::Array(rec));
    }
    Value::Object(obj)
}

fn serialize_event_times(e: &Event) -> (Value, Value) {
    if e.is_all_day {
        let start = json!({"date": e.start.format("%Y-%m-%d").to_string()});
        let end = json!({"date": e.end.format("%Y-%m-%d").to_string()});
        (start, end)
    } else {
        let start = json!({
            "dateTime": e.start.to_rfc3339_opts(SecondsFormat::Secs, true),
            "timeZone": "UTC",
        });
        let end = json!({
            "dateTime": e.end.to_rfc3339_opts(SecondsFormat::Secs, true),
            "timeZone": "UTC",
        });
        (start, end)
    }
}

/// Parse a Google Calendar range bound (`timeMin` / `timeMax`) as an
/// RFC 3339 instant. bifrost always sends these with an offset (`Z` or
/// `+HH:MM`), so a bare offsetless value returns None (open bound).
fn parse_range_bound(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

/// Half-open overlap test against `[min, max)`; an absent bound is open
/// on that side.
fn event_in_window(
    e: &Event,
    min: Option<chrono::DateTime<chrono::Utc>>,
    max: Option<chrono::DateTime<chrono::Utc>>,
) -> bool {
    if let Some(m) = min
        && e.end <= m
    {
        return false;
    }
    if let Some(m) = max
        && e.start >= m
    {
        return false;
    }
    true
}

// ── single-event get ────────────────────────────────────────────────

/// `GET /calendar/v3/calendars/{calendar}/events/{event}` - the
/// per-event read bifrost's `event_get` drives. 404 (`notFound`) when
/// the event is absent from the calendar.
async fn get_event(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((calendar, event_id)): Path<(String, String)>,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "get_event", |_| Ok(())) {
        return o;
    }
    let account_id = bearer_account(&state, &headers);
    let fixture = state.fixture();
    match fixture
        .events_for(&account_id)
        .find(|e| e.id == event_id && e.calendar_id == calendar)
    {
        Some(e) => ok_json(serialize_event(e)),
        None => error(
            StatusCode::NOT_FOUND,
            &format!("event {event_id:?} not in calendar {calendar:?}"),
            "notFound",
        ),
    }
}

// ── mutations ───────────────────────────────────────────────────────

async fn create_event(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(calendar): Path<String>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
    body: axum::body::Body,
) -> Response {
    let account_id = bearer_account(&state, &headers);
    {
        let fixture = state.fixture();
        if !fixture.calendars_for(&account_id).any(|c| c.id == calendar) {
            return error(
                StatusCode::NOT_FOUND,
                &format!("calendar {calendar:?} not declared in fixture"),
                "notFound",
            );
        }
    }
    let parsed = match parse_json_body(body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    state.shared.request_log.record_with_conn(
        "gcal",
        format!("POST /calendar/v3/calendars/{calendar}/events"),
        crate::request_log::body_detail(&parsed),
        connection_id,
    );

    let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
    let event = match build_event_from_create(&mut fix, &calendar, &parsed) {
        Ok(e) => e,
        Err(resp) => return *resp,
    };
    let server_id = event.id.clone();
    let view = event.clone();
    let _ = fix.mutate(|f| {
        f.events.push(event);
        MutationDiff {
            event_created: vec![server_id.clone()],
            ..Default::default()
        }
    });
    (StatusCode::OK, axum::Json(serialize_event(&view))).into_response()
}

async fn patch_event(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((calendar, event_id)): Path<(String, String)>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
    body: axum::body::Body,
) -> Response {
    let account_id = bearer_account(&state, &headers);
    {
        let fixture = state.fixture();
        if !fixture
            .events_for(&account_id)
            .any(|e| e.id == event_id && e.calendar_id == calendar)
        {
            return error(
                StatusCode::NOT_FOUND,
                &format!("event {event_id:?} not in calendar {calendar:?}"),
                "notFound",
            );
        }
    }
    let parsed = match parse_json_body(body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    state.shared.request_log.record_with_conn(
        "gcal",
        format!("PATCH /calendar/v3/calendars/{calendar}/events/{event_id}"),
        crate::request_log::body_detail(&parsed),
        connection_id,
    );

    let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
    let Some(idx) = fix
        .events
        .iter()
        .position(|e| e.account_id == account_id && e.id == event_id)
    else {
        return error(StatusCode::NOT_FOUND, "event vanished", "notFound");
    };
    let mut clone = fix.events[idx].clone();
    if let Err(resp) = apply_event_patch(&mut clone, &parsed) {
        return *resp;
    }
    let view = clone.clone();
    let id_for_diff = event_id.clone();
    let _ = fix.mutate(|f| {
        f.events[idx] = clone;
        MutationDiff {
            event_updated: vec![id_for_diff.clone()],
            ..Default::default()
        }
    });
    ok_json(serialize_event(&view))
}

async fn delete_event(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((calendar, event_id)): Path<(String, String)>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
) -> Response {
    let account_id = bearer_account(&state, &headers);
    {
        let fixture = state.fixture();
        if !fixture
            .events_for(&account_id)
            .any(|e| e.id == event_id && e.calendar_id == calendar)
        {
            return error(
                StatusCode::NOT_FOUND,
                &format!("event {event_id:?} not in calendar {calendar:?}"),
                "notFound",
            );
        }
    }
    state.shared.request_log.record_with_conn(
        "gcal",
        format!("DELETE /calendar/v3/calendars/{calendar}/events/{event_id}"),
        json!({"id": event_id}),
        connection_id,
    );

    let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
    let id_for_diff = event_id.clone();
    let acct = account_id.clone();
    let parent = fix
        .events
        .iter()
        .find(|e| e.account_id == acct && e.id == id_for_diff)
        .map(|e| e.calendar_id.clone());
    let _ = fix.mutate(|f| {
        let len_before = f.events.len();
        f.events
            .retain(|e| !(e.account_id == acct && e.id == id_for_diff));
        if f.events.len() < len_before {
            MutationDiff {
                event_destroyed: vec![id_for_diff.clone()],
                event_destroyed_parents: vec![parent.clone().unwrap_or_default()],
                ..Default::default()
            }
        } else {
            MutationDiff::default()
        }
    });
    StatusCode::NO_CONTENT.into_response()
}

// ── parsing helpers ─────────────────────────────────────────────────

fn build_event_from_create(
    fix: &mut Fixture,
    calendar: &str,
    body: &Value,
) -> Result<Event, Box<Response>> {
    let obj = body.as_object().ok_or_else(|| {
        Box::new(error(
            StatusCode::BAD_REQUEST,
            "create body must be a JSON object",
            "badRequest",
        ))
    })?;
    let summary = obj
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let description = obj
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_string);
    let location = obj
        .get("location")
        .and_then(Value::as_str)
        .map(str::to_string);

    let (start, end, is_all_day) =
        parse_event_times(obj.get("start"), obj.get("end")).ok_or_else(|| {
            Box::new(error(
                StatusCode::BAD_REQUEST,
                "start / end missing or malformed",
                "badRequest",
            ))
        })?;

    let organizer = obj.get("organizer").and_then(parse_address);
    let attendees: Vec<Address> = obj
        .get("attendees")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(parse_address).collect())
        .unwrap_or_default();

    let id = fix.mint_event_id();
    let account_id = fix
        .calendars
        .iter()
        .find(|c| c.id == calendar)
        .map(|c| c.account_id.clone())
        .unwrap_or_else(|| fix.primary_account().id.clone());
    let (recurrence_rule, recurrence_exdates) = obj
        .get("recurrence")
        .map(parse_gcal_recurrence)
        .unwrap_or((None, Vec::new()));
    Ok(Event {
        id,
        account_id,
        calendar_id: calendar.to_string(),
        subject: summary,
        body_preview: None,
        body_text: description,
        start,
        end,
        location,
        organizer,
        attendees,
        is_all_day,
        recurrence_rule,
        recurrence_exdates,
        // Google Calendar create carries no timezone / reminder /
        // raw-ical authoring on this path; the static `[[event]]`
        // fixture shape is the way to stage those.
        time_zone: None,
        reminders: Vec::new(),
        raw_ical: None,
    })
}

/// Read Google Calendar's `recurrence: ["RRULE:...", "EXDATE:...",
/// "RDATE:..."]` array. RRULE values are stored verbatim (CalDAV
/// and the fixture both keep the canonical form); EXDATE entries
/// can be comma-separated date lists per RFC 5545 §3.8.5.1, so
/// each line splits and decodes individually. RDATE / unknown
/// prefixes are silently dropped (real Google emits the array
/// the client posts back verbatim including unknown shapes; the
/// fixture only stores what the read path can re-emit).
fn parse_gcal_recurrence(v: &Value) -> (Option<String>, Vec<chrono::DateTime<chrono::Utc>>) {
    let mut rrule: Option<String> = None;
    let mut exdates: Vec<chrono::DateTime<chrono::Utc>> = Vec::new();
    let Some(arr) = v.as_array() else {
        return (None, exdates);
    };
    for entry in arr {
        let Some(s) = entry.as_str() else { continue };
        if let Some(rest) = s.strip_prefix("RRULE:") {
            rrule = Some(rest.trim().to_string());
        } else if let Some(rest) = s.strip_prefix("EXDATE:") {
            // EXDATE may carry a TZID parameter (`EXDATE;TZID=...:...`)
            // - strip the param segment if present.
            let value = rest.rsplit_once(':').map_or(rest, |(_params, val)| val);
            for part in value.split(',') {
                if let Some(dt) = parse_ical_dt(part.trim()) {
                    exdates.push(dt);
                }
            }
        }
    }
    (rrule, exdates)
}

/// Parse an iCal UTC date-time (`YYYYMMDDTHHMMSSZ`). Tolerant of
/// the trailing `Z` being missing (real clients sometimes drop it
/// when the X-WR-TIMEZONE is UTC).
fn parse_ical_dt(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let stripped = s.trim_end_matches('Z');
    chrono::NaiveDateTime::parse_from_str(stripped, "%Y%m%dT%H%M%S")
        .ok()
        .map(|dt| dt.and_utc())
}

fn apply_event_patch(event: &mut Event, body: &Value) -> Result<(), Box<Response>> {
    let obj = body.as_object().ok_or_else(|| {
        Box::new(error(
            StatusCode::BAD_REQUEST,
            "patch must be a JSON object",
            "badRequest",
        ))
    })?;
    if let Some(s) = obj.get("summary").and_then(Value::as_str) {
        event.subject = s.to_string();
    }
    if let Some(v) = obj.get("description") {
        event.body_text = v.as_str().map(str::to_string);
    }
    if let Some(v) = obj.get("location") {
        event.location = v.as_str().map(str::to_string);
    }
    if (obj.contains_key("start") || obj.contains_key("end"))
        && let Some((s, e, all_day)) = parse_event_times(obj.get("start"), obj.get("end"))
    {
        event.start = s;
        event.end = e;
        event.is_all_day = all_day;
    }
    if let Some(arr) = obj.get("attendees").and_then(Value::as_array) {
        event.attendees = arr.iter().filter_map(parse_address).collect();
    }
    // Real Google Calendar clients can repoint `organizer` on a
    // PATCH (with appropriate ACLs); ratatoskr does not currently
    // exercise this, but parsing it through keeps the gcal patch
    // surface symmetric with create. Graph deliberately ignores
    // `organizer` on patch (Microsoft Graph clients can't
    // repoint), so the divergence between the two surfaces is
    // intentional.
    if let Some(v) = obj.get("organizer") {
        event.organizer = parse_address(v);
    }
    // A PATCH carrying `recurrence: null` clears recurrence on the
    // event; an array replaces it; any other shape silently leaves
    // the existing fields alone (matches real gcal's PATCH semantics
    // where absent / null clears).
    if let Some(v) = obj.get("recurrence") {
        if v.is_null() {
            event.recurrence_rule = None;
            event.recurrence_exdates.clear();
        } else {
            let (rrule, exdates) = parse_gcal_recurrence(v);
            event.recurrence_rule = rrule;
            event.recurrence_exdates = exdates;
        }
    }
    Ok(())
}

fn parse_event_times(
    start: Option<&Value>,
    end: Option<&Value>,
) -> Option<(
    chrono::DateTime<chrono::Utc>,
    chrono::DateTime<chrono::Utc>,
    bool,
)> {
    let s = start?;
    let e = end?;
    if let (Some(sdt), Some(edt)) = (parse_date_time(s), parse_date_time(e)) {
        return Some((sdt, edt, false));
    }
    let sd = s.get("date").and_then(Value::as_str)?;
    let ed = e.get("date").and_then(Value::as_str)?;
    let sdt = chrono::NaiveDate::parse_from_str(sd, "%Y-%m-%d")
        .ok()?
        .and_hms_opt(0, 0, 0)?
        .and_utc();
    let edt = chrono::NaiveDate::parse_from_str(ed, "%Y-%m-%d")
        .ok()?
        .and_hms_opt(0, 0, 0)?
        .and_utc();
    Some((sdt, edt, true))
}

fn parse_date_time(v: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    let s = v.get("dateTime")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

fn parse_address(v: &Value) -> Option<Address> {
    let email = v.get("email").and_then(Value::as_str)?.to_string();
    let name = v
        .get("displayName")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(Address { email, name })
}

// Err is an axum `Response` (large); boxing would force every `?`
// caller to unbox. Allow rather than reshape the helper.
#[allow(clippy::result_large_err)]
async fn parse_json_body(body: axum::body::Body) -> Result<Value, Response> {
    let bytes = match axum::body::to_bytes(body, 1_048_576).await {
        Ok(b) => b,
        Err(e) => {
            return Err(error(
                StatusCode::BAD_REQUEST,
                &format!("failed to read body: {e}"),
                "badRequest",
            ));
        }
    };
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&bytes).map_err(|e| {
        error(
            StatusCode::BAD_REQUEST,
            &format!("body is not JSON: {e}"),
            "invalidRequest",
        )
    })
}

// ── shared helpers ──────────────────────────────────────────────────

fn encode_page_token(offset: usize) -> String {
    format!("p.{offset}")
}

fn parse_page_token(t: Option<&str>) -> Option<usize> {
    t?.strip_prefix("p.")?.parse().ok()
}
