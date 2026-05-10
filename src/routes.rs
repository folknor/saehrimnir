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
use serde_json::{Map, Value, json};

use crate::jmap::{self, JmapRequest, JmapResponse};
use crate::oauth::{self, BearerDecision};
use crate::request_log::RequestEntry;
use crate::shared::SharedHandles;
use crate::smtp::{self, Submission};

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
        .with_state(state.shared.token_store.clone());
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
        .route("/test/snapshot-state", get(snapshot_state))
        .route("/test/latency", get(get_latency).post(set_latency))
        .with_state(state)
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
    let acct_id = &fixture.account.id;
    let acct_name = &fixture.account.name;
    let base = state.base_url.as_str();

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

/// `GET /test/requests` -> JSON array of every protocol-level
/// dispatch event the binary has handled. With `?stable=true` the
/// `received_at` wall-clock timestamp is stripped from each entry so
/// the rendered JSON is byte-deterministic across runs (useful for
/// snapshot-style assertions).
async fn list_requests(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let snap = state.shared.request_log.snapshot();
    if params.get("stable").map(String::as_str) == Some("true") {
        let stripped: Vec<Value> = snap
            .iter()
            .map(|e| {
                json!({
                    "protocol": e.protocol,
                    "command": e.command,
                    "detail": e.detail,
                })
            })
            .collect();
        Json(stripped).into_response()
    } else {
        Json(snap).into_response()
    }
}

async fn clear_requests(State(state): State<AppState>) -> StatusCode {
    state.shared.request_log.clear();
    StatusCode::NO_CONTENT
}

// ── Test-only fixture admin ─────────────────────────────────────────

/// `POST /test/fixture/reset` -> 204. Contract is documented in
/// `notes/orchestration.md` (the "Test / admin control plane"
/// section) and tracked there as the source of truth: this
/// handler is the implementation, not the spec.
///
/// In addition to clearing volatile state, this rewinds the fixture
/// image itself to the post-load baseline (cloned once at startup
/// and held on `SharedHandles`). That lets a harness re-run the
/// same `change(...)` script in one process without restarting the
/// binary - the cursor goes back to 0 and the next
/// `POST /test/fixture/step` applies step-1 against a pristine
/// image. The fixture's read lock is held only briefly.
async fn reset_fixture(State(state): State<AppState>) -> StatusCode {
    state.submission_log.clear();
    state.shared.request_log.clear();
    state.shared.token_store.clear();
    state.shared.latency.clear();
    if let Some(d) = &state.shared.dispatcher {
        d.reset_counts();
    }
    {
        let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
        *fix = (*state.shared.baseline).clone();
    }
    {
        let mut cursor = state.shared.change_cursor.lock().expect("cursor lock poisoned");
        *cursor = 0;
    }
    StatusCode::NO_CONTENT
}

// ── Test-only timing knob + state snapshot ──────────────────────────

/// `GET /test/latency` -> 200 + JSON snapshot of the per-protocol
/// latency map. Empty object when unset (the default). Keys are
/// `"global"` plus the protocol tags (`"jmap"` / `"imap"` /
/// `"smtp"` / `"graph"` / `"gmail"`); values are milliseconds.
async fn get_latency(State(state): State<AppState>) -> Json<Value> {
    let snap = state.shared.latency.snapshot();
    Json(json!(snap))
}

/// Upper bound on a single latency knob. Anything above this is a
/// harness mistake (or a hostile fixture); leaving the cap at
/// `u64::MAX` would let `global_ms = u64::MAX` deadlock every
/// dispatch path until SIGTERM. 60 seconds is well above the
/// largest delay any test or simulated-slow-server scenario should
/// want; raise if a fixture forces it.
const LATENCY_MAX_MS: u64 = 60_000;

