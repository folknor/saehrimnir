//! Gmail mail-sync endpoints.
//!
//! Implements the subset of `/gmail/v1/users/me/...` ratatoskr's
//! Gmail mail-sync code path exercises. See
//! `notes/ratatoskr-gmail-surface.md` for the wire-format citations
//! against ratatoskr's `crates/gmail/` source.

use axum::{
    Router,
    extract::{Path, RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Map, Value, json};

use super::{AppState, error, ok_json};
use crate::fixture::{Address, Attachment, Body, Email, Fixture, MutationDiff, Role};

/// Resolve the request's bearer to the account it authorizes.
/// Falls back to the fixture's primary account when no bearer is
/// presented or the token is unknown - matches the v0 no-auth
/// baseline so single-account fixtures stay one-listener-friendly.
fn bearer_account(state: &AppState, headers: &HeaderMap) -> String {
    crate::oauth::account_from_bearer(&state.fixture(), &state.shared.token_store, headers)
}

/// Default page size for thread list. Real Gmail defaults to 100;
/// we match that.
const THREADS_DEFAULT_MAX: u32 = 100;
const THREADS_HARD_MAX: u32 = 500;

/// Deterministic far-future `users.watch` expiration (unix ms as a
/// string, ~2100). Real Gmail returns a ~7-day expiry; the mock pins it
/// so the watch response stays byte-stable. A harness re-arms by
/// calling `watch` again, exactly as a real client would near expiry.
const WATCH_EXPIRATION_MS: &str = "4102444800000";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/gmail/v1/users/me/profile", get(profile))
        .route("/gmail/v1/users/me/labels", get(list_labels))
        .route("/gmail/v1/users/me/threads", get(list_threads))
        .route("/gmail/v1/users/me/threads/{thread_id}", get(get_thread))
        .route("/gmail/v1/users/me/messages", get(list_messages))
        // Mutations. Static `batchModify` / `batchDelete` segments
        // coexist with the `{message_id}` param at the same level
        // (matchit 0.8 matches statics before params).
        .route(
            "/gmail/v1/users/me/messages/batchModify",
            axum::routing::post(batch_modify),
        )
        .route(
            "/gmail/v1/users/me/messages/batchDelete",
            axum::routing::post(batch_delete),
        )
        .route(
            "/gmail/v1/users/me/messages/{message_id}/modify",
            axum::routing::post(modify_message),
        )
        .route("/gmail/v1/users/me/messages/{message_id}", get(get_message))
        .route("/gmail/v1/users/me/history", get(history))
        .route("/gmail/v1/users/me/watch", axum::routing::post(watch))
        .route("/gmail/v1/users/me/stop", axum::routing::post(stop))
        .route(
            "/gmail/v1/users/me/messages/{message_id}/attachments/{attachment_id}",
            get(get_attachment),
        )
        .route("/gmail/v1/users/me/settings/sendAs", get(list_send_as))
        .route(
            "/gmail/v1/users/me/settings/sendAs/{send_as_email}",
            get(get_send_as).patch(patch_send_as),
        )
}

// ── Profile / Labels ────────────────────────────────────────────────

async fn profile(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(r) = super::maybe_override(&state, "profile", |_s| Ok(())) {
        return r;
    }
    let account_id = bearer_account(&state, &headers);
    let f = state.fixture();
    let acct = f
        .account(&account_id)
        .unwrap_or_else(|| f.primary_account());
    let messages_total = f.emails_for(&account_id).count();
    ok_json(json!({
        "emailAddress": acct.name,
        "messagesTotal": messages_total,
        "threadsTotal": unique_thread_count(&f, &account_id),
        "historyId": f.gmail_history_id(&account_id).to_string(),
    }))
}

fn unique_thread_count(f: &Fixture, account_id: &str) -> usize {
    let mut seen: Vec<&str> = f
        .emails_for(account_id)
        .map(|e| e.thread_id.as_str())
        .collect();
    seen.sort_unstable();
    seen.dedup();
    seen.len()
}

// ── Push: watch / stop (Cloud Pub/Sub subscribe / unsubscribe) ──────

/// `POST /gmail/v1/users/me/watch` (Gmail `users.watch`). Registers a
/// Cloud Pub/Sub watch for the bearer-resolved account so the
/// state-mutation trigger publishes a `{ emailAddress, historyId }`
/// notification onto the watch's topic. Body carries `topicName` /
/// `labelIds` / `labelFilterBehavior`. Response mirrors real Gmail:
/// `{ "historyId": "<string>", "expiration": "<unix-ms-string>" }`.
async fn watch(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<axum::Json<Value>>,
) -> Response {
    let account_id = bearer_account(&state, &headers);
    let body = body.map_or(Value::Null, |axum::Json(v)| v);
    let topic_name = body
        .get("topicName")
        .and_then(|v| v.as_str())
        .unwrap_or("projects/saehrimnir/topics/mock")
        .to_string();
    let label_ids = body
        .get("labelIds")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let history_id = state.fixture().gmail_history_id(&account_id);
    state.shared.push.gmail_watch_set(crate::push::GmailWatch {
        account_id,
        topic_name,
        label_ids,
    });
    ok_json(json!({
        "historyId": history_id.to_string(),
        "expiration": WATCH_EXPIRATION_MS,
    }))
}

/// `POST /gmail/v1/users/me/stop` (Gmail `users.stop`). Cancels the
/// bearer-resolved account's watch. Real Gmail returns 204 No Content.
async fn stop(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let account_id = bearer_account(&state, &headers);
    state.shared.push.gmail_watch_clear(&account_id);
    StatusCode::NO_CONTENT.into_response()
}

