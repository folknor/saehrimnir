#![allow(clippy::unwrap_used)]

//! End-to-end SMTP test driving `serve_connection` over a duplex
//! stream and asserting on the captured submission log.

use saehrimnir::lua;
use saehrimnir::smtp::{self, SubmissionLog};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

/// Build a minimal fixture handle the SMTP tests can pass to
/// `serve_connection`. Uses the canonical `jmap-small.toml` so the
/// primary account name (`test@example.com`) is the default
/// binding for unauthenticated submissions.
fn default_fixture() -> saehrimnir::shared::FixtureHandle {
    let fix = saehrimnir::fixture::load(std::path::Path::new("fixtures/jmap-small.toml"))
        .expect("default fixture loads");
    saehrimnir::shared::handle(fix)
}

fn multi_account_fixture() -> saehrimnir::shared::FixtureHandle {
    let fix = saehrimnir::fixture::load(std::path::Path::new("fixtures/multi-account-small.toml"))
        .expect("multi-account fixture loads");
    saehrimnir::shared::handle(fix)
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHA: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() >= 2 {
            out.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() == 3 {
            out.push(ALPHA[(n & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

async fn run_with_log(script: &[u8]) -> (String, SubmissionLog) {
    let log = SubmissionLog::new();
    let log_clone = log.clone();
    let (server, mut client) = tokio::io::duplex(64 * 1024);
    let (_tx, rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut rx = rx;
        smtp::serve_connection(server, log_clone, None, default_fixture(), saehrimnir::oauth::TokenStore::default(), None, saehrimnir::request_log::RequestLog::default(), saehrimnir::latency::LatencyKnob::default(), &mut rx).await
    });
    client.write_all(script).await.unwrap();
    client.shutdown().await.unwrap();
    let mut buf = Vec::new();
    client.read_to_end(&mut buf).await.unwrap();
    task.await.unwrap().unwrap();
    (String::from_utf8(buf).unwrap(), log)
}

#[tokio::test]
async fn full_submission_round_trip() {
    let script = b"\
        EHLO ratatoskr.test\r\n\
        AUTH PLAIN AGFsaWNlAGh1bnRlcg==\r\n\
        MAIL FROM:<alice@example.com>\r\n\
        RCPT TO:<bob@example.com>\r\n\
        RCPT TO:<carol@example.com>\r\n\
        DATA\r\n\
        From: Alice <alice@example.com>\r\n\
        To: Bob <bob@example.com>\r\n\
        Cc: Carol <carol@example.com>\r\n\
        Subject: hi\r\n\
        Message-ID: <smoke-1@saehrimnir>\r\n\
        \r\n\
        Hello, world.\r\n\
        Second line.\r\n\
        .\r\n\
        QUIT\r\n";
    let (out, log) = run_with_log(script).await;

    assert!(out.starts_with("220 saehrimnir ESMTP ready\r\n"));
    assert!(out.contains("250 AUTH PLAIN LOGIN XOAUTH2\r\n"));
    assert!(out.contains("235 authentication accepted\r\n"));
    assert!(out.contains("354 send data"));
    assert!(out.contains("250 OK queued\r\n"));
    assert!(out.contains("221 saehrimnir bye\r\n"));

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

    let body = std::str::from_utf8(&s.data).unwrap();
    assert!(body.contains("From: Alice <alice@example.com>\r\n"));
    assert!(body.contains("Subject: hi\r\n"));
    assert!(body.contains("Hello, world.\r\n"));
    assert!(body.contains("Second line.\r\n"));
    // The "<CRLF>.<CRLF>" terminator line is not part of the captured
    // payload.
    assert!(!body.ends_with("\r\n.\r\n"));
}

#[tokio::test]
async fn dot_stuffing_is_reversed() {
    // RFC 5321 sec 4.5.2: a line beginning with `.` in the body is
    // sent as `..`; the receiver strips one dot. The mock has to do
    // that to stay byte-identical to a real receiver.
    let script = b"\
        EHLO me\r\n\
        MAIL FROM:<a@b>\r\n\
        RCPT TO:<c@d>\r\n\
        DATA\r\n\
        ..hello\r\n\
        ...still dots\r\n\
        normal\r\n\
        .\r\n\
        QUIT\r\n";
    let (_out, log) = run_with_log(script).await;
    let body = String::from_utf8(log.snapshot()[0].data.clone()).unwrap();
    assert_eq!(body, ".hello\r\n..still dots\r\nnormal\r\n");
}

#[tokio::test]
async fn missing_envelope_pieces_yield_503() {
    // DATA without RCPT TO -> 503.
    let (out, log) = run_with_log(
        b"EHLO me\r\nMAIL FROM:<a@b>\r\nDATA\r\nQUIT\r\n",
    )
    .await;
    assert!(out.contains("503 DATA requires"));
    assert!(log.snapshot().is_empty());
}

#[tokio::test]
async fn auth_attribute_round_trips() {
    let (_out, log) = run_with_log(
        b"EHLO me\r\nAUTH XOAUTH2 dXNlcj1hbGljZQ==\r\nMAIL FROM:<a@b>\r\nRCPT TO:<c@d>\r\nDATA\r\nx\r\n.\r\nQUIT\r\n",
    )
    .await;
    let snap = log.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].auth_mechanism.as_deref(), Some("XOAUTH2"));
}

// ── Reactive-callback tests ────────────────────────────────────────

async fn run_with_dispatcher(
    scenario: &str,
    script: &[u8],
) -> (String, SubmissionLog) {
    let log = SubmissionLog::new();
    let log_clone = log.clone();
    let (_fixture, dispatcher) =
        lua::load_source_with_dispatcher(scenario, "@cb").unwrap();
    let dispatcher = Some(Arc::new(dispatcher));

    let (server, mut client) = tokio::io::duplex(64 * 1024);
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        let mut rx = rx;
        smtp::serve_connection(server, log_clone, dispatcher, default_fixture(), saehrimnir::oauth::TokenStore::default(), None, saehrimnir::request_log::RequestLog::default(), saehrimnir::latency::LatencyKnob::default(), &mut rx).await
    });
    client.write_all(script).await.unwrap();
    client.shutdown().await.unwrap();
    let mut buf = Vec::new();
    client.read_to_end(&mut buf).await.unwrap();
    task.await.unwrap().unwrap();
    (String::from_utf8(buf).unwrap(), log)
}

#[tokio::test]
async fn rcpt_callback_can_inject_452() {
    // Script rejects the second RCPT with a 452 (mailbox full /
    // rate-limit class). Other commands pass through.
    let scenario = r#"
        fixture({ name = "cb" })
        account({ id = "a", name = "a@b" })
        on("smtp", "RCPT", function(req)
            if req.call_index == 2 then
                return { status = "452", message = "rate limit" }
            end
        end)
    "#;
    let (out, _log) = run_with_dispatcher(
        scenario,
        b"EHLO me\r\nMAIL FROM:<a@b>\r\nRCPT TO:<c@d>\r\nRCPT TO:<e@f>\r\nQUIT\r\n",
    )
    .await;
    // First RCPT default OK, second rejected.
    assert!(out.contains("250 OK"));
    assert!(out.contains("452 rate limit"), "got: {out:?}");
}

#[tokio::test]
async fn data_callback_can_reject_submission() {
    let scenario = r#"
        fixture({ name = "cb" })
        account({ id = "a", name = "a@b" })
        on("smtp", "DATA", function(req)
            return { status = "552", message = "message too large" }
        end)
    "#;
    let (out, log) = run_with_dispatcher(
        scenario,
        b"EHLO me\r\nMAIL FROM:<a@b>\r\nRCPT TO:<c@d>\r\nDATA\r\nx\r\n.\r\nQUIT\r\n",
    )
    .await;
    assert!(out.contains("552 message too large"), "got: {out:?}");
    // Submission was rejected before DATA body was consumed, so
    // nothing in the log.
    assert!(log.snapshot().is_empty());
}

#[tokio::test]
async fn parse_mime_extracts_subject_and_attachment() {
    let script = b"\
        EHLO me\r\n\
        MAIL FROM:<a@b>\r\n\
        RCPT TO:<c@d>\r\n\
        DATA\r\n\
        From: a@b\r\n\
        To: c@d\r\n\
        Subject: with attach\r\n\
        MIME-Version: 1.0\r\n\
        Content-Type: multipart/mixed; boundary=\"BND\"\r\n\
        \r\n\
        --BND\r\n\
        Content-Type: text/plain; charset=utf-8\r\n\
        \r\n\
        hi there\r\n\
        --BND\r\n\
        Content-Type: application/pdf; name=\"r.pdf\"\r\n\
        Content-Disposition: attachment; filename=\"r.pdf\"\r\n\
        Content-Transfer-Encoding: base64\r\n\
        \r\n\
        SGVsbG8gV29ybGQ=\r\n\
        --BND--\r\n\
        .\r\n\
        QUIT\r\n";
    let (_, log) = run_with_log(script).await;
    let snap = log.snapshot();
    let parsed = snap[0].parse_mime().expect("parse_mime");
    assert_eq!(parsed.subject.as_deref(), Some("with attach"));
    assert!(parsed.text_bodies.iter().any(|t| t.contains("hi there")));
    assert_eq!(parsed.attachments.len(), 1);
    let att = &parsed.attachments[0];
    assert_eq!(att.filename.as_deref(), Some("r.pdf"));
    assert_eq!(att.content_type, "application/pdf");
    assert_eq!(att.data, b"Hello World");
}

/// Each SMTP verb appends a `(protocol="smtp", command, detail)`
/// entry. Verifies the dispatch hook fires for the full submission
/// path and that an empty (whitespace-only) line surfaces as a `""`
/// verb so harness scripts can detect protocol weirdness.
#[tokio::test]
async fn smtp_dispatch_records_request_log_entries() {
    use saehrimnir::request_log::RequestLog;

    let log = SubmissionLog::new();
    let log_clone = log.clone();
    let req_log = RequestLog::default();
    let req_log_clone = req_log.clone();
    let (server, mut client) = tokio::io::duplex(64 * 1024);
    let (_tx, rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut rx = rx;
        smtp::serve_connection(server, log_clone, None, default_fixture(), saehrimnir::oauth::TokenStore::default(), None, req_log_clone, saehrimnir::latency::LatencyKnob::default(), &mut rx).await
    });

    let script = b"\
        EHLO me\r\n\
        MAIL FROM:<a@b>\r\n\
        RCPT TO:<c@d>\r\n\
        DATA\r\n\
        Subject: hi\r\n\
        \r\n\
        body\r\n\
        .\r\n\
        QUIT\r\n";
    client.write_all(script).await.unwrap();
    client.shutdown().await.unwrap();
    let mut buf = Vec::new();
    client.read_to_end(&mut buf).await.unwrap();
    task.await.unwrap().unwrap();

    let snapshot = req_log.snapshot();
    let commands: Vec<&str> = snapshot
        .iter()
        .map(|e| {
            assert_eq!(e.protocol, "smtp");
            e.command.as_str()
        })
        .collect();
    assert_eq!(commands, ["EHLO", "MAIL", "RCPT", "DATA", "QUIT"]);
}

