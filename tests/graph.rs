#![allow(clippy::unwrap_used)]

//! End-to-end Microsoft Graph mail-sync tests against the canonical
//! fixture, driven via `tower::ServiceExt::oneshot` (no socket bind).

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use saehrimnir::{fixture, graph, lua};

fn router() -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/jmap-small.toml")).unwrap();
    graph::router(graph::AppState::for_test(saehrimnir::shared::handle(fix)))
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

async fn get_raw(router: axum::Router, uri: &str) -> (StatusCode, Vec<u8>, axum::http::HeaderMap) {
    let resp = router
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
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec(), headers)
}

fn attach_router() -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/jmap-attach.toml")).unwrap();
    graph::router(graph::AppState::for_test(saehrimnir::shared::handle(fix)))
}

/// Send a request with an optional JSON body and return (status, body).
/// Clones the router, so the same `app` can be reused across calls.
async fn send_json(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "127.0.0.1:9999")
        .header(header::AUTHORIZATION, "Bearer doesnt-matter");
    let body = match body {
        Some(b) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&b).unwrap())
        }
        None => Body::empty(),
    };
    let resp = app
        .clone()
        .oneshot(builder.body(body).unwrap())
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

#[tokio::test]
async fn graph_create_draft_then_send() {
    let app = router();

    // Create a draft (no Drafts-role mailbox in jmap-small, so it
    // falls back to the first mailbox).
    let (status, v) = send_json(
        &app,
        "POST",
        "/v1.0/me/messages",
        Some(json!({
            "subject": "Hi",
            "toRecipients": [{ "emailAddress": { "address": "bob@example.com" } }],
            "body": { "contentType": "text", "content": "Hello there" },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(v["subject"], "Hi");
    let id = v["id"].as_str().unwrap().to_string();

    // Stored as a draft: a follow-up GET finds it.
    let (status, v) = get_json_with(app.clone(), &format!("/v1.0/me/messages/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["isDraft"], true);
    assert_eq!(
        v["toRecipients"][0]["emailAddress"]["address"],
        "bob@example.com"
    );

    // Send it (bifrost POSTs an empty body); unknown id 404s.
    let (status, _) = send_json(&app, "POST", &format!("/v1.0/me/messages/{id}/send"), None).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, _) = send_json(&app, "POST", "/v1.0/me/messages/ghost/send", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn graph_raw_mime_draft_create_then_send() {
    // bifrost's Graph `send_raw_message` (the raw-MIME import path
    // ratatoskr's MDN read-receipt dispatch uses) POSTs the base64 of the
    // RFC822 octets to `/me/messages` with `Content-Type: text/plain`,
    // creating a draft it then sends. The mock must accept that body
    // shape (the `Json` extractor rejected it) and project the parsed
    // MIME into the draft, addressed per the MIME `To:`.
    let app = router();
    let raw = "From: test@example.com\r\n\
               To: alice@example.com\r\n\
               Subject: Read: your message\r\n\
               Message-ID: <mdn-1@example.com>\r\n\
               Content-Type: text/plain\r\n\r\n\
               Your message was displayed.\r\n";
    // The server's base64 decoder accepts both the standard alphabet
    // (what bifrost emits) and base64url, so encoding either way drives
    // the same code path.
    let b64 = saehrimnir::gmail::mail::base64url_no_pad(raw.as_bytes());
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1.0/me/messages")
                .header(header::HOST, "127.0.0.1:9999")
                .header(header::AUTHORIZATION, "Bearer doesnt-matter")
                .header(header::CONTENT_TYPE, "text/plain")
                .body(Body::from(b64))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["subject"], "Read: your message");
    assert_eq!(
        v["toRecipients"][0]["emailAddress"]["address"],
        "alice@example.com"
    );
    let id = v["id"].as_str().unwrap().to_string();

    // Stored as a draft: a follow-up GET finds it.
    let (status, got) = get_json_with(app.clone(), &format!("/v1.0/me/messages/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["isDraft"], true);

    // Send it (bifrost POSTs an empty body to `.../send`). jmap-small has
    // no Sent-role mailbox, so the send acknowledges 202 without a Sent
    // transition; the ratatoskr-side MDN gate exercises the Sent filing
    // against a fixture that declares one.
    let (status, _) = send_json(&app, "POST", &format!("/v1.0/me/messages/{id}/send"), None).await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn graph_settings_stubs() {
    let app = router();

    // mailboxSettings: GET reports a disabled auto-reply; PATCH echoes.
    let (status, v) = send_json(
        &app,
        "GET",
        "/v1.0/me/mailboxSettings?$select=automaticRepliesSetting",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["automaticRepliesSetting"]["status"], "disabled");

    let (status, v) = send_json(
        &app,
        "PATCH",
        "/v1.0/me/mailboxSettings",
        Some(json!({ "automaticRepliesSetting": { "status": "alwaysEnabled" } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["automaticRepliesSetting"]["status"], "alwaysEnabled");

    // messageRules: empty list, create echoes an id.
    let (status, v) = send_json(&app, "GET", "/v1.0/me/mailFolders/inbox/messageRules", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(v["value"].as_array().unwrap().is_empty());
    let (status, v) = send_json(
        &app,
        "POST",
        "/v1.0/me/mailFolders/inbox/messageRules",
        Some(json!({ "displayName": "Rule" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(v["id"].is_string());

    // subscriptions: create echoes the expiration; delete is 204.
    let (status, v) = send_json(
        &app,
        "POST",
        "/v1.0/subscriptions",
        Some(json!({ "resource": "/me/messages", "expirationDateTime": "2026-07-01T00:00:00Z" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(v["id"].is_string());
    assert_eq!(v["expirationDateTime"], "2026-07-01T00:00:00Z");
    let (status, _) = send_json(
        &app,
        "DELETE",
        "/v1.0/subscriptions/mock-subscription-1",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn graph_mailfolder_crud_round_trip() {
    let app = router();

    // Create a top-level folder.
    let (status, v) = send_json(
        &app,
        "POST",
        "/v1.0/me/mailFolders",
        Some(json!({ "displayName": "Projects" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(v["displayName"], "Projects");
    let id = v["id"].as_str().unwrap().to_string();

    // Create a child under the inbox.
    let (status, v) = send_json(
        &app,
        "POST",
        "/v1.0/me/mailFolders/mbx-inbox/childFolders",
        Some(json!({ "displayName": "Sub" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(v["parentFolderId"], "mbx-inbox");

    // Rename, then move under the inbox.
    let (status, v) = send_json(
        &app,
        "PATCH",
        &format!("/v1.0/me/mailFolders/{id}"),
        Some(json!({ "displayName": "Work" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["displayName"], "Work");

    let (status, v) = send_json(
        &app,
        "POST",
        &format!("/v1.0/me/mailFolders/{id}/move"),
        Some(json!({ "destinationId": "mbx-inbox" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["parentFolderId"], "mbx-inbox");

    // Delete it; it disappears from the folder list.
    let (status, _) = send_json(&app, "DELETE", &format!("/v1.0/me/mailFolders/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, v) = get_json_with(app, "/v1.0/me/mailFolders").await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = v["value"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["id"].as_str())
        .collect();
    assert!(!ids.contains(&id.as_str()), "deleted folder still listed");
}

#[tokio::test]
async fn graph_get_single_message_projects_email() {
    // bifrost hydrates message metadata via $batch of GET
    // /me/messages/{id}; before this route existed it 404'd.
    let (status, v) = get_json("/v1.0/me/messages/email-001?$select=subject,from").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["id"], "email-001");
    assert_eq!(v["subject"], "Hello");
    assert_eq!(v["conversationId"], "email-001");
    assert_eq!(v["parentFolderId"], "mbx-inbox");
    assert_eq!(v["from"]["emailAddress"]["address"], "alice@example.com");

    let (status, v) = get_json("/v1.0/me/messages/no-such-message").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["code"], "ErrorItemNotFound");
}

#[tokio::test]
async fn graph_message_value_returns_assembled_rfc822() {
    // bifrost's open_raw_rfc822 defers real body bytes to $value.
    let (status, bytes, _headers) = get_raw(router(), "/v1.0/me/messages/email-001/$value").await;
    assert_eq!(status, StatusCode::OK);
    let body = String::from_utf8(bytes).unwrap();
    assert!(body.contains("Subject: Hello"), "got: {body}");
    assert!(body.contains("alice@example.com"), "from missing: {body}");
    assert!(body.contains("First message body."));

    let (status, _, _) = get_raw(router(), "/v1.0/me/messages/no-such-message/$value").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn graph_messages_collection_filters_by_conversation() {
    // No filter: all account messages, receivedAt descending.
    let (status, v) = get_json("/v1.0/me/messages").await;
    assert_eq!(status, StatusCode::OK);
    let vals = v["value"].as_array().unwrap();
    assert_eq!(vals.len(), 2);
    assert_eq!(vals[0]["id"], "email-002"); // 11:00, newer
    assert_eq!(vals[1]["id"], "email-001");

    // conversationId filter narrows to that thread (bifrost's
    // message_values_for_thread path).
    let (status, v) = get_json("/v1.0/me/messages?$filter=conversationId%20eq%20'email-002'").await;
    assert_eq!(status, StatusCode::OK);
    let vals = v["value"].as_array().unwrap();
    assert_eq!(vals.len(), 1);
    assert_eq!(vals[0]["id"], "email-002");
    assert_eq!(vals[0]["conversationId"], "email-002");

    // $top paginates with an @odata.nextLink.
    let (status, v) = get_json("/v1.0/me/messages?$top=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["value"].as_array().unwrap().len(), 1);
    assert!(v["@odata.nextLink"].is_string());
}

#[tokio::test]
async fn graph_patch_message_updates_flags() {
    let app = router();
    let body = json!({
        "isRead": true,
        "flag": { "flagStatus": "flagged" },
        "categories": ["Work"],
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/v1.0/me/messages/email-001")
                .header(header::HOST, "127.0.0.1:9999")
                .header(header::AUTHORIZATION, "Bearer doesnt-matter")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["isRead"], true);
    assert_eq!(v["flag"]["flagStatus"], "flagged");
    assert_eq!(v["categories"], json!(["Work"]));

    // Persisted: a follow-up GET on the same fixture reflects it.
    let (status, v) = get_json_with(app, "/v1.0/me/messages/email-001").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["isRead"], true);
    assert_eq!(v["categories"], json!(["Work"]));
}

#[tokio::test]
async fn graph_delete_message_removes_it() {
    let app = router();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/v1.0/me/messages/email-001")
                .header(header::HOST, "127.0.0.1:9999")
                .header(header::AUTHORIZATION, "Bearer doesnt-matter")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Gone afterwards.
    let (status, _) = get_json_with(app, "/v1.0/me/messages/email-001").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn graph_move_message_changes_parent_folder() {
    let app = router();
    let body = json!({ "destinationId": "mbx-archive" });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1.0/me/messages/email-001/move")
                .header(header::HOST, "127.0.0.1:9999")
                .header(header::AUTHORIZATION, "Bearer doesnt-matter")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["parentFolderId"], "mbx-archive");

    // Persisted: the message now lives in the archive folder.
    let (status, v) = get_json_with(app, "/v1.0/me/messages/email-001").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["parentFolderId"], "mbx-archive");
}

#[tokio::test]
async fn graph_batch_hydrates_messages() {
    // bifrost batches per-id GET /me/messages/{id} to hydrate metadata.
    let body = json!({
        "requests": [
            { "id": "1", "method": "GET", "url": "/me/messages/email-001?$select=subject" },
            { "id": "2", "method": "GET", "url": "/me/messages/email-002" },
            { "id": "3", "method": "GET", "url": "/me/messages/no-such-message" },
        ]
    });
    let resp = router()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1.0/$batch")
                .header(header::HOST, "127.0.0.1:9999")
                .header(header::AUTHORIZATION, "Bearer doesnt-matter")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    let responses = v["responses"].as_array().unwrap();
    assert_eq!(responses.len(), 3);
    let by_id = |id: &str| responses.iter().find(|r| r["id"] == id).unwrap();

    let r1 = by_id("1");
    assert_eq!(r1["status"], 200);
    assert_eq!(r1["body"]["id"], "email-001");
    assert_eq!(r1["body"]["subject"], "Hello");

    let r2 = by_id("2");
    assert_eq!(r2["status"], 200);
    assert_eq!(r2["body"]["id"], "email-002");

    // Unknown id fails per-item, not at the batch level.
    let r3 = by_id("3");
    assert_eq!(r3["status"], 404);
    assert_eq!(r3["body"]["error"]["code"], "ErrorItemNotFound");
}

#[tokio::test]
async fn graph_batch_routes_writes() {
    // bifrost routes its message writes (mark-read, delete, move)
    // through $batch, not the direct endpoints.
    let app = router();
    let body = json!({
        "requests": [
            { "id": "1", "method": "PATCH", "url": "/me/messages/email-001", "body": { "isRead": true } },
            { "id": "2", "method": "DELETE", "url": "/me/messages/email-002" },
        ]
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1.0/$batch")
                .header(header::HOST, "127.0.0.1:9999")
                .header(header::AUTHORIZATION, "Bearer doesnt-matter")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    let responses = v["responses"].as_array().unwrap();
    let by_id = |id: &str| responses.iter().find(|r| r["id"] == id).unwrap();
    assert_eq!(by_id("1")["status"], 200);
    assert_eq!(by_id("1")["body"]["isRead"], true);
    assert_eq!(by_id("2")["status"], 204);

    // Persisted: email-001 is now read, email-002 is gone.
    let (status, v) = get_json_with(app.clone(), "/v1.0/me/messages/email-001").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["isRead"], true);
    let (status, _) = get_json_with(app, "/v1.0/me/messages/email-002").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn graph_me_profile_returns_account_identity() {
    // GraphAccountFactory::open's FIRST request. Without this route it
    // hit the catchall 404 and no Graph account could open.
    let (status, v) = get_json("/v1.0/me?$select=displayName,mail,userPrincipalName").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["id"], "account-1");
    assert_eq!(v["mail"], "test@example.com");
    assert_eq!(v["userPrincipalName"], "test@example.com");
    assert_eq!(v["displayName"], "test@example.com");
}

#[tokio::test]
async fn graph_users_profile_resolves_named_and_404s_unknown() {
    // `me` alias resolves to the primary account.
    let (status, v) = get_json("/v1.0/users/me").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["id"], "account-1");

    // A named declared account resolves directly.
    let (status, v) = get_json("/v1.0/users/account-1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["mail"], "test@example.com");

    // An unknown user 404s with the Graph error envelope.
    let (status, v) = get_json("/v1.0/users/nobody").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["code"], "ResourceNotFound");
}

#[tokio::test]
async fn graph_directory_search_matches_accounts() {
    // startswith 'test' matches account-1 (test@example.com).
    let (status, v) = get_json(
        "/v1.0/me/users?$filter=startswith(displayName,'test')%20or%20startswith(mail,'test')",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let mails: Vec<&str> = v["value"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|u| u["mail"].as_str())
        .collect();
    assert!(mails.contains(&"test@example.com"), "got {mails:?}");

    // A non-matching prefix yields an empty directory.
    let (status, v) =
        get_json("/v1.0/users?$filter=startswith(displayName,'zzz')%20or%20startswith(mail,'zzz')")
            .await;
    assert_eq!(status, StatusCode::OK);
    assert!(v["value"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn graph_list_message_attachments_returns_metadata_with_bytes() {
    let (status, v) =
        get_json_with(attach_router(), "/v1.0/me/messages/email-001/attachments").await;
    assert_eq!(status, StatusCode::OK);
    let arr = v["value"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    let a = &arr[0];
    assert_eq!(a["id"], "blob-att-001");
    assert_eq!(a["name"], "sample.txt");
    assert_eq!(a["contentType"], "text/plain");
    assert_eq!(a["@odata.type"], "#microsoft.graph.fileAttachment");
    assert!(
        a["contentBytes"].as_str().unwrap().ends_with("=")
            || !a["contentBytes"].as_str().unwrap().is_empty()
    );
}

#[tokio::test]
async fn graph_get_message_attachment_value_streams_bytes() {
    let (status, body, headers) = get_raw(
        attach_router(),
        "/v1.0/me/messages/email-001/attachments/blob-att-001/$value",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "text/plain");
    assert!(body.starts_with(b"attachment payload"));
}

#[tokio::test]
async fn graph_messages_with_expand_attachments_embeds_them() {
    let (status, v) = get_json_with(
        attach_router(),
        "/v1.0/me/mailFolders/mbx-inbox/messages?$expand=attachments",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let msg = &v["value"][0];
    let atts = msg["attachments"].as_array().unwrap();
    assert_eq!(atts.len(), 1);
    assert_eq!(atts[0]["id"], "blob-att-001");
}

#[tokio::test]
async fn graph_messages_without_expand_omits_attachment_data() {
    let (status, v) =
        get_json_with(attach_router(), "/v1.0/me/mailFolders/mbx-inbox/messages").await;
    assert_eq!(status, StatusCode::OK);
    let msg = &v["value"][0];
    assert_eq!(msg["hasAttachments"], true);
    assert_eq!(msg["attachments"].as_array().unwrap().len(), 0);
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
    assert!(m["receivedDateTime"].as_str().unwrap().ends_with("Z"));
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
            .any(|h| h["name"] == "In-Reply-To" && h["value"] == "<email-001@example.com>")
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
    let (_status, v) = get_json("/v1.0/me/mailFolders/inbox/messages?$top=1").await;
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
    let (_status, v) = get_json("/v1.0/me/mailFolders/inbox/messages/delta").await;
    assert_eq!(v["value"].as_array().unwrap().len(), 2);
    let delta = v["@odata.deltaLink"].as_str().unwrap();
    assert!(delta.contains("$deltatoken="));
    assert!(v.get("@odata.nextLink").is_none());

    // Following the deltaLink (subsequent cycle) returns empty +
    // same-shape deltaLink.
    let path = delta.trim_start_matches("http://127.0.0.1:9999");
    let (_status, v2) = get_json(path).await;
    assert!(v2["value"].as_array().unwrap().is_empty());
    assert!(
        v2["@odata.deltaLink"]
            .as_str()
            .unwrap()
            .contains("$deltatoken=")
    );
}

#[tokio::test]
async fn delta_with_deltatoken_latest_emits_empty_dump() {
    // The "$deltatoken=latest" shortcut: client uses this when
    // discovering a brand-new folder mid-cycle and just wants a
    // forward-only cursor, not a full message dump.
    let (_status, v) =
        get_json("/v1.0/me/mailFolders/inbox/messages/delta?$deltatoken=latest").await;
    assert!(v["value"].as_array().unwrap().is_empty());
    assert!(v["@odata.deltaLink"].is_string());
}

#[tokio::test]
async fn delta_paginates_when_top_smaller_than_total() {
    // $top=1: first page has email-002 + nextLink (no deltaLink yet).
    let (_status, v) = get_json("/v1.0/me/mailFolders/inbox/messages/delta?$top=1").await;
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
async fn malformed_skiptoken_returns_400_not_silent_restart() {
    // Regression: a $skiptoken we can't decode used to silently
    // fall back to offset 0, which would loop a client forever on
    // a stale or corrupted token. Now it surfaces as 400.
    let (status, v) = get_json("/v1.0/me/mailFolders/inbox/messages?$skiptoken=garbage").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"]["code"], "InvalidQueryParameter");
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

// ── Reactive-callback tests ────────────────────────────────────────

fn router_with_lua_scenario(scenario: &str) -> axum::Router {
    let (fixture, dispatcher) = lua::load_source_with_dispatcher(scenario, "@cb").unwrap();
    graph::router(
        graph::AppState::for_test(saehrimnir::shared::handle(fixture))
            .with_dispatcher(Arc::new(dispatcher)),
    )
}

async fn get_json_via(router: axum::Router, uri: &str) -> (StatusCode, Value) {
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header(header::HOST, "127.0.0.1:9999")
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
async fn list_messages_callback_overrides_with_400() {
    let scenario = r#"
        fixture({ name = "cb" })
        account({ id = "account-1", name = "test@example.com" })
        mailbox({ id = "mbx-inbox", name = "Inbox", role = "inbox" })
        on("graph", "list_messages", function(req)
            return { status = "ServerError", message = "synthetic " .. req.folder }
        end)
    "#;
    let router = router_with_lua_scenario(scenario);
    let (status, v) = get_json_via(router, "/v1.0/me/mailFolders/inbox/messages").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"]["code"], "ServerError");
    assert_eq!(v["error"]["message"], "synthetic inbox");
}

#[tokio::test]
async fn delta_messages_callback_call_index_increments() {
    let scenario = r#"
        fixture({ name = "cb" })
        account({ id = "account-1", name = "test@example.com" })
        mailbox({ id = "mbx-inbox", name = "Inbox", role = "inbox" })
        on("graph", "delta_messages", function(req)
            if req.call_index == 2 then
                return { status = "Throttled", message = "second call denied" }
            end
        end)
    "#;
    let router = router_with_lua_scenario(scenario);
    // First call passes through (default delta dump).
    let (s1, v1) = get_json_via(router.clone(), "/v1.0/me/mailFolders/inbox/messages/delta").await;
    assert_eq!(s1, StatusCode::OK);
    assert!(v1.get("@odata.deltaLink").is_some());
    // Second call gets the override.
    let (s2, v2) = get_json_via(router, "/v1.0/me/mailFolders/inbox/messages/delta").await;
    assert_eq!(s2, StatusCode::BAD_REQUEST);
    assert_eq!(v2["error"]["code"], "Throttled");
}

/// HTTP middleware records `(protocol="graph", command="GET <path>",
/// detail.query)` per request. Verifies path is captured without
/// the query string and that the query lands in `detail`.
#[tokio::test]
async fn graph_middleware_records_request_log_entries() {
    use saehrimnir::request_log::RequestLog;

    let request_log = RequestLog::default();
    let fix = fixture::load(std::path::Path::new("fixtures/jmap-small.toml")).unwrap();
    let app = graph::router(
        graph::AppState::for_test(saehrimnir::shared::handle(fix))
            .with_request_log(request_log.clone()),
    );

    let _ = get_json_with(app.clone(), "/v1.0/me/mailFolders").await;
    let _ = get_json_with(app, "/v1.0/me/mailFolders/inbox/messages?$top=10").await;

    let snap = request_log.snapshot();
    assert_eq!(snap.len(), 2);
    assert_eq!(snap[0].protocol, "graph");
    assert_eq!(snap[0].command, "GET /v1.0/me/mailFolders");
    assert!(snap[0].detail["query"].is_null());
    assert_eq!(snap[1].command, "GET /v1.0/me/mailFolders/inbox/messages");
    assert_eq!(snap[1].detail["query"], "$top=10");
}

// ── Calendar surface (Microsoft Graph) ──────────────────────────────

fn calendar_router() -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/graph-calendar-small.toml")).unwrap();
    graph::router(graph::AppState::for_test(saehrimnir::shared::handle(fix)))
}

/// Router over the RSVP fixture (`calendar-rsvp-small.toml`), whose
/// account address (`self@example.test`) is itself an attendee of
/// `ev-rsvp`. Optionally wires a shared request log.
fn rsvp_router_with_log(log: Option<saehrimnir::request_log::RequestLog>) -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/calendar-rsvp-small.toml")).unwrap();
    let state = graph::AppState::for_test(saehrimnir::shared::handle(fix));
    let state = match log {
        Some(l) => state.with_request_log(l),
        None => state,
    };
    graph::router(state)
}

async fn json_request(
    router: axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req_builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "127.0.0.1:9999");
    let body = match body {
        Some(v) => {
            req_builder = req_builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = router
        .oneshot(req_builder.body(body).unwrap())
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

#[tokio::test]
async fn graph_list_calendars_projects_fixture_in_declaration_order() {
    let (status, v) = get_json_with(calendar_router(), "/v1.0/me/calendars").await;
    assert_eq!(status, StatusCode::OK);
    let arr = v["value"].as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["id"], "cal-work");
    assert_eq!(arr[0]["isDefaultCalendar"], true);
    assert_eq!(arr[0]["name"], "Work");
    assert_eq!(arr[0]["color"], "lightBlue");
    assert_eq!(arr[1]["id"], "cal-personal");
    assert_eq!(arr[1]["isDefaultCalendar"], false);
}

#[tokio::test]
async fn graph_get_calendar_resolves_default_alias_to_first_default() {
    let (status, v) = get_json_with(calendar_router(), "/v1.0/me/calendars/default").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["id"], "cal-work");
}

#[tokio::test]
async fn graph_get_calendar_404s_unknown_id() {
    let (status, v) = get_json_with(calendar_router(), "/v1.0/me/calendars/ghost").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["code"], "ResourceNotFound");
}

#[tokio::test]
async fn graph_list_events_filters_by_calendar_and_paginates() {
    let (status, v) = get_json_with(calendar_router(), "/v1.0/me/calendars/cal-work/events").await;
    assert_eq!(status, StatusCode::OK);
    let arr = v["value"].as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["id"], "ev-001");
    assert_eq!(arr[0]["subject"], "Standup");
    assert_eq!(arr[0]["start"]["dateTime"], "2026-01-15T09:00:00Z");
    assert_eq!(arr[0]["start"]["timeZone"], "UTC");
    assert_eq!(
        arr[0]["organizer"]["emailAddress"]["address"],
        "alice@example.com"
    );
    let attendees = arr[0]["attendees"].as_array().unwrap();
    assert_eq!(attendees.len(), 2);
    assert_eq!(attendees[1]["emailAddress"]["address"], "carol@example.com");

    // Paginate one at a time.
    let (status, v) = get_json_with(
        calendar_router(),
        "/v1.0/me/calendars/cal-work/events?$top=1",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let arr = v["value"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    let next = v["@odata.nextLink"].as_str().unwrap();
    assert!(next.contains("$skiptoken=s.1"));

    let (_, v2) = get_json_with(
        calendar_router(),
        "/v1.0/me/calendars/cal-work/events?$top=1&$skiptoken=s.1",
    )
    .await;
    let arr2 = v2["value"].as_array().unwrap();
    assert_eq!(arr2.len(), 1);
    assert_eq!(arr2[0]["id"], "ev-002");
    assert!(v2.get("@odata.nextLink").is_none());
}

#[tokio::test]
async fn graph_list_events_for_empty_calendar_returns_empty_array() {
    let (status, v) =
        get_json_with(calendar_router(), "/v1.0/me/calendars/cal-personal/events").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["value"].as_array().unwrap().len(), 0);
    assert!(v.get("@odata.nextLink").is_none());
}

#[tokio::test]
async fn graph_calendar_view_delta_returns_full_then_empty() {
    let (status, v) = get_json_with(
        calendar_router(),
        "/v1.0/me/calendars/cal-work/calendarView/delta",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["value"].as_array().unwrap().len(), 2);
    assert!(
        v["@odata.deltaLink"]
            .as_str()
            .unwrap()
            .contains("$deltatoken=")
    );

    let (_, v2) = get_json_with(
        calendar_router(),
        "/v1.0/me/calendars/cal-work/calendarView/delta?$deltatoken=d.fixture-state",
    )
    .await;
    assert_eq!(v2["value"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn graph_get_event_projects_fixture() {
    let (status, v) = get_json_with(calendar_router(), "/v1.0/me/events/ev-001").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["id"], "ev-001");
    assert_eq!(v["subject"], "Standup");
}

#[tokio::test]
async fn graph_calendar_view_filters_by_range() {
    // No bounds: both cal-work events (the non-delta read bifrost's
    // events_in_range drives).
    let (status, v) = get_json_with(
        calendar_router(),
        "/v1.0/me/calendars/cal-work/calendarView",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["value"].as_array().unwrap().len(), 2);

    // A January window keeps only ev-001 (Jan 15); ev-002 is Feb 1.
    let (status, v) = get_json_with(
        calendar_router(),
        "/v1.0/me/calendars/cal-work/calendarView?startDateTime=2026-01-01T00:00:00Z&endDateTime=2026-01-31T00:00:00Z",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let arr = v["value"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "ev-001");

    // Unknown calendar 404s.
    let (status, _) =
        get_json_with(calendar_router(), "/v1.0/me/calendars/ghost/calendarView").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn graph_event_rsvp() {
    let app = calendar_router();
    // accept -> 202 Accepted. On this fixture the account address is
    // not among ev-001's attendees, so the RSVP is a no-op durably;
    // the status-code contract still holds.
    let (status, _) = send_json(
        &app,
        "POST",
        "/v1.0/me/events/ev-001/accept",
        Some(json!({ "sendResponse": false })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // unknown action -> 400, unknown event -> 404.
    let (status, _) = send_json(
        &app,
        "POST",
        "/v1.0/me/events/ev-001/frobnicate",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = send_json(
        &app,
        "POST",
        "/v1.0/me/events/ghost/accept",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Look up an attendee's `status.response` in a serialized Graph event
/// by address.
fn graph_attendee_response(event: &Value, email: &str) -> Option<String> {
    event["attendees"].as_array()?.iter().find_map(|a| {
        (a["emailAddress"]["address"] == email)
            .then(|| a["status"]["response"].as_str().map(str::to_string))
            .flatten()
    })
}

/// The calendar-scoped event routes bifrost's Graph client actually
/// addresses (`/calendars/{cal}/events/{id}` for GET / PATCH / DELETE)
/// exist alongside the mailbox-scoped ones. The calendar-scoped GET
/// serves the same body the update/delete pre-read (etag) expects.
#[tokio::test]
async fn graph_calendar_scoped_event_routes_get_patch_delete() {
    let app = calendar_router();

    // GET is calendar-scoped and returns the same event the
    // mailbox-scoped GET does.
    let (status, v) = get_json_with(app.clone(), "/v1.0/me/calendars/cal-work/events/ev-001").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["id"], "ev-001");
    assert_eq!(v["subject"], "Standup");

    // PATCH is calendar-scoped and mutates the event.
    let (status, v) = send_json(
        &app,
        "PATCH",
        "/v1.0/me/calendars/cal-work/events/ev-001",
        Some(json!({ "subject": "Standup (cal-scoped)" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["subject"], "Standup (cal-scoped)");

    // DELETE is calendar-scoped; a follow-up GET 404s.
    let (status, _) = send_json(
        &app,
        "DELETE",
        "/v1.0/me/calendars/cal-work/events/ev-001",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = get_json_with(app, "/v1.0/me/calendars/cal-work/events/ev-001").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Graph RSVP durably records the authenticated user's participation
/// status against its own attendee record, and the read projection
/// serves it back under `attendees[].status.response`. The action
/// touches only the self attendee - the other attendee's authored
/// status is untouched.
#[tokio::test]
async fn graph_rsvp_records_participation_status() {
    let log = saehrimnir::request_log::RequestLog::default();
    let app = rsvp_router_with_log(Some(log.clone()));

    // Baseline: self is needs-action (notResponded), other is declined.
    let (status, v) = get_json_with(app.clone(), "/v1.0/me/calendars/cal-1/events/ev-rsvp").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        graph_attendee_response(&v, "self@example.test").as_deref(),
        Some("notResponded")
    );
    assert_eq!(
        graph_attendee_response(&v, "other@example.test").as_deref(),
        Some("declined")
    );

    // Mailbox-scoped accept flips self to accepted, leaves other.
    let (status, _) = send_json(
        &app,
        "POST",
        "/v1.0/me/events/ev-rsvp/accept",
        Some(json!({ "sendResponse": false })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (_, v) = get_json_with(app.clone(), "/v1.0/me/calendars/cal-1/events/ev-rsvp").await;
    assert_eq!(
        graph_attendee_response(&v, "self@example.test").as_deref(),
        Some("accepted")
    );
    assert_eq!(
        graph_attendee_response(&v, "other@example.test").as_deref(),
        Some("declined")
    );

    // Calendar-scoped tentativelyAccept moves self again.
    let (status, _) = send_json(
        &app,
        "POST",
        "/v1.0/me/calendars/cal-1/events/ev-rsvp/tentativelyAccept",
        Some(json!({ "sendResponse": false })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (_, v) = get_json_with(app, "/v1.0/me/calendars/cal-1/events/ev-rsvp").await;
    assert_eq!(
        graph_attendee_response(&v, "self@example.test").as_deref(),
        Some("tentativelyAccepted")
    );

    // The RSVP is recorded in the request log with the resolved status.
    let snap = log.snapshot();
    assert!(
        snap.iter()
            .any(|e| e.command == "POST /v1.0/me/events/ev-rsvp/accept"
                && e.detail["response"] == "accepted"),
        "rsvp accept row missing: {:?}",
        snap.iter().map(|e| &e.command).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn graph_create_event_echoes_body_and_logs_request() {
    let log = saehrimnir::request_log::RequestLog::default();
    let fix = fixture::load(std::path::Path::new("fixtures/graph-calendar-small.toml")).unwrap();
    let app = graph::router(
        graph::AppState::for_test(saehrimnir::shared::handle(fix)).with_request_log(log.clone()),
    );

    let body = serde_json::json!({
        "subject": "New meeting",
        "start": { "dateTime": "2026-03-01T10:00:00Z", "timeZone": "UTC" },
        "end":   { "dateTime": "2026-03-01T11:00:00Z", "timeZone": "UTC" },
    });
    let (status, v) = json_request(
        app,
        "POST",
        "/v1.0/me/calendars/cal-work/events",
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    // POST now mutates the fixture: response is the freshly created
    // event projected via `serialize_event`. Server id is
    // `mock-event-N` where N counts existing events at create time
    // (the small calendar fixture has two events, so the new event
    // is mock-event-3).
    assert_eq!(v["id"], "mock-event-3");
    assert_eq!(v["calendarId"], "cal-work");
    assert_eq!(v["subject"], "New meeting");
    assert_eq!(v["start"]["dateTime"], "2026-03-01T10:00:00Z");

    // The detail-bearing entry is the handler-emitted one (the
    // middleware also records a path-level entry without the body).
    let snap = log.snapshot();
    let mutation = snap
        .iter()
        .find(|e| {
            e.command == "POST /v1.0/me/calendars/cal-work/events" && !e.detail["body"].is_null()
        })
        .expect("mutation entry recorded");
    assert_eq!(mutation.detail["body"]["subject"], "New meeting");
}

#[tokio::test]
async fn graph_patch_event_echoes_body_and_logs_request() {
    let log = saehrimnir::request_log::RequestLog::default();
    let fix = fixture::load(std::path::Path::new("fixtures/graph-calendar-small.toml")).unwrap();
    let app = graph::router(
        graph::AppState::for_test(saehrimnir::shared::handle(fix)).with_request_log(log.clone()),
    );
    let body = serde_json::json!({ "subject": "Renamed" });
    let (status, v) = json_request(app, "PATCH", "/v1.0/me/events/ev-001", Some(body)).await;
    assert_eq!(status, StatusCode::OK);
    // PATCH now mutates the fixture: response is the post-patch
    // event projected via `serialize_event`.
    assert_eq!(v["id"], "ev-001");
    assert_eq!(v["calendarId"], "cal-work");
    assert_eq!(v["subject"], "Renamed");

    let snap = log.snapshot();
    let entry = snap
        .iter()
        .find(|e| e.command == "PATCH /v1.0/me/events/ev-001" && !e.detail["body"].is_null())
        .expect("patch entry recorded");
    assert_eq!(entry.detail["body"]["subject"], "Renamed");
}

#[tokio::test]
async fn graph_delete_event_returns_204_and_logs_request() {
    let log = saehrimnir::request_log::RequestLog::default();
    let fix = fixture::load(std::path::Path::new("fixtures/graph-calendar-small.toml")).unwrap();
    let app = graph::router(
        graph::AppState::for_test(saehrimnir::shared::handle(fix)).with_request_log(log.clone()),
    );

    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/v1.0/me/events/ev-001")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let snap = log.snapshot();
    assert!(
        snap.iter()
            .any(|e| e.command == "DELETE /v1.0/me/events/ev-001" && e.detail["id"] == "ev-001")
    );
}

#[tokio::test]
async fn graph_patch_event_404s_unknown_id() {
    let log = saehrimnir::request_log::RequestLog::default();
    let fix = fixture::load(std::path::Path::new("fixtures/graph-calendar-small.toml")).unwrap();
    let app = graph::router(
        graph::AppState::for_test(saehrimnir::shared::handle(fix)).with_request_log(log.clone()),
    );
    let body = serde_json::json!({ "subject": "Renamed" });
    let (status, v) = json_request(app, "PATCH", "/v1.0/me/events/ev-missing", Some(body)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["code"], "ResourceNotFound");
    // The middleware still records the (method, path) envelope,
    // but the 404 short-circuits before the body is parsed and
    // attached, so no `body` detail leaks into the log.
    let snap = log.snapshot();
    assert!(!snap.iter().any(|e| !e.detail["body"].is_null()));
}

#[tokio::test]
async fn graph_delete_event_404s_unknown_id() {
    let log = saehrimnir::request_log::RequestLog::default();
    let fix = fixture::load(std::path::Path::new("fixtures/graph-calendar-small.toml")).unwrap();
    let app = graph::router(
        graph::AppState::for_test(saehrimnir::shared::handle(fix)).with_request_log(log.clone()),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/v1.0/me/events/ev-missing")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"]["code"], "ResourceNotFound");
    let snap = log.snapshot();
    assert!(
        !snap
            .iter()
            .any(|e| e.command.starts_with("DELETE /v1.0/me/events/") && !e.detail["id"].is_null())
    );
}

// ── Calendar mutation round-trip through events/delta ──────────────

/// Regression: a tombstone for an event destroyed in calendar A
/// must NOT surface in calendar B's `calendarView/delta` walk. The
/// destroyed-id walk used to ignore parent_id; folder/calendar
/// scoping is now enforced via `event_destroyed_parents` on each
/// transition.
#[tokio::test]
async fn graph_calendar_view_delta_does_not_leak_tombstones_across_calendars() {
    let app = calendar_router();

    // Bootstrap cal-personal (empty) and capture its deltaLink.
    let (_, v) = get_json_with(
        app.clone(),
        "/v1.0/me/calendars/cal-personal/calendarView/delta",
    )
    .await;
    assert_eq!(v["value"].as_array().unwrap().len(), 0);
    let cal_personal_link = v["@odata.deltaLink"].as_str().unwrap().to_string();

    // Delete an event that lives in cal-work.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/v1.0/me/events/ev-001")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // cal-personal's follow-up delta must be empty: the destroy
    // happened in a sibling calendar.
    let path = cal_personal_link
        .split_once("/v1.0/")
        .map(|(_, s)| format!("/v1.0/{s}"))
        .expect("deltaLink starts with /v1.0/");
    let (status, v) = get_json_with(app.clone(), &path).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        v["value"].as_array().unwrap().len(),
        0,
        "cal-personal saw cross-calendar tombstone: {v:?}"
    );

    // Sanity: cal-work's delta DOES see the tombstone.
    let (_, v) = get_json_with(
        app,
        "/v1.0/me/calendars/cal-work/calendarView/delta?$deltatoken=d.fixture-state",
    )
    .await;
    let value = v["value"].as_array().unwrap();
    assert!(
        value
            .iter()
            .any(|e| e["id"] == "ev-001" && e["@removed"]["reason"] == "deleted"),
        "cal-work missed the tombstone it should see: {value:?}"
    );
}

/// Create + patch + delete events, then verify each shows up in the
/// next `calendarView/delta` cycle. Proves the mutation surface
/// actually persists into the change log and round-trips through
/// the Graph delta endpoint - closes the M6.10 audit gap on item 3.
#[tokio::test]
async fn graph_calendar_mutations_round_trip_through_delta() {
    // Pin one router so every request hits the same fixture handle.
    let app = calendar_router();

    // Initial bootstrap: capture the `@odata.deltaLink` so the
    // follow-up call can prove "since this state, here's what
    // changed".
    let (_, v) = get_json_with(
        app.clone(),
        "/v1.0/me/calendars/cal-work/calendarView/delta",
    )
    .await;
    let initial_delta_link = v["@odata.deltaLink"].as_str().unwrap().to_string();
    let initial_events = v["value"].as_array().unwrap().len();

    // 1. Create a new event.
    let create_body = serde_json::json!({
        "subject": "Round-trip event",
        "start": { "dateTime": "2026-04-01T09:00:00Z", "timeZone": "UTC" },
        "end":   { "dateTime": "2026-04-01T10:00:00Z", "timeZone": "UTC" },
    });
    let (status, v) = json_request(
        app.clone(),
        "POST",
        "/v1.0/me/calendars/cal-work/events",
        Some(create_body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let created_id = v["id"].as_str().unwrap().to_string();

    // 2. Patch one of the seeded events.
    let (status, _) = json_request(
        app.clone(),
        "PATCH",
        "/v1.0/me/events/ev-001",
        Some(serde_json::json!({ "subject": "Renamed in round-trip" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 3. Delete another seeded event.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/v1.0/me/events/ev-002")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // 4. Follow-up delta against the original deltaLink. Should
    // contain: the new event (full body), the patched event (full
    // body with new subject), and a tombstone for the deleted event.
    let path_with_token = initial_delta_link
        .split_once("/v1.0/")
        .map(|(_, s)| format!("/v1.0/{s}"))
        .expect("deltaLink starts with /v1.0/");
    let (status, v) = get_json_with(app.clone(), &path_with_token).await;
    assert_eq!(status, StatusCode::OK);
    let value = v["value"].as_array().unwrap();
    assert_eq!(
        value.len(),
        3,
        "expected created+updated+destroyed: {value:?}"
    );

    // The created event projects with its full body.
    let created_in_delta = value
        .iter()
        .find(|e| e["id"] == serde_json::Value::String(created_id.clone()))
        .expect("created event missing from delta");
    assert_eq!(created_in_delta["subject"], "Round-trip event");

    // The patched event projects with its updated subject.
    let patched_in_delta = value
        .iter()
        .find(|e| e["id"] == "ev-001")
        .expect("patched event missing from delta");
    assert_eq!(patched_in_delta["subject"], "Renamed in round-trip");

    // The deleted event appears as a Graph-style tombstone.
    let deleted_in_delta = value
        .iter()
        .find(|e| e["id"] == "ev-002")
        .expect("deleted event missing from delta");
    assert_eq!(deleted_in_delta["@removed"]["reason"], "deleted");

    // The fresh deltaLink in this response carries the post-mutation
    // state, so a second follow-up returns empty.
    let new_delta_link = v["@odata.deltaLink"].as_str().unwrap().to_string();
    let path_with_new_token = new_delta_link
        .split_once("/v1.0/")
        .map(|(_, s)| format!("/v1.0/{s}"))
        .expect("deltaLink starts with /v1.0/");
    let (_, v2) = get_json_with(app.clone(), &path_with_new_token).await;
    assert_eq!(v2["value"].as_array().unwrap().len(), 0);

    // Sanity: the initial bootstrap had two events; current cal-work
    // event count is 2 (one created + one survivor after delete).
    let (_, v3) = get_json_with(app, "/v1.0/me/calendars/cal-work/events").await;
    assert_eq!(v3["value"].as_array().unwrap().len(), 2);
    let _ = initial_events; // documenting the pre-state was 2 too.
}

// ── Contact sync ─────────────────────────────────────────────────────

fn router_contacts() -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/graph-contacts-small.toml")).unwrap();
    graph::router(graph::AppState::for_test(saehrimnir::shared::handle(fix)))
}

#[tokio::test]
async fn graph_contact_folders_list_emits_two_folders() {
    let (status, v) = get_json_with(router_contacts(), "/v1.0/me/contactFolders").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        v["@odata.context"],
        "https://graph.microsoft.com/v1.0/$metadata#me/contactFolders"
    );
    let folders = v["value"].as_array().unwrap();
    assert_eq!(folders.len(), 2);
    assert_eq!(folders[0]["id"], "cf-default");
    assert_eq!(folders[0]["displayName"], "Contacts");
    assert_eq!(folders[1]["id"], "cf-vendors");
    // No nextLink: the page fits in one shot.
    assert!(v.get("@odata.nextLink").is_none());
}

#[tokio::test]
async fn graph_contact_folder_by_id_returns_single_folder() {
    let (status, v) = get_json_with(router_contacts(), "/v1.0/me/contactFolders/cf-default").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["id"], "cf-default");
    assert_eq!(v["displayName"], "Contacts");
}

#[tokio::test]
async fn graph_contact_folder_default_alias_resolves_to_is_default_folder() {
    let (status, v) = get_json_with(router_contacts(), "/v1.0/me/contactFolders/default").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["id"], "cf-default");
}

#[tokio::test]
async fn graph_contact_folder_unknown_id_returns_404() {
    let (status, _) = get_json_with(router_contacts(), "/v1.0/me/contactFolders/cf-bogus").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn graph_contacts_in_folder_emit_full_projection() {
    let (status, v) = get_json_with(
        router_contacts(),
        "/v1.0/me/contactFolders/cf-default/contacts?$select=id,displayName,emailAddresses,parentFolderId&$top=999",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let contacts = v["value"].as_array().unwrap();
    // Three contacts in cf-default: contact-001, contact-002, contact-003.
    // contact-100 is in cf-vendors and must NOT appear here.
    assert_eq!(contacts.len(), 3);
    let ids: Vec<&str> = contacts.iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["contact-001", "contact-002", "contact-003"]);

    // Wire shape: id, parentFolderId, displayName, emailAddresses
    // (the array ratatoskr's GraphContact deserialises).
    let alice = &contacts[0];
    assert_eq!(alice["parentFolderId"], "cf-default");
    assert_eq!(alice["displayName"], "Alice Anderson");
    let emails = alice["emailAddresses"].as_array().unwrap();
    assert_eq!(emails.len(), 2);
    assert_eq!(emails[0]["address"], "alice@example.com");
    assert_eq!(emails[0]["name"], "Alice Anderson");
    // Bare-string sugar gets folded into {address} with no name.
    assert_eq!(emails[1]["address"], "alice.anderson@example.org");
    assert!(emails[1].get("name").is_none());

    // Empty-emails contact still serialises a (empty) emailAddresses
    // array; ratatoskr's extract_emails will skip it cleanly.
    let charlie = &contacts[2];
    assert!(charlie["emailAddresses"].as_array().unwrap().is_empty());
    assert!(charlie.get("displayName").is_none());
}

#[tokio::test]
async fn graph_contacts_in_unknown_folder_returns_404() {
    let (status, _) = get_json_with(
        router_contacts(),
        "/v1.0/me/contactFolders/cf-bogus/contacts",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn graph_single_contact_in_folder_returns_one_body() {
    let (status, v) = get_json_with(
        router_contacts(),
        "/v1.0/me/contactFolders/cf-default/contacts/contact-002",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["id"], "contact-002");
    assert_eq!(v["displayName"], "Bob Bell");

    // Wrong folder for that id: 404.
    let (status, _) = get_json_with(
        router_contacts(),
        "/v1.0/me/contactFolders/cf-vendors/contacts/contact-002",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn graph_single_contact_folder_agnostic_resolves_by_id() {
    let (status, v) = get_json_with(router_contacts(), "/v1.0/me/contacts/contact-100").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["id"], "contact-100");
    assert_eq!(v["parentFolderId"], "cf-vendors");
}

#[tokio::test]
async fn graph_list_all_contacts_spans_folders() {
    // bifrost's contacts_list(None) hits the folder-agnostic list.
    let (status, v) = get_json_with(
        router_contacts(),
        "/v1.0/me/contacts?$select=id,displayName",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = v["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    // All four fixture contacts, across cf-default and cf-vendors.
    assert_eq!(ids.len(), 4);
    assert!(ids.contains(&"contact-001"));
    assert!(ids.contains(&"contact-100")); // lives in cf-vendors

    // $top paginates with an @odata.nextLink.
    let (status, v) = get_json_with(router_contacts(), "/v1.0/me/contacts?$top=2").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["value"].as_array().unwrap().len(), 2);
    assert!(v["@odata.nextLink"].is_string());
}

#[tokio::test]
async fn graph_contact_crud_and_email_filter() {
    let app = router_contacts();

    // Create in the default folder.
    let (status, v) = send_json(
        &app,
        "POST",
        "/v1.0/me/contacts",
        Some(json!({
            "displayName": "New Person",
            "emailAddresses": [{ "address": "new@example.com" }],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(v["displayName"], "New Person");
    assert_eq!(v["parentFolderId"], "cf-default");
    let id = v["id"].as_str().unwrap().to_string();

    // Sparse update.
    let (status, v) = send_json(
        &app,
        "PATCH",
        &format!("/v1.0/me/contacts/{id}"),
        Some(json!({ "displayName": "Renamed" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["displayName"], "Renamed");

    // $filter by email narrows the list to the matching contact.
    let (status, v) = get_json_with(
        app.clone(),
        "/v1.0/me/contactFolders/cf-default/contacts?$filter=emailAddresses/any(a:a/address%20eq%20'bob@example.com')",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = v["value"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["id"].as_str())
        .collect();
    assert_eq!(ids, vec!["contact-002"]);

    // Delete; gone afterwards.
    let (status, _) = send_json(&app, "DELETE", &format!("/v1.0/me/contacts/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = get_json_with(app, &format!("/v1.0/me/contacts/{id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn graph_contacts_delta_initial_dump_then_latest_shortcut() {
    let app = router_contacts();

    // Bootstrap: full dump + deltaLink (cf-default has 3 contacts,
    // small enough to fit in one page).
    let (status, v) = get_json_with(
        app.clone(),
        "/v1.0/me/contactFolders/cf-default/contacts/delta?$select=id",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value = v["value"].as_array().unwrap();
    assert_eq!(value.len(), 3);
    let delta_link = v["@odata.deltaLink"].as_str().unwrap().to_string();

    // Follow up with the deltaLink's path: empty value, fresh
    // deltaLink. (Token pinned to current state; nothing changed.)
    let path_with_token = delta_link
        .split_once("/v1.0/")
        .map(|(_, p)| format!("/v1.0/{p}"))
        .unwrap();
    let (_, v2) = get_json_with(app.clone(), &path_with_token).await;
    assert_eq!(v2["value"].as_array().unwrap().len(), 0);
    assert!(v2["@odata.deltaLink"].is_string());

    // `?$deltatoken=latest` shortcut: empty page, fresh deltaLink,
    // no contact dump regardless of what's in the fixture.
    let (status, v3) = get_json_with(
        app,
        "/v1.0/me/contactFolders/cf-default/contacts/delta?$deltatoken=latest",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v3["value"].as_array().unwrap().len(), 0);
    assert!(v3["@odata.deltaLink"].is_string());
}

// ── Master categories ───────────────────────────────────────────────

fn categories_router_with_log(log: saehrimnir::request_log::RequestLog) -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/graph-categories-small.toml")).unwrap();
    graph::router(graph::AppState::for_test(saehrimnir::shared::handle(fix)).with_request_log(log))
}

fn categories_router() -> axum::Router {
    categories_router_with_log(saehrimnir::request_log::RequestLog::default())
}

#[tokio::test]
async fn graph_list_categories_projects_fixture_in_declaration_order() {
    let (status, v) = get_json_with(categories_router(), "/v1.0/me/outlook/masterCategories").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        v["@odata.context"],
        "https://graph.microsoft.com/v1.0/$metadata#me/outlook/masterCategories"
    );
    let arr = v["value"].as_array().unwrap();
    assert_eq!(arr.len(), 3);
    assert_eq!(arr[0]["id"], "cat-work");
    assert_eq!(arr[0]["displayName"], "Work");
    assert_eq!(arr[0]["color"], "preset0");
    // No-color category omits the field entirely (matches real Graph
    // behaviour for "color = none" categories, where the property is
    // serialised as the string "none" - we leave it absent instead so
    // a client that round-trips the object can distinguish "unset" from
    // "explicit none").
    assert_eq!(arr[2]["id"], "cat-no-color");
    assert!(arr[2].get("color").is_none());
}

#[tokio::test]
async fn graph_get_category_returns_single_resource() {
    let (status, v) = get_json_with(
        categories_router(),
        "/v1.0/me/outlook/masterCategories/cat-personal",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["id"], "cat-personal");
    assert_eq!(v["displayName"], "Personal");
    assert_eq!(v["color"], "preset3");
}

#[tokio::test]
async fn graph_get_category_404s_unknown_id() {
    let (status, v) = get_json_with(
        categories_router(),
        "/v1.0/me/outlook/masterCategories/cat-missing",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["code"], "ResourceNotFound");
}

#[tokio::test]
async fn graph_create_category_mints_id_when_absent_and_bumps_state() {
    let log = saehrimnir::request_log::RequestLog::default();
    let app = categories_router_with_log(log.clone());

    let body = serde_json::json!({
        "displayName": "Urgent",
        "color": "preset5",
    });
    let (status, v) = json_request(
        app.clone(),
        "POST",
        "/v1.0/me/outlook/masterCategories",
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    // The fixture seeds `synthetic_category_seq` to the highest of
    // (a) the largest `mock-category-N` declared (0 here) and
    // (b) the number of declared categories (3). First mint is 4.
    assert_eq!(v["id"], "mock-category-4");
    assert_eq!(v["displayName"], "Urgent");
    assert_eq!(v["color"], "preset5");

    // Body landed in the request log.
    let snap = log.snapshot();
    let entry = snap
        .iter()
        .find(|e| {
            e.command == "POST /v1.0/me/outlook/masterCategories" && !e.detail["body"].is_null()
        })
        .expect("mutation entry recorded");
    assert_eq!(entry.detail["body"]["displayName"], "Urgent");

    // Follow-up list reflects the new category.
    let (_, v) = get_json_with(app, "/v1.0/me/outlook/masterCategories").await;
    assert_eq!(v["value"].as_array().unwrap().len(), 4);
}

#[tokio::test]
async fn graph_create_category_honours_client_supplied_id() {
    let app = categories_router();
    let body = serde_json::json!({
        "id": "cat-custom",
        "displayName": "Custom",
    });
    let (status, v) = json_request(
        app.clone(),
        "POST",
        "/v1.0/me/outlook/masterCategories",
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(v["id"], "cat-custom");
}

#[tokio::test]
async fn graph_create_category_409s_on_duplicate_id() {
    let app = categories_router();
    let body = serde_json::json!({
        "id": "cat-work",
        "displayName": "Work-2",
    });
    let (status, v) =
        json_request(app, "POST", "/v1.0/me/outlook/masterCategories", Some(body)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(v["error"]["code"], "Conflict");
}

#[tokio::test]
async fn graph_create_category_400s_without_display_name() {
    let app = categories_router();
    let body = serde_json::json!({ "color": "preset0" });
    let (status, v) =
        json_request(app, "POST", "/v1.0/me/outlook/masterCategories", Some(body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"]["code"], "BadRequest");
}

#[tokio::test]
async fn graph_patch_category_updates_fields_and_echoes_view() {
    let app = categories_router();
    let body = serde_json::json!({
        "displayName": "Work (renamed)",
        "color": "preset12",
    });
    let (status, v) = json_request(
        app.clone(),
        "PATCH",
        "/v1.0/me/outlook/masterCategories/cat-work",
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["id"], "cat-work");
    assert_eq!(v["displayName"], "Work (renamed)");
    assert_eq!(v["color"], "preset12");

    // Follow-up GET reflects the patch.
    let (_, v) = get_json_with(app, "/v1.0/me/outlook/masterCategories/cat-work").await;
    assert_eq!(v["displayName"], "Work (renamed)");
}

#[tokio::test]
async fn graph_patch_category_404s_unknown_id() {
    let app = categories_router();
    let body = serde_json::json!({ "displayName": "x" });
    let (status, _) = json_request(
        app,
        "PATCH",
        "/v1.0/me/outlook/masterCategories/cat-missing",
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn graph_delete_category_returns_204_and_removes_resource() {
    let log = saehrimnir::request_log::RequestLog::default();
    let app = categories_router_with_log(log.clone());

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/v1.0/me/outlook/masterCategories/cat-work")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Follow-up GET 404s and follow-up list is short by one.
    let (status, _) =
        get_json_with(app.clone(), "/v1.0/me/outlook/masterCategories/cat-work").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, v) = get_json_with(app, "/v1.0/me/outlook/masterCategories").await;
    assert_eq!(v["value"].as_array().unwrap().len(), 2);

    let snap = log.snapshot();
    assert!(snap.iter().any(
        |e| e.command == "DELETE /v1.0/me/outlook/masterCategories/cat-work"
            && e.detail["id"] == "cat-work"
    ));
}

#[tokio::test]
async fn graph_delete_category_404s_unknown_id() {
    let app = categories_router();
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/v1.0/me/outlook/masterCategories/cat-missing")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn graph_category_mutation_records_change_log_transition() {
    // POST -> PATCH -> DELETE should bump fixture.state three times
    // and append three transitions tagged with the right ids.
    let fix = fixture::load(std::path::Path::new("fixtures/graph-categories-small.toml")).unwrap();
    let handle = saehrimnir::shared::handle(fix);
    let initial_state = handle.read().unwrap().primary_state().to_string();
    let app = graph::router(graph::AppState::for_test(Arc::clone(&handle)));

    let body = serde_json::json!({ "id": "cat-new", "displayName": "New" });
    let (s1, _) = json_request(
        app.clone(),
        "POST",
        "/v1.0/me/outlook/masterCategories",
        Some(body),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED);

    let body = serde_json::json!({ "displayName": "Renamed" });
    let (s2, _) = json_request(
        app.clone(),
        "PATCH",
        "/v1.0/me/outlook/masterCategories/cat-new",
        Some(body),
    )
    .await;
    assert_eq!(s2, StatusCode::OK);

    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/v1.0/me/outlook/masterCategories/cat-new")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let fix = handle.read().unwrap();
    assert_ne!(fix.primary_state(), initial_state.as_str());
    let account_id = fix.primary_account().id.clone();
    let trans: Vec<_> = fix.change_log_transitions_for(&account_id).collect();
    assert_eq!(trans.len(), 3);
    assert_eq!(trans[0].category_created, vec!["cat-new".to_string()]);
    assert_eq!(trans[1].category_updated, vec!["cat-new".to_string()]);
    assert_eq!(trans[2].category_destroyed, vec!["cat-new".to_string()]);
    // Sibling categories untouched.
    assert!(fix.categories.iter().any(|c| c.id == "cat-work"));
    assert!(!fix.categories.iter().any(|c| c.id == "cat-new"));
}

// ── Calendar recurrence ─────────────────────────────────────────────

fn recurrence_calendar_router() -> axum::Router {
    let fix = fixture::load(std::path::Path::new(
        "fixtures/calendar-recurrence-small.toml",
    ))
    .unwrap();
    graph::router(graph::AppState::for_test(saehrimnir::shared::handle(fix)))
}

#[tokio::test]
async fn graph_event_get_emits_weekly_recurrence_structured() {
    let app = recurrence_calendar_router();
    let (status, v) = get_json_with(app, "/v1.0/me/events/ev-weekly").await;
    assert_eq!(status, StatusCode::OK);
    let rec = &v["recurrence"];
    assert_eq!(rec["pattern"]["type"], "weekly");
    assert_eq!(rec["pattern"]["interval"], 1);
    // RFC 5545 BYDAY=MO,WE,FR translates to Graph daysOfWeek in the
    // same order. ratatoskr's parse path doesn't care about order
    // (it normalises into a set), but stable order keeps snapshots
    // byte-deterministic.
    assert_eq!(
        rec["pattern"]["daysOfWeek"],
        json!(["monday", "wednesday", "friday"])
    );
    assert_eq!(rec["range"]["type"], "numbered");
    assert_eq!(rec["range"]["numberOfOccurrences"], 10);
    assert_eq!(rec["range"]["startDate"], "2026-01-19");
}

#[tokio::test]
async fn graph_event_get_emits_monthly_recurrence_with_end_date() {
    let app = recurrence_calendar_router();
    let (status, v) = get_json_with(app, "/v1.0/me/events/ev-monthly").await;
    assert_eq!(status, StatusCode::OK);
    let rec = &v["recurrence"];
    assert_eq!(rec["pattern"]["type"], "absoluteMonthly");
    assert_eq!(rec["pattern"]["dayOfMonth"], 15);
    assert_eq!(rec["pattern"]["interval"], 1);
    assert_eq!(rec["range"]["type"], "endDate");
    assert_eq!(rec["range"]["endDate"], "2026-12-15");
    assert_eq!(rec["range"]["startDate"], "2026-01-15");
}

#[tokio::test]
async fn graph_single_instance_event_omits_recurrence() {
    let app = recurrence_calendar_router();
    let (status, v) = get_json_with(app, "/v1.0/me/events/ev-single").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        v.get("recurrence").is_none(),
        "single instance leaked recurrence"
    );
}

// ── Multi-account mail routing (Stage 3) ────────────────────────────
//
// Stage 3 grows `/v1.0/users/{userId}/...` parallel routes for the
// Graph mail surface. `me` is an alias for the primary account;
// other userIds must match a declared `[[account]]`. The same
// inner handlers power both path families, scoped to the resolved
// account.

fn multi_account_graph_router() -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/multi-account-small.toml")).unwrap();
    graph::router(graph::AppState::for_test(saehrimnir::shared::handle(fix)))
}

#[tokio::test]
async fn graph_me_mailfolders_scopes_to_primary() {
    let (status, v) = get_json_with(multi_account_graph_router(), "/v1.0/me/mailFolders").await;
    assert_eq!(status, StatusCode::OK);
    let folders = v["value"].as_array().unwrap();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0]["id"], "mbx-primary-inbox");
}

#[tokio::test]
async fn graph_users_named_account_lists_that_accounts_folders() {
    let (status, v) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/users/account-secondary/mailFolders",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let folders = v["value"].as_array().unwrap();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0]["id"], "mbx-secondary-inbox");
}

#[tokio::test]
async fn graph_users_me_alias_resolves_to_primary() {
    let (status, v) =
        get_json_with(multi_account_graph_router(), "/v1.0/users/me/mailFolders").await;
    assert_eq!(status, StatusCode::OK);
    let folders = v["value"].as_array().unwrap();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0]["id"], "mbx-primary-inbox");
}

#[tokio::test]
async fn graph_users_unknown_account_returns_404_resource_not_found() {
    let (status, v) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/users/account-bogus/mailFolders",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["code"], "ResourceNotFound");
}

#[tokio::test]
async fn graph_users_messages_scope_by_account() {
    // /me/...inbox/messages: primary's email only.
    let (status, v) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/me/mailFolders/mbx-primary-inbox/messages",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let msgs = v["value"].as_array().unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["id"], "email-primary-001");

    // /users/{secondary}/...inbox/messages: secondary's email.
    let (status, v) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/users/account-secondary/mailFolders/mbx-secondary-inbox/messages",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let msgs = v["value"].as_array().unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["id"], "email-secondary-001");

    // Cross-account access: primary cannot resolve secondary's
    // mailbox id (folder lookup scoped to the named account).
    let (status, _) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/me/mailFolders/mbx-secondary-inbox/messages",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn graph_inbox_alias_resolves_within_named_account() {
    // The well-known `inbox` alias resolves to the named account's
    // inbox, not the primary's. Both accounts declare an inbox-role
    // mailbox, so the aliased lookup picks the right one per path.
    let (status, v) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/users/account-secondary/mailFolders/inbox/messages",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let msgs = v["value"].as_array().unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["id"], "email-secondary-001");
}

#[tokio::test]
async fn graph_users_calendars_scope_by_account() {
    // /me/calendars sees primary; /users/{secondary}/calendars sees
    // the secondary account's calendar.
    let (status, v) = get_json_with(multi_account_graph_router(), "/v1.0/me/calendars").await;
    assert_eq!(status, StatusCode::OK);
    let cals = v["value"].as_array().unwrap();
    assert_eq!(cals.len(), 1);
    assert_eq!(cals[0]["id"], "cal-primary");

    let (status, v) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/users/account-secondary/calendars",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let cals = v["value"].as_array().unwrap();
    assert_eq!(cals.len(), 1);
    assert_eq!(cals[0]["id"], "cal-secondary");
}

#[tokio::test]
async fn graph_users_events_scope_by_account() {
    // Primary's calendar can be listed via /me/calendars/{id}/events;
    // secondary's via /users/{secondary}/calendars/{id}/events. Cross-
    // account lookup of a sibling-account event id returns 404.
    let (status, v) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/users/account-secondary/calendars/cal-secondary/events",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let evs = v["value"].as_array().unwrap();
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0]["id"], "ev-secondary-001");

    // Primary's /me/events/{event} doesn't see secondary's event.
    let (status, _) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/me/events/ev-secondary-001",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // /users/{secondary}/events/{event} does.
    let (status, v) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/users/account-secondary/events/ev-secondary-001",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["subject"], "Secondary review");
}

#[tokio::test]
async fn graph_users_contact_folders_scope_by_account() {
    let (status, v) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/users/account-secondary/contactFolders",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let folders = v["value"].as_array().unwrap();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0]["id"], "cf-secondary");

    // /me sees only primary's folder.
    let (status, v) = get_json_with(multi_account_graph_router(), "/v1.0/me/contactFolders").await;
    assert_eq!(status, StatusCode::OK);
    let folders = v["value"].as_array().unwrap();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0]["id"], "cf-primary");
}

#[tokio::test]
async fn graph_users_contacts_scope_by_account() {
    let (status, v) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/users/account-secondary/contactFolders/cf-secondary/contacts",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let cs = v["value"].as_array().unwrap();
    assert_eq!(cs.len(), 1);
    assert_eq!(cs[0]["id"], "contact-secondary-001");

    // Folder-agnostic single-contact lookup: /me sees primary; the
    // secondary's path sees secondary.
    let (status, _) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/me/contacts/contact-secondary-001",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, v) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/users/account-secondary/contacts/contact-secondary-001",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["displayName"], "Bob (secondary)");
}

#[tokio::test]
async fn graph_users_categories_scope_by_account() {
    let (status, v) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/users/account-secondary/outlook/masterCategories",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let cats = v["value"].as_array().unwrap();
    assert_eq!(cats.len(), 1);
    assert_eq!(cats[0]["id"], "cat-secondary-urgent");

    let (status, v) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/me/outlook/masterCategories",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let cats = v["value"].as_array().unwrap();
    assert_eq!(cats.len(), 1);
    assert_eq!(cats[0]["id"], "cat-primary-work");
}

#[tokio::test]
async fn graph_users_unknown_for_each_resource_family_returns_404() {
    for path in [
        "/v1.0/users/account-bogus/calendars",
        "/v1.0/users/account-bogus/contactFolders",
        "/v1.0/users/account-bogus/outlook/masterCategories",
    ] {
        let (status, v) = get_json_with(multi_account_graph_router(), path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "expected 404 for {path}");
        assert_eq!(v["error"]["code"], "ResourceNotFound", "for {path}");
    }
}

// ── Shared-mailbox per-message twins on /users/{id} ─────────────────
//
// The collection reads above cover folder / calendar / contact
// scoping. These pin the per-message treatments the A5 shared-mailbox
// mail-sync harness leans on - single-message GET, `$value`, `$batch`
// sub-requests, and calendarView - on the `/users/{id}` prefix, and
// assert cross-account isolation through each.

#[tokio::test]
async fn graph_users_single_message_scopes_by_account() {
    // /users/{secondary}/messages/{id} hydrates the secondary's mail.
    let (status, v) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/users/account-secondary/messages/email-secondary-001",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["id"], "email-secondary-001");
    assert_eq!(v["subject"], "Hello secondary");
    assert_eq!(
        v["from"]["emailAddress"]["address"],
        "elsewhere@example.com"
    );

    // Primary's /me can't reach the secondary's message id.
    let (status, v) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/me/messages/email-secondary-001",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["code"], "ErrorItemNotFound");

    // Unknown user 404s ResourceNotFound (not ErrorItemNotFound).
    let (status, v) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/users/account-bogus/messages/email-secondary-001",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["code"], "ResourceNotFound");
}

#[tokio::test]
async fn graph_users_message_value_twin_returns_rfc822() {
    let (status, bytes, _) = get_raw(
        multi_account_graph_router(),
        "/v1.0/users/account-secondary/messages/email-secondary-001/$value",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = String::from_utf8(bytes).unwrap();
    assert!(body.contains("Subject: Hello secondary"), "got: {body}");
    assert!(
        body.contains("elsewhere@example.com"),
        "from missing: {body}"
    );
    assert!(body.contains("Secondary account inbox email."));

    // Cross-account: /me can't assemble the secondary's bytes.
    let (status, _, _) = get_raw(
        multi_account_graph_router(),
        "/v1.0/me/messages/email-secondary-001/$value",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn graph_users_batch_subrequests_scope_by_account() {
    // A single $batch mixing a /users/{secondary} sub-request (hits)
    // with a cross-account /me sub-request for the same id (misses).
    let body = json!({
        "requests": [
            { "id": "1", "method": "GET",
              "url": "/users/account-secondary/messages/email-secondary-001" },
            { "id": "2", "method": "GET",
              "url": "/me/messages/email-secondary-001" },
        ]
    });
    let resp = multi_account_graph_router()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1.0/$batch")
                .header(header::HOST, "127.0.0.1:9999")
                .header(header::AUTHORIZATION, "Bearer doesnt-matter")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    let responses = v["responses"].as_array().unwrap();
    let by_id = |id: &str| responses.iter().find(|r| r["id"] == id).unwrap();

    // Secondary-prefixed sub-request hydrates the message.
    let r1 = by_id("1");
    assert_eq!(r1["status"], 200);
    assert_eq!(r1["body"]["id"], "email-secondary-001");
    assert_eq!(r1["body"]["subject"], "Hello secondary");

    // /me sub-request for the same id misses per-item (primary scope).
    let r2 = by_id("2");
    assert_eq!(r2["status"], 404);
    assert_eq!(r2["body"]["error"]["code"], "ErrorItemNotFound");
}

#[tokio::test]
async fn graph_users_calendar_view_twin_scopes_by_account() {
    // Range read of the secondary's calendar via the /users/{id} twin.
    let (status, v) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/users/account-secondary/calendars/cal-secondary/calendarView\
         ?startDateTime=2026-02-01T00:00:00Z&endDateTime=2026-02-28T00:00:00Z",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let evs = v["value"].as_array().unwrap();
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0]["id"], "ev-secondary-001");
    assert_eq!(evs[0]["subject"], "Secondary review");

    // The calendarView/delta twin walks the same account-scoped set.
    let (status, v) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/users/account-secondary/calendars/cal-secondary/calendarView/delta",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let evs = v["value"].as_array().unwrap();
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0]["id"], "ev-secondary-001");

    // Cross-account: primary's /me can't resolve the secondary calendar.
    let (status, _) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/me/calendars/cal-secondary/calendarView\
         ?startDateTime=2026-02-01T00:00:00Z&endDateTime=2026-02-28T00:00:00Z",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ── Graph groups ────────────────────────────────────────────────────

#[tokio::test]
async fn graph_groups_lists_every_declared_group() {
    let (status, v) = get_json_with(multi_account_graph_router(), "/v1.0/groups").await;
    assert_eq!(status, StatusCode::OK);
    let groups = v["value"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    let by_id: std::collections::HashMap<&str, &Value> = groups
        .iter()
        .map(|g| (g["id"].as_str().unwrap(), g))
        .collect();
    let eng = by_id["grp-eng"];
    assert_eq!(eng["displayName"], "Engineering");
    assert_eq!(eng["mail"], "engineering@example.com");
    assert_eq!(eng["mailEnabled"], true);
    assert_eq!(eng["securityEnabled"], true);
    // The members list is NOT inlined on `/groups` - clients call
    // `/groups/{id}/members` to expand. Real Graph behaves the same.
    assert!(eng.get("members").is_none());
}

#[tokio::test]
async fn graph_groups_single_returns_resource_or_404() {
    let (status, v) = get_json_with(multi_account_graph_router(), "/v1.0/groups/grp-leads").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["id"], "grp-leads");

    let (status, v) = get_json_with(multi_account_graph_router(), "/v1.0/groups/grp-bogus").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["code"], "ResourceNotFound");
}

#[tokio::test]
async fn graph_group_members_projects_accounts_as_users() {
    let (status, v) =
        get_json_with(multi_account_graph_router(), "/v1.0/groups/grp-eng/members").await;
    assert_eq!(status, StatusCode::OK);
    let members = v["value"].as_array().unwrap();
    assert_eq!(members.len(), 2);
    let ids: Vec<&str> = members.iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"account-primary"));
    assert!(ids.contains(&"account-secondary"));
    // Each member is projected as a #microsoft.graph.user with
    // mail / userPrincipalName populated from account.name.
    let primary = members
        .iter()
        .find(|m| m["id"] == "account-primary")
        .unwrap();
    assert_eq!(primary["@odata.type"], "#microsoft.graph.user");
    assert_eq!(primary["mail"], "primary@example.com");
    assert_eq!(primary["userPrincipalName"], "primary@example.com");
}

#[tokio::test]
async fn graph_me_memberof_scopes_by_bearer_token() {
    // No bearer -> primary; primary is in both groups.
    let (_, v) = get_json_with(multi_account_graph_router(), "/v1.0/me/memberOf").await;
    let groups = v["value"].as_array().unwrap();
    let ids: Vec<&str> = groups.iter().map(|g| g["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"grp-eng"));
    assert!(ids.contains(&"grp-leads"));
    assert_eq!(groups.len(), 2);
}

#[tokio::test]
async fn graph_users_memberof_scopes_by_account() {
    // Secondary is only in grp-eng (Engineering), not Leadership.
    let (_, v) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/users/account-secondary/memberOf",
    )
    .await;
    let groups = v["value"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["id"], "grp-eng");

    // Unknown user 404s.
    let (status, _) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/users/account-bogus/memberOf",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // `me` resolves to primary on the users/{} path too.
    let (_, v) = get_json_with(multi_account_graph_router(), "/v1.0/users/me/memberOf").await;
    let groups = v["value"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
}

#[tokio::test]
async fn graph_group_transitive_members_mirrors_direct_members() {
    // No nested groups in v0, so transitiveMembers == members.
    let (status, v) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/groups/grp-eng/transitiveMembers",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let members = v["value"].as_array().unwrap();
    assert_eq!(members.len(), 2);
    let ids: Vec<&str> = members.iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"account-primary"));
    assert!(ids.contains(&"account-secondary"));

    // The microsoft.graph.user type-cast is all-pass in v0 (every
    // member is user-typed); same result set.
    let (status, v) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/groups/grp-eng/transitiveMembers/microsoft.graph.user",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let members = v["value"].as_array().unwrap();
    assert_eq!(members.len(), 2);
    assert_eq!(members[0]["@odata.type"], "#microsoft.graph.user");

    // Unknown group still 404s through the transitive twin.
    let (status, v) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/groups/grp-bogus/transitiveMembers",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["code"], "ResourceNotFound");
}

#[tokio::test]
async fn graph_memberof_group_type_cast_mirrors_plain_memberof() {
    // /me/memberOf/microsoft.graph.group == /me/memberOf in v0.
    let (status, v) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/me/memberOf/microsoft.graph.group",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let groups = v["value"].as_array().unwrap();
    assert_eq!(groups.len(), 2);

    // Path-resolved twin scopes by account and honours the cast.
    let (_, v) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/users/account-secondary/memberOf/microsoft.graph.group",
    )
    .await;
    let groups = v["value"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["id"], "grp-eng");

    // Unknown user 404s through the type-cast twin.
    let (status, _) = get_json_with(
        multi_account_graph_router(),
        "/v1.0/users/account-bogus/memberOf/microsoft.graph.group",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ── Calendar recurrence writes ──────────────────────────────────────

#[tokio::test]
async fn graph_create_event_with_weekly_recurrence_round_trips() {
    let app = recurrence_calendar_router();
    let body = json!({
        "subject": "Weekly graph sync",
        "start": { "dateTime": "2026-03-02T10:00:00Z", "timeZone": "UTC" },
        "end":   { "dateTime": "2026-03-02T10:30:00Z", "timeZone": "UTC" },
        "recurrence": {
            "pattern": {
                "type": "weekly",
                "interval": 1,
                "daysOfWeek": ["monday", "wednesday"]
            },
            "range": {
                "type": "numbered",
                "startDate": "2026-03-02",
                "numberOfOccurrences": 8
            }
        }
    });
    let (status, v) = json_request(
        app.clone(),
        "POST",
        "/v1.0/me/calendars/cal-work/events",
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = v["id"].as_str().unwrap().to_string();
    // Follow-up GET: the server's read-side translator should
    // reconstruct an equivalent pattern/range.
    let (status, v) = get_json_with(app, &format!("/v1.0/me/events/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    let rec = &v["recurrence"];
    assert_eq!(rec["pattern"]["type"], "weekly");
    assert_eq!(rec["pattern"]["daysOfWeek"], json!(["monday", "wednesday"]));
    assert_eq!(rec["range"]["type"], "numbered");
    assert_eq!(rec["range"]["numberOfOccurrences"], 8);
}

#[tokio::test]
async fn graph_create_event_with_absolute_monthly_recurrence() {
    let app = recurrence_calendar_router();
    let body = json!({
        "subject": "Monthly review",
        "start": { "dateTime": "2026-03-15T17:00:00Z", "timeZone": "UTC" },
        "end":   { "dateTime": "2026-03-15T18:00:00Z", "timeZone": "UTC" },
        "recurrence": {
            "pattern": { "type": "absoluteMonthly", "interval": 1, "dayOfMonth": 15 },
            "range": { "type": "endDate", "startDate": "2026-03-15", "endDate": "2026-12-15" }
        }
    });
    let (status, v) = json_request(
        app.clone(),
        "POST",
        "/v1.0/me/calendars/cal-work/events",
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = v["id"].as_str().unwrap().to_string();
    let (_, v) = get_json_with(app, &format!("/v1.0/me/events/{id}")).await;
    let rec = &v["recurrence"];
    assert_eq!(rec["pattern"]["type"], "absoluteMonthly");
    assert_eq!(rec["pattern"]["dayOfMonth"], 15);
    assert_eq!(rec["range"]["type"], "endDate");
    assert_eq!(rec["range"]["endDate"], "2026-12-15");
}

#[tokio::test]
async fn graph_patch_recurrence_clears_with_null() {
    let app = recurrence_calendar_router();
    // ev-weekly already has recurrence; PATCH null clears it.
    let body = json!({ "recurrence": null });
    let (status, _) = json_request(
        app.clone(),
        "PATCH",
        "/v1.0/me/events/ev-weekly",
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, v) = get_json_with(app, "/v1.0/me/events/ev-weekly").await;
    assert!(
        v.get("recurrence").is_none(),
        "expected recurrence cleared: {v}"
    );
}

/// Graph container-CRUD round-trip mirroring ratatoskr's
/// `graph-container-crud` gate at the mock's own request layer: a parent
/// and a child folder are created top-level, the child is MOVED under
/// the parent, and the reparent must surface in a follow-up read. The
/// gate's `containers_list` enumerates folders the way bifrost's Graph
/// account does - the flat `/mailFolders` for top-level folders, then a
/// recursive walk into each folder's `/childFolders` - so the moved
/// (now-nested) child is read back through the parent's `childFolders`,
/// not the top-level list. The existing folder-CRUD test only checks the
/// move *response*, never the list readback the gate asserts on.
#[tokio::test]
async fn folder_move_reparent_round_trips_through_list() {
    let app = router();

    let (status, parent) = send_json(
        &app,
        "POST",
        "/v1.0/me/mailFolders",
        Some(json!({ "displayName": "HarnessParent" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let parent_id = parent["id"].as_str().unwrap().to_string();
    assert_eq!(parent["parentFolderId"], Value::Null);

    let (status, child) = send_json(
        &app,
        "POST",
        "/v1.0/me/mailFolders",
        Some(json!({ "displayName": "HarnessChild" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let child_id = child["id"].as_str().unwrap().to_string();

    // Before the move the child is a top-level folder.
    let top_level_ids = |v: &Value| -> Vec<String> {
        v["value"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|f| f["id"].as_str().map(str::to_string))
            .collect()
    };
    let (_, list) = get_json_with(app.clone(), "/v1.0/me/mailFolders").await;
    assert!(
        top_level_ids(&list).contains(&child_id),
        "child should start top-level"
    );

    // Move the child under the parent.
    let (status, _) = send_json(
        &app,
        "POST",
        &format!("/v1.0/me/mailFolders/{child_id}/move"),
        Some(json!({ "destinationId": parent_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The reparent surfaces through the parent's childFolders (bifrost's
    // recursive enumeration), and the child leaves the top-level list.
    let find_child = |v: &Value| -> Option<Value> {
        v["value"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["id"] == json!(child_id))
            .cloned()
    };
    let (_, list) = get_json_with(app.clone(), "/v1.0/me/mailFolders").await;
    assert!(
        !top_level_ids(&list).contains(&child_id),
        "moved child still listed at top level"
    );
    let (status, children) = get_json_with(
        app.clone(),
        &format!("/v1.0/me/mailFolders/{parent_id}/childFolders"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let listed =
        find_child(&children).expect("child folder missing from parent's childFolders after move");
    assert_eq!(
        listed["parentFolderId"], parent_id,
        "child folder did not reparent on the server after move"
    );

    // Rename is reflected too (still nested under the parent).
    let (status, _) = send_json(
        &app,
        "PATCH",
        &format!("/v1.0/me/mailFolders/{child_id}"),
        Some(json!({ "displayName": "HarnessRenamed" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, children) = get_json_with(
        app.clone(),
        &format!("/v1.0/me/mailFolders/{parent_id}/childFolders"),
    )
    .await;
    let listed = find_child(&children).expect("renamed child folder missing from childFolders");
    assert_eq!(listed["displayName"], "HarnessRenamed");
    assert_eq!(
        listed["parentFolderId"], parent_id,
        "rename dropped the parent"
    );

    // Delete both; neither appears in the top-level list.
    let (status, _) = send_json(
        &app,
        "DELETE",
        &format!("/v1.0/me/mailFolders/{child_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = send_json(
        &app,
        "DELETE",
        &format!("/v1.0/me/mailFolders/{parent_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, list) = get_json_with(app, "/v1.0/me/mailFolders").await;
    let ids: Vec<&str> = list["value"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["id"].as_str())
        .collect();
    assert!(
        !ids.contains(&child_id.as_str()),
        "deleted child still listed"
    );
    assert!(
        !ids.contains(&parent_id.as_str()),
        "deleted parent still listed"
    );
}

// ── Message reactions (singleValueExtendedProperties) ────────────────

fn reactions_router() -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/graph-reactions.toml")).unwrap();
    graph::router(graph::AppState::for_test(saehrimnir::shared::handle(fix)))
}

/// The GUID-qualified property ids real Graph uses for Exchange
/// reactions. Spelled out literally here rather than pulled from a
/// crate constant: the point of the assertion is that a consumer
/// filtering on the *real* id matches what the mock serves, so the
/// test must not share a typo with the implementation.
const OWNER_REACTION_TYPE_ID: &str =
    "String {41F28F13-83F4-4114-A584-EEDB5A6B0BFF} Name OwnerReactionType";
const REACTIONS_COUNT_ID: &str =
    "Integer {41F28F13-83F4-4114-A584-EEDB5A6B0BFF} Name ReactionsCount";

/// The `$expand` clause the consumer sends, percent-encoded for the
/// request line. Mirrors bifrost's shape: `attachments(...)` first,
/// then `singleValueExtendedProperties($filter=id eq '..' or id eq
/// '..')` - both carrying commas / `=` inside their options.
fn reactions_expand_query() -> String {
    let clause = format!(
        "attachments($select=id,name,contentType),\
         singleValueExtendedProperties($filter=id eq '{OWNER_REACTION_TYPE_ID}' \
         or id eq '{REACTIONS_COUNT_ID}')"
    );
    format!("$expand={}", pct_encode(&clause))
}

/// Minimal percent-encoder for the characters a URI query cannot carry
/// literally (space / braces / quotes). Everything else passes through.
fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            ' ' => out.push_str("%20"),
            '{' => out.push_str("%7B"),
            '}' => out.push_str("%7D"),
            '\'' => out.push_str("%27"),
            _ => out.push(c),
        }
    }
    out
}

/// Pull the `singleValueExtendedProperties` entry with the given id out
/// of a projected message. `None` when the property is absent.
fn svep<'a>(msg: &'a Value, id: &str) -> Option<&'a Value> {
    msg["singleValueExtendedProperties"]
        .as_array()?
        .iter()
        .find(|p| p["id"] == id)
}

#[tokio::test]
async fn graph_reactions_expand_serves_both_extended_properties() {
    let q = reactions_expand_query();

    // Owner reacted and others did: both properties present.
    let (status, v) = get_json_with(
        reactions_router(),
        &format!("/v1.0/me/messages/email-001?{q}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        svep(&v, OWNER_REACTION_TYPE_ID).unwrap()["value"],
        "like",
        "owner reaction type: {v}"
    );
    // Graph types `value` as a string even for an Integer-typed MAPI
    // property, so the count arrives quoted.
    assert_eq!(svep(&v, REACTIONS_COUNT_ID).unwrap()["value"], "3");

    // Others reacted but the owner did not: count only. The owner-type
    // property is absent, NOT present with an empty value.
    let (status, v) = get_json_with(
        reactions_router(),
        &format!("/v1.0/me/messages/email-002?{q}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        svep(&v, OWNER_REACTION_TYPE_ID).is_none(),
        "owner type must be absent, not empty: {v}"
    );
    assert_eq!(svep(&v, REACTIONS_COUNT_ID).unwrap()["value"], "2");

    // Nobody reacted: the array is empty even though the $expand
    // selected both properties.
    let (status, v) = get_json_with(
        reactions_router(),
        &format!("/v1.0/me/messages/email-003?{q}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        v["singleValueExtendedProperties"],
        json!([]),
        "no reaction staged: {v}"
    );

    // The sibling attachments expand in the same clause still parses:
    // the paren-aware split must not shred it.
    assert!(v["attachments"].is_array());
}

#[tokio::test]
async fn graph_reactions_absent_without_expand() {
    // No $expand: the key is still present (consumers read it
    // unconditionally) but empty, even for a message that has both
    // properties staged.
    let (status, v) = get_json_with(reactions_router(), "/v1.0/me/messages/email-001").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["singleValueExtendedProperties"], json!([]));

    // An $expand that names only attachments does not leak reactions.
    let (status, v) = get_json_with(
        reactions_router(),
        "/v1.0/me/messages/email-001?$expand=attachments($select=id,name)",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["singleValueExtendedProperties"], json!([]));
}

#[tokio::test]
async fn graph_reactions_filter_selects_one_property() {
    // A $filter naming only the count gets only the count back, even
    // though the message also carries an owner reaction type.
    let clause = format!("singleValueExtendedProperties($filter=id eq '{REACTIONS_COUNT_ID}')");
    let (status, v) = get_json_with(
        reactions_router(),
        &format!(
            "/v1.0/me/messages/email-001?$expand={}",
            pct_encode(&clause)
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let props = v["singleValueExtendedProperties"].as_array().unwrap();
    assert_eq!(props.len(), 1, "only the count was selected: {v}");
    assert_eq!(props[0]["id"], REACTIONS_COUNT_ID);

    // An unfiltered expand means "every extended property on the item".
    let (status, v) = get_json_with(
        reactions_router(),
        "/v1.0/me/messages/email-001?$expand=singleValueExtendedProperties",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        v["singleValueExtendedProperties"].as_array().unwrap().len(),
        2
    );
}

#[tokio::test]
async fn graph_batch_reads_reactions_per_message() {
    // The shape the consumer actually drives: a chunked $batch of
    // per-message extended-property reads.
    let q = reactions_expand_query();
    let body = json!({
        "requests": [
            { "id": "1", "method": "GET", "url": format!("/me/messages/email-001?{q}") },
            { "id": "2", "method": "GET", "url": format!("/me/messages/email-002?{q}") },
            { "id": "3", "method": "GET", "url": format!("/me/messages/email-003?{q}") },
        ]
    });
    let resp = reactions_router()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1.0/$batch")
                .header(header::HOST, "127.0.0.1:9999")
                .header(header::AUTHORIZATION, "Bearer doesnt-matter")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    let responses = v["responses"].as_array().unwrap();
    let by_id = |id: &str| responses.iter().find(|r| r["id"] == id).unwrap();

    let r1 = &by_id("1")["body"];
    assert_eq!(svep(r1, OWNER_REACTION_TYPE_ID).unwrap()["value"], "like");
    assert_eq!(svep(r1, REACTIONS_COUNT_ID).unwrap()["value"], "3");

    let r2 = &by_id("2")["body"];
    assert!(svep(r2, OWNER_REACTION_TYPE_ID).is_none());
    assert_eq!(svep(r2, REACTIONS_COUNT_ID).unwrap()["value"], "2");

    // Cleared / never-set message: empty array, not a missing key and
    // not an entry with an empty value.
    let r3 = &by_id("3")["body"];
    assert_eq!(r3["singleValueExtendedProperties"], json!([]));
}

/// The `$filter` the consumer sends on the nested-collection read,
/// un-encoded (it rides a `$batch` sub-request URL, which is a JSON
/// string, not a request line).
fn reactions_collection_query() -> String {
    format!("$filter=id eq '{OWNER_REACTION_TYPE_ID}' or id eq '{REACTIONS_COUNT_ID}'")
}

/// Pull an entry out of a nested-collection extended-property read
/// (`{ "value": [ { id, value } ] }`), as opposed to the `$expand`
/// projection [`svep`] reads.
fn collection_prop<'a>(body: &'a Value, id: &str) -> Option<&'a Value> {
    body["value"].as_array()?.iter().find(|p| p["id"] == id)
}

/// The read the consumer's reaction refresh ACTUALLY issues: per-message
/// `$batch` GETs on the nested `singleValueExtendedProperties`
/// collection, not an `$expand` on the message. Every item must be 200,
/// including the message with no reaction staged - the consumer routes
/// any non-2xx into its failed lane, where a transient error is
/// indistinguishable from a real answer and the cached reaction is
/// stranded rather than updated.
#[tokio::test]
async fn graph_batch_reads_reactions_from_the_nested_property_collection() {
    let q = reactions_collection_query();
    let body = json!({
        "requests": [
            { "id": "0", "method": "GET",
              "url": format!("/me/messages/email-001/singleValueExtendedProperties?{q}") },
            { "id": "1", "method": "GET",
              "url": format!("/me/messages/email-002/singleValueExtendedProperties?{q}") },
            { "id": "2", "method": "GET",
              "url": format!("/me/messages/email-003/singleValueExtendedProperties?{q}") },
            { "id": "3", "method": "GET",
              "url": format!("/me/messages/no-such-message/singleValueExtendedProperties?{q}") },
        ]
    });
    let resp = reactions_router()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1.0/$batch")
                .header(header::HOST, "127.0.0.1:9999")
                .header(header::AUTHORIZATION, "Bearer doesnt-matter")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    let responses = v["responses"].as_array().unwrap();
    let by_id = |id: &str| responses.iter().find(|r| r["id"] == id).unwrap();

    // Owner reacted plus a count.
    let r0 = by_id("0");
    assert_eq!(r0["status"], 200, "{r0}");
    assert_eq!(
        collection_prop(&r0["body"], OWNER_REACTION_TYPE_ID).unwrap()["value"],
        "like"
    );
    assert_eq!(
        collection_prop(&r0["body"], REACTIONS_COUNT_ID).unwrap()["value"],
        "3"
    );

    // Count only: the owner property is absent, not empty.
    let r1 = by_id("1");
    assert_eq!(r1["status"], 200, "{r1}");
    assert!(collection_prop(&r1["body"], OWNER_REACTION_TYPE_ID).is_none());
    assert_eq!(
        collection_prop(&r1["body"], REACTIONS_COUNT_ID).unwrap()["value"],
        "2"
    );

    // No reaction at all: a clean 200 with an empty collection. This is
    // a real "no reaction" answer, not an error.
    let r2 = by_id("2");
    assert_eq!(r2["status"], 200, "{r2}");
    assert_eq!(r2["body"]["value"], json!([]), "{r2}");

    // An id that is not in the account is still a genuine per-item 404.
    let r3 = by_id("3");
    assert_eq!(r3["status"], 404, "{r3}");
}

/// The nested collection honours the `$filter` the same way the
/// `$expand` inner option does - both go through one selection rule.
#[tokio::test]
async fn graph_extended_property_collection_honours_filter_and_404s() {
    // Count only.
    let (status, v) = get_json_with(
        reactions_router(),
        &format!(
            "/v1.0/me/messages/email-001/singleValueExtendedProperties?{}",
            pct_encode(&format!("$filter=id eq '{REACTIONS_COUNT_ID}'"))
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let props = v["value"].as_array().unwrap();
    assert_eq!(props.len(), 1, "{v}");
    assert_eq!(props[0]["id"], REACTIONS_COUNT_ID);

    // No filter = every extended property on the item.
    let (status, v) = get_json_with(
        reactions_router(),
        "/v1.0/me/messages/email-001/singleValueExtendedProperties",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["value"].as_array().unwrap().len(), 2, "{v}");

    let (status, v) = get_json_with(
        reactions_router(),
        "/v1.0/me/messages/nope/singleValueExtendedProperties",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["code"], "ErrorItemNotFound");
}

#[tokio::test]
async fn graph_reactions_surface_in_folder_listing_and_delta() {
    // The same projection backs the collection reads, so an initial
    // sync page carries reactions without a per-message follow-up.
    let q = reactions_expand_query();
    let (status, v) = get_json_with(
        reactions_router(),
        &format!("/v1.0/me/mailFolders/mb-inbox/messages?{q}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let msg = v["value"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "email-001")
        .unwrap();
    assert_eq!(svep(msg, OWNER_REACTION_TYPE_ID).unwrap()["value"], "like");

    let (status, v) = get_json_with(
        reactions_router(),
        &format!("/v1.0/me/mailFolders/mb-inbox/messages/delta?{q}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let msg = v["value"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "email-002")
        .unwrap();
    assert_eq!(svep(msg, REACTIONS_COUNT_ID).unwrap()["value"], "2");
    assert!(svep(msg, OWNER_REACTION_TYPE_ID).is_none());
}

// ── reaction clear (property-scoped DELETE) ─────────────────────────

/// bifrost's reaction CLEAR: a `$batch` of
/// `DELETE .../messages/{id}/singleValueExtendedProperties/{propertyId}`
/// per message (`graph pim.rs::delete_extended_property`), submitted
/// with tolerate-404 on.
///
/// The status distinction is the whole point. The consumer reads a 404
/// as "already absent, nothing to clear" and still resolves the item
/// Ok; a 501 - what the `$batch` default arm returned before this route
/// existed - is an unmodelled method, which puts the item in the failed
/// lane and strands the cached reaction. So every not-applicable case
/// must be a 404 and none of them may be a 501.
#[tokio::test]
async fn graph_batch_clears_an_extended_property_and_404s_the_rest() {
    let app = reactions_router();
    let owner_prop = pct_encode(OWNER_REACTION_TYPE_ID);
    let count_prop = pct_encode(REACTIONS_COUNT_ID);
    let body = json!({
        "requests": [
            // email-001 holds both properties: clearing the owner slot
            // succeeds and leaves the count alone.
            { "id": "1", "method": "DELETE",
              "url": format!("/me/messages/email-001/singleValueExtendedProperties/{owner_prop}") },
            // email-002 has a count but no owner reaction: clearing the
            // absent slot is a 404, not an error.
            { "id": "2", "method": "DELETE",
              "url": format!("/me/messages/email-002/singleValueExtendedProperties/{owner_prop}") },
            // email-003 has neither.
            { "id": "3", "method": "DELETE",
              "url": format!("/me/messages/email-003/singleValueExtendedProperties/{count_prop}") },
            // Unknown message.
            { "id": "4", "method": "DELETE",
              "url": format!("/me/messages/no-such-message/singleValueExtendedProperties/{owner_prop}") },
            // A property id the fixture models nothing for.
            { "id": "5", "method": "DELETE",
              "url": "/me/messages/email-001/singleValueExtendedProperties/String%20%7B00000000-0000-0000-0000-000000000000%7D%20Name%20Whatever" },
        ]
    });
    let (status, v) = post_batch(app.clone(), &body).await;
    assert_eq!(status, StatusCode::OK);
    let responses = v["responses"].as_array().unwrap();
    let by_id = |id: &str| responses.iter().find(|r| r["id"] == id).unwrap();

    assert_eq!(by_id("1")["status"], 204, "{}", by_id("1"));
    for id in ["2", "3", "4", "5"] {
        let r = by_id(id);
        assert_eq!(
            r["status"], 404,
            "sub-request {id} must be a tolerated 404, never a 501: {r}"
        );
    }

    // The clear is real and surgical: the owner property is gone, the
    // count on the same message survives.
    let q = reactions_collection_query();
    let (status, read) = get_json_with(
        app,
        &format!(
            "/v1.0/me/messages/email-001/singleValueExtendedProperties?{}",
            pct_encode(&q)
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        collection_prop(&read, OWNER_REACTION_TYPE_ID).is_none(),
        "cleared property must read back ABSENT, not empty: {read}"
    );
    assert_eq!(
        collection_prop(&read, REACTIONS_COUNT_ID).unwrap()["value"],
        "3",
        "the sibling property must survive a property-scoped clear: {read}"
    );
}

/// The routed twin of the same clear, so the two paths cannot drift.
#[tokio::test]
async fn graph_routed_extended_property_delete_matches_the_batch_arm() {
    let app = reactions_router();
    let uri = format!(
        "/v1.0/me/messages/email-001/singleValueExtendedProperties/{}",
        pct_encode(OWNER_REACTION_TYPE_ID)
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(&uri)
                .header(header::HOST, "127.0.0.1:9999")
                .header(header::AUTHORIZATION, "Bearer doesnt-matter")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Idempotent-by-404: a second clear finds nothing to remove.
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(&uri)
                .header(header::HOST, "127.0.0.1:9999")
                .header(header::AUTHORIZATION, "Bearer doesnt-matter")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── reserved characters in `$batch` sub-request URLs ────────────────

/// The message id staged by `fixtures/graph-reserved-ids.toml`, in its
/// DECODED form (what the fixture declares and what the mock must
/// resolve against).
const RESERVED_MESSAGE_ID: &str = "AAMkAGI2+9/xZ0=";

/// The shared-mailbox owner (`shared@contoso.com`) and that message id
/// as bifrost puts them on the wire.
/// `bifrost_net::url::encode_component` escapes `@` -> `%40`,
/// `+` -> `%2B`, `/` -> `%2F`, `=` -> `%3D`.
const SHARED_OWNER_ENC: &str = "shared%40contoso.com";
const RESERVED_MESSAGE_ID_ENC: &str = "AAMkAGI2%2B9%2FxZ0%3D";

fn reserved_ids_router() -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/graph-reserved-ids.toml")).unwrap();
    graph::router(graph::AppState::for_test(saehrimnir::shared::handle(fix)))
}

async fn post_batch(app: axum::Router, body: &Value) -> (StatusCode, Value) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1.0/$batch")
                .header(header::HOST, "127.0.0.1:9999")
                .header(header::AUTHORIZATION, "Bearer doesnt-matter")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// A `$batch` sub-request URL is a JSON string bifrost builds with
/// `bifrost_net::url::encode_component`, so the `/users/{owner}`
/// segment and the message id both arrive percent-encoded. The mock
/// parses those URLs by hand; without decoding, a shared-mailbox
/// hydration GET misses `Fixture::account` and 404s on an account the
/// fixture declares.
///
/// Every assertion here is on a per-item `status`, not on the envelope:
/// the envelope is 200 even when every sub-request inside it failed,
/// which is precisely how this class of bug hides.
#[tokio::test]
async fn graph_batch_decodes_percent_encoded_owner_and_message_id() {
    let q = reactions_collection_query();
    let body = json!({
        "requests": [
            // Hydration GET, both segments encoded.
            { "id": "1", "method": "GET",
              "url": format!("/users/{SHARED_OWNER_ENC}/messages/{RESERVED_MESSAGE_ID_ENC}?$select=id,subject") },
            // The reaction refresh's nested-collection GET. The encoded
            // `/` inside the id must NOT be read as a segment boundary
            // (decode-then-split would route this at `xZ0=` instead).
            { "id": "2", "method": "GET",
              "url": format!("/users/{SHARED_OWNER_ENC}/messages/{RESERVED_MESSAGE_ID_ENC}/singleValueExtendedProperties?{q}") },
            // Same id through `/me`: still a per-item miss. Decoding
            // must not leak the shared mailbox into the primary scope.
            { "id": "3", "method": "GET",
              "url": format!("/me/messages/{RESERVED_MESSAGE_ID_ENC}") },
            // An encoded owner that resolves to no declared account is
            // a ResourceNotFound, distinct from an unknown message.
            { "id": "4", "method": "GET",
              "url": format!("/users/nobody%40contoso.com/messages/{RESERVED_MESSAGE_ID_ENC}") },
            // The `/v1.0`-prefixed spelling decodes identically.
            { "id": "5", "method": "GET",
              "url": format!("/v1.0/users/{SHARED_OWNER_ENC}/messages/{RESERVED_MESSAGE_ID_ENC}") },
        ]
    });
    let (status, v) = post_batch(reserved_ids_router(), &body).await;
    assert_eq!(status, StatusCode::OK);
    let responses = v["responses"].as_array().unwrap();
    assert_eq!(responses.len(), 5);
    let by_id = |id: &str| responses.iter().find(|r| r["id"] == id).unwrap();

    let r1 = by_id("1");
    assert_eq!(r1["status"], 200, "{r1}");
    assert_eq!(r1["body"]["id"], RESERVED_MESSAGE_ID);
    assert_eq!(r1["body"]["subject"], "Hello shared");
    // The etag derives from the DECODED id, which is the value bifrost
    // will hand straight back as an `If-Match` on the next mutation.
    assert_eq!(
        r1["body"]["@odata.etag"],
        format!("W/\"etag-{RESERVED_MESSAGE_ID}\"")
    );

    let r2 = by_id("2");
    assert_eq!(r2["status"], 200, "{r2}");
    assert_eq!(
        collection_prop(&r2["body"], OWNER_REACTION_TYPE_ID).unwrap()["value"],
        "like"
    );
    assert_eq!(
        collection_prop(&r2["body"], REACTIONS_COUNT_ID).unwrap()["value"],
        "2"
    );

    let r3 = by_id("3");
    assert_eq!(r3["status"], 404, "{r3}");
    assert_eq!(r3["body"]["error"]["code"], "ErrorItemNotFound");

    let r4 = by_id("4");
    assert_eq!(r4["status"], 404, "{r4}");
    assert_eq!(r4["body"]["error"]["code"], "ResourceNotFound");

    let r5 = by_id("5");
    assert_eq!(r5["status"], 200, "{r5}");
    assert_eq!(r5["body"]["id"], RESERVED_MESSAGE_ID);
}

/// The write half. bifrost routes PATCH / POST `.../move` through the
/// same encoded URLs, gated on an `If-Match` it read from the encoded
/// id's projection - so a decode bug shows up as a 404 (or, worse for
/// `move`, as a 501 from the `{id}/move` split landing on the wrong
/// boundary) rather than a mutation.
#[tokio::test]
async fn graph_batch_writes_route_through_percent_encoded_ids() {
    let app = reserved_ids_router();
    let etag = format!("W/\"etag-{RESERVED_MESSAGE_ID}\"");
    let body = json!({
        "requests": [
            { "id": "1", "method": "PATCH",
              "url": format!("/users/{SHARED_OWNER_ENC}/messages/{RESERVED_MESSAGE_ID_ENC}"),
              "headers": { "If-Match": etag },
              "body": { "isRead": true } },
            { "id": "2", "method": "POST",
              "url": format!("/users/{SHARED_OWNER_ENC}/messages/{RESERVED_MESSAGE_ID_ENC}/move"),
              "headers": { "If-Match": etag },
              "body": { "destinationId": "mbx-shared-archive" } },
        ]
    });
    let (status, v) = post_batch(app.clone(), &body).await;
    assert_eq!(status, StatusCode::OK);
    let responses = v["responses"].as_array().unwrap();
    let by_id = |id: &str| responses.iter().find(|r| r["id"] == id).unwrap();
    assert_eq!(by_id("1")["status"], 200, "{}", by_id("1"));
    assert_eq!(by_id("1")["body"]["isRead"], true);
    assert_eq!(by_id("2")["status"], 201, "{}", by_id("2"));

    // Persisted, and visible on the direct (axum-decoded) route too -
    // the hand-rolled `$batch` parser and axum's `Path` extractor must
    // agree on what the id is.
    let (status, v) = get_json_with(
        app,
        &format!("/v1.0/users/{SHARED_OWNER_ENC}/messages/{RESERVED_MESSAGE_ID_ENC}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["id"], RESERVED_MESSAGE_ID);
    assert_eq!(v["isRead"], true);
    assert_eq!(v["parentFolderId"], "mbx-shared-archive");
}
