#![allow(clippy::unwrap_used)]

//! Cold-start backfill: a live change announced WHILE a paginated
//! backfill walk is in flight.
//!
//! These tests assert what the MOCK does. Each one checks three
//! things, because any of them silently failing would leave a
//! consumer-side gate passing while proving nothing:
//!
//! 1. The change lands strictly BETWEEN two pages of one walk - after
//!    page N's response, before page N+1's request is served.
//! 2. The change is really published on the change feed, not just
//!    applied to the fixture. A silently-applied mutation stages a
//!    resync, not an interleaving.
//! 3. The pages either side of it reflect the mutation exactly as an
//!    offset-paged server would.
//!
//! ## What the mock can and cannot guarantee
//!
//! The mock's determinism ends at the socket. It can place the push
//! at a chosen point in the walk; it cannot schedule what the
//! consumer does with it. A consumer learning of the change has its
//! own round trip to make (a Gmail push carries only
//! `{emailAddress, historyId}`, so the ids arrive via a follow-up
//! `history.list`), and whether that completes before the backfill
//! reaches the page carrying the same id is decided by the consumer's
//! task scheduling, not here. These fixtures make the interleaving
//! reachable and reproducible; they do not make it certain.
//!
//! Making it certain would need a request-dependency barrier the mock
//! does not have - "hold the page-N request until `history.list` has
//! been served at least once since the announcement". That is
//! buildable on top of the same trigger machinery. It is deliberately
//! not built: no consumer gate is consuming these yet, and an
//! affordance shaped by guesswork about what a gate will need is how
//! a mock ends up serving the wrong shape convincingly.
//!
//! ## What is NOT staged here
//!
//! The resurrection race - backfill selects an id and finds nothing
//! recorded, the live feed then records and broadcasts `Destroyed`,
//! and the backfill then broadcasts its already-decided synthetic
//! `Created` - is not reachable from a mock at all. That window sits
//! between two operations inside one consumer function with no
//! intervening I/O and no await point, so there is no request for a
//! trigger to key on and nothing for the server to interleave with.
//! Reaching it needs a seam on the consumer side, not an affordance
//! here.
//!
//! The related but distinct case that IS a server behaviour - a
//! destroyed object still appearing in listings after the feed
//! retracted it, because the listing is an eventually-consistent
//! snapshot - would need a stale-inventory affordance this mock does
//! not have. See `fixtures/backfill-late-tombstone.lua`.

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use saehrimnir::{gmail, lua, routes, shared, shared::SharedHandles, smtp};

/// JMAP/admin router (for `/test/...`) plus the Gmail router, over one
/// shared handle bag so a watch registered on one and a trigger fired
/// on the other reach the same `PushHub` and the same request log.
fn routers(fixture_path: &str) -> (axum::Router, axum::Router) {
    let path = std::path::Path::new(fixture_path);
    let source = std::fs::read_to_string(path).unwrap();
    let chunk = format!("@{}", path.display());
    let fix = lua::load_source_with_dir(&source, &chunk, path.parent().unwrap()).unwrap();
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
    (jmap, gmail_app)
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn get(router: &axum::Router, uri: &str) -> Value {
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header(header::AUTHORIZATION, "Bearer doesnt-matter")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "GET {uri}");
    body_json(resp).await
}

/// Register the Gmail Pub/Sub watch, i.e. subscribe to the change feed
/// before the backfill starts. Without it the mutation still happens
/// but nothing is published, and the race degrades into a plain
/// mid-walk mutation.
async fn watch(gmail_app: &axum::Router) {
    let resp = gmail_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/gmail/v1/users/me/watch")
                .header(header::AUTHORIZATION, "Bearer doesnt-matter")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "topicName": "projects/p/topics/mail" })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// Walk `messages.list` to exhaustion at two ids per page, returning
/// one Vec of ids per page. This is the backfill.
async fn walk(gmail_app: &axum::Router) -> Vec<Vec<String>> {
    let mut pages = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let uri = match &token {
            None => "/gmail/v1/users/me/messages?maxResults=2".to_string(),
            Some(t) => format!("/gmail/v1/users/me/messages?maxResults=2&pageToken={t}"),
        };
        let body = get(gmail_app, &uri).await;
        pages.push(
            body["messages"]
                .as_array()
                .unwrap_or(&Vec::new())
                .iter()
                .map(|m| m["id"].as_str().unwrap().to_string())
                .collect(),
        );
        token = body
            .get("nextPageToken")
            .and_then(Value::as_str)
            .map(str::to_string);
        if token.is_none() {
            return pages;
        }
        assert!(pages.len() < 20, "walk did not terminate: {pages:?}");
    }
}

