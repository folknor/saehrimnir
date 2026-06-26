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
    routes::router(routes::AppState::for_test(saehrimnir::shared::handle(fix)))
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
    // jmap-small carries no contact folders, so contacts must NOT be
    // advertised - otherwise bifrost enters the contacts sync flow.
    assert!(!caps.contains_key("urn:ietf:params:jmap:contacts"));

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
    assert!(
        v2["methodResponses"][0][1]["ids"]
            .as_array()
            .unwrap()
            .is_empty()
    );
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
    let received = item["receivedAt"]
        .as_str()
        .expect("receivedAt is a UTCDate string");
    assert!(
        chrono::DateTime::parse_from_rfc3339(received).is_ok(),
        "receivedAt {received:?} is RFC3339",
    );
    let from = item["from"].as_array().unwrap();
    assert_eq!(from[0]["email"], "alice@example.com");

    let part_id = item["textBody"][0]["partId"].as_str().unwrap();
    assert_eq!(item["bodyValues"][part_id]["value"], "First message body.");
    // The three custom-header keys ratatoskr asks for are always present.
    for k in [
        "header:List-Unsubscribe:asText",
        "header:List-Unsubscribe-Post:asText",
        "header:Disposition-Notification-To:asText",
    ] {
        assert!(item.get(k).is_some(), "{k} missing");
    }
}

#[tokio::test]
async fn thread_get_null_ids_lists_every_thread() {
    // Bifrost's JMAP Account::open probes Thread/get during discovery;
    // before the method landed this returned an `unknownMethod` error
    // envelope and the account failed to open. Each fixture email
    // defaults its threadId to its own id, so the two emails are two
    // single-message threads, sorted by id.
    let v = jmap_call("Thread/get", json!({ "accountId": "account-1" }), "t0").await;
    let result = &v["methodResponses"][0][1];
    assert_eq!(v["methodResponses"][0][0], "Thread/get");
    assert_eq!(result["accountId"], "account-1");
    assert!(result["state"].is_string());
    assert_eq!(result["notFound"], json!([]));
    assert_eq!(
        result["list"],
        json!([
            { "id": "email-001", "emailIds": ["email-001"] },
            { "id": "email-002", "emailIds": ["email-002"] },
        ]),
    );
}

#[tokio::test]
async fn thread_get_explicit_ids_partitions_found_and_not_found() {
    let v = jmap_call(
        "Thread/get",
        json!({ "accountId": "account-1", "ids": ["email-002", "no-such-thread"] }),
        "t1",
    )
    .await;
    let result = &v["methodResponses"][0][1];
    assert_eq!(
        result["list"],
        json!([{ "id": "email-002", "emailIds": ["email-002"] }]),
    );
    assert_eq!(result["notFound"], json!(["no-such-thread"]));
}

#[tokio::test]
async fn thread_changes_projects_email_delta_onto_threads() {
    // bifrost drives Thread/changes on the first delta cycle after
    // open; before it existed the dispatcher returned unknownMethod.
    let app = router();

    // Seed state via an empty Email/get.
    let v = jmap_call_on(
        &app,
        "Email/get",
        json!({ "accountId": "account-1", "ids": [] }),
        "t0",
    )
    .await;
    let seed = v["methodResponses"][0][1]["state"]
        .as_str()
        .unwrap()
        .to_string();

    // No changes since the seed -> empty delta, state echoes back.
    let v = jmap_call_on(
        &app,
        "Thread/changes",
        json!({ "accountId": "account-1", "sinceState": seed }),
        "t1",
    )
    .await;
    let r = &v["methodResponses"][0][1];
    assert_eq!(v["methodResponses"][0][0], "Thread/changes");
    assert_eq!(r["created"], json!([]));
    assert_eq!(r["updated"], json!([]));
    assert_eq!(r["destroyed"], json!([]));
    assert_eq!(r["newState"], json!(seed));

    // Mutate email-001 (its threadId defaults to its own id), which
    // bumps the fixture state.
    jmap_call_on(
        &app,
        "Email/set",
        json!({
            "accountId": "account-1",
            "update": { "email-001": { "keywords/$flagged": true } },
        }),
        "t2",
    )
    .await;

    // Thread/changes since the seed now reports email-001's thread as
    // updated.
    let v = jmap_call_on(
        &app,
        "Thread/changes",
        json!({ "accountId": "account-1", "sinceState": seed }),
        "t3",
    )
    .await;
    let r = &v["methodResponses"][0][1];
    assert_eq!(r["created"], json!([]));
    assert_eq!(r["updated"], json!(["email-001"]));
    assert_eq!(r["destroyed"], json!([]));
    let new_state = r["newState"].as_str().unwrap().to_string();
    assert_ne!(new_state, seed);

    // From the post-mutation state, the delta is empty again.
    let v = jmap_call_on(
        &app,
        "Thread/changes",
        json!({ "accountId": "account-1", "sinceState": new_state }),
        "t4",
    )
    .await;
    assert_eq!(v["methodResponses"][0][1]["updated"], json!([]));

    // An unknown sinceState cannot be calculated.
    let v = jmap_call_on(
        &app,
        "Thread/changes",
        json!({ "accountId": "account-1", "sinceState": "bogus-state" }),
        "t5",
    )
    .await;
    assert_eq!(v["methodResponses"][0][0], "error");
    assert_eq!(v["methodResponses"][0][1]["type"], "cannotCalculateChanges");
}

fn contacts_router() -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/graph-contacts-small.toml")).unwrap();
    routes::router(routes::AppState::for_test(saehrimnir::shared::handle(fix)))
}