/// `POST /test/latency` body:
/// ```text
/// { "global_ms": 50,                         // optional
///   "per_protocol": { "graph": 200 } }       // optional
/// ```
/// Either field may be absent. Each call replaces (not merges) the
/// affected keys; `"global_ms": 0` clears the global knob, and
/// `"per_protocol": {"graph": 0}` clears the graph entry. Values
/// above `LATENCY_MAX_MS` (60s) return 400. Returns 200 + the post-
/// update snapshot for round-trip verification.
async fn set_latency(
    State(state): State<AppState>,
    body: Option<Json<Value>>,
) -> Response {
    let body_obj: Map<String, Value> = match body {
        None => Map::new(),
        Some(Json(Value::Null)) => Map::new(),
        Some(Json(Value::Object(m))) => m,
        Some(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "malformed body",
                    "detail": "expected an object or empty body",
                })),
            )
                .into_response();
        }
    };
    if let Some(g) = body_obj.get("global_ms") {
        let n = match g.as_u64() {
            Some(n) => n,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": "malformed body",
                        "detail": "global_ms must be a non-negative integer",
                    })),
                )
                    .into_response();
            }
        };
        if n > LATENCY_MAX_MS {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "malformed body",
                    "detail": format!(
                        "global_ms {n} exceeds cap of {LATENCY_MAX_MS}ms"
                    ),
                })),
            )
                .into_response();
        }
        state.shared.latency.set("global", n);
    }
    if let Some(per) = body_obj.get("per_protocol") {
        let map = match per.as_object() {
            Some(m) => m,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": "malformed body",
                        "detail": "per_protocol must be an object",
                    })),
                )
                    .into_response();
            }
        };
        for (k, v) in map {
            if k == "global" {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": "malformed body",
                        "detail": "use top-level `global_ms`, not per_protocol.global",
                    })),
                )
                    .into_response();
            }
            let n = match v.as_u64() {
                Some(n) => n,
                None => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "error": "malformed body",
                            "detail": format!("per_protocol.{k} must be a non-negative integer"),
                        })),
                    )
                        .into_response();
                }
            };
            if n > LATENCY_MAX_MS {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": "malformed body",
                        "detail": format!(
                            "per_protocol.{k} value {n} exceeds cap of {LATENCY_MAX_MS}ms"
                        ),
                    })),
                )
                    .into_response();
            }
            state.shared.latency.set(k, n);
        }
    }
    Json(json!(state.shared.latency.snapshot())).into_response()
}

/// `GET /test/snapshot-state` -> 200 + JSON dump of the fixture's
/// current mailbox / email / event shape. Used by harness scripts
/// that want to verify post-step state without re-walking every
/// protocol.
///
/// The shape is deliberately a thin projection of the relevant
/// `Fixture` fields; raw bodies and attachments are deliberately
/// omitted (they bloat the response and aren't what assertions
/// check). For per-protocol wire shape, hit the protocol's GET
/// endpoints instead.
async fn snapshot_state(State(state): State<AppState>) -> Json<Value> {
    let fix = state.shared.fixture.read().expect("fixture lock poisoned");
    let mailboxes: Vec<Value> = fix
        .mailboxes
        .iter()
        .map(|m| {
            json!({
                "id": m.id,
                "name": m.name,
                "role": m.role.map(crate::fixture::Role::as_str),
                "parent_id": m.parent_id,
                "sort_order": m.sort_order,
                "is_subscribed": m.is_subscribed,
            })
        })
        .collect();
    let emails: Vec<Value> = fix
        .emails
        .iter()
        .map(|e| {
            json!({
                "id": e.id,
                "thread_id": e.thread_id,
                "mailbox_ids": e.mailbox_ids,
                "keywords": e.keywords,
                "subject": e.subject,
                "received_at": e.received_at.to_rfc3339(),
                "has_attachment": e.has_attachment,
            })
        })
        .collect();
    let events: Vec<Value> = fix
        .events
        .iter()
        .map(|e| {
            json!({
                "id": e.id,
                "calendar_id": e.calendar_id,
                "subject": e.subject,
                "start": e.start.to_rfc3339(),
                "end": e.end.to_rfc3339(),
                "location": e.location,
            })
        })
        .collect();
    let contact_folders: Vec<Value> = fix
        .contact_folders
        .iter()
        .map(|f| {
            json!({
                "id": f.id,
                "display_name": f.display_name,
                "parent_folder_id": f.parent_folder_id,
                "is_default": f.is_default,
            })
        })
        .collect();
    let contacts: Vec<Value> = fix
        .contacts
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "folder_id": c.folder_id,
                "display_name": c.display_name,
                "emails": c
                    .emails
                    .iter()
                    .map(|e| json!({ "address": e.address, "name": e.name }))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    Json(json!({
        "name": fix.name,
        "state": fix.state,
        "mailboxes": mailboxes,
        "emails": emails,
        "events": events,
        "contact_folders": contact_folders,
        "contacts": contacts,
    }))
}

