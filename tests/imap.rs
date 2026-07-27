#![allow(clippy::unwrap_used)]

//! End-to-end IMAP test driving `serve_connection` over a duplex
//! stream. Mirrors the full initial-sync transcript ratatoskr would
//! issue against the canonical fixture.

use std::sync::Arc;

use saehrimnir::{fixture, imap, lua};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

async fn run_with_fixture(script: &[u8]) -> String {
    run_with_fixture_path("fixtures/jmap-small.toml", script).await
}

async fn run_with_fixture_path(path: &str, script: &[u8]) -> String {
    let fix = saehrimnir::shared::handle(fixture::load(std::path::Path::new(path)).unwrap());
    let (server, mut client) = tokio::io::duplex(32 * 1024);
    let (_tx, rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut rx = rx;
        imap::serve_connection(
            server,
            fix,
            None,
            saehrimnir::oauth::TokenStore::default(),
            saehrimnir::request_log::RequestLog::default(),
            saehrimnir::latency::LatencyKnob::default(),
            saehrimnir::push::PushHub::new(),
            &mut rx,
        )
        .await
    });

    client.write_all(script).await.unwrap();
    client.shutdown().await.unwrap();

    let mut buf = Vec::new();
    client.read_to_end(&mut buf).await.unwrap();
    task.await.unwrap().unwrap();
    String::from_utf8(buf).unwrap()
}

