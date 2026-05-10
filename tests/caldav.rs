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

fn router_with_enforce(store: saehrimnir::oauth::TokenStore) -> axum::Router {
    use saehrimnir::fixture::OAuthConfig;
    let mut fix =
        fixture::load(std::path::Path::new("fixtures/graph-calendar-small.toml")).unwrap();
    fix.oauth = OAuthConfig {
        enforce: true,
        issuer: "https://saehrimnir.test/oauth".to_string(),
    };
    let handle = saehrimnir::shared::handle(fix);
    let shared = saehrimnir::shared::SharedHandles::for_test(handle).with_token_store(store);
    caldav::router(caldav::AppState { shared })
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
async fn propfind_calendar_depth_1_lists_events_with_etag_and_content_type() {
    let body_xml = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:getetag/>
    <D:getcontenttype/>
  </D:prop>
</D:propfind>"#;
    let (status, body) = send(
        "PROPFIND",
        "/calendars/account-1/cal-work/",
        Some("1"),
        body_xml,
    )
    .await;
    assert_eq!(status, StatusCode::MULTI_STATUS);

    // Both events show up by href.
    assert!(body.contains("/calendars/account-1/cal-work/ev-001.ics"));
    assert!(body.contains("/calendars/account-1/cal-work/ev-002.ics"));
    // Each event resource has getetag and getcontenttype.
    let etag_count = body.matches("<D:getetag>").count();
    assert_eq!(etag_count, 2, "expected 2 getetag emissions, body: {body}");
    assert!(body.contains("text/calendar; component=vevent"));
    // Empty cal-personal returns just the calendar entry (no events).
    let (_, personal_body) = send(
        "PROPFIND",
        "/calendars/account-1/cal-personal/",
        Some("1"),
        body_xml,
    )
    .await;
    assert!(!personal_body.contains(".ics"));
}

#[tokio::test]
async fn get_event_returns_icalendar_with_etag() {
    let resp = router()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/calendars/account-1/cal-work/ev-001.ics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "text/calendar; charset=utf-8"
    );
    let etag = resp.headers().get("ETag").unwrap().to_str().unwrap();
    // ETag is a quoted opaque string per RFC 7232.
    assert!(etag.starts_with('"') && etag.ends_with('"'), "got: {etag}");
    let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8(body_bytes.to_vec()).unwrap();
    // Minimum VEVENT shape: BEGIN/END markers, UID, SUMMARY, DTSTART,
    // DTEND, ORGANIZER. The fixture's ev-001 carries Standup.
    assert!(body.contains("BEGIN:VCALENDAR"));
    assert!(body.contains("BEGIN:VEVENT"));
    assert!(body.contains("UID:ev-001"));
    assert!(body.contains("SUMMARY:Standup"));
    assert!(body.contains("DTSTART:20260115T090000Z"));
    assert!(body.contains("DTEND:20260115T091500Z"));
    assert!(body.contains("ORGANIZER"));
    assert!(body.contains("mailto:alice@example.com"));
}

