# Ratatoskr's SMTP client surface

What the v0 SMTP mock has to satisfy. Distilled from
`<ratatoskr>/crates/smtp/` on 2026-05-06. Source-of-truth lives there;
this file is a cheat sheet so we don't have to fan out every turn.

Ratatoskr's SMTP client is thin - it delegates to `lettre`'s
`AsyncSmtpTransport` for the wire protocol and only owns the
configuration plumbing and the From/To/Cc/Bcc extraction from the
outbound message. That makes the surface small.

## Connection lifecycle

- Client connects via TCP with security mode: `"tls"` (direct TLS,
  port 465), `"starttls"` (plain to upgrade, port 587), or `"none"`
  (plaintext, port 25). Source: `client.rs:34-79`.
- Server greeting line required before any commands. `lettre` parses
  any `220 ...` greeting; mock will emit
  `220 saehrimnir ESMTP ready\r\n`.
- v0 mock: plaintext + STARTTLS. The acceptor self-signs an ephemeral
  cert at startup (subject alt names: `localhost`, `saehrimnir.test`)
  and offers `STARTTLS` in the EHLO capability list. Clients must
  accept invalid certs - typical for a test mock. No direct TLS on
  port 465 (`lettre`'s `tls`/`starttls` modes both work, but the
  former needs a connected TLS handshake before the greeting which
  v0 does not support).

## Authentication

- Three mechanisms attempted, gated on `config.auth_method`:
  XOAUTH2 (when `auth_method == "oauth2"`), otherwise PLAIN and LOGIN
  in that order. Source: `client.rs:28-32`.
- v0 mock: every credential succeeds (no validation). Stage 5 of
  the multi-account refactor wires **per-connection account
  binding**: the SASL response is parsed and matched against the
  fixture's declared `[[account]]`s. A match rebinds the
  connection state; the resulting `Submission` lands tagged with
  the resolved `account_id` (exposed via
  `GET /test/smtp/submissions`). An unrecognised credential (or
  no AUTH at all) leaves the submission on primary, matching the
  v0 no-auth baseline. Parsing rules:
  - **PLAIN**: base64 of `\0user\0pass`. `user` matched case-
    insensitively against `account.name`.
  - **LOGIN**: the single continuation line is treated as the
    username; the second `Password:` round-trip isn't modelled.
  - **XOAUTH2 / OAUTHBEARER**: scan the `\x01`-separated blob
    for `auth=Bearer <token>` (looked up in the OAuth
    `TokenStore` shared with the Google-family listeners); on
    no bearer match, fall back to the `user=` field.
- Lua fixtures can inject AUTH-time failures via
  `on("smtp", "AUTH", fn)`. The hook fires after the SASL
  response is read but before the connection binds an account;
  `req.payload` carries the mechanism name so the script can
  reject selectively (e.g. fail XOAUTH2 while letting PLAIN
  pass). Returning `{ status = "535", message = "..." }`
  emits `535 ...\r\n` instead of `235 authentication accepted`
  and leaves the connection unauthenticated. The decoded SASL
  response itself is deliberately NOT exposed to callbacks
  (credentials).

## EHLO / capabilities

- Client sends EHLO with the configured hostname. `lettre` parses the
  capability list but ratatoskr's code does not actively probe any
  extension.
- Capabilities the mock advertises (just enough for `lettre` to be
  happy):
  - `SIZE 268435456` (256 MiB) - well above any message a gate
    sends. bifrost-smtp parses the advertised SIZE limit and refuses
    (client-side, before DATA) any message larger than it, so the
    old 50MB limit fast-failed a ~67MB send (50MB attachment) with a
    transport.network error. The mock never enforces this; it buffers
    and accepts the full DATA regardless.
  - `8BITMIME` - so we don't have to advertise a quoted-printable
    transformation.
  - `AUTH PLAIN LOGIN XOAUTH2` - for the three mechanisms above.
- NOT advertised: `CHUNKING`, `BURL`, `DSN`, `STARTTLS`, `PIPELINING`.

## Submission flow

Per outbound message, exactly:

1. Greeting (`220`) on connect.
2. EHLO from client, mock answers with the capability list.
3. AUTH `<mechanism>` `<base64-creds>` (mock returns `235 OK`).
4. MAIL FROM:`<sender@x>` (mock returns `250 OK`).
5. RCPT TO:`<r1@x>` repeated per recipient (mock returns `250 OK`).
6. DATA -> mock returns `354 send data` -> client streams the RFC
   822 message terminated by `\r\n.\r\n` -> mock returns `250 OK
   queued`.
