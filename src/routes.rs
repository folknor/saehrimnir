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
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};

use crate::fixture::Fixture;
use crate::jmap::{self, JmapRequest, JmapResponse};
use crate::oauth::{self, BearerDecision, TokenStore};
use crate::request_log::{RequestEntry, RequestLog};
use crate::smtp::{self, Submission};

#[derive(Clone)]
pub struct AppState {
    pub fixture: Arc<Fixture>,
    pub dispatcher: Option<Arc<crate::lua::Dispatcher>>,
    /// Shared handle to the SMTP submission log. The test-only
    /// `/test/smtp/submissions` route reads (and `DELETE` clears)
    /// this. Tests that don't drive SMTP can construct it via
    /// `SubmissionLog::default()`.
    pub submission_log: smtp::SubmissionLog,
    /// Cross-protocol request log. Threaded into all five
    /// protocol layers so a single `GET /test/requests` snapshot
    /// covers everything the harness has driven. Cheap to clone.
    pub request_log: RequestLog,
    /// OAuth token store. `/oauth/token` mints into it,
    /// `/oauth/userinfo` and the bearer-enforcement middleware
    /// consult it, `/test/oauth/invalidate` and
    /// `/test/fixture/reset` clear/remove from it.
    pub token_store: TokenStore,
}

