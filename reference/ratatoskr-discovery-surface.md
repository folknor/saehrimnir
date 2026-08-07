# Account-discovery surface

Ratatoskr (`crates/core/src/discovery/`) runs a multi-stage cascade
when a user adds an account: hardcoded registry -> Mozilla autoconfig
XML -> MX -> `.well-known/jmap` -> TCP port probe -> OIDC discovery ->
WebFinger -> chained OIDC. Sæhrimnir's discovery routes give the
Lua harness a way to exercise the network-driven stages
(autoconfig / `.well-known/jmap` / OIDC discovery / WebFinger)
without standing up a real IdP.

All three discovery routes mount on the JMAP HTTP listener, share
its base URL, and are unauthenticated (real-world discovery is a
public surface; bearer-gating would lock the cascade out of its
own bootstrap).

## URL shape

Ratatoskr's `RATATOSKR_TEST_DISCOVERY_BASE` rewrite turns
`https://{domain}/.well-known/...` into
`${BASE}/{domain}/.well-known/...` and relaxes the
`https_only(true)` check on the `reqwest` client (gated on the env
var being set; production builds are unaffected). That means the
request URL sæhrimnir actually sees is path-prefixed by either the
domain or, for WebFinger-chained OIDC discovery, the path component
of the JRD's `href`.

| Stage             | Request path                                              | Handler                          |
|-------------------|-----------------------------------------------------------|----------------------------------|
| WebFinger         | `GET /{prefix}/.well-known/webfinger?resource=...&rel=...`| `discovery::webfinger`           |
| OIDC discovery    | `GET /{prefix}/.well-known/openid-configuration`          | `discovery::openid_configuration`|
| Autoconfig (XML)  | `GET /{prefix}/mail/config-v1.1.xml`                      | `discovery::autoconfig`          |

Axum 0.8 only allows the `{*name}` wildcard in terminal position,
so the three shapes share one `/{*discovery_path}` route that
suffix-matches in `discovery::dispatch` before fanning out. Paths
that don't end with a known suffix fall through to a plain 404 -
the catch-all does not shadow the unrelated JMAP routes
(`/jmap/session`, `/.well-known/jmap`, etc.) because literal
segments win over wildcards in axum's matcher.

## Fixture format

A single `[discovery."<prefix>"]` table per path-prefix, with three
optional nested docs. The prefix is the URL segment(s) between the
listener root and the well-known suffix - typically the bare
domain (`corp.test`) for the direct stages, or a sub-path
(`idp/realms/corp`) for the chained OIDC issuer the WebFinger
response points at. Prefixes are stored as opaque strings; the
runtime does a flat compare against the URL-derived prefix and
neither the loader nor the route handler cares whether a given
prefix "looks like" a domain.

```toml
# WebFinger at the bare domain, advertising the chained OIDC issuer.
[discovery."corp.test".webfinger]
links = [
  { rel = "http://openid.net/specs/connect/1.0/issuer", href = "/idp/realms/corp" },
]

# OIDC discovery at the chained issuer path.
[discovery."idp/realms/corp".oidc]
issuer = "/idp/realms/corp"
authorization_endpoint = "/oauth/authorize"
token_endpoint = "/oauth/token"
userinfo_endpoint = "/oauth/userinfo"   # optional
scopes_supported = ["openid", "email", "profile", "offline_access"]
code_challenge_methods_supported = ["S256"]
token_endpoint_auth_methods_supported = ["none"]

# Mozilla autoconfig XML.
[discovery."corp.test".autoconfig]
raw_body = "<?xml ...?>\n<clientConfig version=\"1.1\">...</clientConfig>"
raw_content_type = "application/xml"  # optional
```

Three of the fields (`issuer`, `authorization_endpoint`,
`token_endpoint`, plus `userinfo_endpoint` if present, plus every
WebFinger `links[].href`) accept either an absolute URL
(`http://...` / `https://...`) or a path-relative string starting
with `/`. Path-relative values are prefixed with the live listener
base URL at emit time so fixtures stay free of bound-port coupling.
Absolute URLs are passed through verbatim - the contract negative
tests rely on (a `http://` href in a JRD must remain `http://` in
the wire response so ratatoskr's scheme check rejects it).

`${BASE}` substring substitution applies inside the autoconfig
`raw_body` only. v0 hand-authors IMAP / SMTP host:port in the
autoconfig XML; the port plumbing for ratatoskr stage 2 hasn't
landed yet so the values aren't load-bearing.

### Negative-test escape hatches

| Field                        | Purpose                                                                                  |
|------------------------------|------------------------------------------------------------------------------------------|
| `webfinger.raw_body`         | Serves the literal string instead of computing a JRD. Drives the malformed-JSON path.    |
| `webfinger.raw_content_type` | Overrides the default `application/jrd+json`. Asserts client tolerates `application/json` and rejects `text/html`. |
| `oidc.raw_body`              | Same for the OIDC discovery doc. Drives the malformed-JSON path.                         |
| `oidc.raw_content_type`      | Overrides `application/json`.                                                            |
| `autoconfig.raw_content_type`| Overrides the default `application/xml`.                                                 |

The loader does NOT enforce OIDC's issuer-self-claim
(`issuer == request_url - "/.well-known/openid-configuration"`) -
so a fixture can stage `prefix = "wrong-issuer.test"` with
`oidc.issuer = "/something-else"` and the route serves the
mismatch verbatim. That's the test contract: ratatoskr's probe
does the comparison; sæhrimnir's job is to give it the document.

## Lua surface

```lua
discovery({
  prefix = "corp.test",
  webfinger = {
    links = {
      { rel = "http://openid.net/specs/connect/1.0/issuer", href = "/idp/realms/corp" },
    },
  },
  oidc = { ... },        -- optional
  autoconfig = { ... },  -- optional
})
```

Calling `discovery` twice with the same prefix is rejected. At
least one of `webfinger` / `oidc` / `autoconfig` must be present.

No reactive `on(...)` callback surface for discovery: the cascade
runs once at account setup and is not re-entered mid-test, so
static per-fixture variants cover every negative case worth
authoring.

## Request log

Every discovery response appends one `RequestEntry` with
`protocol = "discovery"` and `command = "webfinger" |
"openid-configuration" | "autoconfig"`. `detail` carries
`prefix` always, and `resource` / `rel` when present on the
WebFinger query string. The cross-connection grouping rules from
`request-log.md` apply: each accepted TCP socket gets a fresh
`connection_id`, so `tower::oneshot`-driven tests see `null` ids
while live-binary harness scripts see a per-request id.

## What sæhrimnir intentionally does NOT do

- No DNS / MX simulation - ratatoskr's stage 3 is unit-tested
  in isolation. Plumbing fake MX records through sæhrimnir would
  require a fake resolver, which is a larger surface than the
  test value warrants.
- No port-probe simulation. Stage 5 (TCP probes against ports
  143/993/465/587/...) is already covered by the listener-bound
  ports themselves: a fixture that binds IMAP and SMTP will pass
  ratatoskr's probe naturally.
- No IMAP / SMTP host:port emission in the autoconfig XML. v0
  leaves these hand-authored; the substitution mechanism can grow
  later if ratatoskr stage 2 starts running against sæhrimnir.