7. QUIT -> mock returns `221 bye` and closes.

No RSET in the read path. One message per connection.

Recipients come from To, Cc, Bcc headers extracted client-side
(`client.rs:104-135`); the mock just sees them as RCPT TO commands.

## Response codes the mock has to emit

The `lettre` parser is strict on the leading numeric code; everything
after the code is informational. Codes ratatoskr's path needs:

- `220` connection greeting.
- `250` EHLO and per-step success.
- `235` AUTH success.
- `354` DATA prompt.
- `221` QUIT goodbye.

Errors are not exercised by the happy path. v0 may emit `500` /
`501` / `503` on protocol violations, but no test currently asserts
on them.

## What the mock must capture

For tests to verify:

- Sender address (from MAIL FROM, with extension parameters stripped).
- Recipient addresses (from RCPT TO commands, also stripped).
- Extension parameters: `MAIL FROM` params land in
  `Submission::from_params` (BTreeMap, keys upper-cased: `SIZE`,
  `BODY`, `ENVID`, `RET`, ...); `RCPT TO` params land in
  `Submission::rcpt_params` parallel to `recipients` (one BTreeMap
  per recipient with `NOTIFY`, `ORCPT`, ...). Bare flags without `=`
  are stored with empty string values. Unknown keys are accepted
  silently.
- Raw message bytes between DATA and the terminator (the full RFC
  822 message). `Submission::parse_mime()` projects these into a
  flat `ParsedSubmission { subject, text_bodies, html_bodies,
  attachments }` so tests can assert without re-implementing a
  parser.
- Per-connection: which AUTH mechanism, which (raw) credential
  string. Useful for tracing OAuth-vs-password regressions even
  though we never validate.
- Connection time / submission count - cheap, lets tests assert
  timing and ordering across multiple submissions.

Captured submissions are exposed to harness scripts over the JMAP
HTTP listener as a test-only route:

- `GET /test/smtp/submissions` -> JSON array of submissions in the
  order they were received. Each entry carries the connection-level
  fields (`from`, `recipients`, `from_params`, `rcpt_params`,
  `auth_mechanism`, `received_at`, `raw_size`) plus an optional
  `parsed` object derived from `Submission::parse_mime()`
  (`subject`, `text_body_count`, `html_body_count`, and an
  `attachments` array of `{filename, content_type, size}`). Raw
  message bytes are deliberately not serialized; tests assert on
  `raw_size` and per-attachment `size` instead.
- `DELETE /test/smtp/submissions` -> `204 No Content`; clears the
  log so tests can assert "no other submission landed in this
  window" without restarting the binary.

Process-scoped: a fresh sæhrimnir start is always an empty log.
No auth or feature gate guards the route - sæhrimnir is a test-only
binary.

The client never reads back a Message-ID or any DSN handle, so the
mock does not have to generate one.

## Out of scope for v0

- Direct TLS on port 465 (STARTTLS is wired; the bare-TLS path is
  not).
- CHUNKING / BDAT - client uses DATA.
- DSN result codes back to the client (NOTIFY/ORCPT are captured but
  the mock never emits a DSN).
- SIZE negotiation - we advertise `SIZE 268435456` (256 MiB) and
  capture the `SIZE=` param if sent, but do not enforce.
- PIPELINING - `lettre` sends sequentially.
- VRFY / EXPN / HELP / NOOP (we'll accept NOOP for politeness, the
  others stay BAD).
- Multiple submissions per connection. Doable trivially via RSET
  but no fixture currently needs it.

## Wire format strictness

- Line endings: CRLF. `lettre` enforces it on send; mock must emit
  it on every reply.
- Casing: SMTP verbs are case-insensitive. Local parts in addresses
  are technically case-sensitive but in practice we just round-trip
  the bytes the client sends.
- Response continuation: a multi-line reply uses `<code>-text` for
  each line except the last, which uses `<code> text`. The mock's
  EHLO reply is the only multi-line response we emit.
- DATA terminator: a line containing only `.\r\n` ends the message.
  Dot-stuffing (a leading `..` on a body line) is reversed by the
  receiver - we will reverse it during capture.
