//! SMTP submission server.
//!
//! Submission-only, single-message-per-connection. Per-connection
//! state machine driven by [`serve_connection`] over any
//! `AsyncRead + AsyncWrite`. Captured submissions land in the shared
//! [`SubmissionLog`] which the test harness reads directly.
//!
//! v0 scope: plaintext only, accept any credential, one message per
//! connection. STARTTLS, CHUNKING, DSN, and PIPELINING are
//! deliberately not advertised. See
//! `notes/ratatoskr-smtp-surface.md` for the wire surface.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use jiff::Timestamp;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufStream};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_rustls::TlsAcceptor;

/// Marker trait that combines the four bounds we need on the per-
/// connection stream. Lets `Conn` hold a `Box<dyn AsyncStream>` so the
/// underlying type can swap from a plaintext TCP/duplex stream to a
/// `TlsStream` after STARTTLS without each handler being generic.
pub trait AsyncStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> AsyncStream for T {}

/// One captured SMTP submission. Tests read these via
/// [`SubmissionLog::snapshot`].
#[derive(Debug, Clone)]
pub struct Submission {
    /// Address bytes from `MAIL FROM:` - typically `<addr@example.com>`,
    /// angle brackets included. Extension parameters (SIZE, BODY, etc.)
    /// are stripped before storage and surfaced separately as
    /// [`Self::from_params`].
    pub from: String,
    /// Address bytes from each `RCPT TO:` in submission order. Like
    /// [`Self::from`] this is the address-only portion; per-recipient
    /// extension parameters land in [`Self::rcpt_params`].
    pub recipients: Vec<String>,
    /// Extension parameters that followed the `MAIL FROM:` address.
    /// Keys are upper-cased for case-insensitive lookup
    /// (`SIZE`, `BODY`, `ENVID`, ...). Values are the raw RHS bytes;
    /// a bare flag without `=` is stored with an empty string value.
    pub from_params: BTreeMap<String, String>,
    /// Per-recipient extension parameters (`NOTIFY`, `ORCPT`, ...).
    /// Same length as [`Self::recipients`] - index `i` of one matches
    /// index `i` of the other.
    pub rcpt_params: Vec<BTreeMap<String, String>>,
    /// SASL mechanism the client authenticated with, if any.
    pub auth_mechanism: Option<String>,
    /// Account this submission belongs to. Stage 4 of the multi-
    /// account refactor wires AUTH-time account binding: PLAIN /
    /// LOGIN's user, or XOAUTH2 / OAUTHBEARER's bearer-token's
    /// account, picks the declared account when one matches. An
    /// unrecognised credential (or no AUTH at all) leaves the
    /// submission on the fixture's primary account.
    pub account_id: String,
    /// Full RFC 822 message bytes with dot-stuffing reversed (a body
    /// line starting `..` is collapsed to a single leading `.`).
    pub data: Vec<u8>,
    /// Wall-clock time the mock saw the submission complete. Not
    /// byte-stable - the determinism contract covers only the
    /// submission contents (from / recipients / data), not the
    /// timestamp.
    pub received_at: Timestamp,
}