// STARTTLS test: spin up a real TCP listener with TLS, drive it with a
// tokio-rustls client that trusts everything (the server cert is
// self-signed so any cert verifier here is a no-op). Confirms the
// upgrade path doesn't drop bytes and capture works post-upgrade.
mod starttls {
    use super::*;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{ClientConfig, DigitallySignedStruct, Error, SignatureScheme};
    use rustls::client::danger::{
        HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
    };
    use std::sync::Arc;
    use tokio::net::TcpStream;
    use tokio_rustls::TlsConnector;

    #[derive(Debug)]
    struct AcceptAny;
    impl ServerCertVerifier for AcceptAny {
        fn verify_server_cert(
            &self,
            _: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: &ServerName<'_>,
            _: &[u8],
            _: UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            Ok(ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![
                SignatureScheme::RSA_PSS_SHA256,
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::ED25519,
            ]
        }
    }

    fn insecure_client_config() -> Arc<ClientConfig> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let cfg = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAny))
            .with_no_client_auth();
        Arc::new(cfg)
    }

    #[tokio::test]
    async fn starttls_upgrade_then_data_round_trip() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let log = SubmissionLog::new();
        let log_clone = log.clone();
        let (_tx, rx) = watch::channel(false);
        let acceptor =
            Arc::new(saehrimnir::tls::make_acceptor().expect("acceptor"));
        let server_task = tokio::spawn(async move {
            smtp::serve(listener, log_clone, None, default_fixture(), saehrimnir::oauth::TokenStore::default(), Some(acceptor), saehrimnir::request_log::RequestLog::default(), saehrimnir::latency::LatencyKnob::default(), rx).await
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let mut buf = [0u8; 1024];
        let mut stream = stream;
        // Read greeting.
        let mut greeting = String::new();
        let mut br = tokio::io::BufReader::new(&mut stream);
        tokio::io::AsyncBufReadExt::read_line(&mut br, &mut greeting)
            .await
            .unwrap();
        assert!(greeting.starts_with("220 "));

        // EHLO and consume capability list.
        stream.write_all(b"EHLO me\r\n").await.unwrap();
        let mut saw_starttls = false;
        loop {
            let n = stream.read(&mut buf).await.unwrap();
            let s = std::str::from_utf8(&buf[..n]).unwrap();
            if s.contains("STARTTLS") {
                saw_starttls = true;
            }
            if s.contains("250 ") {
                break;
            }
        }
        assert!(saw_starttls, "STARTTLS missing from EHLO reply");

        stream.write_all(b"STARTTLS\r\n").await.unwrap();
        let n = stream.read(&mut buf).await.unwrap();
        assert!(std::str::from_utf8(&buf[..n]).unwrap().starts_with("220 "));

        // Upgrade.
        let connector = TlsConnector::from(insecure_client_config());
        let domain = ServerName::try_from("localhost").unwrap();
        let mut tls = connector.connect(domain, stream).await.unwrap();

        // EHLO again, AUTH, MAIL, RCPT, DATA over TLS.
        let script = b"\
            EHLO me\r\n\
            AUTH PLAIN AGFsaWNlAGh1bnRlcg==\r\n\
            MAIL FROM:<a@b>\r\n\
            RCPT TO:<c@d>\r\n\
            DATA\r\n\
            Subject: secret\r\n\
            \r\n\
            shhh\r\n\
            .\r\n\
            QUIT\r\n";
        tls.write_all(script).await.unwrap();
        // Drain whatever the server replied; we just need the
        // connection to close cleanly.
        let mut sink = Vec::new();
        let _ = tls.read_to_end(&mut sink).await;

        let snap = log.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].from, "<a@b>");
        assert_eq!(snap[0].recipients, vec!["<c@d>".to_string()]);

        server_task.abort();
        let _ = server_task.await;
    }
}