/// `POST /test/fixture/step` -> apply the cursor's current step.
///
/// Body: optional JSON object. `{}` is fine. `{"expect": "step-id"}`
/// guards against an out-of-phase harness: the handler verifies that
/// the cursor's current step has that id and returns 409 on mismatch.
///
/// Atomic apply: every op in the step accumulates into one
/// `MutationDiff` routed through one `Fixture::mutate` call. The
/// fixture's emails/mailboxes/events are snapshot before applying;
/// any per-op error rewinds them so a failed step never leaves a
/// half-mutated fixture (the cursor stays put, too). On success the
/// cursor advances by one.
///
/// Response shape (200) when a step ran:
/// ```text
/// { "ok": true, "fixture": "...", "step": "<id>", "applied": 1,
///   "changes": {
///       "emails":    { "created": [], "updated": [], "destroyed": [], "moved": [] },
///       "mailboxes": { "created": [], "updated": [], "destroyed": [] },
///       "events":    { "created": [], "updated": [], "destroyed": [] }
///   },
///   "state": "<post-step JMAP state token>" }
/// ```
///
/// At end of script: 200 with `{"ok": true, "fixture": "...",
/// "step": null, "applied": false}` so the harness knows the
/// script is exhausted.
///
/// Errors:
/// - 400 + `{"error": "...", "detail": "..."}` on malformed body.
/// - 409 + `{"error": "expect mismatch", ...}` when the body's
///   `expect` does not match the cursor's current step.
/// - 422 + apply error envelope when an op fails (unknown id,
///   invalid patch shape, ...). The fixture is not mutated; the
///   cursor does not advance.
async fn step_fixture(
    State(state): State<AppState>,
    body: Option<Json<Value>>,
) -> Response {
    let body_obj: Map<String, Value> = match body {
        None => Map::new(),
        Some(Json(Value::Null)) => Map::new(),
        Some(Json(Value::Object(m))) => m,
        Some(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "malformed body",
                    "detail": "expected an object or empty body",
                })),
            )
                .into_response();
        }
    };
    let expect = match body_obj.get("expect") {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "malformed body",
                    "detail": "`expect` must be a string when present",
                })),
            )
                .into_response();
        }
    };

    let mut cursor = state.shared.change_cursor.lock().expect("cursor lock poisoned");
    let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");

    // Cursor past the script end: nothing to apply.
    if *cursor >= fix.change_script.len() {
        return Json(json!({
            "ok": true,
            "fixture": fix.name,
            "step": Value::Null,
            "applied": false,
        }))
        .into_response();
    }

    let step = fix.change_script[*cursor].clone();
    if let Some(want) = expect.as_deref()
        && want != step.id
    {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "expect mismatch",
                "detail": format!("cursor is at {:?}; got expect={want:?}", step.id),
                "cursor_step": step.id,
                "expect": want,
            })),
        )
            .into_response();
    }

    // Atomic apply: snapshot every mutable fixture section so we can
    // rewind on per-op error. The snapshot must cover every field
    // `apply_change_step` is allowed to touch - missing one leaves a
    // torn write on the failure path. Collect the per-op effect
    // lists so the response can split moves out from regular updates.
    let saved_emails = fix.emails.clone();
    let saved_mailboxes = fix.mailboxes.clone();
    let saved_events = fix.events.clone();
    let saved_contacts = fix.contacts.clone();
    let saved_contact_folders = fix.contact_folders.clone();

    let mut diff = crate::fixture::MutationDiff::default();
    let mut moved: Vec<String> = Vec::new();
    let apply_result: Result<(), (StatusCode, Value)> =
        apply_change_step(&mut fix, &step, &mut diff, &mut moved);

    if let Err((code, payload)) = apply_result {
        // Rewind. The mutation never happened; the cursor stays put.
        fix.emails = saved_emails;
        fix.mailboxes = saved_mailboxes;
        fix.events = saved_events;
        fix.contacts = saved_contacts;
        fix.contact_folders = saved_contact_folders;
        return (code, Json(payload)).into_response();
    }

    // Commit: a single mutate call records exactly one transition for
    // the entire step. The closure does no further mutation - the
    // fixture already holds the post-step image; we just need to
    // surface the diff to the change_log.
    let trans = fix.mutate(|_f| diff);

    *cursor += 1;
    let cursor_after = *cursor;
    drop(cursor);

    let new_state = fix.state.clone();
    let fixture_name = fix.name.clone();
    drop(fix);

    Json(json!({
        "ok": true,
        "fixture": fixture_name,
        "step": step.id,
        "applied": 1,
        "cursor": cursor_after,
        "changes": {
            "emails": {
                "created": trans.email_created,
                "updated": trans.email_updated,
                "destroyed": trans.email_destroyed,
                "moved": moved,
            },
            "mailboxes": {
                "created": trans.mailbox_created,
                "updated": trans.mailbox_updated,
                "destroyed": trans.mailbox_destroyed,
            },
            "events": {
                "created": trans.event_created,
                "updated": trans.event_updated,
                "destroyed": trans.event_destroyed,
            },
            "contact_folders": {
                "created": trans.contact_folder_created,
                "updated": trans.contact_folder_updated,
                "destroyed": trans.contact_folder_destroyed,
            },
            "contacts": {
                "created": trans.contact_created,
                "updated": trans.contact_updated,
                "destroyed": trans.contact_destroyed,
            },
        },
        "state": new_state,
    }))
    .into_response()
}

