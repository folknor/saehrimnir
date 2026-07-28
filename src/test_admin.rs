//! `/test/*` admin / instrumentation routes.
//!
//! Split out of `src/routes.rs` so the JMAP HTTP / OAuth router glue
//! stays scrollable. Every route here is a test-only entry point
//! exposed by the same listener as the JMAP API; `main.rs` only
//! binds the listener on `127.0.0.1`, so these are not reachable
//! off-loopback. See the `SECURITY TODO` block on
//! `crate::routes::router` for the full reachability rationale.
//!
//! Routes:
//! - `GET /test/smtp/submissions`, `DELETE /test/smtp/submissions`
//!   - `list_smtp_submissions` / `clear_smtp_submissions`
//! - `GET /test/requests`, `DELETE /test/requests`
//!   - `list_requests` (with `?stable=true`) / `clear_requests`
//! - `POST /test/fixture/reset` -> `reset_fixture`
//! - `POST /test/fixture/step` -> `step_fixture` (drives
//!   `apply_change_step` against the fixture's `change_script`)
//! - `GET /test/snapshot-state` -> `snapshot_state`
//! - `GET /test/push/jmap`, `GET /test/push/graph`,
//!   `GET /test/push/graph/excluded`, `GET /test/push/subscriptions`
//! - `GET /test/latency`, `POST /test/latency` -> `get_latency` /
//!   `set_latency`

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::routes::AppState;
use crate::smtp::Submission;

/// `/test/*` route family. Merged into `crate::routes::router`.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route(
            "/test/smtp/submissions",
            get(list_smtp_submissions).delete(clear_smtp_submissions),
        )
        .route("/test/requests", get(list_requests).delete(clear_requests))
        .route("/test/fixture/reset", post(reset_fixture))
        .route("/test/fixture/step", post(step_fixture))
        .route("/test/snapshot-state", get(snapshot_state))
        .route("/test/latency", get(get_latency).post(set_latency))
        .route(
            "/test/jmap/fail-open",
            post(arm_fail_open).delete(clear_fail_open),
        )
        .route("/test/push/jmap", get(list_jmap_push))
        .route("/test/push/graph", get(list_graph_push))
        .route("/test/push/graph/excluded", get(list_graph_excluded))
        .route("/test/push/subscriptions", get(list_push_subscriptions))
        .route(
            "/test/gmail/pubsub/messages",
            get(list_pubsub).delete(clear_pubsub),
        )
        .route("/test/gmail/pubsub/publish", post(publish_pubsub))
        .route(
            "/test/gmail/pubsub/push-endpoint",
            post(set_pubsub_endpoint),
        )
        .with_state(state)
}

// ── Test-only push observability + Gmail Pub/Sub control ────────────

/// `GET /test/push/jmap` -> JSON array of every JMAP `StateChange`
/// frame the push hub has emitted (oldest first). Lets a harness
/// assert "the WebSocket push fired" without holding a live socket.
async fn list_jmap_push(State(state): State<AppState>) -> Json<Value> {
    Json(json!(state.shared.push.jmap_log()))
}

/// `GET /test/push/graph` -> JSON array of every Graph change-
/// notification the push hub has emitted. Each entry is
/// `{ "notification_url": <url>, "body": <Graph notification> }`.
async fn list_graph_push(State(state): State<AppState>) -> Json<Value> {
    Json(json!(state.shared.push.graph_log()))
}

/// `GET /test/push/graph/excluded` -> JSON array of every subscription
/// a state advance deliberately skipped. Each entry is
/// `{ "subscription_id", "account_id", "resource", "namespace",
/// "reason" }`. Complements `/test/push/graph`: that route shows what
/// fired, this one shows what was held back and why, so a consumer can
/// prove a public-folder subscription was excluded from the fan-out
/// rather than merely absent.
async fn list_graph_excluded(State(state): State<AppState>) -> Json<Value> {
    Json(json!(state.shared.push.graph_excluded()))
}

/// `GET /test/push/subscriptions` -> JSON array of every registered
/// Graph subscription with its resolved namespace
/// (`personal` / `shared` / `public`) and whether a state advance fans
/// out to it. Ordered by subscription id.
async fn list_push_subscriptions(State(state): State<AppState>) -> Json<Value> {
    Json(json!(state.shared.push.graph_subscriptions()))
}

/// `GET /test/gmail/pubsub/messages` -> JSON array of every Pub/Sub
/// push envelope published onto the mock Pub/Sub source.
async fn list_pubsub(State(state): State<AppState>) -> Json<Value> {
    Json(json!(state.shared.push.pubsub_messages()))
}

/// `DELETE /test/gmail/pubsub/messages` -> clear the Pub/Sub sink.
async fn clear_pubsub(State(state): State<AppState>) -> StatusCode {
    state.shared.push.pubsub_clear();
    StatusCode::NO_CONTENT
}

