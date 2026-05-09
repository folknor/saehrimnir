//! JMAP request envelope, dispatcher, and per-method handlers.
//!
//! RFC 8620 §3.3 (request) and §3.4 (response). Method handlers live
//! here so `routes.rs` can stay just routing. The dispatcher returns
//! `("error", {...})` for unknown or per-call failures, per RFC §3.5.2.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::fixture::{Address, Attachment, Body, Disposition, Email, Fixture, Mailbox};

/// Wire-level request envelope.
#[derive(Debug, Deserialize)]
pub struct JmapRequest {
    #[allow(dead_code)]
    pub using: Vec<String>,
    #[serde(rename = "methodCalls")]
    pub method_calls: Vec<MethodCall>,
    #[serde(rename = "createdIds", default)]
    pub created_ids: Option<Value>,
}

/// Wire-level response envelope.
#[derive(Debug, Serialize)]
pub struct JmapResponse {
    #[serde(rename = "methodResponses")]
    pub method_responses: Vec<MethodResponse>,
    #[serde(rename = "sessionState")]
    pub session_state: String,
    #[serde(rename = "createdIds", skip_serializing_if = "Option::is_none")]
    pub created_ids: Option<Value>,
}

/// `[name, arguments, callId]` per RFC 8620 §3.3.
pub type MethodCall = (String, Value, String);
/// `[name, result, callId]` per RFC 8620 §3.4.
pub type MethodResponse = (String, Value, String);

/// Process every method call in a request envelope and produce the
/// response envelope. Errors per call land inside `methodResponses` as
/// `("error", {...}, callId)`; the envelope itself is always 200.
pub fn handle(
    fixture: &Fixture,
    dispatcher: Option<&std::sync::Arc<crate::lua::Dispatcher>>,
    req: JmapRequest,
) -> JmapResponse {
    let mut responses = Vec::with_capacity(req.method_calls.len());
    for (name, args, call_id) in req.method_calls {
        let (out_name, out_args) = dispatch(fixture, dispatcher, &name, &args);
        responses.push((out_name, out_args, call_id));
    }
    JmapResponse {
        method_responses: responses,
        session_state: fixture.state.clone(),
        created_ids: req.created_ids,
    }
}

fn dispatch(
    fixture: &Fixture,
    dispatcher: Option<&std::sync::Arc<crate::lua::Dispatcher>>,
    name: &str,
    args: &Value,
) -> (String, Value) {
    // Reactive callback: a registered handler can override the
    // method response. Surfaced fields: `account_id` (when present),
    // and `ids` as a 1-based Lua array when the call carries a
    // string-typed `ids[]` (Mailbox/get, Email/get).
    if let Some(d) = dispatcher {
        let account_id = args
            .get("accountId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let ids: Option<Vec<String>> = args
            .get("ids")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            });
        let result = d.dispatch("jmap", name, move |state| {
            if let Some(a) = &account_id {
                crate::lua::req_set_str(state, "account_id", a)?;
            }
            if let Some(ids) = &ids {
                crate::lua::req_set_str_array(state, "ids", ids)?;
            }
            Ok(())
        });
        if let crate::lua::Override::Tagged { status, message } = result {
            return (
                "error".to_string(),
                json!({"type": status, "description": message}),
            );
        }
    }

    match name {
        "Mailbox/get" => match mailbox_get(fixture, args) {
            Ok(v) => (name.to_string(), v),
            Err(err) => ("error".to_string(), err),
        },
        "Email/query" => match email_query(fixture, args) {
            Ok(v) => (name.to_string(), v),
            Err(err) => ("error".to_string(), err),
        },
        "Email/get" => match email_get(fixture, args) {
            Ok(v) => (name.to_string(), v),
            Err(err) => ("error".to_string(), err),
        },
        "Mailbox/changes" => match mailbox_changes(fixture, args) {
            Ok(v) => (name.to_string(), v),
            Err(err) => ("error".to_string(), err),
        },
        "Email/changes" => match email_changes(fixture, args) {
            Ok(v) => (name.to_string(), v),
            Err(err) => ("error".to_string(), err),
        },
        _ => (
            "error".to_string(),
            json!({
                "type": "unknownMethod",
                "description": format!("method {name:?} is not implemented in v0"),
            }),
        ),
    }
}

// ── Mailbox/get ─────────────────────────────────────────────────────

/// RFC 8621 §2.1.
fn mailbox_get(fixture: &Fixture, args: &Value) -> Result<Value, Value> {
    let account_id = args.get("accountId").and_then(Value::as_str).ok_or_else(|| {
        json!({
            "type": "invalidArguments",
            "description": "missing accountId",
        })
    })?;
    if account_id != fixture.account.id {
        return Err(json!({
            "type": "accountNotFound",
            "description": format!("account {account_id:?} not found"),
        }));
    }

    let (list, not_found) = match args.get("ids") {
        None | Some(Value::Null) => {
            let all = fixture
                .mailboxes
                .iter()
                .map(|m| serialize_mailbox(fixture, m))
                .collect::<Vec<_>>();
            (Value::Array(all), Value::Array(vec![]))
        }
        Some(Value::Array(requested)) => {
            let mut list = Vec::with_capacity(requested.len());
            let mut not_found = Vec::new();
            for v in requested {
                let Some(id) = v.as_str() else {
                    return Err(json!({
                        "type": "invalidArguments",
                        "description": "ids must be an array of strings",
                    }));
                };
                match fixture.mailboxes.iter().find(|m| m.id == id) {
                    Some(m) => list.push(serialize_mailbox(fixture, m)),
                    None => not_found.push(Value::String(id.to_string())),
                }
            }
            (Value::Array(list), Value::Array(not_found))
        }
        Some(_) => {
            return Err(json!({
                "type": "invalidArguments",
                "description": "ids must be an array or null",
            }));
        }
    };

    let mut out = Map::new();
    out.insert(
        "accountId".to_string(),
        Value::String(fixture.account.id.clone()),
    );
    out.insert("state".to_string(), Value::String(fixture.state.clone()));
    out.insert("list".to_string(), list);
    out.insert("notFound".to_string(), not_found);
    Ok(Value::Object(out))
}

