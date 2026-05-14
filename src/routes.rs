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
///     `POST /jmap/api`, `GET /jmap/download/...` (each handler
///     calls `enforce_bearer` directly).
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
///   - `/jmap/upload/{accountId}` and `/jmap/eventsource/...`
///     are advertised in the session resource but unrouted here.
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
        .route(
            "/jmap/download/{account_id}/{blob_id}/{name}",
            get(download),
        )
        .with_state(state.clone())
        .merge(crate::test_admin::router(state))
        .merge(oauth_token_router)
        .merge(oauth_userinfo_router)
        .merge(oauth_invalidate_router)
}

async fn root() -> &'static str {
    "saehrimnir\n"
}

/// Session resource per RFC 8620 §2.
///
/// Capabilities are deliberately limited to `core` + `mail` (see
/// `notes/ratatoskr-jmap-surface.md` - advertising `principals`
/// pulls the client into `Principal/get` and `ShareNotification`
/// paths the mock can't satisfy in v0).
async fn session(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, Response> {
    enforce_bearer(&state, &headers).map_err(|b| *b)?;
    let fixture = state.shared.fixture.read().expect("fixture lock poisoned");
    let primary_account = fixture.primary_account();
    let primary_id = primary_account.id.clone();
    let primary_name = primary_account.name.clone();
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
        let has_calendars = fixture.calendars_for(&acct.id).next().is_some();
        if has_calendars {
            caps.insert("urn:ietf:params:jmap:calendars".to_string(), json!({
                "minDateTime": "1970-01-01T00:00:00",
                "maxDateTime": "2099-12-31T23:59:59",
                "maxExpandedQueryDuration": "P1Y",
                "maxParticipantsPerEvent": 256,
                "mayCreateCalendar": true,
            }));
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

    // The top-level capabilities advertisement and `primaryAccounts`
    // mapping pin the primary account; per-method `accountId` args
    // select the actual scope on follow-up calls.
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
    if primary_has_calendars {
        primary.insert(
            "urn:ietf:params:jmap:calendars".to_string(),
            Value::String(primary_id.clone()),
        );
    }
    let advertise_calendars = !fixture.calendars.is_empty();

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
    if advertise_calendars {
        top_caps.insert(
            "urn:ietf:params:jmap:calendars".to_string(),
            json!({}),
        );
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
        "state": fixture.state
    })))
}

/// JMAP method-call endpoint. Always 200; per-call errors land in the
/// envelope's `methodResponses`. JSON parse failures bubble up as 400
/// via axum's `Json` extractor, which is the right behaviour per RFC
/// 8620 §3.6.1.
async fn api(
    State(state): State<AppState>,
    headers: HeaderMap,
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
                b'!' | b'#'
                    | b'$'
                    | b'&'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'|'
                    | b'~'
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
