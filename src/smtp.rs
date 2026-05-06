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

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadHalf, WriteHalf,
};
use tokio::net::TcpListener;
use tokio::sync::watch;

/// One captured SMTP submission. Tests read these via
/// [`SubmissionLog::snapshot`].
#[derive(Debug, Clone)]
pub struct Submission {
    /// Raw payload from `MAIL FROM:` - typically `<addr@example.com>`,
    /// including the angle brackets.
    pub from: String,
    /// Raw payloads from each `RCPT TO:` in submission order.
    pub recipients: Vec<String>,
    /// SASL mechanism the client authenticated with, if any.
    pub auth_mechanism: Option<String>,
    /// Full RFC 822 message bytes with dot-stuffing reversed (a body
    /// line starting `..` is collapsed to a single leading `.`).
    pub data: Vec<u8>,
    /// Wall-clock time the mock saw the submission complete. Not
    /// byte-stable - the determinism contract covers only the
    /// submission contents (from / recipients / data), not the
    /// timestamp.
    pub received_at: DateTime<Utc>,
}

/// Shared in-memory submission log. Cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct SubmissionLog(Arc<Mutex<Vec<Submission>>>);

impl SubmissionLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, s: Submission) {
        self.0.lock().expect("submission log mutex poisoned").push(s);
    }

    /// Return a copy of every submission so far.
    pub fn snapshot(&self) -> Vec<Submission> {
        self.0
            .lock()
            .expect("submission log mutex poisoned")
            .clone()
    }
}

/// Greeting emitted on connect.
pub const GREETING: &str = "220 saehrimnir ESMTP ready\r\n";

