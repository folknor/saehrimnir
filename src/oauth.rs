//! OAuth 2.0 / OIDC mock provider.
//!
//! Mounted on the JMAP HTTP listener under `/oauth/...` plus the
//! test-only admin route `/test/oauth/invalidate`. Designed to
//! unblock ratatoskr's `oauth.exchange_code` headless drive: the
//! harness passes `token_url` + `user_info_url` shaped at the JMAP
//! HTTP base, so all OAuth wire endpoints land on that single
//! listener.
//!
//! Endpoints:
//!
//!   - `POST /oauth/token` - accepts both
//!     `grant_type=authorization_code` and `grant_type=refresh_token`.
//!     v0 doesn't validate `client_id` / `client_secret` /
//!     `redirect_uri` because the binary is a test mock; any
//!     well-formed body succeeds. Returns a deterministic
//!     `access_token` derived from the request body and a counter
//!     that resets on `POST /test/fixture/reset`. `expires_in` is
//!     pinned at 3600 (one hour); the mock never advances time so
//!     tokens never expire on their own.
//!   - `GET /oauth/userinfo` - reads `Authorization: Bearer
//!     <token>`, looks the token up in the store, returns
//!     `{ sub, email, email_verified, name, iss }`. 401 if absent
//!     or invalidated. Email + name come from the fixture's
//!     `[account]`; `sub` is the account id; `iss` is
//!     `fixture.oauth.issuer`.
//!   - `POST /test/oauth/invalidate` - admin route. Body
//!     `{"token": "..."}`. Removes the token from the store so
//!     subsequent userinfo / bearer-validated requests 401. 204 on
//!     success; 404 if the token wasn't issued.
//!
//! Bearer enforcement on the mail-protocol HTTP listeners (JMAP,
//! Graph, Gmail) is gated by `fixture.oauth.enforce`. When `false`
//! (default), all listeners accept any bearer (or none) per the
//! "no auth in v0" baseline. When `true`, requests without a valid
//! `Authorization: Bearer <active-token>` header receive a
//! protocol-shaped 401 / error envelope; see
//! `crate::oauth::enforce_bearer`. IMAP and SMTP have their own
//! auth surfaces and are unaffected.
//!
//! Determinism: token strings are
//! `mock-access-{counter}-{hash(grant)}` where `counter` is reset
//! by `POST /test/fixture/reset` and `hash(grant)` is the FNV-1a
//! of the request body. Same body in -> same token out within one
//! reset window.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::fixture::Fixture;

/// Shared, cheap-to-clone token store. Tracks every active token
/// (access tokens minted by `/oauth/token`, plus any pre-seeded
/// tokens from a fixture). Removed entries no longer authenticate.
#[derive(Debug, Clone, Default)]
pub struct TokenStore(Arc<Mutex<TokenStoreInner>>);

#[derive(Debug, Default)]
struct TokenStoreInner {
    /// Active tokens. Key = token string, value = arbitrary metadata
    /// (currently only the originating grant type).
    tokens: HashMap<String, TokenMetadata>,
    /// Monotonic counter for token suffixes. Resets to 0 on
    /// `clear()`.
    counter: u64,
}

#[derive(Debug, Clone)]
pub struct TokenMetadata {
    pub grant_type: String,
    /// Declared account id this token authorizes. The OAuth
    /// userinfo endpoint serves this account's claims; the
    /// Google-family listeners (Gmail, gcal, People) scope reads
    /// to it. `/oauth/token` defaults to the fixture's primary
    /// account when the client doesn't specify one.
    pub account_id: String,
}

