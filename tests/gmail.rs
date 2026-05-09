#![allow(clippy::unwrap_used)]

//! End-to-end Gmail mail-sync tests against the canonical fixture,
//! driven via `tower::ServiceExt::oneshot` (no socket bind).

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use saehrimnir::{fixture, gmail, lua};

fn router() -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/jmap-small.toml")).unwrap();
    gmail::router(gmail::AppState::for_test(saehrimnir::shared::handle(fix)))
}

async fn get_json(uri: &str) -> (StatusCode, Value) {
    get_json_with(router(), uri).await
}

async fn get_json_with(router: axum::Router, uri: &str) -> (StatusCode, Value) {
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
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

fn attach_router() -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/jmap-attach.toml")).unwrap();
    gmail::router(gmail::AppState::for_test(saehrimnir::shared::handle(fix)))
}

#[tokio::test]
async fn gmail_get_thread_emits_multipart_payload_with_attachment_ref() {
    let (status, v) = get_json_with(
        attach_router(),
        "/gmail/v1/users/me/threads/email-001?format=full",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let payload = &v["messages"][0]["payload"];
    assert_eq!(payload["mimeType"], "multipart/mixed");
    let parts = payload["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["mimeType"], "text/plain");
    assert!(parts[0]["body"]["data"].is_string());
    let att = &parts[1];
    assert_eq!(att["mimeType"], "text/plain");
    assert_eq!(att["filename"], "sample.txt");
    assert_eq!(att["body"]["attachmentId"], "blob-att-001");
    assert!(att["body"]["data"].is_null() || att["body"].get("data").is_none());
}

#[tokio::test]
async fn gmail_get_attachment_returns_base64url_data() {
    let (status, v) = get_json_with(
        attach_router(),
        "/gmail/v1/users/me/messages/email-001/attachments/blob-att-001",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(v["size"].is_i64());
    let data = v["data"].as_str().unwrap();
    // base64url-no-pad of "attachment payload..."
    assert!(!data.contains('='));
    assert!(!data.contains('+'));
    assert!(!data.contains('/'));
    assert!(!data.is_empty());
}

#[tokio::test]
async fn gmail_get_attachment_unknown_blob_404() {
    let (status, _) = get_json_with(
        attach_router(),
        "/gmail/v1/users/me/messages/email-001/attachments/blob-nope",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn profile_returns_history_id_and_counts() {
    let (status, v) = get_json("/gmail/v1/users/me/profile").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["emailAddress"], "test@example.com");
    assert_eq!(v["messagesTotal"], 2);
    assert_eq!(v["threadsTotal"], 2);
    assert_eq!(v["historyId"], "1");
}

#[tokio::test]
async fn labels_include_system_and_fixture_user_labels() {
    let (status, v) = get_json("/gmail/v1/users/me/labels").await;
    assert_eq!(status, StatusCode::OK);
    let labels = v["labels"].as_array().unwrap();
    let system: Vec<&str> = labels
        .iter()
        .filter(|l| l["type"] == "system")
        .map(|l| l["id"].as_str().unwrap())
        .collect();
    // All eight system labels should be advertised, even if the
    // fixture has no Sent/Trash/etc. mailbox.
    for must in [
        "INBOX", "SENT", "DRAFT", "TRASH", "SPAM", "IMPORTANT", "STARRED", "UNREAD",
    ] {
        assert!(system.contains(&must), "missing {must}: {system:?}");
    }
    // INBOX has 2 messages, both unread.
    let inbox = labels.iter().find(|l| l["id"] == "INBOX").unwrap();
    assert_eq!(inbox["messagesTotal"], 2);
    assert_eq!(inbox["messagesUnread"], 2);
    assert_eq!(inbox["threadsTotal"], 2);
}

#[tokio::test]
async fn list_threads_returns_stubs_in_recent_order() {
    let (status, v) = get_json("/gmail/v1/users/me/threads").await;
    assert_eq!(status, StatusCode::OK);
    let threads = v["threads"].as_array().unwrap();
    assert_eq!(threads.len(), 2);
    // email-002 is newer (11:00) so its thread comes first.
    assert_eq!(threads[0]["id"], "email-002");
    assert_eq!(threads[1]["id"], "email-001");
    assert_eq!(threads[0]["historyId"], "1");
    assert!(v.get("nextPageToken").is_none());
    assert_eq!(v["resultSizeEstimate"], 2);
}

#[tokio::test]
async fn list_threads_paginates_via_next_page_token() {
    let (_status, v) =
        get_json("/gmail/v1/users/me/threads?maxResults=1").await;
    let threads = v["threads"].as_array().unwrap();
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0]["id"], "email-002");
    let token = v["nextPageToken"].as_str().unwrap();
    let (_status, v2) =
        get_json(&format!("/gmail/v1/users/me/threads?maxResults=1&pageToken={token}")).await;
    let threads = v2["threads"].as_array().unwrap();
    assert_eq!(threads[0]["id"], "email-001");
    assert!(v2.get("nextPageToken").is_none());
}

#[tokio::test]
async fn list_threads_filter_after_drops_older() {
    let (_status, v) =
        get_json("/gmail/v1/users/me/threads?q=after%3A2026%2F1%2F16").await;
    assert_eq!(v["threads"].as_array().unwrap().len(), 0);

    let (_status, v) =
        get_json("/gmail/v1/users/me/threads?q=after%3A2026%2F1%2F1").await;
    assert_eq!(v["threads"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn list_threads_unparseable_q_returns_400_not_unfiltered_dump() {
    // Regression: the buggy code silently ignored unparseable `q=`
    // and returned the full thread list. Now it errors out so
    // ratatoskr notices a typo or operator drift instead of
    // re-ingesting old threads.
    let (status, v) =
        get_json("/gmail/v1/users/me/threads?q=is%3Aunread").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"]["errors"][0]["reason"], "invalidQuery");
}

#[tokio::test]
async fn get_thread_full_format_returns_message_payload() {
    let (status, v) =
        get_json("/gmail/v1/users/me/threads/email-001?format=full").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["id"], "email-001");
    assert_eq!(v["historyId"], "1");
    let messages = v["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    let m = &messages[0];
    assert_eq!(m["id"], "email-001");
    assert_eq!(m["threadId"], "email-001");
    let label_ids: Vec<&str> = m["labelIds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(label_ids.contains(&"INBOX"));
    assert!(label_ids.contains(&"UNREAD"));
    // internalDate is Unix milliseconds quoted as string.
    let id = m["internalDate"].as_str().unwrap();
    assert!(id.parse::<i64>().is_ok(), "got: {id:?}");

    // Payload tree.
    let payload = &m["payload"];
    assert_eq!(payload["mimeType"], "text/plain");
    let headers: Vec<(String, String)> = payload["headers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            (
                h["name"].as_str().unwrap().to_string(),
                h["value"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let lookup = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    assert_eq!(
        lookup("From"),
        Some("<alice@example.com>".to_string())
    );
    assert_eq!(lookup("Subject"), Some("Hello".to_string()));
    assert_eq!(lookup("Content-Type"), Some("text/plain; charset=utf-8".to_string()));
    assert_eq!(
        lookup("Message-ID"),
        Some("<email-001@example.com>".to_string())
    );

    // body.data is base64url of "First message body."
    let data = payload["body"]["data"].as_str().unwrap();
    assert_eq!(data, "Rmlyc3QgbWVzc2FnZSBib2R5Lg");
    assert_eq!(payload["body"]["size"], 19);
}

#[tokio::test]
async fn get_thread_unknown_returns_404_with_gmail_envelope() {
    let (status, v) = get_json("/gmail/v1/users/me/threads/ghost").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["code"], 404);
    assert_eq!(v["error"]["errors"][0]["reason"], "notFound");
}

#[tokio::test]
async fn history_endpoint_is_stable_no_op() {
    let (status, v) =
        get_json("/gmail/v1/users/me/history?startHistoryId=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["history"].as_array().unwrap().len(), 0);
    assert_eq!(v["historyId"], "1");
}

#[tokio::test]
async fn attachment_fetch_returns_404_for_missing_blobs() {
    // v0 fixtures have no attachments; any call gets the canonical
    // Gmail error envelope.
    let (status, v) = get_json(
        "/gmail/v1/users/me/messages/email-001/attachments/aaa",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["errors"][0]["reason"], "notFound");
}

#[tokio::test]
async fn send_as_is_empty_so_signature_sync_is_a_noop() {
    let (status, v) =
        get_json("/gmail/v1/users/me/settings/sendAs").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["sendAs"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn unimplemented_paths_return_gmail_shaped_404() {
    let (status, v) = get_json("/gmail/v1/users/me/drafts").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["errors"][0]["reason"], "notFound");
}

// ── Reactive-callback tests ────────────────────────────────────────

fn router_with_lua_scenario(scenario: &str) -> axum::Router {
    let (fixture, dispatcher) =
        lua::load_source_with_dispatcher(scenario, "@cb").unwrap();
    gmail::router(
        gmail::AppState::for_test(saehrimnir::shared::handle(fixture)).with_dispatcher(Arc::new(dispatcher)),
    )
}

async fn get_json_via(router: axum::Router, uri: &str) -> (StatusCode, Value) {
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header(header::AUTHORIZATION, "Bearer x")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn list_threads_callback_overrides() {
    let scenario = r#"
        fixture({ name = "cb" })
        account({ id = "account-1", name = "test@example.com" })
        mailbox({ id = "mb", name = "Inbox", role = "inbox" })
        on("gmail", "list_threads", function(req)
            return { status = "rateLimitExceeded", message = "synthetic" }
        end)
    "#;
    let router = router_with_lua_scenario(scenario);
    let (status, v) = get_json_via(router, "/gmail/v1/users/me/threads").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"]["errors"][0]["reason"], "rateLimitExceeded");
}

#[tokio::test]
async fn get_thread_callback_passes_thread_id_to_script() {
    let scenario = r#"
        fixture({ name = "cb" })
        account({ id = "account-1", name = "test@example.com" })
        mailbox({ id = "mb", name = "Inbox", role = "inbox" })
        on("gmail", "get_thread", function(req)
            return { status = "notFound", message = "asked for " .. req.thread_id }
        end)
    "#;
    let router = router_with_lua_scenario(scenario);
    let (status, v) = get_json_via(
        router,
        "/gmail/v1/users/me/threads/abc-123?format=full",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"]["errors"][0]["reason"], "notFound");
    assert_eq!(v["error"]["message"], "asked for abc-123");
}

/// HTTP middleware records `(protocol="gmail", command="GET <path>",
/// detail.query)` per request. Mirrors the Graph version.
#[tokio::test]
async fn gmail_middleware_records_request_log_entries() {
    use saehrimnir::request_log::RequestLog;

    let request_log = RequestLog::default();
    let fix = fixture::load(std::path::Path::new("fixtures/jmap-small.toml")).unwrap();
    let app = gmail::router(
        gmail::AppState::for_test(saehrimnir::shared::handle(fix)).with_request_log(request_log.clone()),
    );

    let _ = get_json_via(app.clone(), "/gmail/v1/users/me/profile").await;
    let _ = get_json_via(app, "/gmail/v1/users/me/threads?q=after:2026/1/1").await;

    let snap = request_log.snapshot();
    assert_eq!(snap.len(), 2);
    assert_eq!(snap[0].protocol, "gmail");
    assert_eq!(snap[0].command, "GET /gmail/v1/users/me/profile");
    assert_eq!(snap[1].command, "GET /gmail/v1/users/me/threads");
    assert_eq!(snap[1].detail["query"], "q=after:2026/1/1");
}
