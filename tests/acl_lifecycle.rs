#![allow(clippy::unwrap_used)]

//! Shared-folder ACL mutation through the change-script control plane.
//!
//! `fixtures/imap-acl-lifecycle.toml` stages the two lifecycle cases a
//! consumer gating shared-mailbox sync has to prove and could not stage
//! before `acl_grant` / `acl_revoke` existed:
//!
//! - **post-attach ACL addition** - a mailbox is shared with the
//!   account *after* it is already connected and syncing, and has to
//!   appear in the other-users namespace without a reconnect;
//! - **ACL revocation** - a mailbox the account could previously see is
//!   withdrawn mid-session, and has to stop being listed and stop being
//!   selectable, again without a reconnect.
//!
//! Both are driven through the same `POST /test/fixture/step` path
//! every other change-script op uses, over a *live* IMAP connection
//! opened before the first step and kept open across both of them.

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;
use tower::ServiceExt;

use saehrimnir::{fixture, imap, routes};

const FIXTURE: &str = "fixtures/imap-acl-lifecycle.toml";

/// JMAP/admin router plus the fixture handle an IMAP connection binds
/// to, so a step driven over HTTP is observable on the IMAP wire.
fn harness() -> (axum::Router, saehrimnir::shared::FixtureHandle) {
    let fix = fixture::load(std::path::Path::new(FIXTURE)).unwrap();
    let handle = saehrimnir::shared::handle(fix);
    let router = routes::router(routes::AppState::for_test(std::sync::Arc::clone(&handle)));
    (router, handle)
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn get(router: &axum::Router, uri: &str) -> Value {
    let resp = router
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "GET {uri}");
    body_json(resp).await
}