impl TokenStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a new access token bound to `account_id`. Returns the
    /// token string; the token is registered as active before this
    /// returns.
    ///
    /// Note on the `mock-access-` prefix: refresh tokens issued by
    /// `token_endpoint` reuse this prefix, with collision-avoidance
    /// guaranteed by the monotonic counter (the second `mint()` call
    /// always sees counter+1) rather than by the prefix or by
    /// `body_hash`. Future code that filters tokens by prefix should
    /// not assume this string distinguishes access from refresh.
    pub fn mint(&self, grant_type: &str, account_id: &str, body_hash: u64) -> String {
        let mut inner = self.0.lock().expect("oauth token store poisoned");
        inner.counter += 1;
        let token = format!("mock-access-{}-{:x}", inner.counter, body_hash);
        inner.tokens.insert(
            token.clone(),
            TokenMetadata {
                grant_type: grant_type.to_string(),
                account_id: account_id.to_string(),
            },
        );
        token
    }

    pub fn is_active(&self, token: &str) -> bool {
        self.0
            .lock()
            .expect("oauth token store poisoned")
            .tokens
            .contains_key(token)
    }

    /// Look up the account a token authorizes. Returns None for
    /// unknown / invalidated tokens; callers map that to the
    /// protocol-appropriate fallback (Google-family listeners
    /// fall back to the primary account when the bearer is
    /// missing or unknown, matching today's no-auth baseline).
    pub fn account_for_token(&self, token: &str) -> Option<String> {
        self.0
            .lock()
            .expect("oauth token store poisoned")
            .tokens
            .get(token)
            .map(|m| m.account_id.clone())
    }

    /// Remove a token. Returns `true` if it was present.
    pub fn invalidate(&self, token: &str) -> bool {
        self.0
            .lock()
            .expect("oauth token store poisoned")
            .tokens
            .remove(token)
            .is_some()
    }

    /// Reset to the post-load baseline: no tokens, counter back to
    /// zero. Wired through `POST /test/fixture/reset` so harness
    /// scripts get deterministic token strings across cohort
    /// cycles.
    ///
    /// Determinism gotcha: after `clear()` the next `mint()` call
    /// reproduces the *same* token string as before the reset
    /// (counter back to 0 + same body hash). That is the contract -
    /// harness scripts get byte-identical wire output across
    /// resets - but a stale assertion holding the old string and
    /// asserting it is invalid will silently re-validate if the
    /// caller mints with the same body. Tests that need
    /// resurrected-token detection should compare token identity
    /// against a `snapshot()` taken before the reset.
    pub fn clear(&self) {
        let mut inner = self.0.lock().expect("oauth token store poisoned");
        inner.tokens.clear();
        inner.counter = 0;
    }

    pub fn active_count(&self) -> usize {
        self.0
            .lock()
            .expect("oauth token store poisoned")
            .tokens
            .len()
    }
}

/// FNV-1a 64-bit. Tiny, fast, and avoids pulling another crate in
/// for what is just a deterministic body fingerprint.
///
/// Single-pass over `&body`; `parse_token_body` parses the same
/// bytes separately at the call site (the buffer is already in
/// memory by then). Reviewers occasionally suspect a double scan,
/// so the layout is documented here.
///
/// Security note: FNV-1a is reversible. Anyone holding a minted
/// token can recover (or brute-force) the originating request
/// body, which contains `client_secret` if the client sent one.
/// This is acceptable because saehrimnir is a test mock bound to
/// loopback and the body is bounded by axum's default extractor
/// cap, but do not lift this scheme into a real-auth context.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

// ── HTTP handlers ───────────────────────────────────────────────────

/// `POST /oauth/token`. Accepts form-encoded
/// (`application/x-www-form-urlencoded`) or JSON body. Both
/// `grant_type=authorization_code` and `grant_type=refresh_token`
/// are recognised; v0 doesn't distinguish their behaviours beyond
/// echoing the grant type into token metadata.
#[derive(Clone)]
pub struct TokenEndpointState {
    pub fixture: crate::shared::FixtureHandle,
    pub store: TokenStore,
}