/// Server-side MIME projection of a captured submission. Returned by
/// [`Submission::parse_mime`] so tests can assert on `Subject`, body
/// parts, and attachments without each pulling in `mail-parser`.
#[derive(Debug, Clone, Default)]
pub struct ParsedSubmission {
    pub subject: Option<String>,
    pub text_bodies: Vec<String>,
    pub html_bodies: Vec<String>,
    pub attachments: Vec<ParsedAttachment>,
    /// Every top-level RFC 822 header on the root message part, in
    /// document order, as `(name, value)` pairs. Lets harness gates
    /// assert arbitrary outgoing headers - e.g. the MDN
    /// `Disposition-Notification-To` bifrost's raw-MDN send emits -
    /// without the projection having to enumerate a fixed allowlist.
    /// Values are the decoded header text (RFC 2047 unfolded by
    /// `mail-parser`); duplicate header names appear once per
    /// occurrence.
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct ParsedAttachment {
    pub filename: Option<String>,
    pub content_type: String,
    pub data: Vec<u8>,
    /// The attachment part's `Content-ID` header (angle brackets
    /// stripped), when present. Inline images carry a `Content-ID`
    /// that the HTML body references via `cid:`; a gate asserting an
    /// inline-attachment round-trip checks this is populated.
    pub content_id: Option<String>,
}

impl Submission {
    /// Parse the captured RFC 822 bytes into a flat structure.
    /// Returns `None` if `mail-parser` rejects the bytes outright;
    /// otherwise the parser is lenient enough that anything `lettre`
    /// might emit will round-trip.
    pub fn parse_mime(&self) -> Option<ParsedSubmission> {
        use mail_parser::MimeHeaders;
        let parser = mail_parser::MessageParser::default();
        let msg = parser.parse(&self.data)?;
        let subject = msg.subject().map(str::to_string);
        // Project every top-level header (name, decoded value) in
        // document order so a gate can assert arbitrary outgoing
        // headers (e.g. `Disposition-Notification-To`) without the
        // projection enumerating a fixed allowlist.
        let headers = msg
            .headers_raw()
            .map(|(name, value)| (name.to_string(), value.trim().to_string()))
            .collect::<Vec<_>>();
        let mut text_bodies = Vec::new();
        for i in 0..msg.text_body_count() {
            if let Some(t) = msg.body_text(i) {
                text_bodies.push(t.into_owned());
            }
        }
        let mut html_bodies = Vec::new();
        for i in 0..msg.html_body_count() {
            if let Some(h) = msg.body_html(i) {
                html_bodies.push(h.into_owned());
            }
        }
        let mut attachments = Vec::new();
        for a in msg.attachments() {
            let content_type = match a.content_type() {
                Some(ct) => format!(
                    "{}/{}",
                    ct.c_type,
                    ct.c_subtype.as_deref().unwrap_or("octet-stream")
                ),
                None => "application/octet-stream".to_string(),
            };
            // `Content-ID` round-trips with angle brackets stripped so
            // a gate can match it against the `cid:` reference in the
            // HTML body (which carries the bare token).
            let content_id = a.content_id().map(|cid| {
                cid.trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_string()
            });
            attachments.push(ParsedAttachment {
                filename: a.attachment_name().map(str::to_string),
                content_type,
                data: a.contents().to_vec(),
                content_id,
            });
        }
        Some(ParsedSubmission {
            subject,
            text_bodies,
            html_bodies,
            attachments,
            headers,
        })
    }
}

/// Shared in-memory submission log. Cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct SubmissionLog(Arc<Mutex<Vec<Submission>>>);

impl SubmissionLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, s: Submission) {
        self.0
            .lock()
            .expect("submission log mutex poisoned")
            .push(s);
    }

    /// Return a copy of every submission so far.
    pub fn snapshot(&self) -> Vec<Submission> {
        self.0
            .lock()
            .expect("submission log mutex poisoned")
            .clone()
    }

    /// Drop every captured submission. Used by the test-only HTTP
    /// route so harness scripts can assert on a clean window without
    /// restarting the binary.
    pub fn clear(&self) {
        self.0
            .lock()
            .expect("submission log mutex poisoned")
            .clear();
    }
}

/// Greeting emitted on connect.
pub const GREETING: &str = "220 saehrimnir ESMTP ready\r\n";

