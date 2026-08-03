//! Exchange Web Services (EWS) SOAP mock + Autodiscover.
//!
//! Serves the SOAP surface ratatoskr's public-folder / EWS-streaming
//! paths (A5b) drive, on a dedicated listener keyed off `--ews-port`:
//!
//! - `POST /autodiscover/autodiscover.svc` - SOAP Autodiscover
//!   `GetUserSettings`. Returns the `ExternalEwsUrl` pointing back at
//!   this process's EWS endpoint, so a client that autodiscovers then
//!   binds to the mock.
//! - `POST /autodiscover/autodiscover.xml` - POX (plain-old-XML)
//!   Autodiscover. The delegate / shared-mailbox discovery channel:
//!   the response's `AlternativeMailboxes` block projects every
//!   declared account other than the requesting one, which is how a
//!   client learns about the shared mailboxes it should also open.
//! - `POST /EWS/Exchange.asmx` - the EWS operation endpoint. Dispatch
//!   is by the local name of the first element inside `soap:Body`:
//!   `FindFolder` / `FindItem` / `GetItem` / `GetAttachment` over the
//!   org-wide public-folder tree (`[[public_folder]]` /
//!   `[[public_item]]`), plus the streaming-notification lifecycle
//!   `Subscribe` (StreamingSubscription) / `GetStreamingEvents` /
//!   `Unsubscribe`.
//!
//! The same router is mounted twice by `main.rs`: on the dedicated
//! `--ews-port` listener, and (merged) on the Graph listener. The
//! harness that drives this mock injects a fixed set of endpoint
//! environment variables into the process under test and has no EWS
//! slot, so co-mounting on Graph is what makes the surface reachable
//! at all from a harness run. Each mount gets its own `base_url`, so
//! the Autodiscover URLs each advertises point back at the listener
//! the request arrived on.
//!
//! Streaming subscriptions register on the shared [`crate::push::
//! PushHub`]; a fixture state advance (`POST /test/fixture/step`)
//! enqueues a notification per subscription bound to the touched
//! account, which the next `GetStreamingEvents` drains - the same
//! mechanism the JMAP WebSocket / Gmail Pub/Sub / Graph webhook
//! surfaces use.
//!
//! Wire shapes follow the published EWS schemas
//! (`.../exchange/services/2006/{messages,types}`) and the
//! Autodiscover SOAP schema. XML is hand-rolled (`ews/xml.rs`) in the
//! CalDAV module's small-well-known-bodies style. v0 is read-only over
//! public folders; there is no EWS write surface. NewMail streaming
//! events carry Watermark + TimeStamp + ParentFolderId (no fabricated
//! ItemId) - enough to prove the push fired.

pub mod xml;

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};

use crate::push::EwsSubscription;
use crate::shared::SharedHandles;

/// EWS/types namespace prefix `t:`.
const NS: &str = "xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
xmlns:m=\"http://schemas.microsoft.com/exchange/services/2006/messages\" \
xmlns:t=\"http://schemas.microsoft.com/exchange/services/2006/types\"";

#[derive(Clone)]
pub struct AppState {
    pub shared: SharedHandles,
    /// Base URL of this EWS listener (`http://127.0.0.1:PORT`), spliced
    /// into the Autodiscover `ExternalEwsUrl`. Set from the bound
    /// socket in `main.rs`; a placeholder in tests.
    pub base_url: String,
}

impl AppState {
    pub fn for_test(fixture: crate::shared::FixtureHandle) -> Self {
        Self {
            shared: SharedHandles::for_test(fixture),
            base_url: "http://127.0.0.1:0".to_string(),
        }
    }

    pub(crate) fn fixture(&self) -> std::sync::RwLockReadGuard<'_, crate::fixture::Fixture> {
        self.shared.fixture.read().expect("fixture lock poisoned")
    }

    pub fn with_request_log(mut self, log: crate::request_log::RequestLog) -> Self {
        self.shared.request_log = log;
        self
    }
}