async fn contacts_jmap_call(method: &str, args: Value, call_id: &str) -> Value {
    let req_body = json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:contacts"],
        "methodCalls": [[method, args, call_id]],
    });
    let resp = contacts_router()
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
async fn contact_card_get_null_ids_projects_jscontact_cards() {
    // RFC 9610 / RFC 9553: every fixture contact projects to a
    // JSContact Card. bifrost's contacts sync reads back id,
    // addressBookIds (first key), name.full, and emails[*].address.
    let v = contacts_jmap_call("ContactCard/get", json!({ "accountId": "account-1" }), "c0").await;
    assert_eq!(v["methodResponses"][0][0], "ContactCard/get");
    let result = &v["methodResponses"][0][1];
    assert_eq!(result["accountId"], "account-1");
    assert!(result["state"].is_string());
    assert_eq!(result["notFound"], json!([]));

    // All four declared contacts, in declaration order.
    let list = result["list"].as_array().unwrap();
    assert_eq!(list.len(), 4);

    // contact-001: multi-address contact in the default folder.
    let alice = &list[0];
    assert_eq!(alice["@type"], "Card");
    assert_eq!(alice["version"], "1.0");
    assert_eq!(alice["id"], "contact-001");
    assert_eq!(alice["uid"], "contact-001");
    assert_eq!(alice["kind"], "individual");
    assert_eq!(alice["addressBookIds"], json!({ "cf-default": true }));
    assert_eq!(
        alice["name"],
        json!({ "@type": "Name", "full": "Alice Anderson" })
    );
    assert_eq!(
        alice["emails"],
        json!({
            "e1": { "@type": "EmailAddress", "address": "alice@example.com" },
            "e2": { "@type": "EmailAddress", "address": "alice.anderson@example.org" },
        }),
    );

    // contact-003: no display name, no emails - name/emails omitted,
    // envelope still well-formed.
    let bare = list.iter().find(|c| c["id"] == "contact-003").unwrap();
    assert!(bare.get("name").is_none());
    assert!(bare.get("emails").is_none());
    assert_eq!(bare["addressBookIds"], json!({ "cf-default": true }));

    // contact-100 lives in the sibling Vendors address book.
    let acme = list.iter().find(|c| c["id"] == "contact-100").unwrap();
    assert_eq!(acme["addressBookIds"], json!({ "cf-vendors": true }));
}

#[tokio::test]
async fn contact_card_get_explicit_ids_partitions_found_and_not_found() {
    let v = contacts_jmap_call(
        "ContactCard/get",
        json!({ "accountId": "account-1", "ids": ["contact-002", "no-such-contact"] }),
        "c1",
    )
    .await;
    let result = &v["methodResponses"][0][1];
    let list = result["list"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], "contact-002");
    assert_eq!(list[0]["name"]["full"], "Bob Bell");
    assert_eq!(result["notFound"], json!(["no-such-contact"]));
}

#[tokio::test]
async fn session_advertises_contacts_when_fixture_has_address_books() {
    let resp = contacts_router()
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
    let caps = v["capabilities"].as_object().unwrap();
    assert!(caps.contains_key("urn:ietf:params:jmap:contacts"));
    let acct_caps = v["accounts"]["account-1"]["accountCapabilities"]
        .as_object()
        .unwrap();
    assert!(acct_caps.contains_key("urn:ietf:params:jmap:contacts"));
    assert_eq!(
        v["primaryAccounts"]["urn:ietf:params:jmap:contacts"],
        "account-1"
    );
}

#[tokio::test]
async fn address_book_get_projects_contact_folders() {
    let v = contacts_jmap_call("AddressBook/get", json!({ "accountId": "account-1" }), "a0").await;
    assert_eq!(v["methodResponses"][0][0], "AddressBook/get");
    let result = &v["methodResponses"][0][1];
    let list = result["list"].as_array().unwrap();
    assert_eq!(list.len(), 2);

    let default = list.iter().find(|b| b["id"] == "cf-default").unwrap();
    assert_eq!(default["name"], "Contacts");
    assert_eq!(default["isDefault"], true);
    // myRights gates write capability in bifrost's address_book_from_jmap.
    assert_eq!(default["myRights"]["mayWrite"], true);
    assert_eq!(default["myRights"]["mayDelete"], true);

    let vendors = list.iter().find(|b| b["id"] == "cf-vendors").unwrap();
    assert_eq!(vendors["name"], "Vendors");
    assert_eq!(vendors["isDefault"], false);
}

#[tokio::test]
async fn contact_card_query_filters_and_paginates() {
    // inAddressBook filter scopes to one folder; ids sort by id.
    let v = contacts_jmap_call(
        "ContactCard/query",
        json!({
            "accountId": "account-1",
            "filter": { "inAddressBook": "cf-default" },
            "calculateTotal": true,
        }),
        "q0",
    )
    .await;
    let result = &v["methodResponses"][0][1];
    assert_eq!(
        result["ids"],
        json!(["contact-001", "contact-002", "contact-003"])
    );
    assert_eq!(result["total"], 3);

    // text filter matches display name...
    let v = contacts_jmap_call(
        "ContactCard/query",
        json!({ "accountId": "account-1", "filter": { "text": "acme" } }),
        "q1",
    )
    .await;
    assert_eq!(v["methodResponses"][0][1]["ids"], json!(["contact-100"]));

    // ...and matches an email address.
    let v = contacts_jmap_call(
        "ContactCard/query",
        json!({ "accountId": "account-1", "filter": { "text": "bob@example.com" } }),
        "q2",
    )
    .await;
    assert_eq!(v["methodResponses"][0][1]["ids"], json!(["contact-002"]));

    // limit paginates the full (unfiltered) account set, id-sorted.
    let v = contacts_jmap_call(
        "ContactCard/query",
        json!({ "accountId": "account-1", "limit": 2, "calculateTotal": true }),
        "q3",
    )
    .await;
    let result = &v["methodResponses"][0][1];
    assert_eq!(result["ids"], json!(["contact-001", "contact-002"]));
    assert_eq!(result["total"], 4);
    assert_eq!(result["position"], 0);
}