/// Apply a `ChangeStep` to `fix` in place. Accumulates per-resource id
/// touches into `diff` for the eventual `Fixture::mutate` commit; the
/// move-only ids land in `moved` so the step response can surface them
/// distinctly from regular `email_updated` updates.
///
/// Errors are returned as `(StatusCode, JSON envelope)` ready to reply
/// directly. The caller is expected to rewind any partial mutation
/// (this function freely mutates as it goes; it does not snapshot).
#[allow(clippy::too_many_lines)]
fn apply_change_step(
    fix: &mut crate::fixture::Fixture,
    step: &crate::fixture::ChangeStep,
    diff: &mut crate::fixture::MutationDiff,
    moved: &mut Vec<String>,
) -> Result<(), (StatusCode, Value)> {
    use crate::fixture::ChangeOp;
    for (i, op) in step.ops.iter().enumerate() {
        match op {
            ChangeOp::EmailCreate(email) => {
                let email = (**email).clone();
                // Cross-ref guard: every mailbox the email points at
                // must exist in the *current* fixture. Earlier ops in
                // this step might have created the mailboxes, so we
                // check at apply time, not at script load.
                for mid in &email.mailbox_ids {
                    if !fix.mailboxes.iter().any(|m| &m.id == mid) {
                        return Err(step_apply_error(
                            &step.id,
                            i,
                            "unknownMailbox",
                            &format!("email_create email {:?}: mailbox {mid:?} not in fixture", email.id),
                        ));
                    }
                }
                if fix.emails.iter().any(|e| e.id == email.id) {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "duplicate",
                        &format!("email_create email {:?}: id already exists", email.id),
                    ));
                }
                let id = email.id.clone();
                fix.emails.push(email);
                diff.email_created.push(id);
            }
            ChangeOp::EmailUpdate { id, patch } => {
                let idx = match fix.emails.iter().position(|e| &e.id == id) {
                    Some(i) => i,
                    None => {
                        return Err(step_apply_error(
                            &step.id,
                            i,
                            "notFound",
                            &format!("email_update {id:?}: no such email"),
                        ));
                    }
                };
                let mut clone = fix.emails[idx].clone();
                if let Err(err) = crate::jmap::apply_email_patch(&mut clone, patch) {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "invalidPatch",
                        &format!("email_update {id:?}: {err}"),
                    ));
                }
                fix.emails[idx] = clone;
                diff.email_updated.push(id.clone());
            }
            ChangeOp::EmailMove { id, mailbox_ids } => {
                for mid in mailbox_ids {
                    if !fix.mailboxes.iter().any(|m| &m.id == mid) {
                        return Err(step_apply_error(
                            &step.id,
                            i,
                            "unknownMailbox",
                            &format!("email_move {id:?}: mailbox {mid:?} not in fixture"),
                        ));
                    }
                }
                let idx = match fix.emails.iter().position(|e| &e.id == id) {
                    Some(i) => i,
                    None => {
                        return Err(step_apply_error(
                            &step.id,
                            i,
                            "notFound",
                            &format!("email_move {id:?}: no such email"),
                        ));
                    }
                };
                fix.emails[idx].mailbox_ids = mailbox_ids.clone();
                diff.email_updated.push(id.clone());
                moved.push(id.clone());
            }
            ChangeOp::EmailDestroy { id } => {
                let len_before = fix.emails.len();
                fix.emails.retain(|e| &e.id != id);
                if fix.emails.len() == len_before {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "notFound",
                        &format!("email_destroy {id:?}: no such email"),
                    ));
                }
                diff.email_destroyed.push(id.clone());
            }
            ChangeOp::MailboxCreate(mailbox) => {
                let mailbox = (**mailbox).clone();
                if fix.mailboxes.iter().any(|m| m.id == mailbox.id) {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "duplicate",
                        &format!("mailbox_create {:?}: id already exists", mailbox.id),
                    ));
                }
                if let Some(parent) = &mailbox.parent_id
                    && !fix.mailboxes.iter().any(|m| &m.id == parent)
                {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "unknownParent",
                        &format!(
                            "mailbox_create {:?}: parent {parent:?} not in fixture",
                            mailbox.id
                        ),
                    ));
                }
                let id = mailbox.id.clone();
                fix.mailboxes.push(mailbox);
                diff.mailbox_created.push(id);
            }
            ChangeOp::MailboxUpdate { id, patch } => {
                let idx = match fix.mailboxes.iter().position(|m| &m.id == id) {
                    Some(i) => i,
                    None => {
                        return Err(step_apply_error(
                            &step.id,
                            i,
                            "notFound",
                            &format!("mailbox_update {id:?}: no such mailbox"),
                        ));
                    }
                };
                let mut clone = fix.mailboxes[idx].clone();
                if let Err(err) = crate::jmap::apply_mailbox_patch(&mut clone, patch) {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "invalidPatch",
                        &format!("mailbox_update {id:?}: {err}"),
                    ));
                }
                fix.mailboxes[idx] = clone;
                diff.mailbox_updated.push(id.clone());
            }
            ChangeOp::MailboxDestroy { id } => {
                let still_referenced = fix
                    .emails
                    .iter()
                    .any(|e| e.mailbox_ids.iter().any(|m| m == id));
                if still_referenced {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "mailboxHasEmail",
                        &format!("mailbox_destroy {id:?}: mailbox is referenced by an email"),
                    ));
                }
                let len_before = fix.mailboxes.len();
                fix.mailboxes.retain(|m| &m.id != id);
                if fix.mailboxes.len() == len_before {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "notFound",
                        &format!("mailbox_destroy {id:?}: no such mailbox"),
                    ));
                }
                diff.mailbox_destroyed.push(id.clone());
            }
            ChangeOp::EventCreate(event) => {
                let event = (**event).clone();
                if !fix.calendars.iter().any(|c| c.id == event.calendar_id) {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "unknownCalendar",
                        &format!(
                            "event_create {:?}: calendar {:?} not in fixture",
                            event.id, event.calendar_id
                        ),
                    ));
                }
                if fix.events.iter().any(|e| e.id == event.id) {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "duplicate",
                        &format!("event_create {:?}: id already exists", event.id),
                    ));
                }
                let id = event.id.clone();
                fix.events.push(event);
                diff.event_created.push(id);
            }
            ChangeOp::EventUpdate { id, patch } => {
                let idx = match fix.events.iter().position(|e| &e.id == id) {
                    Some(i) => i,
                    None => {
                        return Err(step_apply_error(
                            &step.id,
                            i,
                            "notFound",
                            &format!("event_update {id:?}: no such event"),
                        ));
                    }
                };
                if let Err(err) = apply_change_event_patch(&mut fix.events[idx], patch) {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "invalidPatch",
                        &format!("event_update {id:?}: {err}"),
                    ));
                }
                diff.event_updated.push(id.clone());
            }
            ChangeOp::EventDestroy { id } => {
                // Snapshot the parent calendar BEFORE the retain
                // (the event is gone afterwards) so the tombstone
                // carries the right calendar_id for delta filtering.
                let parent = fix
                    .events
                    .iter()
                    .find(|e| &e.id == id)
                    .map(|e| e.calendar_id.clone());
                let len_before = fix.events.len();
                fix.events.retain(|e| &e.id != id);
                if fix.events.len() == len_before {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "notFound",
                        &format!("event_destroy {id:?}: no such event"),
                    ));
                }
                diff.event_destroyed.push(id.clone());
                diff.event_destroyed_parents
                    .push(parent.expect("event existed before retain"));
            }
            ChangeOp::ContactFolderCreate(folder) => {
                let folder = (**folder).clone();
                if fix.contact_folders.iter().any(|f| f.id == folder.id) {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "duplicate",
                        &format!("contact_folder_create {:?}: id already exists", folder.id),
                    ));
                }
                if let Some(parent) = &folder.parent_folder_id
                    && !fix.contact_folders.iter().any(|f| &f.id == parent)
                {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "unknownParent",
                        &format!(
                            "contact_folder_create {:?}: parent {parent:?} not in fixture",
                            folder.id
                        ),
                    ));
                }
                let id = folder.id.clone();
                fix.contact_folders.push(folder);
                diff.contact_folder_created.push(id);
            }
            ChangeOp::ContactFolderUpdate { id, patch } => {
                let idx = match fix.contact_folders.iter().position(|f| &f.id == id) {
                    Some(i) => i,
                    None => {
                        return Err(step_apply_error(
                            &step.id,
                            i,
                            "notFound",
                            &format!("contact_folder_update {id:?}: no such folder"),
                        ));
                    }
                };
                let mut clone = fix.contact_folders[idx].clone();
                if let Err(err) = apply_contact_folder_patch(&mut clone, patch) {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "invalidPatch",
                        &format!("contact_folder_update {id:?}: {err}"),
                    ));
                }
                fix.contact_folders[idx] = clone;
                diff.contact_folder_updated.push(id.clone());
            }
            ChangeOp::ContactFolderDestroy { id } => {
                let still_referenced = fix.contacts.iter().any(|c| &c.folder_id == id);
                if still_referenced {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "folderHasContacts",
                        &format!(
                            "contact_folder_destroy {id:?}: folder still has contacts"
                        ),
                    ));
                }
                let len_before = fix.contact_folders.len();
                fix.contact_folders.retain(|f| &f.id != id);
                if fix.contact_folders.len() == len_before {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "notFound",
                        &format!("contact_folder_destroy {id:?}: no such folder"),
                    ));
                }
                diff.contact_folder_destroyed.push(id.clone());
            }
            ChangeOp::ContactCreate(contact) => {
                let contact = (**contact).clone();
                if !fix
                    .contact_folders
                    .iter()
                    .any(|f| f.id == contact.folder_id)
                {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "unknownFolder",
                        &format!(
                            "contact_create {:?}: folder {:?} not in fixture",
                            contact.id, contact.folder_id
                        ),
                    ));
                }
                if fix.contacts.iter().any(|c| c.id == contact.id) {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "duplicate",
                        &format!("contact_create {:?}: id already exists", contact.id),
                    ));
                }
                let id = contact.id.clone();
                fix.contacts.push(contact);
                diff.contact_created.push(id);
            }
            ChangeOp::ContactUpdate { id, patch } => {
                let idx = match fix.contacts.iter().position(|c| &c.id == id) {
                    Some(i) => i,
                    None => {
                        return Err(step_apply_error(
                            &step.id,
                            i,
                            "notFound",
                            &format!("contact_update {id:?}: no such contact"),
                        ));
                    }
                };
                let mut clone = fix.contacts[idx].clone();
                if let Err(err) = apply_contact_patch(&mut clone, patch, &fix.contact_folders) {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "invalidPatch",
                        &format!("contact_update {id:?}: {err}"),
                    ));
                }
                fix.contacts[idx] = clone;
                diff.contact_updated.push(id.clone());
            }
            ChangeOp::ContactDestroy { id } => {
                // Snapshot the parent folder BEFORE retain so the
                // tombstone the change_log records carries the right
                // folder_id for per-folder contacts/delta filtering.
                let parent = fix
                    .contacts
                    .iter()
                    .find(|c| &c.id == id)
                    .map(|c| c.folder_id.clone());
                let len_before = fix.contacts.len();
                fix.contacts.retain(|c| &c.id != id);
                if fix.contacts.len() == len_before {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "notFound",
                        &format!("contact_destroy {id:?}: no such contact"),
                    ));
                }
                diff.contact_destroyed.push(id.clone());
                diff.contact_destroyed_parents
                    .push(parent.expect("contact existed before retain"));
            }
        }
    }
    Ok(())
}

