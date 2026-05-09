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
or invalidated. 200 returns:

```json
{
  "sub": "<fixture.account.id>",
  "email": "<fixture.account.name>",
  "email_verified": true,
  "name": "<fixture.account.name>",
  "iss": "<fixture.oauth.issuer>"
}
```

Email and name both default to `account.name` (which is itself
email-shaped in v0, see `notes/fixture-format.md`).

### `POST /test/oauth/invalidate`

Test-only admin route. Body `{"token": "..."}`. Removes the token
from the active set so subsequent userinfo / bearer-enforced
requests reject it.

- 204 on success
- 404 if the token wasn't active

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
- No multiple-account-per-token. The mock has exactly one
  fixture-side `[account]` in v0, so userinfo always projects the
  same identity.