#[tokio::test]
async fn contact_card_set_and_changes_round_trip() {
    let app = contacts_router();

    // Create a card, capturing the pre/post state tokens.
    let v = jmap_call_on(
        &app,
        "ContactCard/set",
        json!({
            "accountId": "account-1",
            "create": {
                "c1": {
                    "@type": "Card",
                    "addressBookIds": { "cf-default": true },
                    "name": { "@type": "Name", "full": "New Person" },
                    "emails": { "e1": { "@type": "EmailAddress", "address": "new@example.com" } },
                },
            },
        }),
        "s0",
    )
    .await;
    let result = &v["methodResponses"][0][1];
    let new_id = result["created"]["c1"]["id"].as_str().unwrap().to_string();
    assert!(new_id.starts_with("mock-contact-"), "got {new_id}");
    let base_state = result["oldState"].as_str().unwrap().to_string();
    let after_create = result["newState"].as_str().unwrap().to_string();
    assert_ne!(base_state, after_create);

    // ContactCard/changes from the baseline surfaces the create.
    let v = jmap_call_on(
        &app,
        "ContactCard/changes",
        json!({ "accountId": "account-1", "sinceState": base_state }),
        "s1",
    )
    .await;
    let changes = &v["methodResponses"][0][1];
    assert_eq!(changes["created"], json!([new_id]));
    assert_eq!(changes["newState"], after_create);

    // The created card reads back through ContactCard/get.
    let v = jmap_call_on(
        &app,
        "ContactCard/get",
        json!({ "accountId": "account-1", "ids": [new_id.clone()] }),
        "s2",
    )
    .await;
    let card = &v["methodResponses"][0][1]["list"][0];
    assert_eq!(card["name"]["full"], "New Person");
    assert_eq!(card["emails"]["e1"]["address"], "new@example.com");
    assert_eq!(card["addressBookIds"], json!({ "cf-default": true }));

    // Update the name.
    let v = jmap_call_on(
        &app,
        "ContactCard/set",
        json!({
            "accountId": "account-1",
            "update": { new_id.clone(): { "name": { "@type": "Name", "full": "Renamed" } } },
        }),
        "s3",
    )
    .await;
    assert!(
        v["methodResponses"][0][1]["updated"]
            .as_object()
            .unwrap()
            .contains_key(&new_id)
    );
    let v = jmap_call_on(
        &app,
        "ContactCard/get",
        json!({ "accountId": "account-1", "ids": [new_id.clone()] }),
        "s4",
    )
    .await;
    assert_eq!(
        v["methodResponses"][0][1]["list"][0]["name"]["full"],
        "Renamed"
    );

    // Destroy.
    let v = jmap_call_on(
        &app,
        "ContactCard/set",
        json!({ "accountId": "account-1", "destroy": [new_id.clone()] }),
        "s5",
    )
    .await;
    assert_eq!(v["methodResponses"][0][1]["destroyed"], json!([new_id]));
    let v = jmap_call_on(
        &app,
        "ContactCard/get",
        json!({ "accountId": "account-1", "ids": [new_id.clone()] }),
        "s6",
    )
    .await;
    assert_eq!(v["methodResponses"][0][1]["notFound"], json!([new_id]));
}

fn attach_router() -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/jmap-attach.toml")).unwrap();
    routes::router(routes::AppState::for_test(saehrimnir::shared::handle(fix)))
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
async fn attachment_latency_tag_delays_jmap_download() {
    // Share one router so the latency we set is the same SharedHandles
    // the download endpoint consults. `attach_router()` builds a fresh
    // state per call.
    let app = attach_router();

    let _ = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/test/latency")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "per_protocol": { "attachment": 120 }
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let start = std::time::Instant::now();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/jmap/download/account-1/blob-att-001/sample.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let elapsed = start.elapsed();
    assert!(
        elapsed >= std::time::Duration::from_millis(100),
        "expected >=100ms attachment delay, got {elapsed:?}"
    );
}

#[tokio::test]
async fn attachment_latency_per_blob_override_targets_one_attachment() {
    // Base "attachment" stays 0 so the matched blob is the only one
    // that sleeps. Two downloads: the targeted blob is slow, an
    // unrelated 404 is fast. (Single-attachment fixture, so we use a
    // 404 download as the "fast" comparison since both share the
    // download() entry point.)
    let app = attach_router();
    let _ = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/test/latency")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "per_protocol": { "attachment:blob-att-001": 150 }
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let start = std::time::Instant::now();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/jmap/download/account-1/blob-att-001/sample.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let slow = start.elapsed();
    assert!(
        slow >= std::time::Duration::from_millis(120),
        "expected >=120ms for targeted blob, got {slow:?}"
    );

    let start = std::time::Instant::now();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/jmap/download/account-1/blob-other/x")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let fast = start.elapsed();
    assert!(
        fast < std::time::Duration::from_millis(50),
        "expected unrelated blob to skip the override, got {fast:?}"
    );
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
    let (fixture, dispatcher) = lua::load_source_with_dispatcher(scenario, "@cb-test").unwrap();
    routes::router(
        routes::AppState::for_test(saehrimnir::shared::handle(fixture))
            .with_dispatcher(Arc::new(dispatcher)),
    )
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
    routes::router(
        routes::AppState::for_test(saehrimnir::shared::handle(fix)).with_submission_log(log),
    )
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
        account_id: "account-1".to_string(),
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
    routes::router(
        routes::AppState::for_test(saehrimnir::shared::handle(fix))
            .with_submission_log(smtp_log)
            .with_request_log(request_log),
    )
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

/// `Email/get` request-log rows surface `accountId`, `ids[]`, and
/// `properties[]` in `detail`. Lets a delta-after-mutation script
/// distinguish a metadata-only get from a body-bearing one without
/// having to inspect the response shape.
#[tokio::test]
async fn jmap_request_log_surfaces_ids_and_properties() {
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
                    ["Email/get", {
                        "accountId": "account-1",
                        "ids": ["email-1", "email-2"],
                        "properties": ["id", "keywords"]
                    }, "c0"],
                    ["Email/query", { "accountId": "account-1" }, "c1"]
                ]
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let snap = request_log.snapshot();
    assert_eq!(snap[0].command, "Email/get");
    assert_eq!(snap[0].detail["account_id"], "account-1");
    assert_eq!(snap[0].detail["ids"], json!(["email-1", "email-2"]));
    assert_eq!(snap[0].detail["properties"], json!(["id", "keywords"]));

    // Email/query call doesn't carry ids/properties so those fields
    // stay absent (only call_id + account_id surface).
    assert_eq!(snap[1].command, "Email/query");
    assert_eq!(snap[1].detail["account_id"], "account-1");
    assert!(snap[1].detail.get("ids").is_none(), "{:?}", snap[1].detail);
    assert!(
        snap[1].detail.get("properties").is_none(),
        "{:?}",
        snap[1].detail
    );
}

#[tokio::test]
async fn test_requests_get_returns_snapshot_and_delete_clears() {
    let request_log = saehrimnir::request_log::RequestLog::default();
    request_log.record("imap", "CAPABILITY", json!({"tag": "a1"}));
    request_log.record("smtp", "EHLO", json!({"args": "client"}));

    let app = router_with_logs(
        saehrimnir::smtp::SubmissionLog::default(),
        request_log.clone(),
    );

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
async fn test_fixture_step_with_no_change_script_reports_end_of_script() {
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
    // Fixture has no change_script; cursor is past the (empty) end,
    // so the response is the boring end-of-script shape.
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["ok"], true);
    assert_eq!(v["applied"], false);
    assert!(v["step"].is_null());
}

