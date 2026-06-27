//! HTTP route handlers.
//!
//! All responses derive from the loaded [`Fixture`]. Determinism
//! contract: same fixture in → byte-identical responses out (modulo
//! json key-ordering, which is alphabetical by virtue of `serde_json`'s
//! default `BTreeMap`-backed `Map`).

use std::sync::Arc;

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use serde_json::{Value, json};

use crate::jmap::{self, JmapRequest, JmapResponse};
use crate::oauth::{self, BearerDecision};
use crate::request_log::RequestEntry;
use crate::shared::SharedHandles;
use crate::smtp;

#[derive(Clone)]
pub struct AppState {
    /// Shared handle bag: fixture, dispatcher, request log,
    /// token store. See `crate::shared::SharedHandles`.
    pub shared: SharedHandles,
    /// Shared handle to the SMTP submission log. The test-only
    /// `/test/smtp/submissions` route reads (and `DELETE` clears)
    /// this. Tests that don't drive SMTP can construct it via
    /// `SubmissionLog::default()`.
    pub submission_log: smtp::SubmissionLog,
    /// Externally-visible base URL (`scheme://host[:port]`)
    /// advertised in the JMAP session resource for `apiUrl` /
    /// `downloadUrl` / `uploadUrl` / `eventSourceUrl`. Sourced
    /// from the bound listener in `main.rs`; tests can pass
    /// `"http://localhost".into()`. We deliberately do NOT
    /// derive this from the inbound `Host` header - a client
    /// sending `Host: evil.com` would otherwise rewrite every
    /// advertised endpoint to point at an attacker-chosen host.
    pub base_url: String,
}

impl AppState {
    /// Build an `AppState` around `fixture` with fresh, default
    /// shared handles, an empty SMTP submission log, and a
    /// `"http://localhost"` base URL. Tests that need to drive a
    /// specific log clone the field after construction.
    pub fn for_test(fixture: crate::shared::FixtureHandle) -> Self {
        Self {
            shared: SharedHandles::for_test(fixture),
            submission_log: smtp::SubmissionLog::default(),
            base_url: "http://localhost".into(),
        }
    }

    /// Replace the request log on the shared handle bag.
    pub fn with_request_log(mut self, log: crate::request_log::RequestLog) -> Self {
        self.shared.request_log = log;
        self
    }

    /// Attach a Lua dispatcher.
    pub fn with_dispatcher(mut self, dispatcher: Arc<crate::lua::Dispatcher>) -> Self {
        self.shared.dispatcher = Some(dispatcher);
        self
    }

    /// Replace the OAuth token store.
    pub fn with_token_store(mut self, store: crate::oauth::TokenStore) -> Self {
        self.shared.token_store = store;
        self
    }

    /// Replace the SMTP submission log.
    pub fn with_submission_log(mut self, log: smtp::SubmissionLog) -> Self {
        self.submission_log = log;
        self
    }
}