/// `POST /test/gmail/pubsub/publish` -> publish a Gmail notification
/// envelope onto the mock Pub/Sub source. Body:
/// ```text
/// { "emailAddress": "a@example.com",   // required
///   "historyId": 42,                   // required (number)
///   "account_id": "account-1" }        // optional; routes the
///                                       // delivery to that account's
///                                       // registered push endpoint
/// ```
/// Returns the published Pub/Sub envelope. The state-mutation trigger
/// publishes automatically for any account with an active `users.watch`;
/// this route lets a harness stage a notification directly.
async fn publish_pubsub(State(state): State<AppState>, body: Option<Json<Value>>) -> Response {
    let body = match body {
        Some(Json(Value::Object(m))) => m,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "malformed body", "detail": "expected a JSON object" })),
            )
                .into_response();
        }
    };
    let Some(email) = body.get("emailAddress").and_then(|v| v.as_str()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "malformed body", "detail": "emailAddress (string) required" })),
        )
            .into_response();
    };
    let Some(history_id) = body.get("historyId").and_then(serde_json::Value::as_u64) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "malformed body", "detail": "historyId (number) required" })),
        )
            .into_response();
    };
    let account_id = body
        .get("account_id")
        .and_then(|v| v.as_str())
        .unwrap_or(email);
    let env = state
        .shared
        .push
        .publish_gmail(email, history_id, account_id);
    (StatusCode::OK, Json(env)).into_response()
}

/// `POST /test/gmail/pubsub/push-endpoint` -> register the loopback
/// URL a Pub/Sub push for an account is delivered to (mirrors a real
/// Pub/Sub subscription's push-endpoint config). Body:
/// `{ "account_id": "account-1", "url": "http://127.0.0.1:PORT/push" }`.
async fn set_pubsub_endpoint(State(state): State<AppState>, body: Option<Json<Value>>) -> Response {
    let body = match body {
        Some(Json(Value::Object(m))) => m,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "malformed body", "detail": "expected a JSON object" })),
            )
                .into_response();
        }
    };
    let (Some(account_id), Some(url)) = (
        body.get("account_id").and_then(|v| v.as_str()),
        body.get("url").and_then(|v| v.as_str()),
    ) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "malformed body",
                "detail": "account_id (string) and url (string) required",
            })),
        )
            .into_response();
    };
    state
        .shared
        .push
        .gmail_set_push_endpoint(account_id.to_string(), url.to_string());
    StatusCode::NO_CONTENT.into_response()
}

// ── Test-only SMTP submission log ───────────────────────────────────

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
    pub account_id: String,
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
    /// Every top-level RFC 822 header on the submitted message as
    /// `{ name, value }` objects, in document order. Lets a harness
    /// gate assert the outgoing `Disposition-Notification-To` (or any
    /// other header) bifrost emits without the projection enumerating
    /// a fixed allowlist.
    pub headers: Vec<HeaderJson>,
}

