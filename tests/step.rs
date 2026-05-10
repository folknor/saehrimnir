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
async fn fixture_step_rewind_covers_contacts_and_contact_folders() {
    // Regression: a step that successfully creates a contact and
    // then errors on a later op must roll the contact back out.
    // Previously the snapshot covered only emails / mailboxes /
    // events, leaving the contact persisted and the cursor
    // un-advanced - the script became un-replayable.
    let scenario = r#"
        fixture({ name = "rewind" })
        account({ id = "a", name = "test@example.com" })
        mailbox({ id = "mb", name = "Inbox", role = "inbox" })
        contact_folder({ id = "cf-1", display_name = "People" })
        change({
            id = "creates-contact-then-fails",
            contact_folder_create = {
                { id = "cf-2", display_name = "Late additions" },
            },
            contact_create = {
                {
                    id = "c-1",
                    folder_id = "cf-2",
                    display_name = "Carol",
                    emails = { "carol@example.com" },
                },
            },
            email_destroy = { "missing-email" },
        })
    "#;
    let fix = lua::load_source(scenario, "@rewind").unwrap();
    let handle = saehrimnir::shared::handle(fix);
    let app = routes::router(routes::AppState::for_test(std::sync::Arc::clone(&handle)));

    let (status, _v) = step(&app, json!({})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let snap_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/test/snapshot-state")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(snap_resp.status(), StatusCode::OK);
    let snap = body_json(snap_resp).await;

    // Both the contact and the contact_folder created earlier in
    // the step must be rolled back; only the baseline `cf-1` folder
    // declared at fixture-load time should remain, and no contacts.
    let folders = snap["contact_folders"].as_array().unwrap();
    let folder_ids: Vec<&str> = folders.iter().map(|f| f["id"].as_str().unwrap()).collect();
    assert_eq!(folder_ids, vec!["cf-1"], "snapshot folders: {folders:?}");
    assert_eq!(
        snap["contacts"].as_array().unwrap().len(),
        0,
        "contact survived rewind: {:?}",
        snap["contacts"]
    );

    // Cursor stayed at the failed step; replaying it returns the
    // same error rather than skipping.
    let (status, _v) = step(&app, json!({ "expect": "creates-contact-then-fails" })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
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

/// Regression for the IMAP UID stability contract across change-
/// script mutations. Pre-fix, deleting an email made the freed UID
/// available for reuse on the next `email_create`; any IMAP client
/// caching by UID would silently see a different message at the
/// same UID. Tests the full delete-then-create sequence: baseline
/// has 2 emails (UIDs 1, 2), destroy UID 1, create a third email,
/// the new email gets UID 3 (NOT UID 1).
#[tokio::test]
async fn change_script_destroy_then_create_does_not_reuse_imap_uid() {
    let scenario = r#"
        fixture({ name = "uid-stability" })
        account({ id = "a", name = "test@example.com" })
        mailbox({ id = "mb-inbox", name = "Inbox", role = "inbox" })
        email({
            id = "email-001",
            mailbox_ids = {"mb-inbox"},
            received_at = "2026-01-15T10:00:00Z",
            body_text = "first",
            message_id = {"<001@x>"},
        })
        email({
            id = "email-002",
            mailbox_ids = {"mb-inbox"},
            received_at = "2026-01-15T11:00:00Z",
            body_text = "second",
            message_id = {"<002@x>"},
        })
        change({
            id = "destroy",
            email_destroy = { "email-001" },
        })
        change({
            id = "create",
            email_create = {
                {
                    id = "email-003",
                    mailbox_ids = {"mb-inbox"},
                    received_at = "2026-01-15T12:00:00Z",
                    body_text = "third",
                    message_id = {"<003@x>"},
                },
            },
        })
    "#;
    let fix = lua::load_source(scenario, "@uid-stability").unwrap();
    let handle = saehrimnir::shared::handle(fix);
    let app = routes::router(routes::AppState::for_test(std::sync::Arc::clone(&handle)));

    // Baseline: UIDs 1 and 2 belong to email-001 and email-002.
    // STATUS UIDNEXT = 3.
    let baseline = run_imap(
        &handle,
        b"a LOGIN \"u\" \"p\"\r\nb SELECT \"INBOX\"\r\nc UID FETCH 1:* (UID)\r\nd STATUS \"INBOX\" (UIDNEXT)\r\ne LOGOUT\r\n",
    )
    .await;
    assert!(baseline.contains("* 1 FETCH (UID 1)"), "baseline: {baseline:?}");
    assert!(baseline.contains("* 2 FETCH (UID 2)"), "baseline: {baseline:?}");
    assert!(baseline.contains("UIDNEXT 3"), "baseline UIDNEXT: {baseline:?}");

    // Step "destroy": email-001 (UID 1) goes away.
    let (status, _) = step(&app, json!({ "expect": "destroy" })).await;
    assert_eq!(status, StatusCode::OK);

    // Step "create": email-003 arrives.
    let (status, _) = step(&app, json!({ "expect": "create" })).await;
    assert_eq!(status, StatusCode::OK);

    // After both steps: email-002 keeps UID 2; email-003 gets UID 3
    // (NOT UID 1 - that slot is retired). UIDNEXT advances to 4.
    let after = run_imap(
        &handle,
        b"a LOGIN \"u\" \"p\"\r\nb SELECT \"INBOX\"\r\nc UID FETCH 1:* (UID)\r\nd STATUS \"INBOX\" (UIDNEXT)\r\ne LOGOUT\r\n",
    )
    .await;
    assert!(
        after.contains("* 1 FETCH (UID 2)"),
        "email-002 should be sequence 1, UID 2 (UID stays put after sibling delete): {after:?}"
    );
    assert!(
        after.contains("* 2 FETCH (UID 3)"),
        "email-003 should be sequence 2, UID 3 (fresh allocation, not reusing UID 1): {after:?}"
    );
    assert!(
        !after.contains("UID 1)"),
        "UID 1 must not appear (it was retired by the destroy step): {after:?}"
    );
    assert!(
        after.contains("UIDNEXT 4"),
        "UIDNEXT should reflect the post-create allocation count, not the live email count: {after:?}"
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

/// End-to-end Graph observation: drive a 3-step contact script, then
/// assert the resulting deltas surface through
/// `/v1.0/me/contactFolders/{id}/contacts/delta` with the right
/// RFC-style dominance (created+destroyed cancels; update collapses
/// against destroyed).
#[tokio::test]
async fn fixture_step_mutations_visible_through_graph_contacts_delta() {
    use saehrimnir::graph;

    let path = std::path::Path::new("fixtures/graph-contacts-incremental.lua");
    let source = std::fs::read_to_string(path).unwrap();
    let chunk = format!("@{}", path.display());
    let fix = lua::load_source_with_dir(&source, &chunk, path.parent().unwrap()).unwrap();
    let handle = saehrimnir::shared::handle(fix);
    let route_state = routes::AppState::for_test(std::sync::Arc::clone(&handle));
    let graph_state = graph::AppState {
        shared: route_state.shared.clone(),
    };
    let app = routes::router(route_state);
    let graph_app = graph::router(graph_state);

    // Bootstrap: Graph delta dump returns the two baseline contacts +
    // a deltaLink pinned to the current state.
    let baseline = graph_get_json(
        &graph_app,
        "/v1.0/me/contactFolders/cf-default/contacts/delta?$select=id",
    )
    .await;
    let baseline_value = baseline["value"].as_array().unwrap();
    assert_eq!(baseline_value.len(), 2);
    let baseline_delta = baseline["@odata.deltaLink"].as_str().unwrap().to_string();
    let baseline_path = baseline_delta
        .split_once("/v1.0/")
        .map(|(_, p)| format!("/v1.0/{p}"))
        .unwrap();

    // Apply all three steps.
    for expect in ["new", "change", "delete"] {
        let (status, _) = step(&app, json!({ "expect": expect })).await;
        assert_eq!(status, StatusCode::OK);
    }

    // Following the baseline deltaLink: created (contact-003), updated
    // (contact-002), destroyed (contact-001). RFC §5.2 dominance
    // applies but no within-window collapses fire here.
    let v = graph_get_json(&graph_app, &baseline_path).await;
    let mut ids: Vec<String> = v["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    assert_eq!(ids, vec!["contact-001", "contact-002", "contact-003"]);
    // contact-001 is the tombstone.
    let tombstone = v["value"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "contact-001")
        .unwrap();
    assert_eq!(tombstone["@removed"]["reason"], "deleted");
}

async fn graph_get_json(app: &axum::Router, uri: &str) -> Value {
    let resp = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(uri)
                .header(axum::http::header::HOST, "127.0.0.1:9999")
                .header(axum::http::header::AUTHORIZATION, "Bearer x")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await
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
