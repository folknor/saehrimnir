#![allow(clippy::unwrap_used)]

//! End-to-end EWS SOAP + Autodiscover tests, driven via
//! `tower::ServiceExt::oneshot` (no socket bind). SOAP + POX
//! Autodiscover, the public-folder read path (FindFolder / FindItem /
//! GetItem / GetAttachment), the Graph co-mount, and the streaming-
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
    handle_for("fixtures/ews-public.toml")
}

fn handle_for(path: &str) -> saehrimnir::shared::FixtureHandle {
    saehrimnir::shared::handle(saehrimnir::fixture::load(std::path::Path::new(path)).unwrap())
}

/// The EWS router as it is mounted on the Graph listener: merged into
/// the Graph router over the same shared handles. `main.rs` does the
/// same thing, because the harness driving this mock has no EWS
/// endpoint variable and can only reach the surface through Graph.
fn graph_mounted_router(fixture: saehrimnir::shared::FixtureHandle) -> axum::Router {
    saehrimnir::graph::router(saehrimnir::graph::AppState::for_test(
        std::sync::Arc::clone(&fixture),
    ))
    .merge(ews::router(ews::AppState::for_test(fixture)))
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
    assert!(
        out.contains("<a:Name>ExternalEwsUrl</a:Name>"),
        "got: {out}"
    );
    assert!(
        out.contains("/EWS/Exchange.asmx</a:Value>"),
        "EWS url missing: {out}"
    );
}

/// POX Autodiscover (`/autodiscover/autodiscover.xml`) is the
/// delegate / shared-mailbox discovery channel: the response describes
/// the requested user and lists every *other* declared account as an
/// alternative mailbox.
#[tokio::test]
async fn pox_autodiscover_projects_other_accounts_as_alternative_mailboxes() {
    let app = ews::router(ews::AppState::for_test(fixture()));
    let body = "<Autodiscover \
xmlns=\"http://schemas.microsoft.com/exchange/autodiscover/outlook/requestschema/2006\">\
<Request><EMailAddress>user@example.com</EMailAddress>\
<AcceptableResponseSchema>\
http://schemas.microsoft.com/exchange/autodiscover/outlook/responseschema/2006a\
</AcceptableResponseSchema></Request></Autodiscover>";
    let (status, out) = post(app, "/autodiscover/autodiscover.xml", body).await;
    assert_eq!(status, StatusCode::OK);

    // The requesting user is the account whose name matches.
    assert!(
        out.contains("<EMailAddress>user@example.com</EMailAddress>"),
        "{out}"
    );
    // ... and it binds back to this process's EWS endpoint.
    assert!(out.contains("<EwsUrl>"), "{out}");
    assert!(out.contains("/EWS/Exchange.asmx</EwsUrl>"), "{out}");

    // The other declared account shows up as an alternative mailbox,
    // and the requesting one does not.
    assert!(out.contains("<AlternativeMailboxes>"), "{out}");
    assert!(
        out.contains("<SmtpAddress>shared-team@example.com</SmtpAddress>"),
        "alternative mailbox missing: {out}"
    );
    assert!(
        !out.contains("<SmtpAddress>user@example.com</SmtpAddress>"),
        "requesting mailbox leaked into the alternatives: {out}"
    );
    assert_eq!(
        out.matches("<AlternativeMailbox>").count(),
        1,
        "exactly one alternative expected: {out}"
    );
}

/// A request naming an account we do not know falls back to the
/// primary, and then every other account (including the shared one) is
/// an alternative.
#[tokio::test]
async fn pox_autodiscover_falls_back_to_primary_for_an_unknown_mailbox() {
    let app = ews::router(ews::AppState::for_test(fixture()));
    let body = "<Autodiscover><Request>\
<EMailAddress>nobody@elsewhere.test</EMailAddress></Request></Autodiscover>";
    let (status, out) = post(app, "/autodiscover/autodiscover.xml", body).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        out.contains("<EMailAddress>user@example.com</EMailAddress>"),
        "should fall back to the primary: {out}"
    );
    assert!(
        out.contains("<SmtpAddress>shared-team@example.com</SmtpAddress>"),
        "{out}"
    );
}

/// A body that is not an Autodiscover request gets the POX in-band
/// error document, not a SOAP fault (POX is not SOAP).
#[tokio::test]
async fn pox_autodiscover_rejects_a_non_autodiscover_body() {
    let app = ews::router(ews::AppState::for_test(fixture()));
    let (status, out) = post(app, "/autodiscover/autodiscover.xml", "<nonsense/>").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(out.contains("<ErrorCode>600</ErrorCode>"), "{out}");
    assert!(!out.contains("Envelope"), "POX must not be SOAP: {out}");
}