// ── Multi-account AUTH binding (Stage 4) ────────────────────────────

async fn run_with_multi_account(
    script: &[u8],
    store: saehrimnir::oauth::TokenStore,
) -> (String, SubmissionLog) {
    let log = SubmissionLog::new();
    let log_clone = log.clone();
    let (server, mut client) = tokio::io::duplex(32 * 1024);
    let (_tx, rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut rx = rx;
        smtp::serve_connection(
            server,
            log_clone,
            None,
            multi_account_fixture(),
            store,
            None,
            saehrimnir::request_log::RequestLog::default(),
            saehrimnir::latency::LatencyKnob::default(),
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
async fn smtp_auth_plain_binds_submission_to_matching_account() {
    // SASL PLAIN: `\0secondary@example.com\0password`.
    let payload = b"\0secondary@example.com\0password";
    let encoded = base64_encode(payload);
    let script = format!(
        "EHLO me\r\n\
         AUTH PLAIN {encoded}\r\n\
         MAIL FROM:<x@y>\r\n\
         RCPT TO:<z@w>\r\n\
         DATA\r\n\
         x\r\n\
         .\r\n\
         QUIT\r\n"
    );
    let (_out, log) =
        run_with_multi_account(script.as_bytes(), saehrimnir::oauth::TokenStore::default()).await;
    let snap = log.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].account_id, "account-secondary");
}

#[tokio::test]
async fn smtp_auth_unrecognised_user_stays_on_primary() {
    let payload = b"\0nobody@example.com\0password";
    let encoded = base64_encode(payload);
    let script = format!(
        "EHLO me\r\n\
         AUTH PLAIN {encoded}\r\n\
         MAIL FROM:<x@y>\r\n\
         RCPT TO:<z@w>\r\n\
         DATA\r\n\
         x\r\n\
         .\r\n\
         QUIT\r\n"
    );
    let (_out, log) =
        run_with_multi_account(script.as_bytes(), saehrimnir::oauth::TokenStore::default()).await;
    let snap = log.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].account_id, "account-primary");
}

