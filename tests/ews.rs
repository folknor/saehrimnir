#![allow(clippy::unwrap_used)]

//! End-to-end EWS SOAP + Autodiscover tests, driven via
//! `tower::ServiceExt::oneshot` (no socket bind). Public-folder read
//! path (FindFolder / FindItem / GetItem) plus the streaming-
//! notification lifecycle (Subscribe / GetStreamingEvents /
//! Unsubscribe) wired through the PushHub.

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use tower::ServiceExt;

use saehrimnir::ews;
use saehrimnir::push::{AccountPush, PushHub};

fn fixture() -> saehrimnir::shared::FixtureHandle {
    saehrimnir::shared::handle(
        saehrimnir::fixture::load(std::path::Path::new("fixtures/ews-public.toml")).unwrap(),
    )
}

async fn post(router: axum::Router, path: &str, soap_body: &str) -> (StatusCode, String) {
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header(header::HOST, "127.0.0.1:9999")
                .header(header::CONTENT_TYPE, "text/xml; charset=utf-8")
                .body(Body::from(soap_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

fn envelope(inner: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?>\
<soap:Envelope xmlns:soap=\"http://schemas.xmlsoap.org/soap/envelope/\" \
xmlns:t=\"http://schemas.microsoft.com/exchange/services/2006/types\" \
xmlns:m=\"http://schemas.microsoft.com/exchange/services/2006/messages\">\
<soap:Body>{inner}</soap:Body></soap:Envelope>"
    )
}

#[tokio::test]
async fn autodiscover_returns_external_ews_url() {
    let app = ews::router(ews::AppState::for_test(fixture()));
    let body = envelope(
        "<GetUserSettingsRequestMessage xmlns=\"http://schemas.microsoft.com/exchange/2010/Autodiscover\">\
<Request><Users><User><Mailbox>user@example.com</Mailbox></User></Users>\
<RequestedSettings><Setting>ExternalEwsUrl</Setting></RequestedSettings></Request>\
</GetUserSettingsRequestMessage>",
    );
    let (status, out) = post(app, "/autodiscover/autodiscover.svc", &body).await;
    assert_eq!(status, StatusCode::OK);
    assert!(out.contains("<a:Name>ExternalEwsUrl</a:Name>"), "got: {out}");
    assert!(
        out.contains("/EWS/Exchange.asmx</a:Value>"),
        "EWS url missing: {out}"
    );
}

#[tokio::test]
async fn find_folder_shallow_lists_top_level_and_deep_lists_all() {
    let app = ews::router(ews::AppState::for_test(fixture()));

    // Shallow on publicfoldersroot -> the two top-level folders, not
    // the nested "Releases".
    let body = envelope(
        "<m:FindFolder Traversal=\"Shallow\"><m:FolderShape><t:BaseShape>Default</t:BaseShape>\
</m:FolderShape><m:ParentFolderIds>\
<t:DistinguishedFolderId Id=\"publicfoldersroot\"/></m:ParentFolderIds></m:FindFolder>",
    );
    let (_, out) = post(app.clone(), "/EWS/Exchange.asmx", &body).await;
    assert!(out.contains("<t:DisplayName>Engineering</t:DisplayName>"), "{out}");
    assert!(out.contains("<t:DisplayName>Announcements</t:DisplayName>"), "{out}");
    assert!(!out.contains("<t:DisplayName>Releases</t:DisplayName>"), "{out}");
    // Engineering reports one child (Releases) and one item.
    assert!(out.contains("<t:ChildFolderCount>1</t:ChildFolderCount>"), "{out}");

    // Deep -> the whole tree including Releases.
    let body = envelope(
        "<m:FindFolder Traversal=\"Deep\"><m:FolderShape><t:BaseShape>Default</t:BaseShape>\
</m:FolderShape><m:ParentFolderIds>\
<t:DistinguishedFolderId Id=\"publicfoldersroot\"/></m:ParentFolderIds></m:FindFolder>",
    );
    let (_, out) = post(app.clone(), "/EWS/Exchange.asmx", &body).await;
    assert!(out.contains("<t:DisplayName>Releases</t:DisplayName>"), "deep: {out}");

    // Shallow under Engineering -> just Releases.
    let body = envelope(
        "<m:FindFolder Traversal=\"Shallow\"><m:FolderShape><t:BaseShape>Default</t:BaseShape>\
</m:FolderShape><m:ParentFolderIds>\
<t:FolderId Id=\"pf-root-eng\"/></m:ParentFolderIds></m:FindFolder>",
    );
    let (_, out) = post(app, "/EWS/Exchange.asmx", &body).await;
    assert!(out.contains("<t:DisplayName>Releases</t:DisplayName>"), "{out}");
    assert!(!out.contains("<t:DisplayName>Engineering</t:DisplayName>"), "{out}");
}

#[tokio::test]
async fn find_item_lists_folder_items() {
    let app = ews::router(ews::AppState::for_test(fixture()));
    let body = envelope(
        "<m:FindItem Traversal=\"Shallow\"><m:ItemShape><t:BaseShape>Default</t:BaseShape>\
</m:ItemShape><m:ParentFolderIds><t:FolderId Id=\"pf-root-eng\"/></m:ParentFolderIds></m:FindItem>",
    );
    let (status, out) = post(app.clone(), "/EWS/Exchange.asmx", &body).await;
    assert_eq!(status, StatusCode::OK);
    assert!(out.contains("<t:ItemId Id=\"pi-eng-001\""), "{out}");
    assert!(out.contains("<t:Subject>Team sync notes</t:Subject>"), "{out}");
    assert!(out.contains("<t:EmailAddress>lead@example.com</t:EmailAddress>"), "{out}");
    // Announcements item does not leak into Engineering's listing.
    assert!(!out.contains("Office closed Friday"), "{out}");

    // IdOnly shape drops the subject/from projection.
    let body = envelope(
        "<m:FindItem Traversal=\"Shallow\"><m:ItemShape><t:BaseShape>IdOnly</t:BaseShape>\
</m:ItemShape><m:ParentFolderIds><t:FolderId Id=\"pf-root-eng\"/></m:ParentFolderIds></m:FindItem>",
    );
    let (_, out) = post(app.clone(), "/EWS/Exchange.asmx", &body).await;
    assert!(out.contains("<t:ItemId Id=\"pi-eng-001\""), "{out}");
    assert!(!out.contains("<t:Subject>"), "IdOnly leaked subject: {out}");

    // Unknown folder -> ErrorFolderNotFound.
    let body = envelope(
        "<m:FindItem Traversal=\"Shallow\"><m:ItemShape><t:BaseShape>Default</t:BaseShape>\
</m:ItemShape><m:ParentFolderIds><t:FolderId Id=\"pf-bogus\"/></m:ParentFolderIds></m:FindItem>",
    );
    let (_, out) = post(app, "/EWS/Exchange.asmx", &body).await;
    assert!(out.contains("ErrorFolderNotFound"), "{out}");
}

#[tokio::test]
async fn get_item_returns_body_and_per_item_error() {
    let app = ews::router(ews::AppState::for_test(fixture()));
    let body = envelope(
        "<m:GetItem><m:ItemShape><t:BaseShape>AllProperties</t:BaseShape></m:ItemShape>\
<m:ItemIds><t:ItemId Id=\"pi-announce-001\"/><t:ItemId Id=\"pi-bogus\"/></m:ItemIds></m:GetItem>",
    );
    let (status, out) = post(app, "/EWS/Exchange.asmx", &body).await;
    assert_eq!(status, StatusCode::OK);
    assert!(out.contains("<t:Subject>Office closed Friday</t:Subject>"), "{out}");
    assert!(
        out.contains("<t:Body BodyType=\"Text\">The office is closed this Friday.</t:Body>"),
        "{out}"
    );
    // The unknown id degrades to a per-item error, not a batch failure.
    assert!(out.contains("ErrorItemNotFound"), "{out}");
    assert!(out.contains("ResponseClass=\"Success\""), "{out}");
}

#[tokio::test]
async fn streaming_subscription_delivers_events_on_state_advance() {
    // Build the AppState by hand so the test holds the same PushHub the
    // router serves from, and can drive a state advance.
    let state = ews::AppState::for_test(fixture());
    let hub: PushHub = state.shared.push.clone();
    let app = ews::router(state);

    // Subscribe (no bearer -> primary account).
    let body = envelope(
        "<m:Subscribe><m:StreamingSubscriptionRequest>\
<t:FolderIds><t:DistinguishedFolderId Id=\"inbox\"/></t:FolderIds>\
<t:EventTypes><t:EventType>NewMailEvent</t:EventType></t:EventTypes>\
</m:StreamingSubscriptionRequest></m:Subscribe>",
    );
    let (_, out) = post(app.clone(), "/EWS/Exchange.asmx", &body).await;
    assert!(out.contains("<m:SubscriptionId>"), "{out}");
    let sub_id = out
        .split("<m:SubscriptionId>")
        .nth(1)
        .unwrap()
        .split("</m:SubscriptionId>")
        .next()
        .unwrap()
        .to_string();

    // Before any state advance, GetStreamingEvents is an OK heartbeat
    // with no notifications.
    let poll = envelope(&format!(
        "<m:GetStreamingEvents><m:SubscriptionIds>\
<t:SubscriptionId>{sub_id}</t:SubscriptionId></m:SubscriptionIds>\
<m:ConnectionTimeout>1</m:ConnectionTimeout></m:GetStreamingEvents>"
    ));
    let (_, out) = post(app.clone(), "/EWS/Exchange.asmx", &poll).await;
    assert!(out.contains("<m:ConnectionStatus>OK</m:ConnectionStatus>"), "{out}");
    assert!(!out.contains("<t:NewMailEvent>"), "unexpected event: {out}");

    // Drive a state advance on the subscription's account.
    hub.emit_state_advance(&[AccountPush {
        account_id: "account-primary".to_string(),
        email_address: "user@example.com".to_string(),
        state: "seed.1".to_string(),
        history_id: 2,
        has_calendars: false,
        has_contacts: false,
        change_type: "created".to_string(),
        resource_id: None,
    }]);

    // Now the poll drains the queued NewMailEvent.
    let (_, out) = post(app.clone(), "/EWS/Exchange.asmx", &poll).await;
    assert!(out.contains("<t:NewMailEvent>"), "event missing: {out}");
    assert!(out.contains("<t:Watermark>W"), "watermark missing: {out}");

    // Draining is one-shot: the next poll is empty again.
    let (_, out) = post(app.clone(), "/EWS/Exchange.asmx", &poll).await;
    assert!(!out.contains("<t:NewMailEvent>"), "event re-delivered: {out}");

    // Unsubscribe, then the subscription is gone (Closed).
    let unsub = envelope(&format!(
        "<m:Unsubscribe><m:SubscriptionId>{sub_id}</m:SubscriptionId></m:Unsubscribe>"
    ));
    let (_, out) = post(app.clone(), "/EWS/Exchange.asmx", &unsub).await;
    assert!(out.contains("ResponseClass=\"Success\""), "{out}");
    let (_, out) = post(app, "/EWS/Exchange.asmx", &poll).await;
    assert!(out.contains("<m:ConnectionStatus>Closed</m:ConnectionStatus>"), "{out}");
}