fn serialize_mailbox(fixture: &Fixture, m: &Mailbox) -> Value {
    let (total_emails, unread_emails, total_threads, unread_threads) = mailbox_counts(fixture, m);

    let mut obj = Map::new();
    obj.insert("id".to_string(), Value::String(m.id.clone()));
    obj.insert("name".to_string(), Value::String(m.name.clone()));
    obj.insert(
        "parentId".to_string(),
        match &m.parent_id {
            Some(p) => Value::String(p.clone()),
            None => Value::Null,
        },
    );
    obj.insert(
        "role".to_string(),
        match m.role {
            Some(r) => Value::String(r.as_str().to_string()),
            None => Value::Null,
        },
    );
    obj.insert(
        "sortOrder".to_string(),
        Value::Number(m.sort_order.unwrap_or(0).into()),
    );
    obj.insert("totalEmails".to_string(), Value::Number(total_emails.into()));
    obj.insert(
        "unreadEmails".to_string(),
        Value::Number(unread_emails.into()),
    );
    obj.insert(
        "totalThreads".to_string(),
        Value::Number(total_threads.into()),
    );
    obj.insert(
        "unreadThreads".to_string(),
        Value::Number(unread_threads.into()),
    );
    obj.insert("myRights".to_string(), my_rights_all_true());
    obj.insert("isSubscribed".to_string(), Value::Bool(m.is_subscribed));
    Value::Object(obj)
}

fn mailbox_counts(fixture: &Fixture, m: &Mailbox) -> (u64, u64, u64, u64) {
    let in_box = fixture
        .emails
        .iter()
        .filter(|e| e.mailbox_ids.iter().any(|id| id == &m.id))
        .collect::<Vec<_>>();
    let total_emails = in_box.len() as u64;
    let unread_emails = in_box
        .iter()
        .filter(|e| !e.keywords.iter().any(|k| k == "$seen"))
        .count() as u64;

    let mut threads: Vec<&str> = in_box.iter().map(|e| e.thread_id.as_str()).collect();
    threads.sort_unstable();
    threads.dedup();
    let total_threads = threads.len() as u64;

    let mut unread_threads_set: Vec<&str> = in_box
        .iter()
        .filter(|e| !e.keywords.iter().any(|k| k == "$seen"))
        .map(|e| e.thread_id.as_str())
        .collect();
    unread_threads_set.sort_unstable();
    unread_threads_set.dedup();
    let unread_threads = unread_threads_set.len() as u64;

    (total_emails, unread_emails, total_threads, unread_threads)
}

fn my_rights_all_true() -> Value {
    let mut r = Map::new();
    for k in [
        "mayReadItems",
        "mayAddItems",
        "mayRemoveItems",
        "maySetSeen",
        "maySetKeywords",
        "mayCreateChild",
        "mayRename",
        "mayDelete",
        "maySubmit",
    ] {
        r.insert(k.to_string(), Value::Bool(true));
    }
    Value::Object(r)
}

// ── Email/query ─────────────────────────────────────────────────────

/// Server-side cap on `limit`. Above this we silently truncate.
const QUERY_LIMIT_CAP: u64 = 256;
// ── Mailbox/changes + Email/changes ─────────────────────────────────
//
// RFC 8621 §2.2 (`Mailbox/changes`) and §4.2 (`Email/changes`).
//
// v0 fixture state is constant across a process lifetime: there are
// no `[[change]]` scripts yet (see `TODO.md`, fixture-format growth).
// That gives us exactly two server-side cases:
//
//   1. `sinceState == fixture.state` -> no changes happened in this
//      window. Echo it as `newState`, return empty arrays,
//      `hasMoreChanges = false`. This is the steady-state response a
//      polling client sees.
//
//   2. `sinceState != fixture.state` -> the mock has no recorded
//      history to project, so per the RFC we return
//      `cannotCalculateChanges`. The client falls back to a fresh
//      `Email/query` + `Email/get` round.
//
// When `[[change]]` scripts ship, this dispatch grows a state machine
// that walks an ordered list of `(state, created, updated,
// destroyed)` deltas. The RFC-shaped envelope below stays the same.

fn mailbox_changes(fixture: &Fixture, args: &Value) -> Result<Value, Value> {
    let account_id = require_account(fixture, args)?;
    let since_state = require_since_state(args)?;
    if since_state != fixture.state {
        return Err(json!({
            "type": "cannotCalculateChanges",
            "description": format!(
                "sinceState {since_state:?} does not match the fixture state \
                 (no recorded change history in v0)"
            ),
        }));
    }
    Ok(no_changes_response(account_id, &fixture.state, false))
}

fn email_changes(fixture: &Fixture, args: &Value) -> Result<Value, Value> {
    let account_id = require_account(fixture, args)?;
    let since_state = require_since_state(args)?;
    if since_state != fixture.state {
        return Err(json!({
            "type": "cannotCalculateChanges",
            "description": format!(
                "sinceState {since_state:?} does not match the fixture state \
                 (no recorded change history in v0)"
            ),
        }));
    }
    Ok(no_changes_response(account_id, &fixture.state, true))
}

fn require_account<'a>(fixture: &'a Fixture, args: &'a Value) -> Result<&'a str, Value> {
    let account_id = args
        .get("accountId")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            json!({
                "type": "invalidArguments",
                "description": "missing accountId",
            })
        })?;
    if account_id != fixture.account.id {
        return Err(json!({
            "type": "accountNotFound",
            "description": format!("account {account_id:?} not found"),
        }));
    }
    Ok(account_id)
}

fn require_since_state(args: &Value) -> Result<&str, Value> {
    args.get("sinceState")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            json!({
                "type": "invalidArguments",
                "description": "missing sinceState",
            })
        })
}

/// Build the empty-changes response envelope shared by `Mailbox/
/// changes` and `Email/changes`. `is_email` toggles the
/// `updatedProperties: null` field, which is `Email/changes`-only
/// per RFC 8621 §4.2.
fn no_changes_response(account_id: &str, state: &str, is_email: bool) -> Value {
    let mut out = Map::new();
    out.insert("accountId".to_string(), Value::String(account_id.to_string()));
    out.insert("oldState".to_string(), Value::String(state.to_string()));
    out.insert("newState".to_string(), Value::String(state.to_string()));
    out.insert("hasMoreChanges".to_string(), Value::Bool(false));
    out.insert("created".to_string(), Value::Array(vec![]));
    out.insert("updated".to_string(), Value::Array(vec![]));
    out.insert("destroyed".to_string(), Value::Array(vec![]));
    if is_email {
        out.insert("updatedProperties".to_string(), Value::Null);
    }
    Value::Object(out)
}

