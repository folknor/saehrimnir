//! Microsoft Graph group endpoints.
//!
//! Implements the subset of `/v1.0/groups/...` and the
//! `/me/memberOf` / `/users/{id}/memberOf` walkers ratatoskr's
//! group-enumeration code path exercises. Groups are
//! cross-account: each `[[group]]` in the fixture names a
//! `members` list of declared `[[account]]` ids, and the wire
//! projection of `/v1.0/groups/{id}/members` serialises each
//! member account as a Graph `#microsoft.graph.user` resource.
//!
//! Endpoints (all GET; v0 has no group-mutation surface):
//!
//! - `/v1.0/groups` - list.
//! - `/v1.0/groups/{id}` - single.
//! - `/v1.0/groups/{id}/members` - users belonging to the group.
//! - `/v1.0/groups/{id}/transitiveMembers[/microsoft.graph.user]` -
//!   the transitive-closure twin bifrost's groups surface drives.
//!   v0 has no nested groups, so the transitive closure equals the
//!   direct `members` set, and every member is user-typed - the
//!   `microsoft.graph.user` OData type-cast is therefore an all-pass
//!   filter here. Both alias `list_group_members`; if the fixture
//!   ever grows group-typed members the cast will need a real filter.
//! - `/v1.0/me/memberOf[/microsoft.graph.group]` - groups containing
//!   the bearer-token's account (bearer-resolved via the same
//!   `account_from_bearer` helper Gmail / gcal / People use). The
//!   `microsoft.graph.group` type-cast is all-pass in v0 since
//!   `memberOf` only ever yields group-typed directoryObjects.
//! - `/v1.0/users/{userId}/memberOf[/microsoft.graph.group]` - groups
//!   containing the named account (`me` aliases the bearer-resolved
//!   primary; unknown id returns 404).

use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::get,
};
use serde_json::{Map, Value, json};

use super::{AppState, error, ok_json};
use crate::fixture::{Account, Group};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1.0/groups", get(list_groups))
        .route("/v1.0/groups/{group}", get(get_group))
        .route("/v1.0/groups/{group}/members", get(list_group_members))
        // Transitive-closure twins. No nested groups in v0, so these
        // resolve to the same member set as `/members`; the
        // `microsoft.graph.user` cast is all-pass. See module doc.
        .route(
            "/v1.0/groups/{group}/transitiveMembers",
            get(list_group_members),
        )
        .route(
            "/v1.0/groups/{group}/transitiveMembers/microsoft.graph.user",
            get(list_group_members),
        )
        .route("/v1.0/me/memberOf", get(member_of_me))
        // OData type-cast twin; all-pass in v0 (memberOf yields only
        // group-typed directoryObjects). See module doc.
        .route("/v1.0/me/memberOf/microsoft.graph.group", get(member_of_me))
        .route("/v1.0/users/{user}/memberOf", get(member_of_user))
        .route(
            "/v1.0/users/{user}/memberOf/microsoft.graph.group",
            get(member_of_user),
        )
}

// ── Read handlers ───────────────────────────────────────────────────

async fn list_groups(State(state): State<AppState>) -> Response {
    if let Some(o) = super::maybe_override(&state, "list_groups", |_| Ok(())) {
        return o;
    }
    let fixture = state.fixture();
    let value: Vec<Value> = fixture.groups.iter().map(serialize_group).collect();
    ok_json(json!({
        "@odata.context": "https://graph.microsoft.com/v1.0/$metadata#groups",
        "value": value,
    }))
}

async fn get_group(State(state): State<AppState>, Path(group): Path<String>) -> Response {
    let group_owned = group.clone();
    if let Some(o) = super::maybe_override(&state, "get_group", move |s| {
        crate::lua::req_set_str(s, "group", &group_owned)
    }) {
        return o;
    }
    let fixture = state.fixture();
    match fixture.groups.iter().find(|g| g.id == group) {
        Some(g) => ok_json(serialize_group(g)),
        None => error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("group {group:?} not declared in fixture"),
        ),
    }
}

async fn list_group_members(State(state): State<AppState>, Path(group): Path<String>) -> Response {
    let group_owned = group.clone();
    if let Some(o) = super::maybe_override(&state, "list_group_members", move |s| {
        crate::lua::req_set_str(s, "group", &group_owned)
    }) {
        return o;
    }
    let fixture = state.fixture();
    let Some(g) = fixture.groups.iter().find(|g| g.id == group) else {
        return error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("group {group:?} not declared in fixture"),
        );
    };
    let value: Vec<Value> = g
        .members
        .iter()
        .filter_map(|account_id| fixture.account(account_id).map(serialize_user_member))
        .collect();
    ok_json(json!({
        "@odata.context":
            "https://graph.microsoft.com/v1.0/$metadata#directoryObjects",
        "value": value,
    }))
}

async fn member_of_me(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let account_id =
        crate::oauth::account_from_bearer(&state.fixture(), &state.shared.token_store, &headers);
    member_of_impl(state, &account_id).await
}

async fn member_of_user(State(state): State<AppState>, Path(user): Path<String>) -> Response {
    let account_id = match super::resolve_user_account(&state.fixture(), &user) {
        Ok(id) => id,
        Err(r) => return r,
    };
    member_of_impl(state, &account_id).await
}

async fn member_of_impl(state: AppState, account_id: &str) -> Response {
    if let Some(o) = super::maybe_override(&state, "list_member_of", |_| Ok(())) {
        return o;
    }
    let fixture = state.fixture();
    let value: Vec<Value> = fixture
        .groups
        .iter()
        .filter(|g| g.members.iter().any(|m| m == account_id))
        .map(serialize_group)
        .collect();
    ok_json(json!({
        "@odata.context":
            "https://graph.microsoft.com/v1.0/$metadata#directoryObjects",
        "value": value,
    }))
}

// ── Projection ──────────────────────────────────────────────────────

fn serialize_group(g: &Group) -> Value {
    let mut obj = Map::new();
    obj.insert(
        "@odata.type".into(),
        Value::String("#microsoft.graph.group".into()),
    );
    obj.insert("id".into(), Value::String(g.id.clone()));
    obj.insert("displayName".into(), Value::String(g.display_name.clone()));
    if let Some(d) = &g.description {
        obj.insert("description".into(), Value::String(d.clone()));
    }
    if let Some(m) = &g.mail {
        obj.insert("mail".into(), Value::String(m.clone()));
    }
    obj.insert("mailEnabled".into(), Value::Bool(g.mail_enabled));
    obj.insert("securityEnabled".into(), Value::Bool(g.security_enabled));
    obj.insert(
        "groupTypes".into(),
        Value::Array(g.group_types.iter().cloned().map(Value::String).collect()),
    );
    Value::Object(obj)
}

/// Project an `Account` as a Graph `#microsoft.graph.user`. Used
/// by `/groups/{id}/members`: real Graph emits each member as the
/// underlying directoryObject type (user / group / device); v0
/// only models user-typed members.
fn serialize_user_member(a: &Account) -> Value {
    let mut obj = Map::new();
    obj.insert(
        "@odata.type".into(),
        Value::String("#microsoft.graph.user".into()),
    );
    obj.insert("id".into(), Value::String(a.id.clone()));
    obj.insert("displayName".into(), Value::String(a.name.clone()));
    obj.insert("mail".into(), Value::String(a.name.clone()));
    obj.insert("userPrincipalName".into(), Value::String(a.name.clone()));
    Value::Object(obj)
}
