//! People API contacts handlers.
//!
//! Projects fixture `Contact` entries into Google People API
//! `Person` resources. The People API has no folder concept, so
//! the projection collapses every `Contact` across every
//! `ContactFolder` into one flat connections list, ordered by the
//! contact's `id` lexicographically. Folder-scoped sync remains
//! visible through the existing Graph contacts surface.
//!
//! Sync tokens piggy-back on `Fixture::state`. A follow-up call
//! with a token that matches the current state returns an empty
//! delta + the same token. A token the fixture doesn't recognize
//! (older than the seed or simply not one we ever issued) returns
//! HTTP 410 with the standard People error envelope; ratatoskr's
//! recovery path drops the saved token and re-bootstraps via a
//! token-less call.

use axum::{
    Router,
    extract::{Query, State},
    http::StatusCode,
    response::Response,
    routing::get,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{AppState, error, ok_json};
use crate::fixture::{Contact, Fixture};

/// People-API page size cap from
/// `<ratatoskr>/crates/gmail/src/contacts/mod.rs::PAGE_SIZE`.
/// We honour the request's `pageSize` but never exceed this.
const MAX_PAGE_SIZE: usize = 1000;
const DEFAULT_PAGE_SIZE: usize = 100;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/people/me/connections", get(list_connections))
        .route("/v1/otherContacts", get(list_other_contacts))
}

#[derive(Debug, Deserialize, Default)]
struct ListParams {
    #[serde(default, rename = "pageSize")]
    page_size: Option<usize>,
    #[serde(default, rename = "pageToken")]
    page_token: Option<String>,
    #[serde(default, rename = "syncToken")]
    sync_token: Option<String>,
    /// Echoed silently. The mock always emits the full Person
    /// shape regardless of what fields were requested; ratatoskr's
    /// parser tolerates extra fields. Captured so the request log
    /// records what the client asked for.
    #[serde(default, rename = "personFields")]
    _person_fields: Option<String>,
    #[serde(default, rename = "readMask")]
    _read_mask: Option<String>,
    #[serde(default, rename = "requestSyncToken")]
    _request_sync_token: Option<String>,
}

async fn list_connections(
    State(state): State<AppState>,
    Query(params): Query<ListParams>,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "list_connections", |s| {
        if let Some(t) = &params.sync_token {
            crate::lua::req_set_str(s, "sync_token", t)?;
        }
        if let Some(t) = &params.page_token {
            crate::lua::req_set_str(s, "page_token", t)?;
        }
        Ok(())
    }) {
        return o;
    }

    let fixture = state.fixture();

    // Sync-token paths:
    // - current state: empty delta + same token echoed back.
    // - known historical state: walk the change_log, emit
    //   created/updated contacts as full Persons and destroyed
    //   contacts as `metadata.deleted: true` tombstones, then
    //   emit the current state as `nextSyncToken`. ratatoskr's
    //   recovery branch only fires on 410, so a known token
    //   must produce an honest delta (or recovery breaks
    //   silently: stale token returns full live list, missing
    //   tombstones, contacts persist forever).
    // - unknown / evicted token: HTTP 410 with the
    //   `expired`-reason envelope. Real People API uses
    //   `EXPIRED_TOKEN`; ratatoskr matches on the `syncToken`
    //   substring + the 410 status, so this lands.
    if params.sync_token.as_deref() == Some(fixture.state.as_str()) {
        return ok_json(json!({
            "connections": [],
            "totalPeople": 0,
            "totalItems": 0,
            "nextSyncToken": fixture.state,
        }));
    }
    if let Some(token) = params.sync_token.as_deref() {
        let Some(delta) = fixture.contact_delta_since_any(token) else {
            return error(
                StatusCode::GONE,
                "syncToken expired or not recognised",
                "expired",
            );
        };
        let live_by_id: std::collections::HashMap<&str, &Contact> = fixture
            .contacts
            .iter()
            .map(|c| (c.id.as_str(), c))
            .collect();
        let mut connections: Vec<Value> = Vec::new();
        for id in delta.created.iter().chain(delta.updated.iter()) {
            if let Some(c) = live_by_id.get(id.as_str()) {
                connections.push(serialize_person(c));
            }
        }
        for id in &delta.destroyed {
            connections.push(tombstone_person(id));
        }
        return ok_json(json!({
            "connections": connections,
            "totalPeople": connections.len(),
            "totalItems": connections.len(),
            "nextSyncToken": fixture.state,
        }));
    }

    let mut all = projected_connections(&fixture);
    let total = all.len();
    let page_size = clamp_page_size(params.page_size);
    let offset = parse_page_token(params.page_token.as_deref()).unwrap_or(0);
    if offset > total {
        return error(
            StatusCode::BAD_REQUEST,
            "pageToken offset exceeds total",
            "badRequest",
        );
    }
    let end = (offset + page_size).min(total);
    let slice: Vec<Value> = all.drain(offset..end).collect();
    let mut body = Map::new();
    body.insert("connections".into(), Value::Array(slice));
    body.insert(
        "totalPeople".into(),
        Value::Number((total as u64).into()),
    );
    body.insert(
        "totalItems".into(),
        Value::Number((total as u64).into()),
    );
    if end < total {
        body.insert(
            "nextPageToken".into(),
            Value::String(encode_page_token(end)),
        );
    } else {
        // Final page emits the syncToken; prior pages don't
        // (matches real People API).
        body.insert(
            "nextSyncToken".into(),
            Value::String(fixture.state.clone()),
        );
    }
    ok_json(Value::Object(body))
}