/// Build the EWS / Autodiscover router. Returned fully-stated
/// (`Router<()>`) so it can either be served on its own listener or
/// merged into another listener's router - the Graph one, which is
/// what `main.rs` does to make the surface reachable through the
/// harness's Graph endpoint variable.
///
/// Autodiscover is registered under both the lower-case path
/// (`/autodiscover/autodiscover.{svc,xml}`) and the capitalised one
/// real Outlook-family clients use (`/Autodiscover/Autodiscover.xml`);
/// axum matches paths case-sensitively, so a client that picks the
/// documented capitalisation would otherwise 404.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/autodiscover/autodiscover.svc", post(autodiscover))
        .route("/Autodiscover/Autodiscover.svc", post(autodiscover))
        .route("/autodiscover/autodiscover.xml", post(autodiscover_pox))
        .route("/Autodiscover/Autodiscover.xml", post(autodiscover_pox))
        .route("/EWS/Exchange.asmx", post(ews_endpoint))
        .with_state(state)
}

pub async fn serve(
    listener: tokio::net::TcpListener,
    state: AppState,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> std::io::Result<()> {
    let app = router(state);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<crate::connection_id::ConnInfo>(),
    )
    .with_graceful_shutdown(async move {
        while shutdown_rx.changed().await.is_ok() {
            if *shutdown_rx.borrow() {
                return;
            }
        }
    })
    .await
}

/// Wrap an operation-response fragment in the SOAP envelope + emit as
/// `text/xml`. Every EWS / Autodiscover response goes through here.
fn soap(body: &str) -> Response {
    let doc = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<s:Envelope {NS}><s:Body>{body}</s:Body></s:Envelope>"
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/xml; charset=utf-8")],
        doc,
    )
        .into_response()
}

/// Emit a bare (non-SOAP) XML document as `text/xml`. POX Autodiscover
/// is not SOAP - the response is a plain `<Autodiscover>` root - so it
/// bypasses [`soap`].
fn xml_doc(status: StatusCode, body: &str) -> Response {
    let doc = format!("<?xml version=\"1.0\" encoding=\"utf-8\"?>{body}");
    (
        status,
        [(header::CONTENT_TYPE, "text/xml; charset=utf-8")],
        doc,
    )
        .into_response()
}

/// A SOAP fault for an unrecognised operation.
fn fault(reason: &str) -> Response {
    let body = format!(
        "<s:Fault><faultcode>s:Client</faultcode>\
<faultstring>{}</faultstring></s:Fault>",
        xml::escape(reason)
    );
    let doc = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<s:Envelope {NS}><s:Body>{body}</s:Body></s:Envelope>"
    );
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(header::CONTENT_TYPE, "text/xml; charset=utf-8")],
        doc,
    )
        .into_response()
}

fn log(state: &AppState, detail: &str) {
    state
        .shared
        .request_log
        .record("ews", detail.to_string(), serde_json::json!({}));
}

/// Deterministic opaque `ChangeKey` for a resource. EWS treats it as an
/// opaque version token; we derive it from the fixture's primary state
/// so a fixture mutation moves every key in lock-step (fine for the
/// read-only public-folder surface).
fn change_key(state: &AppState) -> String {
    format!("CK-{}", state.fixture().primary_state())
}

// ── Autodiscover ────────────────────────────────────────────────────

async fn autodiscover(State(state): State<AppState>, body: String) -> Response {
    log(&state, "POST /autodiscover/autodiscover.svc");
    if !xml::has_element(&body, "GetUserSettingsRequestMessage") {
        return fault("unsupported Autodiscover request");
    }
    let ews_url = format!("{}/EWS/Exchange.asmx", state.base_url);
    // Minimal GetUserSettings response: NoError + the ExternalEwsUrl a
    // client needs to bind to the mock. Other settings (CasVersion,
    // ExternalMailboxServer, ...) are omitted - v0 clients only need
    // the EWS URL.
    let inner = format!(
        "<GetUserSettingsResponseMessage \
xmlns=\"http://schemas.microsoft.com/exchange/2010/Autodiscover\">\
<Response xmlns:a=\"http://schemas.microsoft.com/exchange/2010/Autodiscover\">\
<a:ErrorCode>NoError</a:ErrorCode><a:ErrorMessage/>\
<a:UserResponses><a:UserResponse>\
<a:ErrorCode>NoError</a:ErrorCode><a:ErrorMessage/>\
<a:RedirectTarget xsi:nil=\"true\" \
xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\"/>\
<a:UserSettingErrors/>\
<a:UserSettings>\
<a:UserSetting i:type=\"a:StringSetting\" \
xmlns:i=\"http://www.w3.org/2001/XMLSchema-instance\">\
<a:Name>ExternalEwsUrl</a:Name><a:Value>{}</a:Value></a:UserSetting>\
</a:UserSettings>\
</a:UserResponse></a:UserResponses></Response>\
</GetUserSettingsResponseMessage>",
        xml::escape(&ews_url)
    );
    soap(&inner)
}

