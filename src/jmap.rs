//! JMAP request envelope, dispatcher, and per-method handlers.
//!
//! RFC 8620 §3.3 (request) and §3.4 (response). Method handlers live
//! here so `routes.rs` can stay just routing. The dispatcher returns
//! `("error", {...})` for unknown or per-call failures, per RFC §3.5.2.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::fixture::{Address, Attachment, Body, Disposition, Email, Fixture, Mailbox};
use crate::shared::FixtureHandle;

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
    fixture: &FixtureHandle,
    dispatcher: Option<&std::sync::Arc<crate::lua::Dispatcher>>,
    req: JmapRequest,
) -> JmapResponse {
    let mut responses = Vec::with_capacity(req.method_calls.len());
    // Prior results in this envelope, keyed by callId, so a later
    // method can resolve an RFC 8620 §3.7 back-reference against an
    // earlier response (e.g. `EmailSubmission/set`'s `#emailId`
    // resolving `/created/<id>/id` from the preceding `Email/set` or
    // `Email/import`). Cloned per call; envelopes are bounded at
    // `maxCallsInRequest` (16) so this stays cheap.
    let mut prior: Vec<(String, Value)> = Vec::with_capacity(req.method_calls.len());
    for (name, args, call_id) in req.method_calls {
        let (out_name, out_args) = dispatch(fixture, dispatcher, &name, &args, &prior);
        prior.push((call_id.clone(), out_args.clone()));
        responses.push((out_name, out_args, call_id));
    }
    let session_state = fixture
        .read()
        .expect("fixture lock poisoned")
        .primary_state()
        .to_string();
    JmapResponse {
        method_responses: responses,
        session_state,
        created_ids: req.created_ids,
    }
}