async fn list_labels(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(r) = super::maybe_override(&state, "list_labels", |_s| Ok(())) {
        return r;
    }
    let account_id = bearer_account(&state, &headers);
    let mut labels = Vec::new();
    let fixture = state.fixture();

    // System labels are always present, even if no fixture mailbox
    // carries the corresponding role - matches Gmail's behaviour.
    for sys in SYSTEM_LABELS {
        labels.push(label_value(sys, sys, "system", &fixture, &account_id));
    }

    // User labels: fixture mailboxes without a role become user
    // labels.
    for m in fixture.mailboxes_for(&account_id) {
        if m.role.is_some() {
            continue;
        }
        let id = format!("Label_{}", m.id);
        labels.push(label_value(&id, &m.name, "user", &fixture, &account_id));
    }

    // Custom keywords (non-`$` prefixed) on any email become user
    // labels too. Collect distinct ones.
    let mut custom: Vec<String> = fixture
        .emails_for(&account_id)
        .flat_map(|e| e.keywords.iter().filter(|k| !k.starts_with('$')).cloned())
        .collect();
    custom.sort();
    custom.dedup();
    for keyword in custom {
        let id = format!("Label_{keyword}");
        labels.push(label_value(&id, &keyword, "user", &fixture, &account_id));
    }

    ok_json(json!({"labels": labels}))
}

const SYSTEM_LABELS: &[&str] = &[
    "INBOX",
    "SENT",
    "DRAFT",
    "TRASH",
    "SPAM",
    "IMPORTANT",
    "STARRED",
    "UNREAD",
];

fn label_value(id: &str, name: &str, kind: &str, fixture: &Fixture, account_id: &str) -> Value {
    let (msg_total, msg_unread, thread_total, thread_unread) =
        label_counts(fixture, id, account_id);
    json!({
        "id": id,
        "name": name,
        "type": kind,
        "messageListVisibility": "show",
        "labelListVisibility": "labelShow",
        "messagesTotal": msg_total,
        "messagesUnread": msg_unread,
        "threadsTotal": thread_total,
        "threadsUnread": thread_unread,
    })
}

fn label_counts(fixture: &Fixture, label_id: &str, account_id: &str) -> (u64, u64, u64, u64) {
    let in_label: Vec<&Email> = fixture
        .emails_for(account_id)
        .filter(|e| email_carries_label(e, fixture, label_id))
        .collect();
    let total = in_label.len() as u64;
    let unread = in_label
        .iter()
        .filter(|e| !e.keywords.iter().any(|k| k == "$seen"))
        .count() as u64;

    let mut threads: Vec<&str> = in_label.iter().map(|e| e.thread_id.as_str()).collect();
    threads.sort_unstable();
    threads.dedup();
    let thread_total = threads.len() as u64;

    let mut unread_threads: Vec<&str> = in_label
        .iter()
        .filter(|e| !e.keywords.iter().any(|k| k == "$seen"))
        .map(|e| e.thread_id.as_str())
        .collect();
    unread_threads.sort_unstable();
    unread_threads.dedup();
    let thread_unread = unread_threads.len() as u64;

    (total, unread, thread_total, thread_unread)
}

fn email_carries_label(email: &Email, fixture: &Fixture, label_id: &str) -> bool {
    label_ids_for(email, fixture)
        .iter()
        .any(|id| id == label_id)
}

/// Compute the full label set for a fixture email. The projection
/// rules live on `Fixture::gmail_label_ids` so the read path and the
/// `history.list` label-delta capture share one source of truth; see
/// `notes/ratatoskr-gmail-surface.md`.
fn label_ids_for(email: &Email, fixture: &Fixture) -> Vec<String> {
    fixture.gmail_label_ids(email)
}

// ── Threads ─────────────────────────────────────────────────────────

async fn list_threads(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    if let Some(r) = super::maybe_override(&state, "list_threads", |_s| Ok(())) {
        return r;
    }
    let account_id = bearer_account(&state, &headers);
    let q = parse_query(raw.as_deref());
    let max = q
        .max_results
        .unwrap_or(THREADS_DEFAULT_MAX)
        .clamp(1, THREADS_HARD_MAX);
    let offset = q.offset();

    let history_id = state.fixture().gmail_history_id(&account_id).to_string();
    let mut threads = thread_summaries(&state.fixture(), &account_id);
    if let Some(after_q) = &q.q {
        // v0 only knows the `after:YYYY/M/D` shape ratatoskr's
        // initial-sync code emits. Anything else - typo, drift, or
        // a Gmail query operator like `is:unread` - returns BAD
        // rather than silently dropping the filter and bleeding all
        // threads through (which the buggy fall-through used to do).
        let Some(date) = parse_after_query(after_q) else {
            return error(
                StatusCode::BAD_REQUEST,
                &format!("v0 mock only supports q=after:YYYY/M/D (got {after_q:?})"),
                "invalidQuery",
            );
        };
        let cutoff = chrono::Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap_or_default());
        threads.retain(|t| t.received_at >= cutoff);
    }

    let total = threads.len();
    let page: Vec<Value> = threads
        .iter()
        .skip(offset as usize)
        .take(max as usize)
        .map(|t| {
            json!({
                "id": t.id,
                "snippet": t.snippet,
                "historyId": history_id,
            })
        })
        .collect();

    let next_page_token = if (offset as usize) + page.len() < total {
        Some(encode_token((offset as usize) + page.len()))
    } else {
        None
    };

    let mut body = serde_json::Map::new();
    body.insert("threads".to_string(), Value::Array(page));
    if let Some(t) = next_page_token {
        body.insert("nextPageToken".to_string(), Value::String(t));
    }
    body.insert(
        "resultSizeEstimate".to_string(),
        Value::Number((total as u64).into()),
    );
    ok_json(Value::Object(body))
}

#[derive(Debug)]
struct ThreadSummary {
    id: String,
    snippet: String,
    received_at: chrono::DateTime<chrono::Utc>,
}