/// Default page size when the client omits `limit`.
const QUERY_LIMIT_DEFAULT: u64 = 50;

/// RFC 8621 §4.4. v0 supports `{"after": <unix_seconds>}` and
/// `{"inMailbox": <id>}` filter conditions; sort is hard-wired to
/// `receivedAt` descending with `id` lexicographic as tiebreaker so
/// the wire output is byte-stable for a given fixture.
// `expect()` calls inside this fn are bounds-checked one line above
// each use - they cannot panic and aren't wire errors to surface.
#[allow(clippy::unwrap_in_result)]
fn email_query(fixture: &Fixture, args: &Value) -> Result<Value, Value> {
    let account_id = args.get("accountId").and_then(Value::as_str).ok_or_else(|| {
        json!({
            "type": "invalidArguments",
            "description": "missing accountId",
        })
    })?;
    if account_id != fixture.account.id {
        return Err(json!({
            "type": "accountNotFound",
            "description": format!("account {account_id:?} not found"),
        }));
    }

    let filter = parse_filter(args.get("filter"))?;

    let position = match args.get("position") {
        None | Some(Value::Null) => 0i64,
        Some(v) => v.as_i64().ok_or_else(|| {
            json!({
                "type": "invalidArguments",
                "description": "position must be an integer",
            })
        })?,
    };
    if position < 0 {
        return Err(json!({
            "type": "invalidArguments",
            "description": "negative position not supported in v0",
        }));
    }
    let position = u64::try_from(position).expect("position non-negative checked above");

    let limit = match args.get("limit") {
        None | Some(Value::Null) => QUERY_LIMIT_DEFAULT,
        Some(v) => {
            let n = v.as_i64().ok_or_else(|| {
                json!({
                    "type": "invalidArguments",
                    "description": "limit must be an integer",
                })
            })?;
            if n < 0 {
                return Err(json!({
                    "type": "invalidArguments",
                    "description": "limit must be non-negative",
                }));
            }
            u64::try_from(n)
                .expect("limit non-negative checked above")
                .min(QUERY_LIMIT_CAP)
        }
    };

    let calculate_total = args
        .get("calculateTotal")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // Filtered + sorted id list. Sort is descending by receivedAt with
    // id-lexicographic tiebreak, matching the determinism contract in
    // notes/fixture-format.md.
    let mut matches: Vec<&crate::fixture::Email> = fixture
        .emails
        .iter()
        .filter(|e| filter.matches(e))
        .collect();
    matches.sort_by(|a, b| {
        b.received_at
            .cmp(&a.received_at)
            .then_with(|| a.id.cmp(&b.id))
    });

    let total = matches.len() as u64;
    // start/end are bounded by `total = matches.len()` which already
    // fits in usize (it came from a Vec).
    let start = usize::try_from(position.min(total)).expect("start <= matches.len()");
    let end = usize::try_from((position + limit).min(total)).expect("end <= matches.len()");
    let ids: Vec<Value> = matches[start..end]
        .iter()
        .map(|e| Value::String(e.id.clone()))
        .collect();

    let mut out = Map::new();
    out.insert(
        "accountId".to_string(),
        Value::String(fixture.account.id.clone()),
    );
    out.insert(
        "queryState".to_string(),
        Value::String(fixture.state.clone()),
    );
    out.insert("canCalculateChanges".to_string(), Value::Bool(false));
    out.insert(
        "position".to_string(),
        Value::Number(position.into()),
    );
    out.insert("ids".to_string(), Value::Array(ids));
    if calculate_total {
        out.insert("total".to_string(), Value::Number(total.into()));
    }
    Ok(Value::Object(out))
}

/// v0 filter shape. FilterCondition keys are AND-ed together;
/// FilterOperator (explicit `operator` + `conditions` arrays) is
/// rejected because ratatoskr never sends one during initial sync.
#[derive(Default)]
struct Filter {
    after: Option<i64>,
    before: Option<i64>,
    in_mailbox: Option<String>,
}

impl Filter {
    fn matches(&self, e: &crate::fixture::Email) -> bool {
        let ts = e.received_at.timestamp();
        if let Some(a) = self.after
            && ts < a
        {
            return false;
        }
        if let Some(b) = self.before
            && ts >= b
        {
            return false;
        }
        if let Some(mb) = &self.in_mailbox
            && !e.mailbox_ids.iter().any(|m| m == mb)
        {
            return false;
        }
        true
    }
}

/// Accept either an integer (unix seconds, legacy) or a JMAP `UTCDate`
/// string (RFC3339, e.g. `"2026-01-15T11:00:00Z"`). Per RFC 8621
/// §4.4.1 `after`/`before` are `UTCDate`; ratatoskr's notes still
/// describe an integer, so both shapes are accepted.
fn parse_utc_date(field: &str, val: &Value) -> Result<i64, Value> {
    if let Some(i) = val.as_i64() {
        return Ok(i);
    }
    if let Some(s) = val.as_str() {
        let parsed = chrono::DateTime::parse_from_rfc3339(s).map_err(|_| {
            json!({
                "type": "invalidArguments",
                "description": format!(
                    "filter.{field} must be an RFC3339 UTCDate string or unix-seconds integer",
                ),
            })
        })?;
        return Ok(parsed.timestamp());
    }
    Err(json!({
        "type": "invalidArguments",
        "description": format!(
            "filter.{field} must be an RFC3339 UTCDate string or unix-seconds integer",
        ),
    }))
}

fn parse_filter(raw: Option<&Value>) -> Result<Filter, Value> {
    let mut f = Filter::default();
    let Some(v) = raw else { return Ok(f) };
    if v.is_null() {
        return Ok(f);
    }
    let obj = v.as_object().ok_or_else(|| {
        json!({
            "type": "invalidArguments",
            "description": "filter must be an object",
        })
    })?;
    for (key, val) in obj {
        match key.as_str() {
            "after" => {
                f.after = Some(parse_utc_date("after", val)?);
            }
            "before" => {
                f.before = Some(parse_utc_date("before", val)?);
            }
            "inMailbox" => {
                let s = val.as_str().ok_or_else(|| {
                    json!({
                        "type": "invalidArguments",
                        "description": "filter.inMailbox must be a string",
                    })
                })?;
                f.in_mailbox = Some(s.to_string());
            }
            other => {
                return Err(json!({
                    "type": "unsupportedFilter",
                    "description": format!("v0 does not support filter.{other:?}"),
                }));
            }
        }
    }
    Ok(f)
}

