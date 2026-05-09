#![allow(clippy::unwrap_used)]

//! End-to-end tests that drive the router via `tower::ServiceExt::oneshot`
//! without binding a TCP port. Faster than spawning the binary and
//! sufficient for verifying the wire format - the subprocess + sentinel
//! + SIGTERM path is exercised by `scripts/smoke.sh`.

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use saehrimnir::{fixture, lua, routes};

fn router() -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/jmap-small.toml")).unwrap();
    routes::router(routes::AppState {
        fixture: Arc::new(fix),
        dispatcher: None,
        submission_log: saehrimnir::smtp::SubmissionLog::default(),
        request_log: saehrimnir::request_log::RequestLog::default(),
        token_store: saehrimnir::oauth::TokenStore::default(),
        base_url: "http://localhost".into(),
    })
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn jmap_call(method: &str, args: Value, call_id: &str) -> Value {
    let req_body = json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
        "methodCalls": [[method, args, call_id]],
    });
    let resp = router()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/jmap/api")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&req_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await
}

#[tokio::test]
async fn session_resource_advertises_core_and_mail_only() {
    let resp = router()
        .oneshot(
            Request::builder()
                .uri("/jmap/session")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let caps = v.get("capabilities").unwrap().as_object().unwrap();
    assert!(caps.contains_key("urn:ietf:params:jmap:core"));
    assert!(caps.contains_key("urn:ietf:params:jmap:mail"));
    // Plan: must NOT advertise principals or the client takes
    // shared-account / Principal/get paths the mock cannot satisfy.
    assert!(!caps.contains_key("urn:ietf:params:jmap:principals"));

    let accounts = v.get("accounts").unwrap().as_object().unwrap();
    assert_eq!(accounts.len(), 1);
    let acct = accounts.get("account-1").unwrap();
    assert_eq!(acct.get("isPersonal").unwrap(), true);
}

#[tokio::test]
async fn well_known_jmap_matches_session() {
    let r = router();
    let s = r
        .clone()
        .oneshot(
            Request::builder()
                .uri("/jmap/session")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let w = r
        .oneshot(
            Request::builder()
                .uri("/.well-known/jmap")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let s = body_json(s).await;
    let w = body_json(w).await;
    assert_eq!(s, w);
}

#[tokio::test]
async fn mailbox_changes_with_matching_state_returns_empty_delta() {
    let v = jmap_call(
        "Mailbox/changes",
        json!({"accountId": "account-1", "sinceState": "fixture-state"}),
        "c0",
    )
    .await;
    let mr = v.get("methodResponses").unwrap().as_array().unwrap();
    assert_eq!(mr[0][0], "Mailbox/changes");
    let body = &mr[0][1];
    assert_eq!(body["accountId"], "account-1");
    assert_eq!(body["oldState"], "fixture-state");
    assert_eq!(body["newState"], "fixture-state");
    assert_eq!(body["hasMoreChanges"], false);
    assert_eq!(body["created"], json!([]));
    assert_eq!(body["updated"], json!([]));
    assert_eq!(body["destroyed"], json!([]));
    // Mailbox/changes does NOT carry updatedProperties.
    assert!(body.get("updatedProperties").is_none());
}

#[tokio::test]
async fn mailbox_changes_with_unknown_state_returns_cannot_calculate() {
    let v = jmap_call(
        "Mailbox/changes",
        json!({"accountId": "account-1", "sinceState": "stale"}),
        "c0",
    )
    .await;
    let mr = v.get("methodResponses").unwrap().as_array().unwrap();
    assert_eq!(mr[0][0], "error");
    assert_eq!(mr[0][1]["type"], "cannotCalculateChanges");
}

#[tokio::test]
async fn email_changes_with_matching_state_returns_empty_delta_with_updated_properties_null() {
    let v = jmap_call(
        "Email/changes",
        json!({"accountId": "account-1", "sinceState": "fixture-state"}),
        "c0",
    )
    .await;
    let mr = v.get("methodResponses").unwrap().as_array().unwrap();
    assert_eq!(mr[0][0], "Email/changes");
    let body = &mr[0][1];
    assert_eq!(body["newState"], "fixture-state");
    assert_eq!(body["created"], json!([]));
    assert_eq!(body["updated"], json!([]));
    assert_eq!(body["destroyed"], json!([]));
    assert!(body["updatedProperties"].is_null());
}

#[tokio::test]
async fn email_changes_with_unknown_state_returns_cannot_calculate() {
    let v = jmap_call(
        "Email/changes",
        json!({"accountId": "account-1", "sinceState": "old"}),
        "c0",
    )
    .await;
    let mr = v.get("methodResponses").unwrap().as_array().unwrap();
    assert_eq!(mr[0][0], "error");
    assert_eq!(mr[0][1]["type"], "cannotCalculateChanges");
}

#[tokio::test]
async fn changes_methods_validate_account_and_since_state() {
    // Missing sinceState.
    let v = jmap_call("Email/changes", json!({"accountId": "account-1"}), "c0").await;
    assert_eq!(v["methodResponses"][0][0], "error");
    assert_eq!(v["methodResponses"][0][1]["type"], "invalidArguments");

    // Wrong account.
    let v = jmap_call(
        "Email/changes",
        json!({"accountId": "ghost", "sinceState": "fixture-state"}),
        "c0",
    )
    .await;
    assert_eq!(v["methodResponses"][0][0], "error");
    assert_eq!(v["methodResponses"][0][1]["type"], "accountNotFound");
}

#[tokio::test]
async fn mailbox_get_returns_fixture_mailboxes_in_order() {
    let v = jmap_call("Mailbox/get", json!({"accountId": "account-1"}), "c0").await;
    let mr = v.get("methodResponses").unwrap().as_array().unwrap();
    assert_eq!(mr[0][0], "Mailbox/get");
    assert_eq!(mr[0][2], "c0");
    let list = mr[0][1].get("list").unwrap().as_array().unwrap();
    let ids: Vec<&str> = list.iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["mbx-inbox", "mbx-archive"]);

    let inbox = &list[0];
    assert_eq!(inbox.get("totalEmails").unwrap(), 2);
    // Both fixture emails are unread.
    assert_eq!(inbox.get("unreadEmails").unwrap(), 2);
    let rights = inbox.get("myRights").unwrap().as_object().unwrap();
    for k in ["mayReadItems", "mayAddItems", "mayDelete", "maySubmit"] {
        assert_eq!(rights.get(k).unwrap(), true, "{k}");
    }
}

#[tokio::test]
async fn email_query_initial_sync_shape() {
    // Mirrors ratatoskr's first page: filter `after`, calculateTotal.
    let v = jmap_call(
        "Email/query",
        json!({
            "accountId": "account-1",
            "filter": {"after": 0},
            "sort": [{"property": "receivedAt"}],
            "position": 0,
            "limit": 50,
            "calculateTotal": true,
        }),
        "q0",
    )
    .await;
    let result = &v["methodResponses"][0][1];
    let ids: Vec<&str> = result["ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    // email-002 is newer (11:00 vs 10:00) so it lands first.
    assert_eq!(ids, vec!["email-002", "email-001"]);
    assert_eq!(result["total"].as_u64().unwrap(), 2);
    assert_eq!(result["canCalculateChanges"], false);
    assert_eq!(result["queryState"], "fixture-state");
}

#[tokio::test]
async fn email_query_pagination_terminates_below_limit() {
    let v = jmap_call(
        "Email/query",
        json!({"accountId": "account-1", "limit": 1}),
        "q0",
    )
    .await;
    let result = &v["methodResponses"][0][1];
    let ids = result["ids"].as_array().unwrap();
    assert_eq!(ids.len(), 1);

    // Position past total returns empty without erroring.
    let v2 = jmap_call(
        "Email/query",
        json!({"accountId": "account-1", "position": 99}),
        "q1",
    )
    .await;
    assert!(v2["methodResponses"][0][1]["ids"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn email_get_full_email_shape_with_body_values() {
    let v = jmap_call(
        "Email/get",
        json!({
            "accountId": "account-1",
            "ids": ["email-001"],
            "fetchTextBodyValues": true,
            "fetchHtmlBodyValues": true,
        }),
        "g0",
    )
    .await;
    let result = &v["methodResponses"][0][1];
    let item = &result["list"][0];
    assert_eq!(item["id"], "email-001");
    assert_eq!(item["blobId"], "blob-email-001");
    // mailboxIds + keywords are bool maps, not arrays.
    assert_eq!(item["mailboxIds"], json!({"mbx-inbox": true}));
    let received = item["receivedAt"].as_str().expect("receivedAt is a UTCDate string");
    assert!(
        chrono::DateTime::parse_from_rfc3339(received).is_ok(),
        "receivedAt {received:?} is RFC3339",
    );
    let from = item["from"].as_array().unwrap();
    assert_eq!(from[0]["email"], "alice@example.com");

    let part_id = item["textBody"][0]["partId"].as_str().unwrap();
    assert_eq!(
        item["bodyValues"][part_id]["value"], "First message body."
    );
    // The three custom-header keys ratatoskr asks for are always present.
    for k in [
        "header:List-Unsubscribe:asText",
        "header:List-Unsubscribe-Post:asText",
        "header:Disposition-Notification-To:asText",
    ] {
        assert!(item.get(k).is_some(), "{k} missing");
    }
}

fn attach_router() -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/jmap-attach.toml")).unwrap();
    routes::router(routes::AppState {
        fixture: Arc::new(fix),
        dispatcher: None,
        submission_log: saehrimnir::smtp::SubmissionLog::default(),
        request_log: saehrimnir::request_log::RequestLog::default(),
        token_store: saehrimnir::oauth::TokenStore::default(),
        base_url: "http://localhost".into(),
    })
}

async fn attach_jmap_call(method: &str, args: Value, call_id: &str) -> Value {
    let req_body = json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
        "methodCalls": [[method, args, call_id]],
    });
    let resp = attach_router()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/jmap/api")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&req_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await
}

#[tokio::test]
async fn email_get_surfaces_attachments_array() {
    let v = attach_jmap_call(
        "Email/get",
        json!({
            "accountId": "account-1",
            "ids": ["email-001"],
        }),
        "g0",
    )
    .await;
    let item = &v["methodResponses"][0][1]["list"][0];
    assert_eq!(item["hasAttachment"], true);
    let atts = item["attachments"].as_array().unwrap();
    assert_eq!(atts.len(), 1);
    let att = &atts[0];
    assert_eq!(att["blobId"], "blob-att-001");
    assert_eq!(att["name"], "sample.txt");
    assert_eq!(att["type"], "text/plain");
    assert_eq!(att["disposition"], "attachment");
    assert_eq!(att["isInline"], false);
    assert_eq!(att["partId"], "email-001:att-1");
    assert!(att["size"].is_i64());
}

#[tokio::test]
async fn jmap_download_returns_blob_bytes() {
    let resp = attach_router()
        .oneshot(
            Request::builder()
                .uri("/jmap/download/account-1/blob-att-001/sample.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/plain"
    );
    let cd = resp
        .headers()
        .get(header::CONTENT_DISPOSITION)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(cd.starts_with("attachment; filename*=UTF-8''sample.txt"));
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(bytes.starts_with(b"attachment payload"));
}

#[tokio::test]
async fn jmap_download_unknown_blob_returns_404_envelope() {
    let resp = attach_router()
        .oneshot(
            Request::builder()
                .uri("/jmap/download/account-1/blob-nonsense/x")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let v = body_json(resp).await;
    assert!(v["type"].as_str().unwrap().contains("notFound"));
}

#[tokio::test]
async fn email_get_empty_ids_returns_state_only() {
    // get_email_state path: ids=[] purely to read the state token.
    let v = jmap_call(
        "Email/get",
        json!({"accountId": "account-1", "ids": []}),
        "g1",
    )
    .await;
    let result = &v["methodResponses"][0][1];
    assert_eq!(result["state"], "fixture-state");
    assert!(result["list"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn unknown_account_lands_in_response_not_http() {
    let v = jmap_call("Mailbox/get", json!({"accountId": "ghost"}), "c0").await;
    let entry = &v["methodResponses"][0];
    assert_eq!(entry[0], "error");
    assert_eq!(entry[1]["type"], "accountNotFound");
}

#[tokio::test]
async fn unknown_method_lands_in_response_not_http() {
    let v = jmap_call("Email/import", json!({}), "c0").await;
    let entry = &v["methodResponses"][0];
    assert_eq!(entry[0], "error");
    assert_eq!(entry[1]["type"], "unknownMethod");
}

#[tokio::test]
async fn malformed_json_yields_400() {
    // Plain HTTP 400 (axum's Json extractor) - the only non-200 path.
    let resp = router()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/jmap/api")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("not-json"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn full_initial_sync_dispatch_in_a_single_request() {
    // Mirrors what ratatoskr's initial sync does in batched form: hits
    // every load-bearing method in one envelope and verifies they all
    // came back wired correctly.
    let req_body = json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
        "methodCalls": [
            ["Mailbox/get", {"accountId": "account-1"}, "0"],
            ["Email/query", {"accountId": "account-1", "calculateTotal": true}, "1"],
            ["Email/get", {
                "accountId": "account-1",
                "ids": ["email-001", "email-002"],
                "fetchTextBodyValues": true,
            }, "2"],
            ["Email/get", {"accountId": "account-1", "ids": []}, "3"],
        ],
    });
    let resp = router()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/jmap/api")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&req_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let v = body_json(resp).await;
    let mr = v["methodResponses"].as_array().unwrap();
    assert_eq!(mr.len(), 4);
    assert_eq!(mr[0][0], "Mailbox/get");
    assert_eq!(mr[1][0], "Email/query");
    assert_eq!(mr[2][0], "Email/get");
    assert_eq!(mr[3][0], "Email/get");
    // Each response carries its caller's callId.
    for (i, expected) in ["0", "1", "2", "3"].iter().enumerate() {
        assert_eq!(mr[i][2], *expected);
    }
    assert_eq!(v["sessionState"], "fixture-state");
    assert_eq!(mr[3][1]["state"], "fixture-state");
}

#[tokio::test]
async fn responses_are_byte_identical_across_runs() {
    // The determinism contract: same fixture in -> same bytes out. Run
    // a non-trivial method twice and compare raw bytes.
    let payload = json!({
        "using": [],
        "methodCalls": [
            ["Email/query", {"accountId": "account-1", "calculateTotal": true}, "0"],
            ["Mailbox/get", {"accountId": "account-1"}, "1"],
        ],
    });
    let bytes1 = router()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/jmap/api")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let bytes2 = router()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/jmap/api")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(bytes1, bytes2);
}

// ── Reactive-callback tests ────────────────────────────────────────

fn router_with_lua_scenario(scenario: &str) -> axum::Router {
    let (fixture, dispatcher) =
        lua::load_source_with_dispatcher(scenario, "@cb-test").unwrap();
    routes::router(routes::AppState {
        fixture: Arc::new(fixture),
        dispatcher: Some(Arc::new(dispatcher)),
        submission_log: saehrimnir::smtp::SubmissionLog::default(),
        request_log: saehrimnir::request_log::RequestLog::default(),
        token_store: saehrimnir::oauth::TokenStore::default(),
        base_url: "http://localhost".into(),
    })
}

async fn post_jmap(router: axum::Router, body: Value) -> Value {
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/jmap/api")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await
}

#[tokio::test]
async fn jmap_email_get_callback_overrides_with_method_error() {
    let scenario = r#"
        fixture({ name = "cb" })
        account({ id = "account-1", name = "test@example.com" })
        mailbox({ id = "mb", name = "Inbox", role = "inbox" })
        email({
            id = "e1",
            mailbox_ids = {"mb"},
            received_at = "2026-01-15T10:00:00Z",
            body_text = "x",
        })
        on("jmap", "Email/get", function(req)
            -- Pass the accountId through to verify it landed in req.
            return { status = "serverFail", message = "acct=" .. req.account_id }
        end)
    "#;
    let router = router_with_lua_scenario(scenario);
    let v = post_jmap(
        router,
        json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "methodCalls": [["Email/get", {"accountId": "account-1", "ids": ["e1"]}, "c0"]],
        }),
    )
    .await;
    let entry = &v["methodResponses"][0];
    assert_eq!(entry[0], "error");
    assert_eq!(entry[1]["type"], "serverFail");
    assert_eq!(entry[1]["description"], "acct=account-1");
    assert_eq!(entry[2], "c0");
}

#[tokio::test]
async fn jmap_callback_call_index_increments_per_method() {
    // call_index counts per (protocol, command), so two Email/get
    // calls plus an Email/query call should give Email/get a count
    // of 1 then 2, while Email/query stays at 1.
    let scenario = r#"
        fixture({ name = "ix" })
        account({ id = "account-1", name = "test@example.com" })
        mailbox({ id = "mb", name = "Inbox", role = "inbox" })
        on("jmap", "Email/get", function(req)
            return { status = "EG", message = tostring(req.call_index) }
        end)
        on("jmap", "Email/query", function(req)
            return { status = "EQ", message = tostring(req.call_index) }
        end)
    "#;
    let router = router_with_lua_scenario(scenario);
    let v = post_jmap(
        router,
        json!({
            "using": [],
            "methodCalls": [
                ["Email/get",   {"accountId": "account-1", "ids": []}, "a"],
                ["Email/query", {"accountId": "account-1"},            "b"],
                ["Email/get",   {"accountId": "account-1", "ids": []}, "c"],
            ],
        }),
    )
    .await;
    let mr = v["methodResponses"].as_array().unwrap();
    assert_eq!(mr[0][1]["type"], "EG");
    assert_eq!(mr[0][1]["description"], "1");
    assert_eq!(mr[1][1]["type"], "EQ");
    assert_eq!(mr[1][1]["description"], "1");
    assert_eq!(mr[2][1]["type"], "EG");
    assert_eq!(mr[2][1]["description"], "2");
}

#[tokio::test]
async fn jmap_callback_nil_return_passes_through() {
    let scenario = r#"
        fixture({ name = "passthrough" })
        account({ id = "account-1", name = "test@example.com" })
        mailbox({ id = "mb", name = "Inbox", role = "inbox" })
        email({
            id = "e1",
            mailbox_ids = {"mb"},
            received_at = "2026-01-15T10:00:00Z",
            body_text = "hi",
        })
        on("jmap", "Email/get", function(req)
            return nil
        end)
    "#;
    let router = router_with_lua_scenario(scenario);
    let v = post_jmap(
        router,
        json!({
            "using": [],
            "methodCalls": [["Email/get", {"accountId": "account-1", "ids": ["e1"]}, "c"]],
        }),
    )
    .await;
    let entry = &v["methodResponses"][0];
    // No override - method runs normally, returns Email/get result.
    assert_eq!(entry[0], "Email/get");
    let item = &entry[1]["list"][0];
    assert_eq!(item["id"], "e1");
}

#[tokio::test]
async fn jmap_email_get_callback_sees_ids_as_lua_array() {
    // `req.ids` arrives as a 1-based Lua array of strings, populated
    // from the request's `ids[]`. Concatenating the entries with
    // table.concat verifies both the shape (table) and the order.
    let scenario = r#"
        fixture({ name = "ids" })
        account({ id = "account-1", name = "test@example.com" })
        mailbox({ id = "mb", name = "Inbox", role = "inbox" })
        on("jmap", "Email/get", function(req)
            local joined = table.concat(req.ids, ",")
            return {
                status = "serverFail",
                message = "n=" .. #req.ids .. " ids=" .. joined,
            }
        end)
    "#;
    let router = router_with_lua_scenario(scenario);
    let v = post_jmap(
        router,
        json!({
            "using": [],
            "methodCalls": [[
                "Email/get",
                {"accountId": "account-1", "ids": ["e1", "e2", "e3"]},
                "c0",
            ]],
        }),
    )
    .await;
    let entry = &v["methodResponses"][0];
    assert_eq!(entry[0], "error");
    assert_eq!(entry[1]["type"], "serverFail");
    assert_eq!(entry[1]["description"], "n=3 ids=e1,e2,e3");
}

#[tokio::test]
async fn jmap_callback_ids_absent_when_request_omits_them() {
    // Mailbox/get with a missing `ids` (means "all") should not
    // surface `req.ids` at all - the script can rely on `req.ids
    // == nil` as the signal that the call requests every entry.
    let scenario = r#"
        fixture({ name = "noids" })
        account({ id = "account-1", name = "test@example.com" })
        mailbox({ id = "mb", name = "Inbox", role = "inbox" })
        on("jmap", "Mailbox/get", function(req)
            local present = req.ids ~= nil
            return {
                status = "serverFail",
                message = "present=" .. tostring(present),
            }
        end)
    "#;
    let router = router_with_lua_scenario(scenario);
    let v = post_jmap(
        router,
        json!({
            "using": [],
            "methodCalls": [["Mailbox/get", {"accountId": "account-1"}, "c0"]],
        }),
    )
    .await;
    let entry = &v["methodResponses"][0];
    assert_eq!(entry[1]["description"], "present=false");
}

// ── /test/smtp/submissions ─────────────────────────────────────────

fn router_with_smtp_log(log: saehrimnir::smtp::SubmissionLog) -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/jmap-small.toml")).unwrap();
    routes::router(routes::AppState {
        fixture: Arc::new(fix),
        dispatcher: None,
        submission_log: log,
        request_log: saehrimnir::request_log::RequestLog::default(),
        token_store: saehrimnir::oauth::TokenStore::default(),
        base_url: "http://localhost".into(),
    })
}

fn sample_submission(from: &str, attachment_size: usize) -> saehrimnir::smtp::Submission {
    let body = format!(
        "From: <{from}>\r\n\
         To: <to@example.com>\r\n\
         Subject: hello\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"BOUND\"\r\n\
         \r\n\
         --BOUND\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         \r\n\
         body text\r\n\
         --BOUND\r\n\
         Content-Type: application/pdf\r\n\
         Content-Disposition: attachment; filename=\"big.pdf\"\r\n\
         Content-Transfer-Encoding: base64\r\n\
         \r\n\
         {payload}\r\n\
         --BOUND--\r\n",
        payload = "A".repeat(attachment_size),
    );
    saehrimnir::smtp::Submission {
        from: from.to_string(),
        recipients: vec!["to@example.com".to_string()],
        from_params: Default::default(),
        rcpt_params: vec![Default::default()],
        auth_mechanism: Some("PLAIN".to_string()),
        data: body.into_bytes(),
        received_at: chrono::Utc::now(),
    }
}

#[tokio::test]
async fn test_smtp_submissions_returns_parsed_view() {
    let log = saehrimnir::smtp::SubmissionLog::default();
    log.push(sample_submission("alice@example.com", 64));
    let v = body_json(
        router_with_smtp_log(log)
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/test/smtp/submissions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;

    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    let s = &arr[0];
    assert_eq!(s["from"], "alice@example.com");
    assert_eq!(s["recipients"][0], "to@example.com");
    assert_eq!(s["auth_mechanism"], "PLAIN");
    assert!(s["raw_size"].as_u64().unwrap() > 64);
    let parsed = &s["parsed"];
    assert_eq!(parsed["subject"], "hello");
    // mail-parser projects a text/plain body into both text and html
    // counts; assert both fields exist as numbers but don't pin the
    // exact values - the harness scripts care about attachments.
    assert!(parsed["text_body_count"].is_number());
    assert!(parsed["html_body_count"].is_number());
    let attachments = parsed["attachments"].as_array().unwrap();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0]["filename"], "big.pdf");
    assert_eq!(attachments[0]["content_type"], "application/pdf");
    assert!(attachments[0]["size"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn test_smtp_submissions_delete_clears_log() {
    let log = saehrimnir::smtp::SubmissionLog::default();
    log.push(sample_submission("alice@example.com", 16));
    log.push(sample_submission("bob@example.com", 16));
    assert_eq!(log.snapshot().len(), 2);

    let resp = router_with_smtp_log(log.clone())
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/test/smtp/submissions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(log.snapshot().len(), 0);

    let v = body_json(
        router_with_smtp_log(log)
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/test/smtp/submissions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(v.as_array().unwrap().len(), 0);
}

// ── /test/requests + /test/fixture/{reset,step} ─────────────────────

fn router_with_logs(
    smtp_log: saehrimnir::smtp::SubmissionLog,
    request_log: saehrimnir::request_log::RequestLog,
) -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/jmap-small.toml")).unwrap();
    routes::router(routes::AppState {
        fixture: Arc::new(fix),
        dispatcher: None,
        submission_log: smtp_log,
        request_log,
        token_store: saehrimnir::oauth::TokenStore::default(),
        base_url: "http://localhost".into(),
    })
}

#[tokio::test]
async fn jmap_method_calls_land_in_request_log() {
    let request_log = saehrimnir::request_log::RequestLog::default();
    let app = router_with_logs(
        saehrimnir::smtp::SubmissionLog::default(),
        request_log.clone(),
    );

    let req = Request::builder()
        .method("POST")
        .uri("/jmap/api")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({
                "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
                "methodCalls": [
                    ["Mailbox/get", { "accountId": "account-1" }, "c0"],
                    ["Email/query", { "accountId": "account-1" }, "c1"]
                ]
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // One entry per method call, in submission order.
    let snap = request_log.snapshot();
    assert_eq!(snap.len(), 2, "{snap:?}");
    assert_eq!(snap[0].protocol, "jmap");
    assert_eq!(snap[0].command, "Mailbox/get");
    assert_eq!(snap[0].detail["call_id"], "c0");
    assert_eq!(snap[1].command, "Email/query");
    assert_eq!(snap[1].detail["call_id"], "c1");
}

#[tokio::test]
async fn test_requests_get_returns_snapshot_and_delete_clears() {
    let request_log = saehrimnir::request_log::RequestLog::default();
    request_log.record("imap", "CAPABILITY", json!({"tag": "a1"}));
    request_log.record("smtp", "EHLO", json!({"args": "client"}));

    let app =
        router_with_logs(saehrimnir::smtp::SubmissionLog::default(), request_log.clone());

    // GET returns the array.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/test/requests")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["protocol"], "imap");
    assert_eq!(arr[0]["command"], "CAPABILITY");
    assert_eq!(arr[1]["protocol"], "smtp");

    // DELETE clears.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/test/requests")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(request_log.is_empty());
}

#[tokio::test]
async fn test_fixture_reset_clears_both_logs() {
    let smtp_log = saehrimnir::smtp::SubmissionLog::default();
    smtp_log.push(sample_submission("alice@example.com", 16));
    let request_log = saehrimnir::request_log::RequestLog::default();
    request_log.record("imap", "CAPABILITY", json!({}));

    let app = router_with_logs(smtp_log.clone(), request_log.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/test/fixture/reset")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(smtp_log.snapshot().len(), 0);
    assert!(request_log.is_empty());
}

#[tokio::test]
async fn test_fixture_step_returns_501_until_change_scripts_land() {
    let app = router_with_logs(
        saehrimnir::smtp::SubmissionLog::default(),
        saehrimnir::request_log::RequestLog::default(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/test/fixture/step")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    let v = body_json(resp).await;
    assert_eq!(v["error"], "fixture step not implemented");
    assert!(v["detail"].as_str().unwrap().contains("[[change]]"));
}

// ── /oauth/* + /test/oauth/invalidate ───────────────────────────────

fn router_with_token_store(store: saehrimnir::oauth::TokenStore) -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/jmap-small.toml")).unwrap();
    routes::router(routes::AppState {
        fixture: Arc::new(fix),
        dispatcher: None,
        submission_log: saehrimnir::smtp::SubmissionLog::default(),
        request_log: saehrimnir::request_log::RequestLog::default(),
        token_store: store,
        base_url: "http://localhost".into(),
    })
}

#[tokio::test]
async fn oauth_token_authorization_code_grant_mints_active_token() {
    let store = saehrimnir::oauth::TokenStore::default();
    let app = router_with_token_store(store.clone());

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/oauth/token")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(
                    "grant_type=authorization_code\
                    &code=fixture-code\
                    &client_id=test\
                    &client_secret=secret\
                    &redirect_uri=http://localhost/cb",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let access = v["access_token"].as_str().unwrap();
    let refresh = v["refresh_token"].as_str().unwrap();
    assert_eq!(v["token_type"], "Bearer");
    assert_eq!(v["expires_in"], 3600);
    assert!(access.starts_with("mock-access-"));
    assert_ne!(access, refresh);

    // Both tokens are registered in the store.
    assert!(store.is_active(access));
    assert!(store.is_active(refresh));
}

#[tokio::test]
async fn oauth_token_refresh_grant_works_via_json_body() {
    let store = saehrimnir::oauth::TokenStore::default();
    let app = router_with_token_store(store.clone());

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/oauth/token")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "grant_type": "refresh_token",
                        "refresh_token": "rt-abc",
                        "client_id": "test"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert!(v["access_token"].as_str().unwrap().starts_with("mock-access-"));
}

#[tokio::test]
async fn oauth_token_rejects_unsupported_grant_type() {
    let store = saehrimnir::oauth::TokenStore::default();
    let app = router_with_token_store(store);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/oauth/token")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("grant_type=password"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_json(resp).await;
    assert_eq!(v["error"], "unsupported_grant_type");
}

#[tokio::test]
async fn oauth_userinfo_returns_account_claims_with_active_token() {
    let store = saehrimnir::oauth::TokenStore::default();
    let token = store.mint("authorization_code", 0xdead_beef);

    let app = router_with_token_store(store);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/oauth/userinfo")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["sub"], "account-1");
    assert_eq!(v["email"], "test@example.com");
    assert_eq!(v["email_verified"], true);
    assert_eq!(v["name"], "test@example.com");
    assert_eq!(v["iss"], "https://saehrimnir.test/oauth");
}

#[tokio::test]
async fn oauth_userinfo_rejects_unknown_token() {
    let app = router_with_token_store(saehrimnir::oauth::TokenStore::default());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/oauth/userinfo")
                .header(header::AUTHORIZATION, "Bearer nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let v = body_json(resp).await;
    assert_eq!(v["error"], "invalid_token");
}

#[tokio::test]
async fn test_oauth_invalidate_drops_token_from_store() {
    let store = saehrimnir::oauth::TokenStore::default();
    let token = store.mint("authorization_code", 1);
    assert!(store.is_active(&token));

    let app = router_with_token_store(store.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/test/oauth/invalidate")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({"token": token}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(!store.is_active(&token));
}

#[tokio::test]
async fn test_oauth_invalidate_unknown_token_is_404() {
    let app = router_with_token_store(saehrimnir::oauth::TokenStore::default());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/test/oauth/invalidate")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({"token": "ghost"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn fixture_reset_clears_token_store_too() {
    let store = saehrimnir::oauth::TokenStore::default();
    let _ = store.mint("authorization_code", 1);
    assert_eq!(store.active_count(), 1);

    let app = router_with_token_store(store.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/test/fixture/reset")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(store.active_count(), 0);
}

// ── Bearer enforcement ──────────────────────────────────────────────

fn router_with_enforce(store: saehrimnir::oauth::TokenStore) -> axum::Router {
    use saehrimnir::fixture::OAuthConfig;
    let mut fix = fixture::load(std::path::Path::new("fixtures/jmap-small.toml")).unwrap();
    fix.oauth = OAuthConfig {
        enforce: true,
        issuer: "https://saehrimnir.test/oauth".to_string(),
    };
    routes::router(routes::AppState {
        fixture: Arc::new(fix),
        dispatcher: None,
        submission_log: saehrimnir::smtp::SubmissionLog::default(),
        request_log: saehrimnir::request_log::RequestLog::default(),
        token_store: store,
        base_url: "http://localhost".into(),
    })
}

#[tokio::test]
async fn jmap_session_enforces_bearer_when_fixture_oauth_enforce_is_true() {
    let store = saehrimnir::oauth::TokenStore::default();
    let app = router_with_enforce(store.clone());
    // No header.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/jmap/session")
                .header(header::HOST, "127.0.0.1:9999")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let v = body_json(resp).await;
    assert_eq!(v["status"], 401);
    assert_eq!(v["type"], "urn:ietf:params:jmap:error:forbidden");

    // With a valid token.
    let token = store.mint("authorization_code", 1);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/jmap/session")
                .header(header::HOST, "127.0.0.1:9999")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