pub fn router(state: AppState) -> Router {
    let oauth_token_router: Router = Router::new()
        .route("/oauth/token", post(oauth::token_endpoint))
        .with_state(state.token_store.clone());
    let oauth_invalidate_router: Router = Router::new()
        .route("/test/oauth/invalidate", post(oauth::invalidate_endpoint))
        .with_state(state.token_store.clone());
    let oauth_userinfo_router: Router = Router::new()
        .route("/oauth/userinfo", get(oauth::userinfo_endpoint))
        .with_state(oauth::UserInfoState {
            fixture: Arc::clone(&state.fixture),
            store: state.token_store.clone(),
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
        .route(
            "/test/smtp/submissions",
            get(list_smtp_submissions).delete(clear_smtp_submissions),
        )
        .route(
            "/test/requests",
            get(list_requests).delete(clear_requests),
        )
        .route("/test/fixture/reset", post(reset_fixture))
        .route("/test/fixture/step", post(step_fixture))
        .with_state(state)
        .merge(oauth_token_router)
        .merge(oauth_userinfo_router)
        .merge(oauth_invalidate_router)
}

async fn root() -> &'static str {
    "saehrimnir\n"
}

/// Derive the externally-visible base URL (`scheme://host[:port]`) from
/// the request `Host` header so the session resource can advertise
/// absolute `apiUrl` / `downloadUrl` / `uploadUrl` / `eventSourceUrl`
/// values, per RFC 8620 §2 (URL templates resolve against the session
/// URL, but ratatoskr's client treats them as absolute). The JMAP
/// listener is plain HTTP; no TLS termination in v0.
fn base_url(headers: &HeaderMap) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    format!("http://{host}")
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
    let fixture = &state.fixture;
    let acct_id = &fixture.account.id;
    let acct_name = &fixture.account.name;
    let base = base_url(&headers);

    let mut accounts = serde_json::Map::new();
    accounts.insert(
        acct_id.clone(),
        json!({
            "name": acct_name,
            "isPersonal": true,
            "isReadOnly": false,
            "accountCapabilities": {
                "urn:ietf:params:jmap:mail": {}
            }
        }),
    );

    let mut primary = serde_json::Map::new();
    primary.insert(
        "urn:ietf:params:jmap:core".to_string(),
        Value::String(acct_id.clone()),
    );
    primary.insert(
        "urn:ietf:params:jmap:mail".to_string(),
        Value::String(acct_id.clone()),
    );

    Ok(Json(json!({
        "capabilities": {
            "urn:ietf:params:jmap:core": {
                "maxSizeUpload": 50_000_000_u64,
                "maxConcurrentUpload": 4,
                "maxSizeRequest": 10_000_000_u64,
                "maxConcurrentRequests": 4,
                "maxCallsInRequest": 16,
                "maxObjectsInGet": 500,
                "maxObjectsInSet": 500,
                "collationAlgorithms": []
            },
            "urn:ietf:params:jmap:mail": {}
        },
        "accounts": accounts,
        "primaryAccounts": primary,
        "username": acct_name,
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
    // Record one request-log entry per method-call in the envelope so
    // `GET /test/requests` reflects the granularity ratatoskr cares
    // about (per-method asserts, not per-batch).
    for (method, _args, call_id) in &req.method_calls {
        state.request_log.record(
            "jmap",
            method.clone(),
            json!({ "call_id": call_id }),
        );
    }
    Ok(Json(jmap::handle(
        &state.fixture,
        state.dispatcher.as_ref(),
        req,
    )))
}

/// Translate a `BearerDecision::Deny` into a JMAP-shaped 401 with a
/// `urn:ietf:params:jmap:error:forbidden`-shaped body. Real JMAP
/// servers tend to reply with bare HTTP 401; ratatoskr is fine with
/// either, and a JSON body keeps the test surface uniform across
/// the three HTTP-based protocols.
fn enforce_bearer(state: &AppState, headers: &HeaderMap) -> Result<(), Box<Response>> {
    match crate::oauth::check_bearer(&state.fixture, &state.token_store, headers) {
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
    for email in &state.fixture.emails {
        for att in &email.attachments {
            if att.blob_id == blob_id {
                return (
                    StatusCode::OK,
                    [
                        (header::CONTENT_TYPE, att.content_type.clone()),
                        (
                            header::CONTENT_DISPOSITION,
                            format!(
                                "{}; filename=\"{}\"",
                                att.disposition.as_str(),
                                att.name
                            ),
                        ),
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

// ── Test-only SMTP submission introspection ────────────────────────
//
// Sæhrimnir is a test-only binary; no auth or feature gate guards
// these routes. The submission log is process-scoped, so tests can
// either restart the binary for a clean window or hit the `DELETE`.

/// JSON view of a captured SMTP submission. Mirrors `Submission`'s
/// connection-level fields plus a parsed projection of the RFC 822
/// body. Raw bytes are deliberately not serialized; tests assert on
/// `parsed.attachments[].size` and `raw_size` instead.
#[derive(Debug, Serialize)]
pub struct SubmissionJson {
    pub from: String,
    pub recipients: Vec<String>,
    pub from_params: std::collections::BTreeMap<String, String>,
    pub rcpt_params: Vec<std::collections::BTreeMap<String, String>>,
    pub auth_mechanism: Option<String>,
    pub received_at: DateTime<Utc>,
    pub raw_size: usize,
    pub parsed: Option<ParsedJson>,
}

#[derive(Debug, Serialize)]
pub struct ParsedJson {
    pub subject: Option<String>,
    pub text_body_count: usize,
    pub html_body_count: usize,
    pub attachments: Vec<AttachmentJson>,
}

#[derive(Debug, Serialize)]
pub struct AttachmentJson {
    pub filename: Option<String>,
    pub content_type: String,
    pub size: usize,
}

impl SubmissionJson {
    fn from_submission(s: &Submission) -> Self {
        let parsed = s.parse_mime().map(|p| ParsedJson {
            subject: p.subject,
            text_body_count: p.text_bodies.len(),
            html_body_count: p.html_bodies.len(),
            attachments: p
                .attachments
                .into_iter()
                .map(|a| AttachmentJson {
                    filename: a.filename,
                    content_type: a.content_type,
                    size: a.data.len(),
                })
                .collect(),
        });
        SubmissionJson {
            from: s.from.clone(),
            recipients: s.recipients.clone(),
            from_params: s.from_params.clone(),
            rcpt_params: s.rcpt_params.clone(),
            auth_mechanism: s.auth_mechanism.clone(),
            received_at: s.received_at,
            raw_size: s.data.len(),
            parsed,
        }
    }
}

async fn list_smtp_submissions(State(state): State<AppState>) -> Json<Vec<SubmissionJson>> {
    let snapshot = state.submission_log.snapshot();
    Json(
        snapshot
            .iter()
            .map(SubmissionJson::from_submission)
            .collect(),
    )
}

async fn clear_smtp_submissions(State(state): State<AppState>) -> StatusCode {
    state.submission_log.clear();
    StatusCode::NO_CONTENT
}

// ── Test-only cross-protocol request log ───────────────────────────
//
// One entry per protocol-level dispatch event, wired into every
// protocol layer (JMAP method calls in `api`, IMAP commands in
// `imap.rs`, SMTP commands in `smtp.rs`, Graph + Gmail HTTP via
// the request-logging axum middleware in their respective
// modules). Tests assert on `(protocol, command, detail)`;
// `received_at` is wall-clock so byte-stable rendering is not a
// goal.

async fn list_requests(State(state): State<AppState>) -> Json<Vec<RequestEntry>> {
    Json(state.request_log.snapshot())
}

async fn clear_requests(State(state): State<AppState>) -> StatusCode {
    state.request_log.clear();
    StatusCode::NO_CONTENT
}

// ── Test-only fixture admin ─────────────────────────────────────────

/// Reset all in-process mutable state to the post-load baseline.
/// Today that is exactly two pieces: the SMTP submission log and
/// the cross-protocol request log. The fixture itself is read-only
/// in v0 (IMAP `UID STORE` is a non-persistent no-op), so resetting
/// is purely a log-clearing operation. When mutation lands
/// (`[[change]]` scripts, persistent UID STORE, etc.), the
/// implementation grows here without changing the route shape.
/// Returns 204 unconditionally so harness scripts get a stable
/// contract.
async fn reset_fixture(State(state): State<AppState>) -> StatusCode {
    state.submission_log.clear();
    state.request_log.clear();
    state.token_store.clear();
    StatusCode::NO_CONTENT
}

/// Advance one scenario step. Paired with `[[change]]` script
/// entries (see TODO.md "Fixture format growth"), which don't yet
/// exist; today there are no steps to advance, so the route
/// returns 501 with a JSON body so harness scripts can detect the
/// gap rather than silently no-op. When change scripts land, this
/// becomes the dispatch point.
async fn step_fixture() -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": "fixture step not implemented",
            "detail": "no [[change]] scripts in v0; see TODO.md \"Fixture format growth\""
        })),
    )
        .into_response()
}