// ── POX Autodiscover ────────────────────────────────────────────────

/// POX Autodiscover response namespaces. The outer `Autodiscover`
/// element sits in the response schema; the inner `Response` switches
/// the default namespace to the Outlook response schema, and every
/// descendant inherits it (which is why none of the inner elements
/// carry a prefix).
const POX_NS: &str = "http://schemas.microsoft.com/exchange/autodiscover/responseschema/2006";
const POX_OUTLOOK_NS: &str =
    "http://schemas.microsoft.com/exchange/autodiscover/outlook/responseschema/2006a";

/// Synthetic Exchange legacy distinguished name for an account. Real
/// Autodiscover carries one per mailbox and clients echo it back when
/// binding a delegate mailbox; the value is opaque, so we derive it
/// from the fixture account id to keep it deterministic.
fn legacy_dn(account_id: &str) -> String {
    format!("/o=saehrimnir/ou=Exchange Administrative Group/cn=Recipients/cn={account_id}")
}

/// `host:port` of this mount's base URL, used for the POX `<Server>`
/// elements. Falls back to the whole base URL if it carries no scheme.
fn server_host(base_url: &str) -> &str {
    base_url
        .strip_prefix("https://")
        .or_else(|| base_url.strip_prefix("http://"))
        .unwrap_or(base_url)
}

/// `POST /autodiscover/autodiscover.xml` - POX Autodiscover.
///
/// This is the delegate / shared-mailbox discovery channel, distinct
/// from the SOAP `GetUserSettings` endpoint above. The request names a
/// mailbox (`<EMailAddress>`); the response describes that user's own
/// EXCH protocol binding plus an `AlternativeMailboxes` block listing
/// every *other* declared account, which is how a client performing
/// delegate discovery finds the shared mailboxes it should also open.
///
/// Deviation from the published POX schema, deliberate: real POX emits
/// repeated bare `<AlternativeMailbox>` children of `<Account>` with no
/// container element. We wrap them in an `<AlternativeMailboxes>`
/// container (the name the SOAP `AlternateMailboxes` setting uses) so
/// a consumer can key off either the container or the individual
/// entries; a parser that walks by local name finds the entries
/// unchanged.
async fn autodiscover_pox(State(state): State<AppState>, body: String) -> Response {
    log(&state, "POST /autodiscover/autodiscover.xml");
    if !xml::has_element(&body, "Autodiscover") {
        // POX signals a bad request in-band, not with a SOAP fault.
        let err = format!(
            "<Autodiscover xmlns=\"{POX_NS}\"><Response>\
<Error Time=\"00:00:00.0000000\" Id=\"0\">\
<ErrorCode>600</ErrorCode><Message>Invalid Request</Message>\
<DebugData/></Error></Response></Autodiscover>"
        );
        return xml_doc(StatusCode::BAD_REQUEST, &err);
    }

    let requested = xml::element_text(&body, "EMailAddress").unwrap_or_default();
    let ews_url = format!("{}/EWS/Exchange.asmx", state.base_url);
    let server = server_host(&state.base_url).to_string();
    let fixture = state.fixture();

    // The requesting mailbox: the declared account whose name matches
    // `<EMailAddress>` (case-insensitively, as SMTP addresses are), or
    // the primary when the request names nobody we know. Everyone else
    // becomes an alternative mailbox.
    let user = fixture
        .accounts
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case(requested.trim()))
        .unwrap_or_else(|| fixture.primary_account());

    let mut alternatives = String::new();
    for acct in fixture.accounts.iter().filter(|a| a.id != user.id) {
        // `Type` distinguishes a delegate mailbox from an archive.
        // Every non-requesting account is staged as a delegate: the
        // fixture has no archive concept.
        alternatives.push_str(&format!(
            "<AlternativeMailbox><Type>Delegate</Type>\
<DisplayName>{name}</DisplayName>\
<SmtpAddress>{name}</SmtpAddress>\
<OwnerSmtpAddress>{name}</OwnerSmtpAddress>\
<LegacyDN>{dn}</LegacyDN>\
<Server>{server}</Server></AlternativeMailbox>",
            name = xml::escape(&acct.name),
            dn = xml::escape(&legacy_dn(&acct.id)),
            server = xml::escape(&server),
        ));
    }

    let doc = format!(
        "<Autodiscover xmlns=\"{POX_NS}\">\
<Response xmlns=\"{POX_OUTLOOK_NS}\">\
<User><DisplayName>{name}</DisplayName>\
<EMailAddress>{name}</EMailAddress>\
<LegacyDN>{dn}</LegacyDN></User>\
<Account><AccountType>email</AccountType><Action>settings</Action>\
<Protocol><Type>EXCH</Type><Server>{server}</Server>\
<ServerDN>{dn}</ServerDN>\
<EwsUrl>{ews}</EwsUrl><EmwsUrl>{ews}</EmwsUrl>\
<OABUrl>{base}/OAB/</OABUrl></Protocol>\
<AlternativeMailboxes>{alternatives}</AlternativeMailboxes>\
</Account></Response></Autodiscover>",
        name = xml::escape(&user.name),
        dn = xml::escape(&legacy_dn(&user.id)),
        server = xml::escape(&server),
        ews = xml::escape(&ews_url),
        base = xml::escape(&state.base_url),
    );
    xml_doc(StatusCode::OK, &doc)
}