pub async fn token_endpoint(
    State(state): State<TokenEndpointState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let form = parse_token_body(&headers, &body);
    let grant_type = form
        .get("grant_type")
        .map(String::as_str)
        .unwrap_or("");
    if grant_type != "authorization_code" && grant_type != "refresh_token" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "unsupported_grant_type",
                "error_description":
                    "v0 supports authorization_code and refresh_token grants only"
            })),
        )
            .into_response();
    }

    // Resolve the account the token is for. An explicit `account_id`
    // (form or JSON) wins and selects a non-primary account; unknown
    // account_id rejects with 400. Real OAuth providers don't expose
    // an `account_id` knob on the token endpoint, but the wire shape
    // is invisible to clients that don't set it.
    //
    // Without an explicit account_id, a `refresh_token` grant resolves
    // the account from the *presented refresh token*: at mint time the
    // refresh token was bound to its account, and real Google likewise
    // derives the account from the refresh token, not a side channel.
    // ratatoskr's refresh request carries only refresh_token +
    // client_id + grant_type, so this lookup is the only way a
    // secondary account's refresh mints a secondary-bound access token
    // rather than falling back to primary (which would let the
    // secondary read the primary's mailbox). Missing / unknown refresh
    // tokens fall back to primary, matching the no-auth baseline.
    let fixture = state.fixture.read().expect("fixture lock poisoned");
    let account_id = match form.get("account_id").map(String::as_str) {
        Some(id) if !id.is_empty() => match fixture.account(id) {
            Some(a) => a.id.clone(),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": "invalid_request",
                        "error_description": format!(
                            "account_id {id:?} does not match any declared account"
                        ),
                    })),
                )
                    .into_response();
            }
        },
        _ => form
            .get("refresh_token")
            .and_then(|rt| state.store.account_for_token(rt))
            .filter(|id| fixture.account(id).is_some())
            .unwrap_or_else(|| fixture.primary_account().id.clone()),
    };
    drop(fixture);

    let body_hash = fnv1a64(&body);
    let access_token = state.store.mint(grant_type, &account_id, body_hash);
    // Refresh tokens are issued for both grant types so a client
    // that exchanges a code can immediately turn around and refresh
    // - mirrors real provider behaviour. The refresh token is
    // *also* registered in the same store; this is loose by real
    // OAuth standards but matches "any well-formed token works"
    // mock semantics. The refresh token inherits the access
    // token's account binding.
    let refresh_token = state
        .store
        .mint("refresh", &account_id, body_hash.rotate_left(1));

    (
        StatusCode::OK,
        Json(json!({
            "access_token": access_token,
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": refresh_token,
            "scope": "openid email profile",
        })),
    )
        .into_response()
}

fn parse_token_body(headers: &HeaderMap, body: &[u8]) -> HashMap<String, String> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if content_type.starts_with("application/json") {
        if let Ok(v) = serde_json::from_slice::<Value>(body)
            && let Some(obj) = v.as_object()
        {
            return obj
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect();
        }
        HashMap::new()
    } else {
        // application/x-www-form-urlencoded (or unknown). Lenient
        // parser: split on `&`, then `=`, percent-decode each side.
        // Bespoke instead of pulling in `form_urlencoded` so the
        // OAuth module stays dependency-free; keep it under 30
        // lines (urldecode + hex_nibble + this) so the cost-of-
        // ownership case for the inline implementation stays
        // honest. Behaviour deviates from the URL spec on
        // malformed `%XY` sequences (we pass them through as
        // literal bytes) - real ratatoskr never sends those.
        let s = std::str::from_utf8(body).unwrap_or("");
        s.split('&')
            .filter(|p| !p.is_empty())
            .filter_map(|p| {
                let (k, v) = p.split_once('=')?;
                Some((urldecode(k), urldecode(v)))
            })
            .collect()
    }
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = hex_nibble(bytes[i + 1]);
                let lo = hex_nibble(bytes[i + 2]);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h << 4) | l);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[derive(Clone)]
pub struct UserInfoState {
    pub fixture: crate::shared::FixtureHandle,
    pub store: TokenStore,
}

/// `GET /oauth/userinfo`. Reads the bearer, returns the fixture
/// account's identity claims. 401 if no token, unknown token, or
/// invalidated token.
pub async fn userinfo_endpoint(
    State(state): State<UserInfoState>,
    headers: HeaderMap,
) -> Response {
    let Some(token) = bearer_token(&headers) else {
        return unauthorized("missing or malformed Authorization header");
    };
    let Some(account_id) = state.store.account_for_token(&token) else {
        return unauthorized("token is unknown or has been invalidated");
    };

    // Serve the token's account claims, not the primary's.
    // `email` and `name` source from `account.name` (loader
    // rejects non-email-shaped names so the claim is safe).
    let fixture = state.fixture.read().expect("fixture lock poisoned");
    let acct = fixture.account(&account_id).unwrap_or_else(|| fixture.primary_account());
    Json(json!({
        "sub": acct.id,
        "email": acct.name,
        "email_verified": true,
        "name": acct.name,
        "iss": fixture.oauth.issuer,
    }))
    .into_response()
}

