#![allow(clippy::unwrap_used)]

//! Integration tests for `POST /test/fixture/step` and the
//! `change(...)` Lua builder. Exercises the new + change + delete +
//! move trio against `fixtures/jmap-incremental.lua`, asserting that
//! each step's mutation surfaces in the JMAP `Email/changes` delta
//! path with the right RFC 8620 §5.2 dominance.

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use saehrimnir::{imap, lua, routes};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

/// Build a router for the incremental fixture, threading the same
/// `SharedHandles` into the AppState so a single instance survives
/// across calls in one test.
fn router() -> (axum::Router, saehrimnir::shared::FixtureHandle) {
    let path = std::path::Path::new("fixtures/jmap-incremental.lua");
    let source = std::fs::read_to_string(path).unwrap();
    let chunk = format!("@{}", path.display());
    let fix = lua::load_source_with_dir(&source, &chunk, path.parent().unwrap()).unwrap();
    let handle = saehrimnir::shared::handle(fix);
    let router = routes::router(routes::AppState::for_test(std::sync::Arc::clone(&handle)));
    (router, handle)
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn jmap_call(router: &axum::Router, method: &str, args: Value, call_id: &str) -> Value {
    let req_body = json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
        "methodCalls": [[method, args, call_id]],
    });
    let resp = router
        .clone()
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

async fn step(router: &axum::Router, body: Value) -> (StatusCode, Value) {
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/test/fixture/step")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = body_json(resp).await;
    (status, body)
}

