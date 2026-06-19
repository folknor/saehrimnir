//! Graph user-profile surface (`/v1.0/me`, `/v1.0/users/{id}`).
//!
//! `GraphAccountFactory::open` issues `GET /me?$select=displayName,
//! mail,userPrincipalName` as its FIRST request (bifrost
//! `crates/graph/src/api.rs:6-12`); the returned `mail` /
//! `userPrincipalName` becomes the account's own address
//! (`account/mod.rs:298`). Without this route the bare `/v1.0/me`
//! path fell through to the Graph 404 catchall, so `open()` failed
//! with a `Discover` error and no Graph account could open against
//! the mock at all.
//!
//! `me` resolves to the bearer's account (parallel to
//! `/me/memberOf` in `group_sync`); `/users/{id}` resolves the named
//! account with `me` as an alias. Unknown ids 404 `ResourceNotFound`.

use axum::{
    Router,
    extract::{Path, RawQuery, State},
    http::HeaderMap,
    response::Response,
    routing::get,
};
use serde_json::{Map, Value, json};

use super::{AppState, ok_json};
use crate::fixture::Account;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1.0/me", get(profile_me))
        .route("/v1.0/users/{user}", get(profile_user))
        // GAL directory search. bifrost addresses it as `/me/users`
        // (real Graph uses `/users`); accept both.
        .route("/v1.0/users", get(list_users))
        .route("/v1.0/me/users", get(list_users))
}

/// `GET /v1.0/users` / `/v1.0/me/users` - directory (GAL) search.
/// bifrost's `directory_search` filters with
/// `startswith(displayName,'X') or startswith(mail,'X')`; v0 matches
/// declared accounts whose name starts with the term (case-
/// insensitive), projecting each as a Graph user. No filter lists
/// every account. (bifrost swallows a 403 on personal Gmail-style
/// accounts; the mock simply returns the matches.)
async fn list_users(State(state): State<AppState>, RawQuery(raw): RawQuery) -> Response {
    if let Some(o) = super::maybe_override(&state, "list_users", |_| Ok(())) {
        return o;
    }
    let q = super::odata::OdataQuery::parse(raw.as_deref());
    let term = q.filter.as_deref().and_then(parse_startswith_term);
    let fixture = state.fixture();
    let value: Vec<Value> = fixture
        .accounts
        .iter()
        .filter(|a| match &term {
            None => true,
            Some(t) => a.name.to_lowercase().starts_with(&t.to_lowercase()),
        })
        .map(serialize_user)
        .collect();
    ok_json(json!({
        "@odata.context": "https://graph.microsoft.com/v1.0/$metadata#users",
        "value": value,
    }))
}

/// Extract the search term from a `startswith(displayName,'X') or
/// startswith(mail,'X')` `$filter` (the first quoted substring).
fn parse_startswith_term(filter: &str) -> Option<String> {
    let start = filter.find('\'')?;
    let rest = &filter[start + 1..];
    let end = rest.find('\'')?;
    Some(rest[..end].to_string())
}

async fn profile_me(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(o) = super::maybe_override(&state, "get_profile", |_| Ok(())) {
        return o;
    }
    let fixture = state.fixture();
    let account_id =
        crate::oauth::account_from_bearer(&fixture, &state.shared.token_store, &headers);
    // account_from_bearer always returns a declared id (it falls back
    // to primary for missing / unknown tokens), so the lookup is
    // infallible; guard rather than unwrap.
    let account = fixture
        .account(&account_id)
        .unwrap_or_else(|| fixture.primary_account());
    ok_json(serialize_profile(account))
}

async fn profile_user(State(state): State<AppState>, Path(user): Path<String>) -> Response {
    let user_owned = user.clone();
    if let Some(o) = super::maybe_override(&state, "get_profile", move |s| {
        crate::lua::req_set_str(s, "user", &user_owned)
    }) {
        return o;
    }
    let fixture = state.fixture();
    let account_id = match super::resolve_user_account(&fixture, &user) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let account = fixture
        .account(&account_id)
        .expect("resolve_user_account returns a declared id");
    ok_json(serialize_profile(account))
}

/// Project an `Account` as a Graph user entity, matching the
/// `$select=displayName,mail,userPrincipalName` shape bifrost's
/// `get_profile` reads. The fixture `Account` carries only an id + a
/// `name` (its email address), so `displayName` / `mail` /
/// `userPrincipalName` all derive from `name` - the same convention
/// `group_sync::serialize_user_member` uses.
fn serialize_profile(a: &Account) -> Value {
    let mut v = serialize_user(a);
    if let Value::Object(ref mut m) = v {
        m.insert(
            "@odata.context".into(),
            Value::String("https://graph.microsoft.com/v1.0/$metadata#users/$entity".into()),
        );
    }
    v
}

/// The bare Graph user entity (no `@odata.context`), used as a
/// collection member by GAL search and wrapped by `serialize_profile`
/// for the single-entity reads.
fn serialize_user(a: &Account) -> Value {
    let mut obj = Map::new();
    obj.insert("id".into(), Value::String(a.id.clone()));
    obj.insert("displayName".into(), Value::String(a.name.clone()));
    obj.insert("mail".into(), Value::String(a.name.clone()));
    obj.insert("userPrincipalName".into(), Value::String(a.name.clone()));
    Value::Object(obj)
}