async fn step(router: &axum::Router, expect: &str) -> (StatusCode, Value) {
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/test/fixture/step")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "expect": expect })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

/// Read from the duplex client until `needle` appears (or a timeout
/// fires), returning everything read so far.
async fn read_until(client: &mut tokio::io::DuplexStream, needle: &str) -> String {
    let mut acc = String::new();
    let mut buf = [0u8; 4096];
    loop {
        if acc.contains(needle) {
            break;
        }
        let r =
            tokio::time::timeout(std::time::Duration::from_secs(3), client.read(&mut buf)).await;
        let n = match r {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => panic!("read error: {e}"),
            Err(_) => panic!("timed out waiting for {needle:?}; got so far: {acc:?}"),
        };
        if n == 0 {
            break;
        }
        acc.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    acc
}

/// Send `script` and read up to (and including) `needle`.
async fn exchange(client: &mut tokio::io::DuplexStream, script: &str, needle: &str) -> String {
    client.write_all(script.as_bytes()).await.unwrap();
    read_until(client, needle).await
}

const BOB_INBOX: &str = "#user/bob@example.com/INBOX";
const BOB_PROJECTS: &str = "#user/bob@example.com/Projects";

/// One connection, two ACL mutations, no reconnect: the grant makes a
/// previously invisible mailbox appear in the other-users namespace and
/// become selectable; the revoke makes a previously visible one
/// disappear and stop being selectable.
#[tokio::test]
async fn acl_grant_and_revoke_are_observable_on_a_live_imap_connection() {
    let (router, handle) = harness();

    let (server, mut client) = tokio::io::duplex(64 * 1024);
    let (_tx, rx) = watch::channel(false);
    let fix = std::sync::Arc::clone(&handle);
    let task = tokio::spawn(async move {
        let mut rx = rx;
        imap::serve_connection(
            server,
            fix,
            None,
            saehrimnir::oauth::TokenStore::default(),
            saehrimnir::request_log::RequestLog::default(),
            saehrimnir::latency::LatencyKnob::default(),
            saehrimnir::push::PushHub::new(),
            &mut rx,
        )
        .await
    });

    // Baseline: bob's INBOX is shared, Projects is not.
    let out = exchange(
        &mut client,
        "a1 LOGIN \"alice\" \"pw\"\r\na2 LIST \"\" \"#user/*\"\r\n",
        "a2 OK LIST completed",
    )
    .await;
    assert!(out.contains(BOB_INBOX), "shared inbox should be listed: {out}");
    assert!(
        !out.contains(BOB_PROJECTS),
        "ungranted mailbox must not be listed: {out}"
    );

    // ... and an ungranted shared mailbox is not selectable.
    let out = exchange(
        &mut client,
        &format!("a3 SELECT \"{BOB_PROJECTS}\"\r\n"),
        "a3 ",
    )
    .await;
    assert!(
        out.contains("a3 NO SELECT unknown mailbox"),
        "ungranted SELECT should be refused: {out}"
    );

    // Post-attach ACL addition.
    let (status, body) = step(&router, "grant-projects").await;
    assert_eq!(status, StatusCode::OK, "grant step: {body}");
    assert_eq!(body["changes"]["acls"]["granted"][0]["mailbox_id"], "mbx-bob-projects");
    assert_eq!(body["changes"]["acls"]["granted"][0]["identifier"], "account-alice");
    assert_eq!(
        body["changes"]["acls"]["granted"][0]["owner_account_id"],
        "account-bob"
    );

    // The same connection now lists AND selects the new shared folder.
    let out = exchange(
        &mut client,
        "a4 LIST \"\" \"#user/*\"\r\n",
        "a4 OK LIST completed",
    )
    .await;
    assert!(
        out.contains(BOB_PROJECTS),
        "granted mailbox should appear mid-session: {out}"
    );
    assert!(out.contains(BOB_INBOX), "prior grant should survive: {out}");

    let out = exchange(
        &mut client,
        &format!("a5 SELECT \"{BOB_PROJECTS}\"\r\n"),
        "a5 ",
    )
    .await;
    assert!(
        out.contains("a5 OK"),
        "granted mailbox should be selectable: {out}"
    );
    assert!(
        out.contains("* 1 EXISTS"),
        "granted mailbox should expose its message: {out}"
    );

    // ACL revocation.
    let (status, body) = step(&router, "revoke-inbox").await;
    assert_eq!(status, StatusCode::OK, "revoke step: {body}");
    assert!(
        body["changes"]["acls"]["granted"]
            .as_array()
            .unwrap()
            .is_empty(),
        "revoke must not report a grant: {body}"
    );
    assert_eq!(body["changes"]["acls"]["revoked"][0]["mailbox_id"], "mbx-bob-inbox");
    assert_eq!(
        body["changes"]["acls"]["revoked"][0]["owner_account_id"],
        "account-bob"
    );

    let out = exchange(
        &mut client,
        "a6 LIST \"\" \"#user/*\"\r\n",
        "a6 OK LIST completed",
    )
    .await;
    assert!(
        !out.contains(BOB_INBOX),
        "revoked mailbox must stop being listed: {out}"
    );
    assert!(
        out.contains(BOB_PROJECTS),
        "the surviving grant must stay listed: {out}"
    );

    let out = exchange(
        &mut client,
        &format!("a7 SELECT \"{BOB_INBOX}\"\r\n"),
        "a7 ",
    )
    .await;
    assert!(
        out.contains("a7 NO SELECT unknown mailbox"),
        "revoked mailbox must stop being selectable: {out}"
    );

    // Alice's own namespace is untouched by either mutation.
    let out = exchange(
        &mut client,
        "a8 LIST \"\" \"*\"\r\na9 LOGOUT\r\n",
        "a9 OK",
    )
    .await;
    assert!(
        out.contains("\"INBOX\""),
        "personal namespace should be intact: {out}"
    );

    drop(client);
    let _ = task.await;
}

/// MYRIGHTS / GETACL read the same live grant set, so a mid-run grant
/// is reportable through the RFC 4314 surface too, not just LIST.
#[tokio::test]
async fn myrights_follows_a_mid_run_grant() {
    let (router, handle) = harness();

    let (server, mut client) = tokio::io::duplex(64 * 1024);
    let (_tx, rx) = watch::channel(false);
    let fix = std::sync::Arc::clone(&handle);
    let task = tokio::spawn(async move {
        let mut rx = rx;
        imap::serve_connection(
            server,
            fix,
            None,
            saehrimnir::oauth::TokenStore::default(),
            saehrimnir::request_log::RequestLog::default(),
            saehrimnir::latency::LatencyKnob::default(),
            saehrimnir::push::PushHub::new(),
            &mut rx,
        )
        .await
    });

    let out = exchange(
        &mut client,
        &format!("a1 LOGIN \"alice\" \"pw\"\r\na2 MYRIGHTS \"{BOB_PROJECTS}\"\r\n"),
        "a2 ",
    )
    .await;
    assert!(
        out.contains("a2 NO"),
        "MYRIGHTS on an ungranted mailbox should fail: {out}"
    );

    let (status, _) = step(&router, "grant-projects").await;
    assert_eq!(status, StatusCode::OK);

    let out = exchange(
        &mut client,
        &format!("a3 MYRIGHTS \"{BOB_PROJECTS}\"\r\na4 LOGOUT\r\n"),
        "a4 OK",
    )
    .await;
    assert!(
        out.contains("* MYRIGHTS \"#user/bob@example.com/Projects\" lr"),
        "MYRIGHTS should report the fresh grant: {out}"
    );

    drop(client);
    let _ = task.await;
}

/// The admin snapshot projects the live grant set, so a harness can
/// read sharing state without speaking IMAP.
#[tokio::test]
async fn snapshot_state_tracks_acl_mutations() {
    let (router, _handle) = harness();

    let snap = get(&router, "/test/snapshot-state").await;
    let acls = snap["acls"].as_array().unwrap();
    assert_eq!(acls.len(), 1, "baseline has one grant: {snap}");
    assert_eq!(acls[0]["mailbox_id"], "mbx-bob-inbox");
    assert_eq!(acls[0]["rights"], "lr");

    step(&router, "grant-projects").await;
    let snap = get(&router, "/test/snapshot-state").await;
    assert_eq!(snap["acls"].as_array().unwrap().len(), 2, "{snap}");

    step(&router, "revoke-inbox").await;
    let snap = get(&router, "/test/snapshot-state").await;
    let acls = snap["acls"].as_array().unwrap();
    assert_eq!(acls.len(), 1, "{snap}");
    assert_eq!(acls[0]["mailbox_id"], "mbx-bob-projects");
}

/// An ACL touch advances BOTH accounts' state tokens: the owner's
/// (its mailbox's sharing changed) and the grantee's (a mailbox
/// entered or left its other-users namespace). Without the grantee
/// side, a consumer polling as the grantee would see no state movement
/// and never re-walk its shared-folder list.
#[tokio::test]
async fn acl_step_advances_both_accounts() {
    let (router, handle) = harness();

    let before = {
        let fix = handle.read().unwrap();
        (
            fix.state_for("account-alice").to_string(),
            fix.state_for("account-bob").to_string(),
        )
    };

    step(&router, "grant-projects").await;

    let after = {
        let fix = handle.read().unwrap();
        (
            fix.state_for("account-alice").to_string(),
            fix.state_for("account-bob").to_string(),
        )
    };
    assert_ne!(before.0, after.0, "grantee state must advance");
    assert_ne!(before.1, after.1, "owner state must advance");

    step(&router, "revoke-inbox").await;

    let final_states = {
        let fix = handle.read().unwrap();
        (
            fix.state_for("account-alice").to_string(),
            fix.state_for("account-bob").to_string(),
        )
    };
    assert_ne!(after.0, final_states.0, "grantee state must advance again");
    assert_ne!(after.1, final_states.1, "owner state must advance again");
}

/// A revoke with no grant to withdraw is rejected, the fixture is left
/// alone, and the cursor does not advance - the ACL ops honour the same
/// rewind contract as every other op.
#[tokio::test]
async fn revoking_a_missing_grant_rewinds_and_holds_the_cursor() {
    let (router, handle) = harness();

    step(&router, "grant-projects").await;
    step(&router, "revoke-inbox").await;

    let (status, body) = step(&router, "revoke-inbox-again").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "step apply failed");
    assert_eq!(body["kind"], "notFound");
    assert_eq!(body["step"], "revoke-inbox-again");

    // The one surviving grant is untouched...
    {
        let fix = handle.read().unwrap();
        assert_eq!(fix.acls.len(), 1);
        assert_eq!(fix.acls[0].mailbox_id, "mbx-bob-projects");
    }
    // ... and the cursor stayed put, so the same step is still current.
    let (status, body) = step(&router, "revoke-inbox-again").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}