/// Bearer-enforcement coverage on this router (verified
/// 2026-05-09):
///
/// Gated when `fixture.oauth.enforce = true`:
///   - `GET /.well-known/jmap`, `GET /jmap/session`,
///     `POST /jmap/api`, `GET /jmap/download/...`,
///     `POST /jmap/upload/{accountId}` (each handler calls
///     `enforce_bearer` directly).
///
/// Always reachable, even with enforcement on:
///   - `GET /` - static `"saehrimnir\n"` banner; intended health
///     probe.
///   - `POST /oauth/token`, `GET /oauth/userinfo`,
///     `POST /test/oauth/invalidate` - OAuth bootstrap, must not
///     be bearer-gated or the client cannot mint a token.
///   - `GET /test/smtp/submissions`, `DELETE /test/smtp/submissions`,
///     `GET /test/requests`, `DELETE /test/requests`,
///     `POST /test/fixture/reset`, `POST /test/fixture/step` -
///     test-only admin routes; safe because `main.rs` binds the
///     listener on `127.0.0.1` only.
///
/// 404-via-axum-default (no bypass risk):
///   - `/jmap/eventsource/...` is advertised in the session
///     resource but unrouted here.
///
/// SECURITY TODO (2026-05-09 review): if a future flag exposes a
/// non-loopback bind, every `/test/*` route becomes a remote-
/// control surface. Gate them behind `--enable-admin` or a
/// process-only Unix socket at that point.
pub fn router(state: AppState) -> Router {
    let oauth_token_router: Router = Router::new()
        .route("/oauth/token", post(oauth::token_endpoint))
        .with_state(oauth::TokenEndpointState {
            fixture: Arc::clone(&state.shared.fixture),
            store: state.shared.token_store.clone(),
        });
    let oauth_invalidate_router: Router = Router::new()
        .route("/test/oauth/invalidate", post(oauth::invalidate_endpoint))
        .with_state(state.shared.token_store.clone());
    let oauth_userinfo_router: Router = Router::new()
        .route("/oauth/userinfo", get(oauth::userinfo_endpoint))
        .with_state(oauth::UserInfoState {
            fixture: Arc::clone(&state.shared.fixture),
            store: state.shared.token_store.clone(),
        });

    Router::new()
        .route("/", get(root))
        .route("/.well-known/jmap", get(session))
        .route("/jmap/session", get(session))
        .route("/jmap/api", post(api))
        .route("/jmap/upload/{account_id}", post(upload))
        .route("/jmap/ws", get(jmap_ws))
        .route(
            "/jmap/download/{account_id}/{blob_id}/{name}",
            get(download),
        )
        .route("/{*discovery_path}", get(crate::discovery::dispatch))
        .with_state(state.clone())
        .merge(crate::test_admin::router(state))
        .merge(oauth_token_router)
        .merge(oauth_userinfo_router)
        .merge(oauth_invalidate_router)
}

/// Advertised JMAP `urn:ietf:params:jmap:submission` `maxDelayedSend`
/// (seconds). Non-zero so bifrost enables `scheduled_send`; generous
/// (365 days) so a harness can stage a hold-until well into the future
/// without tripping bifrost's `validate_scheduled` window check. The
/// mock has no clock, so it never actually defers delivery.
const SUBMISSION_MAX_DELAYED_SEND: u64 = 31_536_000;

async fn root() -> &'static str {
    "saehrimnir\n"
}

