//! Microsoft Graph calendar endpoints.
//!
//! Implements the subset of `/v1.0/me/calendars/...` and
//! `/v1.0/me/events/...` (plus the parallel
//! `/v1.0/users/{userId}/...` paths) ratatoskr's Graph calendar
//! client exercises. v0 covers:
//!
//! - `GET    /calendars` (list).
//! - `GET    /calendars/{id}` (single).
//! - `GET    /calendars/{id}/events` (list events).
//! - `GET    /calendars/{id}/calendarView/delta`.
//! - `GET    /events/{id}` (single).
//! - `POST   /calendars/{id}/events` (create).
//! - `PATCH  /events/{id}` (update).
//! - `DELETE /events/{id}` (drop).
//!
//! Stage 3 of the multi-account refactor routes per-account on
//! this surface: `/v1.0/me/...` scopes to the primary;
//! `/v1.0/users/{userId}/...` scopes to the named declared
//! account (`me` aliases primary; unknown user returns 404).
//! Folder-agnostic `/events/{id}` looks up the event within the
//! resolved account; mutations land on the calendar's account.
//!
//! Mutations land via `Fixture::mutate` and record
//! `event_created` / `event_updated` / `event_destroyed`
//! transitions so subsequent `calendarView/delta` calls walk
//! them.

use axum::{
    Router,
    body::Body as AxumBody,
    extract::{Path, RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Map, Value, json};

use super::{AppState, error, odata, ok_json};
use crate::fixture::{
    Address, Calendar, Event, EventAttendee, Fixture, MutationDiff, ParticipationStatus,
};

const EVENTS_DEFAULT_TOP: u32 = 50;
const EVENTS_MAX_TOP: u32 = 256;

pub fn router() -> Router<AppState> {
    Router::new()
        // /me/...
        .route("/v1.0/me/calendars", get(list_calendars_me))
        .route("/v1.0/me/calendars/{calendar}", get(get_calendar_me))
        .route(
            "/v1.0/me/calendars/{calendar}/events",
            get(list_events_me).post(create_event_me),
        )
        .route(
            "/v1.0/me/calendars/{calendar}/calendarView/delta",
            get(delta_events_me),
        )
        .route(
            "/v1.0/me/calendars/{calendar}/calendarView",
            get(calendar_view_me),
        )
        .route(
            "/v1.0/me/events/{event}",
            get(get_event_me)
                .patch(patch_event_me)
                .delete(delete_event_me),
        )
        .route("/v1.0/me/events/{event}/{action}", post(rsvp_me))
        // Calendar-scoped event addressing. bifrost's Graph client
        // (`event_update` / `event_delete` / `event_rsvp`) targets the
        // calendar-scoped URL, so these mirror the mailbox-scoped
        // routes above. The inner handlers resolve the event by id
        // within the account, so the `{calendar}` segment is
        // informational.
        .route(
            "/v1.0/me/calendars/{calendar}/events/{event}",
            get(get_event_cal_me)
                .patch(patch_event_cal_me)
                .delete(delete_event_cal_me),
        )
        .route(
            "/v1.0/me/calendars/{calendar}/events/{event}/{action}",
            post(rsvp_cal_me),
        )
        // /users/{user}/...
        .route("/v1.0/users/{user}/calendars", get(list_calendars_user))
        .route(
            "/v1.0/users/{user}/calendars/{calendar}",
            get(get_calendar_user),
        )
        .route(
            "/v1.0/users/{user}/calendars/{calendar}/events",
            get(list_events_user).post(create_event_user),
        )
        .route(
            "/v1.0/users/{user}/calendars/{calendar}/calendarView/delta",
            get(delta_events_user),
        )
        .route(
            "/v1.0/users/{user}/calendars/{calendar}/calendarView",
            get(calendar_view_user),
        )
        .route(
            "/v1.0/users/{user}/events/{event}",
            get(get_event_user)
                .patch(patch_event_user)
                .delete(delete_event_user),
        )
        .route(
            "/v1.0/users/{user}/events/{event}/{action}",
            post(rsvp_user),
        )
        .route(
            "/v1.0/users/{user}/calendars/{calendar}/events/{event}",
            get(get_event_cal_user)
                .patch(patch_event_cal_user)
                .delete(delete_event_cal_user),
        )
        .route(
            "/v1.0/users/{user}/calendars/{calendar}/events/{event}/{action}",
            post(rsvp_cal_user),
        )
}

// ── /me/ wrappers ───────────────────────────────────────────────────

async fn list_calendars_me(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(q): RawQuery,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    list_calendars_impl(state, &account_id, headers, q).await
}

async fn get_calendar_me(State(state): State<AppState>, Path(calendar): Path<String>) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    get_calendar_impl(state, &account_id, &calendar).await
}

async fn list_events_me(
    State(state): State<AppState>,
    Path(calendar): Path<String>,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    list_events_impl(state, &account_id, &calendar, raw_query, true).await
}

async fn calendar_view_me(
    State(state): State<AppState>,
    Path(calendar): Path<String>,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    calendar_view_impl(state, &account_id, &calendar, raw_query, true).await
}

async fn delta_events_me(
    State(state): State<AppState>,
    Path(calendar): Path<String>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    delta_events_impl(state, &account_id, &calendar, headers, raw_query, true).await
}

async fn rsvp_me(
    State(state): State<AppState>,
    Path((event, action)): Path<(String, String)>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    rsvp_impl(state, &account_id, &event, &action, true, connection_id).await
}

async fn rsvp_user(
    State(state): State<AppState>,
    Path((user, event, action)): Path<(String, String, String)>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    rsvp_impl(state, &account_id, &event, &action, false, connection_id).await
}