/// Run the accept loop until `shutdown` flips. One task per accepted
/// connection. Cancels accept on shutdown. When `tls_acceptor` is
/// `Some`, EHLO advertises `STARTTLS` and the upgrade path is wired
/// through the per-connection state machine.
#[allow(clippy::too_many_arguments)]
pub async fn serve(
    listener: TcpListener,
    log: SubmissionLog,
    dispatcher: Option<Arc<crate::lua::Dispatcher>>,
    fixture: crate::shared::FixtureHandle,
    token_store: crate::oauth::TokenStore,
    tls_acceptor: Option<Arc<TlsAcceptor>>,
    request_log: crate::request_log::RequestLog,
    latency: crate::latency::LatencyKnob,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return Ok(());
                }
            }
            accept = listener.accept() => {
                match accept {
                    Ok((stream, peer)) => {
                        let log = log.clone();
                        let disp = dispatcher.clone();
                        let fix = Arc::clone(&fixture);
                        let store = token_store.clone();
                        let tls = tls_acceptor.clone();
                        let req_log = request_log.clone();
                        let lat = latency.clone();
                        let mut sd = shutdown.clone();
                        tokio::spawn(async move {
                            if let Err(e) = serve_connection(stream, log, disp, fix, store, tls, req_log, lat, &mut sd).await {
                                eprintln!("saehrimnir: smtp connection {peer}: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        eprintln!("saehrimnir: smtp accept error: {e}");
                    }
                }
            }
        }
    }
}

/// Drive a single SMTP connection. `dispatcher` is `None` for
/// callback-free scenarios; when present, AUTH / MAIL / RCPT / DATA
/// consult it before applying default behaviour. An
/// `Override::Tagged { status, message }` becomes a wire response
/// `<status> <message>`, where `status` is interpreted as the SMTP
/// numeric code (e.g. `"452"`). When `tls_acceptor` is `Some`, EHLO
/// advertises `STARTTLS` and the connection accepts a TLS upgrade.
#[allow(clippy::too_many_arguments)]
pub async fn serve_connection<S>(
    stream: S,
    log: SubmissionLog,
    dispatcher: Option<Arc<crate::lua::Dispatcher>>,
    fixture: crate::shared::FixtureHandle,
    token_store: crate::oauth::TokenStore,
    tls_acceptor: Option<Arc<TlsAcceptor>>,
    request_log: crate::request_log::RequestLog,
    latency: crate::latency::LatencyKnob,
    shutdown: &mut watch::Receiver<bool>,
) -> std::io::Result<()>
where
    S: AsyncStream + 'static,
{
    let boxed: Box<dyn AsyncStream> = Box::new(stream);
    let primary_account_id = fixture
        .read()
        .expect("fixture lock poisoned")
        .primary_account()
        .id
        .clone();
    let mut conn = Conn {
        stream: Some(BufStream::new(boxed)),
        state: SmtpState::default(),
        log,
        dispatcher,
        fixture,
        token_store,
        tls_acceptor,
        tls_active: false,
        request_log,
        latency,
        account_id: primary_account_id,
        connection_id: crate::connection_id::next(),
    };
    conn.write_str(GREETING).await?;
    loop {
        let line = match conn.read_line(shutdown).await? {
            ReadOutcome::Line(l) => l,
            ReadOutcome::PeerClosed => return Ok(()),
            ReadOutcome::Shutdown => {
                let _ = conn.write_str("421 saehrimnir shutting down\r\n").await;
                return Ok(());
            }
        };
        conn.latency.sleep_for("smtp").await;
        if conn.dispatch(&line).await? {
            // Handler signalled QUIT.
            return Ok(());
        }
    }
}

/// Per-connection state. Only one submission per connection, so the
/// envelope-collection fields are owned directly here rather than
/// on a heap object.
#[derive(Debug, Default)]
struct SmtpState {
    ehlo: Option<String>,
    auth: Option<String>,
    from: Option<String>,
    from_params: BTreeMap<String, String>,
    rcpts: Vec<String>,
    rcpt_params: Vec<BTreeMap<String, String>>,
}

struct Conn {
    /// Always `Some` outside the brief STARTTLS upgrade window. Held
    /// in an `Option` so `cmd_starttls` can `take()` the inner
    /// `BufStream`, surrender the underlying boxed stream to the
    /// acceptor, and re-install a fresh `BufStream<TlsStream<...>>`
    /// before any other handler runs.
    stream: Option<BufStream<Box<dyn AsyncStream>>>,
    state: SmtpState,
    log: SubmissionLog,
    dispatcher: Option<Arc<crate::lua::Dispatcher>>,
    fixture: crate::shared::FixtureHandle,
    token_store: crate::oauth::TokenStore,
    tls_acceptor: Option<Arc<TlsAcceptor>>,
    tls_active: bool,
    request_log: crate::request_log::RequestLog,
    latency: crate::latency::LatencyKnob,
    /// Account this connection's pending submission belongs to.
    /// Initialised to the fixture's primary at connect; `AUTH`
    /// rebinds when a SASL response names (PLAIN / LOGIN) or
    /// authorizes (XOAUTH2 / OAUTHBEARER) a declared account.
    account_id: String,
    /// Per-accepted-TCP-connection id, allocated once per
    /// `serve_connection`. Stamped onto every `request_log` entry
    /// so harness scripts can group `EHLO` / `MAIL FROM` /
    /// `RCPT TO` / `DATA` by session.
    connection_id: u64,
}

enum ReadOutcome {
    Line(String),
    PeerClosed,
    Shutdown,
}

impl Conn {
    fn stream_mut(&mut self) -> &mut BufStream<Box<dyn AsyncStream>> {
        self.stream
            .as_mut()
            .expect("smtp stream consumed outside upgrade window")
    }

    /// Consult the Lua dispatcher for `("smtp", command)`. Returns
    /// `Some((code, message))` if the script overrode the response;
    /// the SMTP-side caller should write `<code> <message>\r\n`
    /// and skip the default behaviour. `code` is parsed from the
    /// override's `status` field (which is the SMTP numeric code as
    /// a string, e.g. `"452"`); a non-numeric status falls back to
    /// `550` (permanent server error) so the client at least gets
    /// a sensible-shaped failure.
    fn maybe_override(&self, command: &str, payload: &str) -> Option<(u16, String)> {
        let d = self.dispatcher.as_ref()?;
        let payload_owned = payload.to_string();
        let result = d.dispatch("smtp", command, move |s| {
            crate::lua::req_set_str(s, "payload", &payload_owned)
        });
        match result {
            crate::lua::Override::Tagged { status, message } => {
                let code = status.parse::<u16>().unwrap_or(550);
                Some((code, message))
            }
            crate::lua::Override::None => None,
        }
    }

    async fn write_str(&mut self, s: &str) -> std::io::Result<()> {
        self.stream_mut().write_all(s.as_bytes()).await?;
        self.stream_mut().flush().await
    }

    async fn write_line(&mut self, code: u16, text: &str) -> std::io::Result<()> {
        self.stream_mut()
            .write_all(format!("{code} {text}\r\n").as_bytes())
            .await?;
        self.stream_mut().flush().await
    }

    async fn read_line(
        &mut self,
        shutdown: &mut watch::Receiver<bool>,
    ) -> std::io::Result<ReadOutcome> {
        let mut buf = String::new();
        loop {
            buf.clear();
            tokio::select! {
                biased;
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        return Ok(ReadOutcome::Shutdown);
                    }
                    continue;
                }
                r = self.stream_mut().read_line(&mut buf) => {
                    let n = r?;
                    return Ok(if n == 0 {
                        ReadOutcome::PeerClosed
                    } else {
                        ReadOutcome::Line(strip_crlf(&buf).to_string())
                    });
                }
            }
        }
    }

    /// Returns `true` if the connection should close (QUIT).
    async fn dispatch(&mut self, line: &str) -> std::io::Result<bool> {
        let (verb, rest) = split_verb(line);
        let upper = verb.to_ascii_uppercase();
        // Empty input still produces a "" verb that maps to a
        // 500 below; record it as well so the log captures
        // ratatoskr's literal command stream including stray
        // empty lines.
        //
        // For `AUTH <mech> [initial-response]` we record only the
        // mechanism token, never the credential payload -
        // /test/requests is exposed unauthenticated and the SASL
        // initial-response carries decoded user/pass for PLAIN /
        // LOGIN / XOAUTH2 / OAUTHBEARER. The 334 continuation
        // line is read directly inside `cmd_auth` and never
        // reaches `dispatch`, so we don't need to redact it here.
        let logged_args = if upper == "AUTH" {
            rest.split_whitespace().next().unwrap_or("")
        } else {
            rest
        };
        self.request_log.record_with_conn(
            "smtp",
            upper.clone(),
            serde_json::json!({ "args": logged_args }),
            Some(self.connection_id),
        );
        match upper.as_str() {
            "EHLO" | "HELO" => self.cmd_ehlo(rest).await.map(|_| false),
            "STARTTLS" => self.cmd_starttls().await.map(|_| false),
            "AUTH" => self.cmd_auth(rest).await.map(|_| false),
            "MAIL" => self.cmd_mail(rest).await.map(|_| false),
            "RCPT" => self.cmd_rcpt(rest).await.map(|_| false),
            "DATA" => self.cmd_data().await.map(|_| false),
            "RSET" => self.cmd_rset().await.map(|_| false),
            "NOOP" => self.write_line(250, "OK").await.map(|_| false),
            "VRFY" | "EXPN" | "HELP" => self
                .write_line(502, "command not implemented")
                .await
                .map(|_| false),
            "QUIT" => self.cmd_quit().await.map(|_| true),
            "" => self.write_line(500, "empty command").await.map(|_| false),
            other => self
                .write_line(500, &format!("unknown command {other}"))
                .await
                .map(|_| false),
        }
    }

    // ── Commands ────────────────────────────────────────────────────

    async fn cmd_ehlo(&mut self, rest: &str) -> std::io::Result<()> {
        let host = rest.trim();
        self.state.ehlo = Some(host.to_string());
        // Reset envelope state on every (EH)LO.
        self.state.from = None;
        self.state.from_params.clear();
        self.state.rcpts.clear();
        self.state.rcpt_params.clear();
        let advertise_starttls = self.tls_acceptor.is_some() && !self.tls_active;
        // Multi-line 250 reply per RFC 5321: each line except the
        // last uses `250-`; the last uses `250 ` (space).
        self.stream_mut()
            .write_all(b"250-saehrimnir greets you\r\n")
            .await?;
        // Advertise a SIZE limit well above any message a harness gate
        // sends. bifrost-smtp parses this advertised limit and refuses
        // (client-side, before DATA) any message whose octet length
        // exceeds it - so the old 50MB (52428800) limit fast-failed a
        // ~67MB send with a transport.network error in ~500ms. 256 MiB
        // clears the large-attachment regression while still modelling a
        // real server that bounds message size. The mock buffers and
        // accepts the full DATA regardless; it never enforces this.
        self.stream_mut()
            .write_all(b"250-SIZE 268435456\r\n")
            .await?;
        self.stream_mut().write_all(b"250-8BITMIME\r\n").await?;
        if advertise_starttls {
            self.stream_mut().write_all(b"250-STARTTLS\r\n").await?;
        }
        self.stream_mut()
            .write_all(b"250 AUTH PLAIN LOGIN XOAUTH2\r\n")
            .await?;
        self.stream_mut().flush().await
    }

    async fn cmd_starttls(&mut self) -> std::io::Result<()> {
        if self.tls_active {
            return self.write_line(503, "TLS already active").await;
        }
        let Some(acceptor) = self.tls_acceptor.clone() else {
            return self.write_line(502, "STARTTLS not supported").await;
        };
        self.write_line(220, "Ready to start TLS").await?;
        // Surrender the BufStream and pull out the inner boxed
        // stream. RFC 3207 says the server MUST discard any data
        // read after the 220 - the BufStream's internal buffer is
        // dropped along with it, so we don't need to do anything
        // explicit.
        let bufstream = self
            .stream
            .take()
            .expect("smtp stream already taken in cmd_starttls");
        let inner: Box<dyn AsyncStream> = bufstream.into_inner();
        let tls = match acceptor.accept(inner).await {
            Ok(t) => t,
            Err(e) => return Err(e),
        };
        let new_boxed: Box<dyn AsyncStream> = Box::new(tls);
        self.stream = Some(BufStream::new(new_boxed));
        self.tls_active = true;
        // Per RFC 3207 §4.2 the server MUST discard any prior knowledge
        // it had about the session. We zero the state so the client's
        // forthcoming EHLO repopulates it.
        self.state = SmtpState::default();
        Ok(())
    }

    async fn cmd_auth(&mut self, rest: &str) -> std::io::Result<()> {
        let mut parts = rest.splitn(2, ' ');
        let mech = parts.next().unwrap_or("").to_ascii_uppercase();
        let initial_response = parts.next().map(|s| s.trim().to_string());
        match mech.as_str() {
            "PLAIN" | "LOGIN" | "XOAUTH2" | "OAUTHBEARER" => {
                let response_b64 = match initial_response {
                    Some(s) => Some(s),
                    None => {
                        // Prompt with a 334 challenge and consume one
                        // continuation line.
                        self.stream_mut().write_all(b"334 \r\n").await?;
                        self.stream_mut().flush().await?;
                        let mut buf = String::new();
                        let n = self.stream_mut().read_line(&mut buf).await?;
                        if n == 0 {
                            return Ok(());
                        }
                        let stripped = strip_crlf(&buf).to_string();
                        if stripped == "*" {
                            return self.write_line(501, "AUTH cancelled").await;
                        }
                        // For LOGIN the server normally sends a second
                        // 334 prompt for the password. v0 simplifies by
                        // accepting whatever the client sent and
                        // skipping the second round-trip. lettre
                        // tolerates this because we return 235 directly.
                        Some(stripped)
                    }
                };
                // Consult the Lua dispatcher BEFORE binding the
                // connection's account. The override payload is the
                // mechanism name (e.g. "PLAIN"); the SASL response
                // itself is deliberately omitted (it carries
                // decoded credentials and the request log already
                // redacts it - see `dispatch`'s logged_args
                // handling). A fixture that wants to inject
                // AUTH-time failures returns `{ status, message }`
                // where `status` is the SMTP numeric code (e.g.
                // `"535"` for invalid credentials, `"454"` for a
                // temporary problem); the override skips both the
                // account binding and the 235 success line.
                if let Some((code, msg)) = self.maybe_override("AUTH", &mech) {
                    return self.write_str(&format!("{code} {msg}\r\n")).await;
                }
                if let Some(b64) = response_b64
                    && let Some(decoded) = crate::imap::sasl_decode_b64(&b64)
                {
                    if let Some(token) = crate::imap::sasl_extract_bearer(&decoded)
                        && let Some(id) = self.token_store.account_for_token(&token)
                    {
                        self.account_id = id;
                    } else if let Some(user) =
                        crate::imap::sasl_extract_username(mech.as_str(), &decoded)
                        && let Some(id) = self.account_id_for_username(&user)
                    {
                        self.account_id = id;
                    }
                }
                self.state.auth = Some(mech.clone());
                self.write_line(235, "authentication accepted").await
            }
            "" => self.write_line(501, "AUTH missing mechanism").await,
            _ => self.write_line(504, "unsupported SASL mechanism").await,
        }
    }

    fn account_id_for_username(&self, identity: &str) -> Option<String> {
        let fixture = self.fixture.read().expect("fixture lock poisoned");
        let trimmed = identity.trim();
        fixture
            .accounts
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(trimmed))
            .map(|a| a.id.clone())
    }

    async fn cmd_mail(&mut self, rest: &str) -> std::io::Result<()> {
        if self.state.ehlo.is_none() {
            return self.write_line(503, "send EHLO first").await;
        }
        let payload = match strip_prefix_ci(rest.trim(), "FROM:") {
            Some(p) => p.trim().to_string(),
            None => return self.write_line(501, "expected MAIL FROM:<addr>").await,
        };
        if let Some((code, msg)) = self.maybe_override("MAIL", &payload) {
            return self.write_str(&format!("{code} {msg}\r\n")).await;
        }
        let (addr, params) = match split_envelope_payload(&payload) {
            Ok(t) => t,
            Err(_) => {
                return self
                    .write_line(501, "malformed MAIL FROM extension parameter")
                    .await;
            }
        };
        self.state.from = Some(addr);
        self.state.from_params = params;
        self.state.rcpts.clear();
        self.state.rcpt_params.clear();
        self.write_line(250, "OK").await
    }

    async fn cmd_rcpt(&mut self, rest: &str) -> std::io::Result<()> {
        if self.state.from.is_none() {
            return self.write_line(503, "send MAIL FROM first").await;
        }
        let payload = match strip_prefix_ci(rest.trim(), "TO:") {
            Some(p) => p.trim().to_string(),
            None => return self.write_line(501, "expected RCPT TO:<addr>").await,
        };
        if let Some((code, msg)) = self.maybe_override("RCPT", &payload) {
            return self.write_str(&format!("{code} {msg}\r\n")).await;
        }
        let (addr, params) = match split_envelope_payload(&payload) {
            Ok(t) => t,
            Err(_) => {
                return self
                    .write_line(501, "malformed RCPT TO extension parameter")
                    .await;
            }
        };
        self.state.rcpts.push(addr);
        self.state.rcpt_params.push(params);
        self.write_line(250, "OK").await
    }

    async fn cmd_data(&mut self) -> std::io::Result<()> {
        if self.state.from.is_none() || self.state.rcpts.is_empty() {
            return self
                .write_line(503, "DATA requires MAIL FROM + at least one RCPT TO")
                .await;
        }
        if let Some((code, msg)) = self.maybe_override("DATA", "") {
            return self.write_str(&format!("{code} {msg}\r\n")).await;
        }
        self.write_line(354, "send data, terminate with <CRLF>.<CRLF>")
            .await?;

        let data = self.read_data_body().await?;
        let submission = Submission {
            from: self.state.from.clone().unwrap_or_default(),
            recipients: std::mem::take(&mut self.state.rcpts),
            from_params: std::mem::take(&mut self.state.from_params),
            rcpt_params: std::mem::take(&mut self.state.rcpt_params),
            auth_mechanism: self.state.auth.clone(),
            account_id: self.account_id.clone(),
            data,
            received_at: Timestamp::now(),
        };
        // Reset envelope state - even though v0 only handles one
        // submission per connection, leaving the slate clean keeps
        // future RSET / second-message paths from inheriting stale
        // values.
        self.state.from = None;
        self.state.rcpts.clear();
        self.state.rcpt_params.clear();

        self.log.push(submission);
        self.write_line(250, "OK queued").await
    }

    async fn read_data_body(&mut self) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut line = String::new();
        loop {
            line.clear();
            let n = self.stream_mut().read_line(&mut line).await?;
            if n == 0 {
                // Peer closed mid-DATA; treat as truncated body and
                // return what we have. The submission log will reflect
                // it; the surrounding loop will return on the next
                // read.
                return Ok(out);
            }
            let stripped = strip_crlf(&line);
            if stripped == "." {
                return Ok(out);
            }
            // Reverse dot-stuffing: a leading `..` becomes `.`.
            let body_line = stripped.strip_prefix('.').unwrap_or(stripped);
            out.extend_from_slice(body_line.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
    }

    async fn cmd_rset(&mut self) -> std::io::Result<()> {
        self.state.from = None;
        self.state.from_params.clear();
        self.state.rcpts.clear();
        self.state.rcpt_params.clear();
        self.write_line(250, "OK").await
    }

    async fn cmd_quit(&mut self) -> std::io::Result<()> {
        self.write_line(221, "saehrimnir bye").await
    }
}

