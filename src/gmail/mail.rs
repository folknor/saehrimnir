//! Gmail mail-sync endpoints.
//!
//! Implements the subset of `/gmail/v1/users/me/...` ratatoskr's
//! Gmail mail-sync code path exercises. See
//! `notes/ratatoskr-gmail-surface.md` for the wire-format citations
//! against ratatoskr's `crates/gmail/` source.

use axum::{
    Router,
    extract::{Path, RawQuery, State},
    http::StatusCode,
    response::Response,
    routing::get,
};
use serde_json::{Map, Value, json};

use super::{AppState, error, ok_json};
use crate::fixture::{Address, Body, Email, Fixture, Mailbox, Role};

/// Pinned historyId for the lifetime of a fixture - matches the
/// determinism contract (read-only fixtures, no real changes).
const HISTORY_ID: &str = "1";

/// Default page size for thread list. Real Gmail defaults to 100;
/// we match that.
const THREADS_DEFAULT_MAX: u32 = 100;
const THREADS_HARD_MAX: u32 = 500;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/gmail/v1/users/me/profile", get(profile))
        .route("/gmail/v1/users/me/labels", get(list_labels))
        .route("/gmail/v1/users/me/threads", get(list_threads))
        .route("/gmail/v1/users/me/threads/{thread_id}", get(get_thread))
        .route("/gmail/v1/users/me/history", get(history))
        .route(
            "/gmail/v1/users/me/messages/{message_id}/attachments/{attachment_id}",
            get(get_attachment),
        )
        .route("/gmail/v1/users/me/settings/sendAs", get(send_as))
}

// ── Profile / Labels ────────────────────────────────────────────────

async fn profile(State(state): State<AppState>) -> Response {
    let f = &state.fixture;
    ok_json(json!({
        "emailAddress": f.account.name,
        "messagesTotal": f.emails.len(),
        "threadsTotal": unique_thread_count(f),
        "historyId": HISTORY_ID,
    }))
}

fn unique_thread_count(f: &Fixture) -> usize {
    let mut seen: Vec<&str> = f.emails.iter().map(|e| e.thread_id.as_str()).collect();
    seen.sort_unstable();
    seen.dedup();
    seen.len()
}

async fn list_labels(State(state): State<AppState>) -> Response {
    let mut labels = Vec::new();

    // System labels are always present, even if no fixture mailbox
    // carries the corresponding role - matches Gmail's behaviour.
    for sys in SYSTEM_LABELS {
        labels.push(label_value(sys, sys, "system", &state.fixture));
    }

    // User labels: fixture mailboxes without a role become user
    // labels.
    for m in &state.fixture.mailboxes {
        if m.role.is_some() {
            continue;
        }
        let id = format!("Label_{}", m.id);
        labels.push(label_value(&id, &m.name, "user", &state.fixture));
    }

    // Custom keywords (non-`$` prefixed) on any email become user
    // labels too. Collect distinct ones.
    let mut custom: Vec<String> = state
        .fixture
        .emails
        .iter()
        .flat_map(|e| e.keywords.iter().filter(|k| !k.starts_with('$')).cloned())
        .collect();
    custom.sort();
    custom.dedup();
    for keyword in custom {
        let id = format!("Label_{keyword}");
        labels.push(label_value(&id, &keyword, "user", &state.fixture));
    }

    ok_json(json!({"labels": labels}))
}

const SYSTEM_LABELS: &[&str] = &[
    "INBOX", "SENT", "DRAFT", "TRASH", "SPAM", "IMPORTANT", "STARRED", "UNREAD",
];

