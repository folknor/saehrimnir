#![allow(clippy::unwrap_used)]

//! End-to-end CalDAV tests against the canonical fixture, driven via
//! `tower::ServiceExt::oneshot` (no socket bind). Exercises the
//! discovery walk + event listing + REPORT + PUT/DELETE round-trips.

use axum::{
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use http_body_util::BodyExt;
use tower::ServiceExt;

use saehrimnir::{caldav, fixture};

fn router() -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/graph-calendar-small.toml")).unwrap();
    caldav::router(caldav::AppState::for_test(saehrimnir::shared::handle(fix)))
}

async fn send(method: &str, uri: &str, depth: Option<&str>, body: &str) -> (StatusCode, String) {
    send_with(router(), method, uri, depth, body).await
}

async fn send_with(
    app: axum::Router,
    method: &str,
    uri: &str,
    depth: Option<&str>,
    body: &str,
) -> (StatusCode, String) {
    let mut req = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(uri)
        .header(header::HOST, "127.0.0.1:0")
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8");
    if let Some(d) = depth {
        req = req.header("Depth", d);
    }
    let req = req.body(Body::from(body.to_string())).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

const PROPFIND_PRINCIPAL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:current-user-principal/>
  </D:prop>
</D:propfind>"#;

const PROPFIND_CALENDAR_HOME: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <C:calendar-home-set/>
  </D:prop>
</D:propfind>"#;

const PROPFIND_CALENDARS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"
            xmlns:CS="http://calendarserver.org/ns/"
            xmlns:IC="http://apple.com/ns/ical/">
  <D:prop>
    <D:resourcetype/>
    <D:displayname/>
    <CS:getctag/>
    <IC:calendar-color/>
    <D:current-user-privilege-set/>
  </D:prop>
</D:propfind>"#;

#[tokio::test]
async fn options_advertises_dav_class_and_caldav_verbs() {
    let resp = router()
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let dav = resp.headers().get("DAV").unwrap().to_str().unwrap();
    assert!(dav.contains("calendar-access"), "DAV header: {dav}");
    let allow = resp.headers().get("Allow").unwrap().to_str().unwrap();
    for verb in ["PROPFIND", "REPORT", "GET", "PUT", "DELETE"] {
        assert!(allow.contains(verb), "Allow missing {verb}: {allow}");
    }
}

#[tokio::test]
async fn propfind_root_returns_current_user_principal() {
    let (status, body) = send("PROPFIND", "/", Some("0"), PROPFIND_PRINCIPAL).await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert!(body.contains("<D:multistatus"));
    assert!(body.contains("/principals/account-1/"));
    assert!(body.contains("current-user-principal"));
}

#[tokio::test]
async fn propfind_well_known_caldav_works_as_root_alias() {
    let (status, body) = send(
        "PROPFIND",
        "/.well-known/caldav",
        Some("0"),
        PROPFIND_PRINCIPAL,
    )
    .await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert!(body.contains("/principals/account-1/"));
}

#[tokio::test]
async fn propfind_principal_returns_calendar_home_set() {
    let (status, body) = send(
        "PROPFIND",
        "/principals/account-1/",
        Some("0"),
        PROPFIND_CALENDAR_HOME,
    )
    .await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert!(body.contains("calendar-home-set"));
    assert!(body.contains("/calendars/account-1/"));
}

#[tokio::test]
async fn propfind_calendar_home_depth_1_lists_calendars() {
    let (status, body) = send(
        "PROPFIND",
        "/calendars/account-1/",
        Some("1"),
        PROPFIND_CALENDARS,
    )
    .await;
    assert_eq!(status, StatusCode::MULTI_STATUS);

    // Both calendars in the fixture appear with their hrefs and
    // displaynames.
    assert!(body.contains("/calendars/account-1/cal-work/"));
    assert!(body.contains("/calendars/account-1/cal-personal/"));
    assert!(body.contains("<D:displayname>Work</D:displayname>"));
    assert!(body.contains("<D:displayname>Personal</D:displayname>"));

    // Resourcetype carries both `collection` and `calendar` for the
    // calendar collections.
    let occurrences = body.matches("<D:collection/><C:calendar/>").count();
    assert_eq!(
        occurrences, 2,
        "expected 2 calendar resourcetype emissions, got {occurrences}: {body}"
    );

    // Privilege-set advertises read + write so ratatoskr's
    // `can_edit.unwrap_or(true)` resolves to true.
    assert!(body.contains("<D:write/>"));

    // Apple calendar-color comes through for the calendars that
    // declared one in the fixture.
    assert!(body.contains("<IC:calendar-color>lightBlue</IC:calendar-color>"));
}

#[tokio::test]
async fn propfind_calendar_home_depth_0_returns_only_home_collection() {
    let (status, body) = send(
        "PROPFIND",
        "/calendars/account-1/",
        Some("0"),
        PROPFIND_CALENDARS,
    )
    .await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    // The home collection itself, but no per-calendar entries.
    assert!(!body.contains("/calendars/account-1/cal-work/"));
    assert!(!body.contains("/calendars/account-1/cal-personal/"));
}

#[tokio::test]
async fn propfind_calendar_depth_0_returns_ctag() {
    let body_xml = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:" xmlns:CS="http://calendarserver.org/ns/">
  <D:prop>
    <CS:getctag/>
    <D:displayname/>
  </D:prop>
</D:propfind>"#;
    let (status, body) = send(
        "PROPFIND",
        "/calendars/account-1/cal-work/",
        Some("0"),
        body_xml,
    )
    .await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert!(body.contains("<CS:getctag>"));
    assert!(body.contains("<D:displayname>Work</D:displayname>"));
}

#[tokio::test]
async fn propfind_unknown_calendar_returns_404() {
    let (status, _) = send(
        "PROPFIND",
        "/calendars/account-1/cal-bogus/",
        Some("0"),
        PROPFIND_CALENDARS,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