#[derive(Debug, Serialize)]
pub struct HeaderJson {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Serialize)]
pub struct AttachmentJson {
    pub filename: Option<String>,
    pub content_type: String,
    pub size: usize,
    /// The attachment part's `Content-ID` (angle brackets stripped),
    /// when present. An inline-attachment gate asserts this is set.
    pub content_id: Option<String>,
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
                    content_id: a.content_id,
                })
                .collect(),
            headers: p
                .headers
                .into_iter()
                .map(|(name, value)| HeaderJson { name, value })
                .collect(),
        });
        SubmissionJson {
            from: s.from.clone(),
            recipients: s.recipients.clone(),
            from_params: s.from_params.clone(),
            rcpt_params: s.rcpt_params.clone(),
            auth_mechanism: s.auth_mechanism.clone(),
            account_id: s.account_id.clone(),
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

/// `GET /test/requests` -> JSON array of every protocol-level
/// dispatch event the binary has handled. With `?stable=true` the
/// `received_at` wall-clock timestamp is stripped and `connection_id`
/// is rewritten to a 0-based first-seen index (raw ids are monotonic
/// from process start, so absolute values aren't byte-stable across
/// restarts; the normalized projection still distinguishes sessions).
/// Entries that recorded no connection (`None`) stay null.
async fn list_requests(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let snap = state.shared.request_log.snapshot();
    if params.get("stable").map(String::as_str) == Some("true") {
        // Walk once to build the raw-id -> dense-index map, then
        // emit the view rows. O(n) per call; the deque is bounded
        // at `REQUEST_LOG_CAP` so this is bounded too.
        let mut dense: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
        for entry in &snap {
            if let Some(raw) = entry.connection_id {
                let next = dense.len() as u64;
                dense.entry(raw).or_insert(next);
            }
        }
        // Borrow-and-serialize via a view struct: serde walks each
        // `detail` once at write time, no per-row JSON tree rebuild.
        #[derive(serde::Serialize)]
        struct Stable<'a> {
            protocol: &'a str,
            command: &'a str,
            detail: &'a Value,
            #[serde(skip_serializing_if = "Option::is_none")]
            connection_id: Option<u64>,
        }
        let stripped: Vec<Stable<'_>> = snap
            .iter()
            .map(|e| Stable {
                protocol: e.protocol,
                command: &e.command,
                detail: &e.detail,
                connection_id: e.connection_id.and_then(|r| dense.get(&r).copied()),
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
    state.shared.push.clear();
    state.shared.open_fault.clear();
    if let Some(d) = &state.shared.dispatcher {
        d.reset_counts();
    }
    // Lock-ordering convention: when both the cursor mutex and the
    // fixture rwlock are needed, acquire the cursor first. Matches
    // `step_fixture`'s order so a future change that holds both
    // simultaneously can't deadlock against either.
    let mut cursor = state
        .shared
        .change_cursor
        .lock()
        .expect("cursor lock poisoned");
    {
        let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
        *fix = (*state.shared.baseline).clone();
    }
    *cursor = 0;
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
async fn set_latency(State(state): State<AppState>, body: Option<Json<Value>>) -> Response {
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

// ── Test-only forced session-open failure ───────────────────────────

/// `POST /test/jmap/fail-open` -> arm the forced JMAP session-open
/// failure knob (`crate::open_fault::OpenFault`). Body (all fields
/// optional):
/// ```text
/// { "count": 3,           // fail the next N consecutive session
///                          //   opens then auto-clear (default 3,
///                          //   which matches bifrost's reopen budget)
///   "sticky": false,      // fail every open until cleared; overrides
///                          //   `count` when true
///   "status": 503,        // HTTP status returned on a forced failure
///   "message": "..." }    // error detail surfaced in the body
/// ```
/// An empty body / `{}` arms `count = 3` with the default 503 status.
/// `count = 0` (and not sticky) clears the knob. Returns 200 + a
/// snapshot of the remaining budget (`null` = disarmed, `"sticky"`, or
/// the integer count) for round-trip verification.
///
/// This is the connection/auth-establishment analogue of token
/// revocation (`POST /test/oauth/invalidate`): a stateful fault on the
/// `/test/*` control plane a harness drives at runtime, then clears via
/// `DELETE /test/jmap/fail-open` so a subsequent open succeeds and
/// resume can be exercised.
async fn arm_fail_open(State(state): State<AppState>, body: Option<Json<Value>>) -> Response {
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

    let sticky = body_obj
        .get("sticky")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // `count` only consulted when not sticky. Default 3 = bifrost's
    // reopen budget, so an empty-body arm exhausts it exactly.
    let count = if sticky {
        None
    } else {
        match body_obj.get("count") {
            None => Some(3),
            Some(v) => match v.as_u64() {
                Some(n) => Some(n),
                None => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "error": "malformed body",
                            "detail": "count must be a non-negative integer",
                        })),
                    )
                        .into_response();
                }
            },
        }
    };

    let status = match body_obj.get("status") {
        None => crate::open_fault::DEFAULT_FAIL_STATUS,
        Some(v) => match v.as_u64().and_then(|n| u16::try_from(n).ok()) {
            // Constrain to real HTTP error codes; a 2xx here would
            // make the "failure" succeed and silently break the test.
            Some(n) if (400..=599).contains(&n) => n,
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": "malformed body",
                        "detail": "status must be an integer in 400..=599",
                    })),
                )
                    .into_response();
            }
        },
    };

    let message = body_obj
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or(crate::open_fault::DEFAULT_FAIL_MESSAGE)
        .to_string();

    state.shared.open_fault.arm(count, status, message);

    let remaining = match state.shared.open_fault.snapshot() {
        None => Value::Null,
        Some(None) => json!("sticky"),
        Some(Some(n)) => json!(n),
    };
    Json(json!({ "remaining": remaining })).into_response()
}

/// `DELETE /test/jmap/fail-open` -> 204. Disarm the forced session-open
/// failure so the next open succeeds. Idempotent.
async fn clear_fail_open(State(state): State<AppState>) -> StatusCode {
    state.shared.open_fault.clear();
    StatusCode::NO_CONTENT
}