/// Calendar-scoped RSVP wrapper. bifrost's Graph client addresses
/// event actions at the calendar-scoped URL
/// (`/calendars/{cal}/events/{id}/{action}`), not the mailbox-scoped
/// one. The `{calendar}` segment is validated against the event's
/// membership only implicitly (the inner handler resolves the event
/// by id within the account); it is otherwise informational.
async fn rsvp_cal_me(
    State(state): State<AppState>,
    Path((_calendar, event, action)): Path<(String, String, String)>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    rsvp_impl(state, &account_id, &event, &action, true, connection_id).await
}

async fn rsvp_cal_user(
    State(state): State<AppState>,
    Path((user, _calendar, event, action)): Path<(String, String, String, String)>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    rsvp_impl(state, &account_id, &event, &action, false, connection_id).await
}

/// `POST /v1.0/me/events/{id}/{accept|decline|tentativelyAccept}` (and
/// the calendar-scoped `/calendars/{cal}/events/{id}/{action}` form) -
/// RSVP to a meeting invite (bifrost `calendar.rs::rsvp`). Durably
/// records the authenticated user's participation status against its
/// own attendee record (the attendee whose address is the account's
/// own, `account.name` per the Graph `mail` convention), so a
/// follow-up read observes `attendees[].status.response`. Records an
/// `event_updated` transition when the status actually moves. Returns
/// 202 Accepted. Unknown action -> 400, unknown event -> 404.
async fn rsvp_impl(
    state: AppState,
    account_id: &str,
    event: &str,
    action: &str,
    me_path: bool,
    connection_id: Option<u64>,
) -> Response {
    let target = match action {
        "accept" => ParticipationStatus::Accepted,
        "decline" => ParticipationStatus::Declined,
        "tentativelyAccept" => ParticipationStatus::Tentative,
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                "ErrorInvalidRequest",
                &format!("unsupported event action {action:?}"),
            );
        }
    };
    let event_owned = event.to_string();
    if let Some(o) = super::maybe_override(&state, "rsvp", move |s| {
        crate::lua::req_set_str(s, "event", &event_owned)
    }) {
        return o;
    }

    let log_path = if me_path {
        format!("POST /v1.0/me/events/{event}/{action}")
    } else {
        format!("POST /v1.0/users/{account_id}/events/{event}/{action}")
    };
    state.shared.request_log.record_with_conn(
        "graph",
        log_path,
        json!({ "event": event, "action": action, "response": target.as_graph() }),
        connection_id,
    );

    let self_email = state
        .fixture()
        .account(account_id)
        .map(|a| a.name.clone())
        .unwrap_or_default();

    let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
    let Some(idx) = fix
        .events
        .iter()
        .position(|e| e.account_id == account_id && e.id == event)
    else {
        return error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("event {event:?} not declared in fixture"),
        );
    };
    // Set the authenticated user's own attendee status. Real Graph
    // applies the action to the caller's attendee record; the mock
    // matches the attendee by the account's own address.
    let mut clone = fix.events[idx].clone();
    let mut changed = false;
    for att in &mut clone.attendees {
        if att.email().eq_ignore_ascii_case(&self_email) {
            if att.status != target {
                att.status = target;
                changed = true;
            }
            break;
        }
    }
    if changed {
        let id = clone.id.clone();
        let _ = fix.mutate(|f| {
            f.events[idx] = clone;
            MutationDiff {
                event_updated: vec![id.clone()],
                ..Default::default()
            }
        });
    }
    StatusCode::ACCEPTED.into_response()
}

async fn get_event_me(State(state): State<AppState>, Path(event): Path<String>) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    get_event_impl(state, &account_id, &event).await
}

async fn create_event_me(
    State(state): State<AppState>,
    Path(calendar): Path<String>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
    body: AxumBody,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    create_event_impl(state, &account_id, &calendar, body, true, connection_id).await
}

async fn patch_event_me(
    State(state): State<AppState>,
    Path(event): Path<String>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
    body: AxumBody,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    patch_event_impl(state, &account_id, &event, body, true, connection_id).await
}

async fn delete_event_me(
    State(state): State<AppState>,
    Path(event): Path<String>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    delete_event_impl(state, &account_id, &event, true, connection_id).await
}

// ── Calendar-scoped /me/ wrappers ───────────────────────────────────
//
// The `{calendar}` path segment is accepted (bifrost addresses events
// this way) but not otherwise used: the inner handlers resolve the
// event by id within the account. The calendar-scoped GET serves the
// same body the mailbox-scoped GET does, which is what the
// update/delete pre-read (etag) expects.

async fn get_event_cal_me(
    State(state): State<AppState>,
    Path((_calendar, event)): Path<(String, String)>,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    get_event_impl(state, &account_id, &event).await
}

async fn patch_event_cal_me(
    State(state): State<AppState>,
    Path((_calendar, event)): Path<(String, String)>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
    body: AxumBody,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    patch_event_impl(state, &account_id, &event, body, true, connection_id).await
}

async fn delete_event_cal_me(
    State(state): State<AppState>,
    Path((_calendar, event)): Path<(String, String)>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
) -> Response {
    let account_id = state.fixture().primary_account().id.clone();
    delete_event_impl(state, &account_id, &event, true, connection_id).await
}

// ── /users/{user}/ wrappers ─────────────────────────────────────────

async fn list_calendars_user(
    State(state): State<AppState>,
    Path(user): Path<String>,
    headers: HeaderMap,
    RawQuery(q): RawQuery,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    list_calendars_impl(state, &account_id, headers, q).await
}

async fn get_calendar_user(
    State(state): State<AppState>,
    Path((user, calendar)): Path<(String, String)>,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    get_calendar_impl(state, &account_id, &calendar).await
}