/// Session resource per RFC 8620 §2.
///
/// Capabilities are deliberately limited to `core` + `mail` (see
/// `notes/ratatoskr-jmap-surface.md` - advertising `principals`
/// pulls the client into `Principal/get` and `ShareNotification`
/// paths the mock can't satisfy in v0).
// The Err variant is an axum `Response` (large); boxing it would break
// the handler's `IntoResponse` impl, so allow rather than shrink. This
// mirrors `enforce_bearer`, which boxes `Response` where it can.
#[allow(clippy::result_large_err)]
async fn session(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, Response> {
    // Forced session-open failure (test-only). bifrost establishes a
    // JMAP account by fetching this resource; an armed `open_fault`
    // short-circuits the fetch with an HTTP error so the consumer's
    // account-open / reopen attempt fails. Checked before bearer
    // enforcement so the fault forces failure regardless of the auth
    // state of the open attempt. Each session GET consumes one attempt
    // from the knob; see `crate::open_fault`.
    if let Some((status, detail)) = state.shared.open_fault.take() {
        return Err(open_fault_response(status, &detail));
    }
    enforce_bearer(&state, &headers).map_err(|b| *b)?;
    let fixture = state.shared.fixture.read().expect("fixture lock poisoned");
    // Resolve the caller's account from the bearer token (parallel
    // to Gmail / Graph / People / IMAP-SASL). Falls back to the
    // fixture primary when no token is bound or auth is not
    // enforced, which keeps single-account fixtures and the
    // no-auth baseline working unchanged. Without this, every
    // bearer (including a secondary-account token minted via
    // `/oauth/token`) sees `primaryAccounts.{mail,calendars}`
    // pinned to the fixture primary - and ratatoskr's JMAP client
    // then routes every subsequent method call under the wrong
    // accountId.
    let caller_id =
        crate::oauth::account_from_bearer(&fixture, &state.shared.token_store, &headers);
    let caller_account = fixture
        .account(&caller_id)
        .unwrap_or_else(|| fixture.primary_account());
    let primary_id = caller_account.id.clone();
    let primary_name = caller_account.name.clone();
    let base = state.base_url.as_str();

    // Stage 2 of the multi-account refactor: enumerate every
    // declared account, deriving per-account capabilities from
    // that account's own resources. Calendar capabilities advertise
    // only on accounts that actually carry calendars; mail
    // capabilities always advertise (every fixture account has at
    // least a notional inbox in v0, even when no mailbox rows are
    // declared on it yet).
    let mut accounts = serde_json::Map::new();
    for acct in &fixture.accounts {
        let mut caps = serde_json::Map::new();
        caps.insert("urn:ietf:params:jmap:mail".to_string(), json!({}));
        // Every fixture account is submission-capable in v0 (the send
        // cut routes SEND / drafts / scheduled-send / raw-MDN through
        // this surface). `maxDelayedSend` advertises the deferred-send
        // window; bifrost gates `scheduled_send` on it being non-zero.
        caps.insert(
            "urn:ietf:params:jmap:submission".to_string(),
            json!({
                "maxDelayedSend": SUBMISSION_MAX_DELAYED_SEND,
                "submissionExtensions": {},
            }),
        );
        let has_calendars = fixture.calendars_for(&acct.id).next().is_some();
        if has_calendars {
            caps.insert(
                "urn:ietf:params:jmap:calendars".to_string(),
                json!({
                    "minDateTime": "1970-01-01T00:00:00",
                    "maxDateTime": "2099-12-31T23:59:59",
                    "maxExpandedQueryDuration": "P1Y",
                    "maxParticipantsPerEvent": 256,
                    "mayCreateCalendar": true,
                }),
            );
        }
        // Contacts advertise on accounts that own an address book
        // (contact folder). Gates the JMAP `AddressBook/*` +
        // `ContactCard/*` surface; bifrost resolves the contacts
        // account from this capability, so a fixture without contact
        // folders never enters the contacts sync flow.
        let has_contacts = fixture.contact_folders_for(&acct.id).next().is_some();
        if has_contacts {
            caps.insert(
                "urn:ietf:params:jmap:contacts".to_string(),
                json!({ "mayCreateAddressBook": true }),
            );
        }
        accounts.insert(
            acct.id.clone(),
            json!({
                "name": acct.name,
                "isPersonal": true,
                "isReadOnly": false,
                "accountCapabilities": Value::Object(caps),
            }),
        );
    }

    // `primaryAccounts` advertises the bearer-resolved caller as
    // the default account for each capability (resolved above via
    // `account_from_bearer`); per-method `accountId` args still
    // select the actual scope on follow-up calls. With no bearer
    // or with `[oauth] enforce = false`, this falls back to the
    // fixture primary - same shape single-account fixtures saw
    // before the multi-account refactor.
    let primary_has_calendars = fixture.calendars_for(&primary_id).next().is_some();
    let mut primary = serde_json::Map::new();
    primary.insert(
        "urn:ietf:params:jmap:core".to_string(),
        Value::String(primary_id.clone()),
    );
    primary.insert(
        "urn:ietf:params:jmap:mail".to_string(),
        Value::String(primary_id.clone()),
    );
    // The submission account bifrost resolves
    // (`client.primary_account::<Submission>()`) for every send path.
    primary.insert(
        "urn:ietf:params:jmap:submission".to_string(),
        Value::String(primary_id.clone()),
    );
    if primary_has_calendars {
        primary.insert(
            "urn:ietf:params:jmap:calendars".to_string(),
            Value::String(primary_id.clone()),
        );
    }
    let primary_has_contacts = fixture.contact_folders_for(&primary_id).next().is_some();
    if primary_has_contacts {
        primary.insert(
            "urn:ietf:params:jmap:contacts".to_string(),
            Value::String(primary_id.clone()),
        );
    }
    let advertise_calendars = !fixture.calendars.is_empty();
    let advertise_contacts = !fixture.contact_folders.is_empty();

    let mut top_caps = serde_json::Map::new();
    top_caps.insert(
        "urn:ietf:params:jmap:core".to_string(),
        json!({
            "maxSizeUpload": 50_000_000_u64,
            "maxConcurrentUpload": 4,
            "maxSizeRequest": 10_000_000_u64,
            "maxConcurrentRequests": 4,
            "maxCallsInRequest": 16,
            "maxObjectsInGet": 500,
            "maxObjectsInSet": 500,
            "collationAlgorithms": [],
        }),
    );
    top_caps.insert("urn:ietf:params:jmap:mail".to_string(), json!({}));
    // Session-level submission capability. bifrost reads
    // `maxDelayedSend` here to decide whether scheduled send is
    // supported (non-zero => enabled).
    top_caps.insert(
        "urn:ietf:params:jmap:submission".to_string(),
        json!({
            "maxDelayedSend": SUBMISSION_MAX_DELAYED_SEND,
            "submissionExtensions": {},
        }),
    );
    // RFC 8887 WebSocket push capability. `url` carries the ws:// (or
    // wss://) endpoint a client upgrades for the StateChange channel;
    // `supportsPush` advertises that the server emits `StateChange`
    // objects (vs. WebSocket-as-request-transport only). Derived from
    // the advertised HTTP base by swapping the scheme so it tracks the
    // actually-bound listener. See `notes/ratatoskr-jmap-surface.md`.
    let ws_base = base
        .strip_prefix("https://")
        .map(|rest| format!("wss://{rest}"))
        .or_else(|| {
            base.strip_prefix("http://")
                .map(|rest| format!("ws://{rest}"))
        })
        .unwrap_or_else(|| base.to_string());
    top_caps.insert(
        "urn:ietf:params:jmap:websocket".to_string(),
        json!({
            "url": format!("{ws_base}/jmap/ws"),
            "supportsPush": true,
        }),
    );
    if advertise_calendars {
        top_caps.insert("urn:ietf:params:jmap:calendars".to_string(), json!({}));
    }
    if advertise_contacts {
        top_caps.insert("urn:ietf:params:jmap:contacts".to_string(), json!({}));
    }

    Ok(Json(json!({
        "capabilities": Value::Object(top_caps),
        "accounts": accounts,
        "primaryAccounts": primary,
        "username": primary_name,
        "apiUrl": format!("{base}/jmap/api"),
        "downloadUrl": format!("{base}/jmap/download/{{accountId}}/{{blobId}}/{{name}}?accept={{type}}"),
        "uploadUrl": format!("{base}/jmap/upload/{{accountId}}"),
        "eventSourceUrl": format!("{base}/jmap/eventsource/?types={{types}}&closeafter={{closeafter}}&ping={{ping}}"),
        "state": fixture.primary_state()
    })))
}

/// JMAP method-call endpoint. Always 200; per-call errors land in the
/// envelope's `methodResponses`. JSON parse failures bubble up as 400
/// via axum's `Json` extractor, which is the right behaviour per RFC
/// 8620 §3.6.1.
// Err is an axum `Response` (large); boxing breaks `IntoResponse`. See
// the note on `session`.
#[allow(clippy::result_large_err)]
async fn api(
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
    Json(req): Json<JmapRequest>,
) -> Result<Json<JmapResponse>, Response> {
    enforce_bearer(&state, &headers).map_err(|b| *b)?;
    state.shared.latency.sleep_for("jmap").await;
    // Record one request-log entry per method-call in the envelope so
    // `GET /test/requests` reflects the granularity ratatoskr cares
    // about (per-method asserts, not per-batch). Batched into a
    // single `extend` so a 16-call envelope doesn't take 16 lock
    // round-trips.
    let now = Utc::now();
    state
        .shared
        .request_log
        .extend(req.method_calls.iter().map(|(method, args, call_id)| {
            // Surface the high-signal arg fields ratatoskr's delta
            // scripts care about: `accountId`, the `ids[]` list when
            // the call carries a string-typed one (Mailbox/get,
            // Email/get, future */set), and the `properties[]` list
            // (lets a script distinguish `Email/get properties=
            // [bodyValues, ...]` from a metadata-only get). Filter
            // arguments and result references are deliberately left
            // out: they're shape-sensitive and would bloat the log
            // without giving asserts a stable matcher.
            let mut detail = json!({ "call_id": call_id });
            if let Some(account_id) = args.get("accountId").and_then(|v| v.as_str()) {
                detail["account_id"] = json!(account_id);
            }
            if let Some(ids) = args.get("ids").and_then(|v| v.as_array()) {
                let strs: Vec<&str> = ids.iter().filter_map(|v| v.as_str()).collect();
                if !strs.is_empty() {
                    detail["ids"] = json!(strs);
                }
            }
            if let Some(props) = args.get("properties").and_then(|v| v.as_array()) {
                let strs: Vec<&str> = props.iter().filter_map(|v| v.as_str()).collect();
                if !strs.is_empty() {
                    detail["properties"] = json!(strs);
                }
            }
            RequestEntry {
                protocol: "jmap",
                command: method.clone(),
                received_at: now,
                detail,
                connection_id,
            }
        }));
    // `jmap::handle` decides between a read and a write guard
    // internally based on whether the envelope contains a mutating
    // method (Email/set, Mailbox/set). Passing the handle (not a
    // guard) keeps the locking decision colocated with the dispatcher.
    Ok(Json(jmap::handle(
        &state.shared.fixture,
        state.shared.dispatcher.as_ref(),
        req,
    )))
}

/// Translate a `BearerDecision::Deny` into a JMAP-shaped 401 with a
/// `urn:ietf:params:jmap:error:forbidden`-shaped body. Real JMAP
/// servers tend to reply with bare HTTP 401; ratatoskr is fine with
/// either, and a JSON body keeps the test surface uniform across
/// the three HTTP-based protocols.
///
/// Why this is a per-handler helper rather than a tower middleware
/// (cf. `graph/mod.rs::enforce_bearer_middleware`,
/// `gmail/mod.rs::enforce_bearer_middleware`): the OAuth sub-
/// routers (`/oauth/token`, `/oauth/userinfo`,
/// `/test/oauth/invalidate`) are merged into this same router and
/// must NOT be bearer-gated - they're how the client mints a
/// token in the first place. Layering bearer middleware on the
/// JMAP router would lock those out and make the surface
/// unbootstrappable. Calling this from each protected handler is
/// the simplest way to keep the bootstrap routes open.
fn enforce_bearer(state: &AppState, headers: &HeaderMap) -> Result<(), Box<Response>> {
    // `expect` is fine here: the only path that poisons the fixture
    // lock is a panic while a handler held the write guard, which
    // means the process is already in an undefined state and there
    // is no graceful answer to give the bearer check anyway. Same
    // rationale as the other `fixture lock poisoned` panics.
    #[allow(clippy::unwrap_in_result)]
    let fix = state.shared.fixture.read().expect("fixture lock poisoned");
    match crate::oauth::check_bearer(&fix, &state.shared.token_store, headers) {
        BearerDecision::Allow => Ok(()),
        BearerDecision::Deny(reason) => Err(Box::new(
            (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                Json(json!({
                    "type": "urn:ietf:params:jmap:error:forbidden",
                    "status": 401,
                    "detail": reason,
                })),
            )
                .into_response(),
        )),
    }
}

/// Build the HTTP error returned when the forced session-open failure
/// knob (`crate::open_fault::OpenFault`) is armed. `status` is the
/// caller-chosen HTTP status (defaulting to 503 at the arming site);
/// an out-of-range value falls back to 503. The body reuses the
/// JMAP-shaped problem object the other JMAP error paths emit so the
/// failure surface stays uniform; `Client::connect` only needs the
/// non-2xx status to treat the open as failed.
fn open_fault_response(status: u16, detail: &str) -> Response {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
    (
        code,
        Json(json!({
            "type": "urn:ietf:params:jmap:error:serverUnavailable",
            "status": status,
            "detail": detail,
        })),
    )
        .into_response()
}

/// Blob-download endpoint per RFC 8620 §6.2. The session resource
/// advertises the URL template `/jmap/download/{accountId}/{blobId}/
/// {name}` plus an `accept` query string; we accept any path-shape
/// the client renders and resolve `blob_id` against every email's
/// attachments. Filenames are echoed in `Content-Disposition` but
/// otherwise unused for resolution. The mock doesn't validate
/// `account_id` (single-account in v0).
async fn download(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((_account_id, blob_id, _name)): Path<(String, String, String)>,
) -> Response {
    if let Err(deny) = enforce_bearer(&state, &headers) {
        return *deny;
    }
    state.shared.latency.sleep_for_attachment(&blob_id).await;
    let fixture = state.shared.fixture.read().expect("fixture lock poisoned");
    for email in &fixture.emails {
        // Whole-message raw RFC822 blob. The `Email` object reports its
        // own `blobId` as `blob-<id>` (see `src/jmap.rs`); bifrost's
        // `open_raw_rfc822` (`crates/jmap/src/sync/blob.rs`) reads that
        // `blobId` off an `Email/get` and downloads it to scan the
        // assembled message (e.g. for a `Disposition-Notification-To`
        // MDN request). Serve the same byte stream the IMAP `BODY[]` /
        // Graph `$value` surfaces emit so all three agree.
        if blob_id == format!("blob-{}", email.id) {
            let raw = crate::imap::assembled_rfc822(email);
            return (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, "message/rfc822".to_string()),
                    (header::CONTENT_DISPOSITION, "attachment".to_string()),
                ],
                Body::from(raw.into_bytes()),
            )
                .into_response();
        }
        // Text body-part blob (`blob-<id>-text`), the part-level blob the
        // `Email/get` `textBody` projection advertises. Cheap to serve and
        // flagged alongside the whole-message blob in `gaps.md`.
        if blob_id == format!("blob-{}-text", email.id) {
            let crate::fixture::Body::Text(text) = &email.body;
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/plain".to_string())],
                Body::from(text.clone().into_bytes()),
            )
                .into_response();
        }
        for att in &email.attachments {
            if att.blob_id == blob_id {
                // RFC 5987 `filename*=UTF-8''<percent-encoded>`
                // form. Avoids splicing a fixture-supplied name
                // unquoted into the header, which would let `"`
                // or CRLF in the name break header framing or
                // inject another field. All modern user agents
                // honour the `filename*` form.
                let disposition = format!(
                    "{}; filename*=UTF-8''{}",
                    att.disposition.as_str(),
                    rfc5987_encode(&att.name),
                );
                return (
                    StatusCode::OK,
                    [
                        (header::CONTENT_TYPE, att.content_type.clone()),
                        (header::CONTENT_DISPOSITION, disposition),
                    ],
                    Body::from(att.data.clone()),
                )
                    .into_response();
            }
        }
    }
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "type": "urn:ietf:params:jmap:error:notFound",
            "status": 404,
            "detail": format!("blob {blob_id} not found"),
        })),
    )
        .into_response()
}