/// Run the accept loop until `shutdown` flips. One task per accepted
/// connection. Cancels accept on shutdown.
pub async fn serve(
    listener: TcpListener,
    log: SubmissionLog,
    dispatcher: Option<std::sync::Arc<crate::lua::Dispatcher>>,
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
                        let mut sd = shutdown.clone();
                        tokio::spawn(async move {
                            if let Err(e) = serve_connection(stream, log, disp, &mut sd).await {
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
/// numeric code (e.g. `"452"`).
pub async fn serve_connection<S>(
    stream: S,
    log: SubmissionLog,
    dispatcher: Option<std::sync::Arc<crate::lua::Dispatcher>>,
    shutdown: &mut watch::Receiver<bool>,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (reader, writer) = tokio::io::split(stream);
    let mut conn = Conn {
        reader: BufReader::new(reader),
        writer,
        state: SmtpState::default(),
        log,
        dispatcher,
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
    rcpts: Vec<String>,
}

struct Conn<S: AsyncRead + AsyncWrite + Unpin> {
    reader: BufReader<ReadHalf<S>>,
    writer: WriteHalf<S>,
    state: SmtpState,
    log: SubmissionLog,
    dispatcher: Option<std::sync::Arc<crate::lua::Dispatcher>>,
}

enum ReadOutcome {
    Line(String),
    PeerClosed,
    Shutdown,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Conn<S> {
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
        self.writer.write_all(s.as_bytes()).await?;
        self.writer.flush().await
    }

    async fn write_line(&mut self, code: u16, text: &str) -> std::io::Result<()> {
        self.writer
            .write_all(format!("{code} {text}\r\n").as_bytes())
            .await?;
        self.writer.flush().await
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
                r = self.reader.read_line(&mut buf) => {
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
        match verb.to_ascii_uppercase().as_str() {
            "EHLO" | "HELO" => self.cmd_ehlo(rest).await.map(|_| false),
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
        self.state.rcpts.clear();
        // Multi-line 250 reply per RFC 5321: each line except the
        // last uses `250-`; the last uses `250 ` (space).
        self.writer
            .write_all(b"250-saehrimnir greets you\r\n")
            .await?;
        self.writer.write_all(b"250-SIZE 52428800\r\n").await?;
        self.writer.write_all(b"250-8BITMIME\r\n").await?;
        self.writer.write_all(b"250 AUTH PLAIN LOGIN XOAUTH2\r\n").await?;
        self.writer.flush().await
    }

    async fn cmd_auth(&mut self, rest: &str) -> std::io::Result<()> {
        let mut parts = rest.splitn(2, ' ');
        let mech = parts.next().unwrap_or("").to_ascii_uppercase();
        let initial_response = parts.next().map(str::trim);
        match mech.as_str() {
            "PLAIN" | "LOGIN" | "XOAUTH2" | "OAUTHBEARER" => {
                if initial_response.is_none() {
                    // Prompt with a 334 challenge and consume one
                    // continuation line.
                    self.writer.write_all(b"334 \r\n").await?;
                    self.writer.flush().await?;
                    let mut buf = String::new();
                    let n = self.reader.read_line(&mut buf).await?;
                    if n == 0 {
                        return Ok(());
                    }
                    if strip_crlf(&buf) == "*" {
                        return self.write_line(501, "AUTH cancelled").await;
                    }
                    // For LOGIN, the server normally sends a second
                    // 334 prompt for the password. v0 simplifies by
                    // accepting whatever the client sent and skipping
                    // the second round-trip. lettre tolerates this
                    // because we return 235 directly.
                }
                self.state.auth = Some(mech.clone());
                self.write_line(235, "authentication accepted").await
            }
            "" => self.write_line(501, "AUTH missing mechanism").await,
            _ => self.write_line(504, "unsupported SASL mechanism").await,
        }
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
        self.state.from = Some(payload);
        self.state.rcpts.clear();
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
        self.state.rcpts.push(payload);
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
            auth_mechanism: self.state.auth.clone(),
            data,
            received_at: Utc::now(),
        };
        // Reset envelope state - even though v0 only handles one
        // submission per connection, leaving the slate clean keeps
        // future RSET / second-message paths from inheriting stale
        // values.
        self.state.from = None;
        self.state.rcpts.clear();

        self.log.push(submission);
        self.write_line(250, "OK queued").await
    }

    async fn read_data_body(&mut self) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut line = String::new();
        loop {
            line.clear();
            let n = self.reader.read_line(&mut line).await?;
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
        self.state.rcpts.clear();
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

    async fn run_script(script: &[u8]) -> (String, SubmissionLog) {
        let log = SubmissionLog::new();
        let (server, mut client) = tokio::io::duplex(32 * 1024);
        let (_tx, rx) = watch::channel(false);
        let log_clone = log.clone();
        let task = tokio::spawn(async move {
            let mut rx = rx;
            serve_connection(server, log_clone, None, &mut rx).await
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
        assert!(out.contains("250-SIZE 52428800\r\n"));
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
        let (out, _) = run_script(
            b"EHLO me\r\nAUTH PLAIN AGFsaWNlAGh1bnRlcg==\r\nQUIT\r\n",
        )
        .await;
        assert!(out.contains("235 authentication accepted\r\n"));
    }

    #[tokio::test]
    async fn auth_with_continuation_is_accepted() {
        let (out, _) = run_script(
            b"EHLO me\r\nAUTH PLAIN\r\nAGFsaWNlAGh1bnRlcg==\r\nQUIT\r\n",
        )
        .await;
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
        let (out, _) = run_script(
            b"EHLO me\r\nMAIL FROM:<a@b>\r\nDATA\r\nQUIT\r\n",
        )
        .await;
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
            vec!["<bob@example.com>".to_string(), "<carol@example.com>".to_string()]
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
    async fn shutdown_signal_drops_connection() {
        let log = SubmissionLog::new();
        let (server, mut client) = tokio::io::duplex(8192);
        let (tx, rx) = watch::channel(false);
        let log_clone = log.clone();
        let task = tokio::spawn(async move {
            let mut rx = rx;
            serve_connection(server, log_clone, None, &mut rx).await
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
