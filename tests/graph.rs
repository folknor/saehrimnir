//! End-to-end Microsoft Graph mail-sync tests against the canonical
//! fixture, driven via `tower::ServiceExt::oneshot` (no socket bind).

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use saehrimnir::{fixture, graph};

fn router() -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/jmap-small.toml")).unwrap();
    graph::router(graph::AppState {
        fixture: Arc::new(fix),
    })
}

async fn get_json(uri: &str) -> (StatusCode, Value) {
    let resp = router()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header(header::HOST, "127.0.0.1:9999")
                .header(header::AUTHORIZATION, "Bearer doesnt-matter")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    (status, v)
}

#[tokio::test]
async fn list_top_level_folders() {
    let (status, v) = get_json("/v1.0/me/mailFolders").await;
    assert_eq!(status, StatusCode::OK);
    let value = v["value"].as_array().unwrap();
    // The fixture declares two top-level folders (Inbox, Archive).
    assert_eq!(value.len(), 2);
    let inbox = &value[0];
    assert_eq!(inbox["id"], "mbx-inbox");
    assert_eq!(inbox["displayName"], "Inbox");
    assert_eq!(inbox["wellKnownName"], "inbox");
    assert_eq!(inbox["totalItemCount"], 2);
    assert_eq!(inbox["unreadItemCount"], 2);
    let archive = &value[1];
    assert_eq!(archive["wellKnownName"], "archive");
    assert!(v.get("@odata.context").is_some());
}

#[tokio::test]
async fn folder_resolves_by_well_known_alias() {
    let (status, v) = get_json("/v1.0/me/mailFolders/inbox").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["id"], "mbx-inbox");
    assert_eq!(v["wellKnownName"], "inbox");
}

#[tokio::test]
async fn folder_resolves_by_opaque_id() {
    let (status, v) = get_json("/v1.0/me/mailFolders/mbx-archive").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["id"], "mbx-archive");
}

#[tokio::test]
async fn folder_unknown_returns_404_with_graph_error_envelope() {
    let (status, v) = get_json("/v1.0/me/mailFolders/ghost").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["code"], "ErrorItemNotFound");
}

#[tokio::test]
async fn list_messages_in_inbox() {
    let (status, v) = get_json("/v1.0/me/mailFolders/inbox/messages").await;
    assert_eq!(status, StatusCode::OK);
    let value = v["value"].as_array().unwrap();
    assert_eq!(value.len(), 2);
    // Determinism: receivedDateTime desc, id-lex tiebreak. email-002
    // is newer (11:00) than email-001 (10:00).
    assert_eq!(value[0]["id"], "email-002");
    assert_eq!(value[1]["id"], "email-001");

    let m = &value[0];
    assert_eq!(m["conversationId"], "email-002");
    assert_eq!(m["parentFolderId"], "mbx-inbox");
    assert_eq!(m["isRead"], false);
    assert_eq!(m["flag"]["flagStatus"], "notFlagged");
    assert_eq!(m["body"]["contentType"], "text");
    assert_eq!(m["body"]["content"], "Reply body.");
    // ISO 8601 with `Z` suffix.
    assert!(
        m["receivedDateTime"]
            .as_str()
            .unwrap()
            .ends_with("Z")
    );
    // Recipients shape.
    let from = &m["from"];
    assert_eq!(from["emailAddress"]["address"], "carol@example.com");
    let to = m["toRecipients"].as_array().unwrap();
    assert_eq!(to[0]["emailAddress"]["address"], "bob@example.com");
    // Headers projection.
    let headers = m["internetMessageHeaders"].as_array().unwrap();
    assert!(
        headers
            .iter()
            .any(|h| h["name"] == "Message-ID" && h["value"] == "<email-002@example.com>")
    );
    assert!(
        headers
            .iter()
            .any(|h| h["name"] == "In-Reply-To"
                && h["value"] == "<email-001@example.com>")
    );
}