fn apply_contact_folder_patch(
    folder: &mut crate::fixture::ContactFolder,
    patch: &Value,
) -> Result<(), String> {
    let obj = patch
        .as_object()
        .ok_or_else(|| "patch must be an object".to_string())?;
    for (k, v) in obj {
        match k.as_str() {
            "display_name" => {
                folder.display_name = v
                    .as_str()
                    .ok_or_else(|| "display_name must be a string".to_string())?
                    .to_string();
            }
            "parent_folder_id" => {
                folder.parent_folder_id = match v {
                    Value::Null => None,
                    Value::String(s) => Some(s.clone()),
                    _ => return Err("parent_folder_id must be a string or null".to_string()),
                };
            }
            other => return Err(format!("unknown patch field {other:?}")),
        }
    }
    Ok(())
}

fn apply_contact_patch(
    contact: &mut crate::fixture::Contact,
    patch: &Value,
    folders: &[crate::fixture::ContactFolder],
) -> Result<(), String> {
    let obj = patch
        .as_object()
        .ok_or_else(|| "patch must be an object".to_string())?;
    for (k, v) in obj {
        match k.as_str() {
            "display_name" => {
                contact.display_name = match v {
                    Value::Null => None,
                    Value::String(s) => Some(s.clone()),
                    _ => return Err("display_name must be a string or null".to_string()),
                };
            }
            "folder_id" => {
                let id = v
                    .as_str()
                    .ok_or_else(|| "folder_id must be a string".to_string())?;
                if !folders.iter().any(|f| f.id == id) {
                    return Err(format!("folder_id {id:?} not in fixture"));
                }
                contact.folder_id = id.to_string();
            }
            "emails" => {
                let arr = v
                    .as_array()
                    .ok_or_else(|| "emails must be an array".to_string())?;
                let mut out = Vec::with_capacity(arr.len());
                for e in arr {
                    let address = e
                        .get("address")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "emails entry missing address".to_string())?
                        .to_string();
                    let name = e
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    out.push(crate::fixture::ContactEmail { address, name });
                }
                contact.emails = out;
            }
            other => return Err(format!("unknown patch field {other:?}")),
        }
    }
    Ok(())
}

