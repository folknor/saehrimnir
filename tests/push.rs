#![allow(clippy::unwrap_used)]
#![allow(clippy::cast_possible_truncation)]

//! Integration tests for the provider push surfaces (JMAP WebSocket
//! `StateChange`, Gmail Cloud Pub/Sub, Graph webhooks).
//!
//! Each test wires one `SharedHandles` into all three protocol routers
//! (so the subscribe endpoint on one listener and the state-mutation
//! trigger on the JMAP `/test/fixture/step` listener share one
//! `PushHub`), registers a subscriber, drives a mutation through the
//! existing `/test/fixture/step` path, and asserts the push fired with
//! the real wire shape. Mirrors the `tests/step.rs` style.

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

use saehrimnir::shared::SharedHandles;
use saehrimnir::{gmail, graph, imap, lua, routes, shared, smtp};
use tokio::sync::watch;

/// Build the JMAP / Gmail / Graph routers over one shared handle bag so
/// a watch / subscription registered on one and a step driven on
/// another hit the same `PushHub`.
fn routers() -> (axum::Router, axum::Router, axum::Router, SharedHandles) {
    let path = std::path::Path::new("fixtures/jmap-incremental.lua");
    let source = std::fs::read_to_string(path).unwrap();
    let chunk = format!("@{}", path.display());
    let fix = lua::load_source_with_dir(&source, &chunk, path.parent().unwrap()).unwrap();
    routers_for(fix)
}

/// Same three-router bag over a TOML fixture.
fn routers_toml(path: &str) -> (axum::Router, axum::Router, axum::Router, SharedHandles) {
    routers_for(saehrimnir::fixture::load(std::path::Path::new(path)).unwrap())
}

fn routers_for(
    fix: saehrimnir::fixture::Fixture,
) -> (axum::Router, axum::Router, axum::Router, SharedHandles) {
    let handle = shared::handle(fix);
    let shared = SharedHandles::for_test(Arc::clone(&handle));
    let jmap = routes::router(routes::AppState {
        shared: shared.clone(),
        submission_log: smtp::SubmissionLog::default(),
        base_url: "http://localhost".into(),
    });
    let gmail_app = gmail::router(gmail::AppState {
        shared: shared.clone(),
    });
    let graph_app = graph::router(graph::AppState {
        shared: shared.clone(),
    });
    (jmap, gmail_app, graph_app, shared)
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

async fn post(router: &axum::Router, uri: &str, body: Value) -> axum::response::Response {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}

/// Drive one `/test/fixture/step` against the JMAP listener.
async fn step(jmap: &axum::Router, expect: &str) {
    let resp = post(jmap, "/test/fixture/step", json!({ "expect": expect })).await;
    assert_eq!(resp.status(), StatusCode::OK, "step {expect}");
}

/// Minimal RFC 4648 standard-base64 decoder for asserting on the
/// Pub/Sub `message.data` payload.
fn base64_decode(s: &str) -> Vec<u8> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let chars: Vec<u8> = s.bytes().filter(|c| *c != b'=').collect();
    let mut out = Vec::new();
    for chunk in chars.chunks(4) {
        let mut n = 0u32;
        let mut bits = 0;
        for &c in chunk {
            n = (n << 6) | val(c).unwrap();
            bits += 6;
        }
        // Drop the low padding bits, emit whole bytes high-first.
        let bytes = bits / 8;
        n >>= bits % 8;
        for i in (0..bytes).rev() {
            out.push(((n >> (i * 8)) & 0xff) as u8);
        }
    }
    out
}

#[tokio::test]
async fn jmap_websocket_statechange_fires_on_step() {
    let (jmap, _gmail, _graph, shared) = routers();

    // Register a push listener for account-1 (what the WebSocket
    // upgrade handler does internally on connect).
    let mut rx = shared.push.register_jmap_ws("account-1".to_string());

    // Step 1 creates email-003 in account-1, advancing its state.
    step(&jmap, "new").await;

    // The StateChange frame reaches the registered listener.
    let frame = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("push frame should arrive within timeout")
        .expect("channel open");
    let frame: Value = serde_json::from_str(&frame).unwrap();
    assert_eq!(frame["@type"], "StateChange");
    let changed = &frame["changed"]["account-1"];
    assert_eq!(changed["Email"], "inc-0.1");
    assert_eq!(changed["Mailbox"], "inc-0.1");
    assert_eq!(changed["Thread"], "inc-0.1");
    // Single-account, mail-only fixture: no calendar / contact keys.
    assert!(changed.get("CalendarEvent").is_none());
    assert!(changed.get("ContactCard").is_none());

    // The same frame is in the observability log.
    let log = get(&jmap, "/test/push/jmap").await;
    assert_eq!(log.as_array().unwrap().len(), 1);
    assert_eq!(log[0]["changed"]["account-1"]["Email"], "inc-0.1");
}