// ── EWS operation dispatch ──────────────────────────────────────────

async fn ews_endpoint(State(state): State<AppState>, headers: HeaderMap, body: String) -> Response {
    // Priority order matters: check `Unsubscribe` and
    // `GetStreamingEvents` before `Subscribe` so the substring
    // `Subscribe` inside `Unsubscribe` doesn't misroute (the xml helper
    // already anchors on `<`/`:`, but ordering keeps intent explicit).
    if xml::has_element(&body, "FindFolder") {
        log(&state, "POST /EWS FindFolder");
        find_folder(&state, &body)
    } else if xml::has_element(&body, "FindItem") {
        log(&state, "POST /EWS FindItem");
        find_item(&state, &body)
    } else if xml::has_element(&body, "GetAttachment") {
        log(&state, "POST /EWS GetAttachment");
        get_attachment(&state, &body)
    } else if xml::has_element(&body, "GetItem") {
        log(&state, "POST /EWS GetItem");
        get_item(&state, &body)
    } else if xml::has_element(&body, "Unsubscribe") {
        log(&state, "POST /EWS Unsubscribe");
        unsubscribe(&state, &body)
    } else if xml::has_element(&body, "GetStreamingEvents") {
        log(&state, "POST /EWS GetStreamingEvents");
        get_streaming_events(&state, &body)
    } else if xml::has_element(&body, "Subscribe") {
        log(&state, "POST /EWS Subscribe");
        subscribe(&state, &headers, &body)
    } else {
        fault("unrecognised EWS operation")
    }
}