async fn list_events_user(
    State(state): State<AppState>,
    Path((user, calendar)): Path<(String, String)>,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    list_events_impl(state, &account_id, &calendar, raw_query, false).await
}

async fn calendar_view_user(
    State(state): State<AppState>,
    Path((user, calendar)): Path<(String, String)>,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    calendar_view_impl(state, &account_id, &calendar, raw_query, false).await
}

async fn delta_events_user(
    State(state): State<AppState>,
    Path((user, calendar)): Path<(String, String)>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    delta_events_impl(state, &account_id, &calendar, headers, raw_query, false).await
}

async fn get_event_user(
    State(state): State<AppState>,
    Path((user, event)): Path<(String, String)>,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    get_event_impl(state, &account_id, &event).await
}

async fn create_event_user(
    State(state): State<AppState>,
    Path((user, calendar)): Path<(String, String)>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
    body: AxumBody,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    create_event_impl(state, &account_id, &calendar, body, false, connection_id).await
}

async fn patch_event_user(
    State(state): State<AppState>,
    Path((user, event)): Path<(String, String)>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
    body: AxumBody,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    patch_event_impl(state, &account_id, &event, body, false, connection_id).await
}

async fn delete_event_user(
    State(state): State<AppState>,
    Path((user, event)): Path<(String, String)>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    delete_event_impl(state, &account_id, &event, false, connection_id).await
}

// ── Calendar-scoped /users/{user}/ wrappers ─────────────────────────

async fn get_event_cal_user(
    State(state): State<AppState>,
    Path((user, _calendar, event)): Path<(String, String, String)>,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    get_event_impl(state, &account_id, &event).await
}

async fn patch_event_cal_user(
    State(state): State<AppState>,
    Path((user, _calendar, event)): Path<(String, String, String)>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
    body: AxumBody,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    patch_event_impl(state, &account_id, &event, body, false, connection_id).await
}

async fn delete_event_cal_user(
    State(state): State<AppState>,
    Path((user, _calendar, event)): Path<(String, String, String)>,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    delete_event_impl(state, &account_id, &event, false, connection_id).await
}

// ── Inner handlers ──────────────────────────────────────────────────

async fn list_calendars_impl(
    state: AppState,
    account_id: &str,
    headers: HeaderMap,
    _q: Option<String>,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "list_calendars", |_| Ok(())) {
        return o;
    }
    let _ = headers;
    let fixture = state.fixture();
    let value: Vec<Value> = fixture
        .calendars_for(account_id)
        .map(|c| serialize_calendar(&fixture, c))
        .collect();
    ok_json(json!({
        "@odata.context": "https://graph.microsoft.com/v1.0/$metadata#me/calendars",
        "value": value,
    }))
}

async fn get_calendar_impl(state: AppState, account_id: &str, calendar: &str) -> Response {
    let cal_owned = calendar.to_string();
    if let Some(o) = super::maybe_override(&state, "get_calendar", move |s| {
        crate::lua::req_set_str(s, "calendar", &cal_owned)
    }) {
        return o;
    }
    let fixture = state.fixture();
    match resolve_calendar(&fixture, calendar, account_id) {
        Some(c) => ok_json(serialize_calendar(&fixture, c)),
        None => error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("calendar {calendar:?} not declared in fixture"),
        ),
    }
}

async fn list_events_impl(
    state: AppState,
    account_id: &str,
    calendar: &str,
    raw_query: Option<String>,
    me_path: bool,
) -> Response {
    let cal_owned = calendar.to_string();
    if let Some(o) = super::maybe_override(&state, "list_events", move |s| {
        crate::lua::req_set_str(s, "calendar", &cal_owned)
    }) {
        return o;
    }
    let fixture = state.fixture();
    if resolve_calendar(&fixture, calendar, account_id).is_none() {
        return error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("calendar {calendar:?} not declared in fixture"),
        );
    }
    let q = odata::OdataQuery::parse(raw_query.as_deref());
    let top = q.top.unwrap_or(EVENTS_DEFAULT_TOP).clamp(1, EVENTS_MAX_TOP);
    let skip = parse_skiptoken(q.skiptoken.as_deref())
        .or(q.skip)
        .unwrap_or(0);

    let page: Vec<Value> = fixture
        .events_for(account_id)
        .filter(|e| e.calendar_id == calendar)
        .skip(skip as usize)
        .take(top as usize)
        .map(|e| serialize_event(&fixture, e))
        .collect();
    let next_skip = (skip as usize) + page.len();
    let has_more = fixture
        .events_for(account_id)
        .filter(|e| e.calendar_id == calendar)
        .nth(next_skip)
        .is_some();
    let next_link = if has_more {
        let base = if me_path {
            format!("https://graph.microsoft.com/v1.0/me/calendars/{calendar}/events")
        } else {
            format!(
                "https://graph.microsoft.com/v1.0/users/{account_id}/calendars/{calendar}/events"
            )
        };
        Some(format!("{base}?$skiptoken=s.{next_skip}"))
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

/// `GET /v1.0/me/calendars/{id}/calendarView` with `startDateTime` /
/// `endDateTime` - the non-delta time-range read bifrost drives via
/// `events_in_range`. Mirrors `list_events_impl` but filters the
/// calendar's events to those overlapping `[startDateTime,
/// endDateTime)`. The two bounds are non-`$` query params; an absent
/// bound is open on that side. bifrost re-checks the range
/// client-side, so the overlap test stays coarse.
async fn calendar_view_impl(
    state: AppState,
    account_id: &str,
    calendar: &str,
    raw_query: Option<String>,
    me_path: bool,
) -> Response {
    let cal_owned = calendar.to_string();
    if let Some(o) = super::maybe_override(&state, "list_events", move |s| {
        crate::lua::req_set_str(s, "calendar", &cal_owned)
    }) {
        return o;
    }
    let fixture = state.fixture();
    if resolve_calendar(&fixture, calendar, account_id).is_none() {
        return error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("calendar {calendar:?} not declared in fixture"),
        );
    }
    let q = odata::OdataQuery::parse(raw_query.as_deref());
    let start =
        odata::query_param(raw_query.as_deref(), "startDateTime").and_then(|s| parse_view_dt(&s));
    let end =
        odata::query_param(raw_query.as_deref(), "endDateTime").and_then(|s| parse_view_dt(&s));
    let top = q.top.unwrap_or(EVENTS_DEFAULT_TOP).clamp(1, EVENTS_MAX_TOP);
    let skip = parse_skiptoken(q.skiptoken.as_deref())
        .or(q.skip)
        .unwrap_or(0);

    let matching: Vec<&Event> = fixture
        .events_for(account_id)
        .filter(|e| e.calendar_id == calendar)
        .filter(|e| event_overlaps_range(e, start, end))
        .collect();
    let page: Vec<Value> = matching
        .iter()
        .skip(skip as usize)
        .take(top as usize)
        .map(|e| serialize_event(&fixture, e))
        .collect();
    let next_skip = (skip as usize) + page.len();
    let next_link = if matching.len() > next_skip {
        let base = if me_path {
            format!("https://graph.microsoft.com/v1.0/me/calendars/{calendar}/calendarView")
        } else {
            format!(
                "https://graph.microsoft.com/v1.0/users/{account_id}/calendars/{calendar}/calendarView"
            )
        };
        Some(format!("{base}?$skiptoken=s.{next_skip}"))
    } else {
        None
    };

    let mut envelope = Map::new();
    envelope.insert(
        "@odata.context".to_string(),
        Value::String(format!(
            "https://graph.microsoft.com/v1.0/$metadata#me/calendars(\"{calendar}\")/calendarView"
        )),
    );
    envelope.insert("value".to_string(), Value::Array(page));
    if let Some(link) = next_link {
        envelope.insert("@odata.nextLink".to_string(), Value::String(link));
    }
    ok_json(Value::Object(envelope))
}

