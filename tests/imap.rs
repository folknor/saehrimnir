#![allow(clippy::unwrap_used)]

//! End-to-end IMAP test driving `serve_connection` over a duplex
//! stream. Mirrors the full initial-sync transcript ratatoskr would
//! issue against the canonical fixture.

use std::sync::Arc;

use saehrimnir::{fixture, imap, lua};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

async fn run_with_fixture(script: &[u8]) -> String {
    let fix = Arc::new(
        fixture::load(std::path::Path::new("fixtures/jmap-small.toml")).unwrap(),
    );
    let (server, mut client) = tokio::io::duplex(32 * 1024);
    let (_tx, rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut rx = rx;
        imap::serve_connection(server, fix, None, &mut rx).await
    });

    client.write_all(script).await.unwrap();
    client.shutdown().await.unwrap();

    let mut buf = Vec::new();
    client.read_to_end(&mut buf).await.unwrap();
    task.await.unwrap().unwrap();
    String::from_utf8(buf).unwrap()
}

/// Mirrors ratatoskr's initial-sync command sequence end-to-end.
#[tokio::test]
async fn full_initial_sync_transcript() {
    let script = b"\
        a1 CAPABILITY\r\n\
        a2 LOGIN \"alice\" \"hunter2\"\r\n\
        a3 ENABLE QRESYNC\r\n\
        a4 LIST \"\" \"*\"\r\n\
        a5 STATUS \"INBOX\" (MESSAGES UNSEEN UIDNEXT UIDVALIDITY HIGHESTMODSEQ)\r\n\
        a6 STATUS \"Archive\" (MESSAGES UNSEEN UIDNEXT UIDVALIDITY HIGHESTMODSEQ)\r\n\
        a7 SELECT \"INBOX\"\r\n\
        a8 UID SEARCH ALL\r\n\
        a9 UID FETCH 1:* (UID FLAGS INTERNALDATE BODY.PEEK[])\r\n\
        a10 UID FETCH 1:* (FLAGS) (CHANGEDSINCE 0)\r\n\
        a11 UID FETCH 1:* (FLAGS) (CHANGEDSINCE 1)\r\n\
        a12 CLOSE\r\n\
        a13 LOGOUT\r\n";
    let out = run_with_fixture(script).await;

    // Greeting.
    assert!(out.starts_with("* OK saehrimnir IMAP4rev1 ready\r\n"));

    // Capability + auth.
    assert!(out.contains("* CAPABILITY IMAP4REV1 CONDSTORE QRESYNC\r\n"));
    assert!(out.contains("a1 OK CAPABILITY completed\r\n"));
    assert!(out.contains("a2 OK [CAPABILITY IMAP4REV1 CONDSTORE QRESYNC] LOGIN completed\r\n"));
    assert!(out.contains("* ENABLED QRESYNC\r\n"));
    assert!(out.contains("a3 OK ENABLE completed\r\n"));

    // LIST emits both fixture mailboxes.
    assert!(out.contains("* LIST (\\Inbox) \"/\" \"INBOX\"\r\n"));
    assert!(out.contains("* LIST (\\Archive) \"/\" \"Archive\"\r\n"));
    assert!(out.contains("a4 OK LIST completed\r\n"));

    // STATUS for both. The canonical fixture has 2 emails in inbox,
    // 0 in archive.
    assert!(
        out.contains(
            "* STATUS \"INBOX\" (MESSAGES 2 UNSEEN 2 UIDNEXT 3 UIDVALIDITY 1 HIGHESTMODSEQ 1)\r\n"
        )
    );
    assert!(
        out.contains(
            "* STATUS \"Archive\" (MESSAGES 0 UNSEEN 0 UIDNEXT 1 UIDVALIDITY 1 HIGHESTMODSEQ 1)\r\n"
        )
    );

    // SELECT INBOX.
    assert!(out.contains("* 2 EXISTS\r\n"));
    assert!(out.contains("* OK [UIDVALIDITY 1]"));
    assert!(out.contains("a7 OK [READ-WRITE] SELECT completed\r\n"));

    // UID SEARCH ALL returns 1 2 (both UIDs ascending).
    assert!(out.contains("* SEARCH 1 2\r\n"));
    assert!(out.contains("a8 OK UID SEARCH completed\r\n"));

    // UID FETCH emits one * <seq> FETCH per message.
    assert!(out.contains("* 1 FETCH ("));
    assert!(out.contains("* 2 FETCH ("));
    assert!(out.contains("UID 1 FLAGS"));
    assert!(out.contains("UID 2 FLAGS"));
    assert!(out.contains("BODY[] {"));
    assert!(out.contains("a9 OK UID FETCH completed\r\n"));

    // CONDSTORE flag resync. CHANGEDSINCE 0 returns everything;
    // CHANGEDSINCE 1 returns nothing (modseq pinned at 1).
    assert!(out.contains("a10 OK UID FETCH completed\r\n"));
    let after_a10 = out.find("a10 OK").unwrap();
    let pre_a10 = &out[..after_a10];
    assert!(pre_a10.matches("FLAGS").count() > 0);
    let between = &out[after_a10..out.find("a11 OK").unwrap()];
    // Between a10 and a11, no fresh "* N FETCH" should appear because
    // CHANGEDSINCE 1 matches nothing.
    assert!(!between.contains("* 1 FETCH"));
    assert!(!between.contains("* 2 FETCH"));

    // CLOSE returns to Authenticated; LOGOUT closes.
    assert!(out.contains("a12 OK CLOSE completed\r\n"));
    assert!(out.contains("* BYE saehrimnir signing off\r\n"));
    assert!(out.contains("a13 OK LOGOUT completed\r\n"));
}