/// The Autodiscover + EWS routes answer identically whether they are
/// reached on the dedicated EWS listener or co-mounted on the Graph
/// listener. Same handlers, same shared fixture - and merging them
/// into the Graph router must not disturb the Graph routes either.
#[tokio::test]
async fn autodiscover_and_ews_answer_identically_on_the_graph_mount() {
    let dedicated = ews::router(ews::AppState::for_test(fixture()));
    let graph_mount = graph_mounted_router(fixture());

    let pox = "<Autodiscover><Request>\
<EMailAddress>user@example.com</EMailAddress></Request></Autodiscover>";
    let soap_autodiscover = envelope(
        "<GetUserSettingsRequestMessage xmlns=\"http://schemas.microsoft.com/exchange/2010/Autodiscover\">\
<Request><Users><User><Mailbox>user@example.com</Mailbox></User></Users>\
<RequestedSettings><Setting>ExternalEwsUrl</Setting></RequestedSettings></Request>\
</GetUserSettingsRequestMessage>",
    );
    let find_folder = envelope(
        "<m:FindFolder Traversal=\"Deep\"><m:FolderShape><t:BaseShape>Default</t:BaseShape>\
</m:FolderShape><m:ParentFolderIds>\
<t:DistinguishedFolderId Id=\"publicfoldersroot\"/></m:ParentFolderIds></m:FindFolder>",
    );
    let get_item = envelope(
        "<m:GetItem><m:ItemShape><t:BaseShape>AllProperties</t:BaseShape></m:ItemShape>\
<m:ItemIds><t:ItemId Id=\"pi-eng-001\"/></m:ItemIds></m:GetItem>",
    );

    for (path, req) in [
        ("/autodiscover/autodiscover.xml", pox.to_string()),
        ("/autodiscover/autodiscover.svc", soap_autodiscover),
        ("/EWS/Exchange.asmx", find_folder),
        ("/EWS/Exchange.asmx", get_item),
    ] {
        let (dedicated_status, dedicated_body) = post(dedicated.clone(), path, &req).await;
        let (graph_status, graph_body) = post(graph_mount.clone(), path, &req).await;
        assert_eq!(dedicated_status, graph_status, "{path}");
        assert_eq!(dedicated_body, graph_body, "{path}");
        assert_eq!(dedicated_status, StatusCode::OK, "{path}");
    }

    // The Graph surface itself still works through the merged router.
    let resp = graph_mount
        .oneshot(
            Request::builder()
                .uri("/v1.0/me")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "graph route lost by merge");
}

/// `folder_class` distinguishes a mail public folder from a non-mail
/// one, so a consumer can keep `IPF.Appointment` out of its mail path.
#[tokio::test]
async fn find_folder_reports_the_fixture_folder_class() {
    let app = ews::router(ews::AppState::for_test(fixture()));
    let body = envelope(
        "<m:FindFolder Traversal=\"Deep\"><m:FolderShape><t:BaseShape>Default</t:BaseShape>\
</m:FolderShape><m:ParentFolderIds>\
<t:DistinguishedFolderId Id=\"publicfoldersroot\"/></m:ParentFolderIds></m:FindFolder>",
    );
    let (_, out) = post(app, "/EWS/Exchange.asmx", &body).await;

    // The calendar folder is the only non-mail one; every other folder
    // takes the IPF.Note default.
    assert_eq!(
        out.matches("<t:FolderClass>IPF.Appointment</t:FolderClass>")
            .count(),
        1,
        "{out}"
    );
    assert_eq!(
        out.matches("<t:FolderClass>IPF.Note</t:FolderClass>")
            .count(),
        3,
        "{out}"
    );
    // The non-mail folder is identifiable by class next to its name.
    let calendar = out
        .split("<t:Folder>")
        .find(|f| f.contains("Team Calendar"))
        .unwrap();
    assert!(
        calendar.contains("<t:FolderClass>IPF.Appointment</t:FolderClass>"),
        "{calendar}"
    );
}

