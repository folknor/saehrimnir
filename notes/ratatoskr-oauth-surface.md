# OAuth surface

Mock OAuth 2.0 provider mounted on the JMAP HTTP listener (the
JMAP port from the readiness sentinel). Designed to unblock
ratatoskr's `oauth.exchange_code` headless drive: the harness can
pass `token_url = http://<jmap_endpoint>/oauth/token` and
`user_info_url = http://<jmap_endpoint>/oauth/userinfo` directly,
no separate listener needed.

The mock is permissive by design - any well-formed request body
is accepted, no `client_id` / `client_secret` / `redirect_uri`
validation. The point is to exercise ratatoskr's code paths, not
to certify against a real provider.

## Endpoints

### `POST /oauth/token`

Accepts both `application/x-www-form-urlencoded` and
`application/json` bodies. Recognised `grant_type` values:

- `authorization_code` - the harness drove `oauth.exchange_code`
  and wants to swap a synthetic auth code for tokens.
- `refresh_token` - the harness is rotating tokens.

Any other `grant_type` -> 400 with
`{"error": "unsupported_grant_type", ...}`.

A `refresh_token` grant whose `refresh_token` has been REVOKED (see
`/test/oauth/invalidate` below) -> 400 with
`{"error": "invalid_grant", "error_description": "refresh token has
been revoked"}`, minting nothing. This is the only way a refresh
fails: an unknown-but-not-revoked refresh token still succeeds (it
falls back to the primary account, matching the permissive baseline).
The rejection is what makes an unrecoverable provider auth failure
reachable - without it, bifrost's OAuthRefresher always recovers.

Optional `account_id` field (form or JSON) binds the minted token
to a specific declared `[[account]]`. Absent / empty defaults to
the primary account. An unknown id -> 400 with
`{"error": "invalid_request", ...}`. The Google-family listeners
(Gmail, gcal, People) use the token's account on every read, so
this knob is what unlocks Gmail / gcal / People multi-account
testing. Real OAuth providers don't expose this knob - clients
that don't set it get the same single-account behaviour as
before.

Successful response (200):

```json
{
  "access_token": "mock-access-1-{fnv1a64-hex}",
  "token_type": "Bearer",
  "expires_in": 3600,
  "refresh_token": "mock-access-2-{fnv1a64-hex}",
  "scope": "openid email profile"
}
```

Token strings are deterministic per `(reset-window, body-hash)`:
the counter portion advances per mint, but a fresh
`POST /test/fixture/reset` resets it to 0. `expires_in` is pinned
at 3600; the mock never advances time so tokens never expire on
their own.

### `GET /oauth/userinfo`

Reads `Authorization: Bearer <token>`. 401 if missing or unknown
or invalidated. 200 returns the token's account claims:

```json
{
  "sub": "<token-account.id>",
  "email": "<token-account.name>",
  "email_verified": true,
  "name": "<token-account.name>",
  "iss": "<fixture.oauth.issuer>"
}
```

Email and name both source from the token's account `name` field
(which is email-shaped, see `notes/fixture-format.md`). Tokens
minted without an `account_id` form field default to the primary
account, so single-account fixtures see no behaviour change.

### `POST /test/oauth/invalidate`

Test-only admin route. Body `{"token": "..."}`. Removes the token
from the active set so subsequent userinfo / bearer-enforced
requests reject it.

- 204 on success
- 404 if the token wasn't active

Revocation cascade: invalidating a token also REVOKES the associated
refresh token(s). Invalidating an access token revokes the refresh
token minted alongside it on the same `/oauth/token` response;
invalidating a refresh token revokes it directly. A revoked refresh
token is rejected by the `refresh_token` grant with
`400 invalid_grant` (see `/oauth/token` above). This is what lets a
gate force an unrecoverable auth failure: the gate invalidates the
account's access token, bifrost's OAuthRefresher then tries a
`refresh_token` grant, and that grant now fails instead of silently
minting a fresh access token. Only explicitly invalidated refresh
tokens are revoked; an unknown-but-not-revoked refresh token keeps
the primary fallback, so the token-rotation flows other tests rely
on are undisturbed. `POST /test/fixture/reset` clears the revoked
set along with the active tokens.

## Bearer enforcement on mail listeners

Gated by `fixture.oauth.enforce`. Default is `false` so existing
fixtures keep behaving like the v0 "no auth" baseline.

When `true`:

- JMAP `/jmap/session`, `/jmap/api`, `/jmap/download/*` -> 401
  with `{"type": "urn:ietf:params:jmap:error:forbidden",
  "status": 401, "detail": "..."}` if no valid bearer.
- Microsoft Graph `/v1.0/...` -> 401 with the Graph error envelope
  `{"error": {"code": "InvalidAuthenticationToken", "message": "..."}}`.
- Gmail `/gmail/v1/...` -> 401 with the Gmail error envelope
  (reason: `authError`).
- CalDAV (every verb on the CalDAV listener) -> 401 with empty
  body and a `WWW-Authenticate: Bearer` header. CalDAV has no
  shared response-body schema, so the rejection is the bare HTTP
  `401`; clients identify the listener by header rather than by
  body shape.
- IMAP and SMTP are unaffected; they have their own auth surfaces.

The bearer must be present in the `TokenStore`. Tokens get there
either by minting via `/oauth/token` or by tests calling
`store.mint(...)` directly. `POST /test/fixture/reset` clears the
store along with the request and submission logs.

## Things ratatoskr should ignore (or that are out of scope)

- No `/oauth/authorize` endpoint. Real auth-code grants start with
  a browser redirect; the mock skips the dance entirely and accepts
  any `code` value at `/oauth/token`. Drive headless via the
  harness; do not expect to point a browser at saehrimnir.
- No `/oauth/jwks`, no signed JWTs. Tokens are opaque strings.
  ratatoskr's OAuth client treats them as opaque too, so there's
  nothing to verify.
- No PKCE, no state-parameter checking, no nonce echo. Add when a
  fixture forces it.
- No multiple-account-per-token. A minted token names one account
  for its lifetime. Re-binding a token to a different account
  isn't on the wire; mint a fresh token instead.