/// Verifies the BODY[] literal block byte count is correct - if the
/// announced size doesn't match the bytes that follow, real IMAP
/// clients hang.
#[tokio::test]
async fn body_literal_size_is_byte_accurate() {
    let script = b"\
        a LOGIN \"u\" \"p\"\r\n\
        b SELECT \"INBOX\"\r\n\
        c UID FETCH 1 (BODY.PEEK[])\r\n\
        q LOGOUT\r\n";
    let out = run_with_fixture(script).await;

    let body_start = out.find("BODY[] {").expect("BODY[] in output");
    let size_start = body_start + "BODY[] {".len();
    let size_end = out[size_start..].find('}').unwrap() + size_start;
    let size: usize = out[size_start..size_end].parse().unwrap();
    let payload_start = size_end + 3; // "}\r\n"
    let payload = &out[payload_start..payload_start + size];

    // Headers we know we synthesize.
    assert!(payload.contains("MIME-Version: 1.0\r\n"), "{payload:?}");
    assert!(payload.contains("Content-Type: text/plain; charset=utf-8\r\n"));
    assert!(payload.contains("\r\n\r\n"), "header/body boundary missing");
    // The fixture body for email-001 is "First message body."
    assert!(payload.ends_with("First message body."));
    // The byte after the literal payload should close the FETCH item
    // with `)\r\n`.
    let trailer = &out[payload_start + size..payload_start + size + 3];
    assert_eq!(trailer, ")\r\n", "literal length off by some bytes");
}

async fn run_with_attach_fixture(script: &[u8]) -> String {
    let fix = Arc::new(
        fixture::load(std::path::Path::new("fixtures/jmap-attach.toml")).unwrap(),
    );
    let (server, mut client) = tokio::io::duplex(32 * 1024);
    let (_tx, rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut rx = rx;
        imap::serve_connection(server, fix, None, &mut rx).await
    });

    client.write_all(script).await.unwrap();
    client.shutdown().await.unwrap();

    let mut buf = Vec::new();
    client.read_to_end(&mut buf).await.unwrap();
    task.await.unwrap().unwrap();
    String::from_utf8(buf).unwrap()
}

#[tokio::test]
async fn imap_multipart_email_emits_boundary_and_attachment_part() {
    let script = b"\
        a LOGIN \"u\" \"p\"\r\n\
        b SELECT \"INBOX\"\r\n\
        c UID FETCH 1 (BODY.PEEK[])\r\n\
        d UID FETCH 1 (BODYSTRUCTURE)\r\n\
        e UID FETCH 1 (BODY.PEEK[1] BODY.PEEK[2] BODY.PEEK[2.MIME])\r\n\
        q LOGOUT\r\n";
    let out = run_with_attach_fixture(script).await;

    // Multipart wire body: top-level Content-Type and boundary marker.
    let body_marker = "BODY[] {";
    let body_start = out.find(body_marker).expect("BODY[] in output");
    let after = &out[body_start..];
    assert!(after.contains("Content-Type: multipart/mixed; boundary=\"=_saehrimnir_email-001_=\""));
    assert!(after.contains("--=_saehrimnir_email-001_=\r\n"));
    assert!(after.contains("Content-Disposition: attachment; filename=\"sample.txt\""));
    assert!(after.contains("--=_saehrimnir_email-001_=--\r\n"));

    // BODYSTRUCTURE: nested multipart shape.
    assert!(out.contains("BODYSTRUCTURE ((\"TEXT\" \"PLAIN\""));
    assert!(out.contains("\"BASE64\""));
    assert!(out.contains("\"MIXED\" (\"BOUNDARY\" \"=_saehrimnir_email-001_=\")"));

    // BODY[1] = text body, BODY[2] = base64-encoded attachment, BODY[2.MIME] = part headers.
    assert!(out.contains("BODY[1] {"));
    assert!(out.contains("See attached."));
    assert!(out.contains("BODY[2] {"));
    assert!(out.contains("BODY[2.MIME] {"));
}