/// `effective_rights` is fixture-driven per folder: one fixture stages
/// a read-only and a writable public folder and `FindFolder` reports
/// different rights for each.
#[tokio::test]
async fn find_folder_reports_per_folder_effective_rights() {
    let app = ews::router(ews::AppState::for_test(handle_for(
        "fixtures/shared-rights.toml",
    )));
    let body = envelope(
        "<m:FindFolder Traversal=\"Deep\"><m:FolderShape><t:BaseShape>Default</t:BaseShape>\
</m:FolderShape><m:ParentFolderIds>\
<t:DistinguishedFolderId Id=\"publicfoldersroot\"/></m:ParentFolderIds></m:FindFolder>",
    );
    let (_, out) = post(app, "/EWS/Exchange.asmx", &body).await;

    let notices = out
        .split("<t:Folder>")
        .find(|f| f.contains("Team Notices"))
        .unwrap();
    let drafts = out
        .split("<t:Folder>")
        .find(|f| f.contains("Team Drafts"))
        .unwrap();

    // Read-only: readable, nothing else.
    assert!(notices.contains("<t:Read>true</t:Read>"), "{notices}");
    assert!(
        notices.contains("<t:CreateContents>false</t:CreateContents>"),
        "{notices}"
    );
    assert!(notices.contains("<t:Modify>false</t:Modify>"), "{notices}");
    assert!(notices.contains("<t:Delete>false</t:Delete>"), "{notices}");

    // Writable: the create / modify / delete bits are on.
    assert!(drafts.contains("<t:Read>true</t:Read>"), "{drafts}");
    assert!(
        drafts.contains("<t:CreateContents>true</t:CreateContents>"),
        "{drafts}"
    );
    assert!(drafts.contains("<t:Modify>true</t:Modify>"), "{drafts}");
    assert!(drafts.contains("<t:Delete>true</t:Delete>"), "{drafts}");
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
    assert!(
        out.contains("<t:DisplayName>Engineering</t:DisplayName>"),
        "{out}"
    );
    assert!(
        out.contains("<t:DisplayName>Announcements</t:DisplayName>"),
        "{out}"
    );
    assert!(
        !out.contains("<t:DisplayName>Releases</t:DisplayName>"),
        "{out}"
    );
    // Engineering reports one child (Releases) and one item.
    assert!(
        out.contains("<t:ChildFolderCount>1</t:ChildFolderCount>"),
        "{out}"
    );

    // Deep -> the whole tree including Releases.
    let body = envelope(
        "<m:FindFolder Traversal=\"Deep\"><m:FolderShape><t:BaseShape>Default</t:BaseShape>\
</m:FolderShape><m:ParentFolderIds>\
<t:DistinguishedFolderId Id=\"publicfoldersroot\"/></m:ParentFolderIds></m:FindFolder>",
    );
    let (_, out) = post(app.clone(), "/EWS/Exchange.asmx", &body).await;
    assert!(
        out.contains("<t:DisplayName>Releases</t:DisplayName>"),
        "deep: {out}"
    );

    // Shallow under Engineering -> just Releases.
    let body = envelope(
        "<m:FindFolder Traversal=\"Shallow\"><m:FolderShape><t:BaseShape>Default</t:BaseShape>\
</m:FolderShape><m:ParentFolderIds>\
<t:FolderId Id=\"pf-root-eng\"/></m:ParentFolderIds></m:FindFolder>",
    );
    let (_, out) = post(app, "/EWS/Exchange.asmx", &body).await;
    assert!(
        out.contains("<t:DisplayName>Releases</t:DisplayName>"),
        "{out}"
    );
    assert!(
        !out.contains("<t:DisplayName>Engineering</t:DisplayName>"),
        "{out}"
    );
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
    assert!(
        out.contains("<t:Subject>Team sync notes</t:Subject>"),
        "{out}"
    );
    assert!(
        out.contains("<t:EmailAddress>lead@example.com</t:EmailAddress>"),
        "{out}"
    );
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
    assert!(
        out.contains("<t:Subject>Office closed Friday</t:Subject>"),
        "{out}"
    );
    assert!(
        out.contains("<t:Body BodyType=\"Text\">The office is closed this Friday.</t:Body>"),
        "{out}"
    );
    // The unknown id degrades to a per-item error, not a batch failure.
    assert!(out.contains("ErrorItemNotFound"), "{out}");
    assert!(out.contains("ResponseClass=\"Success\""), "{out}");
}