// ── /test/latency + /test/snapshot-state + /test/requests?stable ────

#[tokio::test]
async fn test_latency_round_trips_through_post_and_get() {
    let app = router();

    // Default is empty.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/test/latency")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let v = body_json(resp).await;
    assert_eq!(v, json!({}));

    // Set both global and per-protocol.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/test/latency")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "global_ms": 5,
                        "per_protocol": { "graph": 25, "imap": 10 }
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["global"], 5);
    assert_eq!(v["graph"], 25);
    assert_eq!(v["imap"], 10);

    // Setting a key to 0 clears it.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/test/latency")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "per_protocol": { "graph": 0 } })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let v = body_json(resp).await;
    assert!(v.get("graph").is_none());
    assert_eq!(v["global"], 5);
    assert_eq!(v["imap"], 10);
}

#[tokio::test]
async fn test_latency_actually_delays_jmap_dispatch() {
    // Share the router so the latency we set above is the same
    // SharedHandles the JMAP call consults. The `router()` helper
    // builds a fresh AppState per call, so use a single instance.
    let app = router();

    let _ = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/test/latency")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "global_ms": 100 })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let req_body = json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
        "methodCalls": [["Mailbox/get",
                         { "accountId": "account-1", "ids": Value::Null },
                         "c0"]],
    });
    let start = std::time::Instant::now();
    let resp = app
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
    let elapsed = start.elapsed();
    assert!(
        elapsed >= std::time::Duration::from_millis(80),
        "expected >=80ms, got {elapsed:?}"
    );
}

#[tokio::test]
async fn test_latency_rejects_malformed() {
    let app = router();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/test/latency")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "global_ms": "five" })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_latency_rejects_values_above_cap() {
    // Without the cap, `u64::MAX` would deadlock every dispatch path
    // for ~584M years. The clamp is the documented defence; both
    // global and per-protocol values are gated.
    let app = router();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/test/latency")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "global_ms": u64::MAX })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_json(resp).await;
    assert!(
        v["detail"].as_str().unwrap().contains("exceeds cap"),
        "wrong detail: {v:?}"
    );

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/test/latency")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "per_protocol": { "graph": 999_999u64 }
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_snapshot_state_projects_fixture_image() {
    let app = router();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/test/snapshot-state")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["name"], "jmap-small");
    let mailboxes = v["mailboxes"].as_array().unwrap();
    assert!(!mailboxes.is_empty());
    assert!(mailboxes.iter().any(|m| m["role"] == "inbox"));
    let emails = v["emails"].as_array().unwrap();
    assert!(!emails.is_empty());
    // Body bytes deliberately not in snapshot.
    assert!(emails[0].get("body_text").is_none());
    assert!(emails[0]["received_at"].is_string());
}

#[tokio::test]
async fn test_requests_stable_strips_received_at() {
    let app = router();
    // Drive a JMAP call into the SAME router so the request log we
    // read from below sees the entry. (`jmap_call` builds a fresh
    // router with its own SharedHandles; not what we want here.)
    let req_body = json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
        "methodCalls": [["Mailbox/get",
                         { "accountId": "account-1", "ids": Value::Null },
                         "c0"]],
    });
    let _ = app
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

    // Without ?stable: received_at present.
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
    let v = body_json(resp).await;
    let arr = v.as_array().unwrap();
    assert!(arr.iter().any(|e| e.get("received_at").is_some()));

    // With ?stable=true: received_at gone.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/test/requests?stable=true")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let v = body_json(resp).await;
    let arr = v.as_array().unwrap();
    assert!(!arr.is_empty());
    for e in arr {
        assert!(
            e.get("received_at").is_none(),
            "stable mode must strip received_at; got {e:?}"
        );
        assert!(e["protocol"].is_string());
        assert!(e["command"].is_string());
    }
}

// ── /oauth/* + /test/oauth/invalidate ───────────────────────────────

fn router_with_token_store(store: saehrimnir::oauth::TokenStore) -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/jmap-small.toml")).unwrap();
    routes::router(
        routes::AppState::for_test(saehrimnir::shared::handle(fix)).with_token_store(store),
    )
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
    assert!(
        v["access_token"]
            .as_str()
            .unwrap()
            .starts_with("mock-access-")
    );
}