fn thread_summaries(fixture: &Fixture, account_id: &str) -> Vec<ThreadSummary> {
    let mut by_thread: std::collections::BTreeMap<String, Vec<&Email>> = Default::default();
    for e in fixture.emails_for(account_id) {
        by_thread.entry(e.thread_id.clone()).or_default().push(e);
    }
    let mut out: Vec<ThreadSummary> = by_thread
        .into_iter()
        .map(|(id, mut messages)| {
            messages.sort_by_key(|e| e.received_at);
            // by_thread is keyed by thread_id values pulled from
            // the email list itself, so messages is always non-empty
            // here. The `expect` documents that invariant; using a
            // wall-clock fallback would silently break the
            // determinism contract if the surrounding code ever
            // shifted.
            let received_at = messages
                .iter()
                .map(|m| m.received_at)
                .max()
                .expect("thread group derived from emails is non-empty by construction");
            let snippet = messages
                .last()
                .map(|m| match &m.body {
                    Body::Text(t) => t.chars().take(100).collect::<String>(),
                })
                .unwrap_or_default();
            ThreadSummary {
                id,
                snippet,
                received_at,
            }
        })
        .collect();
    // Most-recent thread first; id-lex tiebreak for byte determinism.
    out.sort_by(|a, b| {
        b.received_at
            .cmp(&a.received_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

async fn get_thread(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
) -> Response {
    let thread_id_owned = thread_id.clone();
    if let Some(r) = super::maybe_override(&state, "get_thread", move |s| {
        crate::lua::req_set_str(s, "thread_id", &thread_id_owned)
    }) {
        return r;
    }
    let account_id = bearer_account(&state, &headers);
    let fixture = state.fixture();
    let mut messages: Vec<&Email> = fixture
        .emails_for(&account_id)
        .filter(|e| e.thread_id == thread_id)
        .collect();
    if messages.is_empty() {
        return error(
            StatusCode::NOT_FOUND,
            &format!("thread {thread_id:?} not found"),
            "notFound",
        );
    }
    messages.sort_by_key(|e| e.received_at);

    let snippet = messages
        .last()
        .map(|m| match &m.body {
            Body::Text(t) => t.chars().take(100).collect::<String>(),
        })
        .unwrap_or_default();

    let messages_json: Vec<Value> = messages
        .iter()
        .map(|e| message_value(e, &fixture))
        .collect();

    ok_json(json!({
        "id": thread_id,
        "historyId": fixture.gmail_history_id(&account_id).to_string(),
        "snippet": snippet,
        "messages": messages_json,
    }))
}

fn message_value(e: &Email, fixture: &Fixture) -> Value {
    let label_ids = label_ids_for(e, fixture);
    let payload = build_payload(e);
    let body_size: u64 = match (e.raw_bytes.as_deref(), &e.body) {
        (Some(raw), _) => raw.len() as u64,
        (None, Body::Text(t)) => t.len() as u64,
    };
    let snippet = match (e.raw_bytes.as_deref(), &e.body) {
        (Some(raw), _) => raw.chars().take(100).collect::<String>(),
        (None, Body::Text(t)) => t.chars().take(100).collect::<String>(),
    };
    json!({
        "id": e.id,
        "threadId": e.thread_id,
        "labelIds": label_ids,
        "snippet": snippet,
        "historyId": fixture.gmail_history_id(&e.account_id).to_string(),
        "internalDate": e.received_at.timestamp_millis().to_string(),
        "sizeEstimate": body_size,
        "payload": payload,
    })
}

fn build_payload(e: &Email) -> Value {
    let mut headers = Vec::new();
    if let Some(from) = &e.from {
        headers.push(header("From", &format_address(from)));
    }
    if !e.to.is_empty() {
        headers.push(header("To", &format_address_list(&e.to)));
    }
    if !e.cc.is_empty() {
        headers.push(header("Cc", &format_address_list(&e.cc)));
    }
    if !e.bcc.is_empty() {
        headers.push(header("Bcc", &format_address_list(&e.bcc)));
    }
    if !e.reply_to.is_empty() {
        headers.push(header("Reply-To", &format_address_list(&e.reply_to)));
    }
    if let Some(s) = &e.subject {
        headers.push(header("Subject", s));
    }
    headers.push(header("Date", &e.sent_at.to_rfc2822()));
    if !e.message_id.is_empty() {
        headers.push(header("Message-ID", &e.message_id.join(" ")));
    }
    if !e.in_reply_to.is_empty() {
        headers.push(header("In-Reply-To", &e.in_reply_to.join(" ")));
    }
    if !e.references.is_empty() {
        headers.push(header("References", &e.references.join(" ")));
    }
    headers.push(header("MIME-Version", "1.0"));

    // When the fixture set `raw_bytes`, the body leaf carries those
    // bytes verbatim (base64url) instead of the canonical body_text.
    // attachments are dropped on the raw-bytes path because the bytes
    // are the entire body, including any MIME structure the author
    // wanted (mirrors the IMAP fetch contract documented on
    // `Email::raw_bytes`). Lets fixtures inject anomalous body shapes
    // through the Gmail projection: CRLF-only bodies, bare-LF, 8-bit
    // sequences, oversized data, etc. The malformed-MIME tests in
    // `tests/malformed_mime.rs` exercise the full path.
    let body_str = match (e.raw_bytes.as_deref(), &e.body) {
        (Some(raw), _) => raw.to_string(),
        (None, Body::Text(t)) => t.clone(),
    };

    if e.raw_bytes.is_some() || e.attachments.is_empty() {
        headers.push(header("Content-Type", "text/plain; charset=utf-8"));
        headers.push(header("Content-Transfer-Encoding", "8bit"));
        return text_leaf(String::new(), headers, &body_str);
    }

    // Multipart: root envelope + leaves. Real Gmail emits
    // attachment leaves with `body.attachmentId` and no `body.data`,
    // forcing clients to fetch via `/messages/{id}/attachments/{aid}`.
    let boundary = format!("=_saehrimnir_{}_=", e.id);
    headers.push(header(
        "Content-Type",
        &format!("multipart/mixed; boundary=\"{boundary}\""),
    ));

    let mut parts = Vec::with_capacity(1 + e.attachments.len());
    let text_headers = vec![
        header("Content-Type", "text/plain; charset=utf-8"),
        header("Content-Transfer-Encoding", "8bit"),
    ];
    parts.push(text_leaf("0".to_string(), text_headers, &body_str));
    for (i, a) in e.attachments.iter().enumerate() {
        parts.push(attachment_leaf(format!("{}", i + 1), a));
    }

    let mut root = Map::new();
    root.insert("partId".to_string(), Value::String(String::new()));
    root.insert(
        "mimeType".to_string(),
        Value::String("multipart/mixed".to_string()),
    );
    root.insert("filename".to_string(), Value::String(String::new()));
    root.insert("headers".to_string(), Value::Array(headers));
    root.insert(
        "body".to_string(),
        json!({
            "size": 0,
        }),
    );
    root.insert("parts".to_string(), Value::Array(parts));
    Value::Object(root)
}

fn text_leaf(part_id: String, headers: Vec<Value>, body_str: &str) -> Value {
    let mut leaf = Map::new();
    leaf.insert("partId".to_string(), Value::String(part_id));
    leaf.insert(
        "mimeType".to_string(),
        Value::String("text/plain".to_string()),
    );
    leaf.insert("filename".to_string(), Value::String(String::new()));
    leaf.insert("headers".to_string(), Value::Array(headers));
    leaf.insert(
        "body".to_string(),
        json!({
            "size": body_str.len(),
            "data": base64url_no_pad(body_str.as_bytes()),
        }),
    );
    Value::Object(leaf)
}

fn attachment_leaf(part_id: String, a: &Attachment) -> Value {
    let mut headers = vec![
        header(
            "Content-Type",
            &format!("{}; name=\"{}\"", a.content_type, a.name),
        ),
        header(
            "Content-Disposition",
            &format!("{}; filename=\"{}\"", a.disposition.as_str(), a.name),
        ),
        header("Content-Transfer-Encoding", "base64"),
    ];
    if let Some(cid) = &a.cid {
        headers.push(header("Content-ID", &format!("<{cid}>")));
    }
    let mut leaf = Map::new();
    leaf.insert("partId".to_string(), Value::String(part_id));
    leaf.insert(
        "mimeType".to_string(),
        Value::String(a.content_type.clone()),
    );
    leaf.insert("filename".to_string(), Value::String(a.name.clone()));
    leaf.insert("headers".to_string(), Value::Array(headers));
    leaf.insert(
        "body".to_string(),
        json!({
            "attachmentId": a.blob_id,
            "size": a.size,
        }),
    );
    Value::Object(leaf)
}

fn header(name: &str, value: &str) -> Value {
    json!({"name": name, "value": value})
}

fn format_address(a: &Address) -> String {
    match &a.name {
        Some(name) => format!("{name} <{}>", a.email),
        None => format!("<{}>", a.email),
    }
}

fn format_address_list(xs: &[Address]) -> String {
    xs.iter().map(format_address).collect::<Vec<_>>().join(", ")
}

// ── Messages ────────────────────────────────────────────────────────
//
// bifrost's GoogleAccount is message-centric: it backfills via
// `messages.list` (page 1 of which is the very first backfill call)
// and hydrates each result through `messages.get`. The legacy
// thread-centric surface above does not satisfy it, so these two
// routes are required for the Gmail sync gates to pass.

async fn list_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    if let Some(r) = super::maybe_override(&state, "list_messages", |_s| Ok(())) {
        return r;
    }
    let account_id = bearer_account(&state, &headers);
    let q = parse_query(raw.as_deref());
    let max = q
        .max_results
        .unwrap_or(THREADS_DEFAULT_MAX)
        .clamp(1, THREADS_HARD_MAX);
    let offset = q.offset();

    let fixture = state.fixture();
    let mut messages: Vec<&Email> = fixture.emails_for(&account_id).collect();
    if let Some(after_q) = &q.q {
        // Same `after:YYYY/M/D`-only contract as `list_threads`: any
        // other query shape is a hard 400 rather than a silent
        // full-list bleed-through. See `list_threads` for the rationale.
        let Some(date) = parse_after_query(after_q) else {
            return error(
                StatusCode::BAD_REQUEST,
                &format!("v0 mock only supports q=after:YYYY/M/D (got {after_q:?})"),
                "invalidQuery",
            );
        };
        let cutoff = chrono::Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap_or_default());
        messages.retain(|e| e.received_at >= cutoff);
    }
    // Most-recent message first; id-lex tiebreak for byte determinism.
    messages.sort_by(|a, b| {
        b.received_at
            .cmp(&a.received_at)
            .then_with(|| a.id.cmp(&b.id))
    });

    let total = messages.len();
    let page: Vec<Value> = messages
        .iter()
        .skip(offset as usize)
        .take(max as usize)
        .map(|e| {
            json!({
                "id": e.id,
                "threadId": e.thread_id,
            })
        })
        .collect();

    let next_page_token = if (offset as usize) + page.len() < total {
        Some(encode_token((offset as usize) + page.len()))
    } else {
        None
    };

    let mut body = serde_json::Map::new();
    body.insert("messages".to_string(), Value::Array(page));
    if let Some(t) = next_page_token {
        body.insert("nextPageToken".to_string(), Value::String(t));
    }
    body.insert(
        "resultSizeEstimate".to_string(),
        Value::Number((total as u64).into()),
    );
    ok_json(Value::Object(body))
}

async fn get_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(message_id): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let message_id_lua = message_id.clone();
    if let Some(r) = super::maybe_override(&state, "get_message", move |s| {
        crate::lua::req_set_str(s, "message_id", &message_id_lua)
    }) {
        return r;
    }
    let account_id = bearer_account(&state, &headers);
    let q = parse_query(raw.as_deref());
    let fixture = state.fixture();
    let Some(e) = fixture.emails_for(&account_id).find(|e| e.id == message_id) else {
        return error(
            StatusCode::NOT_FOUND,
            &format!("message {message_id:?} not found"),
            "notFound",
        );
    };

    // `metadata` / `full` / `minimal` all share the structured
    // `message_value` projection - bifrost reads the same camelCase
    // shape for each (it parses leniently, ignoring the payload it
    // doesn't need for the lighter formats). `raw` is the exception:
    // bifrost's `raw_bytes()` reads a top-level base64url `raw` field
    // and errors `ParseFailed` without it, so that format drops the
    // structured `payload` and emits the assembled RFC 822 bytes
    // instead - matching real Gmail's `format=raw` shape.
    let format = q.format.as_deref().unwrap_or("full");
    if format == "raw" {
        let mut obj = match message_value(e, &fixture) {
            Value::Object(m) => m,
            // message_value always builds an object; the arm is
            // structural, not reachable.
            other => return ok_json(other),
        };
        obj.remove("payload");
        let bytes = crate::imap::assembled_rfc822(e);
        obj.insert(
            "raw".to_string(),
            Value::String(base64url_no_pad(bytes.as_bytes())),
        );
        return ok_json(Value::Object(obj));
    }
    ok_json(message_value(e, &fixture))
}

// ── History ─────────────────────────────────────────────────────────

async fn history(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    if let Some(r) = super::maybe_override(&state, "history", |_s| Ok(())) {
        return r;
    }
    let account_id = bearer_account(&state, &headers);
    let q = parse_query(raw.as_deref());
    // `startHistoryId` is the client's cursor. Absent (or 0) means
    // "from the seed", same as the load-time historyId of 1.
    let start = q.start_history_id.unwrap_or(0);
    let fixture = state.fixture();
    match fixture.gmail_history_since(&account_id, start) {
        crate::fixture::GmailHistory::Unknown => {
            // The cursor predates the retained change log. Real Gmail
            // 404s here (with `historyId` in the message), which drives
            // ratatoskr's full re-sync fallback.
            error(
                StatusCode::NOT_FOUND,
                &format!("startHistoryId {start} is too old; a full sync is required (historyId)"),
                "notFound",
            )
        }
        crate::fixture::GmailHistory::Delta {
            records,
            history_id,
        } => {
            let history: Vec<Value> = records
                .iter()
                .map(|r| history_record_json(r, &fixture))
                .collect();
            ok_json(json!({
                "history": history,
                "historyId": history_id.to_string(),
            }))
        }
    }
}

/// Project one [`crate::fixture::GmailHistoryRecord`] into the Gmail
/// `history.list` wire shape. Keys are omitted when empty so a record
/// carries only the change types it actually represents.
fn history_record_json(r: &crate::fixture::GmailHistoryRecord, fixture: &Fixture) -> Value {
    let mut obj = Map::new();
    obj.insert("id".to_string(), Value::String(r.id.to_string()));
    if !r.messages_added.is_empty() {
        let arr: Vec<Value> = r
            .messages_added
            .iter()
            .filter_map(|id| fixture.emails.iter().find(|e| &e.id == id))
            .map(|e| json!({ "message": message_value(e, fixture) }))
            .collect();
        obj.insert("messagesAdded".to_string(), Value::Array(arr));
    }
    if !r.messages_deleted.is_empty() {
        // The email is gone by now, so only its id is recoverable.
        // `threadId` is required for the wire shape to deserialize but
        // ratatoskr ignores it on a tombstone (it keys the delete on
        // the message id alone), so the id stands in as a stable
        // placeholder.
        let arr: Vec<Value> = r
            .messages_deleted
            .iter()
            .map(|id| json!({ "message": { "id": id, "threadId": id } }))
            .collect();
        obj.insert("messagesDeleted".to_string(), Value::Array(arr));
    }
    if !r.labels_added.is_empty() {
        obj.insert(
            "labelsAdded".to_string(),
            Value::Array(label_wrappers(&r.labels_added, fixture)),
        );
    }
    if !r.labels_removed.is_empty() {
        obj.insert(
            "labelsRemoved".to_string(),
            Value::Array(label_wrappers(&r.labels_removed, fixture)),
        );
    }
    Value::Object(obj)
}

/// Build the `{ message: { id, threadId }, labelIds }` wrappers for a
/// `labelsAdded` / `labelsRemoved` array. The email is still live (a
/// label change, not a delete), so its real `threadId` is available.
fn label_wrappers(items: &[(String, Vec<String>)], fixture: &Fixture) -> Vec<Value> {
    items
        .iter()
        .map(|(id, labels)| {
            let thread_id = fixture
                .emails
                .iter()
                .find(|e| &e.id == id)
                .map_or_else(|| id.clone(), |e| e.thread_id.clone());
            json!({
                "message": { "id": id, "threadId": thread_id },
                "labelIds": labels,
            })
        })
        .collect()
}

// ── Mutations: modify / batchModify / batchDelete ───────────────────
//
// bifrost's Gmail mutation pipeline (google `account/mutation.rs`)
// drives mark-read / star / move / permanent-delete by POSTing
// `users.messages.batchModify` (addLabelIds / removeLabelIds over an
// `ids` list) and `users.messages.batchDelete` (destroy an `ids`
// list); the singular `users.messages.modify` is the per-message form
// real clients use. Each mutates the shared fixture and records the
// same change-log transition the read path observes - including the
// Gmail label-set delta sidecar (`email_label_changes`) - so a
// follow-up `history.list` / resync reflects it. Gmail label ids map
// onto the fixture keyword / mailbox model via the inverse of
// `Fixture::gmail_label_ids` (the read projection): the flag labels
// UNREAD / STARRED / DRAFT toggle `$`-keywords, the folder-role labels
// INBOX / SENT / TRASH / SPAM / IMPORTANT toggle the role-mailbox
// membership, and `Label_<x>` toggles either a roleless mailbox
// membership or a user keyword. bifrost expects a 2xx with no body it
// reads, so batchModify / batchDelete return 204; the singular modify
// returns 200 with the updated Message (matching real Gmail).

/// `POST /gmail/v1/users/me/messages/{id}/modify`.
async fn modify_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(message_id): Path<String>,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    let account_id = bearer_account(&state, &headers);
    let add = label_list(&body, "addLabelIds");
    let remove = label_list(&body, "removeLabelIds");
    let found = {
        let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
        modify_one(&mut fix, &account_id, &message_id, &add, &remove)
    };
    if !found {
        return error(
            StatusCode::NOT_FOUND,
            &format!("message {message_id:?} not found"),
            "notFound",
        );
    }
    let fixture = state.fixture();
    match fixture.emails_for(&account_id).find(|e| e.id == message_id) {
        Some(e) => ok_json(message_value(e, &fixture)),
        None => error(
            StatusCode::NOT_FOUND,
            &format!("message {message_id:?} not found"),
            "notFound",
        ),
    }
}

