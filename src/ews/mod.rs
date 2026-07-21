//! Exchange Web Services (EWS) SOAP mock + Autodiscover.
//!
//! Serves the SOAP surface ratatoskr's public-folder / EWS-streaming
//! paths (A5b) drive, on a dedicated listener keyed off `--ews-port`:
//!
//! - `POST /autodiscover/autodiscover.svc` - SOAP Autodiscover
//!   `GetUserSettings`. Returns the `ExternalEwsUrl` pointing back at
//!   this process's EWS endpoint, so a client that autodiscovers then
//!   binds to the mock.
//! - `POST /EWS/Exchange.asmx` - the EWS operation endpoint. Dispatch
//!   is by the local name of the first element inside `soap:Body`:
//!   `FindFolder` / `FindItem` / `GetItem` over the org-wide public-
//!   folder tree (`[[public_folder]]` / `[[public_item]]`), plus the
//!   streaming-notification lifecycle `Subscribe` (StreamingSubscription)
//!   / `GetStreamingEvents` / `Unsubscribe`.
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

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/autodiscover/autodiscover.svc", post(autodiscover))
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

// ── EWS operation dispatch ──────────────────────────────────────────

async fn ews_endpoint(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> Response {
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
        fixture.public_folders_under(parent_fid.as_deref()).collect()
    };

    let mut items = String::new();
    for f in &folders {
        let total = fixture.public_items_in(&f.id).count();
        let children = fixture.public_folders_under(Some(&f.id)).count();
        items.push_str(&format!(
            "<t:Folder><t:FolderId Id=\"{}\" ChangeKey=\"{ck}\"/>\
<t:DisplayName>{}</t:DisplayName>\
<t:TotalCount>{total}</t:TotalCount>\
<t:ChildFolderCount>{children}</t:ChildFolderCount></t:Folder>",
            xml::escape(&f.id),
            xml::escape(&f.display_name),
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
        return response_message_error(
            "FindItem",
            "ErrorFolderNotFound",
            "no such public folder",
        );
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
                it.received_at.format("%Y-%m-%dT%H:%M:%SZ"),
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

/// `GetItem` - hydrate full item bodies by id. Each requested `ItemId`
/// gets its own response message; an unknown id degrades that one
/// message to `ErrorItemNotFound` rather than failing the batch.
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
<t:Subject>{}</t:Subject>\
<t:DateTimeReceived>{}</t:DateTimeReceived>\
<t:Body BodyType=\"Text\">{}</t:Body>\
<t:From><t:Mailbox><t:EmailAddress>{}</t:EmailAddress></t:Mailbox></t:From>\
<t:ToRecipients>{recipients}</t:ToRecipients>\
</t:Message></m:Items></m:GetItemResponseMessage>",
                    xml::escape(&it.id),
                    xml::escape(&it.subject),
                    it.received_at.format("%Y-%m-%dT%H:%M:%SZ"),
                    xml::escape(&it.body_text),
                    xml::escape(&it.from.email),
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

// ── Streaming subscription lifecycle ────────────────────────────────

/// `Subscribe` (StreamingSubscription). Registers a subscription on the
/// PushHub scoped to the bearer-resolved account and returns a minted
/// `SubscriptionId`.
fn subscribe(state: &AppState, headers: &HeaderMap, _body: &str) -> Response {
    let account_id = crate::oauth::account_from_bearer(
        &state.fixture(),
        &state.shared.token_store,
        headers,
    );
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