/// `FindFolder` over the public-folder tree. `Traversal="Deep"` returns
/// the whole tree; `Shallow` (default) returns the children of the
/// named parent - `DistinguishedFolderId Id="publicfoldersroot"` (or an
/// absent parent) means the top-level folders, an explicit
/// `FolderId Id="..."` means that folder's children.
fn find_folder(state: &AppState, body: &str) -> Response {
    let deep = xml::element_attr(body, "FindFolder", "Traversal").as_deref() == Some("Deep");
    let ck = change_key(state);
    let fixture = state.fixture();

    // An explicit `FolderId Id="..."` scopes to that folder's children;
    // `publicfoldersroot` / unspecified -> the top-level folders
    // (`parent_id == None`). Held at function scope so the borrow in
    // `public_folders_under` outlives the collected `folders`.
    let parent_fid = if deep {
        None
    } else {
        xml::element_attr(body, "FolderId", "Id")
    };
    let folders: Vec<&crate::fixture::PublicFolder> = if deep {
        fixture.public_folders.iter().collect()
    } else {
        fixture
            .public_folders_under(parent_fid.as_deref())
            .collect()
    };

    let mut items = String::new();
    for f in &folders {
        let total = fixture.public_items_in(&f.id).count();
        let children = fixture.public_folders_under(Some(&f.id)).count();
        // Element order follows the EWS `t:FolderType` sequence:
        // FolderId, ParentFolderId, FolderClass, DisplayName,
        // TotalCount, ChildFolderCount, ..., EffectiveRights.
        let parent = match &f.parent_id {
            Some(p) => format!(
                "<t:ParentFolderId Id=\"{}\" ChangeKey=\"{ck}\"/>",
                xml::escape(p)
            ),
            // A top-level public folder hangs off the synthetic root.
            None => format!("<t:ParentFolderId Id=\"publicfoldersroot\" ChangeKey=\"{ck}\"/>"),
        };
        items.push_str(&format!(
            "<t:Folder><t:FolderId Id=\"{}\" ChangeKey=\"{ck}\"/>{parent}\
<t:FolderClass>{}</t:FolderClass>\
<t:DisplayName>{}</t:DisplayName>\
<t:TotalCount>{total}</t:TotalCount>\
<t:ChildFolderCount>{children}</t:ChildFolderCount>{}</t:Folder>",
            xml::escape(&f.id),
            xml::escape(&f.folder_class),
            xml::escape(&f.display_name),
            effective_rights_xml(&f.effective_rights),
        ));
    }
    let inner = format!(
        "<m:FindFolderResponse><m:ResponseMessages>\
<m:FindFolderResponseMessage ResponseClass=\"Success\">\
<m:ResponseCode>NoError</m:ResponseCode>\
<m:RootFolder TotalItemsInView=\"{}\" IncludesLastItemInRange=\"true\">\
<t:Folders>{items}</t:Folders></m:RootFolder>\
</m:FindFolderResponseMessage></m:ResponseMessages></m:FindFolderResponse>",
        folders.len(),
    );
    soap(&inner)
}

/// The `t:EffectiveRights` block for a public folder - the rights the
/// calling client holds on it, staged per folder in the fixture
/// (`[[public_folder]] effective_rights`). A read-only folder reports
/// `Read` alone; a writable one turns on the create / modify / delete
/// bits. Child order follows the EWS `EffectiveRightsType` sequence.
fn effective_rights_xml(r: &crate::fixture::EffectiveRights) -> String {
    format!(
        "<t:EffectiveRights>\
<t:CreateAssociated>{}</t:CreateAssociated>\
<t:CreateContents>{}</t:CreateContents>\
<t:CreateHierarchy>{}</t:CreateHierarchy>\
<t:Delete>{}</t:Delete>\
<t:Modify>{}</t:Modify>\
<t:Read>{}</t:Read>\
<t:ViewPrivateItems>{}</t:ViewPrivateItems>\
</t:EffectiveRights>",
        r.create_associated,
        r.create_contents,
        r.create_hierarchy,
        r.delete,
        r.modify,
        r.read,
        r.view_private_items,
    )
}

/// `FindItem` in a public folder. Honours `BaseShape` (`IdOnly` emits
/// just the `ItemId`; anything else emits the default projection:
/// Subject, DateTimeReceived, From).
fn find_item(state: &AppState, body: &str) -> Response {
    let id_only = xml::element_text(body, "BaseShape").as_deref() == Some("IdOnly");
    let ck = change_key(state);
    let fixture = state.fixture();

    let Some(folder_id) = xml::element_attr(body, "FolderId", "Id") else {
        return response_message_error(
            "FindItem",
            "ErrorInvalidRequest",
            "FindItem requires a ParentFolderIds/FolderId",
        );
    };
    if fixture.public_folder(&folder_id).is_none() {
        return response_message_error("FindItem", "ErrorFolderNotFound", "no such public folder");
    }

    let mut rows = String::new();
    let mut count = 0usize;
    for it in fixture.public_items_in(&folder_id) {
        count += 1;
        if id_only {
            rows.push_str(&format!(
                "<t:Message><t:ItemId Id=\"{}\" ChangeKey=\"{ck}\"/></t:Message>",
                xml::escape(&it.id)
            ));
        } else {
            rows.push_str(&format!(
                "<t:Message><t:ItemId Id=\"{}\" ChangeKey=\"{ck}\"/>\
<t:Subject>{}</t:Subject>\
<t:DateTimeReceived>{}</t:DateTimeReceived>\
<t:From><t:Mailbox><t:EmailAddress>{}</t:EmailAddress></t:Mailbox></t:From>\
</t:Message>",
                xml::escape(&it.id),
                xml::escape(&it.subject),
                it.received_at.strftime("%Y-%m-%dT%H:%M:%SZ"),
                xml::escape(&it.from.email),
            ));
        }
    }
    let inner = format!(
        "<m:FindItemResponse><m:ResponseMessages>\
<m:FindItemResponseMessage ResponseClass=\"Success\">\
<m:ResponseCode>NoError</m:ResponseCode>\
<m:RootFolder TotalItemsInView=\"{count}\" IncludesLastItemInRange=\"true\">\
<t:Items>{rows}</t:Items></m:RootFolder>\
</m:FindItemResponseMessage></m:ResponseMessages></m:FindItemResponse>",
    );
    soap(&inner)
}