async fn current_state(router: &axum::Router) -> String {
    let resp = jmap_call(
        router,
        "Mailbox/get",
        json!({ "accountId": "account-1", "ids": Value::Null }),
        "c0",
    )
    .await;
    resp["methodResponses"][0][1]["state"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn email_changes(router: &axum::Router, since: &str) -> Value {
    let resp = jmap_call(
        router,
        "Email/changes",
        json!({ "accountId": "account-1", "sinceState": since }),
        "ec",
    )
    .await;
    resp["methodResponses"][0][1].clone()
}

#[tokio::test]
async fn fixture_step_walks_new_change_delete_move_through_email_changes() {
    let (app, _handle) = router();

    // Baseline: 2 emails, fixture state is "inc-0".
    let s0 = current_state(&app).await;
    assert_eq!(s0, "inc-0");

    // Step 1: new email arrives.
    let (status, v) = step(&app, json!({ "expect": "new" })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["ok"], true);
    assert_eq!(v["step"], "new");
    assert_eq!(v["applied"], 1);
    assert_eq!(v["fixture"], "jmap-incremental");
    assert_eq!(
        v["changes"]["emails"]["created"],
        json!(["email-003"]),
        "step response should advertise the create"
    );
    assert_eq!(v["changes"]["emails"]["updated"], json!([]));
    assert_eq!(v["changes"]["emails"]["destroyed"], json!([]));
    assert_eq!(v["changes"]["emails"]["moved"], json!([]));
    let s1 = v["state"].as_str().unwrap().to_string();
    assert_ne!(s1, s0, "state token must advance after a real mutation");

    // The same delta surfaces through `Email/changes` from S0 -> now.
    let changes = email_changes(&app, &s0).await;
    assert_eq!(changes["created"], json!(["email-003"]));
    assert_eq!(changes["updated"], json!([]));
    assert_eq!(changes["destroyed"], json!([]));
    assert_eq!(changes["oldState"], json!(s0));
    assert_eq!(changes["newState"], json!(s1));

    // Step 2: existing email gets flagged.
    let (status, v) = step(&app, json!({ "expect": "change" })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["step"], "change");
    assert_eq!(v["changes"]["emails"]["updated"], json!(["email-002"]));
    let s2 = v["state"].as_str().unwrap().to_string();

    // Email/changes from S1 collapses to a single update on email-002.
    let changes = email_changes(&app, &s1).await;
    assert_eq!(changes["created"], json!([]));
    assert_eq!(changes["updated"], json!(["email-002"]));
    assert_eq!(changes["destroyed"], json!([]));

    // Step 3: an existing email is destroyed.
    let (status, v) = step(&app, json!({ "expect": "delete" })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["step"], "delete");
    assert_eq!(v["changes"]["emails"]["destroyed"], json!(["email-001"]));
    let s3 = v["state"].as_str().unwrap().to_string();

    let changes = email_changes(&app, &s2).await;
    assert_eq!(changes["destroyed"], json!(["email-001"]));

    // Step 4: a move surfaces under `email_updated` AND under
    // `changes.emails.moved`.
    let (status, v) = step(&app, json!({ "expect": "move" })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["step"], "move");
    assert_eq!(
        v["changes"]["emails"]["updated"],
        json!(["email-002"]),
        "moves are reported as updates for delta-walking purposes"
    );
    assert_eq!(
        v["changes"]["emails"]["moved"],
        json!(["email-002"]),
        "moves are also surfaced separately for harness asserts"
    );

    let changes = email_changes(&app, &s3).await;
    assert_eq!(changes["updated"], json!(["email-002"]));

    // Driven across the whole walk: created+destroyed of email-001
    // cancels per RFC §5.2; email-002 collapses to a single update;
    // email-003 stays created.
    let changes = email_changes(&app, &s0).await;
    assert_eq!(changes["created"], json!(["email-003"]));
    assert_eq!(changes["updated"], json!(["email-002"]));
    assert_eq!(changes["destroyed"], json!(["email-001"]));

    // End of script.
    let (status, v) = step(&app, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["ok"], true);
    assert_eq!(v["applied"], false);
    assert!(v["step"].is_null());
}

#[tokio::test]
async fn fixture_step_expect_mismatch_returns_409_without_advancing() {
    let (app, _handle) = router();

    let (status, v) = step(&app, json!({ "expect": "delete" })).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(v["error"], "expect mismatch");
    assert_eq!(v["cursor_step"], "new");

    // Cursor did not advance: the actual next step still applies.
    let (status, v) = step(&app, json!({ "expect": "new" })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["step"], "new");
}

#[tokio::test]
async fn fixture_step_reset_rewinds_image_and_cursor() {
    let (app, _handle) = router();
    let s0 = current_state(&app).await;

    // Apply the first two steps.
    let (status, _) = step(&app, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = step(&app, json!({})).await;
    assert_eq!(status, StatusCode::OK);

    // Reset.
    let resp = app
        .clone()
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

    // State token is back to baseline; the cursor is back at 0 so
    // step-1 ("new") applies again.
    let s_after = current_state(&app).await;
    assert_eq!(s_after, s0);

    let (status, v) = step(&app, json!({ "expect": "new" })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["step"], "new");
    assert_eq!(v["changes"]["emails"]["created"], json!(["email-003"]));
}

#[tokio::test]
async fn fixture_step_malformed_body_returns_400() {
    let (app, _handle) = router();
    // `expect` must be a string when present.
    let (status, v) = step(&app, json!({ "expect": 42 })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"], "malformed body");
}

/// End-to-end IMAP observation: the same fixture handle that the
/// step handler mutates is what `imap::serve_connection` reads
/// from. After step "new", an IMAP `STATUS Inbox` should report 3
/// messages where the baseline had 2. This is the load-bearing
/// "ratatoskr's IMAP client sees the change" assertion.
#[tokio::test]
async fn fixture_step_mutation_is_visible_through_imap_status() {
    let (app, handle) = router();

    // Quick baseline: STATUS reports 2 messages in INBOX.
    let baseline = run_imap(
        &handle,
        b"a1 LOGIN \"u\" \"p\"\r\na2 STATUS \"INBOX\" (MESSAGES)\r\na3 LOGOUT\r\n",
    )
    .await;
    assert!(
        baseline.contains("* STATUS \"INBOX\" (MESSAGES 2)\r\n"),
        "baseline STATUS should report 2; got: {baseline:?}"
    );

    // Apply step "new" via the HTTP route.
    let (status, _) = step(&app, json!({ "expect": "new" })).await;
    assert_eq!(status, StatusCode::OK);

    // After the step, a fresh IMAP connection sees 3 messages.
    let after = run_imap(
        &handle,
        b"a1 LOGIN \"u\" \"p\"\r\na2 STATUS \"INBOX\" (MESSAGES)\r\na3 LOGOUT\r\n",
    )
    .await;
    assert!(
        after.contains("* STATUS \"INBOX\" (MESSAGES 3)\r\n"),
        "post-step STATUS should report 3; got: {after:?}"
    );
}

async fn run_imap(handle: &saehrimnir::shared::FixtureHandle, script: &[u8]) -> String {
    let fix = std::sync::Arc::clone(handle);
    let (server, mut client) = tokio::io::duplex(32 * 1024);
    let (_tx, rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut rx = rx;
        imap::serve_connection(
            server,
            fix,
            None,
            saehrimnir::request_log::RequestLog::default(),
            saehrimnir::latency::LatencyKnob::default(),
            &mut rx,
        )
        .await
    });
    client.write_all(script).await.unwrap();
    client.shutdown().await.unwrap();
    let mut buf = Vec::new();
    client.read_to_end(&mut buf).await.unwrap();
    task.await.unwrap().unwrap();
    String::from_utf8(buf).unwrap()
}

#[tokio::test]
async fn fixture_step_request_log_records_step_endpoint_separately() {
    // The /test/fixture/step route is admin / control-plane and
    // should NOT pollute the cross-protocol request log (which
    // ratatoskr tests use to assert "follow-up sync used the
    // delta path"). Verify by hitting step and then confirming the
    // request log still only has the JMAP method calls a real
    // sync would have made.
    let (app, _handle) = router();

    // Initial sync via JMAP.
    let _ = jmap_call(
        &app,
        "Email/get",
        json!({
            "accountId": "account-1",
            "ids": Value::Null,
            "properties": ["id", "keywords"]
        }),
        "c0",
    )
    .await;

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
    let log_before = body_json(resp).await;
    let cmd_before: Vec<String> = log_before
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["command"].as_str().unwrap().to_string())
        .collect();
    assert!(cmd_before.contains(&"Email/get".to_string()));

    // Apply a step.
    let (status, _) = step(&app, json!({})).await;
    assert_eq!(status, StatusCode::OK);

    // Follow-up delta sync: Email/changes lands in the log.
    let _ = email_changes(&app, "inc-0").await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/test/requests")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let log_after = body_json(resp).await;
    let cmd_after: Vec<String> = log_after
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["command"].as_str().unwrap().to_string())
        .collect();
    assert!(
        cmd_after.contains(&"Email/changes".to_string()),
        "follow-up sync should use Email/changes after a step"
    );
    // /test/fixture/step itself does not appear: the route does not
    // record into the cross-protocol log (it's admin, not protocol).
    assert!(
        !cmd_after.iter().any(|c| c.contains("fixture/step")),
        "step endpoint must not pollute the protocol-level request log"
    );
}