#[tokio::test]
async fn jmap_session_advertises_websocket_capability() {
    let (jmap, _gmail, _graph, _shared) = routers();
    let session = get(&jmap, "/jmap/session").await;
    let cap = &session["capabilities"]["urn:ietf:params:jmap:websocket"];
    assert_eq!(cap["supportsPush"], true);
    assert_eq!(cap["url"], "ws://localhost/jmap/ws");
}

#[tokio::test]
async fn gmail_pubsub_publishes_on_step_after_watch() {
    let (jmap, gmail_app, _graph, _shared) = routers();

    // A client subscribes via users.watch.
    let resp = post(
        &gmail_app,
        "/gmail/v1/users/me/watch",
        json!({ "topicName": "projects/p/topics/mail", "labelIds": ["INBOX"] }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let watch = body_json(resp).await;
    // Baseline historyId is 1 (counter 0 + 1).
    assert_eq!(watch["historyId"], "1");
    assert!(watch.get("expiration").is_some());

    // A mutation advances account-1's state -> a Pub/Sub push is
    // published onto the mock source.
    step(&jmap, "new").await;

    let msgs = get(&jmap, "/test/gmail/pubsub/messages").await;
    let msgs = msgs.as_array().unwrap();
    assert_eq!(msgs.len(), 1, "exactly one notification published");
    let env = &msgs[0];
    assert_eq!(env["subscription"], "projects/p/subscriptions/mail");
    let data = env["message"]["data"].as_str().unwrap();
    let decoded: Value = serde_json::from_slice(&base64_decode(data)).unwrap();
    assert_eq!(decoded["emailAddress"], "test@example.com");
    // historyId advanced to 2 (counter 1 + 1) after the mutation.
    assert_eq!(decoded["historyId"], 2);
}

#[tokio::test]
async fn gmail_no_pubsub_without_watch() {
    let (jmap, _gmail, _graph, _shared) = routers();
    // No watch registered: a mutation publishes nothing.
    step(&jmap, "new").await;
    let msgs = get(&jmap, "/test/gmail/pubsub/messages").await;
    assert!(msgs.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn gmail_stop_cancels_watch() {
    let (jmap, gmail_app, _graph, _shared) = routers();
    post(
        &gmail_app,
        "/gmail/v1/users/me/watch",
        json!({ "topicName": "projects/p/topics/mail" }),
    )
    .await;
    let resp = post(&gmail_app, "/gmail/v1/users/me/stop", json!({})).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    // After stop, a mutation publishes nothing.
    step(&jmap, "new").await;
    let msgs = get(&jmap, "/test/gmail/pubsub/messages").await;
    assert!(msgs.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn graph_validation_token_is_echoed() {
    let (_jmap, _gmail, graph_app, _shared) = routers();
    let resp = graph_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1.0/subscriptions?validationToken=abc-123")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(ct.starts_with("text/plain"), "content-type was {ct}");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..], b"abc-123");
}

#[tokio::test]
async fn graph_webhook_notification_fires_on_step() {
    let (jmap, _gmail, graph_app, _shared) = routers();

    // Create a subscription bound to a (deliberately dead) loopback URL
    // so delivery is best-effort; the observability log proves the
    // notification fired with the right wire shape + clientState.
    let resp = post(
        &graph_app,
        "/v1.0/subscriptions",
        json!({
            "resource": "me/messages",
            "changeType": "created,updated,deleted",
            "notificationUrl": "http://127.0.0.1:9/none",
            "clientState": "secret-state-xyz",
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = body_json(resp).await;
    let sub_id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["clientState"], "secret-state-xyz");

    // Step 1 (create) advances account-1's state.
    step(&jmap, "new").await;

    let log = get(&jmap, "/test/push/graph").await;
    let log = log.as_array().unwrap();
    assert_eq!(log.len(), 1, "one notification emitted");
    let entry = &log[0];
    assert_eq!(entry["notification_url"], "http://127.0.0.1:9/none");
    let note = &entry["body"]["value"][0];
    assert_eq!(note["subscriptionId"], sub_id);
    assert_eq!(note["clientState"], "secret-state-xyz");
    assert_eq!(note["resource"], "me/messages");
    assert_eq!(note["changeType"], "created");
    assert_eq!(note["resourceData"]["id"], "email-003");
}

#[tokio::test]
async fn graph_webhook_delivers_to_loopback_endpoint() {
    let (jmap, _gmail, graph_app, _shared) = routers();

    // Stand up a one-shot loopback receiver and register it as the
    // subscription's notificationUrl, then assert the change-
    // notification actually arrives over the wire with the clientState.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let receiver = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        // Read until we have headers + the declared body.
        loop {
            let mut tmp = [0u8; 1024];
            let n = sock.read(&mut tmp).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                let len: usize = headers
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                if buf.len() >= pos + 4 + len {
                    let body = buf[pos + 4..pos + 4 + len].to_vec();
                    sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                        .await
                        .unwrap();
                    return body;
                }
            }
        }
        Vec::new()
    });

    let url = format!("http://127.0.0.1:{port}/notify");
    let resp = post(
        &graph_app,
        "/v1.0/subscriptions",
        json!({
            "resource": "me/messages",
            "notificationUrl": url,
            "clientState": "loopback-state",
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    step(&jmap, "new").await;

    let body = tokio::time::timeout(std::time::Duration::from_secs(3), receiver)
        .await
        .expect("loopback receiver should get the POST")
        .unwrap();
    let note: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(note["value"][0]["clientState"], "loopback-state");
    assert_eq!(note["value"][0]["resource"], "me/messages");
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Read from the duplex client until `needle` appears in the
/// accumulated bytes (or a timeout fires), returning everything read.
async fn read_until(client: &mut tokio::io::DuplexStream, needle: &str) -> String {
    let mut acc = String::new();
    let mut buf = [0u8; 1024];
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

// ── Personal + shared + public namespaces ───────────────────────────
//
// `fixtures/push-namespaces.toml` stages all three at once: a personal
// mailbox, a shared mailbox (owned by a second account and granted to
// the personal one via `[[acl]]`), and an org-wide public folder. The
// contract under test is that the shared and public resources do not
// disturb personal push.

/// Await a frame that must NOT arrive.
async fn expect_no_frame(rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>) {
    let r = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await;
    assert!(
        r.is_err(),
        "a mutation in another account must not push this one: {r:?}"
    );
}

async fn create_subscription(
    graph_app: &axum::Router,
    resource: &str,
    client_state: &str,
) -> String {
    let resp = post(
        graph_app,
        "/v1.0/subscriptions",
        json!({
            "resource": resource,
            "changeType": "created,updated,deleted",
            "notificationUrl": "http://127.0.0.1:9/none",
            "clientState": client_state,
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "subscribe {resource}");
    body_json(resp).await["id"].as_str().unwrap().to_string()
}

/// With personal, shared and public resources staged together, a state
/// advance pushes exactly the account it touched, over every transport,
/// and the public-folder subscription is excluded from the fan-out
/// rather than firing alongside the personal one.
#[tokio::test]
async fn personal_shared_and_public_push_per_account() {
    let (jmap, gmail_app, graph_app, shared) = routers_toml("fixtures/push-namespaces.toml");

    let mut personal_ws = shared.push.register_jmap_ws("account-personal".to_string());
    let mut shared_ws = shared.push.register_jmap_ws("account-shared".to_string());

    // Gmail watch binds to the bearer's account (the primary, i.e. the
    // personal one) - the shared mailbox has no watch.
    let resp = post(
        &gmail_app,
        "/gmail/v1/users/me/watch",
        json!({ "topicName": "projects/p/topics/mail" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // One Graph subscription per namespace.
    let personal_sub = create_subscription(&graph_app, "me/messages", "cs-personal").await;
    let shared_sub = create_subscription(
        &graph_app,
        "users/account-shared/mailFolders('inbox')/messages",
        "cs-shared",
    )
    .await;
    let public_sub = create_subscription(
        &graph_app,
        "publicfolders/pf-root-announce/messages",
        "cs-public",
    )
    .await;

    // The subscription set classifies each resource, and says up front
    // which ones a state advance fans out to.
    let subs = get(&jmap, "/test/push/subscriptions").await;
    let subs = subs.as_array().unwrap();
    assert_eq!(subs.len(), 3);
    let by_id = |id: &str| -> Value {
        subs.iter()
            .find(|s| s["id"] == id)
            .unwrap_or_else(|| panic!("subscription {id} missing"))
            .clone()
    };
    assert_eq!(by_id(&personal_sub)["namespace"], "personal");
    assert_eq!(by_id(&personal_sub)["account_id"], "account-personal");
    assert_eq!(by_id(&personal_sub)["emits"], true);
    // A `users/{id}/...` resource binds to the named principal, not the
    // bearer, so a shared-mailbox mutation reaches it.
    assert_eq!(by_id(&shared_sub)["namespace"], "shared");
    assert_eq!(by_id(&shared_sub)["account_id"], "account-shared");
    assert_eq!(by_id(&shared_sub)["emits"], true);
    assert_eq!(by_id(&public_sub)["namespace"], "public");
    assert_eq!(by_id(&public_sub)["emits"], false);

    // ── Personal arrival ────────────────────────────────────────────
    step(&jmap, "personal-arrival").await;

    let frame = tokio::time::timeout(std::time::Duration::from_secs(2), personal_ws.recv())
        .await
        .expect("personal push should arrive")
        .expect("channel open");
    let frame: Value = serde_json::from_str(&frame).unwrap();
    assert_eq!(frame["changed"]["account-personal"]["Email"], "push-0.1");
    expect_no_frame(&mut shared_ws).await;

    // Gmail: exactly one Pub/Sub message, for the personal address.
    let msgs = get(&jmap, "/test/gmail/pubsub/messages").await;
    assert_eq!(msgs.as_array().unwrap().len(), 1);
    let decoded: Value =
        serde_json::from_slice(&base64_decode(msgs[0]["message"]["data"].as_str().unwrap()))
            .unwrap();
    assert_eq!(decoded["emailAddress"], "user@example.com");

    // Graph: the personal subscription fired; the public one did not,
    // and its exclusion is recorded rather than silent.
    let log = get(&jmap, "/test/push/graph").await;
    let log = log.as_array().unwrap();
    assert_eq!(
        log.len(),
        1,
        "only the personal subscription fires: {log:?}"
    );
    assert_eq!(log[0]["namespace"], "personal");
    assert_eq!(log[0]["body"]["value"][0]["clientState"], "cs-personal");
    assert_eq!(
        log[0]["body"]["value"][0]["resourceData"]["id"],
        "email-personal-002"
    );

    let excluded = get(&jmap, "/test/push/graph/excluded").await;
    let excluded = excluded.as_array().unwrap();
    assert_eq!(excluded.len(), 1, "the public sub is excluded once");
    assert_eq!(excluded[0]["subscription_id"], public_sub);
    assert_eq!(excluded[0]["namespace"], "public");

    // ── Shared arrival ──────────────────────────────────────────────
    step(&jmap, "shared-arrival").await;

    let frame = tokio::time::timeout(std::time::Duration::from_secs(2), shared_ws.recv())
        .await
        .expect("shared push should arrive")
        .expect("channel open");
    let frame: Value = serde_json::from_str(&frame).unwrap();
    assert_eq!(frame["changed"]["account-shared"]["Email"], "push-0.1");
    expect_no_frame(&mut personal_ws).await;

    // No Gmail watch on the shared account: the sink is unchanged.
    let msgs = get(&jmap, "/test/gmail/pubsub/messages").await;
    assert_eq!(msgs.as_array().unwrap().len(), 1);

    // Graph: the shared subscription fired this time, the personal one
    // did not, and the public one stayed excluded exactly once (it is
    // bound to the personal account, so a shared advance never reaches
    // it at all).
    let log = get(&jmap, "/test/push/graph").await;
    let log = log.as_array().unwrap();
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[1]["namespace"], "shared");
    assert_eq!(log[1]["body"]["value"][0]["clientState"], "cs-shared");
    assert_eq!(
        log[1]["body"]["value"][0]["resourceData"]["id"],
        "email-shared-002"
    );
    let excluded = get(&jmap, "/test/push/graph/excluded").await;
    assert_eq!(excluded.as_array().unwrap().len(), 1);
}

/// A public-folder subscription registered *first* still doesn't
/// poison the personal one that follows it: the personal notification
/// fires with its own clientState, and no notification ever carries the
/// public subscription's.
#[tokio::test]
async fn public_folder_subscription_does_not_poison_the_personal_set() {
    let (jmap, _gmail, graph_app, _shared) = routers_toml("fixtures/push-namespaces.toml");

    let public_sub = create_subscription(
        &graph_app,
        "publicfolders/pf-root-announce/messages",
        "cs-public",
    )
    .await;
    create_subscription(
        &graph_app,
        "me/mailFolders('inbox')/messages",
        "cs-personal",
    )
    .await;

    step(&jmap, "personal-arrival").await;

    let log = get(&jmap, "/test/push/graph").await;
    let log = log.as_array().unwrap();
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!(log[0]["body"]["value"][0]["clientState"], "cs-personal");
    let rendered = serde_json::to_string(&log).unwrap();
    assert!(
        !rendered.contains("cs-public"),
        "public subscription leaked into the fan-out: {rendered}"
    );

    let excluded = get(&jmap, "/test/push/graph/excluded").await;
    assert_eq!(excluded[0]["subscription_id"], public_sub);
}

/// The fixture is readable as one arrangement: personal + shared
/// mailboxes with the ACL that links them, plus the public tree.
#[tokio::test]
async fn namespaces_fixture_projects_all_three_resource_kinds() {
    let (jmap, _gmail, _graph, _shared) = routers_toml("fixtures/push-namespaces.toml");
    let snap = get(&jmap, "/test/snapshot-state").await;
    let mailboxes = snap["mailboxes"].as_array().unwrap();
    assert_eq!(mailboxes.len(), 2);
    let acls = snap["acls"].as_array().unwrap();
    assert_eq!(acls.len(), 1);
    assert_eq!(acls[0]["mailbox_id"], "mbx-shared-inbox");
    assert_eq!(acls[0]["identifier"], "account-personal");
    let public = snap["public_folders"].as_array().unwrap();
    assert_eq!(public.len(), 1);
    assert_eq!(public[0]["id"], "pf-root-announce");
}

/// IMAP `IDLE` observes a state mutation driven through the *same*
/// test-admin step trigger that fires the other push surfaces: the
/// IMAP connection and the JMAP step router share one `PushHub`. After
/// `SELECT INBOX` + `IDLE`, stepping the change script makes the idling
/// client see `* n EXISTS` for an arriving message and `* n EXPUNGE`
/// for a deleted one, then `DONE` ends the idle.
#[tokio::test]
async fn imap_idle_observes_exists_and_expunge_on_step() {
    let (jmap, _gmail, _graph, shared) = routers();

    let (server, mut client) = tokio::io::duplex(32 * 1024);
    let (_tx, rx) = watch::channel(false);
    let fix = std::sync::Arc::clone(&shared.fixture);
    let push = shared.push.clone();
    let task = tokio::spawn(async move {
        let mut rx = rx;
        imap::serve_connection(
            server,
            fix,
            None,
            saehrimnir::oauth::TokenStore::default(),
            saehrimnir::request_log::RequestLog::default(),
            saehrimnir::latency::LatencyKnob::default(),
            push,
            &mut rx,
        )
        .await
    });

    // LOGIN, SELECT INBOX (2 baseline emails), enter IDLE.
    client
        .write_all(b"a1 LOGIN \"u\" \"p\"\r\na2 SELECT \"INBOX\"\r\na3 IDLE\r\n")
        .await
        .unwrap();
    let pre = read_until(&mut client, "+ idling").await;
    assert!(pre.contains("a2 OK"), "SELECT should complete: {pre:?}");
    assert!(pre.contains("* 2 EXISTS"), "baseline INBOX has 2: {pre:?}");

    // Step "new" creates email-003 in INBOX -> the idler reports the
    // new total via EXISTS and a RECENT count for the arrival.
    step(&jmap, "new").await;
    let after_new = read_until(&mut client, "RECENT").await;
    assert!(
        after_new.contains("* 3 EXISTS"),
        "expected EXISTS 3: {after_new:?}"
    );
    assert!(
        after_new.contains("* 1 RECENT"),
        "expected RECENT 1: {after_new:?}"
    );

    // Step "change" (flag-only, no membership change -> no EXISTS /
    // EXPUNGE) then "delete" (removes email-001, UID 1 / seq 1).
    step(&jmap, "change").await;
    step(&jmap, "delete").await;
    let after_delete = read_until(&mut client, "EXPUNGE").await;
    assert!(
        after_delete.contains("* 1 EXPUNGE"),
        "expected EXPUNGE 1: {after_delete:?}"
    );

    // DONE ends the idle with a tagged OK.
    client.write_all(b"DONE\r\n").await.unwrap();
    let done = read_until(&mut client, "a3 OK").await;
    assert!(
        done.contains("a3 OK IDLE terminated"),
        "idle should terminate: {done:?}"
    );

    drop(client);
    let _ = task.await;
}