/// The `t:Body` element for a public item. `body_html` wins when the
/// fixture stages one (real Exchange serves the richest body it holds
/// and the client asks for HTML); otherwise the plain-text body. The
/// HTML is emitted escaped as element text, matching how EWS carries
/// an HTML body inside the SOAP document.
fn body_xml(item: &crate::fixture::PublicItem) -> String {
    match &item.body_html {
        Some(html) => format!("<t:Body BodyType=\"HTML\">{}</t:Body>", xml::escape(html)),
        None => format!(
            "<t:Body BodyType=\"Text\">{}</t:Body>",
            xml::escape(&item.body_text)
        ),
    }
}

/// The `t:Attachments` block for a public item: metadata only (name,
/// content type, size, inline-ness). The bytes are deliberately
/// absent, because a client fetches them with `GetAttachment`. That is
/// the split real EWS makes, and the one the consumer's hydration path
/// follows. Returns the empty string when the item carries no
/// attachments, so the element is omitted entirely rather than emitted
/// empty.
fn attachments_xml(item: &crate::fixture::PublicItem) -> String {
    if item.attachments.is_empty() {
        return String::new();
    }
    let mut inner = String::new();
    for att in &item.attachments {
        let content_id = match &att.cid {
            Some(cid) => format!("<t:ContentId>{}</t:ContentId>", xml::escape(cid)),
            None => String::new(),
        };
        inner.push_str(&format!(
            "<t:FileAttachment><t:AttachmentId Id=\"{}\"/>\
<t:Name>{}</t:Name>\
<t:ContentType>{}</t:ContentType>{content_id}\
<t:Size>{}</t:Size>\
<t:IsInline>{}</t:IsInline></t:FileAttachment>",
            xml::escape(&att.blob_id),
            xml::escape(&att.name),
            xml::escape(&att.content_type),
            att.size,
            att.disposition == crate::fixture::Disposition::Inline,
        ));
    }
    format!("<t:Attachments>{inner}</t:Attachments>")
}

/// `GetItem` - hydrate full item bodies by id. Each requested `ItemId`
/// gets its own response message; an unknown id degrades that one
/// message to `ErrorItemNotFound` rather than failing the batch.
///
/// The projection carries everything a reading pane needs: the staged
/// body (HTML when the fixture declares one, else text) plus the
/// attachment metadata. Attachment *bytes* come from `GetAttachment`.
fn get_item(state: &AppState, body: &str) -> Response {
    let ids = xml::all_element_attr(body, "ItemId", "Id");
    let ck = change_key(state);
    let fixture = state.fixture();

    let mut messages = String::new();
    for id in &ids {
        match fixture.public_item(id) {
            Some(it) => {
                let recipients: String = it
                    .to
                    .iter()
                    .map(|a| {
                        format!(
                            "<t:Mailbox><t:EmailAddress>{}</t:EmailAddress></t:Mailbox>",
                            xml::escape(&a.email)
                        )
                    })
                    .collect();
                messages.push_str(&format!(
                    "<m:GetItemResponseMessage ResponseClass=\"Success\">\
<m:ResponseCode>NoError</m:ResponseCode><m:Items><t:Message>\
<t:ItemId Id=\"{}\" ChangeKey=\"{ck}\"/>\
<t:ParentFolderId Id=\"{}\" ChangeKey=\"{ck}\"/>\
<t:Subject>{}</t:Subject>\
<t:DateTimeReceived>{}</t:DateTimeReceived>\
{body}{attachments}\
<t:HasAttachments>{has_attachments}</t:HasAttachments>\
<t:From><t:Mailbox><t:EmailAddress>{}</t:EmailAddress></t:Mailbox></t:From>\
<t:ToRecipients>{recipients}</t:ToRecipients>\
</t:Message></m:Items></m:GetItemResponseMessage>",
                    xml::escape(&it.id),
                    xml::escape(&it.folder_id),
                    xml::escape(&it.subject),
                    it.received_at.strftime("%Y-%m-%dT%H:%M:%SZ"),
                    xml::escape(&it.from.email),
                    body = body_xml(it),
                    attachments = attachments_xml(it),
                    has_attachments = !it.attachments.is_empty(),
                ));
            }
            None => {
                messages.push_str(
                    "<m:GetItemResponseMessage ResponseClass=\"Error\">\
<m:ResponseCode>ErrorItemNotFound</m:ResponseCode>\
<m:MessageText>item not found</m:MessageText>\
<m:Items/></m:GetItemResponseMessage>",
                );
            }
        }
    }
    let inner = format!(
        "<m:GetItemResponse><m:ResponseMessages>{messages}</m:ResponseMessages>\
</m:GetItemResponse>",
    );
    soap(&inner)
}

