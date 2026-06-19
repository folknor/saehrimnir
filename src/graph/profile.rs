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
    extract::{Path, State},
    http::HeaderMap,
    response::Response,
    routing::get,
};
use serde_json::{Map, Value};

use super::{AppState, ok_json};
use crate::fixture::Account;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1.0/me", get(profile_me))
        .route("/v1.0/users/{user}", get(profile_user))
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
    let mut obj = Map::new();
    obj.insert(
        "@odata.context".into(),
        Value::String("https://graph.microsoft.com/v1.0/$metadata#users/$entity".into()),
    );
    obj.insert("id".into(), Value::String(a.id.clone()));
    obj.insert("displayName".into(), Value::String(a.name.clone()));
    obj.insert("mail".into(), Value::String(a.name.clone()));
    obj.insert("userPrincipalName".into(), Value::String(a.name.clone()));
    Value::Object(obj)
}
