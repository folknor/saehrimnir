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
        // step 6: "Email/query"
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
}