/// `GET /test/snapshot-state` -> 200 + JSON dump of the fixture's
/// current mutable state across every resource family. Used by
/// harness scripts that want to verify post-step state without
/// re-walking every protocol.
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
    // Shared-folder grants. Mutable since the change-script gained
    // acl_grant / acl_revoke, so a harness can read the current
    // sharing state without going through IMAP GETACL.
    let acls: Vec<Value> = fix
        .acls
        .iter()
        .map(|a| {
            json!({
                "mailbox_id": a.mailbox_id,
                "identifier": a.identifier,
                "rights": a.rights,
            })
        })
        .collect();
    // Public folders are org-wide (no account scope) and immutable in
    // v0; projected here so a fixture staging personal + shared +
    // public resources is fully readable from one admin call.
    let public_folders: Vec<Value> = fix
        .public_folders
        .iter()
        .map(|f| {
            json!({
                "id": f.id,
                "display_name": f.display_name,
                "parent_id": f.parent_id,
                "folder_class": f.folder_class,
            })
        })
        .collect();
    Json(json!({
        "name": fix.name,
        "state": fix.primary_state(),
        "mailboxes": mailboxes,
        "emails": emails,
        "events": events,
        "contact_folders": contact_folders,
        "contacts": contacts,
        "acls": acls,
        "public_folders": public_folders,
    }))
}

/// `POST /test/fixture/step` -> apply the cursor's current step.
///
/// Body: optional JSON object. `{}` is fine. `{"expect": "step-id"}`
/// guards against an out-of-phase harness: the handler verifies that
/// the cursor's current step has that id and returns 409 on mismatch.
///
/// Atomic apply: every op in the step accumulates into one
/// `MutationDiff` routed through one `Fixture::record_transition`
/// call. The fixture sections the step touches are snapshot before
/// applying; any per-op error rewinds them so a failed step never
/// leaves a half-mutated fixture (the cursor stays put, too). On
/// success the cursor advances by one.
async fn step_fixture(State(state): State<AppState>, body: Option<Json<Value>>) -> Response {
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

    let mut cursor = state
        .shared
        .change_cursor
        .lock()
        .expect("cursor lock poisoned");
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

    // Atomic apply: snapshot every mutable fixture section the
    // step might touch so we can rewind on per-op error. Pre-fix
    // every step deep-cloned every section unconditionally;
    // against a 10k-email fixture, a step that only touches
    // contacts paid for cloning all 10k emails. Now we walk the
    // ops once, classify which categories they touch, and only
    // clone those vecs. mailbox_uid_history is touched by every
    // email-shaped op (assign_uid / retire_uid); snapshot it
    // whenever any email op exists.
    let touched = StepTouches::from_step(&step);
    let saved_emails = touched.emails.then(|| fix.emails.clone());
    let saved_mailboxes = touched.mailboxes.then(|| fix.mailboxes.clone());
    let saved_events = touched.events.then(|| fix.events.clone());
    let saved_contacts = touched.contacts.then(|| fix.contacts.clone());
    let saved_contact_folders = touched.contact_folders.then(|| fix.contact_folders.clone());
    let saved_mailbox_uid_history = touched.emails.then(|| fix.mailbox_uid_history.clone());
    let saved_acls = touched.acls.then(|| fix.acls.clone());
    let saved_groups = touched.groups.then(|| fix.groups.clone());

    let mut diff = crate::fixture::MutationDiff::default();
    let mut moved: Vec<String> = Vec::new();
    let apply_result: Result<(), (StatusCode, Value)> =
        apply_change_step(&mut fix, &step, &mut diff, &mut moved);

    if let Err((code, payload)) = apply_result {
        // Rewind. The mutation never happened; the cursor stays put.
        if let Some(v) = saved_emails {
            fix.emails = v;
        }
        if let Some(v) = saved_mailboxes {
            fix.mailboxes = v;
        }
        if let Some(v) = saved_events {
            fix.events = v;
        }
        if let Some(v) = saved_contacts {
            fix.contacts = v;
        }
        if let Some(v) = saved_contact_folders {
            fix.contact_folders = v;
        }
        if let Some(v) = saved_mailbox_uid_history {
            fix.mailbox_uid_history = v;
        }
        if let Some(v) = saved_acls {
            fix.acls = v;
        }
        if let Some(v) = saved_groups {
            fix.groups = v;
        }
        return (code, Json(payload)).into_response();
    }

    // Snapshot which accounts this diff touches before `diff` is
    // consumed by `record_transition`, so the push trigger below can
    // fire per touched account (a mutation on account B must never
    // push account A). `accounts_touched` resolves ownership the same
    // way `record_transition` advances per-account logs.
    let diff_for_push = diff.clone();
    let trans = fix.record_transition(diff);

    *cursor += 1;
    let cursor_after = *cursor;
    drop(cursor);

    let new_state = fix.primary_state().to_string();
    let fixture_name = fix.name.clone();

    // Build the per-account push snapshots while we still hold the
    // fixture guard (the push hub never re-locks the fixture). The
    // Graph `changeType` and `resourceData` id derive from the
    // aggregate transition: dominant kind (created > deleted >
    // updated) and the first touched message id.
    let push_change_type = if !trans.email_created.is_empty() {
        "created"
    } else if !trans.email_destroyed.is_empty() {
        "deleted"
    } else {
        "updated"
    }
    .to_string();
    let push_resource_id = trans
        .email_created
        .first()
        .or_else(|| trans.email_updated.first())
        .or_else(|| trans.email_destroyed.first())
        .cloned();
    let pushes: Vec<crate::push::AccountPush> = fix
        .accounts_touched(&diff_for_push)
        .into_iter()
        .map(|acct_id| {
            let email_address = fix
                .account(&acct_id)
                .map(|a| a.name.clone())
                .unwrap_or_default();
            // Bind every borrow of `acct_id` to a local so its temporary
            // ends here; otherwise the borrows live to the end of the
            // closure block and collide with the `account_id: acct_id`
            // move below (E0505).
            let state = fix.state_for(&acct_id).to_string();
            let history_id = fix.gmail_history_id(&acct_id);
            let has_calendars = fix.calendars_for(&acct_id).next().is_some();
            let has_contacts = fix.contact_folders_for(&acct_id).next().is_some();
            crate::push::AccountPush {
                state,
                history_id,
                has_calendars,
                has_contacts,
                change_type: push_change_type.clone(),
                resource_id: push_resource_id.clone(),
                email_address,
                account_id: acct_id,
            }
        })
        .collect();
    drop(fix);

    // Fire the push surfaces (JMAP StateChange / Gmail Pub/Sub / Graph
    // webhook) for the touched accounts. No-op for accounts with no
    // registered subscriber. See `crate::push`.
    state.shared.push.emit_state_advance(&pushes);

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
            "acls": {
                "granted": acl_refs(&trans.acl_granted),
                "revoked": acl_refs(&trans.acl_revoked),
            },
        },
        "state": new_state,
    }))
    .into_response()
}