// ── Email/get ───────────────────────────────────────────────────────

/// RFC 8621 §4.2. Always returns the full RFC 8621 §4.1 property set;
/// the request's `properties` / `bodyProperties` lists are honoured by
/// the client tolerantly (extra keys are ignored), so emitting
/// everything is simpler than honouring the projection. The two custom
/// `header:*:asText` keys ratatoskr asks for are always present and
/// always `null` in v0.
fn email_get(fixture: &Fixture, args: &Value) -> Result<Value, Value> {
    let account_id = args.get("accountId").and_then(Value::as_str).ok_or_else(|| {
        json!({
            "type": "invalidArguments",
            "description": "missing accountId",
        })
    })?;
    if account_id != fixture.account.id {
        return Err(json!({
            "type": "accountNotFound",
            "description": format!("account {account_id:?} not found"),
        }));
    }

    let fetch_text = args
        .get("fetchTextBodyValues")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let fetch_html = args
        .get("fetchHtmlBodyValues")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let fetch_all = args
        .get("fetchAllBodyValues")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let (list, not_found) = match args.get("ids") {
        // Empty `ids` is the canonical shape ratatoskr's get_email_state
        // call uses purely to read a state token. Return an empty list,
        // not all emails.
        Some(Value::Array(requested)) => {
            let mut list = Vec::with_capacity(requested.len());
            let mut not_found = Vec::new();
            for v in requested {
                let Some(id) = v.as_str() else {
                    return Err(json!({
                        "type": "invalidArguments",
                        "description": "ids must be an array of strings",
                    }));
                };
                match fixture.emails.iter().find(|e| e.id == id) {
                    Some(e) => list.push(serialize_email(e, fetch_text, fetch_html, fetch_all)),
                    None => not_found.push(Value::String(id.to_string())),
                }
            }
            (Value::Array(list), Value::Array(not_found))
        }
        None | Some(Value::Null) => {
            // RFC: null = "all emails". Bound by maxObjectsInGet (500 in
            // our session capabilities). v0 fixtures stay well under
            // that, so no truncation logic.
            let all = fixture
                .emails
                .iter()
                .map(|e| serialize_email(e, fetch_text, fetch_html, fetch_all))
                .collect::<Vec<_>>();
            (Value::Array(all), Value::Array(vec![]))
        }
        Some(_) => {
            return Err(json!({
                "type": "invalidArguments",
                "description": "ids must be an array or null",
            }));
        }
    };

    let mut out = Map::new();
    out.insert(
        "accountId".to_string(),
        Value::String(fixture.account.id.clone()),
    );
    out.insert("state".to_string(), Value::String(fixture.state.clone()));
    out.insert("list".to_string(), list);
    out.insert("notFound".to_string(), not_found);
    Ok(Value::Object(out))
}

fn serialize_email(e: &Email, fetch_text: bool, fetch_html: bool, fetch_all: bool) -> Value {
    let mut obj = Map::new();
    obj.insert("id".to_string(), Value::String(e.id.clone()));
    obj.insert(
        "blobId".to_string(),
        Value::String(format!("blob-{}", e.id)),
    );
    obj.insert("threadId".to_string(), Value::String(e.thread_id.clone()));
    obj.insert("size".to_string(), Value::Number(e.size.into()));
    // Per RFC 8621 §4.1.1: `receivedAt` is `UTCDate` and `sentAt` is
    // `Date`; both serialise as RFC3339 strings, not unix seconds.
    // Fixture timestamps are already in UTC, so the "Z"-suffixed
    // `UTCDate` form is also a valid `Date`. Second precision keeps
    // bytes byte-stable across runs.
    obj.insert(
        "receivedAt".to_string(),
        Value::String(
            e.received_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        ),
    );
    obj.insert(
        "sentAt".to_string(),
        Value::String(
            e.sent_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        ),
    );

    obj.insert("mailboxIds".to_string(), bool_map(&e.mailbox_ids));
    obj.insert("keywords".to_string(), bool_map(&e.keywords));

    obj.insert("messageId".to_string(), string_array_or_null(&e.message_id));
    obj.insert(
        "inReplyTo".to_string(),
        string_array_or_null(&e.in_reply_to),
    );
    obj.insert(
        "references".to_string(),
        string_array_or_null(&e.references),
    );

    obj.insert(
        "from".to_string(),
        match &e.from {
            Some(a) => Value::Array(vec![serialize_address(a)]),
            None => Value::Null,
        },
    );
    obj.insert("to".to_string(), serialize_address_list(&e.to));
    obj.insert("cc".to_string(), serialize_address_list(&e.cc));
    obj.insert("bcc".to_string(), serialize_address_list(&e.bcc));
    obj.insert("replyTo".to_string(), serialize_address_list(&e.reply_to));

    obj.insert(
        "subject".to_string(),
        match &e.subject {
            Some(s) => Value::String(s.clone()),
            None => Value::Null,
        },
    );
    obj.insert(
        "preview".to_string(),
        match &e.preview {
            Some(s) => Value::String(s.clone()),
            None => Value::Null,
        },
    );
    obj.insert("hasAttachment".to_string(), Value::Bool(e.has_attachment));

    let (text_body, html_body, body_values) =
        body_parts_and_values(e, fetch_text, fetch_html, fetch_all);
    obj.insert("textBody".to_string(), text_body);
    obj.insert("htmlBody".to_string(), html_body);
    if let Some(bv) = body_values {
        obj.insert("bodyValues".to_string(), bv);
    }
    obj.insert(
        "attachments".to_string(),
        Value::Array(serialize_attachments(e)),
    );

    // Custom-header keys ratatoskr requests; always present, always
    // null until a fixture cares.
    for k in [
        "header:List-Unsubscribe:asText",
        "header:List-Unsubscribe-Post:asText",
        "header:Disposition-Notification-To:asText",
    ] {
        obj.insert(k.to_string(), Value::Null);
    }

    Value::Object(obj)
}

fn serialize_attachments(e: &Email) -> Vec<Value> {
    e.attachments
        .iter()
        .enumerate()
        .map(|(i, a)| serialize_attachment(e, i, a))
        .collect()
}