#[tokio::test]
async fn oauth_refresh_grant_resolves_account_from_refresh_token() {
    // A refresh on a multi-account fixture: ratatoskr's refresh request
    // carries only refresh_token + client_id + grant_type (no
    // account_id), so the secondary account's refresh must mint a
    // secondary-bound access token by looking the refresh token up in
    // the store - not silently fall back to primary (which would let
    // the secondary read the primary's mailbox).
    let store = saehrimnir::oauth::TokenStore::default();
    let fix = fixture::load(std::path::Path::new("fixtures/multi-account-small.toml")).unwrap();
    let app = routes::router(
        routes::AppState::for_test(saehrimnir::shared::handle(fix)).with_token_store(store.clone()),
    );

    // Mint the initial token pair bound to the secondary account (the
    // authorization_code leg, which still accepts the account_id knob).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/oauth/token")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(
                    "grant_type=authorization_code\
                    &code=fixture-code\
                    &client_id=test\
                    &account_id=account-secondary",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let refresh = v["refresh_token"].as_str().unwrap().to_string();

    // Refresh with ONLY the refresh token + client_id.
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/oauth/token")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "grant_type=refresh_token&refresh_token={refresh}&client_id=test"
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let access = v["access_token"].as_str().unwrap();

    // The minted access token is bound to the secondary, not primary.
    let bound = store
        .snapshot()
        .into_iter()
        .find(|t| t.token == access)
        .expect("minted access token present in store");
    assert_eq!(bound.account_id, "account-secondary");
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
    let token = store.mint("authorization_code", "account-1", 0xdead_beef);

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
    let token = store.mint("authorization_code", "account-1", 1);
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
    let _ = store.mint("authorization_code", "account-1", 1);
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
    routes::router(
        routes::AppState::for_test(saehrimnir::shared::handle(fix)).with_token_store(store),
    )
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
    let token = store.mint("authorization_code", "account-1", 1);
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

/// End-to-end revoked-token-recovery walk against
/// `fixtures/jmap-oauth.toml` (the named bearer-enforced variant of
/// jmap-small). Proves that the fixture parses with `oauth.enforce
/// = true` and that the full sync -> revoke -> 401 -> re-mint -> sync
/// cycle works through the same JMAP endpoint a ratatoskr harness
/// would drive.
#[tokio::test]
async fn jmap_oauth_fixture_drives_revoked_token_recovery_flow() {
    let fix = fixture::load(std::path::Path::new("fixtures/jmap-oauth.toml")).unwrap();
    assert!(fix.oauth.enforce, "fixture must enable bearer enforcement");

    let store = saehrimnir::oauth::TokenStore::default();
    let app = routes::router(
        routes::AppState::for_test(saehrimnir::shared::handle(fix)).with_token_store(store.clone()),
    );

    // Step 1: client mints a token via the OAuth provider.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/oauth/token")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("grant_type=authorization_code&code=abc"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let token = v["access_token"].as_str().unwrap().to_string();

    // Step 2: bearer-gated sync succeeds.
    let resp = app
        .clone()
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

    // Step 3: harness revokes the token.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/test/oauth/invalidate")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({ "token": token }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Step 4: same request now 401s with the JMAP forbidden envelope.
    let resp = app
        .clone()
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
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let v = body_json(resp).await;
    assert_eq!(v["type"], "urn:ietf:params:jmap:error:forbidden");

    // Step 5: re-mint and sync again.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/oauth/token")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("grant_type=authorization_code&code=xyz"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let fresh = v["access_token"].as_str().unwrap();
    assert_ne!(fresh, token, "re-mint must produce a distinct token");

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/jmap/session")
                .header(header::HOST, "127.0.0.1:9999")
                .header(header::AUTHORIZATION, format!("Bearer {fresh}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ── Email/set + Mailbox/set round-trip ──────────────────────────────

/// Helper for multi-call round-trip tests: pin a single router so
/// every method-call hits the same fixture handle.
async fn jmap_call_on(router: &axum::Router, method: &str, args: Value, call_id: &str) -> Value {
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

/// `Email/set update` flips a keyword on an existing fixture email,
/// then `Email/changes(sinceState=fixture-state)` lists the same id
/// in `updated`. Proves the per-state change log walks correctly
/// across at least one mutation.
#[tokio::test]
async fn email_set_update_round_trips_through_email_changes() {
    let app = router();

    let v = jmap_call_on(
        &app,
        "Email/set",
        json!({
            "accountId": "account-1",
            "update": {
                "email-001": { "keywords/$seen": true }
            }
        }),
        "c0",
    )
    .await;
    let body = &v["methodResponses"][0][1];
    assert_eq!(body["accountId"], "account-1");
    assert_eq!(body["oldState"], "fixture-state");
    let new_state = body["newState"].as_str().unwrap().to_string();
    assert_ne!(new_state, "fixture-state", "state must bump on mutation");
    assert_eq!(body["updated"], json!({ "email-001": null }));
    assert!(body["notUpdated"].is_null());

    // Email/changes from the original seed surfaces email-001 in
    // `updated` and reports the new state.
    let v = jmap_call_on(
        &app,
        "Email/changes",
        json!({"accountId": "account-1", "sinceState": "fixture-state"}),
        "c1",
    )
    .await;
    let body = &v["methodResponses"][0][1];
    assert_eq!(body["oldState"], "fixture-state");
    assert_eq!(body["newState"], new_state);
    assert_eq!(body["created"], json!([]));
    assert_eq!(body["updated"], json!(["email-001"]));
    assert_eq!(body["destroyed"], json!([]));
}

/// `Email/set destroy` removes an email; `Email/changes` lists it
/// under `destroyed`; `Email/get` reports the id under `notFound`.
#[tokio::test]
async fn email_set_destroy_round_trips() {
    let app = router();

    let v = jmap_call_on(
        &app,
        "Email/set",
        json!({"accountId": "account-1", "destroy": ["email-001"]}),
        "c0",
    )
    .await;
    let body = &v["methodResponses"][0][1];
    assert_eq!(body["destroyed"], json!(["email-001"]));
    assert!(body["notDestroyed"].is_null());

    let v = jmap_call_on(
        &app,
        "Email/changes",
        json!({"accountId": "account-1", "sinceState": "fixture-state"}),
        "c1",
    )
    .await;
    assert_eq!(
        v["methodResponses"][0][1]["destroyed"],
        json!(["email-001"])
    );

    let v = jmap_call_on(
        &app,
        "Email/get",
        json!({"accountId": "account-1", "ids": ["email-001"]}),
        "c2",
    )
    .await;
    let body = &v["methodResponses"][0][1];
    assert_eq!(body["list"], json!([]));
    assert_eq!(body["notFound"], json!(["email-001"]));
}

/// `Email/set create` assigns a deterministic `mock-email-N` id;
/// the new email surfaces in the next `Email/changes` (`created`)
/// and in `Email/get` (`list`).
#[tokio::test]
async fn email_set_create_round_trips_through_email_get() {
    let app = router();

    let v = jmap_call_on(
        &app,
        "Email/set",
        json!({
            "accountId": "account-1",
            "create": {
                "draft": {
                    "mailboxIds": { "mbx-inbox": true },
                    "keywords": { "$draft": true }
                }
            }
        }),
        "c0",
    )
    .await;
    let body = &v["methodResponses"][0][1];
    let server_id = body["created"]["draft"]["id"].as_str().unwrap().to_string();
    assert_eq!(server_id, "mock-email-3");
    assert!(body["notCreated"].is_null());

    let v = jmap_call_on(
        &app,
        "Email/changes",
        json!({"accountId": "account-1", "sinceState": "fixture-state"}),
        "c1",
    )
    .await;
    assert_eq!(
        v["methodResponses"][0][1]["created"],
        json!([server_id.clone()])
    );

    let v = jmap_call_on(
        &app,
        "Email/get",
        json!({"accountId": "account-1", "ids": [server_id.clone()]}),
        "c2",
    )
    .await;
    let body = &v["methodResponses"][0][1];
    assert_eq!(body["list"][0]["id"], server_id);
    assert_eq!(body["list"][0]["mailboxIds"], json!({ "mbx-inbox": true }));
    assert_eq!(body["list"][0]["keywords"], json!({ "$draft": true }));
}

/// Created-then-destroyed in the same change window cancels per
/// RFC 8620 §5.2: the surviving delta lists neither id.
#[tokio::test]
async fn mailbox_set_create_then_destroy_cancels_in_changes() {
    let app = router();

    let v = jmap_call_on(
        &app,
        "Mailbox/set",
        json!({
            "accountId": "account-1",
            "create": { "scratch": { "name": "Scratch" } }
        }),
        "c0",
    )
    .await;
    let body = &v["methodResponses"][0][1];
    let server_id = body["created"]["scratch"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(server_id, "mock-mailbox-3");

    let v = jmap_call_on(
        &app,
        "Mailbox/set",
        json!({"accountId": "account-1", "destroy": [server_id.clone()]}),
        "c1",
    )
    .await;
    assert_eq!(
        v["methodResponses"][0][1]["destroyed"],
        json!([server_id.clone()])
    );

    let v = jmap_call_on(
        &app,
        "Mailbox/changes",
        json!({"accountId": "account-1", "sinceState": "fixture-state"}),
        "c2",
    )
    .await;
    let body = &v["methodResponses"][0][1];
    assert_eq!(body["created"], json!([]));
    assert_eq!(body["destroyed"], json!([]));
}

/// Destroying a non-empty mailbox fails with `mailboxHasEmail`.
#[tokio::test]
async fn mailbox_set_destroy_rejects_non_empty_mailbox() {
    let app = router();

    let v = jmap_call_on(
        &app,
        "Mailbox/set",
        json!({"accountId": "account-1", "destroy": ["mbx-inbox"]}),
        "c0",
    )
    .await;
    let body = &v["methodResponses"][0][1];
    assert!(body["destroyed"].is_null());
    assert_eq!(body["notDestroyed"]["mbx-inbox"]["type"], "mailboxHasEmail");
    assert_eq!(body["oldState"], "fixture-state");
    assert_eq!(body["newState"], "fixture-state");
}

/// `ifInState` mismatch short-circuits the envelope and leaves the
/// fixture untouched.
#[tokio::test]
async fn email_set_if_in_state_mismatch_rejects_envelope() {
    let app = router();

    let v = jmap_call_on(
        &app,
        "Email/set",
        json!({
            "accountId": "account-1",
            "ifInState": "wrong-state",
            "update": { "email-001": { "keywords/$seen": true } }
        }),
        "c0",
    )
    .await;
    assert_eq!(v["methodResponses"][0][0], "error");
    assert_eq!(v["methodResponses"][0][1]["type"], "stateMismatch");

    let v = jmap_call_on(
        &app,
        "Email/changes",
        json!({"accountId": "account-1", "sinceState": "fixture-state"}),
        "c1",
    )
    .await;
    let body = &v["methodResponses"][0][1];
    assert_eq!(body["oldState"], "fixture-state");
    assert_eq!(body["newState"], "fixture-state");
    assert_eq!(body["updated"], json!([]));
}

// ── Multi-account fixture (Stage 2) ─────────────────────────────────
//
// Stage 2 lands per-resource `account_id` plus JMAP multi-account
// scoping: the session resource now lists every declared account,
// and JMAP method handlers honour the `accountId` argument by
// reading only that account's resources. Non-JMAP protocols (Graph
// `/me/...`, IMAP, SMTP, Gmail, gcal, People) still scope to the
// primary - Stage 3 grows per-account routing for them.

fn multi_account_router() -> axum::Router {
    let fix = fixture::load(std::path::Path::new("fixtures/multi-account-small.toml")).unwrap();
    routes::router(routes::AppState::for_test(saehrimnir::shared::handle(fix)))
}

#[tokio::test]
async fn multi_account_jmap_session_lists_every_declared_account() {
    let resp = multi_account_router()
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
    let accounts = v.get("accounts").unwrap().as_object().unwrap();
    // Stage 2 contract: every declared account is advertised. The
    // primary appears in `primaryAccounts`; per-account capabilities
    // are derived from that account's own resources.
    assert_eq!(accounts.len(), 2);
    assert!(accounts.contains_key("account-primary"));
    assert!(accounts.contains_key("account-secondary"));
    assert_eq!(
        v["primaryAccounts"]["urn:ietf:params:jmap:mail"],
        "account-primary"
    );
    assert_eq!(v["username"], "primary@example.com");
}

async fn multi_account_jmap_call(method: &str, args: Value) -> Value {
    let req_body = json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
        "methodCalls": [[method, args, "c0"]],
    });
    let resp = multi_account_router()
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
    body_json(resp).await["methodResponses"][0].clone()
}

#[tokio::test]
async fn multi_account_mailbox_get_scopes_by_accountid() {
    // accountId=primary returns primary's mailbox only.
    let resp =
        multi_account_jmap_call("Mailbox/get", json!({ "accountId": "account-primary" })).await;
    assert_eq!(resp[0], "Mailbox/get");
    let body = &resp[1];
    assert_eq!(body["accountId"], "account-primary");
    let list = body["list"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], "mbx-primary-inbox");

    // accountId=secondary returns secondary's mailbox only.
    let resp =
        multi_account_jmap_call("Mailbox/get", json!({ "accountId": "account-secondary" })).await;
    let body = &resp[1];
    assert_eq!(body["accountId"], "account-secondary");
    let list = body["list"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], "mbx-secondary-inbox");
}

#[tokio::test]
async fn multi_account_email_get_scopes_by_accountid() {
    // accountId=primary returns primary's email only.
    let resp = multi_account_jmap_call(
        "Email/get",
        json!({
            "accountId": "account-primary",
            "ids": ["email-primary-001", "email-secondary-001"],
        }),
    )
    .await;
    let body = &resp[1];
    let list = body["list"].as_array().unwrap();
    let not_found = body["notFound"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], "email-primary-001");
    // Secondary's email is invisible under accountId=primary.
    assert_eq!(not_found, &vec![json!("email-secondary-001")]);

    // Symmetric assertion for secondary.
    let resp = multi_account_jmap_call(
        "Email/get",
        json!({
            "accountId": "account-secondary",
            "ids": ["email-primary-001", "email-secondary-001"],
        }),
    )
    .await;
    let body = &resp[1];
    let list = body["list"].as_array().unwrap();
    let not_found = body["notFound"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], "email-secondary-001");
    assert_eq!(not_found, &vec![json!("email-primary-001")]);
}