/// `GetAttachment` - serve attachment bytes by `AttachmentId`. This is
/// the second half of the hydration split: `GetItem` hands the client
/// attachment metadata, `GetAttachment` hands it the content, base64'd
/// into `<t:Content>`.
///
/// EWS addresses an attachment by id alone (no item context), so the
/// loader enforces org-wide uniqueness of public attachment blob ids.
/// An unknown id degrades that one response message to
/// `ErrorInvalidAttachmentId` rather than failing the batch, matching
/// `GetItem`'s per-item error behaviour.
fn get_attachment(state: &AppState, body: &str) -> Response {
    let ids = xml::all_element_attr(body, "AttachmentId", "Id");
    let fixture = state.fixture();

    let mut messages = String::new();
    for id in &ids {
        match fixture.public_attachment(id) {
            Some((_item, att)) => {
                let content_id = match &att.cid {
                    Some(cid) => format!("<t:ContentId>{}</t:ContentId>", xml::escape(cid)),
                    None => String::new(),
                };
                messages.push_str(&format!(
                    "<m:GetAttachmentResponseMessage ResponseClass=\"Success\">\
<m:ResponseCode>NoError</m:ResponseCode><m:Attachments><t:FileAttachment>\
<t:AttachmentId Id=\"{}\"/>\
<t:Name>{}</t:Name>\
<t:ContentType>{}</t:ContentType>{content_id}\
<t:Size>{}</t:Size>\
<t:IsInline>{}</t:IsInline>\
<t:Content>{}</t:Content>\
</t:FileAttachment></m:Attachments></m:GetAttachmentResponseMessage>",
                    xml::escape(&att.blob_id),
                    xml::escape(&att.name),
                    xml::escape(&att.content_type),
                    att.size,
                    att.disposition == crate::fixture::Disposition::Inline,
                    xml::base64_standard(&att.data),
                ));
            }
            None => {
                messages.push_str(
                    "<m:GetAttachmentResponseMessage ResponseClass=\"Error\">\
<m:ResponseCode>ErrorInvalidAttachmentId</m:ResponseCode>\
<m:MessageText>attachment not found</m:MessageText>\
<m:Attachments/></m:GetAttachmentResponseMessage>",
                );
            }
        }
    }
    let inner = format!(
        "<m:GetAttachmentResponse><m:ResponseMessages>{messages}</m:ResponseMessages>\
</m:GetAttachmentResponse>",
    );
    soap(&inner)
}

// ── Streaming subscription lifecycle ────────────────────────────────

/// `Subscribe` (StreamingSubscription). Registers a subscription on the
/// PushHub scoped to the bearer-resolved account and returns a minted
/// `SubscriptionId`.
fn subscribe(state: &AppState, headers: &HeaderMap, _body: &str) -> Response {
    let account_id =
        crate::oauth::account_from_bearer(&state.fixture(), &state.shared.token_store, headers);
    let id = format!("ews-sub-{}", state.shared.push.next_seq());
    state.shared.push.ews_create_subscription(EwsSubscription {
        id: id.clone(),
        account_id,
    });
    let inner = format!(
        "<m:SubscribeResponse><m:ResponseMessages>\
<m:SubscribeResponseMessage ResponseClass=\"Success\">\
<m:ResponseCode>NoError</m:ResponseCode>\
<m:SubscriptionId>{}</m:SubscriptionId></m:SubscribeResponseMessage>\
</m:ResponseMessages></m:SubscribeResponse>",
        xml::escape(&id)
    );
    soap(&inner)
}