fn label_value(id: &str, name: &str, kind: &str, fixture: &Fixture) -> Value {
    let (msg_total, msg_unread, thread_total, thread_unread) =
        label_counts(fixture, id);
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

fn label_counts(fixture: &Fixture, label_id: &str) -> (u64, u64, u64, u64) {
    let in_label: Vec<&Email> = fixture
        .emails
        .iter()
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
    label_ids_for(email, fixture).iter().any(|id| id == label_id)
}

/// Compute the full label set for a fixture email, matching the
/// projection rules in `notes/ratatoskr-gmail-surface.md`.
fn label_ids_for(email: &Email, fixture: &Fixture) -> Vec<String> {
    let mut out = Vec::new();
    let by_id: std::collections::HashMap<&str, &Mailbox> = fixture
        .mailboxes
        .iter()
        .map(|m| (m.id.as_str(), m))
        .collect();
    for mb_id in &email.mailbox_ids {
        let Some(m) = by_id.get(mb_id.as_str()) else {
            continue;
        };
        match m.role {
            Some(Role::Inbox) => out.push("INBOX".to_string()),
            Some(Role::Sent) => out.push("SENT".to_string()),
            Some(Role::Drafts) => out.push("DRAFT".to_string()),
            Some(Role::Trash) => out.push("TRASH".to_string()),
            Some(Role::Junk) => out.push("SPAM".to_string()),
            Some(Role::Important) => out.push("IMPORTANT".to_string()),
            Some(Role::Archive) => {
                // Gmail's "archive" is the absence of INBOX, not a
                // distinct label.
            }
            None => out.push(format!("Label_{}", m.id)),
        }
    }
    if !email.keywords.iter().any(|k| k == "$seen") {
        out.push("UNREAD".to_string());
    }
    if email.keywords.iter().any(|k| k == "$flagged") {
        out.push("STARRED".to_string());
    }
    if email.keywords.iter().any(|k| k == "$draft") {
        out.push("DRAFT".to_string());
    }
    for keyword in &email.keywords {
        if !keyword.starts_with('$') {
            out.push(format!("Label_{keyword}"));
        }
    }
    out.sort();
    out.dedup();
    out
}

// ── Threads ─────────────────────────────────────────────────────────

async fn list_threads(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
) -> Response {
    let q = parse_query(raw.as_deref());
    let max = q
        .max_results
        .unwrap_or(THREADS_DEFAULT_MAX)
        .clamp(1, THREADS_HARD_MAX);
    let offset = q.offset();

    let mut threads = thread_summaries(&state.fixture);
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
        let cutoff = chrono::Utc
            .from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap_or_default());
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
                "historyId": HISTORY_ID,
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

fn thread_summaries(fixture: &Fixture) -> Vec<ThreadSummary> {
    let mut by_thread: std::collections::BTreeMap<String, Vec<&Email>> = Default::default();
    for e in &fixture.emails {
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
    Path(thread_id): Path<String>,
) -> Response {
    let mut messages: Vec<&Email> = state
        .fixture
        .emails
        .iter()
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
        .map(|e| message_value(e, &state.fixture))
        .collect();

    ok_json(json!({
        "id": thread_id,
        "historyId": HISTORY_ID,
        "snippet": snippet,
        "messages": messages_json,
    }))
}

fn message_value(e: &Email, fixture: &Fixture) -> Value {
    let label_ids = label_ids_for(e, fixture);
    let payload = build_payload(e);
    let body_size: u64 = match &e.body {
        Body::Text(t) => t.len() as u64,
    };
    json!({
        "id": e.id,
        "threadId": e.thread_id,
        "labelIds": label_ids,
        "snippet": match &e.body {
            Body::Text(t) => t.chars().take(100).collect::<String>(),
        },
        "historyId": HISTORY_ID,
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
    headers.push(header("Content-Type", "text/plain; charset=utf-8"));
    headers.push(header("Content-Transfer-Encoding", "8bit"));

    let body_str = match &e.body {
        Body::Text(t) => t.clone(),
    };
    let encoded = base64url_no_pad(body_str.as_bytes());

    let mut leaf = Map::new();
    leaf.insert("partId".to_string(), Value::String(String::new()));
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
            "data": encoded,
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
    xs.iter()
        .map(format_address)
        .collect::<Vec<_>>()
        .join(", ")
}

// ── History ─────────────────────────────────────────────────────────

async fn history(State(_state): State<AppState>) -> Response {
    // v0 fixtures are read-only; the History endpoint always returns
    // an empty change list paired with the same historyId. The
    // request's startHistoryId is intentionally ignored - any value
    // is "current" because the state never advances.
    ok_json(json!({
        "history": [],
        "historyId": HISTORY_ID,
    }))
}

// ── Attachments ─────────────────────────────────────────────────────

async fn get_attachment(
    Path((message_id, attachment_id)): Path<(String, String)>,
) -> Response {
    error(
        StatusCode::NOT_FOUND,
        &format!(
            "attachment {attachment_id:?} on message {message_id:?} not found"
        ),
        "notFound",
    )
}

// ── SendAs ──────────────────────────────────────────────────────────

async fn send_as() -> Response {
    // v0 emits an empty list so ratatoskr's signature-sync path
    // becomes a no-op. Real signatures will land alongside the
    // fixture's `[account.signature]` block in v1.
    ok_json(json!({"sendAs": []}))
}

// ── Query parsing ───────────────────────────────────────────────────

#[derive(Debug, Default)]
struct GmailQuery {
    q: Option<String>,
    max_results: Option<u32>,
    page_token: Option<String>,
    /// `format` parameter on thread/message GETs - tracked for
    /// completeness but the mock always emits the full shape.
    #[allow(dead_code)]
    format: Option<String>,
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

const URL_ALPHABET: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

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