/// `POST /gmail/v1/users/me/messages/batchModify`. Same label patch
/// applied to every id; 204 No Content on success.
async fn batch_modify(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    let account_id = bearer_account(&state, &headers);
    let ids = label_list(&body, "ids");
    let add = label_list(&body, "addLabelIds");
    let remove = label_list(&body, "removeLabelIds");
    {
        let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
        for id in &ids {
            modify_one(&mut fix, &account_id, id, &add, &remove);
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

/// `POST /gmail/v1/users/me/messages/batchDelete`. Permanently
/// destroys every listed id; 204 No Content.
async fn batch_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    let account_id = bearer_account(&state, &headers);
    let ids = label_list(&body, "ids");
    {
        let mut fix = state.shared.fixture.write().expect("fixture lock poisoned");
        for id in &ids {
            delete_one(&mut fix, &account_id, id);
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Pull a string array (`addLabelIds` / `removeLabelIds` / `ids`) from
/// a request body, defaulting to empty.
fn label_list(body: &Value, key: &str) -> Vec<String> {
    body.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Apply a Gmail label patch to one fixture email, recording an
/// `email_updated` transition plus the Gmail label-set delta sidecar
/// so `history.list` projects the precise `labelsAdded` /
/// `labelsRemoved`. Returns `false` when the id is not in the account.
fn modify_one(
    fix: &mut Fixture,
    account_id: &str,
    message_id: &str,
    add: &[String],
    remove: &[String],
) -> bool {
    let acct = account_id.to_string();
    let mid = message_id.to_string();
    if !fix.emails_for(account_id).any(|e| e.id == message_id) {
        return false;
    }
    let _ = fix.mutate(|f| {
        let Some(idx) = f
            .emails
            .iter()
            .position(|e| e.account_id == acct && e.id == mid)
        else {
            return MutationDiff::default();
        };
        let before = f.gmail_label_ids(&f.emails[idx]);
        let old_mailboxes = f.emails[idx].mailbox_ids.clone();
        for label in add {
            apply_gmail_label(f, idx, &acct, label, true);
        }
        for label in remove {
            apply_gmail_label(f, idx, &acct, label, false);
        }
        let new_mailboxes = f.emails[idx].mailbox_ids.clone();
        f.sync_mailbox_uids(&mid, &old_mailboxes, &new_mailboxes);
        let after = f.gmail_label_ids(&f.emails[idx]);
        let mut diff = MutationDiff {
            email_updated: vec![mid.clone()],
            ..Default::default()
        };
        if let Some(change) = crate::fixture::gmail_label_change(&mid, &before, &after) {
            diff.email_label_changes.push(change);
        }
        diff
    });
    true
}

/// Permanently destroy one fixture email: retire its UID slots in
/// every mailbox it belonged to and record an `email_destroyed`
/// transition (so `history.list` projects a `messagesDeleted` record).
/// Returns `false` when the id is not in the account.
fn delete_one(fix: &mut Fixture, account_id: &str, message_id: &str) -> bool {
    let acct = account_id.to_string();
    let mid = message_id.to_string();
    let Some((mailboxes, owner)) = fix
        .emails
        .iter()
        .find(|e| e.account_id == acct && e.id == mid)
        .map(|e| (e.mailbox_ids.clone(), e.account_id.clone()))
    else {
        return false;
    };
    let _ = fix.mutate(move |f| {
        f.emails.retain(|e| !(e.account_id == acct && e.id == mid));
        for mb in &mailboxes {
            f.retire_uid(mb, &mid);
        }
        MutationDiff {
            email_destroyed: vec![mid.clone()],
            email_destroyed_accounts: vec![owner.clone()],
            ..Default::default()
        }
    });
    true
}

/// Toggle a single Gmail label on / off for `f.emails[idx]`, inverting
/// `Fixture::gmail_label_ids`. Unknown label ids (no `Label_` prefix
/// and not a known system label) are ignored.
fn apply_gmail_label(f: &mut Fixture, idx: usize, account_id: &str, label: &str, add: bool) {
    match label {
        // `UNREAD` present == NOT `$seen`, so adding UNREAD clears
        // `$seen` and removing UNREAD sets it.
        "UNREAD" => toggle_keyword(&mut f.emails[idx].keywords, "$seen", !add),
        "STARRED" => toggle_keyword(&mut f.emails[idx].keywords, "$flagged", add),
        "DRAFT" => toggle_keyword(&mut f.emails[idx].keywords, "$draft", add),
        "INBOX" => toggle_role_membership(f, idx, account_id, Role::Inbox, add),
        "SENT" => toggle_role_membership(f, idx, account_id, Role::Sent, add),
        "TRASH" => toggle_role_membership(f, idx, account_id, Role::Trash, add),
        "SPAM" => toggle_role_membership(f, idx, account_id, Role::Junk, add),
        "IMPORTANT" => toggle_role_membership(f, idx, account_id, Role::Important, add),
        other => {
            let Some(name) = other.strip_prefix("Label_") else {
                return;
            };
            // `Label_<x>` projects from either a roleless mailbox whose
            // id is `<x>` or a user keyword `<x>` (see gmail_label_ids).
            let mailbox_id = f
                .mailboxes_for(account_id)
                .find(|m| m.id == name && m.role.is_none())
                .map(|m| m.id.clone());
            match mailbox_id {
                Some(mb) => toggle_membership(f, idx, &mb, add),
                None => toggle_keyword(&mut f.emails[idx].keywords, name, add),
            }
        }
    }
}

fn toggle_keyword(keywords: &mut Vec<String>, kw: &str, present: bool) {
    let has = keywords.iter().any(|k| k == kw);
    if present && !has {
        keywords.push(kw.to_string());
    } else if !present && has {
        keywords.retain(|k| k != kw);
    }
}

/// Add / remove the role-mailbox membership for the email at `idx`,
/// resolving the account's mailbox carrying `role`. No-op when the
/// account has no mailbox with that role.
fn toggle_role_membership(f: &mut Fixture, idx: usize, account_id: &str, role: Role, add: bool) {
    let mailbox_id = f
        .mailboxes_for(account_id)
        .find(|m| m.role == Some(role))
        .map(|m| m.id.clone());
    if let Some(mb) = mailbox_id {
        toggle_membership(f, idx, &mb, add);
    }
}

fn toggle_membership(f: &mut Fixture, idx: usize, mailbox_id: &str, add: bool) {
    let email = &mut f.emails[idx];
    let has = email.mailbox_ids.iter().any(|m| m == mailbox_id);
    if add && !has {
        email.mailbox_ids.push(mailbox_id.to_string());
    } else if !add && has {
        email.mailbox_ids.retain(|m| m != mailbox_id);
    }
}

// ── Attachments ─────────────────────────────────────────────────────

async fn get_attachment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((message_id, attachment_id)): Path<(String, String)>,
) -> Response {
    let message_id_lua = message_id.clone();
    let attachment_id_lua = attachment_id.clone();
    if let Some(r) = super::maybe_override(&state, "get_attachment", move |s| {
        crate::lua::req_set_str(s, "message_id", &message_id_lua)?;
        crate::lua::req_set_str(s, "attachment_id", &attachment_id_lua)
    }) {
        return r;
    }
    let account_id = bearer_account(&state, &headers);
    state
        .shared
        .latency
        .sleep_for_attachment(&attachment_id)
        .await;
    let fixture = state.fixture();
    let Some(email) = fixture.emails_for(&account_id).find(|e| e.id == message_id) else {
        return error(
            StatusCode::NOT_FOUND,
            &format!("message {message_id:?} not found"),
            "notFound",
        );
    };
    let Some(att) = email
        .attachments
        .iter()
        .find(|a| a.blob_id == attachment_id)
    else {
        return error(
            StatusCode::NOT_FOUND,
            &format!("attachment {attachment_id:?} on message {message_id:?} not found"),
            "notFound",
        );
    };
    ok_json(json!({
        "size": att.size,
        "data": base64url_no_pad(&att.data),
    }))
}

// ── SendAs ──────────────────────────────────────────────────────────

async fn list_send_as(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(r) = super::maybe_override(&state, "send_as", |_s| Ok(())) {
        return r;
    }
    let account_id = bearer_account(&state, &headers);
    let fixture = state.fixture();
    let entries: Vec<Value> = fixture
        .send_as_for(&account_id)
        .map(serialize_send_as)
        .collect();
    ok_json(json!({ "sendAs": entries }))
}

async fn get_send_as(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(send_as_email): Path<String>,
) -> Response {
    let send_as_email_lua = send_as_email.clone();
    if let Some(r) = super::maybe_override(&state, "send_as", move |s| {
        crate::lua::req_set_str(s, "send_as_email", &send_as_email_lua)
    }) {
        return r;
    }
    let account_id = bearer_account(&state, &headers);
    let fixture = state.fixture();
    let Some(sa) = fixture
        .send_as_for(&account_id)
        .find(|s| s.send_as_email == send_as_email)
    else {
        return error(
            StatusCode::NOT_FOUND,
            &format!("sendAs {send_as_email:?} not found"),
            "notFound",
        );
    };
    ok_json(serialize_send_as(sa))
}

async fn patch_send_as(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(send_as_email): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let send_as_email_lua = send_as_email.clone();
    if let Some(r) = super::maybe_override(&state, "send_as", move |s| {
        crate::lua::req_set_str(s, "send_as_email", &send_as_email_lua)
    }) {
        return r;
    }
    let account_id = bearer_account(&state, &headers);
    let patch: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error(
                StatusCode::BAD_REQUEST,
                &format!("invalid JSON body: {e}"),
                "invalidInput",
            );
        }
    };
    let mut fixture = state.shared.fixture.write().expect("fixture lock poisoned");
    let Some(idx) = fixture
        .send_as
        .iter()
        .position(|s| s.account_id == account_id && s.send_as_email == send_as_email)
    else {
        return error(
            StatusCode::NOT_FOUND,
            &format!("sendAs {send_as_email:?} not found"),
            "notFound",
        );
    };
    // Real Gmail's PATCH is a sparse merge: any field present in the
    // body replaces the stored value; absent fields are left alone.
    // We apply the same shape so ratatoskr's signature-sync path can
    // toggle a single field without round-tripping the rest.
    let entry = &mut fixture.send_as[idx];
    if let Some(v) = patch.get("displayName") {
        entry.display_name = v.as_str().map(String::from);
    }
    if let Some(v) = patch.get("replyToAddress") {
        entry.reply_to_address = v.as_str().map(String::from);
    }
    if let Some(v) = patch.get("signature") {
        entry.signature = v.as_str().map(String::from);
    }
    if let Some(v) = patch.get("isDefault").and_then(Value::as_bool) {
        entry.is_default = v;
    }
    if let Some(v) = patch.get("treatAsAlias").and_then(Value::as_bool) {
        entry.treat_as_alias = v;
    }
    // `isPrimary` is read-only in real Gmail (mirrors the underlying
    // account's primary address); v0 ignores the field on PATCH so a
    // client mistakenly setting it doesn't corrupt the fixture.
    let updated = entry.clone();
    ok_json(serialize_send_as(&updated))
}

fn serialize_send_as(sa: &crate::fixture::SendAs) -> Value {
    let mut out = Map::new();
    out.insert(
        "sendAsEmail".to_string(),
        Value::String(sa.send_as_email.clone()),
    );
    if let Some(d) = &sa.display_name {
        out.insert("displayName".to_string(), Value::String(d.clone()));
    }
    if let Some(r) = &sa.reply_to_address {
        out.insert("replyToAddress".to_string(), Value::String(r.clone()));
    }
    if let Some(s) = &sa.signature {
        out.insert("signature".to_string(), Value::String(s.clone()));
    }
    out.insert("isPrimary".to_string(), Value::Bool(sa.is_primary));
    out.insert("isDefault".to_string(), Value::Bool(sa.is_default));
    out.insert("treatAsAlias".to_string(), Value::Bool(sa.treat_as_alias));
    // Real Gmail emits `verificationStatus` and `smtpMsa`; v0 omits
    // both since ratatoskr's sync code doesn't read them.
    Value::Object(out)
}

// ── Query parsing ───────────────────────────────────────────────────

#[derive(Debug, Default)]
struct GmailQuery {
    q: Option<String>,
    max_results: Option<u32>,
    page_token: Option<String>,
    /// `format` parameter on message GETs (`metadata` / `full` /
    /// `minimal` / `raw`). Drives the `messages.get` projection;
    /// `raw` swaps the structured payload for assembled RFC 822 bytes.
    format: Option<String>,
    /// `startHistoryId` cursor on the `history.list` endpoint, in the
    /// reported `historyId` space (the change-log counter + 1).
    start_history_id: Option<u64>,
}

impl GmailQuery {
    fn offset(&self) -> u32 {
        self.page_token
            .as_deref()
            .and_then(decode_token)
            .unwrap_or(0)
    }
}

fn parse_query(raw: Option<&str>) -> GmailQuery {
    let mut out = GmailQuery::default();
    let Some(s) = raw else { return out };
    for pair in s.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = match pair.split_once('=') {
            Some(kv) => kv,
            None => (pair, ""),
        };
        let v = url_decode(v);
        match k {
            "q" => out.q = Some(v),
            "maxResults" => out.max_results = v.parse().ok(),
            "pageToken" => out.page_token = Some(v),
            "format" => out.format = Some(v),
            "startHistoryId" => out.start_history_id = v.parse().ok(),
            _ => {}
        }
    }
    out
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let h = (hex(bytes[i + 1]), hex(bytes[i + 2]));
                if let (Some(a), Some(b)) = h {
                    out.push((a << 4) | b);
                    i += 2;
                } else {
                    out.push(b'%');
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn parse_after_query(q: &str) -> Option<chrono::NaiveDate> {
    let rest = q.trim().strip_prefix("after:")?.trim();
    chrono::NaiveDate::parse_from_str(rest, "%Y/%m/%d").ok()
}

use chrono::TimeZone;

fn encode_token(offset: usize) -> String {
    format!("t.{offset}")
}

fn decode_token(t: &str) -> Option<u32> {
    t.strip_prefix("t.")?.parse().ok()
}

// ── base64url (no padding) ──────────────────────────────────────────
//
// Hand-rolled to keep the dep surface small. Gmail wraps every body
// part in base64url (RFC 4648 sec 5) without trailing `=` padding
// (`parse.rs:269-275` calls `decode_base64url_nopad`).

const URL_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn base64url_no_pad(input: &[u8]) -> String {
    let mut out = String::with_capacity((input.len() * 4).div_ceil(3));
    let mut iter = input.chunks_exact(3);
    for chunk in iter.by_ref() {
        let b0 = chunk[0];
        let b1 = chunk[1];
        let b2 = chunk[2];
        out.push(URL_ALPHABET[(b0 >> 2) as usize] as char);
        out.push(URL_ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(URL_ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        out.push(URL_ALPHABET[(b2 & 0x3f) as usize] as char);
    }
    let rem = iter.remainder();
    match rem.len() {
        0 => {}
        1 => {
            let b0 = rem[0];
            out.push(URL_ALPHABET[(b0 >> 2) as usize] as char);
            out.push(URL_ALPHABET[((b0 & 0x03) << 4) as usize] as char);
        }
        2 => {
            let b0 = rem[0];
            let b1 = rem[1];
            out.push(URL_ALPHABET[(b0 >> 2) as usize] as char);
            out.push(URL_ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
            out.push(URL_ALPHABET[((b1 & 0x0f) << 2) as usize] as char);
        }
        _ => unreachable!(),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_known_vectors() {
        // RFC 4648 standard test vectors, with `=` padding stripped.
        assert_eq!(base64url_no_pad(b""), "");
        assert_eq!(base64url_no_pad(b"f"), "Zg");
        assert_eq!(base64url_no_pad(b"fo"), "Zm8");
        assert_eq!(base64url_no_pad(b"foo"), "Zm9v");
        assert_eq!(base64url_no_pad(b"foob"), "Zm9vYg");
        assert_eq!(base64url_no_pad(b"fooba"), "Zm9vYmE");
        assert_eq!(base64url_no_pad(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64url_uses_url_safe_alphabet() {
        // Bytes 0xFB 0xFF should produce `+` in standard base64 and
        // `-` in URL-safe.
        assert_eq!(base64url_no_pad(&[0xFB, 0xFF]), "-_8");
    }

    #[test]
    fn parse_after_query_accepts_yyyy_m_d() {
        let d = parse_after_query("after:2026/1/15").unwrap();
        assert_eq!(d, chrono::NaiveDate::from_ymd_opt(2026, 1, 15).unwrap());
    }

    #[test]
    fn parse_query_extracts_q_and_max_results() {
        let q = parse_query(Some("q=after%3A2026%2F1%2F1&maxResults=50"));
        assert_eq!(q.q.as_deref(), Some("after:2026/1/1"));
        assert_eq!(q.max_results, Some(50));
    }
}
