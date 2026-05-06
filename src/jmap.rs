//! JMAP request envelope, dispatcher, and per-method handlers.
//!
//! RFC 8620 §3.3 (request) and §3.4 (response). Method handlers live
//! here so `routes.rs` can stay just routing. The dispatcher returns
//! `("error", {...})` for unknown or per-call failures, per RFC §3.5.2.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::fixture::{Fixture, Mailbox};

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
pub fn handle(fixture: &Fixture, req: JmapRequest) -> JmapResponse {
    let mut responses = Vec::with_capacity(req.method_calls.len());
    for (name, args, call_id) in req.method_calls {
        let (out_name, out_args) = dispatch(fixture, &name, &args);
        responses.push((out_name, out_args, call_id));
    }
    JmapResponse {
        method_responses: responses,
        session_state: fixture.state.clone(),
        created_ids: req.created_ids,
    }
}

fn dispatch(fixture: &Fixture, name: &str, args: &Value) -> (String, Value) {
    match name {
        "Mailbox/get" => match mailbox_get(fixture, args) {
            Ok(v) => (name.to_string(), v),
            Err(err) => ("error".to_string(), err),
        },
        "Email/query" => match email_query(fixture, args) {
            Ok(v) => (name.to_string(), v),
            Err(err) => ("error".to_string(), err),
        },
        // step 7: "Email/get"
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
/// Default page size when the client omits `limit`.
const QUERY_LIMIT_DEFAULT: u64 = 50;

/// RFC 8621 §4.4. v0 supports `{"after": <unix_seconds>}` and
/// `{"inMailbox": <id>}` filter conditions; sort is hard-wired to
/// `receivedAt` descending with `id` lexicographic as tiebreaker so
/// the wire output is byte-stable for a given fixture.
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
    let position = position as u64;

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
            (n as u64).min(QUERY_LIMIT_CAP)
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
    let start = position.min(total) as usize;
    let end = (position + limit).min(total) as usize;
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

/// v0 filter shape. Only one condition at a time; FilterOperator
/// (AND/OR/NOT) is not implemented because ratatoskr never sends one
/// during initial sync.
enum Filter {
    None,
    After(i64),
    InMailbox(String),
}

impl Filter {
    fn matches(&self, e: &crate::fixture::Email) -> bool {
        match self {
            Filter::None => true,
            Filter::After(ts) => e.received_at.timestamp() >= *ts,
            Filter::InMailbox(id) => e.mailbox_ids.iter().any(|m| m == id),
        }
    }
}

fn parse_filter(raw: Option<&Value>) -> Result<Filter, Value> {
    let Some(v) = raw else { return Ok(Filter::None) };
    if v.is_null() {
        return Ok(Filter::None);
    }
    let obj = v.as_object().ok_or_else(|| {
        json!({
            "type": "invalidArguments",
            "description": "filter must be an object",
        })
    })?;
    if obj.is_empty() {
        return Ok(Filter::None);
    }
    if let Some(after) = obj.get("after") {
        let ts = after.as_i64().ok_or_else(|| {
            json!({
                "type": "invalidArguments",
                "description": "filter.after must be a unix-seconds integer",
            })
        })?;
        return Ok(Filter::After(ts));
    }
    if let Some(mid) = obj.get("inMailbox") {
        let s = mid.as_str().ok_or_else(|| {
            json!({
                "type": "invalidArguments",
                "description": "filter.inMailbox must be a string",
            })
        })?;
        return Ok(Filter::InMailbox(s.to_string()));
    }
    Err(json!({
        "type": "unsupportedFilter",
        "description": "v0 supports only {after} or {inMailbox}",
    }))
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
                },
            ],
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
        let (name, body) = dispatch(&f, "Email/import", &json!({}));
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
        let resp = handle(&f, req);
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
        let resp = handle(&f, req);
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
}