fn unauthorized(detail: &str) -> Response {
    // `WWW-Authenticate: Bearer` (bare). RFC 6750 §3 expects the
    // full form `Bearer realm="...", error="invalid_token",
    // error_description="..."`; ratatoskr accepts the bare form.
    // Lift to the full form when an interop test or a more
    // pedantic OAuth client points at the mock.
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        Json(json!({
            "error": "invalid_token",
            "error_description": detail,
        })),
    )
        .into_response()
}

/// `POST /test/oauth/invalidate`. Body: `{"token": "..."}`. 204 on
/// success, 404 if the token wasn't active (helps tests catch
/// typos / double-invalidations).
#[derive(Deserialize)]
pub struct InvalidateBody {
    pub token: String,
}

pub async fn invalidate_endpoint(
    State(store): State<TokenStore>,
    Json(body): Json<InvalidateBody>,
) -> Response {
    if store.invalidate(&body.token) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "unknown_token",
                "error_description": "token was not active",
            })),
        )
            .into_response()
    }
}

// ── Bearer enforcement helpers ──────────────────────────────────────

/// Extract the raw token from an `Authorization: Bearer <token>`
/// header, if present and well-formed.
pub fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let trimmed = raw.trim();
    let token = trimmed.strip_prefix("Bearer ")?.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// Decision for a request hitting one of the bearer-enforced
/// listeners (JMAP / Graph / Gmail).
pub enum BearerDecision {
    /// Enforcement is off, or the token is active. Proceed.
    Allow,
    /// Enforcement is on and the request lacks a valid token.
    /// Caller should emit a protocol-shaped 401.
    Deny(String),
}

/// Resolve the account the request's bearer token authorizes,
/// falling back to the fixture's primary account when no bearer
/// is present, the token is unknown / invalidated, or the token
/// points at an account the fixture no longer declares.
///
/// Used by the Google-family listeners (Gmail, gcal, People) to
/// pick which account's resources to serve. Real Google's
/// `users/me` placeholder means "the account this OAuth token
/// was minted for"; the mock honours that by reading the bearer
/// and looking the account up in the token store. Fixtures that
/// don't bind a token (the no-auth baseline) keep getting
/// primary, matching v0 behaviour.
pub fn account_from_bearer(
    fixture: &Fixture,
    store: &TokenStore,
    headers: &HeaderMap,
) -> String {
    if let Some(token) = bearer_token(headers)
        && let Some(account_id) = store.account_for_token(&token)
        && fixture.account(&account_id).is_some()
    {
        return account_id;
    }
    fixture.primary_account().id.clone()
}

pub fn check_bearer(
    fixture: &Fixture,
    store: &TokenStore,
    headers: &HeaderMap,
) -> BearerDecision {
    if !fixture.oauth.enforce {
        return BearerDecision::Allow;
    }
    match bearer_token(headers) {
        None => BearerDecision::Deny(
            "missing or malformed Authorization: Bearer header".into(),
        ),
        Some(t) if !store.is_active(&t) => {
            BearerDecision::Deny("token is unknown or has been invalidated".into())
        }
        Some(_) => BearerDecision::Allow,
    }
}

// ── Serialisable views for tests ────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct TokenSnapshot {
    pub token: String,
    pub grant_type: String,
    pub account_id: String,
}

impl TokenStore {
    /// Snapshot of every active token. Used by tests; not exposed
    /// over HTTP (would leak tokens to anyone who can reach the
    /// listener, even in the test mock).
    pub fn snapshot(&self) -> Vec<TokenSnapshot> {
        self.0
            .lock()
            .expect("oauth token store poisoned")
            .tokens
            .iter()
            .map(|(k, v)| TokenSnapshot {
                token: k.clone(),
                grant_type: v.grant_type.clone(),
                account_id: v.account_id.clone(),
            })
            .collect()
    }
}