/// Blob-upload endpoint per RFC 8620 §6.1. The session resource
/// advertises the URL template `/jmap/upload/{accountId}`; the client
/// POSTs raw octets with a `Content-Type` and receives
/// `{ accountId, blobId, type, size }`. bifrost's raw-send path uploads
/// the assembled RFC 822 message here, then references the returned
/// `blobId` from an `Email/import`. The bytes are stashed in
/// `Fixture::uploaded_blobs` so `Email/import` can project them. Bearer-
/// gated under `[oauth] enforce`. Records a `("jmap", "Blob/upload")`
/// request-log entry so a harness can assert the upload fired.
async fn upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::connection_id::OptConnId(connection_id): crate::connection_id::OptConnId,
    Path(account_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    if let Err(deny) = enforce_bearer(&state, &headers) {
        return *deny;
    }
    state.shared.latency.sleep_for("jmap").await;
    state.shared.request_log.record_with_conn(
        "jmap",
        "Blob/upload".to_string(),
        json!({ "account_id": account_id }),
        connection_id,
    );
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let size = body.len();
    let blob_id = {
        let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
        fix.store_uploaded_blob(content_type.clone(), body.to_vec())
    };
    (
        StatusCode::CREATED,
        Json(json!({
            "accountId": account_id,
            "blobId": blob_id,
            "type": content_type,
            "size": size,
        })),
    )
        .into_response()
}

