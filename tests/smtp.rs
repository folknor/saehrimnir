#![allow(clippy::unwrap_used)]

//! End-to-end SMTP test driving `serve_connection` over a duplex
//! stream and asserting on the captured submission log.

use saehrimnir::lua;
use saehrimnir::smtp::{self, SubmissionLog};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

async fn run_with_log(script: &[u8]) -> (String, SubmissionLog) {
    let log = SubmissionLog::new();
    let log_clone = log.clone();
    let (server, mut client) = tokio::io::duplex(64 * 1024);
    let (_tx, rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut rx = rx;
        smtp::serve_connection(server, log_clone, None, &mut rx).await
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
        smtp::serve_connection(server, log_clone, dispatcher, &mut rx).await
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