/// `GetItem` hydrates the staged body and the attachment metadata -
/// everything a reading pane needs except the bytes, which come from
/// `GetAttachment`.
#[tokio::test]
async fn get_item_returns_html_body_and_attachment_metadata() {
    let app = ews::router(ews::AppState::for_test(fixture()));
    let body = envelope(
        "<m:GetItem><m:ItemShape><t:BaseShape>AllProperties</t:BaseShape></m:ItemShape>\
<m:ItemIds><t:ItemId Id=\"pi-eng-001\"/></m:ItemIds></m:GetItem>",
    );
    let (status, out) = post(app, "/EWS/Exchange.asmx", &body).await;
    assert_eq!(status, StatusCode::OK);

    // The HTML body wins over the plain-text one when the fixture
    // stages both, and rides escaped inside the SOAP document.
    assert!(out.contains("<t:Body BodyType=\"HTML\">"), "{out}");
    assert!(
        out.contains("&lt;p&gt;Notes from the &lt;b&gt;weekly sync&lt;/b&gt;.&lt;/p&gt;"),
        "html body missing: {out}"
    );
    assert!(
        !out.contains("BodyType=\"Text\""),
        "text body should not also be emitted: {out}"
    );

    // Attachment metadata, but deliberately no bytes.
    assert!(
        out.contains("<t:HasAttachments>true</t:HasAttachments>"),
        "{out}"
    );
    assert!(
        out.contains("<t:AttachmentId Id=\"pf-blob-001\"/>"),
        "{out}"
    );
    assert!(
        out.contains("<t:Name>sync-notes.txt</t:Name>"),
        "attachment name: {out}"
    );
    assert!(
        out.contains("<t:ContentType>text/plain</t:ContentType>"),
        "{out}"
    );
    assert!(out.contains("<t:IsInline>false</t:IsInline>"), "{out}");
    assert!(
        !out.contains("<t:Content>"),
        "GetItem must not inline the bytes: {out}"
    );

    // The parent folder is named so a consumer can route the item.
    assert!(
        out.contains("<t:ParentFolderId Id=\"pf-root-eng\""),
        "{out}"
    );
}

/// An item with no staged attachments reports so, and keeps the plain
/// text body shape.
#[tokio::test]
async fn get_item_without_attachments_reports_none() {
    let app = ews::router(ews::AppState::for_test(fixture()));
    let body = envelope(
        "<m:GetItem><m:ItemShape><t:BaseShape>AllProperties</t:BaseShape></m:ItemShape>\
<m:ItemIds><t:ItemId Id=\"pi-announce-001\"/></m:ItemIds></m:GetItem>",
    );
    let (_, out) = post(app, "/EWS/Exchange.asmx", &body).await;
    assert!(
        out.contains("<t:HasAttachments>false</t:HasAttachments>"),
        "{out}"
    );
    assert!(!out.contains("<t:Attachments>"), "{out}");
    assert!(out.contains("<t:Body BodyType=\"Text\">"), "{out}");
}

/// `GetAttachment` serves the bytes for the id `GetItem` handed out,
/// base64'd into `<t:Content>`; an unknown id degrades that one
/// response message rather than the batch.
#[tokio::test]
async fn get_attachment_returns_the_bytes_and_a_per_item_error() {
    let app = ews::router(ews::AppState::for_test(fixture()));
    let body = envelope(
        "<m:GetAttachment><m:AttachmentShape/><m:AttachmentIds>\
<t:AttachmentId Id=\"pf-blob-001\"/><t:AttachmentId Id=\"pf-blob-bogus\"/>\
</m:AttachmentIds></m:GetAttachment>",
    );
    let (status, out) = post(app, "/EWS/Exchange.asmx", &body).await;
    assert_eq!(status, StatusCode::OK);

    let expected = ews::xml::base64_standard(&std::fs::read("fixtures/blobs/sample.txt").unwrap());
    assert!(
        out.contains(&format!("<t:Content>{expected}</t:Content>")),
        "attachment bytes missing: {out}"
    );
    assert!(
        out.contains("<t:Name>sync-notes.txt</t:Name>"),
        "attachment name: {out}"
    );
    assert!(out.contains("ErrorInvalidAttachmentId"), "{out}");
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
    assert!(
        out.contains("<m:ConnectionStatus>OK</m:ConnectionStatus>"),
        "{out}"
    );
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
    assert!(
        !out.contains("<t:NewMailEvent>"),
        "event re-delivered: {out}"
    );

    // Unsubscribe, then the subscription is gone (Closed).
    let unsub = envelope(&format!(
        "<m:Unsubscribe><m:SubscriptionId>{sub_id}</m:SubscriptionId></m:Unsubscribe>"
    ));
    let (_, out) = post(app.clone(), "/EWS/Exchange.asmx", &unsub).await;
    assert!(out.contains("ResponseClass=\"Success\""), "{out}");
    let (_, out) = post(app, "/EWS/Exchange.asmx", &poll).await;
    assert!(
        out.contains("<m:ConnectionStatus>Closed</m:ConnectionStatus>"),
        "{out}"
    );
}