/// Parse a Graph `calendarView` range bound: an RFC3339 instant
/// (`2026-01-01T00:00:00Z`) or a naive local datetime
/// (`2026-01-01T00:00:00`, treated as UTC, which is how Graph emits
/// `startDateTime` / `endDateTime` without an offset).
fn parse_view_dt(s: &str) -> Option<jiff::Timestamp> {
    if let Ok(dt) = s.parse::<jiff::Timestamp>() {
        return Some(dt);
    }
    jiff::civil::DateTime::strptime("%Y-%m-%dT%H:%M:%S%.f", s)
        .or_else(|_| jiff::civil::DateTime::strptime("%Y-%m-%dT%H:%M:%S", s))
        .ok()
        .and_then(crate::timeutil::utc)
}

/// Recurrence-aware range match. A non-recurring event overlaps
/// `[start, end)` when it ends after `start` and begins before `end`;
/// a recurring event (one carrying a structured `recurrence_rule`)
/// matches when its DTSTART falls before `end` and its RRULE `UNTIL`
/// (when present) does not end the series before `start`. An absent
/// bound is open on that side. See
/// [`crate::recurrence::event_matches_range`].
fn event_overlaps_range(
    e: &Event,
    start: Option<jiff::Timestamp>,
    end: Option<jiff::Timestamp>,
) -> bool {
    crate::recurrence::event_matches_range(e.start, e.end, e.recurrence_rule.as_deref(), start, end)
}

async fn delta_events_impl(
    state: AppState,
    account_id: &str,
    calendar: &str,
    headers: HeaderMap,
    raw_query: Option<String>,
    me_path: bool,
) -> Response {
    let cal_owned = calendar.to_string();
    if let Some(o) = super::maybe_override(&state, "delta_events", move |s| {
        crate::lua::req_set_str(s, "calendar", &cal_owned)
    }) {
        return o;
    }
    let fixture = state.fixture();
    if resolve_calendar(&fixture, calendar, account_id).is_none() {
        return error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("calendar {calendar:?} not declared in fixture"),
        );
    }
    let q = odata::OdataQuery::parse(raw_query.as_deref());
    let host = host_or_default(&headers);
    let path = if me_path {
        format!("/v1.0/me/calendars/{calendar}/calendarView/delta")
    } else {
        format!("/v1.0/users/{account_id}/calendars/{calendar}/calendarView/delta")
    };
    let context = format!(
        "https://graph.microsoft.com/v1.0/$metadata#me/calendars(\"{calendar}\")/calendarView"
    );

    if q.deltatoken.as_deref() == Some("latest") {
        let delta_link = odata::build_delta_link(
            &host,
            &path,
            raw_query.as_deref(),
            fixture.state_for(account_id),
        );
        return ok_json(json!({
            "@odata.context": context,
            "value": [],
            "@odata.deltaLink": delta_link,
        }));
    }

    if let Some(token) = q.deltatoken.as_deref() {
        let raw = odata::decode_deltatoken(token).unwrap_or("");
        if let Some(delta) = fixture.event_delta_since(raw, calendar) {
            let by_id: std::collections::HashMap<&str, &crate::fixture::Event> = fixture
                .events_for(account_id)
                .filter(|e| e.calendar_id == calendar)
                .map(|e| (e.id.as_str(), e))
                .collect();
            let mut value: Vec<Value> = Vec::new();
            for id in delta.created.iter().chain(delta.updated.iter()) {
                if let Some(e) = by_id.get(id.as_str()) {
                    value.push(serialize_event(&fixture, e));
                }
            }
            for id in &delta.destroyed {
                value.push(graph_event_tombstone(id));
            }
            let delta_link = odata::build_delta_link(
                &host,
                &path,
                raw_query.as_deref(),
                fixture.state_for(account_id),
            );
            return ok_json(json!({
                "@odata.context": context,
                "value": value,
                "@odata.deltaLink": delta_link,
            }));
        }
    }

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
        .events_for(account_id)
        .filter(|e| e.calendar_id == calendar)
        .skip(offset as usize)
        .take(top as usize)
        .map(|e| serialize_event(&fixture, e))
        .collect();
    let next_offset_val = (offset as usize) + page.len();
    let has_more = fixture
        .events_for(account_id)
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
            Value::String(odata::build_next_link(
                &host,
                &path,
                raw_query.as_deref(),
                next_off,
            )),
        );
    } else {
        envelope.insert(
            "@odata.deltaLink".to_string(),
            Value::String(odata::build_delta_link(
                &host,
                &path,
                raw_query.as_deref(),
                fixture.state_for(account_id),
            )),
        );
    }
    ok_json(Value::Object(envelope))
}