/// JMAP WebSocket push endpoint (RFC 8887). A client upgrades here for
/// the `StateChange` channel advertised in the session's
/// `urn:ietf:params:jmap:websocket` capability. On upgrade we resolve
/// the caller's account from the bearer (same resolution as the session
/// resource), register a push listener in the [`crate::push::PushHub`]
/// keyed by that account, and stream every `StateChange` frame the
/// state-mutation trigger emits for it.
///
/// Inbound frames are accepted leniently: a `WebSocketPushEnable`
/// request (RFC 8887 §4) is acked into the push channel by virtue of
/// the registration already being live; any other text frame is
/// ignored (the mock does not serve JMAP method calls over WebSocket
/// in v0). A close frame, a transport error, or a dropped receiver
/// tears the registration down.
async fn jmap_ws(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> Response {
    let account_id = {
        let fix = state.shared.fixture.read().expect("fixture lock poisoned");
        crate::oauth::account_from_bearer(&fix, &state.shared.token_store, &headers)
    };
    // RFC 8887 §3.1: the subprotocol token is "jmap".
    ws.protocols(["jmap"])
        .on_upgrade(move |socket| jmap_ws_loop(socket, state, account_id))
}

/// Drive one upgraded JMAP WebSocket: forward push frames from the hub
/// to the socket, drain inbound frames until close.
async fn jmap_ws_loop(
    mut socket: axum::extract::ws::WebSocket,
    state: AppState,
    account_id: String,
) {
    use axum::extract::ws::Message;

    let mut rx = state.shared.push.register_jmap_ws(account_id);
    loop {
        tokio::select! {
            frame = rx.recv() => {
                match frame {
                    Some(text) => {
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
            inbound = socket.recv() => {
                match inbound {
                    // Accept and ignore client frames (e.g.
                    // WebSocketPushEnable); the registration is already
                    // live so push flows regardless.
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

/// Percent-encode `s` for use as the value of an RFC 5987
/// `filename*=UTF-8''...` parameter. Per the spec, only `attr-char`
/// (ALPHA / DIGIT / `! # $ & + - . ^ _ \` | ~`) passes through
/// unescaped; everything else (including space, `"`, `'`, `*`, `,`,
/// `;`, `=`, CTLs, and any non-ASCII byte) is `%HH` encoded against
/// the UTF-8 byte stream.
fn rfc5987_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let pass_through = b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            );
        if pass_through {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}
