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

fn send_router() -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/send-small.toml")).unwrap();
    gmail::router(gmail::AppState::for_test(saehrimnir::shared::handle(fix)))
}

async fn post_json(router: axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(header::AUTHORIZATION, "Bearer doesnt-matter")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, v)
}

/// base64url-no-pad encode for building a `messages.send` `raw` body.
fn b64url(input: &[u8]) -> String {
    gmail::mail::base64url_no_pad(input)
}

#[tokio::test]
async fn gmail_messages_send_delivers_to_sent_and_history_reflects_it() {
    let app = send_router();

    // History baseline before the send (startHistoryId = 1).
    let raw =
        b"From: test@example.com\r\nTo: bob@example.com\r\nSubject: Sent via API\r\n\r\nHello body.\r\n";
    let (status, v) = post_json(
        app.clone(),
        "/gmail/v1/users/me/messages/send",
        serde_json::json!({ "raw": b64url(raw) }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let sent_id = v["id"].as_str().unwrap().to_string();
    // The delivered copy carries the SENT label and the parsed subject.
    let labels: Vec<&str> = v["labelIds"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(labels.contains(&"SENT"), "labelIds: {labels:?}");

    // A follow-up message GET finds the delivered message with subject.
    let (status, msg) = get_json_with(
        app.clone(),
        &format!("/gmail/v1/users/me/messages/{sent_id}?format=metadata"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let subject = msg["payload"]["headers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["name"] == "Subject")
        .and_then(|h| h["value"].as_str());
    assert_eq!(subject, Some("Sent via API"));

    // history.list from the pre-send historyId surfaces the add.
    let (status, hist) = get_json_with(app, "/gmail/v1/users/me/history?startHistoryId=1").await;
    assert_eq!(status, StatusCode::OK);
    let added = hist["history"]
        .as_array()
        .map(|records| {
            records.iter().any(|r| {
                r["messagesAdded"]
                    .as_array()
                    .map(|m| m.iter().any(|x| x["message"]["id"] == sent_id))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    assert!(added, "history did not reflect the sent message: {hist:?}");
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
        "INBOX",
        "SENT",
        "DRAFT",
        "TRASH",
        "SPAM",
        "IMPORTANT",
        "STARRED",
        "UNREAD",
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
    let (_status, v) = get_json("/gmail/v1/users/me/threads?maxResults=1").await;
    let threads = v["threads"].as_array().unwrap();
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0]["id"], "email-002");
    let token = v["nextPageToken"].as_str().unwrap();
    let (_status, v2) = get_json(&format!(
        "/gmail/v1/users/me/threads?maxResults=1&pageToken={token}"
    ))
    .await;
    let threads = v2["threads"].as_array().unwrap();
    assert_eq!(threads[0]["id"], "email-001");
    assert!(v2.get("nextPageToken").is_none());
}

#[tokio::test]
async fn list_threads_filter_after_drops_older() {
    let (_status, v) = get_json("/gmail/v1/users/me/threads?q=after%3A2026%2F1%2F16").await;
    assert_eq!(v["threads"].as_array().unwrap().len(), 0);

    let (_status, v) = get_json("/gmail/v1/users/me/threads?q=after%3A2026%2F1%2F1").await;
    assert_eq!(v["threads"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn list_threads_unparseable_q_returns_400_not_unfiltered_dump() {
    // Regression: the buggy code silently ignored unparseable `q=`
    // and returned the full thread list. Now it errors out so
    // ratatoskr notices a typo or operator drift instead of
    // re-ingesting old threads.
    let (status, v) = get_json("/gmail/v1/users/me/threads?q=is%3Aunread").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"]["errors"][0]["reason"], "invalidQuery");
}

#[tokio::test]
async fn get_thread_full_format_returns_message_payload() {
    let (status, v) = get_json("/gmail/v1/users/me/threads/email-001?format=full").await;
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
    assert_eq!(lookup("From"), Some("<alice@example.com>".to_string()));
    assert_eq!(lookup("Subject"), Some("Hello".to_string()));
    assert_eq!(
        lookup("Content-Type"),
        Some("text/plain; charset=utf-8".to_string())
    );
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
    let (status, v) = get_json("/gmail/v1/users/me/history?startHistoryId=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["history"].as_array().unwrap().len(), 0);
    assert_eq!(v["historyId"], "1");
}

// ── Messages (bifrost's message-centric backfill) ──────────────────
//
// bifrost lists `messages` (not `threads`) and hydrates each via
// `messages.get`. These routes are what unblock its backfill.

#[tokio::test]
async fn list_messages_returns_messages_in_recent_order() {
    let (status, v) = get_json("/gmail/v1/users/me/messages").await;
    assert_eq!(status, StatusCode::OK);
    let messages = v["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    // email-002 is newer (11:00), so it sorts first.
    assert_eq!(messages[0]["id"], "email-002");
    assert_eq!(messages[0]["threadId"], "email-002");
    assert_eq!(messages[1]["id"], "email-001");
    assert!(v.get("nextPageToken").is_none());
    assert_eq!(v["resultSizeEstimate"], 2);
}

#[tokio::test]
async fn list_messages_paginates_via_next_page_token() {
    let (_status, v) = get_json("/gmail/v1/users/me/messages?maxResults=1").await;
    let messages = v["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["id"], "email-002");
    let token = v["nextPageToken"].as_str().unwrap();
    let (_status, v2) = get_json(&format!(
        "/gmail/v1/users/me/messages?maxResults=1&pageToken={token}"
    ))
    .await;
    let messages = v2["messages"].as_array().unwrap();
    assert_eq!(messages[0]["id"], "email-001");
    assert!(v2.get("nextPageToken").is_none());
}

#[tokio::test]
async fn list_messages_filter_after_drops_older() {
    let (_status, v) = get_json("/gmail/v1/users/me/messages?q=after%3A2026%2F1%2F16").await;
    assert_eq!(v["messages"].as_array().unwrap().len(), 0);

    let (_status, v) = get_json("/gmail/v1/users/me/messages?q=after%3A2026%2F1%2F1").await;
    assert_eq!(v["messages"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn list_messages_unparseable_q_returns_400() {
    // Same strict contract as `list_threads`: an unsupported query
    // operator is a hard 400, never a silent full-list dump.
    let (status, v) = get_json("/gmail/v1/users/me/messages?q=is%3Aunread").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"]["errors"][0]["reason"], "invalidQuery");
}

#[tokio::test]
async fn get_message_full_returns_payload_and_labels() {
    let (status, v) = get_json("/gmail/v1/users/me/messages/email-001?format=full").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["id"], "email-001");
    assert_eq!(v["threadId"], "email-001");
    let label_ids: Vec<&str> = v["labelIds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(label_ids.contains(&"INBOX"));
    assert!(label_ids.contains(&"UNREAD"));
    let id = v["internalDate"].as_str().unwrap();
    assert!(id.parse::<i64>().is_ok(), "got: {id:?}");
    // Same payload projection as the thread surface emits per message.
    assert_eq!(v["payload"]["mimeType"], "text/plain");
    assert_eq!(v["payload"]["body"]["data"], "Rmlyc3QgbWVzc2FnZSBib2R5Lg");
}

#[tokio::test]
async fn get_message_default_format_emits_payload() {
    // No `format=` defaults to `full`, so the structured payload is
    // present.
    let (status, v) = get_json("/gmail/v1/users/me/messages/email-001").await;
    assert_eq!(status, StatusCode::OK);
    assert!(v["payload"].is_object());
}

#[tokio::test]
async fn get_message_raw_emits_top_level_raw_and_drops_payload() {
    // bifrost's `raw_bytes()` reads a top-level base64url `raw` field
    // and errors `ParseFailed` without it; `format=raw` swaps the
    // structured payload for the assembled RFC 822 bytes.
    let (status, v) = get_json("/gmail/v1/users/me/messages/email-001?format=raw").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["id"], "email-001");
    assert!(v.get("payload").is_none(), "raw format must drop payload");
    let raw = v["raw"].as_str().unwrap();
    assert!(!raw.is_empty());
    // base64url, no padding.
    assert!(!raw.contains('='));
    assert!(!raw.contains('+'));
    assert!(!raw.contains('/'));
}

#[tokio::test]
async fn get_message_unknown_returns_404_with_gmail_envelope() {
    let (status, v) = get_json("/gmail/v1/users/me/messages/ghost").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["code"], 404);
    assert_eq!(v["error"]["errors"][0]["reason"], "notFound");
}

#[tokio::test]
async fn attachment_fetch_returns_404_for_missing_blobs() {
    // v0 fixtures have no attachments; any call gets the canonical
    // Gmail error envelope.
    let (status, v) = get_json("/gmail/v1/users/me/messages/email-001/attachments/aaa").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["errors"][0]["reason"], "notFound");
}

#[tokio::test]
async fn send_as_empty_fixture_returns_empty_list() {
    // `fixtures/jmap-small.toml` declares no `[[send_as]]` rows;
    // a GET still returns the envelope with an empty array.
    let (status, v) = get_json("/gmail/v1/users/me/settings/sendAs").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["sendAs"].as_array().unwrap().len(), 0);
}

fn multi_account_router() -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/multi-account-small.toml")).unwrap();
    gmail::router(gmail::AppState::for_test(saehrimnir::shared::handle(fix)))
}

async fn get_json_with_bearer(
    router: axum::Router,
    uri: &str,
    bearer: &str,
) -> (StatusCode, Value) {
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
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
async fn send_as_lists_fixture_authored_identities() {
    // The multi-account fixture declares one `[[send_as]]` per
    // account. Without a bearer token, sæhrimnir falls back to the
    // primary account, so we see only the primary's identity.
    let (status, v) =
        get_json_with(multi_account_router(), "/gmail/v1/users/me/settings/sendAs").await;
    assert_eq!(status, StatusCode::OK);
    let list = v["sendAs"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["sendAsEmail"], "primary@example.com");
    assert_eq!(list[0]["displayName"], "Primary User");
    assert_eq!(list[0]["isPrimary"], true);
    assert_eq!(list[0]["isDefault"], true);
    assert!(list[0]["signature"].as_str().unwrap().contains("primary"));
}

#[tokio::test]
async fn send_as_get_by_email_returns_the_entry() {
    let (status, v) = get_json_with(
        multi_account_router(),
        "/gmail/v1/users/me/settings/sendAs/primary@example.com",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["sendAsEmail"], "primary@example.com");
    assert_eq!(v["displayName"], "Primary User");
}

#[tokio::test]
async fn send_as_get_unknown_email_returns_404() {
    let (status, v) = get_json_with(
        multi_account_router(),
        "/gmail/v1/users/me/settings/sendAs/nobody@example.com",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["errors"][0]["reason"], "notFound");
}

#[tokio::test]
async fn send_as_patch_replaces_listed_fields_only() {
    let app = multi_account_router();
    let patch = serde_json::json!({
        "signature": "<p>new sig</p>",
        "displayName": "Renamed User"
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/gmail/v1/users/me/settings/sendAs/primary@example.com")
                .header(header::AUTHORIZATION, "Bearer x")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&patch).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["signature"], "<p>new sig</p>");
    assert_eq!(v["displayName"], "Renamed User");
    // isDefault/isPrimary were not in the body; their original
    // values must survive.
    assert_eq!(v["isPrimary"], true);
    assert_eq!(v["isDefault"], true);

    // GET after PATCH surfaces the updated signature - the write
    // persisted on the shared fixture handle.
    let (status, v) = get_json_with(
        app,
        "/gmail/v1/users/me/settings/sendAs/primary@example.com",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["signature"], "<p>new sig</p>");
    assert_eq!(v["displayName"], "Renamed User");
}

#[tokio::test]
async fn send_as_patch_ignores_is_primary_per_real_gmail() {
    // Real Gmail rejects writes to `isPrimary`; v0 silently ignores
    // it so a misbehaving client doesn't corrupt the fixture.
    let app = multi_account_router();
    let patch = serde_json::json!({ "isPrimary": false });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/gmail/v1/users/me/settings/sendAs/primary@example.com")
                .header(header::AUTHORIZATION, "Bearer x")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&patch).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(v["isPrimary"], true, "isPrimary should be read-only");
}

#[tokio::test]
async fn send_as_with_bearer_scoping_returns_each_accounts_identity() {
    // Mint a token bound to the secondary account; the resulting
    // sendAs list contains only secondary's identity.
    use saehrimnir::oauth::TokenStore;
    let store = TokenStore::default();
    let token_secondary = store.mint("authorization_code", "account-secondary", 1);

    let mut fix = fixture::load(std::path::Path::new("fixtures/multi-account-small.toml")).unwrap();
    fix.oauth = saehrimnir::fixture::OAuthConfig {
        enforce: false,
        issuer: "https://saehrimnir.test/oauth".to_string(),
    };
    let handle = saehrimnir::shared::handle(fix);
    let shared = saehrimnir::shared::SharedHandles::for_test(handle).with_token_store(store);
    let app = gmail::router(gmail::AppState { shared });

    let (status, v) =
        get_json_with_bearer(app, "/gmail/v1/users/me/settings/sendAs", &token_secondary).await;
    assert_eq!(status, StatusCode::OK);
    let list = v["sendAs"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["sendAsEmail"], "secondary@example.com");
}

#[tokio::test]
async fn unimplemented_paths_return_gmail_shaped_404() {
    let (status, v) = get_json("/gmail/v1/users/me/drafts").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["errors"][0]["reason"], "notFound");
}

// ── Reactive-callback tests ────────────────────────────────────────

fn router_with_lua_scenario(scenario: &str) -> axum::Router {
    let (fixture, dispatcher) = lua::load_source_with_dispatcher(scenario, "@cb").unwrap();
    gmail::router(
        gmail::AppState::for_test(saehrimnir::shared::handle(fixture))
            .with_dispatcher(Arc::new(dispatcher)),
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
    let (status, v) = get_json_via(router, "/gmail/v1/users/me/threads/abc-123?format=full").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"]["errors"][0]["reason"], "notFound");
    assert_eq!(v["error"]["message"], "asked for abc-123");
}

#[tokio::test]
async fn get_message_callback_passes_message_id_to_script() {
    let scenario = r#"
        fixture({ name = "cb" })
        account({ id = "account-1", name = "test@example.com" })
        mailbox({ id = "mb", name = "Inbox", role = "inbox" })
        on("gmail", "get_message", function(req)
            return { status = "notFound", message = "asked for " .. req.message_id }
        end)
    "#;
    let router = router_with_lua_scenario(scenario);
    let (status, v) = get_json_via(router, "/gmail/v1/users/me/messages/msg-xyz?format=full").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"]["errors"][0]["reason"], "notFound");
    assert_eq!(v["error"]["message"], "asked for msg-xyz");
}

#[tokio::test]
async fn get_attachment_callback_passes_both_path_segments_to_script() {
    let scenario = r#"
        fixture({ name = "cb" })
        account({ id = "account-1", name = "test@example.com" })
        mailbox({ id = "mb", name = "Inbox", role = "inbox" })
        on("gmail", "get_attachment", function(req)
            return {
                status = "notFound",
                message = "asked for " .. req.message_id .. "/" .. req.attachment_id,
            }
        end)
    "#;
    let router = router_with_lua_scenario(scenario);
    let (status, v) = get_json_via(
        router,
        "/gmail/v1/users/me/messages/msg-xyz/attachments/blob-abc",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"]["errors"][0]["reason"], "notFound");
    assert_eq!(v["error"]["message"], "asked for msg-xyz/blob-abc");
}

/// HTTP middleware records `(protocol="gmail", command="GET <path>",
/// detail.query)` per request. Mirrors the Graph version.
#[tokio::test]
async fn gmail_middleware_records_request_log_entries() {
    use saehrimnir::request_log::RequestLog;

    let request_log = RequestLog::default();
    let fix = fixture::load(std::path::Path::new("fixtures/jmap-small.toml")).unwrap();
    let app = gmail::router(
        gmail::AppState::for_test(saehrimnir::shared::handle(fix))
            .with_request_log(request_log.clone()),
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

// ── Multi-account (Stage 4: OAuth-scoped tokens) ────────────────────
//
// Gmail's `users/me` placeholder means "the account this OAuth
// token was minted for". Stage 4 of the multi-account refactor
// makes the mock honour that: tokens carry an account_id, and the
// Gmail handlers scope reads by the bearer-token-resolved account
// (falling back to primary when the bearer is unknown).

fn multi_account_gmail_router(store: saehrimnir::oauth::TokenStore) -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/multi-account-small.toml")).unwrap();
    let shared = saehrimnir::shared::SharedHandles::for_test(saehrimnir::shared::handle(fix))
        .with_token_store(store);
    gmail::router(gmail::AppState { shared })
}

async fn get_with_bearer(router: axum::Router, uri: &str, token: &str) -> (StatusCode, Value) {
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
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
async fn gmail_profile_uses_bearer_token_account() {
    let store = saehrimnir::oauth::TokenStore::default();
    let primary_token = store.mint("authorization_code", "account-primary", 1);
    let secondary_token = store.mint("authorization_code", "account-secondary", 2);

    let (_, v) = get_with_bearer(
        multi_account_gmail_router(store.clone()),
        "/gmail/v1/users/me/profile",
        &primary_token,
    )
    .await;
    assert_eq!(v["emailAddress"], "primary@example.com");

    let (_, v) = get_with_bearer(
        multi_account_gmail_router(store),
        "/gmail/v1/users/me/profile",
        &secondary_token,
    )
    .await;
    assert_eq!(v["emailAddress"], "secondary@example.com");
}

#[tokio::test]
async fn gmail_threads_scope_by_bearer_token_account() {
    let store = saehrimnir::oauth::TokenStore::default();
    let secondary_token = store.mint("authorization_code", "account-secondary", 1);

    let (_, v) = get_with_bearer(
        multi_account_gmail_router(store),
        "/gmail/v1/users/me/threads",
        &secondary_token,
    )
    .await;
    let threads = v["threads"].as_array().unwrap();
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0]["id"], "email-secondary-001");
}

#[tokio::test]
async fn gmail_unknown_bearer_falls_back_to_primary() {
    // Tokens that aren't in the store (or no token at all) keep
    // returning the primary account's resources - matches the v0
    // no-auth baseline for single-account fixtures.
    let store = saehrimnir::oauth::TokenStore::default();
    let (_, v) = get_with_bearer(
        multi_account_gmail_router(store),
        "/gmail/v1/users/me/profile",
        "doesnt-matter",
    )
    .await;
    assert_eq!(v["emailAddress"], "primary@example.com");
}

/// Drive an arbitrary-method JSON request (PATCH / DELETE) against a
/// reusable Gmail router so a stateful CRUD sequence shares one fixture.
async fn send_method(
    router: axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, "Bearer doesnt-matter");
    let req = match body {
        Some(b) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            builder.body(Body::from(serde_json::to_vec(&b).unwrap())).unwrap()
        }
        None => builder.body(Body::empty()).unwrap(),
    };
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, v)
}

fn label_by_name<'a>(list: &'a Value, name: &str) -> Option<&'a Value> {
    list["labels"]
        .as_array()?
        .iter()
        .find(|l| l["name"] == serde_json::json!(name))
}

/// Gmail label-CRUD round-trip mirroring ratatoskr's
/// `gmail-container-crud` gate at the mock's own request layer: a user
/// label is created, renamed, recolored, and deleted, and every
/// `labels.list` readback (the gate's `containers_list`) must reflect
/// the mutation - including the recolor's `backgroundColor`, which the
/// gate asserts as `server_color_bg`.
#[tokio::test]
async fn label_create_rename_recolor_delete_round_trips_through_list() {
    let app = router();

    // Create a user label (no color, matching the gate's first step).
    let (status, created) = post_json(
        app.clone(),
        "/gmail/v1/users/me/labels",
        serde_json::json!({ "name": "HarnessTag" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let label_id = created["id"].as_str().unwrap().to_string();

    let (_, list) = get_json_with(app.clone(), "/gmail/v1/users/me/labels").await;
    assert!(label_by_name(&list, "HarnessTag").is_some(), "label missing after create");

    // Rename.
    let (status, _) = send_method(
        app.clone(),
        "PATCH",
        &format!("/gmail/v1/users/me/labels/{label_id}"),
        Some(serde_json::json!({ "name": "HarnessTagRenamed" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, list) = get_json_with(app.clone(), "/gmail/v1/users/me/labels").await;
    assert!(label_by_name(&list, "HarnessTagRenamed").is_some(), "renamed label missing");
    assert!(label_by_name(&list, "HarnessTag").is_none(), "old label name still present");

    // Recolor: the color must round-trip into labels.list.
    let (status, _) = send_method(
        app.clone(),
        "PATCH",
        &format!("/gmail/v1/users/me/labels/{label_id}"),
        Some(serde_json::json!({
            "name": "HarnessTagRenamed",
            "color": { "textColor": "#ffffff", "backgroundColor": "#fb4c2f" }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, list) = get_json_with(app.clone(), "/gmail/v1/users/me/labels").await;
    let recolored = label_by_name(&list, "HarnessTagRenamed").expect("label missing after recolor");
    assert_eq!(
        recolored["color"]["backgroundColor"], "#fb4c2f",
        "label recolor did not round-trip in labels.list"
    );
    assert_eq!(recolored["color"]["textColor"], "#ffffff");

    // Delete.
    let (status, _) = send_method(
        app.clone(),
        "DELETE",
        &format!("/gmail/v1/users/me/labels/{label_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, list) = get_json_with(app.clone(), "/gmail/v1/users/me/labels").await;
    assert!(
        label_by_name(&list, "HarnessTagRenamed").is_none(),
        "label still present after delete"
    );
}

fn repro_router() -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/gmail-initial-repro.toml")).unwrap();
    gmail::router(gmail::AppState::for_test(saehrimnir::shared::handle(fix)))
}

/// Replays bifrost's exact Gmail initial-sync endpoint sequence against
/// a fixture that carries a roleless "user label" mailbox (the shape the
/// Gmail label-CRUD work models user labels with). Mirrors
/// ratatoskr's `gmail-initial` gate at the mock's own request layer:
///
///   profile -> labels.list -> messages.list (no `q`) -> per-message
///   `messages.get` (metadata / raw / full).
///
/// The load-bearing invariant is bifrost's membership-scope resolution:
/// `discover_memberships` learns the valid label scopes from
/// `labels.list`, and every `labelId` a message carries must be one of
/// those scopes. A message whose `labelIds` reference a label absent
/// from `labels.list` cannot resolve its membership, so initial sync
/// drops it (or terminates) - the "returns 0 messages" symptom.
#[tokio::test]
async fn gmail_initial_sync_replay_message_labels_are_known_scopes() {
    let app = repro_router();

    // 1. profile: emailAddress + numeric historyId, real message count.
    let (status, profile) = get_json_with(app.clone(), "/gmail/v1/users/me/profile").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(profile["emailAddress"], "test@example.com");
    assert!(
        profile["historyId"].as_str().unwrap().parse::<u64>().is_ok(),
        "historyId must parse as u64: {:?}",
        profile["historyId"]
    );
    assert_eq!(profile["messagesTotal"], 2, "profile message count");

    // 2. labels.list: collect the known label-scope ids (what bifrost's
    //    discover_memberships treats as the valid membership universe).
    let (status, labels) = get_json_with(app.clone(), "/gmail/v1/users/me/labels").await;
    assert_eq!(status, StatusCode::OK);
    let known_labels: std::collections::BTreeSet<String> = labels["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["id"].as_str().unwrap().to_string())
        .collect();
    // System folders bifrost maps (INBOX + IMPORTANT) must be present.
    assert!(known_labels.contains("INBOX"), "labels: {known_labels:?}");
    assert!(known_labels.contains("IMPORTANT"), "labels: {known_labels:?}");

    // 3. messages.list with no `q` (bifrost's inventory backfill).
    let (status, list) =
        get_json_with(app.clone(), "/gmail/v1/users/me/messages?maxResults=500").await;
    assert_eq!(status, StatusCode::OK);
    let stubs = list["messages"].as_array().unwrap();
    assert_eq!(stubs.len(), 2, "messages.list must return the fixture's mail");

    // 4. per-message metadata hydration: every labelId the message
    //    carries must be a known label scope from labels.list.
    let mut hydrated = 0;
    for stub in stubs {
        let id = stub["id"].as_str().unwrap();
        let (status, msg) = get_json_with(
            app.clone(),
            &format!("/gmail/v1/users/me/messages/{id}?format=metadata"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "metadata get for {id}");
        for label in msg["labelIds"].as_array().unwrap() {
            let label = label.as_str().unwrap();
            assert!(
                known_labels.contains(label),
                "message {id} carries label {label:?} not advertised by labels.list \
                 (bifrost cannot resolve the membership scope, so the message is dropped); \
                 known labels: {known_labels:?}"
            );
        }

        // 5. raw + full hydration must both succeed (body fetch path).
        let (status, raw) = get_json_with(
            app.clone(),
            &format!("/gmail/v1/users/me/messages/{id}?format=raw"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "raw get for {id}");
        assert!(raw["raw"].is_string(), "raw projection missing for {id}");

        let (status, full) = get_json_with(
            app.clone(),
            &format!("/gmail/v1/users/me/messages/{id}?format=full"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "full get for {id}");
        assert!(full["payload"].is_object(), "full payload missing for {id}");
        hydrated += 1;
    }
    assert_eq!(hydrated, 2, "both fixture messages hydrate end to end");
}