#[tokio::test]
async fn list_messages_with_filter_after_drops_older_messages() {
    // Only email-002 received at 2026-01-15T11:00; email-001 at 10:00.
    let (status, v) = get_json(
        "/v1.0/me/mailFolders/inbox/messages?$filter=receivedDateTime%20ge%202026-01-15T10:30:00Z",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = v["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["email-002"]);
}

#[tokio::test]
async fn list_messages_pagination_emits_next_link_with_skiptoken() {
    let (_status, v) =
        get_json("/v1.0/me/mailFolders/inbox/messages?$top=1").await;
    let ids: Vec<&str> = v["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["email-002"]);
    let next = v["@odata.nextLink"].as_str().unwrap();
    assert!(next.starts_with("http://127.0.0.1:9999/"));
    assert!(next.contains("$skiptoken="));
    assert!(next.contains("$top=1"));

    // Following the nextLink yields the second page; no further nextLink.
    let next_path = next.trim_start_matches("http://127.0.0.1:9999");
    let (_status, v2) = get_json(next_path).await;
    let ids: Vec<&str> = v2["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["email-001"]);
    assert!(v2.get("@odata.nextLink").is_none());
}

#[tokio::test]
async fn delta_initial_emits_messages_then_delta_link() {
    let (_status, v) =
        get_json("/v1.0/me/mailFolders/inbox/messages/delta").await;
    assert_eq!(v["value"].as_array().unwrap().len(), 2);
    let delta = v["@odata.deltaLink"].as_str().unwrap();
    assert!(delta.contains("$deltatoken="));
    assert!(v.get("@odata.nextLink").is_none());

    // Following the deltaLink (subsequent cycle) returns empty +
    // same-shape deltaLink.
    let path = delta.trim_start_matches("http://127.0.0.1:9999");
    let (_status, v2) = get_json(path).await;
    assert!(v2["value"].as_array().unwrap().is_empty());
    assert!(v2["@odata.deltaLink"].as_str().unwrap().contains("$deltatoken="));
}

#[tokio::test]
async fn delta_with_deltatoken_latest_emits_empty_dump() {
    // The "$deltatoken=latest" shortcut: client uses this when
    // discovering a brand-new folder mid-cycle and just wants a
    // forward-only cursor, not a full message dump.
    let (_status, v) = get_json(
        "/v1.0/me/mailFolders/inbox/messages/delta?$deltatoken=latest",
    )
    .await;
    assert!(v["value"].as_array().unwrap().is_empty());
    assert!(v["@odata.deltaLink"].is_string());
}

#[tokio::test]
async fn delta_paginates_when_top_smaller_than_total() {
    // $top=1: first page has email-002 + nextLink (no deltaLink yet).
    let (_status, v) =
        get_json("/v1.0/me/mailFolders/inbox/messages/delta?$top=1").await;
    assert_eq!(v["value"].as_array().unwrap().len(), 1);
    assert!(v.get("@odata.nextLink").is_some());
    assert!(v.get("@odata.deltaLink").is_none());

    // Second page has email-001 + deltaLink (no nextLink).
    let next = v["@odata.nextLink"].as_str().unwrap();
    let path = next.trim_start_matches("http://127.0.0.1:9999");
    let (_status, v2) = get_json(path).await;
    assert_eq!(v2["value"].as_array().unwrap().len(), 1);
    assert!(v2.get("@odata.nextLink").is_none());
    assert!(v2.get("@odata.deltaLink").is_some());
}

#[tokio::test]
async fn unimplemented_paths_return_graph_shaped_404() {
    let (status, v) = get_json("/v1.0/me/calendar/events").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["code"], "ResourceNotImplemented");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("/v1.0/me/calendar/events")
    );
}

#[tokio::test]
async fn child_folders_for_top_level_folder() {
    // No fixture mailbox has a child of inbox, so this is empty.
    let (status, v) = get_json("/v1.0/me/mailFolders/inbox/childFolders").await;
    assert_eq!(status, StatusCode::OK);
    assert!(v["value"].as_array().unwrap().is_empty());
}