async fn get_event_impl(state: AppState, account_id: &str, event: &str) -> Response {
    let event_owned = event.to_string();
    if let Some(o) = super::maybe_override(&state, "get_event", move |s| {
        crate::lua::req_set_str(s, "event", &event_owned)
    }) {
        return o;
    }
    let fixture = state.fixture();
    match fixture.events_for(account_id).find(|e| e.id == event) {
        Some(e) => ok_json(serialize_event(&fixture, e)),
        None => error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("event {event:?} not declared in fixture"),
        ),
    }
}

async fn create_event_impl(
    state: AppState,
    account_id: &str,
    calendar: &str,
    body: AxumBody,
    me_path: bool,
    connection_id: Option<u64>,
) -> Response {
    let calendar_known = {
        let fixture = state.fixture();
        resolve_calendar(&fixture, calendar, account_id).is_some()
    };
    if !calendar_known {
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
    let log_path = if me_path {
        format!("POST /v1.0/me/calendars/{calendar}/events")
    } else {
        format!("POST /v1.0/users/{account_id}/calendars/{calendar}/events")
    };
    state.shared.request_log.record_with_conn(
        "graph",
        log_path,
        crate::request_log::body_detail(&parsed),
        connection_id,
    );

    let result: Result<Value, Response> = {
        let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
        let calendar_id = match resolve_calendar(&fix, calendar, account_id) {
            Some(c) => c.id.clone(),
            None => {
                return error(
                    StatusCode::NOT_FOUND,
                    "ResourceNotFound",
                    &format!("calendar {calendar:?} not declared in fixture"),
                );
            }
        };
        let event = match build_event_from_create(&mut fix, account_id, &calendar_id, &parsed) {
            Ok(e) => e,
            Err(resp) => return *resp,
        };
        let server_id = event.id.clone();
        let event_for_serialize = event.clone();
        let _ = fix.mutate(|f| {
            f.events.push(event);
            MutationDiff {
                event_created: vec![server_id.clone()],
                ..Default::default()
            }
        });
        Ok(serialize_event(&fix, &event_for_serialize))
    };
    match result {
        Ok(v) => (StatusCode::CREATED, axum::Json(v)).into_response(),
        Err(resp) => resp,
    }
}

async fn patch_event_impl(
    state: AppState,
    account_id: &str,
    event: &str,
    body: AxumBody,
    me_path: bool,
    connection_id: Option<u64>,
) -> Response {
    let event_known = {
        let fixture = state.fixture();
        fixture.events_for(account_id).any(|e| e.id == event)
    };
    if !event_known {
        return error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("event {event:?} not declared in fixture"),
        );
    }
    let parsed = match parse_json_body(body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let log_path = if me_path {
        format!("PATCH /v1.0/me/events/{event}")
    } else {
        format!("PATCH /v1.0/users/{account_id}/events/{event}")
    };
    state.shared.request_log.record_with_conn(
        "graph",
        log_path,
        crate::request_log::body_detail(&parsed),
        connection_id,
    );

    let result: Result<Value, Response> = {
        let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
        let idx = match fix
            .events
            .iter()
            .position(|e| e.account_id == account_id && e.id == event)
        {
            Some(i) => i,
            None => {
                return error(
                    StatusCode::NOT_FOUND,
                    "ResourceNotFound",
                    &format!("event {event:?} not declared in fixture"),
                );
            }
        };
        let mut clone = fix.events[idx].clone();
        if let Err(resp) = apply_event_patch(&mut clone, &parsed) {
            return *resp;
        }
        let updated_id = clone.id.clone();
        let view = clone.clone();
        let _ = fix.mutate(|f| {
            f.events[idx] = clone;
            MutationDiff {
                event_updated: vec![updated_id.clone()],
                ..Default::default()
            }
        });
        Ok(serialize_event(&fix, &view))
    };
    match result {
        Ok(v) => ok_json(v),
        Err(resp) => resp,
    }
}

async fn delete_event_impl(
    state: AppState,
    account_id: &str,
    event: &str,
    me_path: bool,
    connection_id: Option<u64>,
) -> Response {
    let event_known = {
        let fixture = state.fixture();
        fixture.events_for(account_id).any(|e| e.id == event)
    };
    if !event_known {
        return error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("event {event:?} not declared in fixture"),
        );
    }
    let log_path = if me_path {
        format!("DELETE /v1.0/me/events/{event}")
    } else {
        format!("DELETE /v1.0/users/{account_id}/events/{event}")
    };
    state.shared.request_log.record_with_conn(
        "graph",
        log_path,
        json!({ "id": event }),
        connection_id,
    );

    {
        let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
        let event_id = event.to_string();
        let acct = account_id.to_string();
        // Snapshot the parent calendar BEFORE retain, so the
        // tombstone the change_log records carries the right
        // parent for per-calendar delta filtering.
        let parent = fix
            .events
            .iter()
            .find(|e| e.account_id == acct && e.id == event_id)
            .map(|e| e.calendar_id.clone());
        let _ = fix.mutate(|f| {
            let len_before = f.events.len();
            f.events
                .retain(|e| !(e.account_id == acct && e.id == event_id));
            if f.events.len() < len_before {
                MutationDiff {
                    event_destroyed: vec![event_id.clone()],
                    event_destroyed_parents: vec![parent.clone().unwrap_or_default()],
                    ..Default::default()
                }
            } else {
                MutationDiff::default()
            }
        });
    }
    StatusCode::NO_CONTENT.into_response()
}