/// Every logged command in order, so the trigger's position relative
/// to the page requests can be asserted rather than assumed.
async fn commands(jmap: &axum::Router) -> Vec<String> {
    get(jmap, "/test/requests")
        .await
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["command"].as_str().unwrap().to_string())
        .collect()
}

/// Index of the single `ANNOUNCE <step>` row. Fails loudly when the
/// trigger did not fire, or fired more than once.
fn announce_index(commands: &[String], step: &str) -> usize {
    let needle = format!("ANNOUNCE {step}");
    let hits: Vec<usize> = commands
        .iter()
        .enumerate()
        .filter(|(_, c)| *c == &needle)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "expected exactly one {needle:?} row, got {hits:?} in {commands:?}"
    );
    hits[0]
}

/// Indices of the `messages.list` page requests, in order.
fn page_request_indices(commands: &[String]) -> Vec<usize> {
    commands
        .iter()
        .enumerate()
        .filter(|(_, c)| *c == "GET /gmail/v1/users/me/messages")
        .map(|(i, _)| i)
        .collect()
}

/// The Pub/Sub envelopes published onto the sink, newest last.
async fn published(jmap: &axum::Router) -> Vec<Value> {
    get(jmap, "/test/gmail/pubsub/messages")
        .await
        .as_array()
        .unwrap()
        .clone()
}

/// Every `history.list` record from the seed cursor: what the feed
/// tells the consumer to go and read.
async fn history(jmap_gmail: &axum::Router) -> Value {
    get(jmap_gmail, "/gmail/v1/users/me/history?startHistoryId=1").await
}

#[tokio::test]
async fn created_is_announced_mid_walk_and_also_served_by_a_later_page() {
    let (jmap, gmail_app) = routers("fixtures/backfill-race-created.lua");
    watch(&gmail_app).await;

    let pages = walk(&gmail_app).await;

    // The walk saw the pre-change inventory on page 1 and the new
    // message on a page served AFTER the announcement, so both sides
    // offered the consumer msg-new. Whether the consumer's live
    // handler actually records it before the backfill reaches page 4
    // is its own scheduling decision - see the module docs on what
    // the mock does and does not guarantee.
    assert_eq!(
        pages,
        vec![
            vec!["msg-5".to_string(), "msg-4".to_string()],
            vec!["msg-3".to_string(), "msg-2".to_string()],
            vec!["msg-1".to_string(), "msg-0".to_string()],
            vec!["msg-new".to_string()],
        ],
        "walk pages"
    );

    // The trigger fired strictly between page 1 and page 2.
    let commands = commands(&jmap).await;
    let announce = announce_index(&commands, "arrive-new");
    let requests = page_request_indices(&commands);
    assert_eq!(requests.len(), 4, "{commands:?}");
    assert!(
        requests[0] < announce && announce < requests[1],
        "announce must land between page 1 and page 2: {commands:?}"
    );

    // It really was announced, not just applied: one Pub/Sub envelope,
    // published while the walk was in flight.
    let msgs = published(&jmap).await;
    assert_eq!(msgs.len(), 1, "{msgs:?}");

    // And the feed's content names msg-new as an addition, so a
    // consumer following the feed ingests it from that side too.
    let hist = history(&gmail_app).await;
    let added: Vec<&str> = hist["history"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r.get("messagesAdded"))
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|e| e["message"]["id"].as_str())
        .collect();
    assert_eq!(added, vec!["msg-new"], "history: {hist}");
}

/// A late tombstone: an id the backfill already delivered is retracted
/// by the feed mid-walk.
///
/// This is an ORDERING fixture, not the resurrection race. The
/// consumer is handed msg-3 and only afterwards told it is gone;
/// correct handling is last-writer-wins on arrival order. The
/// resurrection race needs the inventory to still YIELD msg-3 after
/// the retraction, which this mock cannot do - its listing is live,
/// not a snapshot, so a destroyed message simply stops being listed
/// and the suppression path is never entered. Announcing earlier
/// would not help; it would only remove msg-3 from the walk sooner.
#[tokio::test]
async fn late_tombstone_retracts_an_id_the_backfill_already_delivered() {
    let (jmap, gmail_app) = routers("fixtures/backfill-late-tombstone.lua");
    watch(&gmail_app).await;

    let pages = walk(&gmail_app).await;

    // msg-3 was handed out by page 2, then retracted before page 3.
    assert_eq!(
        pages,
        vec![
            vec!["msg-5".to_string(), "msg-4".to_string()],
            vec!["msg-3".to_string(), "msg-2".to_string()],
            // msg-1 is skipped: removing a row BEFORE the paging cursor
            // shifts every later row left by one. That is what offset
            // pagination does under a concurrent delete, not a mock
            // artifact - pinned here so a change to it is a decision
            // rather than a surprise.
            vec!["msg-0".to_string()],
        ],
        "walk pages"
    );
    // Once retracted, the live listing stops offering it. This is
    // exactly why the fixture cannot stage the resurrection race: the
    // suppression path is only reachable when a page STILL offers a
    // destroyed id, which a live listing never does.
    assert!(
        pages[2..].iter().flatten().all(|id| id != "msg-3"),
        "a retracted id must never be served again: {pages:?}"
    );

    let commands = commands(&jmap).await;
    let announce = announce_index(&commands, "retract-3");
    let requests = page_request_indices(&commands);
    assert_eq!(requests.len(), 3, "{commands:?}");
    assert!(
        requests[1] < announce && announce < requests[2],
        "announce must land between page 2 and page 3: {commands:?}"
    );

    let msgs = published(&jmap).await;
    assert_eq!(msgs.len(), 1, "{msgs:?}");

    let hist = history(&gmail_app).await;
    let deleted: Vec<&str> = hist["history"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r.get("messagesDeleted"))
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|e| e["message"]["id"].as_str())
        .collect();
    assert_eq!(deleted, vec!["msg-3"], "history: {hist}");
}