#[tokio::test]
async fn multi_account_unknown_accountid_returns_account_not_found() {
    let resp =
        multi_account_jmap_call("Mailbox/get", json!({ "accountId": "account-bogus" })).await;
    assert_eq!(resp[0], "error");
    assert_eq!(resp[1]["type"], "accountNotFound");
}

#[tokio::test]
async fn multi_account_email_changes_partition_destroyed_by_account() {
    // A destroy on the secondary's email lands in the change log;
    // primary's per-account walker must NOT see the tombstone, and
    // secondary's must. Pre-fix `email_delta_since` was global and
    // primary's /changes would surface every secondary mutation too.
    let app = multi_account_router();

    // Capture the load-time state token (= fixture seed) so we can
    // walk forward from it after the mutation.
    let v = jmap_call_on(
        &app,
        "Email/get",
        json!({ "accountId": "account-primary", "ids": [] }),
        "c0",
    )
    .await;
    let seed = v["methodResponses"][0][1]["state"]
        .as_str()
        .unwrap()
        .to_string();

    // Destroy secondary's only email.
    let v = jmap_call_on(
        &app,
        "Email/set",
        json!({
            "accountId": "account-secondary",
            "destroy": ["email-secondary-001"],
        }),
        "c1",
    )
    .await;
    let body = &v["methodResponses"][0][1];
    assert_eq!(body["destroyed"], json!(["email-secondary-001"]));

    // Primary's /changes walk from the seed sees nothing.
    let v = jmap_call_on(
        &app,
        "Email/changes",
        json!({ "accountId": "account-primary", "sinceState": seed }),
        "c2",
    )
    .await;
    let body = &v["methodResponses"][0][1];
    assert_eq!(
        body["destroyed"],
        json!([]),
        "primary's /changes leaked secondary's destroy: {body}"
    );
    assert_eq!(body["created"], json!([]));
    assert_eq!(body["updated"], json!([]));

    // Secondary's /changes walk from the seed sees the destroy.
    let v = jmap_call_on(
        &app,
        "Email/changes",
        json!({ "accountId": "account-secondary", "sinceState": seed }),
        "c3",
    )
    .await;
    let body = &v["methodResponses"][0][1];
    assert_eq!(body["destroyed"], json!(["email-secondary-001"]));
}

