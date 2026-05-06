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
- v0 mock: plaintext only. No STARTTLS, no TLS.

## Authentication

- Three mechanisms attempted, gated on `config.auth_method`:
  XOAUTH2 (when `auth_method == "oauth2"`), otherwise PLAIN and LOGIN
  in that order. Source: `client.rs:28-32`.
- v0 mock: accept any credential under any mechanism, respond `235`,
  forget the username.

## EHLO / capabilities

- Client sends EHLO with the configured hostname. `lettre` parses the
  capability list but ratatoskr's code does not actively probe any
  extension.
- Capabilities the mock advertises (just enough for `lettre` to be
  happy):
  - `SIZE 52428800` - large enough for any fixture.
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

- Sender (the envelope address from MAIL FROM).
- Recipients (the list from RCPT TO commands).
- Raw message bytes between DATA and the terminator (the full RFC
  822 message).
- Per-connection: which AUTH mechanism, which (raw) credential
  string. Useful for tracing OAuth-vs-password regressions even
  though we never validate.
- Connection time / submission count - cheap, lets tests assert
  timing and ordering across multiple submissions.

The client never reads back a Message-ID or any DSN handle, so the
mock does not have to generate one.

## Out of scope for v0

- STARTTLS, direct TLS.
- CHUNKING / BDAT - client uses DATA.
- DSN (Delivery Status Notification), NOTIFY, ORCPT.
- SIZE negotiation - client doesn't check before sending.
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