#[tokio::test]
async fn deterministic_two_runs_emit_identical_bytes() {
    // The byte-determinism contract: same fixture, same script -> same
    // bytes out. The IMAP path threads through `chrono` formatting
    // (INTERNALDATE, Date:) which is the most likely place to drift if
    // someone accidentally pulls in `Utc::now()`.
    let script = b"\
        a LOGIN \"u\" \"p\"\r\n\
        b SELECT \"INBOX\"\r\n\
        c UID FETCH 1:* (UID FLAGS INTERNALDATE BODY.PEEK[])\r\n\
        q LOGOUT\r\n";
    let a = run_with_fixture(script).await;
    let b = run_with_fixture(script).await;
    assert_eq!(a, b);
}

// ── Reactive-callback tests ────────────────────────────────────────

async fn run_with_lua_scenario(scenario: &str, imap_script: &[u8]) -> String {
    let (fix, dispatcher) =
        lua::load_source_with_dispatcher(scenario, "@cb-test").unwrap();
    let fix = Arc::new(fix);
    let dispatcher = Some(Arc::new(dispatcher));
    let (server, mut client) = tokio::io::duplex(64 * 1024);
    let (_tx, rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut rx = rx;
        imap::serve_connection(server, fix, dispatcher, &mut rx).await
    });
    client.write_all(imap_script).await.unwrap();
    client.shutdown().await.unwrap();
    let mut buf = Vec::new();
    client.read_to_end(&mut buf).await.unwrap();
    task.await.unwrap().unwrap();
    String::from_utf8(buf).unwrap()
}

#[tokio::test]
async fn uid_fetch_callback_overrides_with_tagged_no() {
    // Scenario: third UID FETCH returns NO. First two pass through
    // to the default handler (which emits FETCH responses).
    let scenario = r#"
        fixture({ name = "cb" })
        account({ id = "a", name = "a@b" })
        mailbox({ id = "mb-inbox", name = "Inbox", role = "inbox" })
        email({
            id = "e1",
            mailbox_ids = {"mb-inbox"},
            received_at = "2026-01-15T10:00:00Z",
            body_text = "hi",
        })
        on("imap", "UID FETCH", function(req)
            if req.call_index == 3 then
                return { status = "NO", message = "transient failure" }
            end
        end)
    "#;
    let imap_script = b"\
        a LOGIN \"u\" \"p\"\r\n\
        b SELECT \"INBOX\"\r\n\
        c1 UID FETCH 1 (UID)\r\n\
        c2 UID FETCH 1 (UID)\r\n\
        c3 UID FETCH 1 (UID)\r\n\
        c4 UID FETCH 1 (UID)\r\n\
        q LOGOUT\r\n";
    let out = run_with_lua_scenario(scenario, imap_script).await;

    // Calls 1 and 2: default (FETCH untagged + tagged OK).
    assert!(out.contains("c1 OK UID FETCH completed"), "got: {out:?}");
    assert!(out.contains("c2 OK UID FETCH completed"));
    // Call 3: overridden to NO, no FETCH untagged emitted.
    assert!(
        out.contains("c3 NO transient failure"),
        "missing override on call 3: {out:?}"
    );
    // Call 4: back to default behaviour.
    assert!(out.contains("c4 OK UID FETCH completed"));
}

#[tokio::test]
async fn uid_fetch_callback_pass_through_returns_default() {
    // Scenario registers a handler but it always returns nil -
    // every FETCH should behave normally.
    let scenario = r#"
        fixture({ name = "passthrough" })
        account({ id = "a", name = "a@b" })
        mailbox({ id = "mb-inbox", name = "Inbox", role = "inbox" })
        email({
            id = "e1",
            mailbox_ids = {"mb-inbox"},
            received_at = "2026-01-15T10:00:00Z",
            body_text = "hi",
        })
        on("imap", "UID FETCH", function(req)
            -- always pass through
            return nil
        end)
    "#;
    let imap_script = b"\
        a LOGIN \"u\" \"p\"\r\n\
        b SELECT \"INBOX\"\r\n\
        c UID FETCH 1 (UID)\r\n\
        q LOGOUT\r\n";
    let out = run_with_lua_scenario(scenario, imap_script).await;
    assert!(out.contains("* 1 FETCH (UID 1)"));
    assert!(out.contains("c OK UID FETCH completed"));
}

