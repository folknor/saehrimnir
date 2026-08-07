-- Same scenario as fixtures/discovery-small.toml. Drives the
-- WebFinger / OIDC discovery / Mozilla autoconfig routes mounted
-- on the JMAP HTTP listener. See reference/ratatoskr-discovery-surface.md.

fixture({
  name = "discovery-small",
})

account({
  id = "account-1",
  name = "user@corp.test",
})

discovery({
  prefix = "corp.test",
  webfinger = {
    links = {
      { rel = "http://openid.net/specs/connect/1.0/issuer", href = "/idp/realms/corp" },
    },
  },
  oidc = {
    issuer = "/corp.test",
    authorization_endpoint = "/oauth/authorize",
    token_endpoint = "/oauth/token",
    scopes_supported = { "openid", "email" },
    code_challenge_methods_supported = { "S256" },
    token_endpoint_auth_methods_supported = { "none" },
  },
  -- dellingr's lexer does not support Lua `[[...]]` long-string
  -- syntax; the autoconfig XML is built up with explicit newlines
  -- via string concatenation so the Lua and TOML fixtures
  -- normalise byte-identically.
  autoconfig = {
    raw_body =
      "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n" ..
      "<clientConfig version=\"1.1\">\n" ..
      "  <emailProvider id=\"corp.test\">\n" ..
      "    <domain>corp.test</domain>\n" ..
      "    <displayName>Corp Mail</displayName>\n" ..
      "    <displayShortName>Corp</displayShortName>\n" ..
      "    <incomingServer type=\"imap\">\n" ..
      "      <hostname>imap.corp.test</hostname>\n" ..
      "      <port>143</port>\n" ..
      "      <socketType>STARTTLS</socketType>\n" ..
      "      <authentication>OAuth2</authentication>\n" ..
      "    </incomingServer>\n" ..
      "    <outgoingServer type=\"smtp\">\n" ..
      "      <hostname>smtp.corp.test</hostname>\n" ..
      "      <port>587</port>\n" ..
      "      <socketType>STARTTLS</socketType>\n" ..
      "      <authentication>OAuth2</authentication>\n" ..
      "    </outgoingServer>\n" ..
      "  </emailProvider>\n" ..
      "  <oAuth2>\n" ..
      "    <issuer>${BASE}/idp/realms/corp</issuer>\n" ..
      "  </oAuth2>\n" ..
      "</clientConfig>\n",
  },
})

discovery({
  prefix = "idp/realms/corp",
  oidc = {
    issuer = "/idp/realms/corp",
    authorization_endpoint = "/oauth/authorize",
    token_endpoint = "/oauth/token",
    userinfo_endpoint = "/oauth/userinfo",
    scopes_supported = { "openid", "email", "profile", "offline_access" },
    code_challenge_methods_supported = { "S256" },
    token_endpoint_auth_methods_supported = { "none" },
  },
})

discovery({
  prefix = "malformed-jrd.test",
  webfinger = {
    raw_body = "this is not json {{[",
    raw_content_type = "application/jrd+json",
  },
})

discovery({
  prefix = "insecure-href.test",
  webfinger = {
    links = {
      { rel = "http://openid.net/specs/connect/1.0/issuer", href = "http://insecure.example/issuer" },
    },
  },
})

discovery({
  prefix = "wrong-issuer.test",
  oidc = {
    issuer = "/something-else",
    authorization_endpoint = "/oauth/authorize",
    token_endpoint = "/oauth/token",
    scopes_supported = { "openid" },
    code_challenge_methods_supported = { "S256" },
    token_endpoint_auth_methods_supported = { "none" },
  },
})