#[tokio::test]
async fn multi_account_email_changes_partition_updates_by_account() {
    // A keyword flip on the secondary's email must not surface in
    // primary's /changes walk. Pre-fix the global walker reported
    // every update regardless of account.
    let app = multi_account_router();

    // Capture the seed.
    let v = jmap_call_on(
        &app,
        "Email/get",
        json!({ "accountId": "account-primary", "ids": [] }),
        "c0",
    )
    .await;
    let seed = v["methodResponses"][0][1]["state"]
        .as_str()
        .unwrap()
        .to_string();

    // Flip $seen on secondary's email.
    let v = jmap_call_on(
        &app,
        "Email/set",
        json!({
            "accountId": "account-secondary",
            "update": { "email-secondary-001": { "keywords/$seen": true } },
        }),
        "c1",
    )
    .await;
    assert_eq!(
        v["methodResponses"][0][1]["updated"],
        json!({ "email-secondary-001": null })
    );

    // Primary's /changes walk: empty.
    let v = jmap_call_on(
        &app,
        "Email/changes",
        json!({ "accountId": "account-primary", "sinceState": seed }),
        "c2",
    )
    .await;
    let body = &v["methodResponses"][0][1];
    assert_eq!(body["updated"], json!([]));
    assert_eq!(body["created"], json!([]));
    assert_eq!(body["destroyed"], json!([]));

    // Secondary's /changes walk: the update.
    let v = jmap_call_on(
        &app,
        "Email/changes",
        json!({ "accountId": "account-secondary", "sinceState": seed }),
        "c3",
    )
    .await;
    let body = &v["methodResponses"][0][1];
    assert_eq!(body["updated"], json!(["email-secondary-001"]));
}