/// `GetStreamingEvents` - drain and return the notifications queued for
/// the subscription since the last poll. Unknown subscription -> a
/// `Closed` connection status; a known one with no pending events -> a
/// heartbeat-shaped `OK` with an empty notifications set.
fn get_streaming_events(state: &AppState, body: &str) -> Response {
    let sub_id = xml::element_text(body, "SubscriptionId").unwrap_or_default();
    if state.shared.push.ews_get_subscription(&sub_id).is_none() {
        let inner = "<m:GetStreamingEventsResponse><m:ResponseMessages>\
<m:GetStreamingEventsResponseMessage ResponseClass=\"Error\">\
<m:ResponseCode>ErrorInvalidSubscription</m:ResponseCode>\
<m:ConnectionStatus>Closed</m:ConnectionStatus>\
</m:GetStreamingEventsResponseMessage></m:ResponseMessages>\
</m:GetStreamingEventsResponse>"
            .to_string();
        return soap(&inner);
    }
    let events = state.shared.push.ews_drain_events(&sub_id);
    let notifications = if events.is_empty() {
        String::new()
    } else {
        let mut inner_events = String::new();
        for ev in &events {
            let watermark = ev.get("watermark").and_then(|v| v.as_str()).unwrap_or("");
            let timestamp = ev.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");
            inner_events.push_str(&format!(
                "<t:NewMailEvent><t:Watermark>{}</t:Watermark>\
<t:TimeStamp>{}</t:TimeStamp>\
<t:ParentFolderId Id=\"inbox\" ChangeKey=\"CK\"/></t:NewMailEvent>",
                xml::escape(watermark),
                xml::escape(timestamp),
            ));
        }
        format!(
            "<m:Notification><t:SubscriptionId>{}</t:SubscriptionId>{inner_events}</m:Notification>",
            xml::escape(&sub_id)
        )
    };
    let inner = format!(
        "<m:GetStreamingEventsResponse><m:ResponseMessages>\
<m:GetStreamingEventsResponseMessage ResponseClass=\"Success\">\
<m:ResponseCode>NoError</m:ResponseCode>\
<m:ConnectionStatus>OK</m:ConnectionStatus>\
<m:Notifications>{notifications}</m:Notifications>\
</m:GetStreamingEventsResponseMessage></m:ResponseMessages>\
</m:GetStreamingEventsResponse>",
    );
    soap(&inner)
}

/// `Unsubscribe` - drop the streaming subscription.
fn unsubscribe(state: &AppState, body: &str) -> Response {
    let sub_id = xml::element_text(body, "SubscriptionId").unwrap_or_default();
    let removed = state.shared.push.ews_delete_subscription(&sub_id);
    let (class, code) = if removed {
        ("Success", "NoError")
    } else {
        ("Error", "ErrorInvalidSubscription")
    };
    let inner = format!(
        "<m:UnsubscribeResponse><m:ResponseMessages>\
<m:UnsubscribeResponseMessage ResponseClass=\"{class}\">\
<m:ResponseCode>{code}</m:ResponseCode></m:UnsubscribeResponseMessage>\
</m:ResponseMessages></m:UnsubscribeResponse>",
    );
    soap(&inner)
}

/// A single-message operation-level error response (e.g. FindItem with
/// a bad folder). `op` is the operation name (`FindItem`), used to name
/// the response-message + wrapper elements.
fn response_message_error(op: &str, code: &str, text: &str) -> Response {
    let inner = format!(
        "<m:{op}Response><m:ResponseMessages>\
<m:{op}ResponseMessage ResponseClass=\"Error\">\
<m:ResponseCode>{code}</m:ResponseCode>\
<m:MessageText>{}</m:MessageText></m:{op}ResponseMessage>\
</m:ResponseMessages></m:{op}Response>",
        xml::escape(text)
    );
    soap(&inner)
}