fn serialize_attachment(e: &Email, index: usize, a: &Attachment) -> Value {
    let mut obj = Map::new();
    obj.insert(
        "partId".to_string(),
        Value::String(format!("{}:att-{}", e.id, index + 1)),
    );
    obj.insert("blobId".to_string(), Value::String(a.blob_id.clone()));
    obj.insert("size".to_string(), Value::Number(a.size.into()));
    obj.insert("name".to_string(), Value::String(a.name.clone()));
    obj.insert(
        "type".to_string(),
        Value::String(a.content_type.clone()),
    );
    obj.insert(
        "disposition".to_string(),
        Value::String(a.disposition.as_str().to_string()),
    );
    obj.insert(
        "isInline".to_string(),
        Value::Bool(matches!(a.disposition, Disposition::Inline)),
    );
    obj.insert(
        "cid".to_string(),
        match &a.cid {
            Some(c) => Value::String(c.clone()),
            None => Value::Null,
        },
    );
    Value::Object(obj)
}

fn body_parts_and_values(
    e: &Email,
    fetch_text: bool,
    fetch_html: bool,
    fetch_all: bool,
) -> (Value, Value, Option<Value>) {
    match &e.body {
        Body::Text(text) => {
            let part_id = format!("{}:text", e.id);
            let blob_id = format!("blob-{}-text", e.id);
            let size = i64::try_from(text.len()).unwrap_or(i64::MAX);
            let part = body_part_text(&part_id, &blob_id, size);
            let text_body = Value::Array(vec![part]);
            let html_body = Value::Array(vec![]);
            let body_values = if fetch_text || fetch_all {
                let mut bv = Map::new();
                bv.insert(
                    part_id,
                    json!({
                        "value": text,
                        "isEncodingProblem": false,
                        "isTruncated": false,
                    }),
                );
                Some(Value::Object(bv))
            } else if fetch_html {
                // Empty bodyValues map (no html parts to fetch).
                Some(Value::Object(Map::new()))
            } else {
                None
            };
            (text_body, html_body, body_values)
        }
    }
}

fn body_part_text(part_id: &str, blob_id: &str, size: i64) -> Value {
    json!({
        "partId": part_id,
        "blobId": blob_id,
        "type": "text/plain",
        "size": size,
        "charset": "utf-8",
        "disposition": null,
        "language": null,
        "location": null,
        "subParts": null,
        "headers": [],
        "name": null,
        "cid": null,
    })
}

fn bool_map(ids: &[String]) -> Value {
    let mut m = Map::new();
    for id in ids {
        m.insert(id.clone(), Value::Bool(true));
    }
    Value::Object(m)
}

fn string_array_or_null(xs: &[String]) -> Value {
    if xs.is_empty() {
        Value::Null
    } else {
        Value::Array(xs.iter().map(|s| Value::String(s.clone())).collect())
    }
}

fn serialize_address(a: &Address) -> Value {
    json!({
        "name": a.name.clone().map(Value::String).unwrap_or(Value::Null),
        "email": a.email,
    })
}

