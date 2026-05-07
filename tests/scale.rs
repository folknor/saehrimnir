#![allow(clippy::unwrap_used)]

//! Scale-correctness tests against `fixtures/jmap-bulk.lua` (10k
//! emails in a single inbox plus a hand-authored marker email in
//! Archive).
//!
//! These don't measure performance - we have the smoke script and
//! release profile for that. The point is to surface pagination
//! off-by-ones, pagination-token mishandling, and any "we
//! materialise everything into a Vec then OOM" bugs that don't
//! show up at the 2-email scale of `jmap-small`.

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use saehrimnir::{fixture, gmail, graph, routes};

/// Total emails in fixtures/jmap-bulk.lua = 10000 bulk + 1 marker.
const BULK_TOTAL: u64 = 10_000;

fn bulk_fixture() -> Arc<fixture::Fixture> {
    let f = fixture::load(std::path::Path::new("fixtures/jmap-bulk.lua")).unwrap();
    Arc::new(f)
}

fn jmap_router() -> axum::Router {
    routes::router(routes::AppState {
        fixture: bulk_fixture(),
        dispatcher: None,
    })
}

fn graph_router() -> axum::Router {
    graph::router(graph::AppState {
        fixture: bulk_fixture(),
        dispatcher: None,
    })
}

fn gmail_router() -> axum::Router {
    gmail::router(gmail::AppState {
        fixture: bulk_fixture(),
        dispatcher: None,
    })
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn jmap_email_query_paginates_through_full_inbox() {
    let router = jmap_router();
    let mut total_ids = Vec::with_capacity(usize::try_from(BULK_TOTAL).expect("BULK_TOTAL fits"));
    let mut position = 0u64;
    let limit = 50u64;
    let mut total_reported: Option<u64> = None;
    let mut iterations = 0u64;
    loop {
        iterations += 1;
        // Sanity bound: with limit=50 over BULK_TOTAL=10000 we expect
        // 200 pages. Allow a few more for a per-mailbox-only filter
        // miss.
        assert!(iterations < 250, "pagination didn't terminate");

        let req = json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "methodCalls": [[
                "Email/query",
                {
                    "accountId": "account-1",
                    "filter": {"inMailbox": "mbx-inbox"},
                    "position": position,
                    "limit": limit,
                    "calculateTotal": position == 0,
                },
                "q",
            ]],
        });
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/jmap/api")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&req).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let result = &v["methodResponses"][0][1];
        if total_reported.is_none() {
            total_reported = Some(result["total"].as_u64().unwrap());
        }
        let ids = result["ids"].as_array().unwrap();
        if ids.is_empty() {
            break;
        }
        for id in ids {
            total_ids.push(id.as_str().unwrap().to_string());
        }
        if (ids.len() as u64) < limit {
            break;
        }
        position += ids.len() as u64;
    }

    assert_eq!(total_reported, Some(BULK_TOTAL));
    assert_eq!(total_ids.len() as u64, BULK_TOTAL);
    // All ids are unique - pagination didn't duplicate.
    let unique: std::collections::HashSet<_> = total_ids.iter().collect();
    assert_eq!(unique.len() as u64, BULK_TOTAL);
}

#[tokio::test]
async fn graph_messages_pagination_through_full_inbox() {
    let router = graph_router();
    let mut next_uri = "/v1.0/me/mailFolders/inbox/messages?$top=50".to_string();
    let mut total_ids = Vec::new();
    let mut iterations = 0u64;
    loop {
        iterations += 1;
        assert!(iterations < 250, "Graph pagination didn't terminate");

        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(next_uri.as_str())
                    .header(header::HOST, "127.0.0.1:0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let value = v["value"].as_array().unwrap();
        for m in value {
            total_ids.push(m["id"].as_str().unwrap().to_string());
        }
        match v.get("@odata.nextLink").and_then(Value::as_str) {
            Some(link) => {
                // Strip the host - the router doesn't care.
                next_uri = link.trim_start_matches("http://127.0.0.1:0").to_string();
            }
            None => break,
        }
    }
    assert_eq!(total_ids.len() as u64, BULK_TOTAL);
    let unique: std::collections::HashSet<_> = total_ids.iter().collect();
    assert_eq!(unique.len() as u64, BULK_TOTAL);
}

#[tokio::test]
async fn graph_delta_initial_dump_paginates() {
    let router = graph_router();
    let mut next_uri = "/v1.0/me/mailFolders/inbox/messages/delta?$top=50".to_string();
    let mut total_ids = Vec::new();
    let mut iterations = 0u64;
    loop {
        iterations += 1;
        assert!(iterations < 250, "delta pagination didn't terminate");

        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(next_uri.as_str())
                    .header(header::HOST, "127.0.0.1:0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let v = body_json(resp).await;
        for m in v["value"].as_array().unwrap() {
            total_ids.push(m["id"].as_str().unwrap().to_string());
        }
        if let Some(link) = v.get("@odata.nextLink").and_then(Value::as_str) {
            next_uri = link.trim_start_matches("http://127.0.0.1:0").to_string();
        } else {
            // Last page must carry the deltaLink, never a nextLink.
            assert!(
                v.get("@odata.deltaLink").is_some(),
                "missing deltaLink on final page"
            );
            break;
        }
    }
    assert_eq!(total_ids.len() as u64, BULK_TOTAL);
}

#[tokio::test]
async fn gmail_threads_pagination_through_full_inbox() {
    let router = gmail_router();
    let mut next_uri = "/gmail/v1/users/me/threads?maxResults=100".to_string();
    let mut total_ids = Vec::new();
    let mut iterations = 0u64;
    loop {
        iterations += 1;
        assert!(iterations < 250, "Gmail pagination didn't terminate");

        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(next_uri.as_str())
                    .header(header::AUTHORIZATION, "Bearer x")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let v = body_json(resp).await;
        let threads = v["threads"].as_array().unwrap();
        for t in threads {
            total_ids.push(t["id"].as_str().unwrap().to_string());
        }
        match v.get("nextPageToken").and_then(Value::as_str) {
            Some(token) => {
                next_uri = format!(
                    "/gmail/v1/users/me/threads?maxResults=100&pageToken={token}"
                );
            }
            None => break,
        }
    }
    // bulk fixture: 10k bulk emails (singleton threads) + 1 marker
    // email in a different mailbox. All become distinct threads.
    assert_eq!(total_ids.len() as u64, BULK_TOTAL + 1);
    let unique: std::collections::HashSet<_> = total_ids.iter().collect();
    assert_eq!(unique.len() as u64, BULK_TOTAL + 1);
}