fn strip_crlf(s: &str) -> &str {
    s.strip_suffix("\r\n")
        .or_else(|| s.strip_suffix('\n'))
        .unwrap_or(s)
}

fn split_verb(line: &str) -> (&str, &str) {
    match line.split_once(' ') {
        Some((v, r)) => (v, r),
        None => (line, ""),
    }
}

/// Split a `MAIL FROM:` / `RCPT TO:` payload into its address bytes
/// and any RFC 5321 / RFC 3461 extension parameters. The payload looks
/// like `<addr@example.com> SIZE=1234 BODY=8BITMIME NOTIFY=SUCCESS,FAILURE`
/// where the address is everything up to the first whitespace and the
/// remaining tokens are `KEY[=VALUE]` pairs (case-insensitive on the
/// key). A bare token without `=` is stored with an empty value;
/// truly malformed input (a token that starts with `=` or has no key
/// before the `=`) is rejected so the client gets a `501`. Unknown
/// parameter keys are accepted silently - the mock just records them.
fn split_envelope_payload(payload: &str) -> Result<(String, BTreeMap<String, String>), String> {
    let trimmed = payload.trim();
    let (addr, rest) = match trimmed.find(|c: char| c.is_ascii_whitespace()) {
        Some(i) => (&trimmed[..i], trimmed[i..].trim_start()),
        None => (trimmed, ""),
    };
    let mut params = BTreeMap::new();
    if !rest.is_empty() {
        for tok in rest.split_ascii_whitespace() {
            match tok.split_once('=') {
                Some((k, v)) => {
                    let k = k.trim();
                    if k.is_empty() {
                        return Err(format!("empty parameter key in {tok:?}"));
                    }
                    params.insert(k.to_ascii_uppercase(), v.to_string());
                }
                None => {
                    if tok.starts_with('=') {
                        return Err(format!("malformed parameter token {tok:?}"));
                    }
                    params.insert(tok.to_ascii_uppercase(), String::new());
                }
            }
        }
    }
    Ok((addr.to_string(), params))
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Load the canonical small fixture for tests that don't care
    /// about specific resources. Primary account is `account-1`.
    fn test_fixture() -> crate::shared::FixtureHandle {
        let fix = crate::fixture::load(std::path::Path::new("fixtures/jmap-small.toml"))
            .expect("default fixture loads");
        crate::shared::handle(fix)
    }

    async fn run_script(script: &[u8]) -> (String, SubmissionLog) {
        let log = SubmissionLog::new();
        let (server, mut client) = tokio::io::duplex(32 * 1024);
        let (_tx, rx) = watch::channel(false);
        let log_clone = log.clone();
        let task = tokio::spawn(async move {
            let mut rx = rx;
            serve_connection(
                server,
                log_clone,
                None,
                test_fixture(),
                crate::oauth::TokenStore::default(),
                None,
                crate::request_log::RequestLog::default(),
                crate::latency::LatencyKnob::default(),
                &mut rx,
            )
            .await
        });
        client.write_all(script).await.unwrap();
        client.shutdown().await.unwrap();
        let mut buf = Vec::new();
        client.read_to_end(&mut buf).await.unwrap();
        task.await.unwrap().unwrap();
        (String::from_utf8(buf).unwrap(), log)
    }

    #[tokio::test]
    async fn greeting_emitted_immediately() {
        let (out, _) = run_script(b"").await;
        assert_eq!(out, GREETING);
    }

    #[tokio::test]
    async fn ehlo_emits_capability_list() {
        let (out, _) = run_script(b"EHLO me.local\r\nQUIT\r\n").await;
        assert!(out.contains("250-saehrimnir greets you\r\n"));
        assert!(out.contains("250-SIZE 268435456\r\n"));
        assert!(out.contains("250-8BITMIME\r\n"));
        // Last line uses space, not dash, so lettre's parser stops.
        assert!(out.contains("250 AUTH PLAIN LOGIN XOAUTH2\r\n"));
        // Banned advertisements.
        for k in ["STARTTLS", "CHUNKING", "PIPELINING", "DSN"] {
            assert!(!out.contains(k), "advertised banned cap {k}: {out:?}");
        }
        assert!(out.contains("221 saehrimnir bye\r\n"));
    }

    #[tokio::test]
    async fn auth_with_initial_response_is_accepted() {
        let (out, _) = run_script(b"EHLO me\r\nAUTH PLAIN AGFsaWNlAGh1bnRlcg==\r\nQUIT\r\n").await;
        assert!(out.contains("235 authentication accepted\r\n"));
    }

    #[tokio::test]
    async fn auth_with_continuation_is_accepted() {
        let (out, _) =
            run_script(b"EHLO me\r\nAUTH PLAIN\r\nAGFsaWNlAGh1bnRlcg==\r\nQUIT\r\n").await;
        assert!(out.contains("334 \r\n"), "expected challenge: {out:?}");
        assert!(out.contains("235 authentication accepted\r\n"));
    }

    #[tokio::test]
    async fn auth_unsupported_mechanism_is_504() {
        let (out, _) = run_script(b"EHLO me\r\nAUTH GSSAPI\r\nQUIT\r\n").await;
        assert!(out.contains("504 unsupported SASL mechanism"));
    }

    #[tokio::test]
    async fn mail_before_ehlo_is_503() {
        let (out, _) = run_script(b"MAIL FROM:<a@b>\r\nQUIT\r\n").await;
        assert!(out.contains("503 send EHLO first"));
    }

    #[tokio::test]
    async fn rcpt_before_mail_is_503() {
        let (out, _) = run_script(b"EHLO me\r\nRCPT TO:<a@b>\r\nQUIT\r\n").await;
        assert!(out.contains("503 send MAIL FROM first"));
    }

    #[tokio::test]
    async fn data_without_recipients_is_503() {
        let (out, _) = run_script(b"EHLO me\r\nMAIL FROM:<a@b>\r\nDATA\r\nQUIT\r\n").await;
        assert!(out.contains("503 DATA requires MAIL FROM + at least one RCPT TO"));
    }

    #[tokio::test]
    async fn full_submission_is_captured_and_dot_unstuffed() {
        let script = b"\
            EHLO me.local\r\n\
            AUTH PLAIN AGFsaWNlAGh1bnRlcg==\r\n\
            MAIL FROM:<alice@example.com>\r\n\
            RCPT TO:<bob@example.com>\r\n\
            RCPT TO:<carol@example.com>\r\n\
            DATA\r\n\
            From: <alice@example.com>\r\n\
            To: <bob@example.com>\r\n\
            Subject: hi\r\n\
            \r\n\
            ..literally a leading dot\r\n\
            normal line\r\n\
            .\r\n\
            QUIT\r\n";
        let (out, log) = run_script(script).await;
        assert!(out.contains("354 send data"));
        assert!(out.contains("250 OK queued"));

        let snap = log.snapshot();
        assert_eq!(snap.len(), 1);
        let s = &snap[0];
        assert_eq!(s.from, "<alice@example.com>");
        assert_eq!(
            s.recipients,
            vec![
                "<bob@example.com>".to_string(),
                "<carol@example.com>".to_string()
            ]
        );
        assert_eq!(s.auth_mechanism.as_deref(), Some("PLAIN"));

        let body = String::from_utf8(s.data.clone()).unwrap();
        assert!(body.contains("From: <alice@example.com>\r\n"));
        assert!(body.contains("Subject: hi\r\n"));
        // The "..literally" line should have been un-stuffed to one
        // leading dot.
        assert!(body.contains(".literally a leading dot\r\n"));
        assert!(!body.contains("..literally"));
    }

    #[tokio::test]
    async fn rset_clears_envelope_state() {
        let script = b"\
            EHLO me\r\n\
            MAIL FROM:<a@b>\r\n\
            RSET\r\n\
            RCPT TO:<c@d>\r\n\
            QUIT\r\n";
        let (out, _) = run_script(script).await;
        // After RSET, RCPT without MAIL FROM is 503.
        assert!(out.contains("503 send MAIL FROM first"));
    }

    #[tokio::test]
    async fn unknown_command_is_500() {
        let (out, _) = run_script(b"EHLO me\r\nFROBNICATE\r\nQUIT\r\n").await;
        assert!(out.contains("500 unknown command FROBNICATE"));
    }

    #[tokio::test]
    async fn mail_and_rcpt_capture_extension_parameters() {
        let script = b"\
            EHLO me\r\n\
            AUTH PLAIN AGFsaWNlAGh1bnRlcg==\r\n\
            MAIL FROM:<a@b> SIZE=1234 BODY=8BITMIME ENVID=foo\r\n\
            RCPT TO:<c@d> NOTIFY=SUCCESS,FAILURE ORCPT=rfc822;c@d\r\n\
            RCPT TO:<e@f>\r\n\
            DATA\r\n\
            hi\r\n\
            .\r\n\
            QUIT\r\n";
        let (out, log) = run_script(script).await;
        assert!(out.contains("250 OK queued"));
        let snap = log.snapshot();
        let s = &snap[0];
        assert_eq!(s.from, "<a@b>");
        assert_eq!(s.from_params.get("SIZE").map(String::as_str), Some("1234"));
        assert_eq!(
            s.from_params.get("BODY").map(String::as_str),
            Some("8BITMIME")
        );
        assert_eq!(s.from_params.get("ENVID").map(String::as_str), Some("foo"));

        assert_eq!(s.recipients, vec!["<c@d>".to_string(), "<e@f>".to_string()]);
        assert_eq!(s.rcpt_params.len(), 2);
        assert_eq!(
            s.rcpt_params[0].get("NOTIFY").map(String::as_str),
            Some("SUCCESS,FAILURE")
        );
        assert_eq!(
            s.rcpt_params[0].get("ORCPT").map(String::as_str),
            Some("rfc822;c@d")
        );
        assert!(s.rcpt_params[1].is_empty());
    }

    #[tokio::test]
    async fn large_size_param_is_accepted_and_buffered() {
        // Regression for the bifrost-smtp large-message fast-fail: the
        // mock must accept a `MAIL FROM ... SIZE=NN` for NN well above
        // 67MB and buffer the full DATA. Here we assert the envelope-
        // level acceptance (the EHLO advertises 256 MiB, so bifrost
        // never refuses client-side) plus a multi-line DATA body that
        // round-trips into the submission log.
        let script = b"\
            EHLO me\r\n\
            AUTH PLAIN AGFsaWNlAGh1bnRlcg==\r\n\
            MAIL FROM:<a@b> SIZE=70254592\r\n\
            RCPT TO:<c@d>\r\n\
            DATA\r\n\
            Subject: big\r\n\
            \r\n\
            payload line one\r\n\
            payload line two\r\n\
            .\r\n\
            QUIT\r\n";
        let (out, log) = run_script(script).await;
        assert!(out.contains("250-SIZE 268435456\r\n"));
        assert!(out.contains("250 OK queued"));
        let snap = log.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(
            snap[0].from_params.get("SIZE").map(String::as_str),
            Some("70254592")
        );
        let body = String::from_utf8(snap[0].data.clone()).unwrap();
        assert!(body.contains("payload line one\r\n"));
        assert!(body.contains("payload line two\r\n"));
    }

    #[tokio::test]
    async fn malformed_envelope_param_is_501() {
        let script = b"\
            EHLO me\r\n\
            MAIL FROM:<a@b> =bogus\r\n\
            QUIT\r\n";
        let (out, _) = run_script(script).await;
        assert!(out.contains("501 malformed MAIL FROM"));
    }

    #[tokio::test]
    async fn shutdown_signal_drops_connection() {
        let log = SubmissionLog::new();
        let (server, mut client) = tokio::io::duplex(8192);
        let (tx, rx) = watch::channel(false);
        let log_clone = log.clone();
        let task = tokio::spawn(async move {
            let mut rx = rx;
            serve_connection(
                server,
                log_clone,
                None,
                test_fixture(),
                crate::oauth::TokenStore::default(),
                None,
                crate::request_log::RequestLog::default(),
                crate::latency::LatencyKnob::default(),
                &mut rx,
            )
            .await
        });
        let mut greet = vec![0u8; GREETING.len()];
        client.read_exact(&mut greet).await.unwrap();
        tx.send(true).unwrap();
        let mut tail = Vec::new();
        client.read_to_end(&mut tail).await.unwrap();
        let s = String::from_utf8(tail).unwrap();
        assert!(s.contains("421 saehrimnir shutting down"));
        task.await.unwrap().unwrap();
    }
}