// ── Mutation helpers ────────────────────────────────────────────────

fn build_event_from_create(
    fix: &mut Fixture,
    account_id: &str,
    calendar_id: &str,
    body: &Value,
) -> Result<Event, Box<Response>> {
    let obj = body.as_object().ok_or_else(|| {
        Box::new(error(
            StatusCode::BAD_REQUEST,
            "BadRequest",
            "create body must be a JSON object",
        ))
    })?;
    let subject = obj
        .get("subject")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let start = parse_graph_datetime(obj.get("start")).ok_or_else(|| {
        Box::new(error(
            StatusCode::BAD_REQUEST,
            "BadRequest",
            "missing or malformed start.dateTime",
        ))
    })?;
    let end = parse_graph_datetime(obj.get("end")).ok_or_else(|| {
        Box::new(error(
            StatusCode::BAD_REQUEST,
            "BadRequest",
            "missing or malformed end.dateTime",
        ))
    })?;
    let body_text = obj
        .get("body")
        .and_then(|v| v.get("content"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let location = obj
        .get("location")
        .and_then(|v| v.get("displayName"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let is_all_day = obj
        .get("isAllDay")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let attendees = obj
        .get("attendees")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(parse_graph_attendee).collect())
        .unwrap_or_default();
    let id = fix.mint_event_id();
    let recurrence_rule = obj.get("recurrence").and_then(parse_graph_recurrence);
    Ok(Event {
        id,
        account_id: account_id.to_string(),
        calendar_id: calendar_id.to_string(),
        subject,
        body_preview: None,
        body_text,
        start,
        end,
        location,
        organizer: None,
        attendees,
        is_all_day,
        recurrence_rule,
        // Graph models exceptions as exception-event resources
        // rather than EXDATE arrays, so the create body has no
        // place to author EXDATEs. The fixture's static authoring
        // shape keeps the field accessible if needed.
        recurrence_exdates: vec![],
        // Graph keeps UTC on the shared read surface and carries no
        // reminders / raw-ical; the static `[[event]]` shape stages
        // those for the CalDAV / JMAP projections.
        time_zone: None,
        reminders: Vec::new(),
        raw_ical: None,
    })
}

fn apply_event_patch(event: &mut Event, body: &Value) -> Result<(), Box<Response>> {
    let obj = body.as_object().ok_or_else(|| {
        Box::new(error(
            StatusCode::BAD_REQUEST,
            "BadRequest",
            "patch body must be a JSON object",
        ))
    })?;
    for (k, v) in obj {
        match k.as_str() {
            "subject" => {
                event.subject = v.as_str().unwrap_or("").to_string();
            }
            "start" => {
                event.start = parse_graph_datetime(Some(v)).ok_or_else(|| {
                    Box::new(error(
                        StatusCode::BAD_REQUEST,
                        "BadRequest",
                        "malformed start.dateTime in patch",
                    ))
                })?;
            }
            "end" => {
                event.end = parse_graph_datetime(Some(v)).ok_or_else(|| {
                    Box::new(error(
                        StatusCode::BAD_REQUEST,
                        "BadRequest",
                        "malformed end.dateTime in patch",
                    ))
                })?;
            }
            "body" => {
                event.body_text = v.get("content").and_then(Value::as_str).map(str::to_string);
            }
            "location" => {
                event.location = v
                    .get("displayName")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            "isAllDay" => {
                event.is_all_day = v.as_bool().unwrap_or(false);
            }
            "attendees" => {
                event.attendees = v
                    .as_array()
                    .map(|arr| arr.iter().filter_map(parse_graph_attendee).collect())
                    .unwrap_or_default();
            }
            "recurrence" => {
                // A PATCH carrying `recurrence: null` clears
                // recurrence; a structured pattern/range replaces
                // it. An unparseable pattern (FREQ we don't model)
                // also clears, on the theory that round-tripping
                // a malformed Graph body to a half-translated
                // RRULE would be worse than dropping it.
                if v.is_null() {
                    event.recurrence_rule = None;
                    event.recurrence_exdates.clear();
                } else {
                    event.recurrence_rule = parse_graph_recurrence(v);
                    event.recurrence_exdates.clear();
                }
            }
            _ => {
                // Quietly ignore properties v0 doesn't model.
            }
        }
    }
    Ok(())
}

fn parse_graph_datetime(v: Option<&Value>) -> Option<jiff::Timestamp> {
    let s = v?.get("dateTime")?.as_str()?;
    let normalised = if s.ends_with('Z') || s.contains('+') || s.contains('-') {
        s.to_string()
    } else {
        format!("{s}Z")
    };
    crate::fixture::parse_ts(&normalised).ok()
}

fn parse_graph_attendee(v: &Value) -> Option<EventAttendee> {
    let email = v
        .get("emailAddress")
        .and_then(|e| e.get("address"))
        .and_then(Value::as_str)?;
    let name = v
        .get("emailAddress")
        .and_then(|e| e.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|n| !n.is_empty());
    // Graph carries the attendee's RSVP under `status.response`
    // (`notResponded` / `accepted` / `declined` / `tentativelyAccepted`).
    // An absent / unrecognised value defaults to needs-action.
    let status = v
        .get("status")
        .and_then(|s| s.get("response"))
        .and_then(Value::as_str)
        .and_then(ParticipationStatus::parse)
        .unwrap_or_default();
    Some(EventAttendee {
        address: Address {
            email: email.to_string(),
            name,
        },
        status,
    })
}

// Err is an axum `Response` (large); allow rather than box (every `?`
// caller would otherwise have to unbox).
#[allow(clippy::result_large_err)]
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

// ── Serialisation ───────────────────────────────────────────────────

/// Resolve a calendar within one account. `default` resolves to that
/// account's default calendar (or its first if none is flagged
/// default). Scoping prevents the alias from leaking across accounts.
fn resolve_calendar<'a>(
    fixture: &'a Fixture,
    id_or_alias: &str,
    account_id: &'a str,
) -> Option<&'a Calendar> {
    if id_or_alias == "default" {
        let mut iter = fixture.calendars_for(account_id);
        let first = iter.next();
        return fixture
            .calendars_for(account_id)
            .find(|c| c.is_default)
            .or(first);
    }
    fixture
        .calendars_for(account_id)
        .find(|c| c.id == id_or_alias)
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
    out.insert(
        "calendarId".to_string(),
        Value::String(e.calendar_id.clone()),
    );
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
            "dateTime": format!("{:.0}", e.start),
            "timeZone": "UTC",
        }),
    );
    out.insert(
        "end".to_string(),
        json!({
            "dateTime": format!("{:.0}", e.end),
            "timeZone": "UTC",
        }),
    );
    out.insert("isAllDay".to_string(), Value::Bool(e.is_all_day));
    if let Some(loc) = &e.location {
        out.insert("location".to_string(), json!({ "displayName": loc }));
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
                    "name": a.address.name.clone().unwrap_or_default(),
                    "address": a.address.email,
                },
                "type": "required",
                "status": {
                    "response": a.status.as_graph(),
                    "time": "0001-01-01T00:00:00Z",
                },
            })
        })
        .collect();
    out.insert("attendees".to_string(), Value::Array(attendees));
    if let Some(rrule) = &e.recurrence_rule {
        let parsed = crate::recurrence::ParsedRule::parse(rrule);
        if let Some(recurrence) = graph_recurrence(&parsed, e.start) {
            out.insert("recurrence".to_string(), recurrence);
        }
    }
    Value::Object(out)
}

