#![allow(clippy::unwrap_used)]

//! Google Calendar v3 integration tests against
//! `fixtures/graph-calendar-small.toml`. Drives the same data the
//! Graph and JMAP calendar tests use, projected through the
//! Google-shaped wire format.

use std::path::Path;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use saehrimnir::{fixture, gcal, graph, shared, smtp};

fn router() -> axum::Router {
    let fix = fixture::load(Path::new("fixtures/graph-calendar-small.toml")).unwrap();
    gcal::router(gcal::AppState::for_test(shared::handle(fix)))
}

/// Two routers (gcal + graph) sharing one fixture handle. Used for
/// the cross-protocol delta visibility test.
fn cross_protocol_routers() -> (axum::Router, axum::Router) {
    let fix = fixture::load(Path::new("fixtures/graph-calendar-small.toml")).unwrap();
    let shared_handles = shared::SharedHandles::for_test(shared::handle(fix));
    let gcal_state = gcal::AppState {
        shared: shared_handles.clone(),
    };
    let graph_state = graph::AppState {
        shared: shared_handles,
    };
    let _ = smtp::SubmissionLog::default();
    (gcal::router(gcal_state), graph::router(graph_state))
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn http(r: &axum::Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    let body = match body {
        Some(v) => Body::from(serde_json::to_vec(&v).unwrap()),
        None => Body::empty(),
    };
    let resp = r.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let v = body_json(resp).await;
    (status, v)
}

#[tokio::test]
async fn calendar_list_emits_calendar_list_entries() {
    let r = router();
    let (status, v) = http(&r, "GET", "/calendar/v3/users/me/calendarList", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["kind"], "calendar#calendarList");
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    let cal_work = items.iter().find(|c| c["id"] == "cal-work").unwrap();
    assert_eq!(cal_work["summary"], "Work");
    assert_eq!(cal_work["primary"], true);
    assert_eq!(cal_work["accessRole"], "owner");
    assert_eq!(cal_work["backgroundColor"], "lightBlue");
    let cal_personal = items.iter().find(|c| c["id"] == "cal-personal").unwrap();
    assert!(cal_personal.get("primary").is_none());
}

#[tokio::test]
async fn list_events_returns_full_set_with_sync_token() {
    let r = router();
    let (status, v) = http(
        &r,
        "GET",
        "/calendar/v3/calendars/cal-work/events?maxResults=50&singleEvents=true",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 2); // ev-001 + ev-002
    let ev = items.iter().find(|e| e["id"] == "ev-001").unwrap();
    assert_eq!(ev["summary"], "Standup");
    assert_eq!(ev["start"]["dateTime"], "2026-01-15T09:00:00Z");
    assert_eq!(ev["start"]["timeZone"], "UTC");
    assert_eq!(ev["organizer"]["email"], "alice@example.com");
    assert_eq!(ev["attendees"][0]["email"], "bob@example.com");
    assert!(v["nextSyncToken"].is_string());
    assert!(v["nextPageToken"].is_null());
}

#[tokio::test]
async fn list_events_paginates_with_max_results() {
    let r = router();
    let (status, page1) = http(
        &r,
        "GET",
        "/calendar/v3/calendars/cal-work/events?maxResults=1",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page1["items"].as_array().unwrap().len(), 1);
    let token = page1["nextPageToken"].as_str().unwrap().to_string();
    assert!(page1["nextSyncToken"].is_null());

    let (_, page2) = http(
        &r,
        "GET",
        &format!("/calendar/v3/calendars/cal-work/events?maxResults=1&pageToken={token}"),
        None,
    )
    .await;
    assert_eq!(page2["items"].as_array().unwrap().len(), 1);
    assert!(page2["nextPageToken"].is_null());
    assert!(page2["nextSyncToken"].is_string());
}

#[tokio::test]
async fn list_events_with_known_sync_token_returns_empty_delta() {
    let r = router();
    let (_, full) = http(&r, "GET", "/calendar/v3/calendars/cal-work/events", None).await;
    let token = full["nextSyncToken"].as_str().unwrap().to_string();
    let (status, follow) = http(
        &r,
        "GET",
        &format!("/calendar/v3/calendars/cal-work/events?syncToken={token}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(follow["items"].as_array().unwrap().len(), 0);
    assert_eq!(follow["nextSyncToken"], token);
}

#[tokio::test]
async fn list_events_with_unknown_sync_token_returns_410() {
    let r = router();
    let (status, v) = http(
        &r,
        "GET",
        "/calendar/v3/calendars/cal-work/events?syncToken=ancient",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::GONE);
    assert_eq!(v["error"]["code"], 410);
    assert_eq!(v["error"]["errors"][0]["reason"], "fullSyncRequired");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("sync token"));
}

#[tokio::test]
async fn create_patch_delete_round_trip() {
    let r = router();

    // Create.
    let (status, created) = http(
        &r,
        "POST",
        "/calendar/v3/calendars/cal-personal/events",
        Some(json!({
            "summary": "Lunch",
            "description": "with Bob",
            "start": {"dateTime": "2026-03-01T12:00:00Z", "timeZone": "UTC"},
            "end": {"dateTime": "2026-03-01T13:00:00Z", "timeZone": "UTC"}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let new_id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["summary"], "Lunch");
    assert_eq!(created["start"]["dateTime"], "2026-03-01T12:00:00Z");

    // Patch.
    let (status, patched) = http(
        &r,
        "PATCH",
        &format!("/calendar/v3/calendars/cal-personal/events/{new_id}"),
        Some(json!({"summary": "Lunch (rescheduled)"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(patched["summary"], "Lunch (rescheduled)");

    // Delete.
    let resp = router()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!(
                    "/calendar/v3/calendars/cal-personal/events/{new_id}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // The fresh router doesn't share state with `r`, so the
    // delete fails - re-issue against the right router.
    drop(resp);
    let resp = r
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!(
                    "/calendar/v3/calendars/cal-personal/events/{new_id}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn unknown_calendar_returns_404() {
    let r = router();
    let (status, v) = http(
        &r,
        "GET",
        "/calendar/v3/calendars/cal-nope/events",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["errors"][0]["reason"], "notFound");
}

#[tokio::test]
async fn list_events_emits_cancelled_tombstone_after_destroy() {
    // Regression: pre-fix, a known-but-stale syncToken returned a
    // full live-events list with no cancelled tombstones, so a
    // deleted event silently persisted in the client's local DB.
    // Now the handler walks the change_log (via
    // `event_delta_since`, which is already calendar-filtered)
    // and emits `{id, status: "cancelled"}` per RFC contract.
    let r = router();

    // Bootstrap → save token; fixture has ev-001 + ev-002 in
    // cal-work.
    let (_, bootstrap) = http(
        &r,
        "GET",
        "/calendar/v3/calendars/cal-work/events",
        None,
    )
    .await;
    let token = bootstrap["nextSyncToken"].as_str().unwrap().to_string();
    assert_eq!(bootstrap["items"].as_array().unwrap().len(), 2);

    // Delete ev-001 via the gcal mutate path.
    let resp = r
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/calendar/v3/calendars/cal-work/events/ev-001")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Replay with the saved token: ev-001 must surface as a
    // cancelled tombstone, ev-002 stays live (and may or may not
    // surface depending on whether it was touched - here it
    // wasn't, so the delta is just the destroy).
    let (status, follow) = http(
        &r,
        "GET",
        &format!("/calendar/v3/calendars/cal-work/events?syncToken={token}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = follow["items"].as_array().unwrap();
    let cancelled = items
        .iter()
        .find(|e| e["id"] == "ev-001")
        .unwrap_or_else(|| panic!("expected tombstone for ev-001; got {items:?}"));
    assert_eq!(
        cancelled["status"], "cancelled",
        "tombstone must carry status=cancelled"
    );
    // ev-002 wasn't modified, so it should NOT appear in the
    // delta even though it's still live.
    assert!(
        !items.iter().any(|e| e["id"] == "ev-002"),
        "untouched event must not surface in delta; got {items:?}"
    );
    assert_ne!(follow["nextSyncToken"].as_str().unwrap(), token);
}

#[tokio::test]
async fn gcal_create_surfaces_in_graph_calendar_view_delta() {
    let (gcal_r, graph_r) = cross_protocol_routers();

    // Seed Graph deltaLink.
    let resp = graph_r
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1.0/me/calendars/cal-work/calendarView/delta?startDateTime=2026-01-01T00:00:00Z&endDateTime=2027-01-01T00:00:00Z")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bootstrap = body_json(resp).await;
    let delta_link = bootstrap["@odata.deltaLink"].as_str().unwrap().to_string();

    // Google Calendar create.
    let (status, _) = http(
        &gcal_r,
        "POST",
        "/calendar/v3/calendars/cal-work/events",
        Some(json!({
            "summary": "Cross-protocol",
            "start": {"dateTime": "2026-05-01T15:00:00Z", "timeZone": "UTC"},
            "end": {"dateTime": "2026-05-01T15:45:00Z", "timeZone": "UTC"}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Replay Graph deltaLink.
    let path = delta_link
        .split_once("/v1.0/")
        .map(|(_, rest)| format!("/v1.0/{rest}"))
        .unwrap();
    let resp = graph_r
        .oneshot(
            Request::builder()
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let titles: Vec<&str> = v["value"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e.get("subject").and_then(Value::as_str))
        .collect();
    assert!(
        titles.contains(&"Cross-protocol"),
        "expected gcal-created event in Graph delta; got {:?}",
        v["value"]
    );
}