async fn list_other_contacts(
    State(state): State<AppState>,
    Query(params): Query<ListParams>,
) -> Response {
    if let Some(o) = super::maybe_override(&state, "list_other_contacts", |s| {
        if let Some(t) = &params.sync_token {
            crate::lua::req_set_str(s, "sync_token", t)?;
        }
        Ok(())
    }) {
        return o;
    }
    let fixture = state.fixture();
    if let Some(token) = params.sync_token.as_deref()
        && !is_known_state(&fixture, token)
    {
        return error(
            StatusCode::GONE,
            "syncToken expired or not recognised",
            "expired",
        );
    }
    // v0: empty list. Reserved for a fixture-side `[other_contact]`
    // shape if ratatoskr ever needs adversarial coverage there.
    ok_json(json!({
        "otherContacts": [],
        "totalSize": 0,
        "nextSyncToken": fixture.state,
    }))
}

// ── Shape helpers ───────────────────────────────────────────────────

fn is_known_state(fixture: &Fixture, state: &str) -> bool {
    if state == fixture.state {
        return true;
    }
    if state == fixture.change_log_seed() {
        return true;
    }
    fixture
        .change_log_transitions()
        .any(|t| t.from_state == state || t.to_state == state)
}

fn clamp_page_size(requested: Option<usize>) -> usize {
    let n = requested.unwrap_or(DEFAULT_PAGE_SIZE);
    n.clamp(1, MAX_PAGE_SIZE)
}

/// Encoded as `p.<offset>` so a malformed token (e.g. someone
/// hand-rolling one) is obvious in the request log.
fn encode_page_token(offset: usize) -> String {
    format!("p.{offset}")
}

fn parse_page_token(t: Option<&str>) -> Option<usize> {
    t?.strip_prefix("p.")?.parse().ok()
}

fn projected_connections(fixture: &Fixture) -> Vec<Value> {
    let mut contacts: Vec<&Contact> = fixture.contacts.iter().collect();
    contacts.sort_by(|a, b| a.id.cmp(&b.id));
    contacts.into_iter().map(serialize_person).collect()
}

/// People API tombstone shape: a Person whose `metadata.deleted`
/// is true. ratatoskr's `PersonMetadata.deleted` (`crates/gmail/
/// src/contacts/mod.rs:58-61`) routes these to delete-the-row.
/// Other fields are minimised because the Person no longer
/// exists - the resource name is the only addressable identity.
fn tombstone_person(id: &str) -> Value {
    json!({
        "resourceName": format!("people/{id}"),
        "etag": format!("etag-{id}"),
        "metadata": {
            "deleted": true,
            "sources": [{ "type": "CONTACT", "id": id }],
        }
    })
}

fn serialize_person(c: &Contact) -> Value {
    let mut obj = Map::new();
    obj.insert(
        "resourceName".into(),
        Value::String(format!("people/{}", c.id)),
    );
    // ETag derives from the contact id so it stays byte-stable
    // for a given fixture. Real People API emits an opaque base64
    // token; ratatoskr's parser passes it through without
    // interpretation.
    obj.insert("etag".into(), Value::String(format!("etag-{}", c.id)));

    let mut metadata = Map::new();
    metadata.insert("deleted".into(), Value::Bool(false));
    metadata.insert(
        "sources".into(),
        Value::Array(vec![json!({
            "type": "CONTACT",
            "id": c.id,
        })]),
    );
    obj.insert("metadata".into(), Value::Object(metadata));

    if let Some(name) = &c.display_name {
        obj.insert(
            "names".into(),
            Value::Array(vec![json!({
                "displayName": name,
                "givenName": name,
                "familyName": "",
            })]),
        );
    }

    let emails: Vec<Value> = c
        .emails
        .iter()
        .map(|e| {
            json!({
                "value": e.address,
                "type": "other",
            })
        })
        .collect();
    obj.insert("emailAddresses".into(), Value::Array(emails));

    Value::Object(obj)
}
