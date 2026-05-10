#![allow(clippy::unwrap_used)]

//! Google People API integration tests.
//!
//! Drives `/v1/people/me/connections` and `/v1/otherContacts`
//! against `fixtures/graph-contacts-small.toml` (whose contacts
//! tables already cover multiple email shapes) via
//! `tower::ServiceExt::oneshot`.

use std::path::Path;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use saehrimnir::{fixture, people, shared};

fn router() -> axum::Router {
    let fix = fixture::load(Path::new("fixtures/graph-contacts-small.toml")).unwrap();
    people::router(people::AppState::for_test(shared::handle(fix)))
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn get(r: &axum::Router, uri: &str) -> (StatusCode, Value) {
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
    let status = resp.status();
    let body = body_json(resp).await;
    (status, body)
}

#[tokio::test]
async fn connections_initial_listing_returns_full_set_with_sync_token() {
    let r = router();
    let (status, v) = get(
        &r,
        "/v1/people/me/connections?personFields=names,emailAddresses&pageSize=100&requestSyncToken=true",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let conns = v["connections"].as_array().unwrap();
    assert_eq!(conns.len(), 4); // matches the four contacts in the fixture.
    assert!(v["nextSyncToken"].is_string());
    assert!(v["nextPageToken"].is_null());

    // Spot-check a contact projection.
    let first = &conns[0];
    assert!(first["resourceName"]
        .as_str()
        .unwrap()
        .starts_with("people/"));
    assert_eq!(first["metadata"]["deleted"], false);
    let emails = first["emailAddresses"].as_array().unwrap();
    assert!(!emails.is_empty());
    assert!(emails[0]["value"].is_string());
}

#[tokio::test]
async fn connections_paginates_with_next_page_token() {
    let r = router();
    let (status, page1) = get(
        &r,
        "/v1/people/me/connections?personFields=names&pageSize=2&requestSyncToken=true",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page1["connections"].as_array().unwrap().len(), 2);
    let next = page1["nextPageToken"].as_str().unwrap().to_string();
    // Mid-page response carries no nextSyncToken (the token is
    // emitted on the final page only).
    assert!(page1["nextSyncToken"].is_null());

    let (status, page2) = get(
        &r,
        &format!("/v1/people/me/connections?personFields=names&pageSize=2&pageToken={next}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page2["connections"].as_array().unwrap().len(), 2);
    assert!(page2["nextPageToken"].is_null());
    assert!(page2["nextSyncToken"].is_string());

    // Across the two pages we see every contact id exactly once.
    let mut all_ids: Vec<String> = page1["connections"]
        .as_array()
        .unwrap()
        .iter()
        .chain(page2["connections"].as_array().unwrap().iter())
        .map(|c| c["resourceName"].as_str().unwrap().to_string())
        .collect();
    all_ids.sort();
    all_ids.dedup();
    assert_eq!(all_ids.len(), 4);
}

#[tokio::test]
async fn connections_with_current_sync_token_returns_empty_delta() {
    let r = router();
    // First call to grab the current sync token.
    let (_, v) = get(
        &r,
        "/v1/people/me/connections?personFields=names&pageSize=100&requestSyncToken=true",
    )
    .await;
    let token = v["nextSyncToken"].as_str().unwrap().to_string();

    // Second call with the same token: empty delta.
    let (status, follow) = get(
        &r,
        &format!("/v1/people/me/connections?personFields=names&pageSize=100&syncToken={token}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(follow["connections"].as_array().unwrap().len(), 0);
    assert_eq!(follow["nextSyncToken"], token);
}

#[tokio::test]
async fn connections_with_unknown_sync_token_returns_410_gone() {
    let r = router();
    let (status, v) = get(
        &r,
        "/v1/people/me/connections?personFields=names&syncToken=ancient-and-gone&pageSize=100",
    )
    .await;
    assert_eq!(status, StatusCode::GONE);
    let err = &v["error"];
    assert_eq!(err["code"], 410);
    assert!(err["message"].as_str().unwrap().contains("syncToken"));
    assert_eq!(err["errors"][0]["reason"], "expired");
}

#[tokio::test]
async fn other_contacts_returns_empty_list_with_sync_token() {
    let r = router();
    let (status, v) = get(
        &r,
        "/v1/otherContacts?readMask=names,emailAddresses&pageSize=100&requestSyncToken=true",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["otherContacts"].as_array().unwrap().len(), 0);
    assert_eq!(v["totalSize"], 0);
    assert!(v["nextSyncToken"].is_string());
}

#[tokio::test]
async fn other_contacts_unknown_sync_token_returns_410() {
    let r = router();
    let (status, _) = get(
        &r,
        "/v1/otherContacts?readMask=names&syncToken=evicted&pageSize=100",
    )
    .await;
    assert_eq!(status, StatusCode::GONE);
}

#[tokio::test]
async fn unknown_path_falls_back_to_404_envelope() {
    let r = router();
    let (status, v) = get(&r, "/v1/people/me/findings").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["code"], 404);
    assert_eq!(v["error"]["errors"][0]["reason"], "notFound");
}