#[tokio::test]
async fn multi_account_mailbox_changes_partition_by_account() {
    // Create + destroy a mailbox on the secondary; primary's
    // /changes must not surface it.
    let app = multi_account_router();

    let v = jmap_call_on(
        &app,
        "Mailbox/get",
        json!({ "accountId": "account-primary", "ids": [] }),
        "c0",
    )
    .await;
    let seed = v["methodResponses"][0][1]["state"]
        .as_str()
        .unwrap()
        .to_string();

    let v = jmap_call_on(
        &app,
        "Mailbox/set",
        json!({
            "accountId": "account-secondary",
            "create": { "scratch": { "name": "Scratch" } },
        }),
        "c1",
    )
    .await;
    let server_id = v["methodResponses"][0][1]["created"]["scratch"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let v = jmap_call_on(
        &app,
        "Mailbox/changes",
        json!({ "accountId": "account-primary", "sinceState": seed }),
        "c2",
    )
    .await;
    let body = &v["methodResponses"][0][1];
    assert_eq!(body["created"], json!([]));
    assert_eq!(body["updated"], json!([]));
    assert_eq!(body["destroyed"], json!([]));

    let v = jmap_call_on(
        &app,
        "Mailbox/changes",
        json!({ "accountId": "account-secondary", "sinceState": seed }),
        "c3",
    )
    .await;
    let body = &v["methodResponses"][0][1];
    assert_eq!(body["created"], json!([server_id]));
}

#[tokio::test]
async fn multi_account_secondary_write_does_not_move_primary_state() {
    // Faithful mirror of the ratatoskr `mutate_other_account`
    // scenario, the regression notes/per-account-state.md targets:
    //   1. Initial sync of primary -> persist primary's Email state
    //      token S_p (the value Email/get returns).
    //   2. Raw Email/set on the SECONDARY flips $seen. Its own
    //      newState must advance (oldState != newState) ...
    //   3. ... but primary's Email/changes(sinceState = S_p) must
    //      report newState == S_p with empty arrays, so ratatoskr
    //      issues zero Email/get. The pre-per-account-state mock
    //      bumped one global counter on step 2 and reported
    //      primary's whole object set as `updated` in step 3.
    let app = multi_account_router();

    // Step 1: primary's baseline Email state token.
    let v = jmap_call_on(
        &app,
        "Email/get",
        json!({ "accountId": "account-primary", "ids": [] }),
        "c0",
    )
    .await;
    let s_p = v["methodResponses"][0][1]["state"]
        .as_str()
        .unwrap()
        .to_string();

    // Step 2: secondary write advances the SECONDARY's token only.
    let v = jmap_call_on(
        &app,
        "Email/set",
        json!({
            "accountId": "account-secondary",
            "update": { "email-secondary-001": { "keywords/$seen": true } },
        }),
        "c1",
    )
    .await;
    let body = &v["methodResponses"][0][1];
    assert_eq!(body["updated"], json!({ "email-secondary-001": null }));
    assert_ne!(
        body["oldState"], body["newState"],
        "secondary write must advance the secondary's own state token: {body}"
    );

    // Step 3: primary's delta is a no-op. newState == S_p, no objects.
    let v = jmap_call_on(
        &app,
        "Email/changes",
        json!({ "accountId": "account-primary", "sinceState": s_p }),
        "c2",
    )
    .await;
    let body = &v["methodResponses"][0][1];
    assert_eq!(
        body["newState"],
        json!(s_p),
        "secondary write moved primary's Email state token: {body}"
    );
    assert_eq!(
        body["created"],
        json!([]),
        "primary leaked a created: {body}"
    );
    assert_eq!(
        body["updated"],
        json!([]),
        "primary leaked an updated: {body}"
    );
    assert_eq!(
        body["destroyed"],
        json!([]),
        "primary leaked a destroyed: {body}"
    );

    // Mailbox mirror: the same isolation for Mailbox/changes.
    let v = jmap_call_on(
        &app,
        "Mailbox/get",
        json!({ "accountId": "account-primary", "ids": [] }),
        "c3",
    )
    .await;
    let mb_s_p = v["methodResponses"][0][1]["state"]
        .as_str()
        .unwrap()
        .to_string();
    jmap_call_on(
        &app,
        "Mailbox/set",
        json!({
            "accountId": "account-secondary",
            "create": { "scratch": { "name": "Scratch" } },
        }),
        "c4",
    )
    .await;
    let v = jmap_call_on(
        &app,
        "Mailbox/changes",
        json!({ "accountId": "account-primary", "sinceState": mb_s_p }),
        "c5",
    )
    .await;
    let body = &v["methodResponses"][0][1];
    assert_eq!(
        body["newState"],
        json!(mb_s_p),
        "secondary write moved primary's Mailbox state: {body}"
    );
    assert_eq!(body["created"], json!([]));
    assert_eq!(body["updated"], json!([]));
    assert_eq!(body["destroyed"], json!([]));
}

#[tokio::test]
async fn multi_account_email_query_scopes_by_accountid() {
    // The query path is Email/query with a filter; assert that
    // primary's filter sees only primary's emails.
    let resp =
        multi_account_jmap_call("Email/query", json!({ "accountId": "account-secondary" })).await;
    let body = &resp[1];
    assert_eq!(body["accountId"], "account-secondary");
    let ids = body["ids"].as_array().unwrap();
    assert_eq!(ids.len(), 1);
    assert_eq!(ids[0], "email-secondary-001");
}

#[tokio::test]
async fn oauth_token_endpoint_accepts_account_id_form_field() {
    // The mock honours an optional `account_id` on the token-form
    // body so harness scripts can mint a token bound to a specific
    // declared account. Real OAuth providers don't expose this knob;
    // the wire shape is invisible to clients that don't set it.
    let fix = fixture::load(std::path::Path::new("fixtures/multi-account-small.toml")).unwrap();
    let app = routes::router(routes::AppState::for_test(saehrimnir::shared::handle(fix)));

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/oauth/token")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(
                    "grant_type=authorization_code&account_id=account-secondary",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let token = v["access_token"].as_str().unwrap();

    // Userinfo with the resulting token surfaces secondary's claims.
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
    assert_eq!(v["sub"], "account-secondary");
    assert_eq!(v["email"], "secondary@example.com");
}

#[tokio::test]
async fn jmap_session_primary_accounts_follow_bearer_token() {
    // Regression: `primaryAccounts.{mail,calendars}` and `username`
    // used to pin to the fixture primary regardless of bearer, so
    // ratatoskr's second-account JMAP sync routed every method call
    // under the wrong accountId. The session resource now resolves
    // the bearer via `oauth::account_from_bearer` (same helper Gmail
    // / Graph / People / IMAP-SASL use) and pins primaryAccounts to
    // the caller. No-bearer / unknown-token requests still fall back
    // to primary - see the no-bearer test above.
    let fix = fixture::load(std::path::Path::new("fixtures/multi-account-small.toml")).unwrap();
    let app = routes::router(routes::AppState::for_test(saehrimnir::shared::handle(fix)));

    // Mint a token bound to the secondary account via the same
    // form-field path harness scripts use.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/oauth/token")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(
                    "grant_type=authorization_code&account_id=account-secondary",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let secondary_token = body_json(resp).await["access_token"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/jmap/session")
                .header(header::AUTHORIZATION, format!("Bearer {secondary_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(
        v["primaryAccounts"]["urn:ietf:params:jmap:mail"],
        "account-secondary"
    );
    assert_eq!(
        v["primaryAccounts"]["urn:ietf:params:jmap:core"],
        "account-secondary"
    );
    assert_eq!(v["username"], "secondary@example.com");
    // The accounts map still enumerates every declared account
    // (the bearer doesn't filter the map - only the primary
    // pointers).
    let accounts = v["accounts"].as_object().unwrap();
    assert!(accounts.contains_key("account-primary"));
    assert!(accounts.contains_key("account-secondary"));
}

#[tokio::test]
async fn oauth_token_endpoint_rejects_unknown_account_id() {
    let fix = fixture::load(std::path::Path::new("fixtures/multi-account-small.toml")).unwrap();
    let app = routes::router(routes::AppState::for_test(saehrimnir::shared::handle(fix)));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/oauth/token")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(
                    "grant_type=authorization_code&account_id=account-bogus",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_json(resp).await;
    assert_eq!(v["error"], "invalid_request");
}

#[tokio::test]
async fn multi_account_oauth_userinfo_returns_primary_claims() {
    let store = saehrimnir::oauth::TokenStore::default();
    let token = store.mint("authorization_code", "account-primary", 0xdead_beef);
    let fix = fixture::load(std::path::Path::new("fixtures/multi-account-small.toml")).unwrap();
    let app = routes::router(
        routes::AppState::for_test(saehrimnir::shared::handle(fix)).with_token_store(store),
    );

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
    assert_eq!(v["sub"], "account-primary");
    assert_eq!(v["email"], "primary@example.com");
}
