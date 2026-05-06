# SMTP mock - implementation plan

Companion to `notes/ratatoskr-smtp-surface.md`. SMTP is a much smaller
protocol than IMAP for our purposes - submission only, one message per
connection, no folder model. The plan is to land it in two commits:
bootstrap + capture.

## Goal

Ratatoskr's outbound submission code, pointed at the SMTP listener,
hands a full RFC 822 message off to the mock. Tests can introspect
the in-memory submission log to verify what was actually sent.

## Out of scope (v0)

- TLS / STARTTLS.
- Multiple submissions per connection. Single-shot is what the client
  does.
- Bounces / DSNs / receipts.
- Real validation: any From / To / RCPT TO is accepted regardless of
  fixture content. The mock is a sink, not a router.
- Outgoing-mail fixture entries. v0 fixtures describe inbound state
  only; submitted messages exist only in-memory and clear on
  shutdown.

## Architecture

Mirrors IMAP:

- `src/smtp.rs` - the whole thing. Listener, connection state machine,
  submission capture.
- Per-connection task on a tokio runtime, generic over
  `AsyncRead+AsyncWrite` so tests can drive a `tokio::io::DuplexStream`
  without a real socket.
- Shared submission log on `AppState`-style: `Arc<Mutex<Vec<Submission>>>`.
  Tests read it directly; production callers (none in v0) ignore it.

## Submission record

```rust
pub struct Submission {
    pub from: String,           // raw `<addr>` payload from MAIL FROM
    pub recipients: Vec<String>,// raw `<addr>` payloads from RCPT TO
    pub auth_mechanism: Option<String>,
    pub data: Vec<u8>,          // full RFC 822 with dot-stuffing reversed
    pub received_at: chrono::DateTime<Utc>, // mock's wall clock
}
```

`received_at` is the only place SMTP needs a clock. The determinism
contract for SMTP is "submission contents are byte-stable" - the
timestamp moves but is not part of the wire output, so tests assert
on `from`, `recipients`, `data`, not on the timestamp.

## Suggested implementation order

1. Listener bootstrap. `--smtp-port`, sentinel line `SMTP <port>`,
   greeting, EHLO with the capability list, NOOP, RSET (no-op),
   QUIT. AUTH and MAIL stubs return `503 bad sequence` until step 2.
2. Submission flow. AUTH (any mechanism, any credential ->
   `235 OK`), MAIL FROM, RCPT TO, DATA + dot-stuffing reversal,
   appending to the shared submission log.
3. Integration tests in `tests/smtp.rs` driving a duplex stream.

## Open questions

- **Multiple submissions per connection.** Skipped; if a fixture ever
  needs it, the connection state machine just needs to accept RSET
  without closing.
- **Per-recipient response codes.** Real SMTP servers can `550
  no such user` per recipient. v0 accepts everything; if we want to
  test ratatoskr's per-recipient error handling later, this is the
  hook.
- **Submission log retention.** In-memory only, cleared at process
  exit. Plan-3 might want a debug HTTP route to read it; deferred.