/// Translate a parsed RRULE into Graph's structured `recurrence`
/// object. See `reference/ratatoskr-graph-surface.md` for the schema
/// reference and Stage-1 scope.
fn graph_recurrence(
    rule: &crate::recurrence::ParsedRule,
    start: jiff::Timestamp,
) -> Option<Value> {
    use crate::recurrence::Frequency;
    let freq = rule.freq?;
    let interval = rule.interval.unwrap_or(1);

    let pattern = match freq {
        Frequency::Daily => json!({
            "type": "daily",
            "interval": interval,
        }),
        Frequency::Weekly => {
            let days = if rule.by_day.is_empty() {
                vec![weekday_of(start).graph().to_string()]
            } else {
                rule.by_day
                    .iter()
                    .map(|d| d.weekday.graph().to_string())
                    .collect()
            };
            json!({
                "type": "weekly",
                "interval": interval,
                "daysOfWeek": days,
                "firstDayOfWeek": "sunday",
            })
        }
        Frequency::Monthly => {
            if let Some(&day) = rule.by_month_day.first().filter(|d| **d > 0) {
                json!({
                    "type": "absoluteMonthly",
                    "interval": interval,
                    "dayOfMonth": day,
                })
            } else if let Some(first) = rule.by_day.first().filter(|d| d.ordinal.is_some()) {
                let index = graph_week_index(first.ordinal.unwrap_or(1));
                json!({
                    "type": "relativeMonthly",
                    "interval": interval,
                    "daysOfWeek": [first.weekday.graph()],
                    "index": index,
                })
            } else {
                json!({
                    "type": "absoluteMonthly",
                    "interval": interval,
                    "dayOfMonth": u32::try_from(crate::timeutil::civil_utc(start).day()).unwrap_or(1),
                })
            }
        }
        Frequency::Yearly => {
            let month = rule
                .by_month
                .first()
                .copied()
                .unwrap_or_else(|| {
                    u8::try_from(crate::timeutil::civil_utc(start).month()).unwrap_or(1)
                });
            if let Some(&day) = rule.by_month_day.first().filter(|d| **d > 0) {
                json!({
                    "type": "absoluteYearly",
                    "interval": interval,
                    "dayOfMonth": day,
                    "month": month,
                })
            } else {
                json!({
                    "type": "absoluteYearly",
                    "interval": interval,
                    "dayOfMonth": u32::try_from(crate::timeutil::civil_utc(start).day()).unwrap_or(1),
                    "month": month,
                })
            }
        }
    };

    let start_date = start.strftime("%Y-%m-%d").to_string();
    let range = if let Some(count) = rule.count {
        json!({
            "type": "numbered",
            "startDate": start_date,
            "numberOfOccurrences": count,
        })
    } else if let Some(until) = rule.until {
        json!({
            "type": "endDate",
            "startDate": start_date,
            "endDate": until.strftime("%Y-%m-%d").to_string(),
        })
    } else {
        json!({
            "type": "noEnd",
            "startDate": start_date,
        })
    };

    Some(json!({ "pattern": pattern, "range": range }))
}