#[tokio::test]
async fn uid_fetch_callback_sees_request_fields() {
    // Scenario inspects req.uid_set, req.attrs, req.mailbox and
    // emits a status that echoes them back. Validates that the
    // dispatcher populates the req table with the IMAP-specific
    // fields the protocol layer pushes.
    let scenario = r#"
        fixture({ name = "echo" })
        account({ id = "a", name = "a@b" })
        mailbox({ id = "mb-inbox", name = "Inbox", role = "inbox" })
        email({
            id = "e1",
            mailbox_ids = {"mb-inbox"},
            received_at = "2026-01-15T10:00:00Z",
            body_text = "hi",
        })
        on("imap", "UID FETCH", function(req)
            return {
                status = "NO",
                message = "set=" .. req.uid_set ..
                    " attrs=" .. req.attrs ..
                    " mb=" .. req.mailbox,
            }
        end)
    "#;
    let imap_script = b"\
        a LOGIN \"u\" \"p\"\r\n\
        b SELECT \"INBOX\"\r\n\
        c UID FETCH 1:5 (UID FLAGS)\r\n\
        q LOGOUT\r\n";
    let out = run_with_lua_scenario(scenario, imap_script).await;
    assert!(
        out.contains("c NO set=1:5 attrs=(UID FLAGS) mb=mb-inbox"),
        "got: {out:?}"
    );
}

#[tokio::test]
async fn uid_fetch_no_handler_passes_through_silently() {
    // Scenario has a dispatcher (it's a Lua scenario) but no
    // on("imap", "UID FETCH", ...) registered. Should behave
    // identically to a TOML scenario.
    let scenario = r#"
        fixture({ name = "no-handler" })
        account({ id = "a", name = "a@b" })
        mailbox({ id = "mb-inbox", name = "Inbox", role = "inbox" })
        email({
            id = "e1",
            mailbox_ids = {"mb-inbox"},
            received_at = "2026-01-15T10:00:00Z",
            body_text = "hi",
        })
    "#;
    let imap_script = b"\
        a LOGIN \"u\" \"p\"\r\n\
        b SELECT \"INBOX\"\r\n\
        c UID FETCH 1 (UID)\r\n\
        q LOGOUT\r\n";
    let out = run_with_lua_scenario(scenario, imap_script).await;
    assert!(out.contains("* 1 FETCH (UID 1)"));
    assert!(out.contains("c OK UID FETCH completed"));
}

async fn run_with_imap_small(script: &[u8]) -> String {
    let fix = Arc::new(fixture::load(std::path::Path::new("fixtures/imap-small.toml")).unwrap());
    let (server, mut client) = tokio::io::duplex(32 * 1024);
    let (_tx, rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut rx = rx;
        imap::serve_connection(server, fix, None, &mut rx).await
    });

    client.write_all(script).await.unwrap();
    client.shutdown().await.unwrap();

    let mut buf = Vec::new();
    client.read_to_end(&mut buf).await.unwrap();
    task.await.unwrap().unwrap();
    String::from_utf8(buf).unwrap()
}

/// `fixtures/imap-small.toml` is the M8 IMAP smoke fixture. It mirrors
/// jmap-small.toml (same two messages, same Message-IDs, same thread
/// shape) but adds IMAP-native coverage: $seen on email-001,
/// $flagged on email-002. This test asserts the keyword -> flag
/// projection and the resulting UNSEEN counter.
#[tokio::test]
async fn imap_small_fixture_projects_seen_and_flagged() {
    let script = b"\
        a1 LOGIN \"u\" \"p\"\r\n\
        a2 STATUS \"INBOX\" (MESSAGES UNSEEN UIDNEXT)\r\n\
        a3 SELECT \"INBOX\"\r\n\
        a4 UID FETCH 1:* (UID FLAGS)\r\n\
        a5 LOGOUT\r\n";
    let out = run_with_imap_small(script).await;

    // STATUS: 2 messages, 1 unseen (only email-002 lacks $seen).
    assert!(out.contains("* STATUS \"INBOX\" (MESSAGES 2 UNSEEN 1 UIDNEXT 3)\r\n"));

    // FETCH FLAGS: UID 1 has \Seen, UID 2 has \Flagged.
    assert!(out.contains("* 1 FETCH (UID 1 FLAGS (\\Seen))\r\n"));
    assert!(out.contains("* 2 FETCH (UID 2 FLAGS (\\Flagged))\r\n"));
    assert!(out.contains("a4 OK UID FETCH completed\r\n"));
}