/// Render the ACL side of a step's transition for the step response:
/// `[{ "mailbox_id": ..., "identifier": ..., "owner_account_id": ... }]`.
/// The owner is included because a consumer driving the other-users
/// namespace needs to know whose namespace segment moved without
/// re-reading the mailbox table.
fn acl_refs(refs: &[crate::fixture::AclRef]) -> Vec<Value> {
    refs.iter()
        .map(|a| {
            json!({
                "mailbox_id": a.mailbox_id,
                "identifier": a.identifier,
                "owner_account_id": a.owner_account_id,
            })
        })
        .collect()
}

/// Which fixture sections a step's ops will touch. Lets
/// `step_fixture` snapshot only the relevant sections for
/// rewind-on-error rather than deep-cloning every mutable vec.
#[derive(Default)]
struct StepTouches {
    emails: bool,
    mailboxes: bool,
    events: bool,
    contacts: bool,
    contact_folders: bool,
    acls: bool,
    groups: bool,
}

impl StepTouches {
    fn from_step(step: &crate::fixture::ChangeStep) -> Self {
        use crate::fixture::ChangeOp;
        let mut t = Self::default();
        for op in &step.ops {
            match op {
                ChangeOp::EmailCreate(_)
                | ChangeOp::EmailUpdate { .. }
                | ChangeOp::EmailReaction { .. }
                | ChangeOp::EmailMove { .. }
                | ChangeOp::EmailDestroy { .. } => t.emails = true,
                ChangeOp::MailboxCreate(_)
                | ChangeOp::MailboxUpdate { .. }
                | ChangeOp::MailboxDestroy { .. } => t.mailboxes = true,
                ChangeOp::EventCreate(_)
                | ChangeOp::EventUpdate { .. }
                | ChangeOp::EventDestroy { .. } => t.events = true,
                ChangeOp::ContactCreate(_)
                | ChangeOp::ContactUpdate { .. }
                | ChangeOp::ContactDestroy { .. } => t.contacts = true,
                ChangeOp::ContactFolderCreate(_)
                | ChangeOp::ContactFolderUpdate { .. }
                | ChangeOp::ContactFolderDestroy { .. } => t.contact_folders = true,
                ChangeOp::AclGrant { .. } | ChangeOp::AclRevoke { .. } => t.acls = true,
                ChangeOp::GroupCreate(_)
                | ChangeOp::GroupUpdate { .. }
                | ChangeOp::GroupDestroy { .. } => t.groups = true,
            }
        }
        t
    }
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
                let mut email = (**email).clone();
                // Cross-ref guard: every mailbox the email points at
                // must exist in the *current* fixture. Earlier ops in
                // this step might have created the mailboxes, so we
                // check at apply time, not at script load.
                let mut first_mb_account: Option<String> = None;
                for mid in &email.mailbox_ids {
                    let Some(mb) = fix.mailboxes.iter().find(|m| &m.id == mid) else {
                        return Err(step_apply_error(
                            &step.id,
                            i,
                            "unknownMailbox",
                            &format!(
                                "email_create email {:?}: mailbox {mid:?} not in fixture",
                                email.id
                            ),
                        ));
                    };
                    match &first_mb_account {
                        None => first_mb_account = Some(mb.account_id.clone()),
                        Some(prev) if prev != &mb.account_id => {
                            return Err(step_apply_error(
                                &step.id,
                                i,
                                "accountMismatch",
                                &format!(
                                    "email_create email {:?}: mailbox {mid:?} lives in account {:?} but a sibling mailbox lives in {:?}",
                                    email.id, mb.account_id, prev
                                ),
                            ));
                        }
                        _ => {}
                    }
                }
                // Inherit account from the mailbox set (Stage 2 of
                // the loader does the same for static fixtures).
                let derived_account =
                    first_mb_account.unwrap_or_else(|| fix.primary_account().id.clone());
                if email.account_id.is_empty() {
                    email.account_id = derived_account;
                } else if email.account_id != derived_account {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "accountMismatch",
                        &format!(
                            "email_create email {:?}: declared account {:?} disagrees with mailbox-derived {:?}",
                            email.id, email.account_id, derived_account
                        ),
                    ));
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
                let mailboxes = email.mailbox_ids.clone();
                fix.emails.push(email);
                for mb in &mailboxes {
                    fix.assign_uid(mb, id.clone());
                }
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
                let old_mailboxes = fix.emails[idx].mailbox_ids.clone();
                let old_labels = fix.gmail_label_ids(&fix.emails[idx]);
                let mut clone = fix.emails[idx].clone();
                if let Err(err) = crate::jmap::apply_email_patch(&mut clone, patch) {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "invalidPatch",
                        &format!("email_update {id:?}: {err}"),
                    ));
                }
                let new_mailboxes = clone.mailbox_ids.clone();
                let new_labels = fix.gmail_label_ids(&clone);
                fix.emails[idx] = clone;
                fix.sync_mailbox_uids(id, &old_mailboxes, &new_mailboxes);
                diff.email_updated.push(id.clone());
                if let Some(c) = crate::fixture::gmail_label_change(id, &old_labels, &new_labels) {
                    diff.email_label_changes.push(c);
                }
            }
            ChangeOp::EmailReaction {
                id,
                reaction_type,
                reaction_count,
                clear,
            } => {
                let Some(idx) = fix.emails.iter().position(|e| &e.id == id) else {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "notFound",
                        &format!("email_reaction {id:?}: no such email"),
                    ));
                };
                if *clear {
                    fix.emails[idx].reaction_type = None;
                    fix.emails[idx].reaction_count = None;
                } else {
                    if let Some(t) = reaction_type {
                        fix.emails[idx].reaction_type = Some(t.clone());
                    }
                    if let Some(c) = reaction_count {
                        fix.emails[idx].reaction_count = Some(*c);
                    }
                }
                // Mailbox membership and keywords are untouched, so no
                // UID sync and no Gmail label change - just the update
                // marker, which is what makes the next Graph delta /
                // JMAP Email/changes cycle re-hydrate the message.
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
                let old_mailboxes = fix.emails[idx].mailbox_ids.clone();
                let old_labels = fix.gmail_label_ids(&fix.emails[idx]);
                fix.emails[idx].mailbox_ids = mailbox_ids.clone();
                let new_labels = fix.gmail_label_ids(&fix.emails[idx]);
                fix.sync_mailbox_uids(id, &old_mailboxes, mailbox_ids);
                diff.email_updated.push(id.clone());
                moved.push(id.clone());
                if let Some(c) = crate::fixture::gmail_label_change(id, &old_labels, &new_labels) {
                    diff.email_label_changes.push(c);
                }
            }
            ChangeOp::EmailDestroy { id } => {
                let Some(idx) = fix.emails.iter().position(|e| &e.id == id) else {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "notFound",
                        &format!("email_destroy {id:?}: no such email"),
                    ));
                };
                let mailboxes = fix.emails[idx].mailbox_ids.clone();
                let account_id = fix.emails[idx].account_id.clone();
                fix.emails.remove(idx);
                for mb in &mailboxes {
                    fix.retire_uid(mb, id);
                }
                diff.email_destroyed.push(id.clone());
                diff.email_destroyed_accounts.push(account_id);
            }
            ChangeOp::MailboxCreate(mailbox) => {
                let mut mailbox = (**mailbox).clone();
                if fix.mailboxes.iter().any(|m| m.id == mailbox.id) {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "duplicate",
                        &format!("mailbox_create {:?}: id already exists", mailbox.id),
                    ));
                }
                let parent_account = if let Some(parent) = &mailbox.parent_id {
                    let Some(p) = fix.mailboxes.iter().find(|m| &m.id == parent) else {
                        return Err(step_apply_error(
                            &step.id,
                            i,
                            "unknownParent",
                            &format!(
                                "mailbox_create {:?}: parent {parent:?} not in fixture",
                                mailbox.id
                            ),
                        ));
                    };
                    Some(p.account_id.clone())
                } else {
                    None
                };
                // Inherit account from parent when present; default
                // to primary otherwise. The empty-string placeholder
                // from `fixture::mailbox_create_op` triggers the
                // default; an explicit `account_id` must match the
                // parent's when there is one.
                let primary_id = fix.primary_account().id.clone();
                let default_account = parent_account.unwrap_or(primary_id);
                if mailbox.account_id.is_empty() {
                    mailbox.account_id = default_account;
                } else if mailbox.account_id != default_account {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "accountMismatch",
                        &format!(
                            "mailbox_create {:?}: account {:?} disagrees with parent {:?}",
                            mailbox.id, mailbox.account_id, default_account
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
                let account_id = fix
                    .mailboxes
                    .iter()
                    .find(|m| &m.id == id)
                    .map(|m| m.account_id.clone());
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
                diff.mailbox_destroyed_accounts
                    .push(account_id.expect("mailbox existed before retain"));
            }
            ChangeOp::EventCreate(event) => {
                let mut event = (**event).clone();
                let Some(cal) = fix.calendars.iter().find(|c| c.id == event.calendar_id) else {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "unknownCalendar",
                        &format!(
                            "event_create {:?}: calendar {:?} not in fixture",
                            event.id, event.calendar_id
                        ),
                    ));
                };
                if event.account_id.is_empty() {
                    event.account_id = cal.account_id.clone();
                } else if event.account_id != cal.account_id {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "accountMismatch",
                        &format!(
                            "event_create {:?}: declared account {:?} disagrees with calendar's {:?}",
                            event.id, event.account_id, cal.account_id
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
                if let Err(err) =
                    crate::fixture::apply_change_event_patch(&mut fix.events[idx], patch)
                {
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
                let mut folder = (**folder).clone();
                if fix.contact_folders.iter().any(|f| f.id == folder.id) {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "duplicate",
                        &format!("contact_folder_create {:?}: id already exists", folder.id),
                    ));
                }
                let parent_account = if let Some(parent) = &folder.parent_folder_id {
                    let Some(p) = fix.contact_folders.iter().find(|f| &f.id == parent) else {
                        return Err(step_apply_error(
                            &step.id,
                            i,
                            "unknownParent",
                            &format!(
                                "contact_folder_create {:?}: parent {parent:?} not in fixture",
                                folder.id
                            ),
                        ));
                    };
                    Some(p.account_id.clone())
                } else {
                    None
                };
                let primary_id = fix.primary_account().id.clone();
                let default_account = parent_account.unwrap_or(primary_id);
                if folder.account_id.is_empty() {
                    folder.account_id = default_account;
                } else if folder.account_id != default_account {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "accountMismatch",
                        &format!(
                            "contact_folder_create {:?}: account {:?} disagrees with parent {:?}",
                            folder.id, folder.account_id, default_account
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
                if let Err(err) = crate::fixture::apply_contact_folder_patch(&mut clone, patch) {
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
                        &format!("contact_folder_destroy {id:?}: folder still has contacts"),
                    ));
                }
                let destroyed_account = fix
                    .contact_folders
                    .iter()
                    .find(|f| &f.id == id)
                    .map(|f| f.account_id.clone());
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
                diff.contact_folder_destroyed_accounts
                    .push(destroyed_account.expect("folder existed (retain removed it)"));
            }
            ChangeOp::ContactCreate(contact) => {
                let mut contact = (**contact).clone();
                let folder = match fix
                    .contact_folders
                    .iter()
                    .find(|f| f.id == contact.folder_id)
                {
                    Some(f) => f,
                    None => {
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
                };
                // Inherit account_id from the parent folder when the
                // change-script didn't set it (the empty string is
                // the apply-time placeholder produced by
                // `fixture::contact_create_op`). Reject cross-account
                // pairings explicitly so the change-script can't
                // smuggle a contact into a folder it doesn't belong to.
                if contact.account_id.is_empty() {
                    contact.account_id = folder.account_id.clone();
                } else if contact.account_id != folder.account_id {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "accountMismatch",
                        &format!(
                            "contact_create {:?}: account {:?} disagrees with folder's {:?}",
                            contact.id, contact.account_id, folder.account_id
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
                if let Err(err) =
                    crate::fixture::apply_contact_patch(&mut clone, patch, &fix.contact_folders)
                {
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
            ChangeOp::GroupCreate(group) => {
                if fix.groups.iter().any(|existing| existing.id == group.id) {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "duplicate",
                        &format!("group_create {:?}: id already exists", group.id),
                    ));
                }
                if group
                    .members
                    .iter()
                    .any(|member| !fix.accounts.iter().any(|account| &account.id == member))
                {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "unknownAccount",
                        "group_create member not declared",
                    ));
                }
                fix.groups.push((**group).clone());
            }
            ChangeOp::GroupUpdate {
                id,
                display_name,
                mail,
                group_types,
                members,
            } => {
                if let Some(members) = members
                    && members
                        .iter()
                        .any(|member| !fix.accounts.iter().any(|account| &account.id == member))
                {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "unknownAccount",
                        "group_update member not declared",
                    ));
                }
                let Some(group) = fix.groups.iter_mut().find(|group| &group.id == id) else {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "notFound",
                        &format!("group_update {id:?}: no such group"),
                    ));
                };
                if let Some(value) = display_name {
                    group.display_name = value.clone();
                }
                if let Some(value) = mail {
                    group.mail = value.clone();
                }
                if let Some(value) = group_types {
                    group.group_types = value.clone();
                }
                if let Some(value) = members {
                    group.members = value.clone();
                }
            }
            ChangeOp::GroupDestroy { id } => {
                let before = fix.groups.len();
                fix.groups.retain(|group| &group.id != id);
                if before == fix.groups.len() {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "notFound",
                        &format!("group_destroy {id:?}: no such group"),
                    ));
                }
            }
            ChangeOp::AclGrant {
                mailbox_id,
                identifier,
                rights,
            } => {
                // Same cross-reference rules the static `[[acl]]`
                // loader enforces, re-checked here because an earlier
                // op in this step may have created the mailbox.
                let Some(mailbox) = fix.mailboxes.iter().find(|m| &m.id == mailbox_id) else {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "unknownMailbox",
                        &format!("acl_grant {mailbox_id:?}: mailbox not in fixture"),
                    ));
                };
                let owner_account_id = mailbox.account_id.clone();
                if fix.account(identifier).is_none() {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "unknownAccount",
                        &format!(
                            "acl_grant on {mailbox_id:?}: identifier {identifier:?} does not match any declared account"
                        ),
                    ));
                }
                if &owner_account_id == identifier {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "ownerGrant",
                        &format!(
                            "acl_grant on {mailbox_id:?}: identifier {identifier:?} owns the mailbox (owner rights are implicit)"
                        ),
                    ));
                }
                // Resolve to an index rather than holding an
                // `iter_mut` borrow across the arms: the `None` arm
                // needs to push, which borrowck won't allow while the
                // scrutinee's mutable borrow is live.
                let existing = fix
                    .acls
                    .iter()
                    .position(|a| &a.mailbox_id == mailbox_id && &a.identifier == identifier);
                match existing {
                    Some(idx) if fix.acls[idx].rights == *rights => {
                        return Err(step_apply_error(
                            &step.id,
                            i,
                            "duplicate",
                            &format!(
                                "acl_grant on {mailbox_id:?}: {identifier:?} already holds {rights:?}"
                            ),
                        ));
                    }
                    // Re-grant with different rights: a rights change
                    // is a real transition (a read-only share becoming
                    // writable), recorded as a grant.
                    Some(idx) => fix.acls[idx].rights.clone_from(rights),
                    None => fix.acls.push(crate::fixture::AclGrant {
                        mailbox_id: mailbox_id.clone(),
                        identifier: identifier.clone(),
                        rights: rights.clone(),
                    }),
                }
                diff.acl_granted.push(crate::fixture::AclRef {
                    mailbox_id: mailbox_id.clone(),
                    identifier: identifier.clone(),
                    owner_account_id,
                });
            }
            ChangeOp::AclRevoke {
                mailbox_id,
                identifier,
            } => {
                let Some(idx) = fix
                    .acls
                    .iter()
                    .position(|a| &a.mailbox_id == mailbox_id && &a.identifier == identifier)
                else {
                    return Err(step_apply_error(
                        &step.id,
                        i,
                        "notFound",
                        &format!("acl_revoke on {mailbox_id:?}: no grant for {identifier:?}"),
                    ));
                };
                // Resolve the owner BEFORE the removal; a revoke
                // leaves nothing behind to route the owner's log from
                // (same reason the destroy paths capture parents).
                let owner_account_id = fix
                    .mailbox_by_id(mailbox_id)
                    .map(|m| m.account_id.clone())
                    .unwrap_or_else(|| fix.primary_account().id.clone());
                fix.acls.remove(idx);
                diff.acl_revoked.push(crate::fixture::AclRef {
                    mailbox_id: mailbox_id.clone(),
                    identifier: identifier.clone(),
                    owner_account_id,
                });
            }
        }
    }
    Ok(())
}

fn step_apply_error(
    step_id: &str,
    op_index: usize,
    kind: &str,
    detail: &str,
) -> (StatusCode, Value) {
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