#[tokio::test]
async fn get_unknown_event_returns_404() {
    let resp = router()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/calendars/account-1/cal-work/ev-bogus.ics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn report_calendar_multiget_returns_ical_for_each_href() {
    let body = r#"<?xml version="1.0" encoding="utf-8"?>
<C:calendar-multiget xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:getetag/>
    <C:calendar-data/>
  </D:prop>
  <D:href>/calendars/account-1/cal-work/ev-001.ics</D:href>
  <D:href>/calendars/account-1/cal-work/ev-002.ics</D:href>
  <D:href>/calendars/account-1/cal-work/ev-bogus.ics</D:href>
</C:calendar-multiget>"#;
    let (status, response) = send(
        "REPORT",
        "/calendars/account-1/cal-work/",
        Some("1"),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    // Each known event surfaces with a getetag and a calendar-data
    // payload.
    assert!(response.contains("ev-001.ics"));
    assert!(response.contains("ev-002.ics"));
    let etag_count = response.matches("<D:getetag>").count();
    assert_eq!(etag_count, 2, "expected 2 etags, got {etag_count}");
    let calendar_data_count = response.matches("<C:calendar-data>").count();
    assert_eq!(calendar_data_count, 2);
    // The bogus href gets a 404 entry so the client can prune.
    assert!(response.contains("ev-bogus.ics"));
    assert!(response.contains("HTTP/1.1 404 Not Found"));
    // The iCalendar bodies survived XML escaping (BEGIN/END markers
    // become &lt;-encoded inside calendar-data; ratatoskr's parser
    // unescapes them on the way back out).
    assert!(response.contains("BEGIN:VEVENT"));
}

#[tokio::test]
async fn report_calendar_query_filters_by_time_range() {
    // ev-001 is 2026-01-15, ev-002 is 2026-02-01. Query a window
    // that covers only January.
    let body = r#"<?xml version="1.0" encoding="utf-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:getetag/>
    <C:calendar-data/>
  </D:prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR">
      <C:comp-filter name="VEVENT">
        <C:time-range start="20260101T000000Z" end="20260131T235959Z"/>
      </C:comp-filter>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>"#;
    let (status, response) = send(
        "REPORT",
        "/calendars/account-1/cal-work/",
        Some("1"),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert!(response.contains("ev-001.ics"));
    assert!(!response.contains("ev-002.ics"), "out-of-range event leaked: {response}");
}

#[tokio::test]
async fn report_calendar_query_with_no_range_returns_all_events() {
    let body = r#"<?xml version="1.0" encoding="utf-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:getetag/>
    <C:calendar-data/>
  </D:prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR">
      <C:comp-filter name="VEVENT"/>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>"#;
    let (status, response) = send(
        "REPORT",
        "/calendars/account-1/cal-work/",
        Some("1"),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert!(response.contains("ev-001.ics"));
    assert!(response.contains("ev-002.ics"));
}

#[tokio::test]
async fn put_creates_new_event_and_returns_etag() {
    let app = router();
    let body = "BEGIN:VCALENDAR\r\n\
                VERSION:2.0\r\n\
                BEGIN:VEVENT\r\n\
                UID:ev-new\r\n\
                SUMMARY:Sprint planning\r\n\
                DTSTART:20260301T100000Z\r\n\
                DTEND:20260301T110000Z\r\n\
                LOCATION:Online\r\n\
                END:VEVENT\r\n\
                END:VCALENDAR\r\n";
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/calendars/account-1/cal-work/ev-new.ics")
                .header(header::CONTENT_TYPE, "text/calendar; charset=utf-8")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let etag = resp.headers().get("ETag").unwrap().to_str().unwrap();
    assert!(etag.starts_with('"'));

    // GET round-trip sees the new event.
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/calendars/account-1/cal-work/ev-new.ics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8(body_bytes.to_vec()).unwrap();
    assert!(body.contains("UID:ev-new"));
    assert!(body.contains("SUMMARY:Sprint planning"));
}

#[tokio::test]
async fn put_updates_existing_event_and_etag_changes() {
    let app = router();
    // Get the baseline ETag for ev-001.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/calendars/account-1/cal-work/ev-001.ics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let baseline_etag = resp.headers().get("ETag").unwrap().to_str().unwrap().to_string();

    // Update via PUT.
    let body = "BEGIN:VCALENDAR\r\n\
                VERSION:2.0\r\n\
                BEGIN:VEVENT\r\n\
                UID:ev-001\r\n\
                SUMMARY:Standup (rescheduled)\r\n\
                DTSTART:20260115T100000Z\r\n\
                DTEND:20260115T101500Z\r\n\
                END:VEVENT\r\n\
                END:VCALENDAR\r\n";
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/calendars/account-1/cal-work/ev-001.ics")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let new_etag = resp.headers().get("ETag").unwrap().to_str().unwrap();
    assert_ne!(new_etag, baseline_etag, "ETag must cycle on update");

    // Subsequent GET reflects the rescheduled time.
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/calendars/account-1/cal-work/ev-001.ics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = String::from_utf8(
        resp.into_body().collect().await.unwrap().to_bytes().to_vec(),
    )
    .unwrap();
    assert!(body.contains("DTSTART:20260115T100000Z"));
    assert!(body.contains("Standup (rescheduled)"));
}

#[tokio::test]
async fn put_with_if_match_mismatch_returns_412() {
    let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:ev-001\r\nSUMMARY:X\r\nDTSTART:20260115T090000Z\r\nDTEND:20260115T091500Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let resp = router()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/calendars/account-1/cal-work/ev-001.ics")
                .header("If-Match", "\"stale-etag\"")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn put_with_if_match_star_on_missing_event_returns_412() {
    let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:ev-new2\r\nSUMMARY:X\r\nDTSTART:20260115T090000Z\r\nDTEND:20260115T091500Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let resp = router()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/calendars/account-1/cal-work/ev-new2.ics")
                .header("If-Match", "*")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn delete_removes_event() {
    let app = router();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/calendars/account-1/cal-work/ev-002.ics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Subsequent GET 404s.
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/calendars/account-1/cal-work/ev-002.ics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn put_with_body_uid_mismatch_returns_400() {
    // Regression: PUT body's UID must match the URL's event id, or
    // the resource ↔ id mapping breaks. URL says ev-mismatch but
    // body's UID:ev-other - 400, and the URL's resource is not
    // touched.
    let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:ev-other\r\nSUMMARY:X\r\nDTSTART:20260115T090000Z\r\nDTEND:20260115T091500Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let resp = router()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/calendars/account-1/cal-work/ev-mismatch.ics")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn put_with_multi_vevent_body_returns_400() {
    // Regression: a body with two VEVENTs is rejected. Pre-fix the
    // first VEVENT was parsed and the second silently dropped,
    // letting an attacker supply two with conflicting UIDs.
    let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
                BEGIN:VEVENT\r\nUID:ev-001\r\nSUMMARY:Real\r\nDTSTART:20260115T090000Z\r\nDTEND:20260115T091500Z\r\nEND:VEVENT\r\n\
                BEGIN:VEVENT\r\nUID:ev-shadow\r\nSUMMARY:Smuggled\r\nDTSTART:20260115T100000Z\r\nDTEND:20260115T110000Z\r\nEND:VEVENT\r\n\
                END:VCALENDAR\r\n";
    let resp = router()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/calendars/account-1/cal-work/ev-001.ics")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn put_if_match_accepts_quoted_wildcard_and_weak_validator() {
    // Regression: pre-fix the wildcard branch only matched the
    // literal three bytes `*`. Quoted `"*"`, weak `W/*`, and
    // weak-quoted `W/"current-etag"` must all parse as RFC 7232
    // intends.
    let app = router();

    // Get baseline ETag for ev-001.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/calendars/account-1/cal-work/ev-001.ics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let etag = resp
        .headers()
        .get("ETag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // PUT with quoted wildcard. ev-001 exists, so this matches.
    let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:ev-001\r\nSUMMARY:V1\r\nDTSTART:20260115T090000Z\r\nDTEND:20260115T091500Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/calendars/account-1/cal-work/ev-001.ics")
                .header("If-Match", "\"*\"")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NO_CONTENT,
        "quoted wildcard should match an existing resource (PUT-update returns 204)"
    );

    // PUT with W/<current-etag> (weak validator wrapping the current
    // etag). After the previous PUT the etag changed, so refetch.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/calendars/account-1/cal-work/ev-001.ics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let etag2 = resp
        .headers()
        .get("ETag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_ne!(etag2, etag);

    let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:ev-001\r\nSUMMARY:V2\r\nDTSTART:20260115T090000Z\r\nDTEND:20260115T091500Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/calendars/account-1/cal-work/ev-001.ics")
                .header("If-Match", format!("W/{etag2}"))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NO_CONTENT,
        "W/<etag> should match when the unweak comparison equals current"
    );
}

#[tokio::test]
async fn delete_with_if_match_mismatch_returns_412() {
    let resp = router()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/calendars/account-1/cal-work/ev-001.ics")
                .header("If-Match", "\"wrong\"")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn caldav_put_visible_through_graph_calendar_view_delta() {
    // Both surfaces share the same fixture handle, so a CalDAV PUT
    // must surface in the next Graph calendarView/delta walk.
    use saehrimnir::graph;
    let fix = fixture::load(std::path::Path::new("fixtures/graph-calendar-small.toml")).unwrap();
    let handle = saehrimnir::shared::handle(fix);
    let caldav_app = caldav::router(caldav::AppState {
        shared: saehrimnir::shared::SharedHandles::for_test(std::sync::Arc::clone(&handle)),
    });
    let graph_state = graph::AppState {
        shared: saehrimnir::shared::SharedHandles::for_test(std::sync::Arc::clone(&handle)),
    };
    let graph_app = graph::router(graph_state);

    // Bootstrap delta token.
    let resp = graph_app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1.0/me/calendars/cal-work/calendarView/delta")
                .header(header::HOST, "127.0.0.1:0")
                .header(header::AUTHORIZATION, "Bearer x")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let delta_link = v["@odata.deltaLink"].as_str().unwrap().to_string();
    let delta_path = delta_link
        .split_once("/v1.0/")
        .map(|(_, p)| format!("/v1.0/{p}"))
        .unwrap();

    // CalDAV PUT a new event.
    let put_body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:ev-from-caldav\r\nSUMMARY:Cross-protocol\r\nDTSTART:20260601T120000Z\r\nDTEND:20260601T130000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let resp = caldav_app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/calendars/account-1/cal-work/ev-from-caldav.ics")
                .body(Body::from(put_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Graph delta walk now sees the new event.
    let resp = graph_app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&delta_path)
                .header(header::HOST, "127.0.0.1:0")
                .header(header::AUTHORIZATION, "Bearer x")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let value = v["value"].as_array().unwrap();
    assert!(
        value.iter().any(|e| e["id"] == "ev-from-caldav"),
        "Graph delta should observe the CalDAV PUT: {value:?}"
    );
}

#[tokio::test]
async fn propfind_path_with_duplicate_slashes_returns_404() {
    // `/calendars//account-1/cal-work/` previously collapsed to
    // `/calendars/account-1/cal-work/` because the segment filter
    // dropped empty segments. That left request_log entries
    // unable to distinguish the two paths and was a defence-in-
    // depth concern. Reject upfront.
    let (status, _) = send(
        "PROPFIND",
        "/calendars//account-1/cal-work/",
        Some("0"),
        PROPFIND_CALENDARS,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn propfind_path_percent_decodes_segments() {
    // Real clients (Apple Calendar) round-trip ids through URL
    // percent-encoding. Without per-segment decoding, a calendar
    // id like `cal-work` requested as `cal%2Dwork` wouldn't match.
    let (status, body) = send(
        "PROPFIND",
        "/calendars/account-1/cal%2Dwork/",
        Some("0"),
        PROPFIND_CALENDARS,
    )
    .await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert!(
        body.contains("displayname"),
        "percent-decoded path didn't reach the calendar handler: {body:?}"
    );
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

/// Regression: ETags must be per-resource, not derived from the
/// fixture-wide state token. A mutation in calendar A used to bump
/// every event's ETag in calendar B and B's CTag too, forcing
/// real-client re-walks of every calendar after every unrelated
/// mutation. Walks the change_log to find the last touch of each
/// specific resource instead.
#[tokio::test]
async fn caldav_etag_and_ctag_are_per_resource_not_fixture_wide() {
    let app = router();

    // Capture baselines: ev-001 ETag (lives in cal-work) and the
    // PROPFIND CTag for cal-personal (which has no events).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/calendars/account-1/cal-work/ev-001.ics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ev001_etag_before = resp
        .headers()
        .get("ETag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    let (_, body_before) = send_with(
        app.clone(),
        "PROPFIND",
        "/calendars/account-1/cal-personal/",
        Some("0"),
        PROPFIND_CALENDARS,
    )
    .await;
    let cal_personal_ctag_before = extract_ctag(&body_before);

    // Mutate inside cal-work: PUT a brand-new event there. Pre-fix
    // this would have bumped ev-001's ETag (sibling event, same
    // calendar AND fixture state) and cal-personal's CTag (sibling
    // calendar, same fixture state). Post-fix: ev-001 unchanged
    // (its specific id wasn't touched), cal-personal CTag unchanged
    // (no event in that calendar was touched).
    let body = "BEGIN:VCALENDAR\r\n\
                VERSION:2.0\r\n\
                BEGIN:VEVENT\r\n\
                UID:ev-fresh\r\n\
                SUMMARY:Unrelated\r\n\
                DTSTART:20260301T100000Z\r\n\
                DTEND:20260301T110000Z\r\n\
                END:VEVENT\r\n\
                END:VCALENDAR\r\n";
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/calendars/account-1/cal-work/ev-fresh.ics")
                .header(header::CONTENT_TYPE, "text/calendar; charset=utf-8")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // ev-001 ETag must be unchanged.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/calendars/account-1/cal-work/ev-001.ics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let ev001_etag_after = resp.headers().get("ETag").unwrap().to_str().unwrap();
    assert_eq!(
        ev001_etag_after, ev001_etag_before,
        "ev-001 ETag bumped by an unrelated PUT to ev-fresh"
    );

    // cal-personal CTag must be unchanged.
    let (_, body_after) = send_with(
        app.clone(),
        "PROPFIND",
        "/calendars/account-1/cal-personal/",
        Some("0"),
        PROPFIND_CALENDARS,
    )
    .await;
    let cal_personal_ctag_after = extract_ctag(&body_after);
    assert_eq!(
        cal_personal_ctag_after, cal_personal_ctag_before,
        "cal-personal CTag bumped by an unrelated PUT in cal-work"
    );

    // Sanity: cal-work's CTag DID advance (the calendar this PUT
    // landed in must report a fresh CTag so subscribed clients re-
    // walk it).
    let (_, body_work) = send_with(
        app,
        "PROPFIND",
        "/calendars/account-1/cal-work/",
        Some("0"),
        PROPFIND_CALENDARS,
    )
    .await;
    let cal_work_ctag = extract_ctag(&body_work);
    assert_ne!(
        cal_work_ctag, cal_personal_ctag_before,
        "cal-work CTag should differ from cal-personal's pristine CTag after a PUT"
    );
}

fn extract_ctag(body: &str) -> String {
    let open = body
        .find("<CS:getctag>")
        .or_else(|| body.find("<getctag>"))
        .expect("CTag in body");
    let after = &body[open..];
    let val_start = after.find('>').unwrap() + 1;
    let val_end = after[val_start..].find('<').unwrap();
    after[val_start..val_start + val_end].to_string()
}

/// Bearer enforcement parity with the JMAP / Graph / Gmail
/// listeners: when `fixture.oauth.enforce` is true, every CalDAV
/// verb must reject requests without a valid bearer. Pre-fix CalDAV
/// silently bypassed enforcement, leaving PUT / DELETE reachable
/// with no token.
#[tokio::test]
async fn caldav_enforces_bearer_when_oauth_enforce_is_true() {
    let store = saehrimnir::oauth::TokenStore::default();
    let app = router_with_enforce(store.clone());

    // No header: every CalDAV verb returns 401 + WWW-Authenticate.
    for method in ["OPTIONS", "PROPFIND", "GET", "PUT", "DELETE", "REPORT"] {
        let req = Request::builder()
            .method(Method::from_bytes(method.as_bytes()).unwrap())
            .uri("/calendars/account-1/cal-work/")
            .header(header::HOST, "127.0.0.1:0")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{method}: expected 401"
        );
        assert_eq!(
            resp.headers()
                .get("WWW-Authenticate")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer"),
            "{method}: missing WWW-Authenticate"
        );
    }

    // With a valid token, the same OPTIONS request succeeds.
    let token = store.mint("authorization_code", 1);
    let req = Request::builder()
        .method("OPTIONS")
        .uri("/calendars/account-1/cal-work/")
        .header(header::HOST, "127.0.0.1:0")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