#[tokio::test]
async fn smtp_auth_xoauth2_binds_via_token_store() {
    let store = saehrimnir::oauth::TokenStore::default();
    let token = store.mint("authorization_code", "account-secondary", 1);
    let payload = format!(
        "user=secondary@example.com\x01auth=Bearer {token}\x01\x01"
    );
    let encoded = base64_encode(payload.as_bytes());
    let script = format!(
        "EHLO me\r\n\
         AUTH XOAUTH2 {encoded}\r\n\
         MAIL FROM:<x@y>\r\n\
         RCPT TO:<z@w>\r\n\
         DATA\r\n\
         x\r\n\
         .\r\n\
         QUIT\r\n"
    );
    let (_out, log) = run_with_multi_account(script.as_bytes(), store).await;
    let snap = log.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].account_id, "account-secondary");
}

#[tokio::test]
async fn smtp_no_auth_stays_on_primary() {
    // Submissions without AUTH inherit the connection's default
    // (primary). Matches the v0 no-auth baseline.
    let script = b"EHLO me\r\n\
         MAIL FROM:<x@y>\r\n\
         RCPT TO:<z@w>\r\n\
         DATA\r\n\
         x\r\n\
         .\r\n\
         QUIT\r\n";
    let (_out, log) =
        run_with_multi_account(script, saehrimnir::oauth::TokenStore::default()).await;
    let snap = log.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].account_id, "account-primary");
}
