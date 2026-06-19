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

use std::sync::Arc;

use saehrimnir::{fixture, lua, people, routes, shared, smtp};

fn router() -> axum::Router {
    let fix = fixture::load(Path::new("fixtures/graph-contacts-small.toml")).unwrap();
    people::router(people::AppState::for_test(shared::handle(fix)))
}

/// Two routers (people + routes) sharing one fixture handle. Used
/// to drive `/test/fixture/step` from the routes router and read
/// back via the People one. The dispatcher comes along for the
/// ride so change-script ops fire in the right order.
fn cross_protocol_routers(scenario_path: &str) -> (axum::Router, axum::Router) {
    let (fix, dispatcher) =
        lua::load_source_with_dispatcher(&std::fs::read_to_string(scenario_path).unwrap(), "@fxt")
            .unwrap();
    let mut shared_handles = shared::SharedHandles::for_test(shared::handle(fix));
    shared_handles.dispatcher = Some(Arc::new(dispatcher));
    let people_state = people::AppState {
        shared: shared_handles.clone(),
    };
    let routes_state = routes::AppState {
        shared: shared_handles,
        submission_log: smtp::SubmissionLog::default(),
        base_url: "http://localhost".into(),
    };
    (people::router(people_state), routes::router(routes_state))
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
async fn get_single_person_returns_projection_and_404s_unknown() {
    let r = router();

    // bifrost's get_person drives this for contact_get AND the
    // etag-prefetch before updateContact; without it both 404.
    let (status, v) = get(&r, "/v1/people/contact-001?personFields=names,emailAddresses").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["resourceName"], "people/contact-001");
    assert_eq!(v["etag"], "etag-contact-001");
    assert_eq!(v["names"][0]["displayName"], "Alice Anderson");
    assert_eq!(v["emailAddresses"][0]["value"], "alice@example.com");

    // Unknown resource name 404s.
    let (status, _) = get(&r, "/v1/people/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
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
async fn connections_emits_metadata_deleted_tombstone_after_destroy() {
    // Regression: pre-fix, a known-but-stale syncToken returned a
    // full live-contacts list with no tombstones, so a destroyed
    // contact silently persisted in the client's local DB. Now
    // the handler walks the change_log and emits
    // `metadata.deleted: true` Persons for `contact_destroyed`
    // ids; ratatoskr's PersonMetadata reader routes those to
    // delete-the-row.
    let (people_r, routes_r) =
        cross_protocol_routers("fixtures/graph-contacts-incremental.lua");

    // Bootstrap: token-less call returns the seed three contacts
    // (the fixture's two declared contacts; step 1 is `new` which
    // adds contact-003, but we haven't stepped yet).
    let (status, bootstrap) = get(
        &people_r,
        "/v1/people/me/connections?personFields=names&pageSize=100&requestSyncToken=true",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = bootstrap["nextSyncToken"].as_str().unwrap().to_string();
    assert_eq!(bootstrap["connections"].as_array().unwrap().len(), 2);

    // Drive the change-script to apply the `delete` step (step 3)
    // which calls contact_destroy on contact-001. Steps 1 + 2 fire
    // in order before that.
    for _ in 0..3 {
        let resp = routes_r
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/test/fixture/step")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // Follow up with the saved token. Expect:
    // - contact-001 emitted as a tombstone (metadata.deleted=true)
    // - contact-002 emitted as updated (display_name changed)
    // - contact-003 emitted as created (added in step 1)
    let (status, follow) = get(
        &people_r,
        &format!("/v1/people/me/connections?personFields=names&pageSize=100&syncToken={token}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let conns = follow["connections"].as_array().unwrap();

    let tombstone = conns
        .iter()
        .find(|p| p["resourceName"] == "people/contact-001")
        .unwrap_or_else(|| panic!("expected tombstone for contact-001; got {conns:?}"));
    assert_eq!(
        tombstone["metadata"]["deleted"], true,
        "tombstone must carry metadata.deleted=true"
    );

    let live_002 = conns
        .iter()
        .find(|p| p["resourceName"] == "people/contact-002")
        .unwrap();
    assert_eq!(live_002["metadata"]["deleted"], false);

    let live_003 = conns
        .iter()
        .find(|p| p["resourceName"] == "people/contact-003")
        .unwrap();
    assert_eq!(live_003["metadata"]["deleted"], false);

    // nextSyncToken advances to the post-step state.
    let new_token = follow["nextSyncToken"].as_str().unwrap();
    assert_ne!(new_token, token);
}

async fn request(
    r: &axum::Router,
    method: &str,
    uri: &str,
    body: Body,
    content_type: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(ct) = content_type {
        req = req.header("content-type", ct);
    }
    let resp = r.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let body = body_json(resp).await;
    (status, body)
}

#[tokio::test]
async fn update_contact_records_transition_and_returns_person() {
    let r = router();

    // Bootstrap to grab the seed sync token.
    let (_, bootstrap) = get(
        &r,
        "/v1/people/me/connections?personFields=names&pageSize=100&requestSyncToken=true",
    )
    .await;
    let token = bootstrap["nextSyncToken"].as_str().unwrap().to_string();

    // PATCH the first contact in the fixture (graph-contacts-small
    // declares contact-001 ... contact-004).
    let body = Body::from(
        serde_json::to_vec(&serde_json::json!({
            "etag": "*",
            "phoneNumbers": [{ "value": "+1-555-0100" }],
            "organizations": [{ "name": "Hammerworks" }],
        }))
        .unwrap(),
    );
    let (status, person) = request(
        &r,
        "PATCH",
        "/v1/people/contact-001:updateContact?updatePersonFields=phoneNumbers,organizations",
        body,
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(person["resourceName"], "people/contact-001");
    assert_eq!(person["metadata"]["deleted"], false);

    // Delta walk: contact-001 surfaces in the updated set.
    let (status, follow) = get(
        &r,
        &format!("/v1/people/me/connections?personFields=names&pageSize=100&syncToken={token}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let conns = follow["connections"].as_array().unwrap();
    let entry = conns
        .iter()
        .find(|p| p["resourceName"] == "people/contact-001")
        .unwrap_or_else(|| panic!("expected contact-001 in delta; got {conns:?}"));
    assert_eq!(entry["metadata"]["deleted"], false);
    assert_ne!(follow["nextSyncToken"], Value::String(token));
}

#[tokio::test]
async fn update_contact_unknown_id_returns_404() {
    let r = router();
    let body = Body::from(b"{\"etag\":\"*\"}".to_vec());
    let (status, v) = request(
        &r,
        "PATCH",
        "/v1/people/contact-999:updateContact?updatePersonFields=phoneNumbers",
        body,
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["errors"][0]["reason"], "notFound");
}

#[tokio::test]
async fn delete_contact_removes_and_emits_tombstone_in_delta() {
    let r = router();

    let (_, bootstrap) = get(
        &r,
        "/v1/people/me/connections?personFields=names&pageSize=100&requestSyncToken=true",
    )
    .await;
    let token = bootstrap["nextSyncToken"].as_str().unwrap().to_string();
    let initial = bootstrap["connections"].as_array().unwrap().len();

    let (status, _) = request(
        &r,
        "DELETE",
        "/v1/people/contact-002:deleteContact",
        Body::empty(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Bootstrap again (no token) to confirm contact-002 is gone.
    let (_, fresh) = get(
        &r,
        "/v1/people/me/connections?personFields=names&pageSize=100&requestSyncToken=true",
    )
    .await;
    assert_eq!(
        fresh["connections"].as_array().unwrap().len(),
        initial - 1,
        "contact-002 should be removed from the live list"
    );
    assert!(
        fresh["connections"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["resourceName"] != "people/contact-002")
    );

    // Delta walk from the pre-delete token surfaces the tombstone.
    let (_, follow) = get(
        &r,
        &format!("/v1/people/me/connections?personFields=names&pageSize=100&syncToken={token}"),
    )
    .await;
    let tombstone = follow["connections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["resourceName"] == "people/contact-002")
        .unwrap_or_else(|| panic!("expected tombstone for contact-002 in delta"));
    assert_eq!(tombstone["metadata"]["deleted"], true);
}

#[tokio::test]
async fn delete_contact_unknown_id_returns_404() {
    let r = router();
    let (status, _) = request(
        &r,
        "DELETE",
        "/v1/people/contact-999:deleteContact",
        Body::empty(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unknown_path_falls_back_to_404_envelope() {
    let r = router();
    let (status, v) = get(&r, "/v1/people/me/findings").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["code"], 404);
    assert_eq!(v["error"]["errors"][0]["reason"], "notFound");
}

// ── Multi-account (Stage 4: OAuth-scoped tokens) ────────────────────

fn multi_account_people_router(store: saehrimnir::oauth::TokenStore) -> axum::Router {
    let fix = fixture::load(Path::new("fixtures/multi-account-small.toml")).unwrap();
    let shared = shared::SharedHandles::for_test(shared::handle(fix)).with_token_store(store);
    people::router(people::AppState { shared })
}

async fn get_with_bearer(router: &axum::Router, uri: &str, token: &str) -> (StatusCode, Value) {
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let v = body_json(resp).await;
    (status, v)
}

#[tokio::test]
async fn people_connections_scope_by_bearer_token() {
    let store = saehrimnir::oauth::TokenStore::default();
    let primary_token = store.mint("authorization_code", "account-primary", 1);
    let secondary_token = store.mint("authorization_code", "account-secondary", 2);

    let r = multi_account_people_router(store.clone());
    let (_, v) = get_with_bearer(&r, "/v1/people/me/connections", &primary_token).await;
    let conns = v["connections"].as_array().unwrap();
    assert_eq!(conns.len(), 1);
    assert_eq!(conns[0]["resourceName"], "people/contact-primary-001");

    let r = multi_account_people_router(store);
    let (_, v) = get_with_bearer(&r, "/v1/people/me/connections", &secondary_token).await;
    let conns = v["connections"].as_array().unwrap();
    assert_eq!(conns.len(), 1);
    assert_eq!(conns[0]["resourceName"], "people/contact-secondary-001");
}

#[tokio::test]
async fn people_update_cross_account_returns_404() {
    // A token bound to secondary cannot update primary's contact.
    let store = saehrimnir::oauth::TokenStore::default();
    let secondary_token = store.mint("authorization_code", "account-secondary", 1);
    let r = multi_account_people_router(store);
    let resp = r
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/v1/people/contact-primary-001:updateContact?updatePersonFields=displayName")
                .header(
                    axum::http::header::AUTHORIZATION,
                    format!("Bearer {secondary_token}"),
                )
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
