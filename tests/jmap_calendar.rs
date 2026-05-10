#![allow(clippy::unwrap_used)]

//! JMAP calendar integration tests against the
//! `fixtures/graph-calendar-small.toml` scenario.
//!
//! Coverage: session capability advertisement, `Calendar/get`,
//! `CalendarEvent/get`, `CalendarEvent/changes` against the seed
//! state, `CalendarEvent/set` create / update / destroy, and a
//! cross-protocol assertion that a JMAP create surfaces in Graph
//! `calendarView/delta`.

use std::path::Path;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use saehrimnir::{fixture, graph, routes, shared, smtp};

fn router() -> axum::Router {
    let fix = fixture::load(Path::new("fixtures/graph-calendar-small.toml")).unwrap();
    routes::router(routes::AppState::for_test(shared::handle(fix)))
}

/// Two routers (JMAP + Graph) sharing one fixture handle. Used to
/// assert that a JMAP mutation surfaces in a Graph delta call.
fn cross_protocol_routers() -> (axum::Router, axum::Router) {
    let fix = fixture::load(Path::new("fixtures/graph-calendar-small.toml")).unwrap();
    let shared_handles = shared::SharedHandles::for_test(shared::handle(fix));
    let jmap_state = routes::AppState {
        shared: shared_handles.clone(),
        submission_log: smtp::SubmissionLog::default(),
        base_url: "http://localhost".into(),
    };
    let graph_state = graph::AppState {
        shared: shared_handles,
    };
    (routes::router(jmap_state), graph::router(graph_state))
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn graph_get(r: &axum::Router, uri: &str) -> Value {
    let resp = r
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "GET {uri}");
    body_json(resp).await
}

async fn jmap_call(r: &axum::Router, method: &str, args: Value) -> Value {
    let req_body = json!({
        "using": [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:calendars"
        ],
        "methodCalls": [[method, args, "c0"]],
    });
    let resp = r
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/jmap/api")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&req_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    v["methodResponses"][0].clone()
}

#[tokio::test]
async fn session_advertises_calendars_when_fixture_has_calendars() {
    let resp = router()
        .oneshot(
            Request::builder()
                .uri("/jmap/session")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let v = body_json(resp).await;
    let caps = v["capabilities"].as_object().unwrap();
    assert!(caps.contains_key("urn:ietf:params:jmap:calendars"));
    let acct = v["accounts"]["account-1"]["accountCapabilities"]
        .as_object()
        .unwrap();
    assert!(acct.contains_key("urn:ietf:params:jmap:calendars"));
    assert_eq!(
        v["primaryAccounts"]["urn:ietf:params:jmap:calendars"],
        "account-1"
    );
}

#[tokio::test]
async fn calendar_get_returns_all_calendars() {
    let r = router();
    let resp = jmap_call(&r, "Calendar/get", json!({"accountId": "account-1"})).await;
    assert_eq!(resp[0], "Calendar/get");
    let body = &resp[1];
    let list = body["list"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["id"], "cal-work");
    assert_eq!(list[0]["name"], "Work");
    assert_eq!(list[0]["isDefault"], true);
    assert_eq!(list[0]["color"], "lightBlue");
    assert_eq!(list[0]["myRights"]["mayWriteAll"], true);
    assert_eq!(list[1]["id"], "cal-personal");
    assert_eq!(list[1]["isDefault"], false);
}

#[tokio::test]
async fn calendar_get_filters_by_ids_and_reports_not_found() {
    let r = router();
    let resp = jmap_call(
        &r,
        "Calendar/get",
        json!({"accountId": "account-1", "ids": ["cal-work", "cal-bogus"]}),
    )
    .await;
    let body = &resp[1];
    let list = body["list"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], "cal-work");
    assert_eq!(body["notFound"], json!(["cal-bogus"]));
}