fn dispatch(
    fixture: &FixtureHandle,
    dispatcher: Option<&std::sync::Arc<crate::lua::Dispatcher>>,
    name: &str,
    args: &Value,
    prior: &[(String, Value)],
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
        let ids: Option<Vec<String>> = args.get("ids").and_then(Value::as_array).map(|arr| {
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

    // Acquire the appropriate guard *after* the dispatcher callback
    // returns (callbacks must never run with a fixture lock held - a
    // future hook that mutates would deadlock). Mutating methods
    // take a write guard for the duration of the handler; everything
    // else takes a brief read guard. Each method-call in a batched
    // envelope gets its own acquisition; readers and a writer
    // mutually exclude per-call only.
    if matches!(
        name,
        "Email/set"
            | "Mailbox/set"
            | "CalendarEvent/set"
            | "ContactCard/set"
            | "Email/import"
            | "EmailSubmission/set"
    ) {
        let mut fix = fixture.write().expect("fixture lock poisoned");
        let res = match name {
            "Email/set" => email_set(&mut fix, args),
            "Mailbox/set" => mailbox_set(&mut fix, args),
            "CalendarEvent/set" => crate::jmap_calendar::calendar_event_set(&mut fix, args),
            "ContactCard/set" => crate::jmap_contacts::contact_card_set(&mut fix, args),
            "Email/import" => email_import(&mut fix, args),
            "EmailSubmission/set" => email_submission_set(&mut fix, args, prior),
            _ => unreachable!(),
        };
        return match res {
            Ok(v) => (name.to_string(), v),
            Err(err) => ("error".to_string(), err),
        };
    }
    let fix = fixture.read().expect("fixture lock poisoned");
    match name {
        "Mailbox/get" => match mailbox_get(&fix, args) {
            Ok(v) => (name.to_string(), v),
            Err(err) => ("error".to_string(), err),
        },
        "Email/query" => match email_query(&fix, args) {
            Ok(v) => (name.to_string(), v),
            Err(err) => ("error".to_string(), err),
        },
        "Email/get" => match email_get(&fix, args) {
            Ok(v) => (name.to_string(), v),
            Err(err) => ("error".to_string(), err),
        },
        "Thread/get" => match thread_get(&fix, args) {
            Ok(v) => (name.to_string(), v),
            Err(err) => ("error".to_string(), err),
        },
        "Thread/changes" => match thread_changes(&fix, args) {
            Ok(v) => (name.to_string(), v),
            Err(err) => ("error".to_string(), err),
        },
        "AddressBook/get" => match crate::jmap_contacts::address_book_get(&fix, args) {
            Ok(v) => (name.to_string(), v),
            Err(err) => ("error".to_string(), err),
        },
        "ContactCard/get" => match crate::jmap_contacts::contact_card_get(&fix, args) {
            Ok(v) => (name.to_string(), v),
            Err(err) => ("error".to_string(), err),
        },
        "ContactCard/query" => match crate::jmap_contacts::contact_card_query(&fix, args) {
            Ok(v) => (name.to_string(), v),
            Err(err) => ("error".to_string(), err),
        },
        "ContactCard/changes" => match crate::jmap_contacts::contact_card_changes(&fix, args) {
            Ok(v) => (name.to_string(), v),
            Err(err) => ("error".to_string(), err),
        },
        "Principal/get" => match crate::jmap_principals::principal_get(&fix, args) {
            Ok(v) => (name.to_string(), v),
            Err(err) => ("error".to_string(), err),
        },
        "Mailbox/changes" => match mailbox_changes(&fix, args) {
            Ok(v) => (name.to_string(), v),
            Err(err) => ("error".to_string(), err),
        },
        "Email/changes" => match email_changes(&fix, args) {
            Ok(v) => (name.to_string(), v),
            Err(err) => ("error".to_string(), err),
        },
        "Calendar/get" => match crate::jmap_calendar::calendar_get(&fix, args) {
            Ok(v) => (name.to_string(), v),
            Err(err) => ("error".to_string(), err),
        },
        "Calendar/changes" => match crate::jmap_calendar::calendar_changes(&fix, args) {
            Ok(v) => (name.to_string(), v),
            Err(err) => ("error".to_string(), err),
        },
        "CalendarEvent/get" => match crate::jmap_calendar::calendar_event_get(&fix, args) {
            Ok(v) => (name.to_string(), v),
            Err(err) => ("error".to_string(), err),
        },
        "CalendarEvent/query" => match crate::jmap_calendar::calendar_event_query(&fix, args) {
            Ok(v) => (name.to_string(), v),
            Err(err) => ("error".to_string(), err),
        },
        "CalendarEvent/changes" => match crate::jmap_calendar::calendar_event_changes(&fix, args) {
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
    let account_id = args
        .get("accountId")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            json!({
                "type": "invalidArguments",
                "description": "missing accountId",
            })
        })?;
    if fixture.account(account_id).is_none() {
        return Err(json!({
            "type": "accountNotFound",
            "description": format!("account {account_id:?} not found"),
        }));
    }

    let (list, not_found) = match args.get("ids") {
        None | Some(Value::Null) => {
            let all = fixture
                .mailboxes_for(account_id)
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
                match fixture.mailboxes_for(account_id).find(|m| m.id == id) {
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
        Value::String(account_id.to_string()),
    );
    out.insert(
        "state".to_string(),
        Value::String(fixture.state_for(account_id).to_string()),
    );
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
    obj.insert(
        "totalEmails".to_string(),
        Value::Number(total_emails.into()),
    );
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
//
// RFC conformance points verified during the 2026-05-09 review and
// preserved here so a regression trips a known assertion:
//
//   - `Email/changes` always emits `updatedProperties: null`. RFC
//     §4.2 lets the server set this to `null` when it cannot
//     determine which subset of properties changed; "all
//     properties" is the safe fallback.
//   - `Mailbox/changes` deliberately *omits* `updatedProperties`;
//     RFC §2.5 does not define that field for mailboxes.
//   - Missing `sinceState` returns `invalidArguments` per RFC
//     §3.2 (required-arg violation).
//   - Mismatched `sinceState` returns `cannotCalculateChanges`
//     per RFC §3.2 (the canonical "client must full-resync"
//     signal).
//   - Unknown `accountId` returns `accountNotFound`.

fn mailbox_changes(fixture: &Fixture, args: &Value) -> Result<Value, Value> {
    let account_id = require_account(fixture, args)?;
    let since_state = require_since_state(args)?;
    let delta = fixture
        .mailbox_delta_since_account(since_state, account_id)
        .ok_or_else(|| {
            json!({
                "type": "cannotCalculateChanges",
                "description": format!(
                    "sinceState {since_state:?} is not a known fixture state \
                     (older than the seed or evicted from the bounded change log)"
                ),
            })
        })?;
    Ok(changes_response(
        account_id,
        since_state,
        fixture.state_for(account_id),
        delta,
        false,
    ))
}

fn email_changes(fixture: &Fixture, args: &Value) -> Result<Value, Value> {
    let account_id = require_account(fixture, args)?;
    let since_state = require_since_state(args)?;
    let delta = fixture
        .email_delta_since_account(since_state, account_id)
        .ok_or_else(|| {
            json!({
                "type": "cannotCalculateChanges",
                "description": format!(
                    "sinceState {since_state:?} is not a known fixture state \
                     (older than the seed or evicted from the bounded change log)"
                ),
            })
        })?;
    Ok(changes_response(
        account_id,
        since_state,
        fixture.state_for(account_id),
        delta,
        true,
    ))
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
    if fixture.account(account_id).is_none() {
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

/// Build the changes response envelope shared by `Mailbox/changes`
/// and `Email/changes`. `is_email` toggles the `updatedProperties:
/// null` field, which is `Email/changes`-only per RFC 8621 §4.2.
fn changes_response(
    account_id: &str,
    old_state: &str,
    new_state: &str,
    delta: crate::fixture::DeltaSet,
    is_email: bool,
) -> Value {
    let mut out = Map::new();
    out.insert(
        "accountId".to_string(),
        Value::String(account_id.to_string()),
    );
    out.insert("oldState".to_string(), Value::String(old_state.to_string()));
    out.insert("newState".to_string(), Value::String(new_state.to_string()));
    out.insert("hasMoreChanges".to_string(), Value::Bool(false));
    out.insert(
        "created".to_string(),
        Value::Array(delta.created.into_iter().map(Value::String).collect()),
    );
    out.insert(
        "updated".to_string(),
        Value::Array(delta.updated.into_iter().map(Value::String).collect()),
    );
    out.insert(
        "destroyed".to_string(),
        Value::Array(delta.destroyed.into_iter().map(Value::String).collect()),
    );
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
    let account_id = args
        .get("accountId")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            json!({
                "type": "invalidArguments",
                "description": "missing accountId",
            })
        })?;
    if fixture.account(account_id).is_none() {
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
        .emails_for(account_id)
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
        Value::String(account_id.to_string()),
    );
    out.insert(
        "queryState".to_string(),
        Value::String(fixture.state_for(account_id).to_string()),
    );
    out.insert("canCalculateChanges".to_string(), Value::Bool(false));
    out.insert("position".to_string(), Value::Number(position.into()));
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
    let account_id = args
        .get("accountId")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            json!({
                "type": "invalidArguments",
                "description": "missing accountId",
            })
        })?;
    if fixture.account(account_id).is_none() {
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
                match fixture.emails_for(account_id).find(|e| e.id == id) {
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
                .emails_for(account_id)
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
        Value::String(account_id.to_string()),
    );
    out.insert(
        "state".to_string(),
        Value::String(fixture.state_for(account_id).to_string()),
    );
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
            e.received_at
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        ),
    );
    obj.insert(
        "sentAt".to_string(),
        Value::String(e.sent_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
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
    obj.insert("type".to_string(), Value::String(a.content_type.clone()));
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
    // Resolve the body string. When the fixture set `raw_bytes`, the
    // wire body is whatever bytes the author wrote, lossy-decoded as
    // UTF-8 - JMAP `bodyValues[*].value` is a JSON string, so any
    // ill-formed sequence collapses to U+FFFD. Lets a fixture inject
    // anomalous body content (CRLF-only, bare-LF, 8-bit, truncated
    // shapes) through the JMAP projection in addition to the
    // already-honoured IMAP path.
    let body_str: std::borrow::Cow<'_, str> = match (e.raw_bytes.as_deref(), &e.body) {
        (Some(raw), _) => std::borrow::Cow::Owned(raw.to_string()),
        (None, Body::Text(text)) => std::borrow::Cow::Borrowed(text),
    };

    let part_id = format!("{}:text", e.id);
    let blob_id = format!("blob-{}-text", e.id);
    let size = i64::try_from(body_str.len()).unwrap_or(i64::MAX);
    let part = body_part_text(&part_id, &blob_id, size);
    let text_body = Value::Array(vec![part]);
    let html_body = Value::Array(vec![]);
    let body_values = if fetch_text || fetch_all {
        let mut bv = Map::new();
        bv.insert(
            part_id,
            json!({
                "value": body_str.as_ref(),
                "isEncodingProblem": false,
                "isTruncated": false,
            }),
        );
        Some(Value::Object(bv))
    } else if fetch_html {
        Some(Value::Object(Map::new()))
    } else {
        None
    };
    (text_body, html_body, body_values)
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

// ── Thread/get ──────────────────────────────────────────────────────
//
// RFC 8621 §3. A Thread groups every Email that shares a `threadId`;
// the object is `{ id, emailIds }`. Bifrost's JMAP `Account::open`
// probes `Thread/get` during account discovery - before this landed
// the dispatcher returned `unknownMethod` and the account failed to
// open with `Wire(Jmap(UnknownMethod))`, so the whole sync never
// started. The mock derives threads from the fixture's existing
// per-email `thread_id`; there is no separate thread resource to
// author. `state` reuses the fixture-level state token, like the
// other `/get` responses. Reads scope to the request's `accountId`
// so a multi-account fixture's secondary threads don't leak.

/// RFC 8621 §3.1. `ids = null` lists every thread in the account;
/// an explicit id array returns matched threads with unknown ids in
/// `notFound`.
fn thread_get(fixture: &Fixture, args: &Value) -> Result<Value, Value> {
    let account_id = args
        .get("accountId")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            json!({
                "type": "invalidArguments",
                "description": "missing accountId",
            })
        })?;
    if fixture.account(account_id).is_none() {
        return Err(json!({
            "type": "accountNotFound",
            "description": format!("account {account_id:?} not found"),
        }));
    }

    let (list, not_found) = match args.get("ids") {
        None | Some(Value::Null) => {
            // RFC: null = all threads. Distinct threadIds in the
            // account, sorted for byte-stable ordering.
            let mut threads: Vec<&str> = fixture
                .emails_for(account_id)
                .map(|e| e.thread_id.as_str())
                .collect();
            threads.sort_unstable();
            threads.dedup();
            let all = threads
                .into_iter()
                .map(|tid| serialize_thread(fixture, account_id, tid))
                .collect::<Vec<_>>();
            (Value::Array(all), Value::Array(vec![]))
        }
        Some(Value::Array(requested)) => {
            let mut list = Vec::with_capacity(requested.len());
            let mut not_found = Vec::new();
            for v in requested {
                let Some(tid) = v.as_str() else {
                    return Err(json!({
                        "type": "invalidArguments",
                        "description": "ids must be an array of strings",
                    }));
                };
                if fixture.emails_for(account_id).any(|e| e.thread_id == tid) {
                    list.push(serialize_thread(fixture, account_id, tid));
                } else {
                    not_found.push(Value::String(tid.to_string()));
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
        Value::String(account_id.to_string()),
    );
    out.insert(
        "state".to_string(),
        Value::String(fixture.state_for(account_id).to_string()),
    );
    out.insert("list".to_string(), list);
    out.insert("notFound".to_string(), not_found);
    Ok(Value::Object(out))
}

/// One Thread object: `emailIds` are the account's emails in this
/// thread, sorted by `receivedAt` ascending (RFC 8621 §3) with `id`
/// lexicographic as a deterministic tiebreak.
fn serialize_thread(fixture: &Fixture, account_id: &str, thread_id: &str) -> Value {
    let mut emails: Vec<&Email> = fixture
        .emails_for(account_id)
        .filter(|e| e.thread_id == thread_id)
        .collect();
    emails.sort_by(|a, b| {
        a.received_at
            .cmp(&b.received_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    let email_ids: Vec<Value> = emails.iter().map(|e| Value::String(e.id.clone())).collect();

    let mut obj = Map::new();
    obj.insert("id".to_string(), Value::String(thread_id.to_string()));
    obj.insert("emailIds".to_string(), Value::Array(email_ids));
    Value::Object(obj)
}

// ── Thread/changes ──────────────────────────────────────────────────
//
// RFC 8621 §3.2. bifrost seeds a Thread cursor at account open
// (`sync/factory.rs`) and drives `Thread/changes` for it on the first
// delta cycle right after open (`sync/changes.rs`); before this
// landed the dispatcher returned `unknownMethod`, terminating the
// Thread sync scope.
//
// The mock has no thread-specific change log - threads are derived
// from each email's `thread_id`. So we project the per-account email
// delta onto threads: a thread is reported whenever one of its emails
// changed in the window. Threads of created emails land in `created`,
// threads of updated emails in `updated` (deduped; a thread already
// in `created` is not repeated). `destroyed` is always empty: a
// destroyed email's `thread_id` is unrecoverable from the log (the
// email is gone), so v0 does not synthesise thread tombstones -
// bifrost re-fetches the reported threads via `Thread/get` and
// reconciles membership, which collapses an emptied thread to an
// empty `emailIds` rather than a tombstone. Unknown / evicted
// `sinceState` returns `cannotCalculateChanges`, same as
// `Email/changes`.
fn thread_changes(fixture: &Fixture, args: &Value) -> Result<Value, Value> {
    let account_id = require_account(fixture, args)?;
    let since_state = require_since_state(args)?;
    let delta = fixture
        .email_delta_since_account(since_state, account_id)
        .ok_or_else(|| {
            json!({
                "type": "cannotCalculateChanges",
                "description": format!(
                    "sinceState {since_state:?} is not a known fixture state \
                     (older than the seed or evicted from the bounded change log)"
                ),
            })
        })?;

    let thread_of = |email_id: &str| {
        fixture
            .emails
            .iter()
            .find(|e| e.id == email_id)
            .map(|e| e.thread_id.clone())
    };

    let mut created: Vec<String> = Vec::new();
    for id in &delta.created {
        if let Some(t) = thread_of(id)
            && !created.contains(&t)
        {
            created.push(t);
        }
    }
    let mut updated: Vec<String> = Vec::new();
    for id in &delta.updated {
        if let Some(t) = thread_of(id)
            && !created.contains(&t)
            && !updated.contains(&t)
        {
            updated.push(t);
        }
    }

    let mut out = Map::new();
    out.insert(
        "accountId".to_string(),
        Value::String(account_id.to_string()),
    );
    out.insert(
        "oldState".to_string(),
        Value::String(since_state.to_string()),
    );
    out.insert(
        "newState".to_string(),
        Value::String(fixture.state_for(account_id).to_string()),
    );
    out.insert("hasMoreChanges".to_string(), Value::Bool(false));
    out.insert(
        "created".to_string(),
        Value::Array(created.into_iter().map(Value::String).collect()),
    );
    out.insert(
        "updated".to_string(),
        Value::Array(updated.into_iter().map(Value::String).collect()),
    );
    out.insert("destroyed".to_string(), Value::Array(vec![]));
    Ok(Value::Object(out))
}

// ── Email/set ───────────────────────────────────────────────────────
//
// RFC 8621 §4.6. The v0 surface is deliberately narrow: it accepts
// the create / update / destroy maps ratatoskr's writeback path
// emits, applies them to the fixture, and bumps the state token so
// `Email/changes` can return a real delta on the next call.
//
// Update patches honour the two property paths ratatoskr touches:
//   - `keywords` (full replace) and `keywords/<name>` (true=add,
//     null=remove). RFC 8621 §4.1.1: keywords are a `String[Boolean]`
//     map. The `unread` synthetic property is *not* exposed - the
//     client toggles `$seen` directly via `keywords`.
//   - `mailboxIds` (full replace) and `mailboxIds/<id>` (true=add,
//     null=remove). RFC 8621 §4.1.1: a Set<Id> represented as
//     `String[true]`.
// Anything else is rejected as `notUpdated[<id>] = invalidProperties`.
//
// Create / destroy are full-replace and ID-removal respectively;
// they round-trip through `Email/changes` with no extra trickery.

fn email_set(fixture: &mut Fixture, args: &Value) -> Result<Value, Value> {
    let account_id = require_account(fixture, args)?.to_string();
    if let Some(if_in_state) = args.get("ifInState").and_then(Value::as_str)
        && if_in_state != fixture.state_for(&account_id)
    {
        return Err(json!({
            "type": "stateMismatch",
            "description": format!(
                "ifInState {if_in_state:?} does not match current state {:?}",
                fixture.state_for(&account_id),
            ),
        }));
    }
    let creates = args
        .get("create")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let updates = args
        .get("update")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let destroys: Vec<String> = args
        .get("destroy")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let mut created_out = Map::new();
    let mut not_created = Map::new();
    let mut updated_out = Map::new();
    let mut not_updated = Map::new();
    let mut destroyed_out: Vec<String> = Vec::new();
    let mut not_destroyed = Map::new();

    // Capture this account's state token before and after the
    // mutation: the aggregate `Transition` reports the *primary*
    // account's tokens, so a write against a secondary account must
    // read its own per-account token through `state_for`.
    let old_state = fixture.state_for(&account_id).to_string();
    fixture.mutate(|fix| {
        let mut diff = crate::fixture::MutationDiff::default();

        // Creates: assign a server id and append. Default field
        // values are filled from the fixture's existing rendering
        // (zero-length subject, empty body, no attachments) so
        // tests that round-trip a brand-new email can rely on a
        // stable shape.
        for (client_id, body) in &creates {
            match build_email_from_create(fix, body) {
                Ok((server_id, email)) => {
                    let mailboxes = email.mailbox_ids.clone();
                    fix.emails.push(email);
                    for mb in &mailboxes {
                        fix.assign_uid(mb, server_id.clone());
                    }
                    diff.email_created.push(server_id.clone());
                    created_out.insert(
                        client_id.clone(),
                        json!({
                            "id": server_id,
                            "blobId": format!("blob-{server_id}"),
                            "threadId": server_id,
                            "size": 0,
                        }),
                    );
                }
                Err(err) => {
                    not_created.insert(client_id.clone(), err);
                }
            }
        }

        // Updates: locate by id, apply the patch, record. We mutate
        // a clone first so a partial failure on one property leaves
        // the original untouched; on success we swap the clone back
        // in. Build an id -> idx map once; per-update `position`
        // would otherwise be O(U · N).
        let id_to_idx: std::collections::HashMap<String, usize> = fix
            .emails
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
        for (id, patch) in &updates {
            let Some(&idx) = id_to_idx.get(id) else {
                not_updated.insert(id.clone(), set_error("notFound", "no such email"));
                continue;
            };
            let old_mailboxes = fix.emails[idx].mailbox_ids.clone();
            let mut clone = fix.emails[idx].clone();
            match apply_email_patch(&mut clone, patch) {
                Ok(()) => {
                    let new_mailboxes = clone.mailbox_ids.clone();
                    fix.emails[idx] = clone;
                    fix.sync_mailbox_uids(id, &old_mailboxes, &new_mailboxes);
                    diff.email_updated.push(id.clone());
                    updated_out.insert(id.clone(), Value::Null);
                }
                Err(err) => {
                    not_updated.insert(id.clone(), err);
                }
            }
        }

        // Destroys: snapshot mailbox memberships + account for every
        // id we're about to drop, then a single-pass retain. Pre-fix
        // this was an O(D · N) loop of `position` + `remove`. Walking
        // the request `destroys` afterwards preserves response order.
        // The account snapshot lands in `email_destroyed_accounts`
        // so the per-account `Email/changes` walker can filter
        // tombstones.
        let destroy_set: std::collections::HashSet<&str> =
            destroys.iter().map(String::as_str).collect();
        let memberships: std::collections::HashMap<String, (Vec<String>, String)> = fix
            .emails
            .iter()
            .filter(|e| destroy_set.contains(e.id.as_str()))
            .map(|e| (e.id.clone(), (e.mailbox_ids.clone(), e.account_id.clone())))
            .collect();
        fix.emails.retain(|e| !destroy_set.contains(e.id.as_str()));
        for id in &destroys {
            match memberships.get(id) {
                Some((mailboxes, account_id)) => {
                    for mb in mailboxes {
                        fix.retire_uid(mb, id);
                    }
                    diff.email_destroyed.push(id.clone());
                    diff.email_destroyed_accounts.push(account_id.clone());
                    destroyed_out.push(id.clone());
                }
                None => {
                    not_destroyed.insert(id.clone(), set_error("notFound", "no such email"));
                }
            }
        }

        diff
    });
    let new_state = fixture.state_for(&account_id).to_string();

    Ok(set_response(
        &account_id,
        &old_state,
        &new_state,
        created_out,
        not_created,
        updated_out,
        not_updated,
        destroyed_out,
        not_destroyed,
    ))
}

fn build_email_from_create(fix: &mut Fixture, body: &Value) -> Result<(String, Email), Value> {
    let body = body
        .as_object()
        .ok_or_else(|| set_error("invalidProperties", "create body must be an object"))?;
    let mailbox_ids: Vec<String> = body
        .get("mailboxIds")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .ok_or_else(|| set_error("invalidProperties", "missing mailboxIds"))?;
    for id in &mailbox_ids {
        if !fix.mailboxes.iter().any(|m| &m.id == id) {
            return Err(set_error(
                "invalidProperties",
                &format!("unknown mailboxId {id:?}"),
            ));
        }
    }
    let keywords: Vec<String> = body
        .get("keywords")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    let server_id = fix.mint_email_id();
    // Determinism: a create-into-empty-fixture must produce the
    // same timestamp every run. The fixture has no clock; we anchor
    // to a fixed epoch so byte-stable transcripts hold even when
    // the create path fires from a fresh fixture.
    let received_at = fix
        .emails
        .iter()
        .map(|e| e.received_at)
        .max()
        .unwrap_or_else(|| {
            chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .expect("hardcoded RFC3339")
                .with_timezone(&chrono::Utc)
        });
    // Email/set create is scoped to the primary account in v0;
    // mailboxIds in the body must belong to it. Stage 3 of the
    // multi-account refactor lets JMAP route mutations into a
    // non-primary account through the request's accountId.
    let account_id = fix
        .mailboxes
        .iter()
        .find(|m| mailbox_ids.iter().any(|id| id == &m.id))
        .map(|m| m.account_id.clone())
        .unwrap_or_else(|| fix.primary_account().id.clone());
    // Project the structured compose properties bifrost's client-send path
    // sets on the `Email/set` create (RFC 8621 §4.6): subject, the address
    // lists, and the body. Without this a sent message round-trips with an
    // empty subject / no sender, so a send-writeback gate cannot identify it.
    let subject = body
        .get("subject")
        .and_then(Value::as_str)
        .map(std::string::ToString::to_string);
    let from = jmap_address_list(body, "from").into_iter().next();
    let to = jmap_address_list(body, "to");
    let cc = jmap_address_list(body, "cc");
    let bcc = jmap_address_list(body, "bcc");
    let reply_to = jmap_address_list(body, "replyTo");
    let message_id = body
        .get("messageId")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .map(std::string::ToString::to_string)
                .collect()
        })
        .unwrap_or_default();
    let body_text = jmap_text_body(body);

    let email = Email {
        id: server_id.clone(),
        account_id,
        thread_id: server_id.clone(),
        mailbox_ids,
        keywords,
        size: i64::try_from(body_text.len()).unwrap_or(i64::MAX),
        received_at,
        sent_at: received_at,
        from,
        to,
        cc,
        bcc,
        reply_to,
        subject,
        preview: None,
        message_id,
        in_reply_to: vec![],
        references: vec![],
        has_attachment: false,
        body: crate::fixture::Body::Text(body_text),
        attachments: vec![],
        raw_bytes: None,
        reaction_type: None,
        reaction_count: None,
    };
    Ok((server_id, email))
}

/// Parse a JMAP `EmailAddress[]` property (`from` / `to` / `cc` / `bcc` /
/// `replyTo`) - each entry `{ "name": <string|null>, "email": <string> }`
/// (RFC 8621 §4.1.2) - into the fixture address shape, dropping entries with
/// no `email`.
fn jmap_address_list(body: &Map<String, Value>, key: &str) -> Vec<Address> {
    body.get(key)
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let email = entry.get("email").and_then(Value::as_str)?;
                    Some(Address {
                        name: entry
                            .get("name")
                            .and_then(Value::as_str)
                            .map(std::string::ToString::to_string),
                        email: email.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve the plain-text body of an `Email/set` create. The compose path
/// carries `bodyValues` keyed by partId plus a `textBody` part list; prefer
/// the part `textBody[0]` references, else the first available value.
fn jmap_text_body(body: &Map<String, Value>) -> String {
    let Some(values) = body.get("bodyValues").and_then(Value::as_object) else {
        return String::new();
    };
    let part_id = body
        .get("textBody")
        .and_then(Value::as_array)
        .and_then(|parts| parts.first())
        .and_then(|part| part.get("partId"))
        .and_then(Value::as_str);
    part_id
        .and_then(|pid| values.get(pid))
        .or_else(|| values.values().next())
        .and_then(|value| value.get("value"))
        .and_then(Value::as_str)
        .map(std::string::ToString::to_string)
        .unwrap_or_default()
}

pub(crate) fn apply_email_patch(email: &mut Email, patch: &Value) -> Result<(), Value> {
    let obj = patch
        .as_object()
        .ok_or_else(|| set_error("invalidProperties", "patch must be an object"))?;
    for (path, value) in obj {
        match path.as_str() {
            "keywords" => {
                let map = value
                    .as_object()
                    .ok_or_else(|| set_error("invalidProperties", "keywords must be an object"))?;
                email.keywords = map.keys().cloned().collect();
            }
            "mailboxIds" => {
                let map = value.as_object().ok_or_else(|| {
                    set_error("invalidProperties", "mailboxIds must be an object")
                })?;
                email.mailbox_ids = map.keys().cloned().collect();
            }
            other if other.starts_with("keywords/") => {
                let name = other.strip_prefix("keywords/").expect("checked");
                if value.is_null() {
                    email.keywords.retain(|k| k != name);
                } else if value.as_bool() == Some(true) {
                    if !email.keywords.iter().any(|k| k == name) {
                        email.keywords.push(name.to_string());
                    }
                } else {
                    return Err(set_error(
                        "invalidProperties",
                        &format!("{path} expects true or null"),
                    ));
                }
            }
            other if other.starts_with("mailboxIds/") => {
                let id = other.strip_prefix("mailboxIds/").expect("checked");
                if value.is_null() {
                    email.mailbox_ids.retain(|m| m != id);
                } else if value.as_bool() == Some(true) {
                    if !email.mailbox_ids.iter().any(|m| m == id) {
                        email.mailbox_ids.push(id.to_string());
                    }
                } else {
                    return Err(set_error(
                        "invalidProperties",
                        &format!("{path} expects true or null"),
                    ));
                }
            }
            other => {
                return Err(set_error(
                    "invalidProperties",
                    &format!("v0 mock does not implement patch path {other:?}"),
                ));
            }
        }
    }
    Ok(())
}

// ── Email/import ────────────────────────────────────────────────────
//
// RFC 8621 §4.8. bifrost's raw-send path uploads the assembled RFC 822
// octets as a blob (`Blob/upload`), imports them into the Drafts
// mailbox here, then submits via `EmailSubmission/set`. The mock pulls
// the uploaded bytes back out of `Fixture::uploaded_blobs`, parses them
// with `mail-parser` to project subject / from / to / message-id / body
// onto the new `Email`, and keeps the raw octets on the email's
// `raw_bytes` so the IMAP + JMAP body surfaces echo exactly what was
// sent. Response shape mirrors `Email/set`'s created map so bifrost's
// `EmailImportResponse` deserializes (`created.<id>.id`).
fn email_import(fixture: &mut Fixture, args: &Value) -> Result<Value, Value> {
    let account_id = require_account(fixture, args)?.to_string();
    let emails = args
        .get("emails")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let mut created_out = Map::new();
    let mut not_created = Map::new();

    let old_state = fixture.state_for(&account_id).to_string();
    fixture.mutate(|fix| {
        let mut diff = crate::fixture::MutationDiff::default();
        for (client_id, body) in &emails {
            match build_email_from_import(fix, body) {
                Ok((server_id, email)) => {
                    let size = email.size;
                    let mailboxes = email.mailbox_ids.clone();
                    fix.emails.push(email);
                    for mb in &mailboxes {
                        fix.assign_uid(mb, server_id.clone());
                    }
                    diff.email_created.push(server_id.clone());
                    created_out.insert(
                        client_id.clone(),
                        json!({
                            "id": server_id,
                            "blobId": format!("blob-{server_id}"),
                            "threadId": server_id,
                            "size": size,
                        }),
                    );
                }
                Err(err) => {
                    not_created.insert(client_id.clone(), err);
                }
            }
        }
        diff
    });
    let new_state = fixture.state_for(&account_id).to_string();

    let mut out = Map::new();
    out.insert("accountId".to_string(), Value::String(account_id.clone()));
    out.insert("oldState".to_string(), Value::String(old_state));
    out.insert("newState".to_string(), Value::String(new_state));
    out.insert(
        "created".to_string(),
        if created_out.is_empty() {
            Value::Null
        } else {
            Value::Object(created_out)
        },
    );
    out.insert(
        "notCreated".to_string(),
        if not_created.is_empty() {
            Value::Null
        } else {
            Value::Object(not_created)
        },
    );
    Ok(Value::Object(out))
}

/// Build a fixture `Email` from an `Email/import` entry: resolve the
/// `blobId` to previously-uploaded bytes, parse them, and project the
/// structured fields. The mailboxIds / keywords come from the import
/// entry (Drafts + `$draft` in bifrost's flow).
fn build_email_from_import(fix: &mut Fixture, body: &Value) -> Result<(String, Email), Value> {
    let body = body
        .as_object()
        .ok_or_else(|| set_error("invalidProperties", "import entry must be an object"))?;
    let blob_id = body
        .get("blobId")
        .and_then(Value::as_str)
        .ok_or_else(|| set_error("invalidProperties", "missing blobId"))?;
    let raw = fix
        .uploaded_blobs
        .get(blob_id)
        .map(|b| b.data.clone())
        .ok_or_else(|| set_error("blobNotFound", &format!("unknown blobId {blob_id:?}")))?;
    let mailbox_ids: Vec<String> = body
        .get("mailboxIds")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .ok_or_else(|| set_error("invalidProperties", "missing mailboxIds"))?;
    for id in &mailbox_ids {
        if !fix.mailboxes.iter().any(|m| &m.id == id) {
            return Err(set_error(
                "invalidProperties",
                &format!("unknown mailboxId {id:?}"),
            ));
        }
    }
    let keywords: Vec<String> = body
        .get("keywords")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();

    let account_id = fix
        .mailboxes
        .iter()
        .find(|m| mailbox_ids.iter().any(|id| id == &m.id))
        .map(|m| m.account_id.clone())
        .unwrap_or_else(|| fix.primary_account().id.clone());
    let received_at = fix
        .emails
        .iter()
        .map(|e| e.received_at)
        .max()
        .unwrap_or_else(|| {
            chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .expect("hardcoded RFC3339")
                .with_timezone(&chrono::Utc)
        });
    let server_id = fix.mint_email_id();
    let parsed = crate::fixture::parse_rfc822_email(&raw);
    let raw_string = String::from_utf8_lossy(&raw).into_owned();
    let email = Email {
        id: server_id.clone(),
        account_id,
        thread_id: server_id.clone(),
        mailbox_ids,
        keywords,
        size: i64::try_from(raw.len()).unwrap_or(i64::MAX),
        received_at,
        sent_at: received_at,
        from: parsed.from,
        to: parsed.to,
        cc: vec![],
        bcc: vec![],
        reply_to: vec![],
        subject: parsed.subject,
        preview: None,
        message_id: parsed.message_id,
        in_reply_to: vec![],
        references: vec![],
        has_attachment: false,
        body: crate::fixture::Body::Text(parsed.body_text.unwrap_or_default()),
        attachments: vec![],
        raw_bytes: Some(raw_string),
        reaction_type: None,
        reaction_count: None,
    };
    Ok((server_id, email))
}

// ── EmailSubmission/set ─────────────────────────────────────────────
//
// RFC 8621 §7. The submission-capable surface bifrost's B5 send cut
// drives. A `create` marks a referenced `Email` submitted and delivers
// it: the server-side `onSuccessUpdateEmail` patch files the message in
// Sent (move out of Drafts), or `onSuccessDestroyEmail` discards it
// when the caller opted out of saving. The `emailId` is either explicit
// (draft-send) or an RFC 8620 §3.7 back-reference (`#emailId`) resolved
// against the preceding `Email/set` (compose) / `Email/import` (raw)
// response in the same envelope.
//
// Scheduled send (`scheduled_send`): the envelope's
// `mailFrom.parameters.holduntil` (SMTP FUTURERELEASE) is accepted and
// acknowledged - the mock has no clock, so delivery is immediate, but
// the submission succeeds rather than erroring so the delegated
// scheduled path is gateable. `update` (cancel / reschedule via
// `undoStatus`) is accepted and echoed.
//
// The created submission id is deterministic (`submission-<emailId>`)
// so a follow-up cancel/reschedule can address it. bifrost reads
// `created.<createId>.id` (and, for a scheduled send, returns it as the
// undo handle).
fn email_submission_set(
    fixture: &mut Fixture,
    args: &Value,
    prior: &[(String, Value)],
) -> Result<Value, Value> {
    let account_id = require_account(fixture, args)?.to_string();
    let creates = args
        .get("create")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let updates = args
        .get("update")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let on_success_update = args
        .get("onSuccessUpdateEmail")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let on_success_destroy: Vec<String> = args
        .get("onSuccessDestroyEmail")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let mut created_out = Map::new();
    let mut not_created = Map::new();
    let mut updated_out = Map::new();

    let old_state = fixture.state_for(&account_id).to_string();
    fixture.mutate(|fix| {
        let mut diff = crate::fixture::MutationDiff::default();
        for (create_id, sub) in &creates {
            let Some(email_id) = resolve_submission_email_id(sub, prior) else {
                not_created.insert(
                    create_id.clone(),
                    set_error("invalidProperties", "missing or unresolvable emailId"),
                );
                continue;
            };
            if !fix.emails.iter().any(|e| e.id == email_id) {
                not_created.insert(
                    create_id.clone(),
                    set_error(
                        "invalidProperties",
                        &format!("emailId {email_id:?} not found"),
                    ),
                );
                continue;
            }
            // Server-side onSuccess action, keyed by this submission's
            // create-id with the RFC 8621 `#` create-reference prefix.
            let key = format!("#{create_id}");
            if let Some(patch) = on_success_update.get(&key) {
                if let Some(idx) = fix.emails.iter().position(|e| e.id == email_id) {
                    let old_mailboxes = fix.emails[idx].mailbox_ids.clone();
                    let mut clone = fix.emails[idx].clone();
                    if apply_email_patch(&mut clone, patch).is_ok() {
                        let new_mailboxes = clone.mailbox_ids.clone();
                        fix.emails[idx] = clone;
                        fix.sync_mailbox_uids(&email_id, &old_mailboxes, &new_mailboxes);
                        diff.email_updated.push(email_id.clone());
                    }
                }
            } else if on_success_destroy.contains(&key)
                && let Some(idx) = fix.emails.iter().position(|e| e.id == email_id)
            {
                let mailboxes = fix.emails[idx].mailbox_ids.clone();
                let owner = fix.emails[idx].account_id.clone();
                fix.emails.remove(idx);
                for mb in &mailboxes {
                    fix.retire_uid(mb, &email_id);
                }
                diff.email_destroyed.push(email_id.clone());
                diff.email_destroyed_accounts.push(owner);
            }
            let undo_status = sub
                .get("undoStatus")
                .and_then(Value::as_str)
                .unwrap_or("final");
            created_out.insert(
                create_id.clone(),
                json!({
                    "id": format!("submission-{email_id}"),
                    "emailId": email_id,
                    "undoStatus": undo_status,
                }),
            );
        }
        diff
    });
    let new_state = fixture.state_for(&account_id).to_string();

    // Cancel / reschedule come through as `update` blocks patching
    // `undoStatus`; the mock has no real queue, so it acknowledges them
    // without further state change.
    for id in updates.keys() {
        updated_out.insert(id.clone(), Value::Null);
    }

    Ok(set_response(
        &account_id,
        &old_state,
        &new_state,
        created_out,
        not_created,
        updated_out,
        Map::new(),
        vec![],
        Map::new(),
    ))
}

/// Resolve an `EmailSubmission` create's target email id: an explicit
/// `emailId`, else the RFC 8620 §3.7 back-reference `#emailId`
/// (`{ resultOf, name, path }`) evaluated as a JSON pointer against the
/// referenced earlier response in this envelope.
fn resolve_submission_email_id(sub: &Value, prior: &[(String, Value)]) -> Option<String> {
    if let Some(id) = sub.get("emailId").and_then(Value::as_str) {
        return Some(id.to_string());
    }
    let reference = sub.get("#emailId")?;
    let result_of = reference.get("resultOf").and_then(Value::as_str)?;
    let path = reference.get("path").and_then(Value::as_str)?;
    let result = prior
        .iter()
        .find(|(call_id, _)| call_id == result_of)
        .map(|(_, value)| value)?;
    result
        .pointer(path)
        .and_then(Value::as_str)
        .map(str::to_string)
}

// ── Mailbox/set ─────────────────────────────────────────────────────
//
// RFC 8621 §2.5. v0 supports the `name`, `parentId`, `sortOrder`,
// `role`, and `isSubscribed` properties on create + update. The
// counter / sub-resource fields (`totalEmails`, `unreadEmails`, ...)
// are derived at read time and cannot be patched.

fn mailbox_set(fixture: &mut Fixture, args: &Value) -> Result<Value, Value> {
    let account_id = require_account(fixture, args)?.to_string();
    if let Some(if_in_state) = args.get("ifInState").and_then(Value::as_str)
        && if_in_state != fixture.state_for(&account_id)
    {
        return Err(json!({
            "type": "stateMismatch",
            "description": format!(
                "ifInState {if_in_state:?} does not match current state {:?}",
                fixture.state_for(&account_id),
            ),
        }));
    }
    let creates = args
        .get("create")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let updates = args
        .get("update")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let destroys: Vec<String> = args
        .get("destroy")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let mut created_out = Map::new();
    let mut not_created = Map::new();
    let mut updated_out = Map::new();
    let mut not_updated = Map::new();
    let mut destroyed_out: Vec<String> = Vec::new();
    let mut not_destroyed = Map::new();

    // Per-account state token captured around the mutation; see the
    // matching note in `email_set`.
    let old_state = fixture.state_for(&account_id).to_string();
    fixture.mutate(|fix| {
        let mut diff = crate::fixture::MutationDiff::default();

        for (client_id, body) in &creates {
            match build_mailbox_from_create(fix, &account_id, body) {
                Ok(mailbox) => {
                    let server_id = mailbox.id.clone();
                    fix.mailboxes.push(mailbox);
                    diff.mailbox_created.push(server_id.clone());
                    created_out.insert(
                        client_id.clone(),
                        json!({
                            "id": server_id,
                            "totalEmails": 0,
                            "unreadEmails": 0,
                            "totalThreads": 0,
                            "unreadThreads": 0,
                            "myRights": {
                                "mayReadItems": true,
                                "mayAddItems": true,
                                "mayRemoveItems": true,
                                "maySetSeen": true,
                                "maySetKeywords": true,
                                "mayCreateChild": true,
                                "mayRename": true,
                                "mayDelete": true,
                                "maySubmit": true,
                            },
                        }),
                    );
                }
                Err(err) => {
                    not_created.insert(client_id.clone(), err);
                }
            }
        }

        for (id, patch) in &updates {
            let Some(idx) = fix.mailboxes.iter().position(|m| &m.id == id) else {
                not_updated.insert(id.clone(), set_error("notFound", "no such mailbox"));
                continue;
            };
            let mut clone = fix.mailboxes[idx].clone();
            match apply_mailbox_patch(&mut clone, patch) {
                Ok(()) => {
                    fix.mailboxes[idx] = clone;
                    diff.mailbox_updated.push(id.clone());
                    updated_out.insert(id.clone(), Value::Null);
                }
                Err(err) => {
                    not_updated.insert(id.clone(), err);
                }
            }
        }

        for id in &destroys {
            // RFC 8621 §2.5: destroying a mailbox that still
            // contains emails is rejected with `mailboxHasEmail`.
            // We follow the same shape so ratatoskr's destroy path
            // sees the realistic failure mode.
            if fix.emails.iter().any(|e| e.mailbox_ids.contains(id)) {
                not_destroyed.insert(
                    id.clone(),
                    set_error(
                        "mailboxHasEmail",
                        "mailbox still contains emails; destroy them first",
                    ),
                );
                continue;
            }
            // Snapshot the account before removal so we can record it
            // parallel to the destroyed id.
            let account = fix
                .mailboxes
                .iter()
                .find(|m| &m.id == id)
                .map(|m| m.account_id.clone());
            let len_before = fix.mailboxes.len();
            fix.mailboxes.retain(|m| &m.id != id);
            if fix.mailboxes.len() < len_before {
                diff.mailbox_destroyed.push(id.clone());
                diff.mailbox_destroyed_accounts
                    .push(account.expect("mailbox existed before retain"));
                destroyed_out.push(id.clone());
            } else {
                not_destroyed.insert(id.clone(), set_error("notFound", "no such mailbox"));
            }
        }

        diff
    });
    let new_state = fixture.state_for(&account_id).to_string();

    Ok(set_response(
        &account_id,
        &old_state,
        &new_state,
        created_out,
        not_created,
        updated_out,
        not_updated,
        destroyed_out,
        not_destroyed,
    ))
}

fn build_mailbox_from_create(
    fix: &Fixture,
    request_account_id: &str,
    body: &Value,
) -> Result<Mailbox, Value> {
    let body = body
        .as_object()
        .ok_or_else(|| set_error("invalidProperties", "create body must be an object"))?;
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| set_error("invalidProperties", "missing name"))?
        .to_string();
    let parent_id = body.get("parentId").and_then(|v| {
        if v.is_null() {
            None
        } else {
            v.as_str().map(String::from)
        }
    });
    if let Some(pid) = &parent_id {
        let parent = fix.mailboxes.iter().find(|m| &m.id == pid);
        let Some(parent) = parent else {
            return Err(set_error(
                "invalidProperties",
                &format!("unknown parentId {pid:?}"),
            ));
        };
        // A child mailbox must inherit the parent's account. If the
        // request targets a different account than the parent's,
        // there's no way to honour both - reject so callers see the
        // mismatch.
        if parent.account_id != request_account_id {
            return Err(set_error(
                "invalidProperties",
                &format!(
                    "parentId {pid:?} belongs to account {:?} but request accountId is {request_account_id:?}",
                    parent.account_id,
                ),
            ));
        }
    }
    let sort_order = body.get("sortOrder").and_then(Value::as_i64);
    let is_subscribed = body
        .get("isSubscribed")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let role = body
        .get("role")
        .and_then(|v| if v.is_null() { None } else { v.as_str() })
        .map(crate::fixture::Role::parse)
        .transpose()
        .map_err(|e: String| set_error("invalidProperties", &e))?;
    let id = fix.fresh_mailbox_id();
    Ok(Mailbox {
        id,
        account_id: request_account_id.to_string(),
        name,
        role,
        parent_id,
        sort_order,
        is_subscribed,
    })
}

pub(crate) fn apply_mailbox_patch(mailbox: &mut Mailbox, patch: &Value) -> Result<(), Value> {
    let obj = patch
        .as_object()
        .ok_or_else(|| set_error("invalidProperties", "patch must be an object"))?;
    for (path, value) in obj {
        match path.as_str() {
            "name" => {
                let s = value
                    .as_str()
                    .ok_or_else(|| set_error("invalidProperties", "name must be a string"))?;
                mailbox.name = s.to_string();
            }
            "parentId" => {
                if value.is_null() {
                    mailbox.parent_id = None;
                } else if let Some(s) = value.as_str() {
                    mailbox.parent_id = Some(s.to_string());
                } else {
                    return Err(set_error(
                        "invalidProperties",
                        "parentId must be a string or null",
                    ));
                }
            }
            "sortOrder" => {
                if value.is_null() {
                    mailbox.sort_order = None;
                } else if let Some(n) = value.as_i64() {
                    mailbox.sort_order = Some(n);
                } else {
                    return Err(set_error(
                        "invalidProperties",
                        "sortOrder must be an integer or null",
                    ));
                }
            }
            "isSubscribed" => {
                let b = value.as_bool().ok_or_else(|| {
                    set_error("invalidProperties", "isSubscribed must be a boolean")
                })?;
                mailbox.is_subscribed = b;
            }
            "role" => {
                if value.is_null() {
                    mailbox.role = None;
                } else if let Some(s) = value.as_str() {
                    mailbox.role = Some(
                        crate::fixture::Role::parse(s)
                            .map_err(|e: String| set_error("invalidProperties", &e))?,
                    );
                } else {
                    return Err(set_error(
                        "invalidProperties",
                        "role must be a string or null",
                    ));
                }
            }
            _other => {
                // A real JMAP server ignores patch keys it does not
                // model (and null-valued keys clearing absent
                // properties). bifrost's `Mailbox/set` update patch
                // serializes `role: null` and `shareWith: null`
                // alongside the property it actually changes
                // (`name` / `parentId`); rejecting the unknown
                // `shareWith` key surfaced to bifrost as
                // `request.malformed` and broke rename / move. Accept
                // any other key as a no-op so the modelled keys above
                // still apply.
            }
        }
    }
    Ok(())
}

// ── /set helpers ────────────────────────────────────────────────────

fn set_error(kind: &str, description: &str) -> Value {
    json!({
        "type": kind,
        "description": description,
    })
}

#[allow(clippy::too_many_arguments)]
fn set_response(
    account_id: &str,
    old_state: &str,
    new_state: &str,
    created: Map<String, Value>,
    not_created: Map<String, Value>,
    updated: Map<String, Value>,
    not_updated: Map<String, Value>,
    destroyed: Vec<String>,
    not_destroyed: Map<String, Value>,
) -> Value {
    let mut out = Map::new();
    out.insert(
        "accountId".to_string(),
        Value::String(account_id.to_string()),
    );
    out.insert("oldState".to_string(), Value::String(old_state.to_string()));
    out.insert("newState".to_string(), Value::String(new_state.to_string()));
    out.insert(
        "created".to_string(),
        if created.is_empty() {
            Value::Null
        } else {
            Value::Object(created)
        },
    );
    out.insert(
        "notCreated".to_string(),
        if not_created.is_empty() {
            Value::Null
        } else {
            Value::Object(not_created)
        },
    );
    out.insert(
        "updated".to_string(),
        if updated.is_empty() {
            Value::Null
        } else {
            Value::Object(updated)
        },
    );
    out.insert(
        "notUpdated".to_string(),
        if not_updated.is_empty() {
            Value::Null
        } else {
            Value::Object(not_updated)
        },
    );
    out.insert(
        "destroyed".to_string(),
        if destroyed.is_empty() {
            Value::Null
        } else {
            Value::Array(destroyed.into_iter().map(Value::String).collect())
        },
    );
    out.insert(
        "notDestroyed".to_string(),
        if not_destroyed.is_empty() {
            Value::Null
        } else {
            Value::Object(not_destroyed)
        },
    );
    Value::Object(out)
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
            state_seed: "s1".into(),
            accounts: vec![Account {
                id: "acct".into(),
                name: "a@b".into(),
                is_personal: true,
                primary: true,
            }],
            mailboxes: vec![
                Mailbox {
                    id: "mb-inbox".into(),
                    account_id: "acct".into(),
                    name: "Inbox".into(),
                    role: Some(Role::Inbox),
                    parent_id: None,
                    sort_order: Some(0),
                    is_subscribed: true,
                },
                Mailbox {
                    id: "mb-archive".into(),
                    account_id: "acct".into(),
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
                    account_id: "acct".into(),
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
                    raw_bytes: None,
                    reaction_type: None,
                    reaction_count: None,
                },
                Email {
                    id: "e2".into(),
                    account_id: "acct".into(),
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
                    raw_bytes: None,
                    reaction_type: None,
                    reaction_count: None,
                },
            ],
            oauth: crate::fixture::OAuthConfig::default(),
            caldav: crate::fixture::CalDavConfig::default(),
            announce: vec![],
            discovery: crate::fixture::DiscoveryConfig::default(),
            calendars: vec![],
            events: vec![],
            account_logs: Default::default(),
            change_script: Vec::new(),
            contact_folders: vec![],
            contacts: vec![],
            contact_groups: vec![],
            other_contacts: vec![],
            directory_people: vec![],
            categories: vec![],
            groups: vec![],
            acls: vec![],
            public_folders: vec![],
            public_items: vec![],
            send_as: vec![],
            mailbox_uid_history: std::collections::HashMap::new(),
            synthetic_event_seq: 0,
            synthetic_email_seq: 0,
            synthetic_category_seq: 0,
            synthetic_contact_seq: 0,
            uploaded_blobs: std::collections::BTreeMap::new(),
            gmail_label_colors: std::collections::BTreeMap::new(),
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
        let f = crate::shared::handle(fix());
        // `Email/copy` stays out of scope in v0; `Email/import` and
        // `EmailSubmission/set` are now implemented for the send cut.
        let (name, body) = dispatch(&f, None, "Email/copy", &json!({}), &[]);
        assert_eq!(name, "error");
        assert_eq!(body.get("type").unwrap(), "unknownMethod");
    }

    #[test]
    fn handle_passes_call_id_through_and_echoes_state() {
        let f = crate::shared::handle(fix());
        let req = JmapRequest {
            using: vec!["urn:ietf:params:jmap:core".into()],
            method_calls: vec![(
                "Mailbox/get".into(),
                json!({"accountId": "acct"}),
                "c0".into(),
            )],
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
        let f = crate::shared::handle(fix());
        let req = JmapRequest {
            using: vec![],
            method_calls: vec![
                (
                    "Mailbox/get".into(),
                    json!({"accountId": "acct"}),
                    "a".into(),
                ),
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
            account_id: "acct".into(),
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
            raw_bytes: None,
            reaction_type: None,
            reaction_count: None,
        }
    }

    fn fix_for_query() -> Fixture {
        let mk = |y, m, d, hh| chrono::Utc.with_ymd_and_hms(y, m, d, hh, 0, 0).unwrap();
        Fixture {
            name: "q".into(),
            state_seed: "s1".into(),
            accounts: vec![Account {
                id: "acct".into(),
                name: "a@b".into(),
                is_personal: true,
                primary: true,
            }],
            mailboxes: vec![
                Mailbox {
                    id: "mb-inbox".into(),
                    account_id: "acct".into(),
                    name: "Inbox".into(),
                    role: Some(Role::Inbox),
                    parent_id: None,
                    sort_order: Some(0),
                    is_subscribed: true,
                },
                Mailbox {
                    id: "mb-archive".into(),
                    account_id: "acct".into(),
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
            caldav: crate::fixture::CalDavConfig::default(),
            announce: vec![],
            discovery: crate::fixture::DiscoveryConfig::default(),
            calendars: vec![],
            events: vec![],
            account_logs: Default::default(),
            change_script: Vec::new(),
            contact_folders: vec![],
            contacts: vec![],
            contact_groups: vec![],
            other_contacts: vec![],
            directory_people: vec![],
            categories: vec![],
            groups: vec![],
            acls: vec![],
            public_folders: vec![],
            public_items: vec![],
            send_as: vec![],
            mailbox_uid_history: std::collections::HashMap::new(),
            synthetic_event_seq: 0,
            synthetic_email_seq: 0,
            synthetic_category_seq: 0,
            synthetic_contact_seq: 0,
            uploaded_blobs: std::collections::BTreeMap::new(),
            gmail_label_colors: std::collections::BTreeMap::new(),
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
        let ts = chrono::Utc
            .with_ymd_and_hms(2026, 1, 15, 11, 0, 0)
            .unwrap()
            .timestamp();
        let resp = email_query(&f, &json!({"accountId": "acct", "filter": {"after": ts}})).unwrap();
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
        let p1 = email_query(&f, &json!({"accountId": "acct", "limit": 2, "position": 0})).unwrap();
        let p2 = email_query(&f, &json!({"accountId": "acct", "limit": 2, "position": 2})).unwrap();
        let p3 = email_query(&f, &json!({"accountId": "acct", "limit": 2, "position": 4})).unwrap();
        let len = |v: &Value| v.get("ids").unwrap().as_array().unwrap().len();
        assert_eq!(len(&p1), 2);
        assert_eq!(len(&p2), 2);
        assert_eq!(len(&p3), 1);
        // Position past total returns empty without erroring.
        let p4 = email_query(
            &f,
            &json!({"accountId": "acct", "limit": 2, "position": 99}),
        )
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
        let sent = chrono::Utc
            .with_ymd_and_hms(2026, 1, 15, 9, 59, 50)
            .unwrap();
        Fixture {
            name: "g".into(),
            state_seed: "s1".into(),
            accounts: vec![Account {
                id: "acct".into(),
                name: "a@b".into(),
                is_personal: true,
                primary: true,
            }],
            mailboxes: vec![Mailbox {
                id: "mb-inbox".into(),
                account_id: "acct".into(),
                name: "Inbox".into(),
                role: Some(Role::Inbox),
                parent_id: None,
                sort_order: Some(0),
                is_subscribed: true,
            }],
            emails: vec![Email {
                id: "e1".into(),
                account_id: "acct".into(),
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
                raw_bytes: None,
                reaction_type: None,
                reaction_count: None,
            }],
            oauth: crate::fixture::OAuthConfig::default(),
            caldav: crate::fixture::CalDavConfig::default(),
            announce: vec![],
            discovery: crate::fixture::DiscoveryConfig::default(),
            calendars: vec![],
            events: vec![],
            account_logs: Default::default(),
            change_script: Vec::new(),
            contact_folders: vec![],
            contacts: vec![],
            contact_groups: vec![],
            other_contacts: vec![],
            directory_people: vec![],
            categories: vec![],
            groups: vec![],
            acls: vec![],
            public_folders: vec![],
            public_items: vec![],
            send_as: vec![],
            mailbox_uid_history: std::collections::HashMap::new(),
            synthetic_event_seq: 0,
            synthetic_email_seq: 0,
            synthetic_category_seq: 0,
            synthetic_contact_seq: 0,
            uploaded_blobs: std::collections::BTreeMap::new(),
            gmail_label_colors: std::collections::BTreeMap::new(),
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
        assert_eq!(
            item.get("attachments").unwrap().as_array().unwrap().len(),
            0
        );

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
        let resp = email_get(&f, &json!({"accountId": "acct", "ids": ["e1", "ghost"]})).unwrap();
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