#[tokio::test]
async fn updated_is_announced_before_the_page_that_serves_it() {
    let (jmap, gmail_app) = routers("fixtures/backfill-race-updated.lua");
    watch(&gmail_app).await;

    let pages = walk(&gmail_app).await;

    // The negative control. msg-1 is announced as UPDATED while the
    // walk is two pages away from it, and the walk still serves it. A
    // de-dup set that suppressed the backfill emission on the strength
    // of the update would drop msg-1 entirely - during a cold start
    // the consumer holds no row for it yet, so the update alone does
    // not describe the object.
    assert_eq!(
        pages,
        vec![
            vec!["msg-5".to_string(), "msg-4".to_string()],
            vec!["msg-3".to_string(), "msg-2".to_string()],
            vec!["msg-1".to_string(), "msg-0".to_string()],
        ],
        "an in-place update shifts nothing: {pages:?}"
    );

    let commands = commands(&jmap).await;
    let announce = announce_index(&commands, "touch-1");
    let requests = page_request_indices(&commands);
    assert_eq!(requests.len(), 3, "{commands:?}");
    assert!(
        requests[0] < announce && announce < requests[1],
        "announce must land between page 1 and page 2: {commands:?}"
    );

    let msgs = published(&jmap).await;
    assert_eq!(msgs.len(), 1, "{msgs:?}");

    // The feed reports a label change on msg-1 alone - an update, not
    // an add and not a delete. That distinction is what the consumer
    // branches on.
    let hist = history(&gmail_app).await;
    let records = hist["history"].as_array().unwrap();
    assert!(
        records.iter().all(|r| r.get("messagesAdded").is_none()),
        "an update must not project as an add: {hist}"
    );
    let labelled: Vec<(&str, Vec<&str>)> = records
        .iter()
        .filter_map(|r| r.get("labelsAdded"))
        .filter_map(Value::as_array)
        .flatten()
        .map(|e| {
            (
                e["message"]["id"].as_str().unwrap(),
                e["labelIds"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(Value::as_str)
                    .collect(),
            )
        })
        .collect();
    assert_eq!(labelled.len(), 1, "history: {hist}");
    assert_eq!(labelled[0].0, "msg-1");
    assert!(labelled[0].1.contains(&"STARRED"), "history: {hist}");
}

/// A trigger fires once, on the nth match, and the counter is
/// per-trigger state of THIS run: `POST /test/fixture/reset` rewinds
/// the fixture and must zero it too, or a replayed script would fire
/// its `nth = 2` trigger on the first request of the second run.
#[tokio::test]
async fn reset_rewinds_the_trigger_counter_with_the_fixture() {
    let (jmap, gmail_app) = routers("fixtures/backfill-race-created.lua");
    let pages = walk(&gmail_app).await;
    assert_eq!(pages.len(), 4, "first walk: {pages:?}");

    let resp = jmap
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

    // Second walk: the fixture is back to six messages and the trigger
    // fires again at the same point, not immediately.
    let pages = walk(&gmail_app).await;
    assert_eq!(
        pages,
        vec![
            vec!["msg-5".to_string(), "msg-4".to_string()],
            vec!["msg-3".to_string(), "msg-2".to_string()],
            vec!["msg-1".to_string(), "msg-0".to_string()],
            vec!["msg-new".to_string()],
        ],
        "replayed walk must reproduce the first: {pages:?}"
    );
    let commands = commands(&jmap).await;
    let announce = announce_index(&commands, "arrive-new");
    let requests = page_request_indices(&commands);
    assert!(
        requests[0] < announce && announce < requests[1],
        "replayed trigger must fire between page 1 and page 2: {commands:?}"
    );
}