#[tokio::test]
async fn calendar_event_get_emits_jscalendar_shape() {
    let r = router();
    let resp = jmap_call(&r, "CalendarEvent/get", json!({"accountId": "account-1"})).await;
    let body = &resp[1];
    let list = body["list"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    let ev = &list[0];
    assert_eq!(ev["@type"], "Event");
    assert_eq!(ev["id"], "ev-001");
    assert_eq!(ev["uid"], "ev-001");
    assert_eq!(ev["calendarIds"]["cal-work"], true);
    assert_eq!(ev["title"], "Standup");
    assert_eq!(ev["start"], "2026-01-15T09:00:00");
    assert_eq!(ev["duration"], "PT15M");
    assert_eq!(ev["timeZone"], "UTC");
    assert_eq!(ev["status"], "confirmed");
    assert_eq!(ev["locations"]["loc1"]["name"], "Conf Room A");
    let parts = ev["participants"].as_object().unwrap();
    let org = parts.get("org").unwrap();
    assert_eq!(org["email"], "alice@example.com");
    assert_eq!(org["sendTo"]["imip"], "mailto:alice@example.com");
    assert_eq!(org["roles"]["owner"], true);
    assert_eq!(parts.len(), 3); // organizer + 2 attendees
}

#[tokio::test]
async fn calendar_event_changes_returns_empty_at_seed_state() {
    let r = router();
    let resp = jmap_call(
        &r,
        "CalendarEvent/changes",
        json!({"accountId": "account-1", "sinceState": "fixture-state"}),
    )
    .await;
    let body = &resp[1];
    assert_eq!(body["oldState"], "fixture-state");
    assert_eq!(body["newState"], "fixture-state");
    assert_eq!(body["created"], json!([]));
    assert_eq!(body["updated"], json!([]));
    assert_eq!(body["destroyed"], json!([]));
    assert_eq!(body["hasMoreChanges"], false);
}

#[tokio::test]
async fn calendar_event_changes_unknown_state_returns_cannot_calculate() {
    let r = router();
    let resp = jmap_call(
        &r,
        "CalendarEvent/changes",
        json!({"accountId": "account-1", "sinceState": "ancient-and-gone"}),
    )
    .await;
    assert_eq!(resp[0], "error");
    assert_eq!(resp[1]["type"], "cannotCalculateChanges");
}

#[tokio::test]
async fn calendar_event_set_create_update_destroy_round_trip() {
    let r = router();

    // Create.
    let create = jmap_call(
        &r,
        "CalendarEvent/set",
        json!({
            "accountId": "account-1",
            "create": {
                "new1": {
                    "@type": "Event",
                    "calendarIds": {"cal-personal": true},
                    "title": "Lunch",
                    "description": "with Bob",
                    "start": "2026-03-01T12:00:00",
                    "duration": "PT1H",
                    "timeZone": "UTC",
                    "participants": {
                        "p1": {
                            "@type": "Participant",
                            "name": "Bob",
                            "sendTo": {"imip": "mailto:bob@example.com"},
                            "roles": {"attendee": true}
                        }
                    }
                }
            }
        }),
    )
    .await;
    let body = &create[1];
    let new_id = body["created"]["new1"]["id"].as_str().unwrap().to_string();
    assert_eq!(body["created"]["new1"]["uid"], new_id);
    assert_ne!(body["oldState"], body["newState"]);

    // CalendarEvent/get observes the create.
    let after_create = jmap_call(
        &r,
        "CalendarEvent/get",
        json!({"accountId": "account-1", "ids": [new_id]}),
    )
    .await;
    assert_eq!(after_create[1]["list"][0]["title"], "Lunch");
    assert_eq!(after_create[1]["list"][0]["calendarIds"]["cal-personal"], true);

    // Update.
    let updated = jmap_call(
        &r,
        "CalendarEvent/set",
        json!({
            "accountId": "account-1",
            "update": { new_id.clone(): {"title": "Lunch (rescheduled)"} }
        }),
    )
    .await;
    let updated_obj = updated[1]["updated"].as_object().unwrap();
    assert!(updated_obj.contains_key(&new_id));
    let after_update = jmap_call(
        &r,
        "CalendarEvent/get",
        json!({"accountId": "account-1", "ids": [&new_id]}),
    )
    .await;
    assert_eq!(
        after_update[1]["list"][0]["title"],
        "Lunch (rescheduled)"
    );

    // Destroy.
    let destroyed = jmap_call(
        &r,
        "CalendarEvent/set",
        json!({"accountId": "account-1", "destroy": [&new_id]}),
    )
    .await;
    assert_eq!(destroyed[1]["destroyed"], json!([new_id]));
    let after_destroy = jmap_call(
        &r,
        "CalendarEvent/get",
        json!({"accountId": "account-1", "ids": [&new_id]}),
    )
    .await;
    assert_eq!(after_destroy[1]["list"], json!([]));
    assert_eq!(after_destroy[1]["notFound"], json!([new_id]));
}

#[tokio::test]
async fn calendar_event_set_creates_visible_in_changes_delta() {
    let r = router();
    let seed = "fixture-state";

    let _ = jmap_call(
        &r,
        "CalendarEvent/set",
        json!({
            "accountId": "account-1",
            "create": {
                "new1": {
                    "calendarIds": {"cal-work": true},
                    "title": "Late add",
                    "start": "2026-04-01T10:00:00",
                    "duration": "PT30M"
                }
            }
        }),
    )
    .await;

    let delta = jmap_call(
        &r,
        "CalendarEvent/changes",
        json!({"accountId": "account-1", "sinceState": seed}),
    )
    .await;
    let body = &delta[1];
    let created = body["created"].as_array().unwrap();
    assert_eq!(created.len(), 1);
    assert_ne!(body["newState"], seed);
}

#[tokio::test]
async fn calendar_event_changes_does_not_leak_across_calendars() {
    // Regression: pre-fix, `event_delta_since` filtered tombstones
    // by parent calendar but left created/updated unfiltered, so a
    // create in cal A surfaced in cal B's per-calendar walk and the
    // JMAP cross-calendar union over-reported on multi-calendar
    // fixtures. Post-fix, JMAP uses `event_delta_since_any` which
    // deliberately skips per-calendar filtering, and Graph's
    // per-calendar callers see only their own events because
    // `event_delta_since` filters created/updated against the live
    // event's `calendar_id` too.
    let r = router();

    // Create one event each in cal-work and cal-personal. Both
    // bumps land in the change log between the seed state and
    // current.
    let _ = jmap_call(
        &r,
        "CalendarEvent/set",
        json!({
            "accountId": "account-1",
            "create": {
                "wA": {
                    "calendarIds": {"cal-work": true},
                    "title": "in work",
                    "start": "2026-04-01T10:00:00",
                    "duration": "PT30M"
                }
            }
        }),
    )
    .await;
    let _ = jmap_call(
        &r,
        "CalendarEvent/set",
        json!({
            "accountId": "account-1",
            "create": {
                "pA": {
                    "calendarIds": {"cal-personal": true},
                    "title": "in personal",
                    "start": "2026-04-02T10:00:00",
                    "duration": "PT30M"
                }
            }
        }),
    )
    .await;

    // Now destroy the cal-work event. With the create+destroy in
    // the same delta window, dominance must cancel them entirely:
    // a sync client should never see the transient event.
    let work_id = {
        let resp = jmap_call(
            &r,
            "CalendarEvent/get",
            json!({"accountId": "account-1"}),
        )
        .await;
        let list = resp[1]["list"].as_array().unwrap();
        list.iter()
            .find(|e| e["title"] == "in work")
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let _ = jmap_call(
        &r,
        "CalendarEvent/set",
        json!({"accountId": "account-1", "destroy": [&work_id]}),
    )
    .await;

    // From seed: net delta is one created (cal-personal's pA),
    // because the cal-work event was created and destroyed in
    // the same window. Pre-fix the per-calendar union would have
    // reported the cal-work event as created (cal-personal's
    // walk had it in created over-broadly, cal-work's walk
    // cancelled it under dominance, but the seen-set dedupe
    // kept the cal-personal copy alive).
    let delta = jmap_call(
        &r,
        "CalendarEvent/changes",
        json!({"accountId": "account-1", "sinceState": "fixture-state"}),
    )
    .await;
    let body = &delta[1];
    let created = body["created"].as_array().unwrap();
    let destroyed = body["destroyed"].as_array().unwrap();
    assert!(
        !created.iter().any(|v| v.as_str() == Some(work_id.as_str())),
        "destroyed-in-same-window event must not surface as created; \
         got created={created:?}"
    );
    assert!(
        !destroyed.iter().any(|v| v.as_str() == Some(work_id.as_str())),
        "destroyed-in-same-window event must cancel under dominance, \
         not surface as destroyed either; got destroyed={destroyed:?}"
    );
    assert_eq!(
        created.len(),
        1,
        "only the surviving cal-personal create should remain; got {created:?}"
    );
}

#[tokio::test]
async fn jmap_event_set_surfaces_in_graph_calendar_view_delta() {
    // Two routers, one shared fixture handle. JMAP mutates, Graph
    // reads back through `calendarView/delta`.
    let (jmap, graph_r) = cross_protocol_routers();

    // First Graph delta call returns full bootstrap + a deltaLink we
    // capture as the since-token.
    let g_body = graph_get(
        &graph_r,
        "/v1.0/me/calendars/cal-work/calendarView/delta?startDateTime=2026-01-01T00:00:00Z&endDateTime=2027-01-01T00:00:00Z",
    )
    .await;
    let delta_link = g_body["@odata.deltaLink"].as_str().unwrap().to_string();

    // JMAP create on the same shared fixture.
    let _ = jmap_call(
        &jmap,
        "CalendarEvent/set",
        json!({
            "accountId": "account-1",
            "create": {
                "x": {
                    "calendarIds": {"cal-work": true},
                    "title": "Cross-protocol",
                    "start": "2026-05-01T15:00:00",
                    "duration": "PT45M"
                }
            }
        }),
    )
    .await;

    // Replay the deltaLink path on the Graph router.
    let path = delta_link
        .split_once("/v1.0/")
        .map(|(_, rest)| format!("/v1.0/{rest}"))
        .unwrap();
    let g2_body = graph_get(&graph_r, &path).await;
    let value = g2_body["value"].as_array().unwrap();
    let titles: Vec<&str> = value
        .iter()
        .filter_map(|v| v.get("subject").and_then(Value::as_str))
        .collect();
    assert!(
        titles.contains(&"Cross-protocol"),
        "expected JMAP-created event in Graph delta; got {value:?}"
    );
}