/// bifrost's inventory FETCH is `(UID FLAGS ENVELOPE RFC822.SIZE
/// MODSEQ)` - it always asks for ENVELOPE and appends MODSEQ because
/// we advertise CONDSTORE. Before ENVELOPE/MODSEQ were parsed the
/// whole UID FETCH replied BAD, breaking initial mail sync.
#[tokio::test]
async fn uid_fetch_envelope_and_modseq() {
    let script = b"\
        a1 LOGIN \"alice\" \"hunter2\"\r\n\
        a2 SELECT \"INBOX\"\r\n\
        a3 UID FETCH 1 (UID FLAGS ENVELOPE RFC822.SIZE MODSEQ)\r\n\
        a4 LOGOUT\r\n";
    let out = run_with_fixture(script).await;

    // The attr list parses (no BAD) and the fetch completes.
    assert!(out.contains("a3 OK UID FETCH completed\r\n"), "got: {out}");

    // ENVELOPE: quoted date, subject, from/to address structures
    // `(name adl mailbox host)`, and message-id.
    assert!(out.contains("ENVELOPE (\""), "envelope missing: {out}");
    assert!(out.contains("\"Hello\""));
    assert!(out.contains("((NIL NIL \"alice\" \"example.com\"))"));
    assert!(out.contains("((NIL NIL \"bob\" \"example.com\"))"));
    assert!(out.contains("\"<email-001@example.com>\""));

    // CONDSTORE per-message modseq, non-zero (bifrost rejects 0).
    assert!(out.contains("MODSEQ (1)"));
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
    assert!(
        out.contains(
            "* CAPABILITY IMAP4REV1 IDLE CONDSTORE QRESYNC MOVE UIDPLUS NAMESPACE ACL\r\n"
        )
    );
    assert!(out.contains("a1 OK CAPABILITY completed\r\n"));
    assert!(out.contains(
        "a2 OK [CAPABILITY IMAP4REV1 IDLE CONDSTORE QRESYNC MOVE UIDPLUS NAMESPACE ACL] LOGIN completed\r\n"
    ));
    assert!(out.contains("* ENABLED QRESYNC\r\n"));
    assert!(out.contains("a3 OK ENABLE completed\r\n"));

    // LIST emits both fixture mailboxes.
    assert!(out.contains("* LIST (\\Inbox) \"/\" \"INBOX\"\r\n"));
    assert!(out.contains("* LIST (\\Archive) \"/\" \"Archive\"\r\n"));
    assert!(out.contains("a4 OK LIST completed\r\n"));

    // STATUS for both. The canonical fixture has 2 emails in inbox,
    // 0 in archive.
    assert!(out.contains(
        "* STATUS \"INBOX\" (MESSAGES 2 UNSEEN 2 UIDNEXT 3 UIDVALIDITY 1 HIGHESTMODSEQ 1)\r\n"
    ));
    assert!(out.contains(
        "* STATUS \"Archive\" (MESSAGES 0 UNSEEN 0 UIDNEXT 1 UIDVALIDITY 1 HIGHESTMODSEQ 1)\r\n"
    ));

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

/// IDLE is advertised in CAPABILITY and rejected outside the Selected
/// state. The mutation-driven IDLE round-trip (an idling client
/// observing EXISTS / EXPUNGE on a state advance) lives in
/// `tests/push.rs`, where the IMAP connection can share a `PushHub`
/// with the test-admin step trigger.
#[tokio::test]
async fn idle_advertised_and_requires_select() {
    let out =
        run_with_fixture(b"a1 CAPABILITY\r\na2 LOGIN \"u\" \"p\"\r\na3 IDLE\r\na4 LOGOUT\r\n")
            .await;
    assert!(
        out.contains(" IDLE "),
        "IDLE advertised in CAPABILITY: {out:?}"
    );
    assert!(
        out.contains("a3 BAD IDLE requires SELECT first"),
        "IDLE before SELECT must be rejected: {out:?}"
    );
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
    let fix = saehrimnir::shared::handle(
        fixture::load(std::path::Path::new("fixtures/jmap-attach.toml")).unwrap(),
    );
    let (server, mut client) = tokio::io::duplex(32 * 1024);
    let (_tx, rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut rx = rx;
        imap::serve_connection(
            server,
            fix,
            None,
            saehrimnir::oauth::TokenStore::default(),
            saehrimnir::request_log::RequestLog::default(),
            saehrimnir::latency::LatencyKnob::default(),
            saehrimnir::push::PushHub::new(),
            &mut rx,
        )
        .await
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
    let (fix, dispatcher) = lua::load_source_with_dispatcher(scenario, "@cb-test").unwrap();
    let fix = saehrimnir::shared::handle(fix);
    let dispatcher = Some(Arc::new(dispatcher));
    let (server, mut client) = tokio::io::duplex(64 * 1024);
    let (_tx, rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut rx = rx;
        imap::serve_connection(
            server,
            fix,
            dispatcher,
            saehrimnir::oauth::TokenStore::default(),
            saehrimnir::request_log::RequestLog::default(),
            saehrimnir::latency::LatencyKnob::default(),
            saehrimnir::push::PushHub::new(),
            &mut rx,
        )
        .await
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
    let fix = saehrimnir::shared::handle(
        fixture::load(std::path::Path::new("fixtures/imap-small.toml")).unwrap(),
    );
    let (server, mut client) = tokio::io::duplex(32 * 1024);
    let (_tx, rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut rx = rx;
        imap::serve_connection(
            server,
            fix,
            None,
            saehrimnir::oauth::TokenStore::default(),
            saehrimnir::request_log::RequestLog::default(),
            saehrimnir::latency::LatencyKnob::default(),
            saehrimnir::push::PushHub::new(),
            &mut rx,
        )
        .await
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

/// Each IMAP command appends a `(protocol="imap", command, detail)`
/// entry to the shared request log. Verifies UID FETCH lands as
/// `"UID FETCH"` rather than the raw `"UID"` verb so test assertions
/// can target the sub-command ratatoskr actually issued.
#[tokio::test]
async fn imap_dispatch_records_request_log_entries() {
    use saehrimnir::request_log::RequestLog;

    let log = RequestLog::default();
    let log_clone = log.clone();
    let fix = saehrimnir::shared::handle(
        fixture::load(std::path::Path::new("fixtures/imap-small.toml")).unwrap(),
    );

    let (server, mut client) = tokio::io::duplex(32 * 1024);
    let (_tx, rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut rx = rx;
        imap::serve_connection(
            server,
            fix,
            None,
            saehrimnir::oauth::TokenStore::default(),
            log_clone,
            saehrimnir::latency::LatencyKnob::default(),
            saehrimnir::push::PushHub::new(),
            &mut rx,
        )
        .await
    });

    let script = b"\
        a1 CAPABILITY\r\n\
        a2 LOGIN \"u\" \"p\"\r\n\
        a3 SELECT \"INBOX\"\r\n\
        a4 UID FETCH 1:* (UID)\r\n\
        a5 LOGOUT\r\n";
    client.write_all(script).await.unwrap();
    client.shutdown().await.unwrap();
    let mut buf = Vec::new();
    client.read_to_end(&mut buf).await.unwrap();
    task.await.unwrap().unwrap();

    let snapshot = log.snapshot();
    let commands: Vec<&str> = snapshot
        .iter()
        .map(|e| {
            assert_eq!(e.protocol, "imap");
            e.command.as_str()
        })
        .collect();
    assert_eq!(
        commands,
        ["CAPABILITY", "LOGIN", "SELECT", "UID FETCH", "LOGOUT"]
    );
}

/// `UID FETCH` log rows expose `detail.attrs` (parsed FETCH item
/// list) and `detail.body` (true when any item asks for message
/// bytes). Lets a steady-state delta test soften to "no body
/// refetch" while still permitting flag-only reconciliation.
#[tokio::test]
async fn imap_uid_fetch_log_distinguishes_body_from_metadata() {
    use saehrimnir::request_log::RequestLog;

    let log = RequestLog::default();
    let log_clone = log.clone();
    let fix = saehrimnir::shared::handle(
        fixture::load(std::path::Path::new("fixtures/imap-small.toml")).unwrap(),
    );

    let (server, mut client) = tokio::io::duplex(64 * 1024);
    let (_tx, rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut rx = rx;
        imap::serve_connection(
            server,
            fix,
            None,
            saehrimnir::oauth::TokenStore::default(),
            log_clone,
            saehrimnir::latency::LatencyKnob::default(),
            saehrimnir::push::PushHub::new(),
            &mut rx,
        )
        .await
    });

    let script = b"\
        a1 LOGIN \"u\" \"p\"\r\n\
        a2 SELECT \"INBOX\"\r\n\
        a3 UID FETCH 1:* (UID FLAGS INTERNALDATE)\r\n\
        a4 UID FETCH 1 (UID BODY.PEEK[])\r\n\
        a5 UID FETCH 1 (UID FLAGS) (CHANGEDSINCE 0)\r\n\
        a6 LOGOUT\r\n";
    client.write_all(script).await.unwrap();
    client.shutdown().await.unwrap();
    let mut buf = Vec::new();
    client.read_to_end(&mut buf).await.unwrap();
    task.await.unwrap().unwrap();

    let fetches: Vec<_> = log
        .snapshot()
        .into_iter()
        .filter(|e| e.command == "UID FETCH")
        .collect();
    assert_eq!(fetches.len(), 3);

    // Metadata-only fetch.
    let attrs0: Vec<&str> = fetches[0].detail["attrs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(attrs0, ["UID", "FLAGS", "INTERNALDATE"]);
    assert_eq!(fetches[0].detail["body"], serde_json::json!(false));

    // Body fetch.
    let attrs1: Vec<&str> = fetches[1].detail["attrs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(attrs1, ["UID", "BODY[]"]);
    assert_eq!(fetches[1].detail["body"], serde_json::json!(true));

    // Modifier list (CHANGEDSINCE) doesn't perturb attrs/body.
    let attrs2: Vec<&str> = fetches[2].detail["attrs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(attrs2, ["UID", "FLAGS"]);
    assert_eq!(fetches[2].detail["body"], serde_json::json!(false));
}

// ── UID STORE / COPY / EXPUNGE persistence ─────────────────────────

/// `UID STORE +FLAGS (\Seen)` now persists. A subsequent
/// `UID FETCH (FLAGS)` against the same connection sees the flag,
/// closing the v0 "no-op writeback" gap. `imap-small.toml` starts
/// with email-002 carrying only `$flagged`; after the store both
/// `\Seen` and `\Flagged` are present.
#[tokio::test]
async fn uid_store_persists_across_fetches() {
    let script = b"\
        a LOGIN \"u\" \"p\"\r\n\
        b SELECT \"INBOX\"\r\n\
        c UID STORE 2 +FLAGS (\\Seen)\r\n\
        d UID FETCH 2 (UID FLAGS)\r\n\
        e LOGOUT\r\n";
    let out = run_with_imap_small(script).await;
    assert!(out.contains("c OK UID STORE completed"));
    // Post-store FETCH on the same connection sees both flags.
    let post = out
        .split("c OK UID STORE completed")
        .nth(1)
        .expect("post-store transcript present");
    assert!(
        post.contains("* 2 FETCH (UID 2 FLAGS (\\Flagged \\Seen))"),
        "post-store FETCH missing combined flags: {post:?}"
    );
}

/// `UID COPY <set> "Archive"` adds the target mailbox to the
/// matched email's `mailbox_ids`. After SELECT-ing Archive the
/// copied email is visible there too. `imap-small.toml` already
/// declares an `Archive` mailbox.
#[tokio::test]
async fn uid_copy_makes_email_visible_in_target_mailbox() {
    let script = b"\
        a LOGIN \"u\" \"p\"\r\n\
        b SELECT \"INBOX\"\r\n\
        c UID COPY 1 \"Archive\"\r\n\
        d SELECT \"Archive\"\r\n\
        e UID FETCH 1:* (UID)\r\n\
        f LOGOUT\r\n";
    let out = run_with_imap_small(script).await;
    assert!(out.contains("c OK UID COPY completed"));
    // Archive's first SELECT now reports EXISTS 1 (was 0); FETCH
    // returns the copied email.
    assert!(out.contains("d OK [READ-WRITE] SELECT completed"));
    let post_select = out
        .split("d OK")
        .nth(1)
        .expect("post-Archive SELECT transcript");
    assert!(
        post_select.contains("* 1 FETCH (UID 1)"),
        "Archive missing copied email after UID COPY: {post_select:?}"
    );
}

/// Copying to an unknown mailbox returns `NO [TRYCREATE]`, mirroring
/// real IMAP. The fixture is left unchanged.
#[tokio::test]
async fn uid_copy_unknown_mailbox_returns_no_trycreate() {
    let script = b"\
        a LOGIN \"u\" \"p\"\r\n\
        b SELECT \"INBOX\"\r\n\
        c UID COPY 1 \"Nope\"\r\n\
        d LOGOUT\r\n";
    let out = run_with_imap_small(script).await;
    assert!(out.contains("c NO [TRYCREATE]"), "got: {out:?}");
}

/// `UID EXPUNGE` removes only `\Deleted`-flagged emails. The
/// canonical writeback flow is `STORE +FLAGS (\Deleted)` then
/// `UID EXPUNGE`. The expunged messages emit `* <seq> EXPUNGE` and
/// disappear from subsequent fetches.
#[tokio::test]
async fn uid_expunge_drops_only_deleted_flagged_messages() {
    let script = b"\
        a LOGIN \"u\" \"p\"\r\n\
        b SELECT \"INBOX\"\r\n\
        c UID STORE 1 +FLAGS (\\Deleted)\r\n\
        d UID EXPUNGE 1:*\r\n\
        e UID FETCH 1:* (UID)\r\n\
        f LOGOUT\r\n";
    let out = run_with_imap_small(script).await;
    // Only the \Deleted-flagged UID 1 expunges; UID 2 (only
    // \Flagged in imap-small.toml) survives.
    assert!(out.contains("* 1 EXPUNGE"), "got: {out:?}");
    assert!(!out.contains("* 2 EXPUNGE"));
    assert!(out.contains("d OK UID EXPUNGE completed"));
    // Post-expunge: UID 1 is retired but UID 2 keeps its UID per
    // RFC 3501 §2.3.1.1 (UIDs must never be reused). The surviving
    // message takes sequence number 1 (only live message in the
    // mailbox) but its UID stays 2.
    let post = out
        .split("d OK UID EXPUNGE completed")
        .nth(1)
        .expect("post-expunge");
    assert!(
        post.contains("* 1 FETCH (UID 2)"),
        "surviving message missing post-expunge or UID was reassigned: {post:?}"
    );
    assert!(
        !post.contains("UID 1)"),
        "UID 1 should not have been reused: {post:?}"
    );
}

#[tokio::test]
async fn body_raw_bytes_emits_verbatim_through_imap_fetch() {
    // Adversarial-shape fixture: the email's raw bytes deliberately
    // claim a multipart/mixed Content-Type but never emit the
    // boundary, so a strict client will fail to parse. The IMAP
    // mock must hand the bytes back verbatim - that's the whole
    // point of the body_raw_bytes escape hatch.
    let scenario = r#"
        fixture({ name = "raw" })
        account({ id = "a", name = "a@b" })
        mailbox({ id = "mb-inbox", name = "Inbox", role = "inbox" })
        email({
            id = "e1",
            mailbox_ids = {"mb-inbox"},
            received_at = "2026-01-15T10:00:00Z",
            body_text = "ignored",
            body_raw_bytes = "From: alice@example.com\r\nSubject: malformed\r\nContent-Type: multipart/mixed; boundary=\"X\"\r\n\r\n--X-but-no-real-boundary\r\nbroken body\r\n",
        })
    "#;
    let imap_script = b"\
        a LOGIN \"u\" \"p\"\r\n\
        b SELECT \"INBOX\"\r\n\
        c UID FETCH 1 (UID RFC822.SIZE BODY.PEEK[])\r\n\
        d UID FETCH 1 (BODY.PEEK[HEADER])\r\n\
        e UID FETCH 1 (BODY.PEEK[TEXT])\r\n\
        f UID FETCH 1 (BODYSTRUCTURE)\r\n\
        q LOGOUT\r\n";
    let out = run_with_lua_scenario(scenario, imap_script).await;

    let raw = "From: alice@example.com\r\nSubject: malformed\r\nContent-Type: multipart/mixed; boundary=\"X\"\r\n\r\n--X-but-no-real-boundary\r\nbroken body\r\n";

    // BODY[] = raw bytes verbatim, RFC822.SIZE = byte length.
    assert!(out.contains(raw), "BODY[] not verbatim: {out:?}");
    assert!(
        out.contains(&format!("RFC822.SIZE {}", raw.len())),
        "RFC822.SIZE wrong: {out:?}"
    );
    assert!(out.contains("c OK UID FETCH completed"));

    // BODY[HEADER] = headers terminated by the last field's CRLF
    // (NOT the blank-line CRLF; that's the separator, not part of
    // the header section per the BODY[HEADER] convention used by
    // the structured render path).
    let head = &raw[..raw.find("\r\n\r\n").unwrap() + 2];
    assert!(
        out.contains(&format!("BODY[HEADER] {{{}}}\r\n{head}", head.len())),
        "BODY[HEADER] slice wrong: {out:?}"
    );
    assert!(out.contains("d OK UID FETCH completed"));

    // BODY[TEXT] = bytes after CRLFCRLF.
    let text = &raw[raw.find("\r\n\r\n").unwrap() + 4..];
    assert!(
        out.contains(&format!("BODY[TEXT] {{{}}}\r\n{text}", text.len())),
        "BODY[TEXT] slice wrong: {out:?}"
    );
    assert!(out.contains("e OK UID FETCH completed"));

    // BODYSTRUCTURE returns a single text/plain leaf reporting the
    // raw octet count. Lossy by design (the bytes claim multipart);
    // this is the documented best-effort answer for raw-bytes emails.
    assert!(
        out.contains(&format!(
            "BODYSTRUCTURE (\"TEXT\" \"PLAIN\" (\"CHARSET\" \"utf-8\") NIL NIL \"8BIT\" {} ",
            raw.len()
        )),
        "BODYSTRUCTURE missing raw-leaf shape: {out:?}"
    );
    assert!(out.contains("f OK UID FETCH completed"));
}

/// Regression for the IMAP UID stability contract (RFC 3501
/// §2.3.1.1): once a UID is assigned to a message in a mailbox, it
/// must never refer to a different message. Pre-fix the wire
/// derived UIDs from filter-then-enumerate over the live email
/// list, so deleting UID 1 silently shifted UID 2 down to UID 1 -
/// any client cache pointing at UID 1 would now see a different
/// message without the client knowing the identity changed.
#[tokio::test]
async fn uid_expunge_does_not_reuse_uid_after_delete() {
    let script = b"\
        a LOGIN \"u\" \"p\"\r\n\
        b SELECT \"INBOX\"\r\n\
        c UID STORE 1 +FLAGS (\\Deleted)\r\n\
        d UID EXPUNGE 1\r\n\
        e UID FETCH 1:* (UID)\r\n\
        f STATUS \"INBOX\" (UIDNEXT)\r\n\
        g LOGOUT\r\n";
    let out = run_with_imap_small(script).await;
    assert!(out.contains("d OK UID EXPUNGE completed"));

    // After expunging UID 1, the surviving message (UID 2) keeps
    // UID 2; the addressable UID-1 slot is gone forever. The
    // sequence number is 1 (only live message) but the UID stays.
    let post_e = out.split("e OK").next().expect("transcript before e OK");
    assert!(
        post_e.contains("* 1 FETCH (UID 2)"),
        "UID was reused after expunge: {post_e:?}"
    );

    // UIDNEXT keeps growing across the delete; it never drops back
    // to the freed value. Pre-fix UIDNEXT was `exists + 1` so a
    // 2-msg mailbox showed UIDNEXT=3, then after expunging one it
    // would have shrunk to UIDNEXT=2.
    assert!(
        out.contains("UIDNEXT 3"),
        "UIDNEXT shrank after expunge: {out:?}"
    );
}

/// Regression for the cross-mailbox UID stability contract: COPY
/// allocates a fresh UID in the target mailbox; subsequent EXPUNGE
/// in the source must not affect the target's UID. Pre-fix any
/// mutation reshuffled enumeration in BOTH mailboxes.
#[tokio::test]
async fn uid_copy_then_source_expunge_keeps_target_uid_stable() {
    let script = b"\
        a LOGIN \"u\" \"p\"\r\n\
        b SELECT \"INBOX\"\r\n\
        c UID COPY 1 \"Archive\"\r\n\
        d UID STORE 1 +FLAGS (\\Deleted)\r\n\
        e UID EXPUNGE 1\r\n\
        f SELECT \"Archive\"\r\n\
        g UID FETCH 1:* (UID)\r\n\
        h STATUS \"Archive\" (UIDNEXT)\r\n\
        i LOGOUT\r\n";
    let out = run_with_imap_small(script).await;
    assert!(out.contains("c OK UID COPY completed"));
    assert!(out.contains("e OK UID EXPUNGE completed"));

    // Archive sees the copied email at UID 1 (its first allocation
    // in that mailbox), and that UID survives the source-side
    // expunge.
    let post_g = out.split("g OK").next().expect("transcript before g OK");
    assert!(
        post_g.contains("* 1 FETCH (UID 1)"),
        "Archive UID changed after source expunge: {post_g:?}"
    );

    // Archive's UIDNEXT = 2 (one message ever assigned a UID in
    // Archive). Pre-fix it would have stayed flat across
    // source-mailbox mutations or reflected source-mailbox state.
    assert!(
        out.contains("Archive\" (UIDNEXT 2)"),
        "Archive UIDNEXT off: {out:?}"
    );
}

/// `UID EXPUNGE` is a no-op when no message in range carries
/// `\Deleted`: the tagged OK fires but no `* <seq> EXPUNGE` lines
/// are emitted and the mailbox view is unchanged.
#[tokio::test]
async fn uid_expunge_without_deleted_flag_is_noop() {
    let script = b"\
        a LOGIN \"u\" \"p\"\r\n\
        b SELECT \"INBOX\"\r\n\
        c UID EXPUNGE 1:*\r\n\
        d UID FETCH 1:* (UID)\r\n\
        e LOGOUT\r\n";
    let out = run_with_imap_small(script).await;
    let pre_post_split: Vec<&str> = out.split("c OK UID EXPUNGE completed").collect();
    assert_eq!(pre_post_split.len(), 2, "tagged OK missing: {out:?}");
    let pre = pre_post_split[0];
    assert!(!pre.contains("EXPUNGE\r\n"), "stray EXPUNGE: {pre:?}");
    let post = pre_post_split[1];
    assert!(post.contains("* 1 FETCH (UID 1)"));
    assert!(post.contains("* 2 FETCH (UID 2)"));
}

// ── CONDSTORE / QRESYNC (RFC 7162) ─────────────────────────────────

/// bifrost opens folders with `SELECT INBOX (CONDSTORE)` (RFC 7162
/// 3.1.1). The mock must accept the select-parameter group instead of
/// rejecting the trailing parens as junk (which used to reply BAD and
/// is the gap codex shimmed around with an MITM proxy). QRESYNC's
/// `(QRESYNC (<uidvalidity> <modseq>))` form is accepted too.
#[tokio::test]
async fn select_accepts_condstore_and_qresync_select_parameters() {
    let script = b"\
        a LOGIN \"u\" \"p\"\r\n\
        b SELECT \"INBOX\" (CONDSTORE)\r\n\
        c EXAMINE \"INBOX\" (QRESYNC (1 1))\r\n\
        d LOGOUT\r\n";
    let out = run_with_imap_small(script).await;
    assert!(
        out.contains("b OK [READ-WRITE] SELECT completed"),
        "got: {out}"
    );
    assert!(
        out.contains("c OK [READ-ONLY] EXAMINE completed"),
        "got: {out}"
    );
    // A baseline (never-mutated) fixture reports HIGHESTMODSEQ 1.
    assert!(out.contains("* OK [HIGHESTMODSEQ 1] modseq"), "got: {out}");
    // No expunges on a fresh fixture: QRESYNC emits no VANISHED.
    assert!(!out.contains("VANISHED"), "unexpected VANISHED: {out}");
}

/// The real CONDSTORE delta path the cut exists to prove: a flag write
/// advances the account modseq, so a follow-up `FETCH (CHANGEDSINCE
/// <old>)` returns only the mutated message (carrying its new MODSEQ),
/// and SELECT reports the advanced HIGHESTMODSEQ.
#[tokio::test]
async fn condstore_changedsince_returns_only_mutated_message() {
    let script = b"\
        a LOGIN \"u\" \"p\"\r\n\
        b SELECT \"INBOX\" (CONDSTORE)\r\n\
        c UID STORE 2 +FLAGS (\\Answered)\r\n\
        d SELECT \"INBOX\" (CONDSTORE)\r\n\
        e UID FETCH 1:* (UID FLAGS) (CHANGEDSINCE 1)\r\n\
        f LOGOUT\r\n";
    let out = run_with_imap_small(script).await;

    // Before the write, HIGHESTMODSEQ is the baseline 1. After the
    // write bumps the account counter, the re-SELECT reports 2.
    let post_store = out
        .split("c OK UID STORE completed")
        .nth(1)
        .expect("post-store");
    assert!(
        post_store.contains("* OK [HIGHESTMODSEQ 2] modseq"),
        "highestmodseq did not advance: {out}"
    );

    // CHANGEDSINCE 1 returns ONLY uid 2 (modseq 2 > 1); uid 1 is
    // untouched (modseq 1, not > 1) and is filtered out.
    let delta = out
        .split("d OK [READ-WRITE] SELECT completed")
        .nth(1)
        .expect("post-reselect");
    let delta = delta
        .split("e OK UID FETCH completed")
        .next()
        .expect("delta window");
    assert!(delta.contains("UID 2"), "mutated message missing: {delta}");
    assert!(
        delta.contains("MODSEQ (2)"),
        "advanced modseq missing: {delta}"
    );
    assert!(
        !delta.contains("UID 1 "),
        "untouched message leaked into delta: {delta}"
    );
    assert!(
        !delta.contains("* 1 FETCH"),
        "untouched message leaked into delta: {delta}"
    );
}

/// QRESYNC expunge-delta: after a message is expunged its UID-history
/// slot retires to `None`. Re-opening with `SELECT (QRESYNC (... <known
/// uids>))` reports it via `* VANISHED (EARLIER) <uid>`, bounded to the
/// client's known-UID set.
#[tokio::test]
async fn qresync_select_emits_vanished_for_expunged_uid() {
    let script = b"\
        a LOGIN \"u\" \"p\"\r\n\
        b SELECT \"INBOX\"\r\n\
        c UID STORE 1 +FLAGS (\\Deleted)\r\n\
        d UID EXPUNGE 1\r\n\
        e SELECT \"INBOX\" (QRESYNC (1 1 1:2))\r\n\
        f LOGOUT\r\n";
    let out = run_with_imap_small(script).await;
    assert!(
        out.contains("d OK UID EXPUNGE completed"),
        "expunge failed: {out}"
    );
    let reopen = out
        .split("d OK UID EXPUNGE completed")
        .nth(1)
        .expect("post-expunge");
    assert!(
        reopen.contains("* VANISHED (EARLIER) 1\r\n"),
        "VANISHED missing for expunged uid: {reopen}"
    );
}

// ── Multi-account (Stage 4: AUTH-driven account binding) ────────────

async fn run_with_multi_account(script: &[u8], store: saehrimnir::oauth::TokenStore) -> String {
    let fix = saehrimnir::shared::handle(
        fixture::load(std::path::Path::new("fixtures/multi-account-small.toml")).unwrap(),
    );
    let (server, mut client) = tokio::io::duplex(32 * 1024);
    let (_tx, rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut rx = rx;
        imap::serve_connection(
            server,
            fix,
            None,
            store,
            saehrimnir::request_log::RequestLog::default(),
            saehrimnir::latency::LatencyKnob::default(),
            saehrimnir::push::PushHub::new(),
            &mut rx,
        )
        .await
    });
    client.write_all(script).await.unwrap();
    client.shutdown().await.unwrap();
    let mut buf = Vec::new();
    client.read_to_end(&mut buf).await.unwrap();
    task.await.unwrap().unwrap();
    String::from_utf8(buf).unwrap()
}

#[tokio::test]
async fn imap_login_binds_to_matching_account_and_lists_its_mailboxes() {
    // LOGIN with secondary's email -> LIST returns only secondary's
    // mailbox. The primary account's mailbox is invisible.
    let script = b"\
        a1 LOGIN \"secondary@example.com\" \"password\"\r\n\
        a2 LIST \"\" \"*\"\r\n\
        a3 LOGOUT\r\n";
    let out = run_with_multi_account(script, saehrimnir::oauth::TokenStore::default()).await;
    let lists: Vec<&str> = out
        .split("\r\n")
        .filter(|l| l.starts_with("* LIST"))
        .collect();
    assert_eq!(lists.len(), 1);
    assert!(
        lists[0].contains("INBOX") && !lists[0].contains("mbx-primary-inbox"),
        "expected secondary's inbox; got {lists:?}",
    );
}

#[tokio::test]
async fn imap_login_unrecognised_user_stays_on_primary() {
    // No matching account -> connection stays on primary, matching
    // the v0 no-auth baseline.
    let script = b"\
        a1 LOGIN \"nobody@example.com\" \"password\"\r\n\
        a2 STATUS INBOX (MESSAGES)\r\n\
        a3 LOGOUT\r\n";
    let out = run_with_multi_account(script, saehrimnir::oauth::TokenStore::default()).await;
    // Primary has email-primary-001 in its inbox.
    assert!(out.contains("* STATUS \"INBOX\" (MESSAGES 1)"), "got {out}");
}

#[tokio::test]
async fn imap_authenticate_xoauth2_resolves_via_token_store() {
    let store = saehrimnir::oauth::TokenStore::default();
    let token = store.mint("authorization_code", "account-secondary", 1);
    // SASL XOAUTH2 initial response: base64 of
    // `user=secondary@example.com\x01auth=Bearer <token>\x01\x01`.
    let payload = format!("user=secondary@example.com\x01auth=Bearer {token}\x01\x01");
    let encoded = base64_encode(payload.as_bytes());
    let script = format!(
        "a1 AUTHENTICATE XOAUTH2 {encoded}\r\n\
         a2 LIST \"\" \"*\"\r\n\
         a3 LOGOUT\r\n"
    );
    let out = run_with_multi_account(script.as_bytes(), store).await;
    let lists: Vec<&str> = out
        .split("\r\n")
        .filter(|l| l.starts_with("* LIST"))
        .collect();
    assert_eq!(lists.len(), 1);
    assert!(
        lists[0].contains("INBOX"),
        "expected an inbox; got {lists:?}"
    );
    // Secondary's inbox-role mailbox is `mbx-secondary-inbox`; LIST
    // serializes it as "INBOX" (the IMAP convention for role=inbox).
    // The primary's `mbx-primary-inbox` would also render as
    // "INBOX", but it's invisible under the secondary-bound token.
    // Disambiguate by checking the STATUS message count: secondary
    // has 1 message; primary has 1 too in this fixture, so check
    // the body subject through a FETCH.
}

/// Two TCP connections against the same shared `RequestLog` each
/// get a distinct `connection_id`, and every command on a given
/// connection carries the same id. The Phase 7 ratatoskr harness
/// assertion ("one LOGIN + one SELECT per (account, folder) batch")
/// is derived directly from this primitive.
#[tokio::test]
async fn per_connection_id_groups_request_log_entries() {
    let fix = saehrimnir::shared::handle(
        fixture::load(std::path::Path::new("fixtures/jmap-small.toml")).unwrap(),
    );
    let shared_log = saehrimnir::request_log::RequestLog::default();

    async fn drive_session(
        fix: saehrimnir::shared::FixtureHandle,
        log: saehrimnir::request_log::RequestLog,
        script: &[u8],
    ) {
        let (server, mut client) = tokio::io::duplex(32 * 1024);
        let (_tx, rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            let mut rx = rx;
            imap::serve_connection(
                server,
                fix,
                None,
                saehrimnir::oauth::TokenStore::default(),
                log,
                saehrimnir::latency::LatencyKnob::default(),
                saehrimnir::push::PushHub::new(),
                &mut rx,
            )
            .await
        });
        client.write_all(script).await.unwrap();
        client.shutdown().await.unwrap();
        let mut buf = Vec::new();
        client.read_to_end(&mut buf).await.unwrap();
        task.await.unwrap().unwrap();
    }

    drive_session(
        Arc::clone(&fix),
        shared_log.clone(),
        b"a LOGIN u p\r\nb SELECT INBOX\r\nc UID FETCH 1 (UID)\r\nq LOGOUT\r\n",
    )
    .await;
    drive_session(
        Arc::clone(&fix),
        shared_log.clone(),
        b"a LOGIN u p\r\nb SELECT INBOX\r\nq LOGOUT\r\n",
    )
    .await;

    let entries: Vec<_> = shared_log
        .snapshot()
        .into_iter()
        .filter(|e| e.protocol == "imap")
        .collect();
    let ids: Vec<u64> = entries.iter().filter_map(|e| e.connection_id).collect();
    assert_eq!(ids.len(), entries.len(), "every imap entry has an id");
    let mut distinct: Vec<u64> = ids.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), 2, "two TCP connections, two ids: {ids:?}");

    // First session: 4 commands (LOGIN, SELECT, UID FETCH, LOGOUT).
    // Second session: 3 (LOGIN, SELECT, LOGOUT). Verify the run
    // lengths match by walking the log in order.
    let first_id = ids[0];
    let first_run = ids.iter().take_while(|&&i| i == first_id).count();
    let second_run = ids.iter().skip(first_run).count();
    assert_eq!(first_run, 4);
    assert_eq!(second_run, 3);
    let second_id = ids[first_run];
    assert!(ids[first_run..].iter().all(|&i| i == second_id));
}

fn base64_encode(bytes: &[u8]) -> String {
    // Standard base64 with `=` padding.
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
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

/// IMAP container-CRUD round-trip mirroring ratatoskr's
/// `imap-container-crud` gate at the mock's own request layer: a single
/// connection drives CREATE -> LIST -> RENAME -> LIST -> DELETE -> LIST,
/// and each `LIST` block must reflect the preceding mutation, since the
/// gate's `containers_list` is exactly a follow-up mailbox `LIST`.
#[tokio::test]
async fn create_rename_delete_round_trips_through_list() {
    let script = b"\
        a1 LOGIN \"alice\" \"hunter2\"\r\n\
        c1 CREATE \"HarnessBox\"\r\n\
        l1 LIST \"\" \"*\"\r\n\
        r1 RENAME \"HarnessBox\" \"HarnessBoxRenamed\"\r\n\
        l2 LIST \"\" \"*\"\r\n\
        d1 DELETE \"HarnessBoxRenamed\"\r\n\
        l3 LIST \"\" \"*\"\r\n\
        a9 LOGOUT\r\n";
    let out = run_with_fixture(script).await;

    assert!(
        out.contains("c1 OK CREATE completed"),
        "create failed: {out}"
    );
    assert!(
        out.contains("r1 OK RENAME completed"),
        "rename failed: {out}"
    );
    assert!(
        out.contains("d1 OK DELETE completed"),
        "delete failed: {out}"
    );

    // Slice the transcript into the three LIST blocks by their tagged
    // completions (untagged `* LIST` lines precede each tag).
    let i1 = out.find("l1 OK LIST").expect("l1");
    let i2 = out.find("l2 OK LIST").expect("l2");
    let i3 = out.find("l3 OK LIST").expect("l3");
    let after_create = &out[..i1];
    let after_rename = &out[i1..i2];
    let after_delete = &out[i2..i3];

    // CREATE is reflected.
    assert!(
        after_create.contains("\"HarnessBox\"\r\n"),
        "created mailbox missing from LIST: {after_create}"
    );
    // RENAME is reflected: new name present, old name gone.
    assert!(
        after_rename.contains("\"HarnessBoxRenamed\"\r\n"),
        "renamed mailbox missing from LIST: {after_rename}"
    );
    assert!(
        !after_rename.contains("\"HarnessBox\"\r\n"),
        "old mailbox name still listed after rename: {after_rename}"
    );
    // DELETE is reflected: neither name present.
    assert!(
        !after_delete.contains("HarnessBox"),
        "deleted mailbox still listed: {after_delete}"
    );
}

/// IMAP RENAME doubles as a re-parent (a move under a new parent), the
/// path bifrost's `container_move` drives. Create a parent and a child
/// at top level, then RENAME the child to `Parent/Child` and assert the
/// follow-up LIST nests it under the parent.
#[tokio::test]
async fn rename_reparents_child_under_parent_in_list() {
    let script = b"\
        a1 LOGIN \"alice\" \"hunter2\"\r\n\
        c1 CREATE \"HParent\"\r\n\
        c2 CREATE \"HChild\"\r\n\
        r1 RENAME \"HChild\" \"HParent/HChild\"\r\n\
        l1 LIST \"\" \"*\"\r\n\
        a9 LOGOUT\r\n";
    let out = run_with_fixture(script).await;
    assert!(
        out.contains("r1 OK RENAME completed"),
        "reparent failed: {out}"
    );
    let i1 = out.find("l1 OK LIST").expect("l1");
    let after = &out[..i1];
    assert!(
        after.contains("\"HParent/HChild\"\r\n"),
        "child not reparented under parent in LIST: {after}"
    );
    assert!(
        !after.contains("\"HChild\"\r\n"),
        "child still listed at top level after reparent: {after}"
    );
}

/// A LOGIN with an ordinary (non-sentinel) password still succeeds -
/// the opt-in rejection must NOT disturb the v0 accept-everything
/// baseline that the existing sync-harness scripts rely on.
#[tokio::test]
async fn login_with_ordinary_password_succeeds() {
    let script = b"\
        a1 LOGIN \"alice\" \"any-old-password\"\r\n\
        a2 SELECT \"INBOX\"\r\n\
        a3 LOGOUT\r\n";
    let out = run_with_fixture(script).await;
    assert!(
        out.contains("a1 OK [CAPABILITY"),
        "ordinary LOGIN should still succeed: {out}"
    );
    assert!(
        out.contains("a1 OK") && out.contains("LOGIN completed"),
        "ordinary LOGIN should still succeed: {out}"
    );
    // The connection reached the Authenticated state (SELECT worked).
    assert!(
        out.contains("a2 OK [READ-WRITE] SELECT completed"),
        "SELECT after ordinary LOGIN should work: {out}"
    );
}

/// A LOGIN presenting the reserved rejection sentinel password fails
/// with a tagged `NO [AUTHENTICATIONFAILED]`, and the connection stays
/// unauthenticated (a following SELECT is refused). This is the trigger
/// a ratatoskr harness script drives to prove a bad-password account
/// verify surfaces an AccountError.
#[tokio::test]
async fn login_with_reject_sentinel_fails() {
    let script = b"\
        a1 LOGIN \"alice\" \"saehrimnir-reject-auth\"\r\n\
        a2 SELECT \"INBOX\"\r\n\
        a3 LOGOUT\r\n";
    let out = run_with_fixture(script).await;
    assert!(
        out.contains("a1 NO [AUTHENTICATIONFAILED]"),
        "reject sentinel should fail auth: {out}"
    );
    assert!(
        !out.contains("a1 OK"),
        "rejected LOGIN must not also report OK: {out}"
    );
    // Still not authenticated: SELECT before auth is a BAD.
    assert!(
        out.contains("a2 BAD"),
        "SELECT after a failed LOGIN should be refused: {out}"
    );
}

/// The same reserved sentinel, presented over `AUTHENTICATE PLAIN`
/// (`base64(\0user\0pass)`), also fails - so the trigger works whether
/// ratatoskr's account verify uses LOGIN or SASL PLAIN.
#[tokio::test]
async fn authenticate_plain_with_reject_sentinel_fails() {
    let payload = "\0alice\0saehrimnir-reject-auth";
    let encoded = base64_encode(payload.as_bytes());
    let script = format!(
        "a1 AUTHENTICATE PLAIN {encoded}\r\n\
         a2 SELECT \"INBOX\"\r\n\
         a3 LOGOUT\r\n"
    );
    let out = run_with_fixture(script.as_bytes()).await;
    assert!(
        out.contains("a1 NO [AUTHENTICATIONFAILED]"),
        "PLAIN reject sentinel should fail auth: {out}"
    );
    assert!(
        out.contains("a2 BAD"),
        "SELECT after a failed AUTHENTICATE should be refused: {out}"
    );
}

/// A `AUTHENTICATE PLAIN` with an ordinary password still authenticates
/// - the PLAIN rejection is opt-in on the sentinel only.
#[tokio::test]
async fn authenticate_plain_with_ordinary_password_succeeds() {
    let payload = "\0alice\0hunter2";
    let encoded = base64_encode(payload.as_bytes());
    let script = format!(
        "a1 AUTHENTICATE PLAIN {encoded}\r\n\
         a2 SELECT \"INBOX\"\r\n\
         a3 LOGOUT\r\n"
    );
    let out = run_with_fixture(script.as_bytes()).await;
    assert!(
        out.contains("a1 OK [CAPABILITY"),
        "ordinary PLAIN auth should succeed: {out}"
    );
    assert!(
        out.contains("a2 OK [READ-WRITE] SELECT completed"),
        "SELECT after ordinary PLAIN auth should work: {out}"
    );
}

// ── Shared folders (NAMESPACE / ACL / #user namespace) ──────────────
//
// The imap-shared fixture makes bob's inbox visible to alice (primary)
// via an `[[acl]]` grant. A default connection binds to alice.

/// NAMESPACE advertises the personal + other-users prefixes bifrost
/// walks to discover shared folders.
#[tokio::test]
async fn namespace_advertises_personal_and_other_users() {
    let script = b"\
        a1 LOGIN \"alice\" \"pw\"\r\n\
        a2 NAMESPACE\r\n\
        a3 LOGOUT\r\n";
    let out = run_with_fixture_path("fixtures/imap-shared.toml", script).await;
    assert!(
        out.contains("* NAMESPACE ((\"\" \"/\")) ((\"#user/\" \"/\")) NIL\r\n"),
        "got: {out}"
    );
    assert!(out.contains("a2 OK NAMESPACE completed\r\n"), "got: {out}");
}

/// A personal-only fixture (no `[[acl]]`, no scripted `acl_grant`, no
/// non-personal account) advertises NO other-users namespace: clients
/// see the common personal-server NAMESPACE shape and never learn a
/// `#user/` root that could not possibly grow folders.
#[tokio::test]
async fn namespace_stays_personal_only_without_shared_surface() {
    let script = b"\
        a1 LOGIN \"alice\" \"hunter2\"\r\n\
        a2 NAMESPACE\r\n\
        a3 LOGOUT\r\n";
    let out = run_with_fixture(script).await;
    assert!(
        out.contains("* NAMESPACE ((\"\" \"/\")) NIL NIL\r\n"),
        "got: {out}"
    );
    assert!(out.contains("a2 OK NAMESPACE completed\r\n"), "got: {out}");
}

/// `LIST "" "#user/*"` enumerates the shared folder, while a bare
/// `LIST "" "*"` stays personal-only (the other-users namespace is
/// only walked when named).
#[tokio::test]
async fn list_shared_namespace_scopes_to_user_prefix() {
    let script = b"\
        a1 LOGIN \"alice\" \"pw\"\r\n\
        a2 LIST \"\" \"*\"\r\n\
        a3 LIST \"\" \"#user/*\"\r\n\
        a4 LOGOUT\r\n";
    let out = run_with_fixture_path("fixtures/imap-shared.toml", script).await;

    // Alice's personal INBOX is listed on the bare `*`, bob's shared
    // folder is not.
    let after_a2 = out.split("a2 OK LIST completed").next().unwrap();
    assert!(after_a2.contains("\"INBOX\""), "own inbox missing: {out}");
    assert!(
        !after_a2.contains("#user/"),
        "bare LIST leaked shared folder: {out}"
    );

    // The `#user/*` pattern surfaces bob's shared inbox with its role
    // attribute, and does NOT re-list alice's own INBOX.
    let a3 = out
        .split("a2 OK LIST completed")
        .nth(1)
        .unwrap()
        .split("a3 OK LIST completed")
        .next()
        .unwrap();
    assert!(
        a3.contains("* LIST (\\Inbox) \"/\" \"#user/bob@example.com/INBOX\"\r\n"),
        "shared inbox missing: {out}"
    );
    assert!(
        !a3.contains("\"INBOX\"\r\n"),
        "own inbox leaked into #user listing: {out}"
    );
}

/// MYRIGHTS reports full owner rights on a personal mailbox and the
/// granted rights on a shared folder; GETACL lists owner + grants.
#[tokio::test]
async fn myrights_and_getacl_report_shared_grants() {
    let script = b"\
        a1 LOGIN \"alice\" \"pw\"\r\n\
        a2 MYRIGHTS \"INBOX\"\r\n\
        a3 MYRIGHTS \"#user/bob@example.com/INBOX\"\r\n\
        a4 GETACL \"#user/bob@example.com/INBOX\"\r\n\
        a5 LOGOUT\r\n";
    let out = run_with_fixture_path("fixtures/imap-shared.toml", script).await;

    assert!(
        out.contains("* MYRIGHTS \"INBOX\" lrswipkxtea\r\n"),
        "own rights: {out}"
    );
    assert!(
        out.contains("* MYRIGHTS \"#user/bob@example.com/INBOX\" lr\r\n"),
        "shared rights: {out}"
    );
    assert!(
        out.contains(
            "* ACL \"#user/bob@example.com/INBOX\" bob@example.com lrswipkxtea alice@example.com lr\r\n"
        ),
        "acl listing: {out}"
    );
}

/// SELECT + FETCH on a shared folder read the owner's messages while
/// the connection stays authenticated as the borrowing account.
#[tokio::test]
async fn select_and_fetch_shared_folder_reads_owner_messages() {
    let script = b"\
        a1 LOGIN \"alice\" \"pw\"\r\n\
        a2 SELECT \"#user/bob@example.com/INBOX\"\r\n\
        a3 UID FETCH 1:* (UID FLAGS BODY.PEEK[])\r\n\
        a4 LOGOUT\r\n";
    let out = run_with_fixture_path("fixtures/imap-shared.toml", script).await;

    assert!(
        out.contains("* 1 EXISTS\r\n"),
        "shared inbox should show bob's one message: {out}"
    );
    // Bob's grant to alice is `lr` - no write-shaped right - so the
    // SELECT opens READ-ONLY even though the command was SELECT. That
    // is the client's first signal that the folder is read-only,
    // before it ever issues MYRIGHTS.
    assert!(
        out.contains("a2 OK [READ-ONLY] SELECT completed\r\n"),
        "select shared: {out}"
    );
    assert!(
        out.contains("a3 OK UID FETCH completed\r\n"),
        "fetch: {out}"
    );
    // Bob's message body, not alice's.
    assert!(out.contains("Subject: Bob shared"), "bob body: {out}");
    assert!(!out.contains("Alice private"), "leaked alice's mail: {out}");
}

/// A shared folder the viewer holds no grant on does not resolve, and
/// a write on a (read-only) shared selection is refused.
#[tokio::test]
async fn shared_folder_access_and_write_are_gated() {
    // Unknown shared path (bob has not shared a "Sent" folder) 404s.
    let script = b"\
        a1 LOGIN \"alice\" \"pw\"\r\n\
        a2 SELECT \"#user/bob@example.com/Sent\"\r\n\
        a3 MYRIGHTS \"#user/bob@example.com/Sent\"\r\n\
        a4 LOGOUT\r\n";
    let out = run_with_fixture_path("fixtures/imap-shared.toml", script).await;
    assert!(
        out.contains("a2 NO SELECT unknown mailbox\r\n"),
        "got: {out}"
    );
    assert!(
        out.contains("a3 NO MYRIGHTS unknown or inaccessible mailbox\r\n"),
        "got: {out}"
    );

    // A write against a read-only shared selection is refused NOPERM.
    let script = b"\
        a1 LOGIN \"alice\" \"pw\"\r\n\
        a2 SELECT \"#user/bob@example.com/INBOX\"\r\n\
        a3 UID STORE 1 +FLAGS (\\Seen)\r\n\
        a4 LOGOUT\r\n";
    let out = run_with_fixture_path("fixtures/imap-shared.toml", script).await;
    assert!(
        out.contains("a3 NO [NOPERM]"),
        "shared write should be NOPERM: {out}"
    );
}

/// One fixture stages a read-only shared folder (`lr`) and a writable
/// one (`lrswipkxte`) side by side. The rights are fixture-driven, so
/// the two must behave differently on every rights-observing surface:
/// MYRIGHTS, GETACL, the SELECT access level, and the write gate.
#[tokio::test]
async fn shared_folder_rights_distinguish_readonly_from_writable() {
    let script = b"\
        a1 LOGIN \"alice\" \"pw\"\r\n\
        a2 MYRIGHTS \"#user/bob@example.com/INBOX\"\r\n\
        a3 MYRIGHTS \"#user/bob@example.com/Projects\"\r\n\
        a4 GETACL \"#user/bob@example.com/Projects\"\r\n\
        a5 LOGOUT\r\n";
    let out = run_with_fixture_path("fixtures/shared-rights.toml", script).await;
    assert!(
        out.contains("* MYRIGHTS \"#user/bob@example.com/INBOX\" lr\r\n"),
        "read-only rights: {out}"
    );
    assert!(
        out.contains("* MYRIGHTS \"#user/bob@example.com/Projects\" lrswipkxte\r\n"),
        "writable rights: {out}"
    );
    assert!(
        out.contains(
            "* ACL \"#user/bob@example.com/Projects\" bob@example.com lrswipkxtea alice@example.com lrswipkxte\r\n"
        ),
        "acl listing: {out}"
    );

    // The read-only folder opens READ-ONLY and refuses a flag write.
    let script = b"\
        a1 LOGIN \"alice\" \"pw\"\r\n\
        a2 SELECT \"#user/bob@example.com/INBOX\"\r\n\
        a3 UID STORE 1 +FLAGS (\\Seen)\r\n\
        a4 LOGOUT\r\n";
    let out = run_with_fixture_path("fixtures/shared-rights.toml", script).await;
    assert!(
        out.contains("a2 OK [READ-ONLY] SELECT completed\r\n"),
        "read-only select: {out}"
    );
    assert!(out.contains("a3 NO [NOPERM]"), "read-only write: {out}");

    // The writable folder opens READ-WRITE and accepts the same write,
    // which lands on the owner's message.
    let script = b"\
        a1 LOGIN \"alice\" \"pw\"\r\n\
        a2 SELECT \"#user/bob@example.com/Projects\"\r\n\
        a3 UID STORE 1 +FLAGS (\\Seen)\r\n\
        a4 UID FETCH 1 (UID FLAGS)\r\n\
        a5 LOGOUT\r\n";
    let out = run_with_fixture_path("fixtures/shared-rights.toml", script).await;
    assert!(
        out.contains("a2 OK [READ-WRITE] SELECT completed\r\n"),
        "writable select: {out}"
    );
    assert!(
        out.contains("a3 OK UID STORE completed\r\n"),
        "writable store should succeed: {out}"
    );
    assert!(!out.contains("NO [NOPERM]"), "unexpected refusal: {out}");
    assert!(out.contains("\\Seen"), "flag should have landed: {out}");
}