/// Plain-shape event patch used by change-script `event_update` ops.
/// Mirrors the keys the Lua builder emits (`subject`, `start`, `end`,
/// `location`, `body_text`); RFC3339 strings only. Distinct from
/// `graph::calendar::apply_event_patch` which decodes Graph's nested
/// `start.dateTime` shape.
fn apply_change_event_patch(
    event: &mut crate::fixture::Event,
    patch: &Value,
) -> Result<(), String> {
    let obj = patch
        .as_object()
        .ok_or_else(|| "patch must be an object".to_string())?;
    for (k, v) in obj {
        match k.as_str() {
            "subject" => {
                event.subject = v
                    .as_str()
                    .ok_or_else(|| "subject must be a string".to_string())?
                    .to_string();
            }
            "start" => {
                let s = v
                    .as_str()
                    .ok_or_else(|| "start must be an RFC3339 string".to_string())?;
                event.start = crate::fixture::parse_ts(s)?;
            }
            "end" => {
                let s = v
                    .as_str()
                    .ok_or_else(|| "end must be an RFC3339 string".to_string())?;
                event.end = crate::fixture::parse_ts(s)?;
            }
            "location" => {
                event.location = match v {
                    Value::Null => None,
                    Value::String(s) => Some(s.clone()),
                    _ => return Err("location must be a string or null".to_string()),
                };
            }
            "body_text" => {
                event.body_text = match v {
                    Value::Null => None,
                    Value::String(s) => Some(s.clone()),
                    _ => return Err("body_text must be a string or null".to_string()),
                };
            }
            other => return Err(format!("unknown patch field {other:?}")),
        }
    }
    Ok(())
}

fn step_apply_error(step_id: &str, op_index: usize, kind: &str, detail: &str) -> (StatusCode, Value) {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        json!({
            "error": "step apply failed",
            "step": step_id,
            "op_index": op_index,
            "kind": kind,
            "detail": detail,
        }),
    )
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