fn weekday_of(dt: jiff::Timestamp) -> crate::recurrence::Weekday {
    use crate::recurrence::Weekday;
    match crate::timeutil::civil_utc(dt).weekday() {
        jiff::civil::Weekday::Monday => Weekday::Mo,
        jiff::civil::Weekday::Tuesday => Weekday::Tu,
        jiff::civil::Weekday::Wednesday => Weekday::We,
        jiff::civil::Weekday::Thursday => Weekday::Th,
        jiff::civil::Weekday::Friday => Weekday::Fr,
        jiff::civil::Weekday::Saturday => Weekday::Sa,
        jiff::civil::Weekday::Sunday => Weekday::Su,
    }
}

fn graph_week_index(ordinal: i32) -> &'static str {
    match ordinal {
        1 => "first",
        2 => "second",
        3 => "third",
        4 => "fourth",
        -1 => "last",
        _ => "first",
    }
}

fn graph_index_to_ordinal(index: &str) -> i32 {
    match index.to_ascii_lowercase().as_str() {
        "first" => 1,
        "second" => 2,
        "third" => 3,
        "fourth" => 4,
        "last" => -1,
        _ => 1,
    }
}

/// Inverse of [`graph_recurrence`]. Read Graph's structured
/// `recurrence: { pattern, range }` body and synthesise an RFC
/// 5545 RRULE string the fixture can store. Returns None when the
/// body is missing pattern/range or carries a `pattern.type`
/// outside the daily / weekly / absoluteMonthly / relativeMonthly
/// / absoluteYearly set v0 models. Graph's `range.endDate` lands
/// in UNTIL at `T235959Z` (end-of-day UTC) since Graph dates are
/// inclusive and the iCal UNTIL is a date-time the spec defines
/// to include the last instance.
///
/// Graph models exceptions as a separate seriesMaster + exception
/// event pair rather than EXDATE arrays, so this function only
/// returns the RRULE string; recurrence_exdates stays empty on
/// the write path (callers can still author EXDATEs via the
/// fixture's static authoring shape).
fn parse_graph_recurrence(v: &Value) -> Option<String> {
    use crate::recurrence::{ByDay, Frequency, ParsedRule, Weekday};
    let obj = v.as_object()?;
    let pattern = obj.get("pattern")?.as_object()?;
    let range = obj.get("range")?.as_object()?;
    let ptype = pattern.get("type")?.as_str()?;
    let interval = pattern
        .get("interval")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());
    let mut rule = ParsedRule {
        freq: None,
        interval,
        ..Default::default()
    };
    match ptype.to_ascii_lowercase().as_str() {
        "daily" => rule.freq = Some(Frequency::Daily),
        "weekly" => {
            rule.freq = Some(Frequency::Weekly);
            if let Some(days) = pattern.get("daysOfWeek").and_then(Value::as_array) {
                rule.by_day = days
                    .iter()
                    .filter_map(|d| d.as_str().and_then(Weekday::parse_long))
                    .map(|w| ByDay {
                        ordinal: None,
                        weekday: w,
                    })
                    .collect();
            }
        }
        "absolutemonthly" => {
            rule.freq = Some(Frequency::Monthly);
            if let Some(d) = pattern
                .get("dayOfMonth")
                .and_then(Value::as_i64)
                .and_then(|n| i8::try_from(n).ok())
            {
                rule.by_month_day = vec![d];
            }
        }
        "relativemonthly" => {
            rule.freq = Some(Frequency::Monthly);
            let index_s = pattern
                .get("index")
                .and_then(Value::as_str)
                .unwrap_or("first");
            let ordinal = graph_index_to_ordinal(index_s);
            if let Some(days) = pattern.get("daysOfWeek").and_then(Value::as_array)
                && let Some(w) = days
                    .iter()
                    .filter_map(|d| d.as_str().and_then(Weekday::parse_long))
                    .next()
            {
                rule.by_day = vec![ByDay {
                    ordinal: Some(ordinal),
                    weekday: w,
                }];
            }
        }
        "absoluteyearly" => {
            rule.freq = Some(Frequency::Yearly);
            if let Some(d) = pattern
                .get("dayOfMonth")
                .and_then(Value::as_i64)
                .and_then(|n| i8::try_from(n).ok())
            {
                rule.by_month_day = vec![d];
            }
            if let Some(m) = pattern
                .get("month")
                .and_then(Value::as_u64)
                .and_then(|n| u8::try_from(n).ok())
            {
                rule.by_month = vec![m];
            }
        }
        _ => return None,
    }

    let rtype = range.get("type").and_then(Value::as_str).unwrap_or("noEnd");
    match rtype.to_ascii_lowercase().as_str() {
        "noend" => {}
        "enddate" => {
            if let Some(s) = range.get("endDate").and_then(Value::as_str)
                && let Ok(d) = jiff::civil::Date::strptime("%Y-%m-%d", s)
            {
                rule.until = crate::timeutil::utc(d.at(23, 59, 59, 0));
            }
        }
        "numbered" => {
            if let Some(n) = range
                .get("numberOfOccurrences")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
            {
                rule.count = Some(n);
            }
        }
        _ => {}
    }

    crate::recurrence::format_rrule(&rule)
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Graph-style deletion stub for `events/delta`.
fn graph_event_tombstone(id: &str) -> Value {
    json!({
        "id": id,
        "@removed": { "reason": "deleted" },
    })
}

fn host_or_default(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("graph.microsoft.com")
        .to_string()
}

fn parse_skiptoken(t: Option<&str>) -> Option<u32> {
    let s = t?;
    s.strip_prefix("s.")?.parse().ok()
}
