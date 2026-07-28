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
async fn send_as_patch_records_account_and_body_in_request_log() {
    // The signature-writeback round-trip: a token minted for the
    // secondary account PATCHes its own sendAs signature. The mock
    // must record the resolved account, the sendAs target, and the
    // request body so a multi-account harness can assert the write
    // hit the right account with the right signature.
    use saehrimnir::oauth::TokenStore;
    use saehrimnir::request_log::RequestLog;

    let store = TokenStore::default();
    let token_secondary = store.mint("authorization_code", "account-secondary", 1);

    let request_log = RequestLog::default();
    let fix = fixture::load(std::path::Path::new("fixtures/multi-account-small.toml")).unwrap();
    let shared = saehrimnir::shared::SharedHandles::for_test(saehrimnir::shared::handle(fix))
        .with_token_store(store);
    let app = gmail::router(gmail::AppState { shared }.with_request_log(request_log.clone()));

    let patch = serde_json::json!({ "signature": "<p>secondary sig</p>" });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/gmail/v1/users/me/settings/sendAs/secondary@example.com")
                .header(header::AUTHORIZATION, format!("Bearer {token_secondary}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&patch).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(v["signature"], "<p>secondary sig</p>");

    // The enriched handler entry carries account + sendAs target +
    // body, alongside the middleware's bare `METHOD path` entry.
    let snap = request_log.snapshot();
    let enriched = snap
        .iter()
        .find(|e| {
            e.command == "PATCH /gmail/v1/users/me/settings/sendAs/secondary@example.com"
                && e.detail.get("account").is_some()
        })
        .expect("enriched sendAs PATCH entry recorded");
    assert_eq!(enriched.protocol, "gmail");
    assert_eq!(enriched.detail["account"], "account-secondary");
    assert_eq!(enriched.detail["sendAsEmail"], "secondary@example.com");
    assert_eq!(enriched.detail["body"]["signature"], "<p>secondary sig</p>");
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
            builder
                .body(Body::from(serde_json::to_vec(&b).unwrap()))
                .unwrap()
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
    assert!(
        label_by_name(&list, "HarnessTag").is_some(),
        "label missing after create"
    );

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
    assert!(
        label_by_name(&list, "HarnessTagRenamed").is_some(),
        "renamed label missing"
    );
    assert!(
        label_by_name(&list, "HarnessTag").is_none(),
        "old label name still present"
    );

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
        profile["historyId"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .is_ok(),
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
    assert!(
        known_labels.contains("IMPORTANT"),
        "labels: {known_labels:?}"
    );

    // 3. messages.list with no `q` (bifrost's inventory backfill).
    let (status, list) =
        get_json_with(app.clone(), "/gmail/v1/users/me/messages?maxResults=500").await;
    assert_eq!(status, StatusCode::OK);
    let stubs = list["messages"].as_array().unwrap();
    assert_eq!(
        stubs.len(),
        2,
        "messages.list must return the fixture's mail"
    );

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

// ── Multi-message threads: message-target vs thread-target ──────────
//
// bifrost's Gmail mutation pipeline (`google pim.rs::modify_target`)
// splits on the mutation target: a `MutationTarget::Message` becomes
// `POST /messages/{id}/modify`, a `MutationTarget::Thread` becomes
// `POST /threads/{id}/modify`, and an already-trashed thread destroy
// becomes `DELETE /threads/{id}`. A consumer gating "a label change on
// ONE message of a multi-message thread leaves the other messages'
// membership alone" needs all three served, and needs `history.list` to
// stay MESSAGE-scoped so the two shapes are distinguishable in a delta.

fn threads_router() -> axum::Router {
    let path = std::path::Path::new("fixtures/gmail-threads.lua");
    let source = std::fs::read_to_string(path).unwrap();
    let chunk = format!("@{}", path.display());
    let fix = lua::load_source_with_dir(&source, &chunk, path.parent().unwrap()).unwrap();
    gmail::router(gmail::AppState::for_test(saehrimnir::shared::handle(fix)))
}

/// The Gmail label set of `message_id` inside a `threads.get` body.
fn thread_labels(thread: &Value, message_id: &str) -> Vec<String> {
    let message = thread["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == message_id)
        .unwrap_or_else(|| panic!("thread carries no message {message_id}: {thread}"));
    message["labelIds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

/// Every `(message id, labelIds)` pair a `history.list` body reports
/// under `key` (`labelsAdded` / `labelsRemoved`), across all records.
fn history_label_entries(history: &Value, key: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    for record in history["history"].as_array().unwrap() {
        let Some(entries) = record.get(key).and_then(Value::as_array) else {
            continue;
        };
        for entry in entries {
            let labels = entry["labelIds"]
                .as_array()
                .unwrap_or_else(|| {
                    panic!("{key} entry has no labelIds array (bifrost's GmailHistoryLabelWrapper requires it): {entry}")
                })
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect();
            out.push((entry["message"]["id"].as_str().unwrap().to_string(), labels));
        }
    }
    out
}

#[tokio::test]
async fn gmail_message_target_modify_leaves_thread_siblings_intact() {
    let app = threads_router();

    let (status, before) =
        get_json_with(app.clone(), "/gmail/v1/users/me/threads/thread-multi").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(before["messages"].as_array().unwrap().len(), 3);
    let a_before = thread_labels(&before, "msg-a");
    let c_before = thread_labels(&before, "msg-c");
    // The siblings must differ from each other, or "unchanged" could
    // hold by accident on a uniform thread.
    assert!(c_before.contains(&"UNREAD".to_string()), "{c_before:?}");
    assert!(!a_before.contains(&"UNREAD".to_string()), "{a_before:?}");

    // Star exactly one message of the thread.
    let (status, modified) = post_json(
        app.clone(),
        "/gmail/v1/users/me/messages/msg-b/modify",
        serde_json::json!({ "addLabelIds": ["STARRED"], "removeLabelIds": [] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(modified["id"], "msg-b");
    assert_eq!(modified["threadId"], "thread-multi");
    let modified_labels: Vec<&str> = modified["labelIds"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(modified_labels.contains(&"STARRED"), "{modified_labels:?}");

    // The siblings' membership survives, byte for byte.
    let (status, after) =
        get_json_with(app.clone(), "/gmail/v1/users/me/threads/thread-multi").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(after["messages"].as_array().unwrap().len(), 3);
    assert_eq!(thread_labels(&after, "msg-a"), a_before);
    assert_eq!(thread_labels(&after, "msg-c"), c_before);
    assert!(thread_labels(&after, "msg-b").contains(&"STARRED".to_string()));

    // And the delta names that one message alone. This is the half a
    // consumer cannot fake: a thread-scoped record would let a
    // whole-thread change masquerade as a single-message one.
    let (status, history) = get_json_with(app, "/gmail/v1/users/me/history?startHistoryId=1").await;
    assert_eq!(status, StatusCode::OK);
    let added = history_label_entries(&history, "labelsAdded");
    assert_eq!(
        added,
        vec![("msg-b".to_string(), vec!["STARRED".to_string()])],
        "history: {history}"
    );
    assert!(history_label_entries(&history, "labelsRemoved").is_empty());
}

#[tokio::test]
async fn gmail_thread_target_modify_touches_every_message_in_the_thread() {
    let app = threads_router();

    // The same patch bifrost sends for a message target, aimed at the
    // thread instead: every message in it must move.
    let (status, thread) = post_json(
        app.clone(),
        "/gmail/v1/users/me/threads/thread-multi/modify",
        serde_json::json!({ "addLabelIds": ["STARRED"], "removeLabelIds": [] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // The response is the shape bifrost deserializes (`GmailThread`).
    assert_eq!(thread["id"], "thread-multi");
    let messages = thread["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    for message in messages {
        let labels: Vec<&str> = message["labelIds"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(
            labels.contains(&"STARRED"),
            "message {} missed the thread-wide label: {labels:?}",
            message["id"]
        );
    }

    // One history record PER MESSAGE, not one for the thread - which is
    // what makes this distinguishable from the single-message case.
    let (_, history) =
        get_json_with(app.clone(), "/gmail/v1/users/me/history?startHistoryId=1").await;
    let mut added = history_label_entries(&history, "labelsAdded");
    added.sort();
    assert_eq!(
        added,
        vec![
            ("msg-a".to_string(), vec!["STARRED".to_string()]),
            ("msg-b".to_string(), vec!["STARRED".to_string()]),
            ("msg-c".to_string(), vec!["STARRED".to_string()]),
        ],
        "history: {history}"
    );

    // A thread the account does not have is a per-request notFound,
    // not a silent success.
    let (status, v) = post_json(
        app,
        "/gmail/v1/users/me/threads/no-such-thread/modify",
        serde_json::json!({ "addLabelIds": ["STARRED"] }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["errors"][0]["reason"], "notFound");
}

#[tokio::test]
async fn gmail_thread_delete_destroys_every_message_and_reports_tombstones() {
    let app = threads_router();

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/gmail/v1/users/me/threads/thread-trashed")
                .header(header::AUTHORIZATION, "Bearer doesnt-matter")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let (status, _) = get_json_with(app.clone(), "/gmail/v1/users/me/threads/thread-trashed").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Both messages surface as tombstones, and the untouched threads
    // are not swept up with them.
    let (_, history) = get_json_with(app, "/gmail/v1/users/me/history?startHistoryId=1").await;
    let mut deleted: Vec<String> = history["history"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r.get("messagesDeleted").and_then(Value::as_array))
        .flatten()
        .map(|e| e["message"]["id"].as_str().unwrap().to_string())
        .collect();
    deleted.sort();
    assert_eq!(deleted, vec!["msg-t1".to_string(), "msg-t2".to_string()]);
}

// ── Bulk mutation: combined add-and-remove, and archive ─────────────
//
// bifrost's Gmail bulk driver (`google account/mutation.rs`) has no
// "move" verb to reach for: a move is `messages.batchModify` carrying
// `addLabelIds` (the destination) and `removeLabelIds` (the container
// being left) in ONE request. The source-carrying bulk-move work makes
// `removeLabelIds` name the actual source instead of a blanket INBOX,
// so the mock has to honour both halves of a single patch, in the
// right order, including when the removed label is a mutually
// exclusive system container (SPAM / TRASH).
//
// These paths were entirely untested before: `batchModify` and
// `batchDelete` had no coverage at all, which is precisely the
// wrong-shape trap - the mock could have served any shape and every
// gate would still have passed.

fn bulk_fixture() -> saehrimnir::shared::FixtureHandle {
    let path = std::path::Path::new("fixtures/gmail-bulk-move.lua");
    saehrimnir::shared::handle(fixture::load(path).unwrap())
}

fn bulk_router(handle: &saehrimnir::shared::FixtureHandle) -> axum::Router {
    gmail::router(gmail::AppState::for_test(Arc::clone(handle)))
}

/// Gmail `labelIds` of one message, sorted, read straight off
/// `messages.get`.
async fn message_labels(router: axum::Router, id: &str) -> Vec<String> {
    let (status, v) = get_json_with(router, &format!("/gmail/v1/users/me/messages/{id}")).await;
    assert_eq!(status, StatusCode::OK, "messages.get {id}: {v}");
    let mut labels: Vec<String> = v["labelIds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect();
    labels.sort();
    labels
}

/// The fixture-level mailbox membership of one message. The Gmail wire
/// projection deliberately cannot show this for an archived message
/// (All Mail has no label), so proving "the archive fallback landed it
/// somewhere real" needs the canonical state.
fn mailbox_ids(handle: &saehrimnir::shared::FixtureHandle, id: &str) -> Vec<String> {
    let fix = handle.read().unwrap();
    let mut ids = fix
        .emails
        .iter()
        .find(|e| e.id == id)
        .unwrap_or_else(|| panic!("no email {id}"))
        .mailbox_ids
        .clone();
    ids.sort();
    ids
}

#[tokio::test]
async fn gmail_batch_modify_applies_add_and_remove_in_one_request() {
    let handle = bulk_fixture();
    let app = bulk_router(&handle);

    // The shape a source-carrying bulk move drives: destination and
    // source in one patch, over several ids.
    let (status, body) = post_json(
        app.clone(),
        "/gmail/v1/users/me/messages/batchModify",
        serde_json::json!({
            "ids": ["msg-1", "msg-2"],
            "addLabelIds": ["Label_mb-work"],
            "removeLabelIds": ["INBOX"],
        }),
    )
    .await;
    // bifrost reads no body from batchModify; real Gmail answers 204.
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(body, Value::Null);

    for id in ["msg-1", "msg-2"] {
        let labels = message_labels(app.clone(), id).await;
        assert!(
            labels.contains(&"Label_mb-work".to_string()),
            "{id} missed the destination: {labels:?}"
        );
        assert!(
            !labels.contains(&"INBOX".to_string()),
            "{id} kept the source it was moved out of: {labels:?}"
        );
        // Both halves landed on the same message in the same request -
        // an add-only mock would keep INBOX, a remove-only mock would
        // drop the message into All Mail.
        assert_eq!(mailbox_ids(&handle, id), vec!["mb-work".to_string()]);
    }

    // Untouched ids stay untouched: a bulk patch is not a broadcast.
    assert_eq!(mailbox_ids(&handle, "msg-spam"), vec!["mb-spam".to_string()]);

    // One history record PER MESSAGE, each carrying both sides of the
    // patch. A consumer resyncing after a bulk move reads exactly this.
    let (_, history) =
        get_json_with(app, "/gmail/v1/users/me/history?startHistoryId=1").await;
    let mut added = history_label_entries(&history, "labelsAdded");
    added.sort();
    assert_eq!(
        added,
        vec![
            ("msg-1".to_string(), vec!["Label_mb-work".to_string()]),
            ("msg-2".to_string(), vec!["Label_mb-work".to_string()]),
        ],
        "history: {history}"
    );
    let mut removed = history_label_entries(&history, "labelsRemoved");
    removed.sort();
    assert_eq!(
        removed,
        vec![
            ("msg-1".to_string(), vec!["INBOX".to_string()]),
            ("msg-2".to_string(), vec!["INBOX".to_string()]),
        ],
        "history: {history}"
    );
}

/// The exclusive-container case. Gmail displays a message carrying
/// SPAM or TRASH in that container whatever else it also carries, so a
/// bulk move INTO the inbox is only correct if the same request strips
/// them. Both must come off in one patch alongside the INBOX add.
#[tokio::test]
async fn gmail_batch_modify_move_to_inbox_strips_spam_and_trash() {
    let handle = bulk_fixture();
    let app = bulk_router(&handle);

    let (status, _) = post_json(
        app.clone(),
        "/gmail/v1/users/me/messages/batchModify",
        serde_json::json!({
            "ids": ["msg-spam", "msg-trash"],
            "addLabelIds": ["INBOX"],
            "removeLabelIds": ["SPAM", "TRASH"],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    for id in ["msg-spam", "msg-trash"] {
        let labels = message_labels(app.clone(), id).await;
        assert!(labels.contains(&"INBOX".to_string()), "{id}: {labels:?}");
        assert!(!labels.contains(&"SPAM".to_string()), "{id}: {labels:?}");
        assert!(!labels.contains(&"TRASH".to_string()), "{id}: {labels:?}");
        // Exactly the inbox, not the inbox alongside the container it
        // was supposed to leave.
        assert_eq!(mailbox_ids(&handle, id), vec!["mb-inbox".to_string()]);
    }
}

/// The message-scoped modify is the same patch semantics as the bulk
/// one, and returns the updated Message. bifrost's single-target path
/// drives this; a divergence between the two would let a gate pass on
/// one shape and ship the other.
#[tokio::test]
async fn gmail_message_modify_honours_combined_add_and_remove() {
    let handle = bulk_fixture();
    let app = bulk_router(&handle);

    let (status, v) = post_json(
        app.clone(),
        "/gmail/v1/users/me/messages/msg-spam/modify",
        serde_json::json!({
            "addLabelIds": ["INBOX"],
            "removeLabelIds": ["SPAM", "TRASH"],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let labels: Vec<&str> = v["labelIds"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    // The RESPONSE already reflects the whole patch - a consumer that
    // trusts the returned Message must not need a follow-up read.
    assert!(labels.contains(&"INBOX"), "{labels:?}");
    assert!(!labels.contains(&"SPAM"), "{labels:?}");
    assert_eq!(mailbox_ids(&handle, "msg-spam"), vec!["mb-inbox".to_string()]);
}

/// Adds are applied before removes. Observable here because the add is
/// a no-op (the message already carries the destination) while the
/// remove is not: the message must end up detached from the inbox and
/// still carrying the label, not stripped of both.
#[tokio::test]
async fn gmail_modify_add_then_remove_ordering_leaves_the_destination_on() {
    let handle = bulk_fixture();
    let app = bulk_router(&handle);

    let (status, _) = post_json(
        app.clone(),
        "/gmail/v1/users/me/messages/batchModify",
        serde_json::json!({
            "ids": ["msg-3"],
            "addLabelIds": ["Label_mb-work"],
            "removeLabelIds": ["INBOX"],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(mailbox_ids(&handle, "msg-3"), vec!["mb-work".to_string()]);
    let labels = message_labels(app, "msg-3").await;
    assert!(labels.contains(&"Label_mb-work".to_string()), "{labels:?}");
    assert!(!labels.contains(&"INBOX".to_string()), "{labels:?}");
}

// ── Archive: a message with zero containers ─────────────────────────
//
// `removeLabelIds: ["INBOX"]` with nothing added is an ARCHIVE. Real
// Gmail answers 200 and the message keeps existing with no container
// label at all - it lives in All Mail. The fixture LOADER rejects an
// email with empty `mailbox_ids`, and that rule stays: a mailbox-less
// email is not a representable fixture state on any of the other
// protocols this same fixture feeds. The mutation is the side that
// adapts, landing the message in the `role = "archive"` mailbox, which
// `Fixture::gmail_label_ids` already projects to no Gmail label. Both
// sides then agree, and the wire shape matches real Gmail exactly.

#[tokio::test]
async fn gmail_archive_lands_the_message_in_all_mail() {
    let handle = bulk_fixture();
    let app = bulk_router(&handle);

    let (status, v) = post_json(
        app.clone(),
        "/gmail/v1/users/me/messages/msg-1/modify",
        serde_json::json!({ "addLabelIds": [], "removeLabelIds": ["INBOX"] }),
    )
    .await;
    // Not a 4xx: archiving is an ordinary Gmail operation.
    assert_eq!(status, StatusCode::OK, "{v}");
    let labels: Vec<&str> = v["labelIds"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(!labels.contains(&"INBOX"), "{labels:?}");
    // No container label appears in its place. All Mail is not a
    // label, and inventing one (say "ARCHIVE") would be a shape real
    // Gmail never serves.
    for container in ["SPAM", "TRASH", "SENT", "ARCHIVE", "ALL_MAIL"] {
        assert!(!labels.contains(&container), "{container} in {labels:?}");
    }

    // The canonical state is the archive mailbox, never empty - which
    // is what keeps the served fixture loadable by its own loader.
    assert_eq!(mailbox_ids(&handle, "msg-1"), vec!["mb-archive".to_string()]);

    // Still readable, still listed: archive is not a delete.
    let (status, _) = get_json_with(app.clone(), "/gmail/v1/users/me/messages/msg-1").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn gmail_batch_modify_archive_lands_every_message_in_all_mail() {
    let handle = bulk_fixture();
    let app = bulk_router(&handle);

    let (status, _) = post_json(
        app.clone(),
        "/gmail/v1/users/me/messages/batchModify",
        serde_json::json!({
            "ids": ["msg-1", "msg-2"],
            "addLabelIds": [],
            "removeLabelIds": ["INBOX"],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    for id in ["msg-1", "msg-2"] {
        assert_eq!(mailbox_ids(&handle, id), vec!["mb-archive".to_string()]);
        assert!(!message_labels(app.clone(), id).await.contains(&"INBOX".to_string()));
    }
}

/// A fixture with no `role = "archive"` mailbox cannot express All
/// Mail, so an archive there is refused rather than silently producing
/// a message the fixture loader would reject. Loud beats a 2xx that
/// leaves the fixture in a state it could not have been authored in.
#[tokio::test]
async fn gmail_archive_without_an_archive_mailbox_is_refused_and_changes_nothing() {
    let scenario = r#"
        fixture({ name = "no-archive", state = "na-0" })
        account({ id = "account-1", name = "test@example.com", primary = true })
        mailbox({ id = "mb-inbox", name = "Inbox", role = "inbox", sort_order = 0 })
        email({
            id = "only",
            thread_id = "t",
            mailbox_ids = { "mb-inbox" },
            keywords = { "$seen" },
            received_at = "2026-04-01T09:00:00Z",
            from = "a@example.com",
            to = { "test@example.com" },
            subject = "Only",
            body_text = "Body.",
            message_id = { "<only@example.com>" },
        })
    "#;
    let handle = saehrimnir::shared::handle(lua::load_source(scenario, "@no-archive").unwrap());
    let app = bulk_router(&handle);

    let (status, v) = post_json(
        app.clone(),
        "/gmail/v1/users/me/messages/only/modify",
        serde_json::json!({ "removeLabelIds": ["INBOX"] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"]["errors"][0]["reason"], "invalidArgument");
    // The message names the fixture-side fix rather than blaming the
    // client, because the client did nothing wrong.
    let message = v["error"]["message"].as_str().unwrap();
    assert!(message.contains("archive"), "{message}");

    // Rolled back whole: membership intact, and no state advance (an
    // empty MutationDiff records no transition), so a consumer polling
    // history does not see a phantom change.
    assert_eq!(mailbox_ids(&handle, "only"), vec!["mb-inbox".to_string()]);
    let (_, history) =
        get_json_with(app, "/gmail/v1/users/me/history?startHistoryId=1").await;
    assert!(
        history["history"]
            .as_array()
            .is_none_or(std::vec::Vec::is_empty),
        "refused patch left a history record: {history}"
    );
}

/// The mirror-image gap: an `addLabelIds` naming a system container
/// the fixture declares no mailbox for. Swallowing it silently would
/// turn a consumer's move-to-trash into an archive - a 2xx, a passing
/// gate, and the wrong state on the server.
#[tokio::test]
async fn gmail_add_of_an_undeclared_system_container_is_refused() {
    let scenario = r#"
        fixture({ name = "no-trash", state = "nt-0" })
        account({ id = "account-1", name = "test@example.com", primary = true })
        mailbox({ id = "mb-inbox", name = "Inbox", role = "inbox", sort_order = 0 })
        mailbox({ id = "mb-archive", name = "Archive", role = "archive", sort_order = 1 })
        email({
            id = "only",
            thread_id = "t",
            mailbox_ids = { "mb-inbox" },
            keywords = { "$seen" },
            received_at = "2026-04-01T09:00:00Z",
            from = "a@example.com",
            to = { "test@example.com" },
            subject = "Only",
            body_text = "Body.",
            message_id = { "<only@example.com>" },
        })
    "#;
    let handle = saehrimnir::shared::handle(lua::load_source(scenario, "@no-trash").unwrap());
    let app = bulk_router(&handle);

    let (status, v) = post_json(
        app.clone(),
        "/gmail/v1/users/me/messages/batchModify",
        serde_json::json!({
            "ids": ["only"],
            "addLabelIds": ["TRASH"],
            "removeLabelIds": ["INBOX"],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"]["errors"][0]["reason"], "invalidArgument");
    assert!(v["error"]["message"].as_str().unwrap().contains("TRASH"));
    // Refused before anything moved: no archive fallback, no detach.
    assert_eq!(mailbox_ids(&handle, "only"), vec!["mb-inbox".to_string()]);
}

/// `batchDelete` had no coverage either. It is the other bulk verb
/// bifrost drives, and its 204-with-no-body contract is the same one
/// `batchModify` relies on.
#[tokio::test]
async fn gmail_batch_delete_destroys_the_listed_ids_only() {
    let handle = bulk_fixture();
    let app = bulk_router(&handle);

    let (status, body) = post_json(
        app.clone(),
        "/gmail/v1/users/me/messages/batchDelete",
        serde_json::json!({ "ids": ["msg-1", "msg-trash"] }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(body, Value::Null);

    for id in ["msg-1", "msg-trash"] {
        let (status, _) = get_json_with(app.clone(), &format!("/gmail/v1/users/me/messages/{id}"))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{id} survived batchDelete");
    }
    let (status, _) = get_json_with(app.clone(), "/gmail/v1/users/me/messages/msg-2").await;
    assert_eq!(status, StatusCode::OK, "batchDelete swept up an unlisted id");

    let (_, history) = get_json_with(app, "/gmail/v1/users/me/history?startHistoryId=1").await;
    let mut deleted: Vec<String> = history["history"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r.get("messagesDeleted").and_then(Value::as_array))
        .flatten()
        .map(|e| e["message"]["id"].as_str().unwrap().to_string())
        .collect();
    deleted.sort();
    assert_eq!(deleted, vec!["msg-1".to_string(), "msg-trash".to_string()]);
}