fn serialize_address_list(xs: &[Address]) -> Value {
    if xs.is_empty() {
        Value::Null
    } else {
        Value::Array(xs.iter().map(serialize_address).collect())
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{Account, Body, Email, Fixture, Mailbox, Role};
    use chrono::TimeZone;

    fn fix() -> Fixture {
        let ts = chrono::Utc.with_ymd_and_hms(2026, 1, 15, 10, 0, 0).unwrap();
        Fixture {
            name: "t".into(),
            state: "s1".into(),
            account: Account {
                id: "acct".into(),
                name: "a@b".into(),
            },
            mailboxes: vec![
                Mailbox {
                    id: "mb-inbox".into(),
                    name: "Inbox".into(),
                    role: Some(Role::Inbox),
                    parent_id: None,
                    sort_order: Some(0),
                    is_subscribed: true,
                },
                Mailbox {
                    id: "mb-archive".into(),
                    name: "Archive".into(),
                    role: Some(Role::Archive),
                    parent_id: None,
                    sort_order: Some(1),
                    is_subscribed: true,
                },
            ],
            emails: vec![
                Email {
                    id: "e1".into(),
                    thread_id: "t1".into(),
                    mailbox_ids: vec!["mb-inbox".into()],
                    keywords: vec!["$seen".into()],
                    size: 1,
                    received_at: ts,
                    sent_at: ts,
                    from: None,
                    to: vec![],
                    cc: vec![],
                    bcc: vec![],
                    reply_to: vec![],
                    subject: None,
                    preview: None,
                    message_id: vec![],
                    in_reply_to: vec![],
                    references: vec![],
                    has_attachment: false,
                    body: Body::Text("x".into()),
                    attachments: vec![],
                },
                Email {
                    id: "e2".into(),
                    thread_id: "t2".into(),
                    mailbox_ids: vec!["mb-inbox".into()],
                    keywords: vec![],
                    size: 1,
                    received_at: ts,
                    sent_at: ts,
                    from: None,
                    to: vec![],
                    cc: vec![],
                    bcc: vec![],
                    reply_to: vec![],
                    subject: None,
                    preview: None,
                    message_id: vec![],
                    in_reply_to: vec![],
                    references: vec![],
                    has_attachment: false,
                    body: Body::Text("y".into()),
                    attachments: vec![],
                },
            ],
            oauth: crate::fixture::OAuthConfig::default(),
        }
    }

    #[test]
    fn mailbox_get_all_returns_declaration_order() {
        let f = fix();
        let args = json!({"accountId": "acct"});
        let resp = mailbox_get(&f, &args).unwrap();
        let list = resp.get("list").unwrap().as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].get("id").unwrap(), "mb-inbox");
        assert_eq!(list[1].get("id").unwrap(), "mb-archive");
        assert_eq!(resp.get("state").unwrap(), "s1");
        assert_eq!(resp.get("accountId").unwrap(), "acct");
        assert!(resp.get("notFound").unwrap().as_array().unwrap().is_empty());
    }

    #[test]
    fn mailbox_get_filters_by_ids_and_reports_not_found() {
        let f = fix();
        let args = json!({"accountId": "acct", "ids": ["mb-archive", "ghost"]});
        let resp = mailbox_get(&f, &args).unwrap();
        let list = resp.get("list").unwrap().as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].get("id").unwrap(), "mb-archive");
        let nf = resp.get("notFound").unwrap().as_array().unwrap();
        assert_eq!(nf, &vec![Value::String("ghost".into())]);
    }

    #[test]
    fn mailbox_get_null_ids_means_all() {
        let f = fix();
        let args = json!({"accountId": "acct", "ids": null});
        let resp = mailbox_get(&f, &args).unwrap();
        assert_eq!(resp.get("list").unwrap().as_array().unwrap().len(), 2);
    }

    #[test]
    fn mailbox_get_unknown_account_errors() {
        let f = fix();
        let args = json!({"accountId": "nope"});
        let err = mailbox_get(&f, &args).unwrap_err();
        assert_eq!(err.get("type").unwrap(), "accountNotFound");
    }

    #[test]
    fn mailbox_get_missing_account_errors() {
        let f = fix();
        let args = json!({});
        let err = mailbox_get(&f, &args).unwrap_err();
        assert_eq!(err.get("type").unwrap(), "invalidArguments");
    }

    #[test]
    fn mailbox_get_counts_total_and_unread() {
        let f = fix();
        let args = json!({"accountId": "acct", "ids": ["mb-inbox"]});
        let resp = mailbox_get(&f, &args).unwrap();
        let inbox = &resp.get("list").unwrap().as_array().unwrap()[0];
        assert_eq!(inbox.get("totalEmails").unwrap(), 2);
        assert_eq!(inbox.get("unreadEmails").unwrap(), 1);
        assert_eq!(inbox.get("totalThreads").unwrap(), 2);
        assert_eq!(inbox.get("unreadThreads").unwrap(), 1);
    }

    #[test]
    fn dispatch_unknown_method_returns_error() {
        let f = fix();
        let (name, body) = dispatch(&f, None, "Email/import", &json!({}));
        assert_eq!(name, "error");
        assert_eq!(body.get("type").unwrap(), "unknownMethod");
    }

    #[test]
    fn handle_passes_call_id_through_and_echoes_state() {
        let f = fix();
        let req = JmapRequest {
            using: vec!["urn:ietf:params:jmap:core".into()],
            method_calls: vec![("Mailbox/get".into(), json!({"accountId": "acct"}), "c0".into())],
            created_ids: None,
        };
        let resp = handle(&f, None, req);
        assert_eq!(resp.session_state, "s1");
        assert_eq!(resp.method_responses.len(), 1);
        let (name, _args, call_id) = &resp.method_responses[0];
        assert_eq!(name, "Mailbox/get");
        assert_eq!(call_id, "c0");
    }

    #[test]
    fn handle_dispatches_each_call_independently() {
        let f = fix();
        let req = JmapRequest {
            using: vec![],
            method_calls: vec![
                ("Mailbox/get".into(), json!({"accountId": "acct"}), "a".into()),
                ("Email/import".into(), json!({}), "b".into()),
            ],
            created_ids: None,
        };
        let resp = handle(&f, None, req);
        assert_eq!(resp.method_responses[0].0, "Mailbox/get");
        assert_eq!(resp.method_responses[0].2, "a");
        assert_eq!(resp.method_responses[1].0, "error");
        assert_eq!(resp.method_responses[1].2, "b");
    }

    fn email(id: &str, mailbox: &str, ts: chrono::DateTime<chrono::Utc>) -> Email {
        Email {
            id: id.into(),
            thread_id: format!("t-{id}"),
            mailbox_ids: vec![mailbox.into()],
            keywords: vec![],
            size: 1,
            received_at: ts,
            sent_at: ts,
            from: None,
            to: vec![],
            cc: vec![],
            bcc: vec![],
            reply_to: vec![],
            subject: None,
            preview: None,
            message_id: vec![],
            in_reply_to: vec![],
            references: vec![],
            has_attachment: false,
            body: Body::Text("x".into()),
            attachments: vec![],
        }
    }

    fn fix_for_query() -> Fixture {
        let mk = |y, m, d, hh| chrono::Utc.with_ymd_and_hms(y, m, d, hh, 0, 0).unwrap();
        Fixture {
            name: "q".into(),
            state: "s1".into(),
            account: Account {
                id: "acct".into(),
                name: "a@b".into(),
            },
            mailboxes: vec![
                Mailbox {
                    id: "mb-inbox".into(),
                    name: "Inbox".into(),
                    role: Some(Role::Inbox),
                    parent_id: None,
                    sort_order: Some(0),
                    is_subscribed: true,
                },
                Mailbox {
                    id: "mb-archive".into(),
                    name: "Archive".into(),
                    role: Some(Role::Archive),
                    parent_id: None,
                    sort_order: Some(1),
                    is_subscribed: true,
                },
            ],
            // Declaration order is intentionally NOT receivedAt order, to
            // prove the sort.
            emails: vec![
                email("c", "mb-inbox", mk(2026, 1, 15, 10)),
                email("a", "mb-inbox", mk(2026, 1, 15, 12)),
                email("b", "mb-inbox", mk(2026, 1, 15, 11)),
                // Same timestamp as "a"; lex on id breaks the tie.
                email("a2", "mb-inbox", mk(2026, 1, 15, 12)),
                email("d", "mb-archive", mk(2026, 1, 15, 9)),
            ],
            oauth: crate::fixture::OAuthConfig::default(),
        }
    }

    #[test]
    fn email_query_sorts_desc_by_received_at_with_id_tiebreak() {
        let f = fix_for_query();
        let resp = email_query(&f, &json!({"accountId": "acct"})).unwrap();
        let ids: Vec<&str> = resp
            .get("ids")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        // 12:00 -> a, a2 (lex tiebreak) ; 11:00 -> b ; 10:00 -> c ; 09:00 -> d.
        assert_eq!(ids, vec!["a", "a2", "b", "c", "d"]);
    }

    #[test]
    fn email_query_in_mailbox_filter() {
        let f = fix_for_query();
        let resp = email_query(
            &f,
            &json!({"accountId": "acct", "filter": {"inMailbox": "mb-archive"}}),
        )
        .unwrap();
        let ids: Vec<&str> = resp
            .get("ids")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["d"]);
    }

    #[test]
    fn email_query_after_filter_uses_unix_seconds() {
        let f = fix_for_query();
        // 2026-01-15T11:00:00Z -> matches "a", "a2", "b".
        let ts = chrono::Utc.with_ymd_and_hms(2026, 1, 15, 11, 0, 0).unwrap().timestamp();
        let resp =
            email_query(&f, &json!({"accountId": "acct", "filter": {"after": ts}})).unwrap();
        let ids: Vec<&str> = resp
            .get("ids")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["a", "a2", "b"]);
    }

    #[test]
    fn email_query_after_filter_accepts_rfc3339_utc_date() {
        let f = fix_for_query();
        // Same cutoff as the unix-seconds test, expressed as a JMAP
        // UTCDate string per RFC 8621 §4.4.1.
        let resp = email_query(
            &f,
            &json!({
                "accountId": "acct",
                "filter": {"after": "2026-01-15T11:00:00Z"},
            }),
        )
        .unwrap();
        let ids: Vec<&str> = resp
            .get("ids")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["a", "a2", "b"]);
    }

    #[test]
    fn email_query_before_filter_accepts_rfc3339_utc_date() {
        let f = fix_for_query();
        // Strict-less-than-cutoff: 11:00:00Z excludes the 11:00 emails
        // ("a", "a2", "b") and keeps the earlier "c" (10:00) and "d"
        // (09:00).
        let resp = email_query(
            &f,
            &json!({
                "accountId": "acct",
                "filter": {"before": "2026-01-15T11:00:00Z"},
            }),
        )
        .unwrap();
        let ids: Vec<&str> = resp
            .get("ids")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["c", "d"]);
    }

    #[test]
    fn email_query_after_filter_rejects_garbage_string() {
        let f = fix_for_query();
        let err = email_query(
            &f,
            &json!({"accountId": "acct", "filter": {"after": "not-a-date"}}),
        )
        .unwrap_err();
        assert_eq!(err.get("type").unwrap(), "invalidArguments");
    }

    #[test]
    fn email_query_pagination_terminates_below_limit() {
        let f = fix_for_query();
        // limit 2 across 5 ids -> pages of 2, 2, 1.
        let p1 = email_query(&f, &json!({"accountId": "acct", "limit": 2, "position": 0}))
            .unwrap();
        let p2 = email_query(&f, &json!({"accountId": "acct", "limit": 2, "position": 2}))
            .unwrap();
        let p3 = email_query(&f, &json!({"accountId": "acct", "limit": 2, "position": 4}))
            .unwrap();
        let len = |v: &Value| v.get("ids").unwrap().as_array().unwrap().len();
        assert_eq!(len(&p1), 2);
        assert_eq!(len(&p2), 2);
        assert_eq!(len(&p3), 1);
        // Position past total returns empty without erroring.
        let p4 = email_query(&f, &json!({"accountId": "acct", "limit": 2, "position": 99}))
            .unwrap();
        assert_eq!(len(&p4), 0);
    }

    #[test]
    fn email_query_calculate_total_only_when_requested() {
        let f = fix_for_query();
        let r1 = email_query(&f, &json!({"accountId": "acct", "calculateTotal": true})).unwrap();
        assert_eq!(r1.get("total").unwrap().as_u64().unwrap(), 5);
        let r2 = email_query(&f, &json!({"accountId": "acct"})).unwrap();
        assert!(r2.get("total").is_none());
    }

    #[test]
    fn email_query_response_has_query_state_and_can_calculate_changes_false() {
        let f = fix_for_query();
        let r = email_query(&f, &json!({"accountId": "acct"})).unwrap();
        assert_eq!(r.get("queryState").unwrap(), "s1");
        assert_eq!(r.get("canCalculateChanges").unwrap(), false);
        assert_eq!(r.get("accountId").unwrap(), "acct");
        assert_eq!(r.get("position").unwrap().as_u64().unwrap(), 0);
    }

    #[test]
    fn email_query_unknown_account_errors() {
        let f = fix_for_query();
        let err = email_query(&f, &json!({"accountId": "nope"})).unwrap_err();
        assert_eq!(err.get("type").unwrap(), "accountNotFound");
    }

    #[test]
    fn email_query_after_and_inmailbox_combine_with_and() {
        // Regression: parse_filter previously returned on the first
        // matched key and ignored later keys, so a filter with both
        // `after` and `inMailbox` would silently ignore inMailbox and
        // bleed cross-mailbox results.
        let f = fix_for_query();
        let cutoff = chrono::Utc
            .with_ymd_and_hms(2026, 1, 15, 11, 0, 0)
            .unwrap()
            .timestamp();
        let resp = email_query(
            &f,
            &json!({
                "accountId": "acct",
                "filter": {"after": cutoff, "inMailbox": "mb-archive"},
            }),
        )
        .unwrap();
        // mb-archive has only "d" at 09:00, which is BEFORE the
        // cutoff; AND-ing means the result is empty, not the
        // 11:00+ inbox emails the buggy code would have returned.
        let ids: Vec<&str> = resp
            .get("ids")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(ids.is_empty(), "got: {ids:?}");
    }

    #[test]
    fn email_query_unsupported_filter_errors() {
        let f = fix_for_query();
        let err = email_query(
            &f,
            &json!({"accountId": "acct", "filter": {"hasAttachment": true}}),
        )
        .unwrap_err();
        assert_eq!(err.get("type").unwrap(), "unsupportedFilter");
    }

    #[test]
    fn email_query_negative_position_errors() {
        let f = fix_for_query();
        let err = email_query(&f, &json!({"accountId": "acct", "position": -1})).unwrap_err();
        assert_eq!(err.get("type").unwrap(), "invalidArguments");
    }

    #[test]
    fn email_query_caps_limit_at_256() {
        let f = fix_for_query();
        // Limit 1_000_000 is silently clamped; with 5 emails the result
        // is the full list either way, so the visible signal is just
        // "no error".
        let r = email_query(&f, &json!({"accountId": "acct", "limit": 1_000_000})).unwrap();
        assert_eq!(r.get("ids").unwrap().as_array().unwrap().len(), 5);
    }

    // ── Email/get tests ─────────────────────────────────────────────

    fn fix_for_get() -> Fixture {
        let ts = chrono::Utc.with_ymd_and_hms(2026, 1, 15, 10, 0, 0).unwrap();
        let sent = chrono::Utc.with_ymd_and_hms(2026, 1, 15, 9, 59, 50).unwrap();
        Fixture {
            name: "g".into(),
            state: "s1".into(),
            account: Account {
                id: "acct".into(),
                name: "a@b".into(),
            },
            mailboxes: vec![Mailbox {
                id: "mb-inbox".into(),
                name: "Inbox".into(),
                role: Some(Role::Inbox),
                parent_id: None,
                sort_order: Some(0),
                is_subscribed: true,
            }],
            emails: vec![Email {
                id: "e1".into(),
                thread_id: "t1".into(),
                mailbox_ids: vec!["mb-inbox".into()],
                keywords: vec!["$seen".into(), "$flagged".into()],
                size: 5,
                received_at: ts,
                sent_at: sent,
                from: Some(Address {
                    name: Some("Alice".into()),
                    email: "alice@example.com".into(),
                }),
                to: vec![Address {
                    name: None,
                    email: "bob@example.com".into(),
                }],
                cc: vec![],
                bcc: vec![],
                reply_to: vec![],
                subject: Some("hi".into()),
                preview: None,
                message_id: vec!["<e1@example.com>".into()],
                in_reply_to: vec![],
                references: vec![],
                has_attachment: false,
                body: Body::Text("hello".into()),
                attachments: vec![],
            }],
            oauth: crate::fixture::OAuthConfig::default(),
        }
    }

    #[test]
    fn email_get_returns_state_for_empty_ids() {
        let f = fix_for_get();
        let resp = email_get(&f, &json!({"accountId": "acct", "ids": []})).unwrap();
        assert_eq!(resp.get("state").unwrap(), "s1");
        assert!(resp.get("list").unwrap().as_array().unwrap().is_empty());
        assert!(resp.get("notFound").unwrap().as_array().unwrap().is_empty());
    }

    #[test]
    fn email_get_full_shape() {
        let f = fix_for_get();
        let resp = email_get(
            &f,
            &json!({
                "accountId": "acct",
                "ids": ["e1"],
                "fetchTextBodyValues": true,
            }),
        )
        .unwrap();
        let item = &resp.get("list").unwrap().as_array().unwrap()[0];

        assert_eq!(item.get("id").unwrap(), "e1");
        assert_eq!(item.get("threadId").unwrap(), "t1");
        assert_eq!(item.get("blobId").unwrap(), "blob-e1");
        assert_eq!(item.get("size").unwrap(), 5);
        assert_eq!(item.get("subject").unwrap(), "hi");
        assert_eq!(item.get("hasAttachment").unwrap(), false);

        // Timestamps as RFC3339 UTCDate / Date strings per RFC 8621
        // §4.1.1.
        assert_eq!(item.get("receivedAt").unwrap(), "2026-01-15T10:00:00Z");
        let sent = item.get("sentAt").unwrap().as_str().unwrap();
        let received = item.get("receivedAt").unwrap().as_str().unwrap();
        let sent_ts = chrono::DateTime::parse_from_rfc3339(sent).unwrap();
        let received_ts = chrono::DateTime::parse_from_rfc3339(received).unwrap();
        assert!(sent_ts < received_ts);

        // mailboxIds + keywords are maps to true.
        let mb = item.get("mailboxIds").unwrap().as_object().unwrap();
        assert_eq!(mb.get("mb-inbox").unwrap(), true);
        let kw = item.get("keywords").unwrap().as_object().unwrap();
        assert_eq!(kw.get("$seen").unwrap(), true);
        assert_eq!(kw.get("$flagged").unwrap(), true);

        // Addresses: from is an array even though there's one entry.
        let from = item.get("from").unwrap().as_array().unwrap();
        assert_eq!(from[0].get("name").unwrap(), "Alice");
        assert_eq!(from[0].get("email").unwrap(), "alice@example.com");
        // null name when fixture omits it.
        let to = item.get("to").unwrap().as_array().unwrap();
        assert!(to[0].get("name").unwrap().is_null());

        // Empty address lists serialize as null (parser tolerates both).
        assert!(item.get("cc").unwrap().is_null());
        assert!(item.get("bcc").unwrap().is_null());

        // Header references are null when empty.
        assert!(item.get("inReplyTo").unwrap().is_null());
        assert!(item.get("references").unwrap().is_null());
        let mid = item.get("messageId").unwrap().as_array().unwrap();
        assert_eq!(mid[0], "<e1@example.com>");

        // Body part shape.
        let tb = item.get("textBody").unwrap().as_array().unwrap();
        assert_eq!(tb.len(), 1);
        let part = &tb[0];
        assert_eq!(part.get("partId").unwrap(), "e1:text");
        assert_eq!(part.get("type").unwrap(), "text/plain");
        assert_eq!(part.get("charset").unwrap(), "utf-8");
        assert_eq!(part.get("size").unwrap(), 5);
        assert!(item.get("htmlBody").unwrap().as_array().unwrap().is_empty());

        // bodyValues populated because fetchTextBodyValues was true.
        let bv = item.get("bodyValues").unwrap().as_object().unwrap();
        let entry = bv.get("e1:text").unwrap();
        assert_eq!(entry.get("value").unwrap(), "hello");
        assert_eq!(entry.get("isEncodingProblem").unwrap(), false);
        assert_eq!(entry.get("isTruncated").unwrap(), false);

        // Attachments empty array.
        assert_eq!(item.get("attachments").unwrap().as_array().unwrap().len(), 0);

        // The three custom-header keys are always present, always null.
        for k in [
            "header:List-Unsubscribe:asText",
            "header:List-Unsubscribe-Post:asText",
            "header:Disposition-Notification-To:asText",
        ] {
            assert!(item.get(k).unwrap().is_null(), "{k} not null");
        }
    }

    #[test]
    fn email_get_omits_body_values_when_not_requested() {
        let f = fix_for_get();
        let resp = email_get(&f, &json!({"accountId": "acct", "ids": ["e1"]})).unwrap();
        let item = &resp.get("list").unwrap().as_array().unwrap()[0];
        assert!(item.get("bodyValues").is_none());
    }

    #[test]
    fn email_get_reports_not_found() {
        let f = fix_for_get();
        let resp = email_get(
            &f,
            &json!({"accountId": "acct", "ids": ["e1", "ghost"]}),
        )
        .unwrap();
        assert_eq!(resp.get("list").unwrap().as_array().unwrap().len(), 1);
        let nf = resp.get("notFound").unwrap().as_array().unwrap();
        assert_eq!(nf, &vec![Value::String("ghost".into())]);
    }

    #[test]
    fn email_get_null_ids_returns_all() {
        let f = fix_for_get();
        let resp = email_get(&f, &json!({"accountId": "acct", "ids": null})).unwrap();
        assert_eq!(resp.get("list").unwrap().as_array().unwrap().len(), 1);
    }

    #[test]
    fn email_get_unknown_account_errors() {
        let f = fix_for_get();
        let err = email_get(&f, &json!({"accountId": "nope", "ids": []})).unwrap_err();
        assert_eq!(err.get("type").unwrap(), "accountNotFound");
    }
}
